use std::fs::{self, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::{Method, StatusCode};
use bytes::Bytes;
use serde::{Serialize, Serializer};
use tracing::warn;

use crate::metrics::MetricLabels;
use onair_core::config::{DebugCaptureConfig, DebugCaptureMode};
use onair_core::openai::UsageDiagnostics;
use onair_core::sanitize::{STORAGE_SEGMENT_MAX_CHARS, sanitize_for_storage};

use super::stream_capture::{StreamCapture, StreamTimings};

const INBOUND_BODY_FILE: &str = "inbound.body";
const UPSTREAM_BODY_FILE: &str = "upstream.body";
const UPSTREAM_ERROR_BODY_FILE: &str = "upstream_error.body";
const UPSTREAM_RESPONSE_FILE: &str = "upstream_response.ndjson";
const CLIENT_RESPONSE_FILE: &str = "client_response.ndjson";
const METADATA_FILE: &str = "metadata.json";

static CAPTURE_COUNTER: AtomicU64 = AtomicU64::new(1);

fn serialize_status_code_as_u16<S: Serializer>(
    value: &StatusCode,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_u16(value.as_u16())
}

fn serialize_optional_status_code_as_u16<S: Serializer>(
    value: &Option<StatusCode>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match value {
        Some(code) => serializer.serialize_some(&code.as_u16()),
        None => serializer.serialize_none(),
    }
}

pub struct CaptureRequest<'a> {
    pub method: &'a Method,
    pub client_path: &'a str,
    pub client_query: Option<&'a str>,
    pub upstream_path: &'a str,
    pub upstream_query: Option<&'a str>,
    pub content_type: Option<&'a str>,
    pub request_id: Option<&'a str>,
    pub labels: &'a MetricLabels,
    pub requested_model: &'a str,
    pub public_model: &'a str,
    pub backend_model: &'a str,
    pub inbound_body: &'a Bytes,
    pub upstream_body: &'a [u8],
}

pub struct RequestCapture {
    directory: PathBuf,
    metadata: CaptureMetadata,
}

impl RequestCapture {
    pub fn id(&self) -> &str {
        &self.metadata.id
    }

    pub fn record_outcome(&mut self, outcome: CaptureOutcome) {
        self.metadata.outcome = outcome;
        if let Err(error) = self.write_metadata() {
            warn!(
                capture_id = %self.metadata.id,
                directory = %self.directory.display(),
                ?error,
                "failed to update debug capture metadata"
            );
        }
    }

    pub fn record_upstream_error_response(
        &mut self,
        upstream_status: StatusCode,
        content_type: Option<&str>,
        body: &[u8],
        truncated: bool,
    ) {
        if let Err(error) =
            self.write_upstream_error_response(upstream_status, content_type, body, truncated)
        {
            warn!(
                capture_id = %self.metadata.id,
                directory = %self.directory.display(),
                ?error,
                "failed to capture upstream error response body"
            );
        }
    }

    pub fn record_stream_usage(&mut self, diagnostics: UsageDiagnostics) {
        self.metadata.stream_usage = diagnostics;
        if let Err(error) = self.write_metadata() {
            warn!(
                capture_id = %self.metadata.id,
                directory = %self.directory.display(),
                ?error,
                "failed to update debug capture stream usage metadata"
            );
        }
    }

    /// Open a streaming capture for one side of the proxy
    /// (`upstream_response` or `client_response`). Returns
    /// `None` if the directory cannot be opened. The caller owns
    /// the returned `StreamCapture` and is responsible for calling
    /// `finish` to drain it. On finish, the proxy passes the
    /// resulting `StreamTimings` and the NDJSON file path to
    /// `record_stream_summary` so `metadata.json` can reference
    /// them.
    pub fn open_stream_capture(&self, side: &'static str) -> Option<StreamCapture> {
        StreamCapture::new(&self.directory, side, None).ok()
    }

    /// Record that a streaming capture ran on this request. Adds
    /// the NDJSON file reference and the per-side timings to
    /// `metadata.json`. Idempotent for the same `side` (later calls
    /// overwrite the earlier timings).
    pub fn record_stream_summary(
        &mut self,
        side: &'static str,
        file_path: &Path,
        timings: &StreamTimings,
    ) {
        if side == "upstream_response" {
            self.metadata.files.upstream_response = Some(UPSTREAM_RESPONSE_FILE);
            self.metadata.timings.upstream_response = Some(StoredTimings {
                file: UPSTREAM_RESPONSE_FILE.to_owned(),
                started_at_unix_us: timings.started_at_unix_us,
                first_event_at_unix_us: timings.first_event_at_unix_us,
                completed_at_unix_us: timings.completed_at_unix_us,
                total_duration_us: timings.total_duration_us,
                event_count: timings.event_count,
                dropped_events: timings.dropped_events,
                truncated: timings.truncated,
            });
        } else if side == "client_response" {
            self.metadata.files.client_response = Some(CLIENT_RESPONSE_FILE);
            self.metadata.timings.client_response = Some(StoredTimings {
                file: CLIENT_RESPONSE_FILE.to_owned(),
                started_at_unix_us: timings.started_at_unix_us,
                first_event_at_unix_us: timings.first_event_at_unix_us,
                completed_at_unix_us: timings.completed_at_unix_us,
                total_duration_us: timings.total_duration_us,
                event_count: timings.event_count,
                dropped_events: timings.dropped_events,
                truncated: timings.truncated,
            });
        } else {
            warn!(
                capture_id = %self.metadata.id,
                side,
                path = %file_path.display(),
                "record_stream_summary called with unknown side; ignoring"
            );
            return;
        }
        if let Err(error) = self.write_metadata() {
            warn!(
                capture_id = %self.metadata.id,
                directory = %self.directory.display(),
                ?error,
                "failed to update debug capture stream summary metadata"
            );
        }
    }

    fn write_upstream_error_response(
        &mut self,
        upstream_status: StatusCode,
        content_type: Option<&str>,
        body: &[u8],
        truncated: bool,
    ) -> std::io::Result<()> {
        write_private_file_replace(&self.directory.join(UPSTREAM_ERROR_BODY_FILE), body)?;
        self.metadata.files.upstream_error_body = Some(UPSTREAM_ERROR_BODY_FILE);
        self.metadata.upstream_error_status = Some(upstream_status);
        self.metadata.upstream_error_content_type = content_type.map(str::to_owned);
        self.metadata.upstream_error_body_bytes = Some(body.len());
        self.metadata.upstream_error_body_truncated = Some(truncated);
        self.write_metadata()
    }

    fn write_metadata(&self) -> std::io::Result<()> {
        let bytes = serde_json::to_vec_pretty(&self.metadata).map_err(std::io::Error::other)?;
        write_private_file_replace(&self.directory.join(METADATA_FILE), &bytes)
    }
}

#[derive(Clone, Serialize)]
struct CaptureMetadata {
    id: String,
    captured_at_unix_ms: u128,
    method: String,
    client_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_query: Option<String>,
    upstream_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    upstream_query: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    request_id: Option<String>,
    mode: DebugCaptureMode,
    identity: String,
    route: String,
    backend: String,
    requested_model: String,
    public_model: String,
    backend_model: String,
    stream: bool,
    request_body_bytes: usize,
    upstream_body_bytes: usize,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_optional_status_code_as_u16"
    )]
    upstream_error_status: Option<StatusCode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    upstream_error_content_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    upstream_error_body_bytes: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    upstream_error_body_truncated: Option<bool>,
    stream_usage: UsageDiagnostics,
    files: CaptureFiles,
    /// Streaming capture summary, populated by
    /// `RequestCapture::record_stream_summary` when `stream_capture`
    /// is enabled for the request. Default-omitted in JSON so
    /// non-streaming captures do not grow.
    #[serde(default, skip_serializing_if = "CaptureTimings::is_empty")]
    timings: CaptureTimings,
    outcome: CaptureOutcome,
}

impl CaptureTimings {
    fn is_empty(&self) -> bool {
        self.upstream_response.is_none() && self.client_response.is_none()
    }
}

#[derive(Clone, Serialize)]
struct CaptureFiles {
    inbound_body: &'static str,
    upstream_body: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    upstream_error_body: Option<&'static str>,
    /// NDJSON file path for the per-event upstream response
    /// stream capture (only present when `stream_capture` was
    /// enabled for this request). See
    /// `.local/decisions/2026-06-27-streaming-debug-capture.md`.
    #[serde(skip_serializing_if = "Option::is_none")]
    upstream_response: Option<&'static str>,
    /// NDJSON file path for the per-event client response stream
    /// capture (only present when `stream_capture` was enabled).
    #[serde(skip_serializing_if = "Option::is_none")]
    client_response: Option<&'static str>,
}

/// Per-side streaming capture summary, written into `metadata.json`
/// when `record_stream_summary` is called. Holds only the
/// summary; the events themselves live in the NDJSON file
/// referenced by `file`.
#[derive(Clone, Serialize)]
struct StoredTimings {
    file: String,
    started_at_unix_us: u64,
    first_event_at_unix_us: Option<u64>,
    completed_at_unix_us: Option<u64>,
    total_duration_us: u64,
    event_count: u64,
    dropped_events: u64,
    truncated: bool,
}

/// Combined streaming-capture summary in `metadata.json`. Each
/// field is set when the corresponding side's `StreamCapture`
/// finishes. Both are absent for non-streaming requests or when
/// `stream_capture` was disabled.
#[derive(Clone, Serialize, Default)]
struct CaptureTimings {
    #[serde(skip_serializing_if = "Option::is_none")]
    upstream_response: Option<StoredTimings>,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_response: Option<StoredTimings>,
}

#[derive(Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CaptureOutcome {
    SentToUpstream,
    Success {
        #[serde(serialize_with = "serialize_status_code_as_u16")]
        upstream_status: StatusCode,
    },
    StreamCompleted {
        #[serde(serialize_with = "serialize_status_code_as_u16")]
        upstream_status: StatusCode,
        stream_duration_ms: u128,
        input_tokens: u64,
        cached_input_tokens: u64,
        output_tokens: u64,
    },
    StreamIncomplete {
        #[serde(serialize_with = "serialize_status_code_as_u16")]
        upstream_status: StatusCode,
        stream_duration_ms: u128,
        input_tokens: u64,
        cached_input_tokens: u64,
        output_tokens: u64,
    },
    UpstreamNonSuccess {
        #[serde(serialize_with = "serialize_status_code_as_u16")]
        upstream_status: StatusCode,
        #[serde(serialize_with = "serialize_status_code_as_u16")]
        client_status: StatusCode,
    },
    UpstreamTimeout {
        #[serde(serialize_with = "serialize_status_code_as_u16")]
        client_status: StatusCode,
    },
    UpstreamRequestFailed {
        #[serde(serialize_with = "serialize_status_code_as_u16")]
        client_status: StatusCode,
        error_kind: &'static str,
    },
    UpstreamBodyReadFailed {
        #[serde(serialize_with = "serialize_status_code_as_u16")]
        client_status: StatusCode,
        error_kind: &'static str,
    },
    UpstreamStreamFailed {
        #[serde(serialize_with = "serialize_status_code_as_u16")]
        upstream_status: StatusCode,
        stream_duration_ms: u128,
        error_kind: &'static str,
        input_tokens: u64,
        cached_input_tokens: u64,
        output_tokens: u64,
    },
}

pub fn capture_request(
    config: &DebugCaptureConfig,
    request: CaptureRequest<'_>,
) -> Option<RequestCapture> {
    if !config.enabled {
        return None;
    }

    match create_capture(config, request) {
        Ok(capture) => {
            warn!(
                capture_id = %capture.metadata.id,
                directory = %capture.directory.display(),
                mode = ?config.mode,
                "debug capture wrote exact request bodies; disable after debugging"
            );
            Some(capture)
        }
        Err(error) => {
            warn!(
                directory = %config.directory.display(),
                ?error,
                "debug capture failed; continuing without captured request bodies"
            );
            None
        }
    }
}

fn create_capture(
    config: &DebugCaptureConfig,
    request: CaptureRequest<'_>,
) -> std::io::Result<RequestCapture> {
    fs::create_dir_all(&config.directory)?;
    ensure_private_directory(&config.directory)?;
    let captured_at_unix_ms = unix_millis();
    let id = capture_id(captured_at_unix_ms, request.request_id);
    let directory = config.directory.join(&id);
    create_private_dir(&directory)?;

    write_private_file_new(&directory.join(INBOUND_BODY_FILE), request.inbound_body)?;
    write_private_file_new(&directory.join(UPSTREAM_BODY_FILE), request.upstream_body)?;

    let metadata = CaptureMetadata {
        id,
        captured_at_unix_ms,
        method: request.method.as_str().to_owned(),
        client_path: request.client_path.to_owned(),
        client_query: request.client_query.map(str::to_owned),
        upstream_path: request.upstream_path.to_owned(),
        upstream_query: request.upstream_query.map(str::to_owned),
        content_type: request.content_type.map(str::to_owned),
        request_id: request.request_id.map(str::to_owned),
        mode: config.mode,
        identity: request.labels.identity.clone(),
        route: request.labels.route.clone(),
        backend: request.labels.backend.clone(),
        requested_model: request.requested_model.to_owned(),
        public_model: request.public_model.to_owned(),
        backend_model: request.backend_model.to_owned(),
        stream: request.labels.stream,
        request_body_bytes: request.inbound_body.len(),
        upstream_body_bytes: request.upstream_body.len(),
        upstream_error_status: None,
        upstream_error_content_type: None,
        upstream_error_body_bytes: None,
        upstream_error_body_truncated: None,
        stream_usage: UsageDiagnostics::default(),
        files: CaptureFiles {
            inbound_body: INBOUND_BODY_FILE,
            upstream_body: UPSTREAM_BODY_FILE,
            upstream_error_body: None,
            upstream_response: None,
            client_response: None,
        },
        timings: CaptureTimings::default(),
        outcome: CaptureOutcome::SentToUpstream,
    };
    let capture = RequestCapture {
        directory,
        metadata,
    };
    capture.write_metadata()?;
    Ok(capture)
}

fn capture_id(captured_at_unix_ms: u128, request_id: Option<&str>) -> String {
    let sequence = CAPTURE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut id = format!("{captured_at_unix_ms}-{}-{sequence}", std::process::id());
    if let Some(request_id) =
        request_id.and_then(|value| sanitize_for_storage(value, STORAGE_SEGMENT_MAX_CHARS))
    {
        id.push('-');
        id.push_str(&request_id);
    }
    id
}

fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn ensure_private_directory(path: &Path) -> std::io::Result<()> {
    if !path.is_dir() {
        return Err(std::io::Error::other(format!(
            "debug capture directory '{}' is not a directory",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(unix)]
fn create_private_dir(path: &Path) -> std::io::Result<()> {
    fs::DirBuilder::new().mode(0o700).create(path)
}

#[cfg(not(unix))]
fn create_private_dir(path: &Path) -> std::io::Result<()> {
    fs::create_dir(path)
}

#[cfg(unix)]
fn write_private_file_new(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)
}

#[cfg(not(unix))]
fn write_private_file_new(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)
}

#[cfg(unix)]
fn write_private_file_replace(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)
}

#[cfg(not(unix))]
fn write_private_file_replace(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    file.write_all(bytes)
}

#[cfg(test)]
mod tests {
    use axum::http::Method;

    use super::*;
    use crate::metrics::MetricLabels;

    #[cfg(unix)]
    #[test]
    fn capture_root_and_request_directories_are_private() {
        let root = temp_capture_root("private-directories");
        let labels = MetricLabels {
            route: "responses".to_owned(),
            identity: "dev".to_owned(),
            public_model: "public".to_owned(),
            backend: "backend-a".to_owned(),
            stream: false,
        };
        let method = Method::POST;
        let inbound = Bytes::from_static(br#"{"model":"public"}"#);
        let upstream = br#"{"model":"backend"}"#;
        let config = DebugCaptureConfig {
            enabled: true,
            mode: DebugCaptureMode::All,
            directory: root.clone(),
        };

        let capture = capture_request(
            &config,
            CaptureRequest {
                method: &method,
                client_path: "/v1/responses",
                client_query: None,
                upstream_path: "/v1/responses",
                upstream_query: None,
                content_type: Some("application/json"),
                request_id: Some("req_test"),
                labels: &labels,
                requested_model: "public",
                public_model: "public",
                backend_model: "backend",
                inbound_body: &inbound,
                upstream_body: upstream,
            },
        )
        .unwrap();

        assert_eq!(file_mode(&root), 0o700);
        assert_eq!(file_mode(&capture.directory), 0o700);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    fn file_mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn temp_capture_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "onair-debug-capture-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }
}
