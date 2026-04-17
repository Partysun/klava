//! Qwen Code OAuth authentication module
//!
//! Handles OAuth 2.0 Device Authorization Grant flow for Qwen Code service.
//! This module is only available when the `qwen-code` feature is enabled.

use base64::prelude::*;
use chrono::{DateTime, Duration, Utc};
use rand::Rng;
use reqwest::Client;
use reqwest::header::{HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::PathBuf;
use tokio::time::{Duration as TokioDuration, sleep};

// OAuth constants for Qwen
const CLIENT_ID: &str = "f0304373b74a44d2b584a3fb70ca9e56";
const DEVICE_CODE_URL: &str = "https://chat.qwen.ai/api/v1/oauth2/device/code";
const TOKEN_URL: &str = "https://chat.qwen.ai/api/v1/oauth2/token";
const SCOPE: &str = "openid profile email model.completion";
const DEFAULT_BASE_URL: &str = "https://portal.qwen.ai/v1";
/// User agent matching Qwen Code client for quota access.
/// Format: "QwenCode/{version} ({platform}; {arch})"
const USER_AGENT: &str = "QwenCode/0.13.2 (darwin; arm64)";

/// Available and supported Qwen models for free quota
/// Based on reverse engineering of the official Qwen CLI
///
/// The first model (qwen3-coder-plus) is the recommended default.
pub const SUPPORTED_MODELS: &[&str] = &[
    "qwen3-coder-plus",
    "coder-model", // Maps to qwen3.5-plus
];

/// Get the default model for Qwen-free provider
pub fn get_default_model() -> &'static str {
    SUPPORTED_MODELS[0] // qwen3-coder-plus
}

/// OAuth device flow response
#[derive(Debug, Deserialize)]
struct DeviceCodeResponse {
    pub device_code: String,
    pub user_code: String,
    // pub _verification_uri: String, // We use verification_uri_complete instead
    pub verification_uri_complete: String,
    pub expires_in: i64,
    #[serde(default = "default_interval")]
    pub interval: i64,
}

fn default_interval() -> i64 {
    5
}

/// OAuth device code request
#[derive(Debug, Serialize)]
struct DeviceCodeRequest {
    client_id: String,
    scope: String,
    code_challenge: String,
    code_challenge_method: String,
}

/// Token exchange request
#[derive(Debug, Serialize)]
struct TokenRequest {
    grant_type: String,
    client_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    device_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    refresh_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code_verifier: Option<String>,
}

/// Successful token response
#[derive(Debug, Deserialize)]
struct TokenSuccessResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    // pub _token_type: String, // We know it's Bearer, no need to validate
    pub expires_in: i64,
    #[serde(default)]
    pub resource_url: String,
}

/// OAuth error response
#[derive(Debug, Deserialize)]
struct TokenErrorResponse {
    pub error: String,
    #[serde(default)]
    pub error_description: Option<String>,
}

/// Authentication credentials
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QwenCredentials {
    pub access_token: String,
    pub refresh_token: String,
    pub resource_url: String,
    pub expires_at: DateTime<Utc>,
}

/// Persistent auth data structure for storage
#[derive(Debug, Serialize, Deserialize)]
struct AuthData {
    #[serde(rename = "type")]
    auth_type: String,
    access_token: String,
    refresh_token: String,
    resource_url: String,
    expires_at: String,
}

/// Qwen OAuth authentication manager
pub struct QwenAuth {
    client: Client,
    auth_file_path: PathBuf,
    cached_credentials: Option<QwenCredentials>,
}

impl QwenAuth {
    /// Create a new QwenAuth instance
    pub fn new() -> Self {
        // Use platform-specific data directory:
        // - Windows: C:\Users\username\AppData\Local\klava
        // - macOS: ~/Library/Application Support/klava
        // - Linux: ~/.local/share/klava
        let auth_dir = dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("klava");

        // Create auth directory if it doesn't exist
        if !auth_dir.exists() {
            fs::create_dir_all(&auth_dir).ok();
        }

        let auth_file_path = auth_dir.join("qwen-auth.json");

        Self {
            client: Client::new(),
            auth_file_path,
            cached_credentials: None,
        }
    }

    /// Get the base URL from resource_url or return default
    pub fn get_base_url(&self) -> String {
        DEFAULT_BASE_URL.to_string()
    }

    /// Check if credentials need refresh
    pub fn needs_refresh(&self) -> bool {
        self.cached_credentials.as_ref().is_none_or(|creds| {
            creds.expires_at.signed_duration_since(Utc::now()) <= Duration::seconds(60)
        })
    }

    /// Get valid access token, refreshing if needed
    pub async fn get_token(&mut self) -> Result<String, AuthError> {
        if self.cached_credentials.is_none()
            && let Ok(creds) = self.load_credentials()
        {
            self.cached_credentials = Some(creds);
        }

        if self.needs_refresh() {
            self.refresh_token_internal().await?;
        }

        self.cached_credentials
            .as_ref()
            .map(|c| c.access_token.clone())
            .ok_or(AuthError::NotAuthenticated)
    }

    /// Load credentials from file
    fn load_credentials(&self) -> Result<QwenCredentials, AuthError> {
        if !self.auth_file_path.exists() {
            return Err(AuthError::NotAuthenticated);
        }

        let content = fs::read_to_string(&self.auth_file_path)
            .map_err(|e| AuthError::Io(format!("Failed to read auth file: {}", e)))?;
        let auth_data: AuthData = serde_json::from_str(&content)?;

        if auth_data.auth_type != "qwen" {
            return Err(AuthError::InvalidToken("Wrong auth type".to_string()));
        }

        let expires_at = DateTime::parse_from_rfc3339(&auth_data.expires_at)
            .map_err(|e| AuthError::InvalidToken(format!("Invalid expiry date: {}", e)))?
            .with_timezone(&Utc);

        Ok(QwenCredentials {
            access_token: auth_data.access_token,
            refresh_token: auth_data.refresh_token,
            resource_url: auth_data.resource_url,
            expires_at,
        })
    }

    /// Save credentials to file
    fn save_credentials(&self, creds: &QwenCredentials) -> Result<(), AuthError> {
        let auth_dir = self
            .auth_file_path
            .parent()
            .ok_or_else(|| AuthError::Io("Invalid auth path".to_string()))?;

        fs::create_dir_all(auth_dir)
            .map_err(|e| AuthError::Io(format!("Failed to create directory: {}", e)))?;

        let auth_data = AuthData {
            auth_type: "qwen".to_string(),
            access_token: creds.access_token.clone(),
            refresh_token: creds.refresh_token.clone(),
            resource_url: creds.resource_url.clone(),
            expires_at: creds.expires_at.to_rfc3339(),
        };

        let json = serde_json::to_string_pretty(&auth_data)?;
        fs::write(&self.auth_file_path, json)
            .map_err(|e| AuthError::Io(format!("Failed to write auth file: {}", e)))?;

        Ok(())
    }

    /// Generate PKCE code verifier and challenge
    pub fn generate_pkce() -> (String, String) {
        let mut verifier_bytes = [0u8; 32];
        let mut rng = rand::rng();
        rng.fill_bytes(&mut verifier_bytes);
        let code_verifier = BASE64_URL_SAFE_NO_PAD.encode(verifier_bytes);

        let mut hasher = Sha256::new();
        hasher.update(code_verifier.as_bytes());
        let challenge_bytes = hasher.finalize();
        let code_challenge = BASE64_URL_SAFE_NO_PAD.encode(challenge_bytes);

        (code_verifier, code_challenge)
    }

    /// Open browser with authentication URL
    fn open_browser(&self, url: &str) -> Result<(), AuthError> {
        let result = if cfg!(target_os = "macos") {
            std::process::Command::new("open").arg(url).status()
        } else if cfg!(target_os = "linux") {
            std::process::Command::new("xdg-open")
                .arg(url)
                .status()
                .or_else(|_| {
                    std::process::Command::new("gio")
                        .arg("open")
                        .arg(url)
                        .status()
                })
        } else if cfg!(target_os = "windows") {
            std::process::Command::new("cmd")
                .args(["/C", "start", "", url])
                .status()
        } else {
            return Err(AuthError::Browser("Unsupported OS".to_string()));
        };

        match result {
            Ok(status) if status.success() => Ok(()),
            Ok(_) => Err(AuthError::Browser("Browser command failed".to_string())),
            Err(e) => Err(AuthError::Browser(format!("Failed to open: {}", e))),
        }
    }

    /// Display authentication instructions
    fn display_instructions(&self, url: &str) {
        println!("\n╔════════════════════════════════════════════════════════════╗");
        println!("║         Qwen Code Authentication Required                  ║");
        println!("╚════════════════════════════════════════════════════════════╝");
        println!("\n📍 Please authorize this application in your browser:");
        println!("\n   {}\n", url);
        println!("🔄 Waiting for authorization...");
        println!("   (You can also open the URL manually)\n");
    }

    /// Initiate device authorization flow
    async fn initiate_device_flow(&self) -> Result<(String, String, i64, i64, String), AuthError> {
        let (code_verifier, code_challenge) = Self::generate_pkce();

        let params = DeviceCodeRequest {
            client_id: CLIENT_ID.to_string(),
            scope: SCOPE.to_string(),
            code_challenge,
            code_challenge_method: "S256".to_string(),
        };

        let response = self
            .client
            .post(DEVICE_CODE_URL)
            .form(&params)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(AuthError::Http)?;

        if !response.status().is_success() {
            let error = response.text().await.unwrap_or_default();
            return Err(AuthError::Auth(format!(
                "Device code request failed: {}",
                error
            )));
        }

        let device_flow: DeviceCodeResponse = response.json().await.map_err(AuthError::Http)?;

        tracing::info!(
            "Device flow initiated. User code: {}",
            device_flow.user_code
        );

        Ok((
            device_flow.device_code,
            device_flow.verification_uri_complete,
            device_flow.expires_in,
            device_flow.interval,
            code_verifier,
        ))
    }

    /// Poll for token after device authorization
    async fn poll_for_token(
        &self,
        device_code: String,
        code_verifier: String,
        expires_in: i64,
        interval: i64,
    ) -> Result<(String, String, String, i64), AuthError> {
        let start_time = tokio::time::Instant::now();
        let timeout = TokioDuration::from_secs(expires_in as u64);
        let mut current_interval = TokioDuration::from_secs(interval as u64);
        let mut poll_count = 0;

        loop {
            if start_time.elapsed() > timeout {
                return Err(AuthError::Timeout("Authentication timed out".to_string()));
            }

            if poll_count > 0 {
                sleep(current_interval).await;
            }

            poll_count += 1;

            let response = self
                .make_token_request(&device_code, Some(&code_verifier), None)
                .await?;

            if response.status().is_success() {
                let token_response: TokenSuccessResponse = response.json().await?;
                println!("✅ Authentication successful!");

                return Ok((
                    token_response.access_token,
                    token_response.refresh_token,
                    token_response.resource_url,
                    token_response.expires_in,
                ));
            }

            if response.status() == reqwest::StatusCode::BAD_REQUEST {
                let error_response: TokenErrorResponse = response.json().await?;

                match error_response.error.as_str() {
                    "authorization_pending" => {
                        tracing::debug!("Authorization pending, poll #{}", poll_count);
                        continue;
                    }
                    "slow_down" => {
                        // Increase polling interval by 1.5x as requested by server
                        current_interval =
                            TokioDuration::from_secs((current_interval.as_secs_f64() * 0.5) as u64);
                        if current_interval > TokioDuration::from_secs(10) {
                            current_interval = TokioDuration::from_secs(10);
                        }
                        tracing::debug!("Rate limit, slowing down");
                        continue;
                    }
                    "expired_token" => {
                        return Err(AuthError::ExpiredToken("Device code expired".to_string()));
                    }
                    "access_denied" => {
                        return Err(AuthError::AccessDenied("Authorization denied".to_string()));
                    }
                    error => {
                        return Err(AuthError::Auth(format!(
                            "Token error: {} - {}",
                            error,
                            error_response.error_description.unwrap_or_default()
                        )));
                    }
                }
            } else {
                return Err(AuthError::Auth(format!(
                    "Token request failed with status {}",
                    response.status()
                )));
            }
        }
    }

    /// Make token request (for device code flow or refresh)
    async fn make_token_request(
        &self,
        device_code: &str,
        code_verifier: Option<&str>,
        refresh_token: Option<&str>,
    ) -> Result<reqwest::Response, AuthError> {
        let request = TokenRequest {
            grant_type: if refresh_token.is_some() {
                "refresh_token".to_string()
            } else {
                "urn:ietf:params:oauth:grant-type:device_code".to_string()
            },
            client_id: CLIENT_ID.to_string(),
            device_code: if refresh_token.is_none() {
                Some(device_code.to_string())
            } else {
                None
            },
            refresh_token: refresh_token.map(|s| s.to_string()),
            code_verifier: code_verifier.map(|s| s.to_string()),
        };

        let response = self
            .client
            .post(TOKEN_URL)
            .form(&request)
            .header("Accept", "application/json")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .send()
            .await
            .map_err(AuthError::Http)?;

        Ok(response)
    }

    /// Internal refresh token method
    async fn refresh_token_internal(&mut self) -> Result<(), AuthError> {
        // Ensure we have cached credentials with refresh token
        if self.cached_credentials.is_none() {
            self.load_credentials()?;
        }

        let refresh_token = self
            .cached_credentials
            .as_ref()
            .map(|c| c.refresh_token.clone())
            .ok_or(AuthError::NotAuthenticated)?;

        tracing::info!("Refreshing Qwen token...");

        let response = self
            .make_token_request("", None, Some(&refresh_token))
            .await?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            tracing::error!("Token refresh failed: {}", error_text);

            // Try parsing as error response
            if let Ok(error_response) = serde_json::from_str::<TokenErrorResponse>(&error_text) {
                return Err(AuthError::Auth(format!(
                    "{} - {}",
                    error_response.error,
                    error_response.error_description.unwrap_or_default()
                )));
            }

            return Err(AuthError::Auth(format!(
                "Token refresh failed: {}",
                error_text
            )));
        }

        let token_response: TokenSuccessResponse = response.json().await?;
        let expires_at = Utc::now() + Duration::seconds(token_response.expires_in);

        // Update cached credentials
        let new_creds = QwenCredentials {
            access_token: token_response.access_token,
            refresh_token: if token_response.refresh_token.is_empty() {
                refresh_token.clone()
            } else {
                token_response.refresh_token
            },
            resource_url: if token_response.resource_url.is_empty() {
                self.cached_credentials
                    .as_ref()
                    .map(|c| c.resource_url.clone())
                    .unwrap_or_default()
            } else {
                token_response.resource_url
            },
            expires_at,
        };

        // Save to file and update cache
        self.save_credentials(&new_creds)?;
        self.cached_credentials = Some(new_creds);

        tracing::info!("Token refreshed successfully");
        Ok(())
    }

    /// Perform complete authentication flow
    pub async fn authenticate(&mut self) -> Result<(), AuthError> {
        println!("🚀 Starting Qwen Code authentication...");

        // Step 1: Initiate device flow
        let (device_code, auth_url, expires_in, interval, code_verifier) =
            self.initiate_device_flow().await?;

        // Step 2: Try to open browser
        match self.open_browser(&auth_url) {
            Ok(_) => {
                println!("✓ Browser opened");
            }
            Err(e) => {
                tracing::warn!("Could not open browser: {}", e);
            }
        }

        // Step: Display instructions
        self.display_instructions(&auth_url);

        // Step 4: Poll for token
        let (access_token, refresh_token, resource_url, expires_in_seconds) = self
            .poll_for_token(device_code, code_verifier, expires_in, interval)
            .await?;

        // Step: Calculate expiry
        let expires_at = Utc::now() + Duration::seconds(expires_in_seconds);

        let credentials = QwenCredentials {
            access_token,
            refresh_token,
            resource_url,
            expires_at,
        };

        // Step 6: Save credentials
        self.save_credentials(&credentials)?;
        self.cached_credentials = Some(credentials);

        println!("📝 Credentials saved to: {}", self.auth_file_path.display());

        Ok(())
    }

    /// Get current credentials without forcing refresh
    pub fn get_credentials(&self) -> Option<&QwenCredentials> {
        self.cached_credentials.as_ref()
    }

    /// Check if authenticated
    /// Check if authenticated
    pub fn is_authenticated(&self) -> bool {
        self.cached_credentials.is_some() || self.load_credentials().is_ok()
    }

    /// Clear authentication (logout)
    pub fn logout(&mut self) -> Result<(), AuthError> {
        self.cached_credentials = None;

        if self.auth_file_path.exists() {
            fs::remove_file(&self.auth_file_path)
                .map_err(|e| AuthError::Io(format!("Failed to remove auth file: {}", e)))?;
        }

        println!("✅ Logged out successfully");
        Ok(())
    }

    /// Get the required headers for Qwen API requests
    /// Based on JavaScript reference implementation, these headers are important
    /// for proper functionality and quota access
    /// This method will automatically refresh the token if needed.
    pub async fn get_auth_headers(&mut self) -> Result<HeaderMap, AuthError> {
        let token = self.get_token().await?;

        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", token)).map_err(|e| {
                AuthError::InvalidToken(format!("Invalid authorization header: {}", e))
            })?,
        );
        headers.insert(
            "X-DashScope-AuthType",
            HeaderValue::from_static("qwen-oauth"),
        );
        headers.insert(
            "X-DashScope-CacheControl",
            HeaderValue::from_static("enable"),
        );
        headers.insert(
            "X-DashScope-UserAgent",
            HeaderValue::from_static(USER_AGENT),
        );
        headers.insert(
            reqwest::header::USER_AGENT,
            HeaderValue::from_static(USER_AGENT),
        );

        Ok(headers)
    }
}

impl Default for QwenAuth {
    fn default() -> Self {
        Self::new()
    }
}

/// Authentication errors
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("Not authenticated")]
    NotAuthenticated,

    #[error("Invalid token: {0}")]
    InvalidToken(String),

    #[error("Expired token: {0}")]
    ExpiredToken(String),

    #[error("Access denied: {0}")]
    AccessDenied(String),

    #[error("Authentication failed: {0}")]
    Auth(String),

    #[error("Browser error: {0}")]
    Browser(String),

    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("Timeout: {0}")]
    Timeout(String),

    #[error("IO error: {0}")]
    Io(String),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pkce_generation() {
        let (verifier, challenge) = QwenAuth::generate_pkce();
        assert!(!verifier.is_empty());
        assert!(!challenge.is_empty());
        assert_ne!(verifier, challenge);
    }

    #[cfg(test)]
    impl QwenAuth {
        fn set_credentials_for_test(&mut self, creds: QwenCredentials) {
            self.cached_credentials = Some(creds);
        }
    }

    #[test]
    fn test_token_refresh_scenarios() {
        let mut auth = QwenAuth::new();

        // Case 1: No credentials - should need refresh
        assert!(
            auth.needs_refresh(),
            "No credentials should require refresh"
        );

        // Case 2: Expired token (expired 2 minutes ago) - should need refresh
        let expired_creds = QwenCredentials {
            access_token: "expired".to_string(),
            refresh_token: "refresh".to_string(),
            resource_url: "portal.qwen.ai".to_string(),
            expires_at: Utc::now() - Duration::minutes(2),
        };
        auth.set_credentials_for_test(expired_creds);
        assert!(auth.needs_refresh(), "Expired token should require refresh");

        // Case: Token expiring soon (30 seconds) - should need refresh (buffer is 60s)
        let soon_expiring = QwenCredentials {
            access_token: "soon".to_string(),
            refresh_token: "refresh".to_string(),
            resource_url: "portal.qwen.ai".to_string(),
            expires_at: Utc::now() + Duration::seconds(30),
        };
        auth.set_credentials_for_test(soon_expiring);
        assert!(
            auth.needs_refresh(),
            "Token expiring within buffer should require refresh"
        );

        // Case 4: Fresh token (2 hours) - should NOT need refresh
        let fresh_creds = QwenCredentials {
            access_token: "fresh".to_string(),
            refresh_token: "refresh".to_string(),
            resource_url: "portal.qwen.ai".to_string(),
            expires_at: Utc::now() + Duration::hours(2),
        };
        auth.set_credentials_for_test(fresh_creds);
        assert!(
            !auth.needs_refresh(),
            "Fresh token should not require refresh"
        );

        // Case: Token just outside buffer (61 seconds) - should NOT need refresh
        let just_safe = QwenCredentials {
            access_token: "safe".to_string(),
            refresh_token: "refresh".to_string(),
            resource_url: "portal.qwen.ai".to_string(),
            expires_at: Utc::now() + Duration::seconds(61),
        };
        auth.set_credentials_for_test(just_safe);
        assert!(
            !auth.needs_refresh(),
            "Token outside refresh buffer should not need refresh"
        );
    }
}
