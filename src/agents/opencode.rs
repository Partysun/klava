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

            if let Some(config_content) = std::fs::read_to_string(&config_path).ok() {
                if let Ok(config) = serde_json::from_str::<OpenCodeConfig>(&config_content) {
                    if let Some(klava_provider) = config.provider.get("klava") {
                        for (name, cfg) in &klava_provider.models {
                            if let serde_json::Value::Object(cfg_map) = cfg
                                && Self::is_klava_model(cfg_map)
                            {
                                models.push(name.clone());
                            }
                        }
                    }
                }
            }
        }

        models.sort();
        models
    }

    pub fn is_klava_model(cfg: &serde_json::Map<String, serde_json::Value>) -> bool {
        if let Some(serde_json::Value::Bool(true)) = cfg.get("_klava") {
            return true;
        }

        if let Some(serde_json::Value::String(name)) = cfg.get("name") {
            return name == "klava-smart";
        }

        false
    }
}

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

    fn run(
        &self,
        args: &[String],
        _proxy_url: &str,
    ) -> impl std::future::Future<Output = Result<(), anyhow::Error>> + Send {
        async move {
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
            .or_insert_with(|| serde_json::json!(base_url));

        // Ensure klava-smart model exists with _klava marker
        let model_entry = if let Some(existing) = provider.models.get("klava-smart") {
            // Update existing entry only if needed
            let mut entry = existing.clone();
            if let serde_json::Value::Object(ref mut map) = entry {
                map.entry("_klava".to_string())
                    .or_insert_with(|| serde_json::json!(true));
            }
            entry
        } else {
            // Create new model entry
            serde_json::json!({
                "name": "klava-smart",
                "_klava": true
            })
        };

        provider
            .models
            .insert("klava-smart".to_string(), model_entry);

        // Generate new config JSON
        let new_config = serde_json::to_string_pretty(&config)?;

        // Check if there are actual changes
        if old_config_str.trim() == new_config.trim() {
            // No changes needed
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
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
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

/// Provider configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// OpenCode state file structure
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OpenCodeState {
    #[serde(default)]
    pub recent: Vec<RecentEntry>,

    #[serde(default)]
    pub favorite: Vec<serde_json::Value>,

    #[serde(default)]
    pub variant: HashMap<String, serde_json::Value>,
}

/// Recent model entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecentEntry {
    pub provider_id: String,
    pub model_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_klava_model() {
        use serde_json::{Map, json};

        let mut cfg = Map::new();
        cfg.insert("_klava".to_string(), json!(true));
        assert!(OpencodeRunner::is_klava_model(&cfg));

        let mut cfg2 = Map::new();
        cfg2.insert("name".to_string(), json!("klava-smart"));
        assert!(OpencodeRunner::is_klava_model(&cfg2));

        let cfg3 = Map::new();
        assert!(!OpencodeRunner::is_klava_model(&cfg3));
    }

    // this test is only to check how serde default works with external function default_schema()
    #[test]
    fn test_opencode_config() {
        let json_str = "{}"; // Missing "$schema"
        let config_from_json: OpenCodeConfig = serde_json::from_str(json_str).unwrap();
        println!("{}", config_from_json.schema);
        assert!(default_schema() == config_from_json.schema);
    }
}
