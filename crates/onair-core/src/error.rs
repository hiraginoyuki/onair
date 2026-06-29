use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("{0}")]
    Message(String),
}

impl ConfigError {
    pub fn new(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }
}

impl From<String> for ConfigError {
    fn from(value: String) -> Self {
        Self::Message(value)
    }
}

impl From<&str> for ConfigError {
    fn from(value: &str) -> Self {
        Self::Message(value.to_owned())
    }
}

impl From<String> for Error {
    fn from(value: String) -> Self {
        Self::Config(ConfigError::Message(value))
    }
}

impl From<&str> for Error {
    fn from(value: &str) -> Self {
        Self::Config(ConfigError::Message(value.to_owned()))
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("failed to read config {path}: {source}")]
    ConfigRead {
        path: String,
        source: std::io::Error,
    },
    #[error("failed to parse config {path}: {source}")]
    ConfigParse {
        path: String,
        source: toml::de::Error,
    },
    #[error("invalid config: {0}")]
    Config(#[from] ConfigError),
    #[error("missing environment variable {0}")]
    MissingEnv(String),
    #[error("config watcher error: {0}")]
    ConfigWatch(String),
    #[error("http client error: {0}")]
    HttpClient(#[from] reqwest::Error),
    #[error("inspector persistence error: {0}")]
    InspectorPersistence(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("server error: {0}")]
    Server(#[from] axum::Error),
    #[error("telemetry error: {0}")]
    Telemetry(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct OpenAiErrorBody {
    pub error: OpenAiError,
}

#[derive(Debug, Clone, Serialize)]
pub struct OpenAiError {
    pub message: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub param: Option<String>,
    pub code: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
    pub kind: String,
    pub code: Option<String>,
    pub param: Option<String>,
}

impl ApiError {
    pub fn authentication(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            message: message.into(),
            kind: "authentication_error".to_owned(),
            code: Some("invalid_api_key".to_owned()),
            param: None,
        }
    }

    pub fn bad_request(message: impl Into<String>, param: impl Into<Option<String>>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
            kind: "invalid_request_error".to_owned(),
            code: None,
            param: param.into(),
        }
    }

    pub fn model_not_found(model: &str) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: format!("The model '{model}' does not exist or is not available."),
            kind: "invalid_request_error".to_owned(),
            code: Some("model_not_found".to_owned()),
            param: Some("model".to_owned()),
        }
    }

    pub fn endpoint_unavailable(path: &str, model: Option<&str>) -> Self {
        let message = match model {
            Some(model) => {
                format!("The model '{model}' is configured but does not serve endpoint '{path}'.")
            }
            None => format!("The requested endpoint '{path}' is unavailable."),
        };
        Self {
            status: StatusCode::NOT_FOUND,
            message,
            kind: "invalid_request_error".to_owned(),
            code: Some("endpoint_unavailable".to_owned()),
            param: Some("endpoint".to_owned()),
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
            kind: "invalid_request_error".to_owned(),
            code: Some("not_found".to_owned()),
            param: None,
        }
    }

    pub fn upstream(status: StatusCode) -> Self {
        match status {
            StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => Self {
                status,
                message: "The request could not be completed by the selected model.".to_owned(),
                kind: "invalid_request_error".to_owned(),
                code: None,
                param: None,
            },
            StatusCode::TOO_MANY_REQUESTS => Self {
                status: StatusCode::TOO_MANY_REQUESTS,
                message: "The selected model is temporarily rate limited.".to_owned(),
                kind: "rate_limit_error".to_owned(),
                code: Some("rate_limit_exceeded".to_owned()),
                param: None,
            },
            StatusCode::REQUEST_TIMEOUT => Self {
                status: StatusCode::REQUEST_TIMEOUT,
                message: "The selected model did not respond in time.".to_owned(),
                kind: "server_error".to_owned(),
                code: Some("request_timeout".to_owned()),
                param: None,
            },
            _ => Self {
                status: map_upstream_status(status),
                message: "The selected model could not complete the request.".to_owned(),
                kind: "server_error".to_owned(),
                code: Some("upstream_error".to_owned()),
                param: None,
            },
        }
    }

    pub fn timeout() -> Self {
        Self {
            status: StatusCode::GATEWAY_TIMEOUT,
            message: "The selected model did not respond in time.".to_owned(),
            kind: "server_error".to_owned(),
            code: Some("request_timeout".to_owned()),
            param: None,
        }
    }

    pub fn internal() -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "The request could not be completed.".to_owned(),
            kind: "server_error".to_owned(),
            code: Some("internal_error".to_owned()),
            param: None,
        }
    }

    pub fn into_parts(self) -> (StatusCode, OpenAiErrorBody) {
        (
            self.status,
            OpenAiErrorBody {
                error: OpenAiError {
                    message: self.message,
                    kind: self.kind,
                    param: self.param,
                    code: self.code,
                },
            },
        )
    }
}

/// Map an upstream response status to the closest onair client-side
/// status. Statuses that are safe to forward verbatim (4xx, 429,
/// 408) keep their value; everything else collapses to
/// `BAD_GATEWAY` (502) so the client does not see obscure upstream
/// codes the proxy cannot categorize. The sanitized
/// `ApiError::upstream` path uses this mapping; the
/// `expose_backend_errors` opt-in path also uses it (but forwards
/// the body verbatim).
pub fn map_upstream_status(status: StatusCode) -> StatusCode {
    match status {
        StatusCode::BAD_REQUEST
        | StatusCode::UNPROCESSABLE_ENTITY
        | StatusCode::TOO_MANY_REQUESTS
        | StatusCode::REQUEST_TIMEOUT => status,
        _ => StatusCode::BAD_GATEWAY,
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, body) = self.into_parts();
        (status, Json(body)).into_response()
    }
}

// ── Anthropic error format ──────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct AnthropicError {
    pub message: String,
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AnthropicErrorBody {
    pub error: AnthropicError,
    #[serde(rename = "type")]
    pub body_type: String,
}

impl ApiError {
    /// Convert this error into an Anthropic-format error response.
    ///
    /// Maps `ApiError.kind` to the Anthropic error `type` category.
    pub fn into_anthropic_parts(self) -> (StatusCode, AnthropicErrorBody) {
        let kind = match self.status.as_u16() {
            404 => "not_found_error",
            _ => match self.kind.as_str() {
                "invalid_request_error" => "invalid_request_error",
                "authentication_error" => "authentication_error",
                "rate_limit_error" => "rate_limit_error",
                "server_error" => "api_error",
                _ => "api_error",
            },
        };
        (
            self.status,
            AnthropicErrorBody {
                error: AnthropicError {
                    message: self.message,
                    kind: kind.to_owned(),
                },
                body_type: "error".to_owned(),
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_upstream_status_keeps_pass_through_codes() {
        for code in [
            StatusCode::BAD_REQUEST,
            StatusCode::UNPROCESSABLE_ENTITY,
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::REQUEST_TIMEOUT,
        ] {
            assert_eq!(map_upstream_status(code), code, "code={code}");
        }
    }

    #[test]
    fn map_upstream_status_collapses_other_codes_to_bad_gateway() {
        for code in [
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::GATEWAY_TIMEOUT,
            StatusCode::IM_A_TEAPOT,
            StatusCode::OK,
        ] {
            assert_eq!(
                map_upstream_status(code),
                StatusCode::BAD_GATEWAY,
                "code={code}"
            );
        }
    }

    #[test]
    fn api_error_upstream_message_preserved_per_status() {
        // The refactor must not regress the per-status messaging of
        // the sanitized path.
        let bad_request = ApiError::upstream(StatusCode::BAD_REQUEST);
        assert_eq!(bad_request.kind, "invalid_request_error");
        assert_eq!(bad_request.code, None);
        let rate_limited = ApiError::upstream(StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(rate_limited.kind, "rate_limit_error");
        assert_eq!(rate_limited.code.as_deref(), Some("rate_limit_exceeded"));
        let server_error = ApiError::upstream(StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(server_error.status, StatusCode::BAD_GATEWAY);
        assert_eq!(server_error.code.as_deref(), Some("upstream_error"));
    }
}
