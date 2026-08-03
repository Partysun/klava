use crate::agents::AgentRunner;
use crate::utils::{ask_user_approval, generate_diff, write_with_backup};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::process::Stdio;
use tokio::process::Command as TokioCommand;
use which::which;

/// Klava profile name for Codex
const KLAVA_PROFILE_NAME: &str = "klava";

/// Codex Configuration Example
///
/// Below is an example of the config.toml structure that Codex uses.
/// For more information, see: https://developers.openai.com/codex/config-sample
///
/// Model Providers Configuration Options:
///
/// [model_providers] - Define custom model providers
///
/// Built-in providers include:
/// - openai
/// - ollama
/// - lmstudio
///
/// Custom provider options:
/// name = "Provider Name"                    # Display name for the provider
/// base_url = "https://api.example.com/v1"  # Base URL for API calls
/// wire_api = "responses"                    # API wire format (typically "responses")
/// request_max_retries = 4                   # Number of retries for failed requests (default: 4, max: 100)
/// stream_max_retries = 5                    # Number of retries for stream failures (default: 5, max: 100)
/// stream_idle_timeout_ms = 300000          # Timeout for stream idle periods (default: 300_000ms/5m)
/// supports_websockets = true/false         # Whether provider supports websockets
/// experimental_bearer_token = "token"      # Direct bearer token (dev-only)
/// http_headers = { "Header" = "value" }    # Static HTTP headers to include
/// env_http_headers = { "HEADER" = "ENV_VAR" } # Headers populated from environment variables
/// env_key = "ENV_VAR_NAME"                 # Environment variable name for API key
/// env_key_instructions = "Instructions..." # Instructions for setting up the API key
/// query_params = { "param" = "value" }     # Query parameters to append to requests
/// requires_openai_auth = true/false        # Whether OpenAI authentication is required
///
/// Example:
/// [model_providers.klava]
/// name = "Klava Proxy"
/// base_url = "http://localhost:8080"
/// wire_api = "responses"
pub struct CodexRunner;

impl CodexRunner {
    pub const fn name_static() -> &'static str {
        "codex"
    }
    pub fn new() -> Self {
        Self
    }

    fn profile_config_path(&self) -> Option<std::path::PathBuf> {
        dirs::home_dir().map(|home| home.join(".codex").join(format!("{}.config.toml", KLAVA_PROFILE_NAME)))
    }

    fn root_config_path(&self) -> Option<std::path::PathBuf> {
        dirs::home_dir().map(|home| home.join(".codex").join("config.toml"))
    }

    /// Get list of currently configured models
    pub fn model(&self) -> Vec<String> {
        let mut models = Vec::new();

        if let Some(profile_path) = self.profile_config_path() {
            let Ok(config_content) = std::fs::read_to_string(&profile_path) else {
                return models;
            };

            let Ok(config) = toml::from_str::<CodexConfig>(&config_content) else {
                return models;
            };

            models.push(config.model.clone());
        }

        models
    }
}

impl Default for CodexRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentRunner for CodexRunner {
    fn name(&self) -> &'static str {
        Self::name_static()
    }

    fn paths(&self) -> Vec<std::path::PathBuf> {
        let mut paths = Vec::new();

        // Profile config (new format)
        if let Some(profile_path) = self.profile_config_path() {
            paths.push(profile_path);
        }

        // Root config
        if let Some(config_path) = self.root_config_path() {
            paths.push(config_path);
        }

        paths
    }

    fn check_installation() -> Result<(), anyhow::Error> {
        which(Self::name_static()).map_err(|_| {
            anyhow::anyhow!(
                "codex is not installed. Visit https://developers.openai.com/codex/quickstart"
            )
        })?;
        Ok(())
    }

    async fn run(&self, args: &[String], proxy_url: &str) -> Result<(), anyhow::Error> {
        // Ensure the profile config exists before running
        self.ensure_profile_config(proxy_url).await?;

        // Clean up legacy profile config from main config.toml
        self.cleanup_legacy_config().await?;

        let mut cmd = TokioCommand::new(self.name());
        // Use --profile to select the klava profile
        cmd.arg("--profile").arg(KLAVA_PROFILE_NAME);

        cmd.args(args)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());

        let status = cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!("Failed to execute codex: {}", e))?
            .wait()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to wait for codex: {}", e))?;

        if !status.success() {
            anyhow::bail!("codex exited with status: {}", status);
        }

        Ok(())
    }

    async fn setup(&self, proxy_url: &str) -> Result<(), anyhow::Error> {
        self.ensure_profile_config(proxy_url).await
    }
}

impl CodexRunner {
    /// Clean up legacy profile configuration from main config.toml
    async fn cleanup_legacy_config(&self) -> Result<(), anyhow::Error> {
        let root_config_path = self.root_config_path()
            .ok_or_else(|| anyhow::anyhow!("Could not find config.toml config path"))?;

        if !root_config_path.exists() {
            return Ok(());
        }

        let content = match tokio::fs::read_to_string(&root_config_path).await {
            Ok(c) => c,
            Err(_) => return Ok(()),
        };

        let mut updated = content.clone();
        let mut changed = false;

        // Remove legacy `profile = "klava"` line
        if let Some(line_start) = find_line_start(&content, &format!("profile = \"{}\"", KLAVA_PROFILE_NAME)) {
            if let Some(line_end) = content[line_start..].find('\n') {
                let line_end = line_start + line_end + 1;
                updated = format!("{}{}", &updated[..line_start], &updated[line_end..]);
                changed = true;
            }
        }

        // Remove legacy `[profiles.klava]` section
        let profile_header = format!("[profiles.{}]", KLAVA_PROFILE_NAME);
        if let Some(section_start) = updated.find(&profile_header) {
            if let Some(next_section_start) = find_next_section(&updated, section_start + profile_header.len()) {
                updated = format!("{}{}", &updated[..section_start], &updated[next_section_start..]);
            } else {
                updated = updated[..section_start].to_string();
            }
            changed = true;
        }

        if changed && updated.trim() != content.trim() {
            updated = updated.trim().to_string();
            if !updated.ends_with('\n') {
                updated.push('\n');
            }

            // Validate the updated config is valid TOML
            if toml::from_str::<toml::Value>(&updated).is_ok() {
                write_with_backup(&root_config_path, updated.as_bytes())
                    .map_err(|e| anyhow::anyhow!("Failed to update config.toml: {}", e))?;
            }
        }

        Ok(())
    }

    /// Ensure profile config exists and is up to date
    async fn ensure_profile_config(&self, proxy_url: &str) -> Result<(), anyhow::Error> {
        let profile_path = self.profile_config_path()
            .ok_or_else(|| anyhow::anyhow!("Could not find profile config path"))?;
        let base_url = format!("{}/v1", proxy_url);

        // Ensure directory exists
        if let Some(parent) = profile_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        // Read existing profile config or create new
        let old_config_str = if profile_path.exists() {
            tokio::fs::read_to_string(&profile_path)
                .await
                .unwrap_or_default()
        } else {
            String::new()
        };

        let mut config: CodexConfig = if old_config_str.is_empty() {
            CodexConfig::default()
        } else {
            toml::from_str(&old_config_str).unwrap_or_default()
        };
        let old_config = config.clone();

        self.configure_provider(&mut config, &base_url);
        // Don't add profiles to the profile config itself - that's the legacy format
        // The profile config explicitly sets the model and provider at root level
        config.model = default_model_value();
        config.model_provider_id = default_model_provider();
        config.analytics = AnalyticsConfig::default();

        let new_config = toml::to_string_pretty(&config)?;

        if old_config == config && !old_config_str.is_empty() {
            return Ok(());
        }

        // Show diff and ask for approval
        if !old_config_str.is_empty() {
            let diff = generate_diff(&old_config_str, &new_config, &profile_path);
            println!("\nProposed changes to {}:", profile_path.display());
            println!("{}", diff);

            if !ask_user_approval("Do you want to apply these changes?")? {
                println!("Changes cancelled.");
                return Ok(());
            }
        } else {
            println!("\nNew profile configuration will be created at {}:", profile_path.display());
            println!("{}", new_config);

            if !ask_user_approval("Do you want to create this configuration?")? {
                println!("Changes cancelled.");
                return Ok(());
            }
        }

        // Write profile config with backup
        write_with_backup(&profile_path, new_config.as_bytes())
            .map_err(|e| anyhow::anyhow!("Failed to write profile config: {}", e))?;

        println!("✓ Profile configuration updated successfully");
        println!("  Profile config: {}", profile_path.display());

        Ok(())
    }

    fn configure_provider(&self, config: &mut CodexConfig, proxy_url: &str) {
        let base_url = proxy_url.to_string();

        let provider = config
            .model_providers
            .entry(KLAVA_PROFILE_NAME.to_string())
            .or_insert_with(|| ProviderConfig {
                name: "Klava Proxy".to_string(),
                base_url: None,
                wire_api: Some("responses".to_string()),
                request_max_retries: None,
                stream_max_retries: None,
                stream_idle_timeout_ms: None,
                supports_websockets: None,
                experimental_bearer_token: None,
                http_headers: None,
                env_http_headers: None,
                env_key: None,
                env_key_instructions: None,
                query_params: None,
                requires_openai_auth: None,
            });

        provider.base_url = Some(base_url.clone());

        config.model_provider_id = KLAVA_PROFILE_NAME.to_string();
        if config.model.is_empty() || config.model == "klava" {
            config.model = KLAVA_PROFILE_NAME.to_string();
        }

        // Add analytics configuration as requested
        config.analytics.enabled = false;
    }
}

/// Find the start index of a line containing the given text
fn find_line_start(content: &str, text: &str) -> Option<usize> {
    content.find(text).map(|idx| {
        let before = &content[..idx];
        before.rfind('\n').map(|n| n + 1).unwrap_or(0)
    })
}

/// Find the start of the next section (line starting with [) after the given position
fn find_next_section(content: &str, after: usize) -> Option<usize> {
    let search_area = &content[after..];
    search_area.find("\n[").map(|idx| after + idx + 1)
}

// CodexConfig structures

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CodexConfig {
    #[serde(default = "default_model_value")]
    pub model: String,

    #[serde(rename = "model_provider", default = "default_model_provider")]
    pub model_provider_id: String,

    #[serde(default)]
    pub model_providers: HashMap<String, ProviderConfig>,

    #[serde(default)]
    pub profiles: HashMap<String, ProfileConfig>,

    #[serde(default)]
    pub analytics: AnalyticsConfig,
}

impl Default for CodexConfig {
    fn default() -> Self {
        Self {
            model: default_model_value(),
            model_provider_id: default_model_provider(),
            model_providers: HashMap::new(),
            profiles: HashMap::new(),
            analytics: AnalyticsConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct AnalyticsConfig {
    #[serde(default = "default_analytics_enabled")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProfileConfig {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(rename = "model_provider", default)]
    pub model_provider: Option<String>,
    #[serde(default)]
    pub approval_policy: Option<String>,
    #[serde(default)]
    pub sandbox_mode: Option<String>,
    #[serde(default)]
    pub service_tier: Option<String>,
    #[serde(default)]
    pub oss_provider: Option<String>,
    #[serde(default)]
    pub model_reasoning_effort: Option<String>,
    #[serde(default)]
    pub plan_mode_reasoning_effort: Option<String>,
    #[serde(default)]
    pub model_reasoning_summary: Option<String>,
    #[serde(default)]
    pub model_verbosity: Option<String>,
    #[serde(default)]
    pub personality: Option<String>,
    #[serde(default)]
    pub chatgpt_base_url: Option<String>,
    #[serde(default)]
    pub model_catalog_json: Option<String>,
    #[serde(default)]
    pub model_instructions_file: Option<String>,
    #[serde(default)]
    pub experimental_compact_prompt_file: Option<String>,
    #[serde(default)]
    pub features: Option<HashMap<String, bool>>,
}

impl Default for ProfileConfig {
    fn default() -> Self {
        Self {
            model: None,
            model_provider: None,
            approval_policy: None,
            sandbox_mode: None,
            service_tier: None,
            oss_provider: None,
            model_reasoning_effort: None,
            plan_mode_reasoning_effort: None,
            model_reasoning_summary: None,
            model_verbosity: None,
            personality: None,
            chatgpt_base_url: None,
            model_catalog_json: None,
            model_instructions_file: None,
            experimental_compact_prompt_file: None,
            features: None,
        }
    }
}

fn default_model_value() -> String {
    "klava".to_string()
}

fn default_model_provider() -> String {
    "klava".to_string()
}

fn default_analytics_enabled() -> bool {
    false
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProviderConfig {
    pub name: String,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub wire_api: Option<String>,
    #[serde(default)]
    pub request_max_retries: Option<u32>,
    #[serde(default)]
    pub stream_max_retries: Option<u32>,
    #[serde(default)]
    pub stream_idle_timeout_ms: Option<u64>,
    #[serde(default)]
    pub supports_websockets: Option<bool>,
    #[serde(default)]
    pub experimental_bearer_token: Option<String>,
    #[serde(default)]
    pub http_headers: Option<HashMap<String, String>>,
    #[serde(default)]
    pub env_http_headers: Option<HashMap<String, String>>,
    #[serde(default)]
    pub env_key: Option<String>,
    #[serde(default)]
    pub env_key_instructions: Option<String>,
    #[serde(default)]
    pub query_params: Option<HashMap<String, String>>,
    #[serde(default)]
    pub requires_openai_auth: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use toml;

    #[test]
    fn test_codex_config_default() {
        let config = CodexConfig::default();
        eprintln!("Debug: config.model = {:?}", config.model);
        eprintln!(
            "Debug: config.model_provider_id = {:?}",
            config.model_provider_id
        );
        assert_eq!(config.model_provider_id, "klava".to_string());
        assert_eq!(config.model, "klava".to_string());
        assert!(!config.analytics.enabled);
    }

    #[test]
    fn test_codex_config_serialization() {
        let config = CodexConfig {
            model: "klava-model".to_string(),
            profiles: HashMap::new(),
            model_provider_id: "klava".to_string(),
            model_providers: {
                let mut map = HashMap::new();
                map.insert(
                    "klava".to_string(),
                    ProviderConfig {
                        name: "Klava Proxy".to_string(),
                        base_url: Some("http://localhost:8080".to_string()),
                        wire_api: Some("responses".to_string()),
                        request_max_retries: None,
                        stream_max_retries: None,
                        stream_idle_timeout_ms: None,
                        supports_websockets: None,
                        experimental_bearer_token: None,
                        http_headers: None,
                        env_http_headers: None,
                        env_key: None,
                        env_key_instructions: None,
                        query_params: None,
                        requires_openai_auth: None,
                    },
                );
                map
            },
            analytics: AnalyticsConfig { enabled: false },
        };

        let serialized = toml::to_string_pretty(&config).unwrap();
        let deserialized: CodexConfig = toml::from_str(&serialized).unwrap();

        assert_eq!(config, deserialized);
    }

    #[test]
    fn test_configure_provider() {
        let runner = CodexRunner::new();
        let mut config = CodexConfig::default();

        runner.configure_provider(&mut config, "http://test-proxy:8080");

        let provider = config.model_providers.get("klava").unwrap();
        assert_eq!(
            provider.base_url.as_ref().unwrap(),
            "http://test-proxy:8080"
        );

        assert_eq!(config.model_provider_id, "klava".to_string());
        assert_eq!(config.model, "klava".to_string());
        assert!(!config.analytics.enabled);
    }
}
