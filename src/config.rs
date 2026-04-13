use crate::error::{Error, Result};
use crate::providers::{Config as ProviderConfig, Type as ProviderType};
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub port: u16,
    #[serde(default)]
    pub verbose: bool,
    #[serde(default)]
    pub active_provider: String,
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: 48017,
            active_provider: "qwen".to_string(),
            providers: vec![ProviderConfig {
                name: "qwen".to_string(),
                provider_type: ProviderType::QwenCode,
                base_url: None,
                api_key: None,
                api_key_name: None,
                reasoning_model: None,
                completion_model: None,
            }],
            verbose: false,
        }
    }
}

impl Config {
    pub const APP_NAME: &str = "klava";

    /// Get the config file path
    pub fn get_path() -> std::path::PathBuf {
        confy::get_configuration_file_path(Self::APP_NAME, Some("config"))
            .expect("Failed to get path")
    }

    /// Create the config directory if it doesn't exist
    pub fn ensure_dir() -> Result<()> {
        let path = Self::get_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        Ok(())
    }

    /// Create config file with default values if it doesn't exist
    pub fn ensure_exists(force: bool) -> Result<std::path::PathBuf> {
        let path = Self::get_path();

        if force || !path.exists() {
            Self::ensure_dir()?;
            Self::write_template()?;
            println!("✓ Created config file at: {}", path.display());
            println!();
        } else {
            println!("Config file already exists at: {}", path.display());
            println!();
        }

        // Show configuration details
        Self::show_config_details();

        Ok(path)
    }

    /// Show detailed information about configuration fields
    fn show_config_details() {
        println!("📝 Configuration fields:");
        println!();
        println!("active_provider:  Currently active provider");
        println!("                  Run 'klava providers' to see all available options");
        println!("providers:        List of provider configurations");
        println!("Each provider has its own settings (name, type, base_url, etc.)");
        println!();
        println!("port:             Port to listen on");
        println!("                  (default: 48017), can be set via PORT env var");
        println!();
        println!("verbose:          Enable verbose logging");
        println!("                  Logs full request/response bodies");
        println!();
    }

    /// Write the config file template with commented examples
    fn write_template() -> Result<()> {
        let path = Self::get_path();
        let template = r#"# Klava Configuration File
# Multi-provider configuration
# Copy uncommented lines to set your preferences

# Currently active provider
active_provider = "qwen"

// # List of provider configurations
// [[providers]]
// name = "default"
// type = "openai-compatible"

[[providers]]
name = "qwen"
type = "qwen-code"

# Port to listen on (default: 48017)
# Can also be set via PORT env var
#port = 48017

# Enable verbose logging (logs full request/response bodies)
#verbose = false
"#;

        fs::write(&path, template)?;
        Ok(())
    }

    pub fn load() -> Result<Config> {
        let path = Config::get_path();
        let cfg: Config = confy::load_path(path)?;
        Ok(cfg)
    }

    pub fn save(&self) -> Result<()> {
        let path = Config::get_path();
        confy::store_path(path, self)?;
        Ok(())
    }

    /// Partial update of config (only writes provided fields)
    pub fn update<F>(mut self, update_fn: F) -> Result<Self>
    where
        F: FnOnce(&mut Self),
    {
        update_fn(&mut self);
        self.save()?;
        Ok(self)
    }

    /// Get the active provider configuration
    pub fn get_active_provider_config(&self) -> Option<&ProviderConfig> {
        self.providers
            .iter()
            .find(|p| p.name == self.active_provider)
    }

    /// Get a specific provider configuration by name
    pub fn get_provider_config(&self, name: &str) -> Option<&ProviderConfig> {
        self.providers.iter().find(|p| p.name == name)
    }

    pub fn resolve_base_url(&self) -> Option<String> {
        self.get_active_provider_config()?.resolve_base_url()
    }

    pub fn resolve_api_key(&self) -> Option<String> {
        let config_to_use = self.get_active_provider_config();

        if let Some(config) = config_to_use {
            if let Some(key_name) = &config.api_key_name {
                env::var(key_name).ok()
            } else {
                config.api_key.clone()
            }
            .filter(|k| !k.is_empty())
        } else {
            None
        }
    }

    /// Resolve reasoning model for the active provider configuration
    pub fn resolve_reasoning_model(&self) -> Option<String> {
        let active_config = self.get_active_provider_config()?;
        active_config.reasoning_model()
    }

    /// Resolve completion model for the active provider configuration
    pub fn resolve_completion_model(&self) -> Option<String> {
        let active_config = self.get_active_provider_config()?;
        active_config.completion_model()
    }

    /// Validate base URL format
    pub fn validate_base_url(&self, base_url: &str) -> Result<()> {
        if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
            return Err(Error::InvalidBaseUrl(base_url.to_string()));
        }
        Ok(())
    }

    /// Validate configuration is complete for running
    pub fn validate_complete(&self) -> Result<()> {
        // Find the active provider config
        let active_provider_config = self.get_active_provider_config().ok_or_else(|| {
            Error::Internal(format!(
                "Active provider '{}' not found",
                self.active_provider
            ))
        })?;

        tracing::debug!("{:?}", self);
        tracing::debug!("{:?}", &active_provider_config.name);

        // Validate using the current config - the resolution methods will check environment vars
        active_provider_config.validate_config(self)?;

        Ok(())
    }

    pub fn resolve_api_key_from_config(&self, provider_config: &ProviderConfig) -> Option<String> {
        if let Some(key_name) = &provider_config.api_key_name {
            env::var(key_name).ok() // Still allow custom env var name from config
        } else {
            provider_config.api_key.clone() // Only from config file
        }
        .filter(|k| !k.is_empty())
    }

    /// Resolve base URL for a specific provider config - only from config file (no env var fallbacks)
    pub fn resolve_base_url_from_config(&self, provider_config: &ProviderConfig) -> Option<String> {
        provider_config.base_url.clone()
    }

    /// Update the active provider config
    pub fn update_active_provider_config<F>(&mut self, update_fn: F) -> Result<()>
    where
        F: FnOnce(&mut ProviderConfig),
    {
        // Find the index of the active provider
        let active_index = self
            .providers
            .iter()
            .position(|p| p.name == self.active_provider)
            .ok_or_else(|| {
                Error::Internal(format!(
                    "Active provider '{}' not found in providers list",
                    self.active_provider
                ))
            })?;

        // Apply the update function to the provider at that index
        let provider = &mut self.providers[active_index];
        update_fn(provider);

        self.save()
    }

    pub fn chat_completions_url(&self) -> String {
        let base_url = self
            .resolve_base_url() // None means use active provider config
            .expect("Base URL not configured for active provider");
        format!("{}/v1/chat/completions", base_url.trim_end_matches('/'))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_default_creates_default_provider() {
        let config = Config::default();
        assert_eq!(config.active_provider, "qwen");
        assert_eq!(config.providers.len(), 1);
        assert_eq!(config.providers[0].name, "qwen");
        assert_eq!(config.providers[0].provider_type, ProviderType::QwenCode);
    }

    #[test]
    fn test_get_active_provider_config_returns_correct_provider() {
        let config = Config {
            port: 3000,
            active_provider: "test_provider".to_string(),
            providers: vec![
                ProviderConfig {
                    name: "test_provider".to_string(),
                    provider_type: ProviderType::OpenAICompatible,
                    base_url: Some("https://test.example.com".to_string()),
                    api_key: None,
                    api_key_name: None,
                    reasoning_model: Some("alisyao".to_string()),
                    completion_model: None,
                },
                ProviderConfig {
                    name: "other_provider".to_string(),
                    provider_type: ProviderType::OpenAICompatible,
                    base_url: Some("https://other.example.com".to_string()),
                    api_key: None,
                    api_key_name: None,
                    reasoning_model: None,
                    completion_model: None,
                },
            ],
            verbose: false,
        };

        let active_config = config.get_active_provider_config().unwrap();
        assert_eq!(active_config.name, "test_provider");
        assert_eq!(
            active_config.base_url.as_ref().unwrap(),
            "https://test.example.com"
        );
    }

    #[test]
    fn test_get_provider_config_returns_correct_provider() {
        let config = Config {
            port: 3000,
            active_provider: "second_provider".to_string(),
            providers: vec![
                ProviderConfig {
                    name: "first_provider".to_string(),
                    provider_type: ProviderType::OpenAICompatible,
                    base_url: Some("https://first.example.com".to_string()),
                    api_key: None,
                    api_key_name: None,
                    reasoning_model: None,
                    completion_model: None,
                },
                ProviderConfig {
                    name: "second_provider".to_string(),
                    provider_type: ProviderType::OpenAICompatible,
                    base_url: Some("https://second.example.com".to_string()),
                    api_key: None,
                    api_key_name: None,
                    reasoning_model: None,
                    completion_model: None,
                },
            ],
            verbose: false,
        };

        let provider_config = config.get_provider_config("second_provider").unwrap();
        assert_eq!(
            config.resolve_base_url().unwrap().as_str(),
            "https://second.example.com"
        );
        assert_eq!(provider_config.name, "second_provider");
        assert_eq!(
            provider_config.base_url.as_ref().unwrap(),
            "https://second.example.com"
        );
    }

    #[test]
    fn test_resolve_methods_work_with_active_provider() {
        let config = Config {
            port: 3000,
            active_provider: "test".to_string(),
            providers: vec![ProviderConfig {
                name: "test".to_string(),
                provider_type: ProviderType::OpenAICompatible,
                base_url: Some("https://test.example.com".to_string()),
                api_key: Some("test-key".to_string()),
                api_key_name: None,
                reasoning_model: Some("test-reasoning-model".to_string()),
                completion_model: Some("test-completion-model".to_string()),
            }],
            verbose: false,
        };

        let active_config = config.get_active_provider_config().unwrap();
        assert_eq!(active_config.name, "test");
        assert_eq!(config.port, 3000);
    }
}
