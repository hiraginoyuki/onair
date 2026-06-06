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
                status: StatusCode::BAD_GATEWAY,
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

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, body) = self.into_parts();
        (status, Json(body)).into_response()
    }
}
