use axum::body::Bytes;
use futures_util::StreamExt;
use tokio::sync::watch;

use onair_obs::observe::{RequestTimeline, TimelineEvent};

use super::attempt::InspectorAttemptBuilder;

const MAX_UPSTREAM_ERROR_CAPTURE_BYTES: usize = 1024 * 1024;

pub(super) enum UpstreamSendError {
    Request(reqwest::Error),
    Shutdown,
}

pub(super) async fn send_upstream_request(
    request: reqwest::RequestBuilder,
    shutdown: &mut watch::Receiver<bool>,
) -> std::result::Result<reqwest::Response, UpstreamSendError> {
    if *shutdown.borrow() {
        return Err(UpstreamSendError::Shutdown);
    }
    let send = request.send();
    tokio::pin!(send);

    tokio::select! {
        biased;
        changed = shutdown.changed() => {
            if changed.is_err() || *shutdown.borrow() {
                Err(UpstreamSendError::Shutdown)
            } else {
                send.await.map_err(UpstreamSendError::Request)
            }
        }
        result = &mut send => result.map_err(UpstreamSendError::Request),
    }
}

pub(super) async fn next_stream_chunk<S>(
    chunks: &mut S,
    shutdown: &mut watch::Receiver<bool>,
) -> Option<std::result::Result<Bytes, reqwest::Error>>
where
    S: futures_util::Stream<Item = std::result::Result<Bytes, reqwest::Error>> + Unpin,
{
    if *shutdown.borrow() {
        return None;
    }

    tokio::select! {
        biased;
        changed = shutdown.changed() => {
            if changed.is_err() || *shutdown.borrow() {
                None
            } else {
                chunks.next().await
            }
        }
        chunk = chunks.next() => chunk,
    }
}

pub(super) enum BufferedBodyReadError {
    Upstream(reqwest::Error),
    Shutdown,
}

pub(super) async fn read_buffered_upstream_body(
    upstream: reqwest::Response,
    timeline: &mut RequestTimeline,
    mut current_attempt: Option<&mut InspectorAttemptBuilder>,
    shutdown: &mut watch::Receiver<bool>,
) -> std::result::Result<Bytes, BufferedBodyReadError> {
    let mut bytes = Vec::new();
    let mut chunks = upstream.bytes_stream();
    loop {
        if *shutdown.borrow() {
            return Err(BufferedBodyReadError::Shutdown);
        }
        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return Err(BufferedBodyReadError::Shutdown);
                }
            }
            chunk = chunks.next() => {
                let Some(chunk) = chunk else {
                    break;
                };
                let chunk = chunk.map_err(BufferedBodyReadError::Upstream)?;
                let body_first_chunk_us = timeline.mark(TimelineEvent::BackendBodyFirstChunk);
                if let Some(attempt_record) = current_attempt.as_deref_mut() {
                    attempt_record.mark_body_first_chunk(body_first_chunk_us);
                }
                bytes.extend_from_slice(&chunk);
            }
        }
    }
    let body_complete_us = timeline.mark(TimelineEvent::BackendBodyComplete);
    if let Some(attempt_record) = current_attempt {
        attempt_record.mark_body_complete(body_complete_us);
    }
    Ok(Bytes::from(bytes))
}

pub(super) struct CapturedUpstreamErrorBody {
    pub(super) bytes: Bytes,
    pub(super) truncated: bool,
}

pub(super) async fn read_capped_upstream_error_body(
    upstream: reqwest::Response,
    timeline: &mut RequestTimeline,
    mut current_attempt: Option<&mut InspectorAttemptBuilder>,
    shutdown: &mut watch::Receiver<bool>,
) -> std::result::Result<CapturedUpstreamErrorBody, BufferedBodyReadError> {
    let mut bytes = Vec::new();
    let mut chunks = upstream.bytes_stream();
    let mut truncated = false;
    let mut completed = false;

    loop {
        let Some(chunk) = next_stream_chunk(&mut chunks, shutdown).await else {
            if *shutdown.borrow() {
                return Err(BufferedBodyReadError::Shutdown);
            }
            completed = true;
            break;
        };
        let chunk = chunk.map_err(BufferedBodyReadError::Upstream)?;
        let body_first_chunk_us = timeline.mark(TimelineEvent::BackendBodyFirstChunk);
        if let Some(attempt_record) = current_attempt.as_deref_mut() {
            attempt_record.mark_body_first_chunk(body_first_chunk_us);
        }

        let remaining = MAX_UPSTREAM_ERROR_CAPTURE_BYTES.saturating_sub(bytes.len());
        if chunk.len() > remaining {
            bytes.extend_from_slice(&chunk[..remaining]);
            truncated = true;
            break;
        }
        bytes.extend_from_slice(&chunk);
        if bytes.len() == MAX_UPSTREAM_ERROR_CAPTURE_BYTES {
            truncated = true;
            break;
        }
    }

    if completed {
        let body_complete_us = timeline.mark(TimelineEvent::BackendBodyComplete);
        if let Some(attempt_record) = current_attempt {
            attempt_record.mark_body_complete(body_complete_us);
        }
    }

    Ok(CapturedUpstreamErrorBody {
        bytes: Bytes::from(bytes),
        truncated,
    })
}

pub(super) fn upstream_url(base_url: &str, path: &str, query: Option<&str>) -> String {
    let mut upstream_url = format!("{base_url}{path}");
    if let Some(query) = query {
        upstream_url.push('?');
        upstream_url.push_str(query);
    }
    upstream_url
}

pub(super) fn upstream_path(path: &str) -> &str {
    match path {
        "/v1/chat/completion" => "/v1/chat/completions",
        path => path,
    }
}

pub(super) fn backend_target(base_url: &str) -> String {
    let Ok(url) = url::Url::parse(base_url) else {
        return "unknown".to_owned();
    };
    let host = url.host_str().unwrap_or("unknown");
    match url.port_or_known_default() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    }
}

pub(super) fn upstream_error_kind(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connect"
    } else if error.is_request() {
        "request"
    } else if error.is_body() {
        "body"
    } else if error.is_decode() {
        "decode"
    } else if error.is_redirect() {
        "redirect"
    } else {
        "unknown"
    }
}

pub(super) fn retryable_send_error(error: &reqwest::Error) -> bool {
    error.is_connect()
        || (error.is_request() && !error.is_body() && !error.is_decode() && !error.is_redirect())
}
