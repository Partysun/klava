use crate::agents::AgentRunner;
use crate::utils::{ask_user_approval, generate_diff, write_with_backup};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::process::Command as TokioCommand;
use which::which;

/// Provider name registered in Pi's models.json
const KLAVA_PROVIDER_NAME: &str = "klava";
/// Wire API Pi uses against the klava proxy
const KLAVA_WIRE_API: &str = "openai-completions";
/// Dummy API key required by Pi; the klava proxy ignores it (like Ollama's "ollama")
const KLAVA_API_KEY: &str = "klava";

/// Pi Runner
///
/// Pi (https://pi.dev) is a minimal, extensible agent harness. Compatible
/// endpoints are declared in `<agent-dir>/models.json` (default
/// `~/.pi/agent/models.json`, overridable via `PI_CODING_AGENT_DIR`).
///
/// Example models.json entry that klava generates:
///
/// ```json
/// {
///   "$schema": "https://pi.dev/schemas/models.schema.json",
///   "providers": {
///     "klava": {
///       "baseUrl": "http://localhost:48017/v1",
///       "api": "openai-completions",
///       "apiKey": "klava",
///       "models": [{ "id": "<active model>" }]
///     }
///   }
/// }
/// ```
///
/// `<agent-dir>/settings.json` gets `defaultProvider: "klava"` and
/// `defaultModel: "<active model>"`, so pi always starts on the active
/// klava provider and its model.
pub struct PiRunner;

impl PiRunner {
    pub const fn name_static() -> &'static str {
        "pi"
    }

    pub fn new() -> Self {
        Self
    }

    /// Pi agent directory: `PI_CODING_AGENT_DIR` or `~/.pi/agent`
    fn agent_dir() -> Option<PathBuf> {
        if let Ok(dir) = std::env::var("PI_CODING_AGENT_DIR")
            && !dir.is_empty()
        {
            return Some(PathBuf::from(dir));
        }
        dirs::home_dir().map(|home| home.join(".pi").join("agent"))
    }

    /// Path of Pi's user-level models.json
    fn models_config_path() -> Option<PathBuf> {
        Self::agent_dir().map(|dir| dir.join("models.json"))
    }

    /// Path of Pi's user-level settings.json
    fn settings_config_path() -> Option<PathBuf> {
        Self::agent_dir().map(|dir| dir.join("settings.json"))
    }

    /// Model ids exposed to pi for the active klava provider:
    /// the completion model first, then the reasoning model
    /// (deduplicated, so agents that use one model for both get a
    /// single entry). Falls back to "klava". The klava proxy maps
    /// incoming model ids to the configured models, so these only
    /// affect what pi displays and lets the user pick via /model.
    fn active_model_ids() -> Vec<String> {
        let mut ids = Vec::new();
        if let Ok(config) = crate::config::Config::load() {
            if let Some(model) = config.resolve_completion_model() {
                ids.push(model);
            }
            if let Some(model) = config.resolve_reasoning_model()
                && !ids.contains(&model)
            {
                ids.push(model);
            }
        }
        if ids.is_empty() {
            ids.push("klava".to_string());
        }
        ids
    }

    /// Get list of currently configured klava model IDs
    pub fn model(&self) -> Vec<String> {
        let mut models = Vec::new();
        let Some(config_path) = Self::models_config_path() else {
            return models;
        };

        let Ok(config_content) = std::fs::read_to_string(&config_path) else {
            return models;
        };
        let Ok(config) = serde_json::from_str::<PiModelsConfig>(&config_content) else {
            return models;
        };

        let Some(provider) = config.providers.get(KLAVA_PROVIDER_NAME) else {
            return models;
        };
        let Some(model_entries) = provider.get("models").and_then(|m| m.as_array()) else {
            return models;
        };

        for model in model_entries {
            if let Some(id) = model.get("id").and_then(|v| v.as_str()) {
                models.push(id.to_string());
            }
        }

        models.sort();
        models
    }

    /// Merge or update the klava provider entry in a models.json config
    fn configure_provider(
        &self,
        config: &mut PiModelsConfig,
        proxy_url: &str,
        model_ids: &[String],
    ) {
        let base_url = format!("{}/v1", proxy_url);

        let models: Vec<serde_json::Value> = model_ids
            .iter()
            .map(|id| serde_json::json!({ "id": id }))
            .collect();

        let provider_entry = serde_json::json!({
            "baseUrl": base_url,
            "api": KLAVA_WIRE_API,
            "apiKey": KLAVA_API_KEY,
            "models": models
        });

        config
            .providers
            .insert(KLAVA_PROVIDER_NAME.to_string(), provider_entry);
    }

    /// Merge or update the klava defaults in a settings.json config,
    /// preserving all unrelated settings. The default model is the
    /// provider's completion model (the first model id).
    fn configure_settings(&self, config: &mut PiSettingsConfig, model_ids: &[String]) {
        let default_model = model_ids.first().map(|s| s.as_str()).unwrap_or("klava");
        config.rest.insert(
            "defaultProvider".to_string(),
            serde_json::json!(KLAVA_PROVIDER_NAME),
        );
        config
            .rest
            .insert("defaultModel".to_string(), serde_json::json!(default_model));
    }
}

impl Default for PiRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentRunner for PiRunner {
    fn name(&self) -> &'static str {
        Self::name_static()
    }

    fn paths(&self) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        if let Some(config_path) = Self::models_config_path() {
            paths.push(config_path);
        }
        if let Some(config_path) = Self::settings_config_path() {
            paths.push(config_path);
        }
        paths
    }

    fn check_installation() -> Result<(), anyhow::Error> {
        which(Self::name_static()).map_err(|_| {
            anyhow::anyhow!(
                "pi is not installed. Install from https://pi.dev (curl -fsSL https://pi.dev/install.sh | sh) \
                 or npm install -g --ignore-scripts @earendil-works/pi-coding-agent"
            )
        })?;
        Ok(())
    }

    async fn run(&self, args: &[String], _proxy_url: &str) -> Result<(), anyhow::Error> {
        let mut cmd = TokioCommand::new(self.name());

        // Select the klava provider and the active provider's model for
        // this session. settings.json defaults are also written by setup,
        // so plain `pi` runs keep using klava as well.
        let model_ids = Self::active_model_ids();
        let default_model = model_ids.first().map(|s| s.as_str()).unwrap_or("klava");
        cmd.arg("--provider")
            .arg(KLAVA_PROVIDER_NAME)
            .arg("--model")
            .arg(default_model);

        cmd.args(args)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());

        let status = cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!("Failed to execute pi: {}", e))?
            .wait()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to wait for pi: {}", e))?;

        if !status.success() {
            anyhow::bail!("pi exited with status: {}", status);
        }

        Ok(())
    }

    async fn setup(&self, proxy_url: &str) -> Result<(), anyhow::Error> {
        let model_ids = Self::active_model_ids();

        let models_path = Self::models_config_path()
            .ok_or_else(|| anyhow::anyhow!("Could not find Pi models.json config path"))?;
        let settings_path = Self::settings_config_path()
            .ok_or_else(|| anyhow::anyhow!("Could not find Pi settings.json config path"))?;

        // Ensure directory exists
        if let Some(parent) = models_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        // --- models.json: klava provider + active model ---
        let old_models_str = if models_path.exists() {
            tokio::fs::read_to_string(&models_path)
                .await
                .unwrap_or_default()
        } else {
            String::new()
        };

        let mut models_config: PiModelsConfig = if old_models_str.is_empty() {
            PiModelsConfig::default()
        } else {
            serde_json::from_str(&old_models_str).unwrap_or_default()
        };
        let old_models_config = models_config.clone();

        self.configure_provider(&mut models_config, proxy_url, &model_ids);

        let new_models_str = serde_json::to_string_pretty(&models_config)?;
        let models_changed = old_models_config != models_config || old_models_str.is_empty();

        // --- settings.json: klava defaults (provider + active model) ---
        let old_settings_str = if settings_path.exists() {
            tokio::fs::read_to_string(&settings_path)
                .await
                .unwrap_or_default()
        } else {
            String::new()
        };

        let mut settings_config: PiSettingsConfig = if old_settings_str.is_empty() {
            PiSettingsConfig::default()
        } else {
            serde_json::from_str(&old_settings_str).unwrap_or_default()
        };
        let old_settings_config = settings_config.clone();

        self.configure_settings(&mut settings_config, &model_ids);

        let new_settings_str = serde_json::to_string_pretty(&settings_config)?;
        let settings_changed =
            old_settings_config != settings_config || old_settings_str.is_empty();

        if !models_changed && !settings_changed {
            return Ok(());
        }

        // Show diffs for every file that would change, then ask once
        if models_changed {
            if old_models_str.is_empty() {
                println!(
                    "\nNew configuration will be created at {}:",
                    models_path.display()
                );
                println!("{}", new_models_str);
            } else {
                let diff = generate_diff(&old_models_str, &new_models_str, &models_path);
                println!("\nProposed changes to {}:", models_path.display());
                println!("{}", diff);
            }
        }

        if settings_changed {
            if old_settings_str.is_empty() {
                println!(
                    "\nNew configuration will be created at {}:",
                    settings_path.display()
                );
                println!("{}", new_settings_str);
            } else {
                let diff = generate_diff(&old_settings_str, &new_settings_str, &settings_path);
                println!("\nProposed changes to {}:", settings_path.display());
                println!("{}", diff);
            }
        }

        if !ask_user_approval("Do you want to apply these changes?")? {
            println!("Changes cancelled.");
            return Ok(());
        }

        // Write both configs with backup
        if models_changed {
            write_with_backup(&models_path, new_models_str.as_bytes())
                .map_err(|e| anyhow::anyhow!("Failed to write {}: {}", models_path.display(), e))?;
            println!("✓ Configuration updated: {}", models_path.display());
        }

        if settings_changed {
            write_with_backup(&settings_path, new_settings_str.as_bytes()).map_err(|e| {
                anyhow::anyhow!("Failed to write {}: {}", settings_path.display(), e)
            })?;
            println!("✓ Configuration updated: {}", settings_path.display());
        }

        Ok(())
    }
}

// PiModelsConfig structures

/// Pi user-level models.json configuration structure
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PiModelsConfig {
    #[serde(rename = "$schema", default = "default_models_schema")]
    pub schema: String,

    #[serde(default)]
    pub providers: HashMap<String, serde_json::Value>,
}

impl Default for PiModelsConfig {
    fn default() -> Self {
        Self {
            schema: default_models_schema(),
            providers: HashMap::new(),
        }
    }
}

fn default_models_schema() -> String {
    "https://pi.dev/schemas/models.schema.json".to_string()
}

/// Pi user-level settings.json configuration structure. Unknown keys are
/// preserved through the `rest` map so unrelated settings survive edits.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PiSettingsConfig {
    #[serde(rename = "$schema", default = "default_settings_schema")]
    pub schema: String,

    #[serde(flatten)]
    pub rest: HashMap<String, serde_json::Value>,
}

impl Default for PiSettingsConfig {
    fn default() -> Self {
        Self {
            schema: default_settings_schema(),
            rest: HashMap::new(),
        }
    }
}

fn default_settings_schema() -> String {
    "https://pi.dev/schemas/settings.schema.json".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_models_config_default() {
        let config = PiModelsConfig::default();
        assert_eq!(
            config.schema,
            "https://pi.dev/schemas/models.schema.json".to_string()
        );
        assert!(config.providers.is_empty());
    }

    #[test]
    fn test_models_config_serialization_round_trip() {
        let mut config = PiModelsConfig::default();
        config.providers.insert(
            "klava".to_string(),
            json!({
                "baseUrl": "http://localhost:48017/v1",
                "api": "openai-completions",
                "apiKey": "klava",
                "models": [{ "id": "deepseek-v4-flash-0731" }]
            }),
        );

        let serialized = serde_json::to_string_pretty(&config).unwrap();
        let deserialized: PiModelsConfig = serde_json::from_str(&serialized).unwrap();

        assert_eq!(config, deserialized);
    }

    #[test]
    fn test_configure_provider_into_empty_config() {
        let runner = PiRunner::new();
        let mut config = PiModelsConfig::default();

        runner.configure_provider(
            &mut config,
            "http://test-proxy:8080",
            &["deepseek-v4-flash".to_string()],
        );

        let provider = config
            .providers
            .get("klava")
            .expect("klava provider should exist");
        assert_eq!(provider["baseUrl"], json!("http://test-proxy:8080/v1"));
        assert_eq!(provider["api"], json!("openai-completions"));
        assert_eq!(provider["apiKey"], json!("klava"));
        assert_eq!(provider["models"], json!([{ "id": "deepseek-v4-flash" }]));
    }

    #[test]
    fn test_configure_provider_registers_reasoning_and_completion_models() {
        let runner = PiRunner::new();
        let mut config = PiModelsConfig::default();

        runner.configure_provider(
            &mut config,
            "http://test-proxy:8080",
            &["glm-4.7".to_string(), "glm-4.7-reasoning".to_string()],
        );

        let provider = config
            .providers
            .get("klava")
            .expect("klava provider should exist");
        assert_eq!(
            provider["models"],
            json!([{ "id": "glm-4.7" }, { "id": "glm-4.7-reasoning" }])
        );
    }

    #[test]
    fn test_configure_provider_preserves_existing_providers() {
        let runner = PiRunner::new();
        let mut config = PiModelsConfig::default();
        config.providers.insert(
            "ollama".to_string(),
            json!({
                "baseUrl": "http://localhost:11434/v1",
                "api": "openai-completions",
                "apiKey": "ollama",
                "models": [{ "id": "qwen2.5-coder:7b" }]
            }),
        );

        runner.configure_provider(
            &mut config,
            "http://localhost:48017",
            &["glm-4.7".to_string()],
        );

        assert!(config.providers.contains_key("ollama"));
        assert_eq!(
            config.providers["ollama"]["baseUrl"],
            json!("http://localhost:11434/v1")
        );
        assert!(config.providers.contains_key("klava"));
    }

    #[test]
    fn test_configure_provider_idempotent() {
        let runner = PiRunner::new();
        let mut config = PiModelsConfig::default();

        runner.configure_provider(
            &mut config,
            "http://localhost:48017",
            &["glm-4.7".to_string()],
        );
        let first = config.clone();

        runner.configure_provider(
            &mut config,
            "http://localhost:48017",
            &["glm-4.7".to_string()],
        );
        assert_eq!(config, first);
    }

    #[test]
    fn test_settings_config_default() {
        let config = PiSettingsConfig::default();
        assert_eq!(
            config.schema,
            "https://pi.dev/schemas/settings.schema.json".to_string()
        );
        assert!(config.rest.is_empty());
    }

    #[test]
    fn test_configure_settings_sets_klava_defaults() {
        let runner = PiRunner::new();
        let mut config = PiSettingsConfig::default();
        config.rest.insert("theme".to_string(), json!("fleet"));

        runner.configure_settings(
            &mut config,
            &[
                "deepseek-v4-flash".to_string(),
                "deepseek-v4-reason".to_string(),
            ],
        );

        assert_eq!(config.rest["defaultProvider"], json!("klava"));
        // Default model is the completion model (first id)
        assert_eq!(config.rest["defaultModel"], json!("deepseek-v4-flash"));
        // Unrelated settings survive
        assert_eq!(config.rest["theme"], json!("fleet"));
    }

    #[test]
    fn test_settings_config_serialization_round_trip() {
        let mut config = PiSettingsConfig::default();
        config
            .rest
            .insert("defaultProvider".to_string(), json!("klava"));
        config
            .rest
            .insert("defaultModel".to_string(), json!("glm-4.7"));

        let serialized = serde_json::to_string_pretty(&config).unwrap();
        let deserialized: PiSettingsConfig = serde_json::from_str(&serialized).unwrap();

        assert_eq!(config, deserialized);
    }
}
