use std::fs::{self, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::Method;
use bytes::Bytes;
use serde::Serialize;
use tracing::warn;

use crate::config::{DebugCaptureConfig, DebugCaptureMode};
use crate::metrics::MetricLabels;
use crate::openai::UsageDiagnostics;

const INBOUND_BODY_FILE: &str = "inbound.body";
const UPSTREAM_BODY_FILE: &str = "upstream.body";
const UPSTREAM_ERROR_BODY_FILE: &str = "upstream_error.body";
const METADATA_FILE: &str = "metadata.json";

static CAPTURE_COUNTER: AtomicU64 = AtomicU64::new(1);

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
        upstream_status: u16,
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

    fn write_upstream_error_response(
        &mut self,
        upstream_status: u16,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    upstream_error_status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    upstream_error_content_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    upstream_error_body_bytes: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    upstream_error_body_truncated: Option<bool>,
    stream_usage: UsageDiagnostics,
    files: CaptureFiles,
    outcome: CaptureOutcome,
}

#[derive(Clone, Serialize)]
struct CaptureFiles {
    inbound_body: &'static str,
    upstream_body: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    upstream_error_body: Option<&'static str>,
}

#[derive(Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CaptureOutcome {
    SentToUpstream,
    Success {
        upstream_status: u16,
    },
    StreamCompleted {
        upstream_status: u16,
        stream_duration_ms: u128,
        input_tokens: u64,
        cached_input_tokens: u64,
        output_tokens: u64,
    },
    StreamIncomplete {
        upstream_status: u16,
        stream_duration_ms: u128,
        input_tokens: u64,
        cached_input_tokens: u64,
        output_tokens: u64,
    },
    UpstreamNonSuccess {
        upstream_status: u16,
        client_status: u16,
    },
    UpstreamTimeout {
        client_status: u16,
    },
    UpstreamRequestFailed {
        client_status: u16,
        error_kind: &'static str,
    },
    UpstreamBodyReadFailed {
        client_status: u16,
        error_kind: &'static str,
    },
    UpstreamStreamFailed {
        upstream_status: u16,
        stream_duration_ms: u128,
        error_kind: &'static str,
        input_tokens: u64,
        cached_input_tokens: u64,
        output_tokens: u64,
    },
}

pub fn validate_config(config: &DebugCaptureConfig) -> crate::error::Result<()> {
    if !config.enabled {
        return Ok(());
    }

    if config.directory.as_os_str().is_empty() {
        return Err(crate::error::Error::Config(
            "debug_capture.directory must not be empty when debug capture is enabled".to_owned(),
        ));
    }

    if config
        .directory
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(crate::error::Error::Config(
            "debug_capture.directory must not contain '..' components".to_owned(),
        ));
    }

    if !config
        .directory
        .components()
        .any(|component| matches!(component, Component::Normal(_)))
    {
        return Err(crate::error::Error::Config(
            "debug_capture.directory must include a directory name".to_owned(),
        ));
    }

    Ok(())
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
        },
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
    if let Some(request_id) = request_id.and_then(safe_request_id_segment) {
        id.push('-');
        id.push_str(&request_id);
    }
    id
}

fn safe_request_id_segment(request_id: &str) -> Option<String> {
    let segment = request_id
        .chars()
        .filter_map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                Some(character)
            } else if character.is_ascii_graphic() {
                Some('_')
            } else {
                None
            }
        })
        .take(80)
        .collect::<String>();
    (!segment.is_empty()).then_some(segment)
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
