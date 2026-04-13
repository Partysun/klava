use crate::agents::AgentRunner;
use crate::utils::{ask_user_approval, generate_diff, write_with_backup};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::process::Stdio;
use tokio::process::Command as TokioCommand;
use which::which;

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

    /// Get list of currently configured models
    pub fn model(&self) -> Vec<String> {
        let mut models = Vec::new();

        for config_path in self.paths() {
            // Only read from config.toml, skip other files
            if config_path.file_name().and_then(|n| n.to_str()) != Some("config.toml") {
                continue;
            }

            let Ok(config_content) = std::fs::read_to_string(&config_path) else {
                continue;
            };

            let Ok(config) = toml::from_str::<CodexConfig>(&config_content) else {
                continue;
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

        if let Some(home) = dirs::home_dir() {
            let config_path = home.join(".codex").join("config.toml");
            // Always include config path for setup
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

    async fn run(&self, args: &[String], _proxy_url: &str) -> Result<(), anyhow::Error> {
        let mut cmd = TokioCommand::new(self.name());
        // Codex without profile will runned with defaul openai cloud profile
        cmd.arg("--profile").arg("klava");

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
        let config_path = self
            .paths()
            .into_iter()
            .find(|p| p.file_name().and_then(|n| n.to_str()) == Some("config.toml"))
            .ok_or_else(|| anyhow::anyhow!("Could not find config.toml config path"))?;

        // Ensure directory exists
        tokio::fs::create_dir_all(config_path.parent().unwrap()).await?;

        // Read existing config or create new
        let old_config_str = if config_path.exists() {
            tokio::fs::read_to_string(&config_path)
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

        let base_url = format!("{}/v1", proxy_url);

        self.configure_provider(&mut config, &base_url);
        self.configure_profile(&mut config);

        let new_config = toml::to_string_pretty(&config)?;

        if old_config == config {
            return Ok(());
        }

        // Show diff and ask for approval
        if !old_config_str.is_empty() {
            let diff = generate_diff(&old_config_str, &new_config, &config_path);
            println!("\nProposed changes to config.toml:");
            println!("{}", diff);

            if !ask_user_approval("Do you want to apply these changes?")? {
                println!("Changes cancelled.");
                return Ok(());
            }
        } else {
            println!("\nNew configuration will be created:");
            println!("{}", new_config);

            if !ask_user_approval("Do you want to create this configuration?")? {
                println!("Changes cancelled.");
                return Ok(());
            }
        }

        // Write config with backup
        write_with_backup(&config_path, new_config.as_bytes())
            .map_err(|e| anyhow::anyhow!("Failed to write config.toml: {}", e))?;

        println!("✓ Configuration updated successfully");

        Ok(())
    }
}

impl CodexRunner {
    fn configure_provider(&self, config: &mut CodexConfig, proxy_url: &str) {
        let base_url = proxy_url.to_string();

        let provider = config
            .model_providers
            .entry("klava".to_string())
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

        config.model_provider_id = "klava".to_string();
        if config.model.is_empty() || config.model == "klava" {
            config.model = "klava".to_string();
        }

        // Add analytics configuration as requested
        config.analytics.enabled = false;
    }

    fn configure_profile(&self, config: &mut CodexConfig) {
        let profile = config.profiles.entry("klava".to_string()).or_default();

        // Set the profile to use the klava model provider
        profile.model_provider = Some("klava".to_string());

        // Set a default model for the profile if not already set
        if profile.model.is_none() {
            profile.model = Some("klava".to_string());
        }
    }
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
