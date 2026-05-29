use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

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
    Config(String),
    #[error("missing environment variable {0}")]
    MissingEnv(String),
    #[error("config watcher error: {0}")]
    ConfigWatch(String),
    #[error("http client error: {0}")]
    HttpClient(#[from] reqwest::Error),
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
        let (status, kind, code, message) = match status {
            StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => (
                status,
                "invalid_request_error",
                None,
                "The request could not be completed by the selected model.",
            ),
            StatusCode::TOO_MANY_REQUESTS => (
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_error",
                Some("rate_limit_exceeded"),
                "The selected model is temporarily rate limited.",
            ),
            StatusCode::REQUEST_TIMEOUT => (
                StatusCode::REQUEST_TIMEOUT,
                "server_error",
                Some("request_timeout"),
                "The selected model did not respond in time.",
            ),
            _ => (
                StatusCode::BAD_GATEWAY,
                "server_error",
                Some("upstream_error"),
                "The selected model could not complete the request.",
            ),
        };

        Self {
            status,
            message: message.to_owned(),
            kind: kind.to_owned(),
            code: code.map(str::to_owned),
            param: None,
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

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, body) = self.into_parts();
        (status, Json(body)).into_response()
    }
}
