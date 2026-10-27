use crate::agents::AgentRunner;
use crate::utils::{ask_user_approval, generate_diff, write_with_backup};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::process::Stdio;
use tokio::process::Command as TokioCommand;
use which::which;

pub struct OpencodeRunner;

impl OpencodeRunner {
    pub const fn name_static() -> &'static str {
        "opencode"
    }

    pub fn new() -> Self {
        Self
    }

    /// Get list of currently configured models
    pub fn model(&self) -> Vec<String> {
        let mut models = Vec::new();

        for config_path in self.paths() {
            // Only read from opencode.json, skip state files
            if config_path.file_name().and_then(|n| n.to_str()) != Some("opencode.json") {
                continue;
            }

            let Ok(config_content) = std::fs::read_to_string(&config_path) else {
                continue;
            };

            let Ok(config) = serde_json::from_str::<OpenCodeConfig>(&config_content) else {
                continue;
            };

            let mut config_models = Self::extract_klava_models(&config);
            models.append(&mut config_models);
        }

        models.sort();
        models
    }

    /// Extract klava models from a config
    fn extract_klava_models(config: &OpenCodeConfig) -> Vec<String> {
        let mut models = Vec::new();
        if let Some(klava_provider) = config.provider.get("klava") {
            for (name, cfg) in &klava_provider.models {
                if let serde_json::Value::Object(cfg_map) = cfg
                    && Self::is_klava_model(cfg_map)
                {
                    models.push(name.clone());
                }
            }
        }
        models.sort();
        models
    }

    /// Distinct configured model names, completion model first.
    fn configured_model_names() -> Vec<String> {
        let Ok(config) = crate::config::Config::load() else {
            return Vec::new();
        };
        let mut names = Vec::new();
        for name in [
            config.resolve_completion_model(),
            config.resolve_reasoning_model(),
        ]
        .into_iter()
        .flatten()
        {
            if !names.contains(&name) {
                names.push(name);
            }
        }
        for name in config.model_names() {
            if !names.contains(&name) {
                names.push(name);
            }
        }
        names
    }

    /// Write klava's effort policy into an opencode model entry.
    ///
    /// `reasoning: true` tells opencode the model reasons, which is what makes
    /// it offer effort variants for a provider model it has no models.dev entry
    /// for. `variants.<level>.disabled` hides the levels the configured cap
    /// forbids, so the picker never offers a value klava would only clamp.
    ///
    /// opencode turns the selected variant into `reasoning_effort` in the
    /// request body; klava then treats it as the client's own level, which
    /// outranks every configured default.
    fn apply_effort_settings(
        map: &mut serde_json::Map<String, serde_json::Value>,
        config: &crate::config::Config,
        name: &str,
    ) {
        let Some(entry) = config.pool(name).first().copied() else {
            return;
        };
        let policy = crate::effort::EffortPolicy::for_entry(config, entry);
        if !policy.is_configured() {
            return;
        }

        map.insert("reasoning".to_string(), serde_json::json!(true));

        let Some(cap) = policy.cap() else {
            return;
        };
        let mut disabled = serde_json::Map::new();
        for level in crate::models::openai::ReasoningEffort::ALL {
            if level.rank() > cap.rank() {
                disabled.insert(
                    level.variant_name().to_string(),
                    serde_json::json!({ "disabled": true }),
                );
            }
        }
        if disabled.is_empty() {
            return;
        }
        match map.get_mut("variants") {
            Some(serde_json::Value::Object(existing)) => {
                for (level, value) in disabled {
                    existing.insert(level, value);
                }
            }
            _ => {
                map.insert("variants".to_string(), serde_json::Value::Object(disabled));
            }
        }
    }

    pub fn is_klava_model(cfg: &serde_json::Map<String, serde_json::Value>) -> bool {
        if let Some(serde_json::Value::Bool(true)) = cfg.get("_klava") {
            return true;
        }

        if let Some(serde_json::Value::String(name)) = cfg.get("name") {
            return name == "klava";
        }

        false
    }
}

impl Default for OpencodeRunner {
    fn default() -> Self {
        Self::new()
    }
}

// Works on Linux & MacOS
impl AgentRunner for OpencodeRunner {
    fn name(&self) -> &'static str {
        Self::name_static()
    }

    fn paths(&self) -> Vec<std::path::PathBuf> {
        let mut paths = Vec::new();

        if let Some(home) = dirs::home_dir() {
            let config_path = home.join(".config").join("opencode").join("opencode.json");
            // Always include config path for setup
            paths.push(config_path);

            let state_path = home
                .join(".local")
                .join("state")
                .join("opencode")
                .join("model.json");
            if state_path.exists() {
                paths.push(state_path);
            }
        }

        paths
    }

    fn check_installation() -> Result<(), anyhow::Error> {
        which(Self::name_static()).map_err(|_| {
            anyhow::anyhow!("opencode is not installed. Install from https://opencode.ai")
        })?;
        Ok(())
    }

    async fn run(&self, args: &[String], _proxy_url: &str) -> Result<(), anyhow::Error> {
        let mut cmd = TokioCommand::new(self.name());
        cmd.args(args)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());

        let status = cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!("Failed to execute opencode: {}", e))?
            .wait()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to wait for opencode: {}", e))?;

        if !status.success() {
            anyhow::bail!("opencode exited with status: {}", status);
        }

        Ok(())
    }

    async fn setup(&self, proxy_url: &str) -> Result<(), anyhow::Error> {
        // Get config path from paths() method
        let config_path = self
            .paths()
            .into_iter()
            .find(|p| p.file_name().and_then(|n| n.to_str()) == Some("opencode.json"))
            .ok_or_else(|| anyhow::anyhow!("Could not find opencode.json config path"))?;

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

        let mut config: OpenCodeConfig = if old_config_str.is_empty() {
            OpenCodeConfig::default()
        } else {
            serde_json::from_str(&old_config_str).unwrap_or_default()
        };
        let old_config = config.clone();

        // Build proxy URL with /v1 suffix
        let base_url = format!("{}/v1", proxy_url);

        // Ensure provider structure exists
        let provider = config
            .provider
            .entry("klava".to_string())
            .or_insert_with(|| ProviderConfig {
                npm: "@ai-sdk/openai-compatible".to_string(),
                name: "Klava".to_string(),
                options: HashMap::new(),
                models: HashMap::new(),
            });

        // Update or set baseURL
        provider
            .options
            .entry("baseURL".to_string())
            .and_modify(|v| *v = serde_json::json!(base_url))
            .or_insert_with(|| serde_json::json!(base_url));

        // Expose every configured model under its real name, so the picker and
        // the model label show e.g. `~deepseek/deepseek-flash-latest` instead
        // of a synthetic proxy name. klava routes by that name.
        let model_names = Self::configured_model_names();
        let klava_config = crate::config::Config::load().ok();
        for name in &model_names {
            let mut entry = if let Some(existing) = provider.models.get(name) {
                let mut entry = existing.clone();
                if let serde_json::Value::Object(ref mut map) = entry {
                    map.entry("_klava".to_string())
                        .or_insert_with(|| serde_json::json!(true));
                }
                entry
            } else {
                serde_json::json!({
                    "name": name,
                    "_klava": true
                })
            };

            // Mirror klava's effort policy into opencode's model entry: the
            // capability unlocks the variant picker, and levels above the
            // configured cap are marked unsupported so the picker cannot offer
            // a value klava would only clamp anyway.
            if let Some(config) = &klava_config
                && let serde_json::Value::Object(ref mut map) = entry
            {
                Self::apply_effort_settings(map, config, name);
            }

            provider.models.insert(name.clone(), entry);
        }

        // Drop klava-managed entries that are no longer configured (and the
        // old synthetic one), so the picker stops offering a model klava would
        // silently reroute to a default.
        if !model_names.is_empty() {
            let stale: Vec<String> = provider
                .models
                .iter()
                .filter(|(name, cfg)| {
                    !model_names.contains(name)
                        && Self::is_klava_model(cfg.as_object().unwrap_or(&Default::default()))
                })
                .map(|(name, _)| name.clone())
                .collect();
            for name in stale {
                provider.models.remove(&name);
            }
        }

        // Point the default and the small model at real names too.
        if let Some(default) = model_names.first() {
            let qualified = format!("klava/{default}");
            config.model = Some(qualified.clone());
            if config.small_model.is_none() {
                config.small_model = Some(qualified);
            }
        }

        let new_config = serde_json::to_string_pretty(&config)?;

        if old_config == config {
            return Ok(());
        }

        // Show diff and ask for approval
        if !old_config_str.is_empty() {
            let diff = generate_diff(&old_config_str, &new_config, &config_path);
            println!("\nProposed changes to opencode.json:");
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
            .map_err(|e| anyhow::anyhow!("Failed to write opencode.json: {}", e))?;

        println!("✓ Configuration updated successfully");

        Ok(())
    }
}

// OpenCodeConfig part

/// OpenCode main configuration structure
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct OpenCodeConfig {
    #[serde(rename = "$schema", default = "default_schema")]
    pub schema: String,

    #[serde(default)]
    pub provider: HashMap<String, ProviderConfig>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub small_model: Option<String>,
}

fn default_schema() -> String {
    "https://opencode.ai/config.json".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProviderConfig {
    pub npm: String,
    pub name: String,

    #[serde(default)]
    pub options: HashMap<String, serde_json::Value>,

    #[serde(default)]
    pub models: HashMap<String, serde_json::Value>,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            npm: "@ai-sdk/openai-compatible".to_string(),
            name: "Klava".to_string(),
            options: HashMap::new(),
            models: HashMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use serde_json::json;

    #[test]
    fn test_is_klava_model() {
        use serde_json::{Map, json};

        let mut cfg = Map::new();
        cfg.insert("_klava".to_string(), json!(true));
        assert!(OpencodeRunner::is_klava_model(&cfg));

        let mut cfg2 = Map::new();
        cfg2.insert("name".to_string(), json!("klava"));
        assert!(OpencodeRunner::is_klava_model(&cfg2));

        let cfg3 = Map::new();
        assert!(!OpencodeRunner::is_klava_model(&cfg3));
    }

    fn effort_config(global: Option<&str>, model: Option<&str>, cap: Option<&str>) -> Config {
        Config {
            reasoning_effort: global.map(str::to_string),
            reasoning_effort_cap: cap.map(str::to_string),
            models: vec![crate::providers::Model {
                name: "~deepseek/deepseek-flash-latest".to_string(),
                reasoning_effort: model.map(str::to_string),
                ..crate::providers::Model::default()
            }],
            ..Config::default()
        }
    }

    #[test]
    fn effort_settings_unlock_the_variant_picker() {
        let config = effort_config(Some("high"), None, None);
        let mut entry = serde_json::Map::new();
        OpencodeRunner::apply_effort_settings(
            &mut entry,
            &config,
            "~deepseek/deepseek-flash-latest",
        );
        assert_eq!(entry["reasoning"], json!(true));
        // No cap: every level opencode knows stays available.
        assert!(entry.get("variants").is_none());
    }

    #[test]
    fn the_cap_disables_the_variants_above_it() {
        let config = effort_config(None, Some("max"), Some("high"));
        let mut entry = serde_json::Map::new();
        OpencodeRunner::apply_effort_settings(
            &mut entry,
            &config,
            "~deepseek/deepseek-flash-latest",
        );
        let variants = entry["variants"].as_object().unwrap();
        assert_eq!(variants["xhigh"]["disabled"], json!(true));
        assert_eq!(variants["max"]["disabled"], json!(true));
        assert!(!variants.contains_key("high"));
        assert!(!variants.contains_key("low"));
    }

    #[test]
    fn no_effort_config_leaves_the_model_entry_alone() {
        let config = effort_config(None, None, None);
        let mut entry = serde_json::Map::new();
        OpencodeRunner::apply_effort_settings(
            &mut entry,
            &config,
            "~deepseek/deepseek-flash-latest",
        );
        assert!(entry.is_empty(), "entry: {entry:?}");
    }

    #[test]
    fn test_opencode_config() {
        let json_str = "{}"; // Missing "$schema"
        let config_from_json: OpenCodeConfig = serde_json::from_str(json_str).unwrap();
        assert!(default_schema() == config_from_json.schema);
    }

    #[test]
    fn test_extract_klava_models() {
        let config = json!({
            "provider": {
                "klava": {
                    "npm": "@ai-sdk/openai-compatible",
                    "name": "Klava",
                    "models": {
                        "model1": {
                            "_klava": true
                        },
                        "model2": {
                            "name": "klava"
                        },
                        "invalid": {
                            "other": "field"
                        }
                    }
                }
            }
        });
        let config: OpenCodeConfig = serde_json::from_value(config).unwrap();

        let models = OpencodeRunner::extract_klava_models(&config);
        assert_eq!(models, vec!["model1", "model2"]);
    }

    #[test]
    fn test_extract_klava_models_empty() {
        let config = json!({
            "provider": {
                "klava": {
                    "npm": "@ai-sdk/openai-compatible",
                    "name": "Klava",
                    "models": {}
                }
            }
        });
        let config: OpenCodeConfig = serde_json::from_value(config).unwrap();

        let models = OpencodeRunner::extract_klava_models(&config);
        assert_eq!(models, Vec::<String>::new());
    }

    #[test]
    fn test_extract_klava_models_no_klava_provider() {
        let config = json!({
            "provider": {
                "other": {
                    "npm": "@ai-sdk/openai-compatible",
                    "name": "Other",
                    "models": {
                        "model1": {
                            "_klava": true
                        }
                    }
                }
            }
        });
        let config: OpenCodeConfig = serde_json::from_value(config).unwrap();

        let models = OpencodeRunner::extract_klava_models(&config);
        assert_eq!(models, Vec::<String>::new());
    }
}
