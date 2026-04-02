use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;
use thiserror::Error;

/// Application-specific errors for klava
#[derive(Error, Debug)]
pub enum Error {
    /// Configuration file errors (from confy)
    #[error("configuration file error: {0}")]
    Config(#[from] confy::ConfyError),

    /// Configuration validation errors
    #[error("base URL is required")]
    MissingBaseUrl,

    #[error("invalid base URL: {0}")]
    InvalidBaseUrl(String),

    #[error("API key is required for provider '{0}'")]
    MissingApiKey(String),

    /// Provider authentication errors
    #[error("provider authentication error: {0}")]
    Provider(String),

    /// Request/response transformation errors
    #[error("transformation error: {0}")]
    Transform(String),

    /// Upstream API errors
    #[error("upstream API error: {0}")]
    Upstream(String),

    /// Serialization/deserialization errors
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// HTTP client errors
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    /// IO errors (file operations)
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// Internal application errors
    #[error("internal error: {0}")]
    Internal(String),
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let (status, error_message) = match self {
            Error::Config(err) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Configuration file error: {}", err),
            ),
            Error::MissingBaseUrl => (StatusCode::BAD_REQUEST, "Base URL is required".to_string()),
            Error::InvalidBaseUrl(url) => (
                StatusCode::BAD_REQUEST,
                format!("Invalid base URL: {}", url),
            ),
            Error::MissingApiKey(provider) => (
                StatusCode::BAD_REQUEST,
                format!("API key is required for provider '{}'", provider),
            ),
            Error::Provider(msg) => (
                StatusCode::UNAUTHORIZED,
                format!("Provider authentication error: {}", msg),
            ),
            Error::Transform(msg) => (StatusCode::BAD_REQUEST, msg),
            Error::Upstream(msg) => (StatusCode::BAD_GATEWAY, msg),
            Error::Serialization(err) => (StatusCode::BAD_REQUEST, format!("JSON error: {}", err)),
            Error::Http(err) => (StatusCode::BAD_GATEWAY, format!("HTTP error: {}", err)),
            Error::Io(err) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("IO error: {}", err),
            ),
            Error::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
        };

        let body = Json(json!({
            "error": {
                "type": "klava_error",
                "message": error_message,
            }
        }));

        (status, body).into_response()
    }
}

/// Result type for klava operations
pub type Result<T> = std::result::Result<T, Error>;
