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

    #[error("no active provider configured - run 'klava config --setup'")]
    ActiveProviderNotSetup,

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
            Error::ActiveProviderNotSetup => (
                StatusCode::BAD_REQUEST,
                "No active provider configured - run 'klava config --setup'".to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::StatusCode;

    async fn status_and_body(e: Error) -> (StatusCode, serde_json::Value) {
        let resp = e.into_response();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    fn serde_err() -> serde_json::Error {
        serde_json::from_str::<serde_json::Value>("not json").unwrap_err()
    }

    #[tokio::test]
    async fn maps_each_variant_to_expected_status() {
        let cases = vec![
            (Error::MissingBaseUrl, StatusCode::BAD_REQUEST),
            (Error::InvalidBaseUrl("x".into()), StatusCode::BAD_REQUEST),
            (Error::MissingApiKey("p".into()), StatusCode::BAD_REQUEST),
            (Error::ActiveProviderNotSetup, StatusCode::BAD_REQUEST),
            (Error::Transform("m".into()), StatusCode::BAD_REQUEST),
            (Error::Serialization(serde_err()), StatusCode::BAD_REQUEST),
            (Error::Provider("m".into()), StatusCode::UNAUTHORIZED),
            (Error::Upstream("m".into()), StatusCode::BAD_GATEWAY),
            (
                Error::Io(std::io::Error::other("io")),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            (
                Error::Internal("m".into()),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        ];
        for (e, want) in cases {
            let (status, _) = status_and_body(e).await;
            assert_eq!(status, want);
        }
    }

    #[tokio::test]
    async fn body_has_klava_error_shape_with_message() {
        let (_, body) = status_and_body(Error::Upstream("boom".to_string())).await;
        assert_eq!(body["error"]["type"], "klava_error");
        assert!(body["error"]["message"].as_str().unwrap().contains("boom"));
    }

    #[tokio::test]
    async fn http_variant_maps_to_bad_gateway() {
        // reqwest::Error cannot be constructed by hand; force a real one from a
        // connection to a port that is guaranteed closed (port 1).
        let client = reqwest::Client::new();
        let e = client
            .get("http://127.0.0.1:1")
            .send()
            .await
            .expect_err("port 1 must refuse connections");
        let (status, _) = status_and_body(Error::Http(e)).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
    }
}
