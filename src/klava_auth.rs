//! Klava OAuth authentication module
//!
//! RFC 8628 device authorization grant flow for the klava provider at
//! klava.zatsepin.dev. The flow mints a long-lived klava API key
//! (`sk_kl_...`) server-side and saves it to
//! `~/.local/share/klava/klava-auth.json`. Keys are non-expiring and are
//! revoked (server-side) on `logout`.
//!
//! This module is only compiled when the `klava` feature is enabled.

use base64::prelude::*;
use reqwest::Client;
use reqwest::header::{HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

// The provider host can be overridden with KLAVA_BASE_URL (used for
// development/testing against a local PocketBase).
const DEFAULT_BASE_URL: &str = "https://klava.zatsepin.dev";
const CLIENT_ID: &str = "klava-cli";
const SCOPE: &str = "openid profile email";

/// A model as returned by the proxy catalog (`GET /v1/models`).
#[derive(Debug, Clone, Deserialize)]
pub struct KlavaModel {
    pub id: String,
    #[serde(default)]
    pub openrouter_id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub enabled: bool,
}

impl KlavaModel {
    /// The model ID accepted by the chat completions endpoint. The catalog's
    /// `id` is a backend record ID, not a valid model — `openrouter_id` is.
    pub fn chat_id(&self) -> String {
        if self.openrouter_id.is_empty() {
            self.id.clone()
        } else {
            self.openrouter_id.clone()
        }
    }
}

/// Deserialize a money amount that the backend may send either as a JSON
/// number (`9411.823457`) or as a decimal string (`"9411.823457"`).
///
/// Reads through `serde_json::Value` rather than an untagged enum: with
/// `serde_json`'s `arbitrary_precision` feature enabled anywhere in the
/// dependency graph (jigor turns it on), a numeric `deserialize_any` no longer
/// matches an untagged `f64` variant, so the untagged form silently rejects
/// numbers.
fn deserialize_amount<'de, D>(d: D) -> Result<f64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(d)?;
    match &value {
        serde_json::Value::Number(n) => n
            .as_f64()
            .ok_or_else(|| serde::de::Error::custom(format!("invalid amount: {n}"))),
        serde_json::Value::String(s) => s
            .trim()
            .parse()
            .map_err(|_| serde::de::Error::custom(format!("invalid amount: {s:?}"))),
        other => Err(serde::de::Error::custom(format!(
            "invalid amount: {other:?}"
        ))),
    }
}

/// `GET /v1/me` — authenticated user identity plus balance.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct KlavaMe {
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub key_name: String,
    #[serde(default, deserialize_with = "deserialize_amount")]
    pub amount_rub: f64,
    #[serde(default)]
    pub currency: String,
}

/// `GET /v1/balance` response.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct KlavaBalance {
    #[serde(default, deserialize_with = "deserialize_amount")]
    pub amount_rub: f64,
    #[serde(default)]
    pub currency: String,
}

/// Device authorization flow response (RFC 8628).
#[derive(Debug, Deserialize)]
struct DeviceCodeResponse {
    pub device_code: String,
    pub user_code: String,
    // pub _verification_uri: String, // We use verification_uri_complete instead
    pub verification_uri_complete: String,
    #[serde(default)]
    pub expires_in: i64,
    #[serde(default = "default_interval")]
    pub interval: i64,
}

fn default_interval() -> i64 {
    5
}

/// Device code request body.
#[derive(Debug, Serialize)]
struct DeviceCodeRequest {
    client_id: String,
    scope: String,
    code_challenge: String,
    code_challenge_method: String,
}

/// Token exchange request body.
#[derive(Debug, Serialize)]
struct TokenRequest {
    grant_type: String,
    client_id: String,
    device_code: String,
    code_verifier: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_name: Option<String>,
}

/// Successful token exchange response — carries the minted API key.
#[derive(Debug, Deserialize)]
struct TokenSuccessResponse {
    pub api_key: String,
    #[serde(default)]
    pub key_preview: String,
}

/// OAuth error response.
#[derive(Debug, Deserialize)]
struct TokenErrorResponse {
    pub error: String,
    #[serde(default)]
    pub error_description: Option<String>,
}

/// Persisted Klava authentication data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KlavaCredentials {
    pub api_key: String,
    pub key_preview: String,
    #[serde(default)]
    pub email: String,
    pub base_url: String,
    pub created_at: String,
}

/// Klava OAuth authentication manager (device flow + persistent key).
pub struct KlavaAuth {
    client: Client,
    base_url: String,
    auth_file_path: PathBuf,
    cached_credentials: Option<KlavaCredentials>,
}

impl KlavaAuth {
    pub fn new() -> Self {
        // Use platform-specific data directory:
        // - Windows: C:\Users\username\AppData\Local\klava
        // - macOS: ~/Library/Application Support/klava
        // - Linux: ~/.local/share/klava
        let auth_dir = dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("klava");

        // Test/dev override: point the auth file elsewhere (e.g. a temp dir).
        // Used by unit tests to be hermetic; not a supported user feature.
        let auth_file_path = std::env::var("KLAVA_AUTH_FILE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                fs::create_dir_all(&auth_dir).ok();
                auth_dir.join("klava-auth.json")
            });
        let base_url = std::env::var("KLAVA_BASE_URL")
            .unwrap_or_else(|_| DEFAULT_BASE_URL.to_string())
            .trim_end_matches('/')
            .to_string();

        Self {
            client: Client::new(),
            base_url,
            auth_file_path,
            cached_credentials: None,
        }
    }

    /// Base URL of the klava host (OAuth + API endpoints).
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Load credentials from the auth file (cached on first read).
    fn load_credentials(&mut self) -> Result<KlavaCredentials, KlavaAuthError> {
        if let Some(creds) = &self.cached_credentials {
            return Ok(creds.clone());
        }
        if !self.auth_file_path.exists() {
            return Err(KlavaAuthError::NotAuthenticated);
        }

        let content = fs::read_to_string(&self.auth_file_path)
            .map_err(|e| KlavaAuthError::Io(format!("Failed to read auth file: {}", e)))?;
        let creds: KlavaCredentials = serde_json::from_str(&content)?;

        if creds.api_key.is_empty() {
            return Err(KlavaAuthError::InvalidKey("Empty API key".to_string()));
        }

        self.cached_credentials = Some(creds.clone());
        Ok(creds)
    }

    /// Save credentials to file (permissions 0600 on unix).
    fn save_credentials(&self, creds: &KlavaCredentials) -> Result<(), KlavaAuthError> {
        let auth_dir = self
            .auth_file_path
            .parent()
            .ok_or_else(|| KlavaAuthError::Io("Invalid auth path".to_string()))?;

        fs::create_dir_all(auth_dir)
            .map_err(|e| KlavaAuthError::Io(format!("Failed to create directory: {}", e)))?;

        let json = serde_json::to_string_pretty(creds)?;
        fs::write(&self.auth_file_path, json)
            .map_err(|e| KlavaAuthError::Io(format!("Failed to write auth file: {}", e)))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = fs::metadata(&self.auth_file_path) {
                let mut perms = meta.permissions();
                perms.set_mode(0o600);
                let _ = fs::set_permissions(&self.auth_file_path, perms);
            }
        }

        Ok(())
    }

    /// Generate PKCE code verifier and challenge (RFC 7636, base64url S256).
    fn generate_pkce() -> (String, String) {
        let mut verifier_bytes = [0u8; 32];
        let mut rng = rand::rng();
        use rand::Rng;
        rng.fill_bytes(&mut verifier_bytes);
        let code_verifier = BASE64_URL_SAFE_NO_PAD.encode(verifier_bytes);

        let mut hasher = Sha256::new();
        hasher.update(code_verifier.as_bytes());
        let challenge_bytes = hasher.finalize();
        let code_challenge = BASE64_URL_SAFE_NO_PAD.encode(challenge_bytes);

        (code_verifier, code_challenge)
    }

    /// Open the system browser at the verification URL.
    fn open_browser(&self, url: &str) -> Result<(), KlavaAuthError> {
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
            return Err(KlavaAuthError::Browser("Unsupported OS".to_string()));
        };

        match result {
            Ok(status) if status.success() => Ok(()),
            Ok(_) => Err(KlavaAuthError::Browser(
                "Browser command failed".to_string(),
            )),
            Err(e) => Err(KlavaAuthError::Browser(format!("Failed to open: {}", e))),
        }
    }

    /// Initiate the device authorization flow.
    async fn initiate_device_flow(
        &self,
    ) -> Result<(String, String, i64, i64, String), KlavaAuthError> {
        let (code_verifier, code_challenge) = Self::generate_pkce();

        let params = DeviceCodeRequest {
            client_id: CLIENT_ID.to_string(),
            scope: SCOPE.to_string(),
            code_challenge,
            code_challenge_method: "S256".to_string(),
        };

        let url = format!("{}/api/oauth/device", self.base_url);
        let response = self
            .client
            .post(url)
            .form(&params)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(KlavaAuthError::Http)?;

        if !response.status().is_success() {
            let error = response.text().await.unwrap_or_default();
            return Err(KlavaAuthError::Auth(format!(
                "Device code request failed: {}",
                error
            )));
        }

        let device_flow: DeviceCodeResponse =
            response.json().await.map_err(KlavaAuthError::Http)?;
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

    /// Poll the token endpoint until the device is authorized or times out.
    async fn poll_for_token(
        &self,
        device_code: String,
        code_verifier: String,
        expires_in: i64,
        interval: i64,
    ) -> Result<TokenSuccessResponse, KlavaAuthError> {
        let start = tokio::time::Instant::now();
        let timeout_secs = if expires_in > 0 { expires_in } else { 600 } as u64;
        let mut current_interval = interval.max(1) as u64;
        let mut poll_count = 0u64;

        loop {
            if start.elapsed() > std::time::Duration::from_secs(timeout_secs) {
                return Err(KlavaAuthError::Timeout(
                    "Authentication timed out".to_string(),
                ));
            }
            if poll_count > 0 {
                tokio::time::sleep(std::time::Duration::from_secs(current_interval)).await;
            }
            poll_count += 1;

            let request = TokenRequest {
                grant_type: "urn:ietf:params:oauth:grant-type:device_code".to_string(),
                client_id: CLIENT_ID.to_string(),
                device_code: device_code.clone(),
                code_verifier: code_verifier.clone(),
                client_name: self.client_name(),
            };

            let url = format!("{}/api/oauth/token", self.base_url);
            let response = self
                .client
                .post(&url)
                .form(&request)
                .header("Accept", "application/json")
                .send()
                .await
                .map_err(KlavaAuthError::Http)?;

            if response.status().is_success() {
                let token_response: TokenSuccessResponse =
                    response.json().await.map_err(KlavaAuthError::Http)?;
                return Ok(token_response);
            }

            if response.status() == reqwest::StatusCode::BAD_REQUEST {
                let error_response: TokenErrorResponse = response.json().await?;
                match error_response.error.as_str() {
                    "authorization_pending" => {
                        tracing::debug!("Authorization pending, poll #{}", poll_count);
                        continue;
                    }
                    "slow_down" => {
                        current_interval = (current_interval as f64 * 1.5).ceil() as u64;
                        if current_interval > 10 {
                            current_interval = 10;
                        }
                        tracing::debug!("Rate limit, slowing down");
                        continue;
                    }
                    "expired_token" => {
                        return Err(KlavaAuthError::ExpiredToken(
                            "Device code expired".to_string(),
                        ));
                    }
                    "access_denied" => {
                        return Err(KlavaAuthError::AccessDenied(
                            "Authorization denied".to_string(),
                        ));
                    }
                    error => {
                        return Err(KlavaAuthError::Auth(format!(
                            "Token error: {} - {}",
                            error,
                            error_response.error_description.unwrap_or_default()
                        )));
                    }
                }
            } else {
                return Err(KlavaAuthError::Auth(format!(
                    "Token request failed with status {}",
                    response.status()
                )));
            }
        }
    }

    /// Human-readable client name stored on the minted key.
    fn client_name(&self) -> Option<String> {
        let os = if cfg!(target_os = "macos") {
            "darwin"
        } else if cfg!(target_os = "windows") {
            "windows"
        } else if cfg!(target_os = "linux") {
            "linux"
        } else {
            "unknown"
        };
        let arch = std::env::consts::ARCH;
        let when = chrono::Utc::now().format("%Y-%m-%d").to_string();
        Some(format!("klava-cli ({os}/{arch} @ {when})"))
    }

    /// Perform the complete device authorization flow and persist the key.
    pub async fn authenticate(&mut self) -> Result<KlavaCredentials, KlavaAuthError> {
        println!("🚀 Starting Klava authentication...");

        let (device_code, auth_url, expires_in, interval, code_verifier) =
            self.initiate_device_flow().await?;

        match self.open_browser(&auth_url) {
            Ok(_) => println!("✓ Browser opened"),
            Err(e) => {
                tracing::warn!("Could not open browser: {}", e);
            }
        }

        println!("\n╔════════════════════════════════════════════════════════════╗");
        println!("║              Klava Authentication Required                  ║");
        println!("╚════════════════════════════════════════════════════════════╝");
        println!("\n📍 Authorize this device in your browser:");
        println!("\n   {}\n", auth_url);
        println!("🔄 Waiting for authorization...");
        println!("   (You can also open the URL manually)\n");

        let token_response = self
            .poll_for_token(device_code, code_verifier, expires_in, interval)
            .await?;

        let created_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| chrono::DateTime::<chrono::Utc>::from(SystemTime::UNIX_EPOCH + d).to_rfc3339())
            .unwrap_or_default();

        // Attach the account email when /v1/me is reachable (optional).
        let email = self
            .me_with_key(&token_response.api_key)
            .await
            .map(|m| m.email.clone())
            .unwrap_or_default();

        let credentials = KlavaCredentials {
            api_key: token_response.api_key,
            key_preview: token_response.key_preview,
            email,
            base_url: self.base_url.clone(),
            created_at,
        };

        self.save_credentials(&credentials)?;
        self.cached_credentials = Some(credentials.clone());

        println!("✅ Authentication successful!");
        println!("📝 Key saved to: {}", self.auth_file_path.display());
        println!();

        Ok(credentials)
    }

    /// Get the API key (loads from the auth file when needed).
    pub fn get_api_key(&mut self) -> Result<String, KlavaAuthError> {
        self.load_credentials().map(|c| c.api_key.clone())
    }

    /// Check if a klava session exists.
    pub fn is_authenticated(&mut self) -> bool {
        self.load_credentials().is_ok()
    }

    /// Current credentials (loads from disk when not cached).
    pub fn get_credentials(&mut self) -> Option<KlavaCredentials> {
        self.load_credentials().ok()
    }

    /// Authorization headers for klava API requests.
    pub fn get_auth_headers(&mut self) -> Result<HeaderMap, KlavaAuthError> {
        let api_key = self.get_api_key()?;
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", api_key))
                .map_err(|e| KlavaAuthError::InvalidKey(format!("Invalid auth header: {}", e)))?,
        );
        Ok(headers)
    }

    /// GET /v1/models — public catalog with enabled models.
    pub async fn models(&mut self) -> Result<Vec<KlavaModel>, KlavaAuthError> {
        #[derive(Debug, Deserialize)]
        struct ModelsResponse {
            models: Vec<KlavaModel>,
        }
        let url = format!("{}/v1/models", self.base_url);
        let response = self
            .client
            .get(&url)
            .headers(self.get_auth_headers()?)
            .send()
            .await
            .map_err(KlavaAuthError::Http)?;
        if !response.status().is_success() {
            return Err(KlavaAuthError::Auth(format!(
                "GET /v1/models failed with status {}",
                response.status()
            )));
        }
        let parsed: ModelsResponse = response.json().await.map_err(KlavaAuthError::Http)?;
        Ok(parsed.models)
    }

    /// GET /v1/me — identity plus balance (needs the proxy /v1/me route).
    pub async fn me(&mut self) -> Result<KlavaMe, KlavaAuthError> {
        let key = self.get_api_key()?;
        self.me_with_key(&key).await
    }

    async fn me_with_key(&self, api_key: &str) -> Result<KlavaMe, KlavaAuthError> {
        let url = format!("{}/v1/me", self.base_url);
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", api_key))
                .map_err(|e| KlavaAuthError::InvalidKey(format!("Invalid auth header: {}", e)))?,
        );
        let response = self
            .client
            .get(&url)
            .headers(headers)
            .send()
            .await
            .map_err(KlavaAuthError::Http)?;
        if !response.status().is_success() {
            return Err(KlavaAuthError::Auth(format!(
                "GET /v1/me failed with status {}",
                response.status()
            )));
        }
        response.json().await.map_err(KlavaAuthError::Http)
    }

    /// GET /v1/balance — prepaid RUB balance.
    pub async fn balance(&mut self) -> Result<KlavaBalance, KlavaAuthError> {
        let url = format!("{}/v1/balance", self.base_url);
        let response = self
            .client
            .get(&url)
            .headers(self.get_auth_headers()?)
            .send()
            .await
            .map_err(KlavaAuthError::Http)?;
        if !response.status().is_success() {
            return Err(KlavaAuthError::Auth(format!(
                "GET /v1/balance failed with status {}",
                response.status()
            )));
        }
        response.json().await.map_err(KlavaAuthError::Http)
    }

    /// Revoke the current key server-side (used by `klava logout`).
    ///
    /// Best-effort: network/availability failures are reported but do not
    /// prevent the local session from being cleared.
    pub async fn revoke(&mut self) -> Result<(), KlavaAuthError> {
        #[derive(Serialize)]
        struct RevokeRequest<'a> {
            api_key: &'a str,
        }
        let api_key = self.get_api_key()?;
        let url = format!("{}/api/cli/apikey/revoke", self.base_url);
        let response = self
            .client
            .post(&url)
            .headers(self.get_auth_headers()?)
            .json(&RevokeRequest { api_key: &api_key })
            .send()
            .await
            .map_err(KlavaAuthError::Http)?;

        if !response.status().is_success() {
            return Err(KlavaAuthError::Auth(format!(
                "Revoke request failed with status {}",
                response.status()
            )));
        }
        Ok(())
    }

    /// Clear the local session (optionally revoking the key server-side).
    pub async fn logout(&mut self, revoke_remote: bool) -> Result<(), KlavaAuthError> {
        if revoke_remote && self.is_authenticated() {
            match self.revoke().await {
                Ok(_) => println!("✓ API key revoked on klava.zatsepin.dev"),
                Err(e) => {
                    println!("⚠️  Remote revoke failed (key may still be valid): {}", e);
                }
            }
        }

        self.cached_credentials = None;
        if self.auth_file_path.exists() {
            fs::remove_file(&self.auth_file_path)
                .map_err(|e| KlavaAuthError::Io(format!("Failed to remove auth file: {}", e)))?;
        }
        Ok(())
    }
}

impl Default for KlavaAuth {
    fn default() -> Self {
        Self::new()
    }
}

/// Klava authentication errors
#[derive(Debug, thiserror::Error)]
pub enum KlavaAuthError {
    #[error("not signed in to Klava — run 'klava login' first")]
    NotAuthenticated,

    #[error("invalid klava key: {0}")]
    InvalidKey(String),

    #[error("expired code: {0}")]
    ExpiredToken(String),

    #[error("access denied: {0}")]
    AccessDenied(String),

    #[error("authentication failed: {0}")]
    Auth(String),

    #[error("browser error: {0}")]
    Browser(String),

    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("timeout: {0}")]
    Timeout(String),

    #[error("IO error: {0}")]
    Io(String),

    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pkce_generation() {
        let (verifier, challenge) = KlavaAuth::generate_pkce();
        assert!(!verifier.is_empty());
        assert!(!challenge.is_empty());
        assert_ne!(verifier, challenge);
        assert!(
            challenge
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
    }

    #[test]
    fn test_client_name_format() {
        let auth = KlavaAuth::new();
        let name = auth.client_name().unwrap();
        assert!(name.starts_with("klava-cli ("));
        assert!(name.ends_with(')'));
    }

    #[test]
    fn test_balance_deserializes_numeric_amount() {
        let balance: KlavaBalance =
            serde_json::from_str(r#"{"amount_rub": 9411.823457, "currency": "RUB"}"#).unwrap();
        assert_eq!(balance.amount_rub, 9411.823457);
        assert_eq!(balance.currency, "RUB");
    }

    #[test]
    fn test_balance_deserializes_string_amount() {
        let balance: KlavaBalance =
            serde_json::from_str(r#"{"amount_rub": "9411.823457", "currency": "RUB"}"#).unwrap();
        assert_eq!(balance.amount_rub, 9411.823457);
        assert_eq!(balance.currency, "RUB");
    }

    #[test]
    fn test_me_deserializes_numeric_amount() {
        let me: KlavaMe = serde_json::from_str(
            r#"{"email":"a@b.c","amount_rub": 1234.0,"usd_rub_rate": 83.9497,"currency":"RUB"}"#,
        )
        .unwrap();
        assert_eq!(me.amount_rub, 1234.0);
        assert_eq!(me.email, "a@b.c");
    }

    #[test]
    fn test_balance_deserializes_missing_amount() {
        let balance: KlavaBalance = serde_json::from_str(r#"{"currency": "RUB"}"#).unwrap();
        assert_eq!(balance.amount_rub, 0.0);
        assert_eq!(balance.currency, "RUB");
    }

    #[test]
    fn test_chat_id_prefers_openrouter_id() {
        let model = KlavaModel {
            id: "o7gii3jijhxnxf3".to_string(),
            openrouter_id: "~deepseek/deepseek-flash-latest".to_string(),
            label: "DeepSeek Flash (latest)".to_string(),
            enabled: true,
        };
        assert_eq!(model.chat_id(), "~deepseek/deepseek-flash-latest");
    }

    #[test]
    fn test_chat_id_falls_back_to_record_id() {
        let model = KlavaModel {
            id: "qbpfc4hl61d0hls".to_string(),
            openrouter_id: String::new(),
            label: String::new(),
            enabled: true,
        };
        assert_eq!(model.chat_id(), "qbpfc4hl61d0hls");
    }
}
