use crate::error::Result;
use anyhow::anyhow;
use inquire::Text;
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub port: u16,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub api_key_name: Option<String>,
    pub reasoning_model: Option<String>,
    pub completion_model: Option<String>,
    pub verbose: bool,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            port: 3000,
            base_url: None,
            api_key: None,
            api_key_name: None,
            reasoning_model: None,
            completion_model: None,
            verbose: false,
        }
    }
}

impl Config {
    const APP_NAME: &str = "klava";

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
            println!("ℹ️  Config file already exists at: {}", path.display());
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
        println!("  base_url:         OpenAI-compatible API endpoint");
        println!("                    (e.g., https://openrouter.ai/api)");
        println!();
        println!("  api_key:          Optional API key");
        println!(
            "                    Also available via KLAVA_API_KEY or OPENROUTER_API_KEY env vars"
        );
        println!();
        println!("  api_key_name:     Custom environment variable name for API key");
        println!("                    If set, klava looks for this specific env var");
        println!();
        println!("  port:             Port to listen on");
        println!("                    (default: ), can be set via PORT env var");
        println!();
        println!("  reasoning_model:  Override reasoning model");
        println!("                    (e.g., anthropic/claude-opus)");
        println!();
        println!("  completion_model: Override completion model");
        println!("                    (e.g., anthropic/claude-sonnet)");
        println!();
        println!("  verbose:          Enable verbose logging");
        println!("                    Logs full request/response bodies");
        println!();
    }

    /// Write the config file template with commented examples
    fn write_template() -> Result<()> {
        let path = Self::get_path();
        let template = r#"# Klava Configuration File
# Copy uncommented lines to set your preferences

# Port to listen on (default: )
#port =

# OpenAI-compatible API base URL (required if not set via KLAVA_BASE_URL env var)
# Examples:
# - OpenRouter: https://openrouter.ai/api
# - OpenAI: https://api.openai.com
# - Local LLM: http://localhost:
#base_url = "https://openrouter.ai/api"

# API key (optional, can also be set via KLAVA_API_KEY or OPENROUTER_API_KEY env vars)
#api_key = "your-api-key"

# Custom API key environment variable name
# If set, klava will look for this specific env var instead of KLAVA_API_KEY
#api_key_name = "CUSTOM_API_KEY"

# Reasoning model override
#reasoning_model = "anthropic/claude-sonnet"

# Completion model override
#completion_model = "anthropic/claude-sonnet-4"

# Enable verbose logging (logs full request/response bodies)
#verbose = false
"#;

        fs::write(&path, template)?;
        Ok(())
    }

    pub fn load() -> Result<Config> {
        let path = confy::get_configuration_file_path(Self::APP_NAME, Some("config"))
            .expect("Failed to get path");

        tracing::debug!("Config path: {:?}", path);

        if let Ok(content) = fs::read_to_string(&path) {
            tracing::debug!("Current config contents:\n{}", content);
        }

        let cfg: Config = confy::load(Self::APP_NAME, Some("config"))?;
        Ok(cfg)
    }

    // pub fn save(cfg: &Config) -> Result<()> {
    //     confy::store(Self::APP_NAME, None, cfg)?;
    //     Ok(())
    // }

    pub fn from_config_and_env() -> anyhow::Result<Self> {
        let persistent_config =
            Self::load().map_err(|e| anyhow!("Failed to load persistent config: {}", e))?;
        tracing::debug!("📄 Loaded persistent config from confy");

        let port = env::var("PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(persistent_config.port);

        let base_url = env::var("KLAVA_BASE_URL")
            .ok()
            .or_else(|| persistent_config.base_url.clone());

        // If base_url is not set, prompt the user interactively
        let base_url = match base_url {
            Some(url) => url,
            None => {
                println!("❌ KLAVA_BASE_URL is not configured");
                println!();
                println!("Please provide your OpenAI-compatible API base URL:");
                println!("  • OpenRouter: https://openrouter.ai/api");
                println!("  • OpenAI: https://api.openai.com");
                println!("  • Local LLM: http://localhost:");
                println!();

                let answer = Text::new("Enter base URL:")
                    .with_help_message("e.g., https://openrouter.ai/api")
                    .prompt()
                    .map_err(|e| anyhow::anyhow!("Failed to get base URL: {}", e))?;

                if answer.trim().is_empty() {
                    return Err(anyhow::anyhow!(
                        "Base URL is required. Please run again and provide a valid URL."
                    ));
                }

                // Save the provided base URL to config
                let config_copy = &persistent_config;
                let updated_config = Config {
                    base_url: Some(answer.clone()),
                    ..config_copy.clone()
                };
                confy::store(Self::APP_NAME, Some("config"), &updated_config)?;
                println!("✓ Saved base URL to config file");
                println!();

                answer
            }
        };

        let api_key_name = persistent_config.api_key_name;

        let api_key = if let Some(key_name) = &api_key_name {
            env::var(key_name).ok()
        } else {
            env::var("KLAVA_API_KEY")
                .or_else(|_| env::var("OPENROUTER_API_KEY"))
                .ok()
                .or(persistent_config.api_key)
        }
        .filter(|k| !k.is_empty());

        let reasoning_model = env::var("REASONING_MODEL")
            .ok()
            .or(persistent_config.reasoning_model);

        let completion_model = env::var("COMPLETION_MODEL")
            .ok()
            .or(persistent_config.completion_model);

        let verbose = env::var("VERBOSE")
            .ok()
            .and_then(|v| {
                if v == "1" || v.to_lowercase() == "true" {
                    Some(true)
                } else if v == "0" || v.to_lowercase() == "false" {
                    Some(false)
                } else {
                    None
                }
            })
            .unwrap_or(persistent_config.verbose);

        if verbose {
            eprintln!("ℹ️  VERBOSE mode enabled (use RUST_LOG=trace for full details)");
        }

        if base_url.ends_with("/v1") {
            eprintln!("⚠️  WARNING: KLAVA_BASE_URL ends with '/v1'");
            eprintln!(
                "   This will result in URLs like: {}/v1/chat/completions",
                base_url
            );
            eprintln!("   Consider removing '/v1' from KLAVA_BASE_URL");
            eprintln!("   Correct: https://openrouter.ai/api");
            eprintln!("   Wrong:   https://openrouter.ai/api/v1");
        }

        Ok(Config {
            port,
            base_url: Some(base_url),
            api_key,
            api_key_name,
            reasoning_model,
            completion_model,
            verbose,
        })
    }

    pub fn chat_completions_url(&self) -> String {
        format!(
            "{}/v1/chat/completions",
            self.base_url.as_ref().unwrap().trim_end_matches('/')
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_url_construction_handles_various_base_urls() {
        let test_cases = vec![
            (
                "https://example.com",
                "https://example.com/v1/chat/completions",
            ),
            (
                "https://example.com/",
                "https://example.com/v1/chat/completions",
            ),
            (
                "https://openrouter.ai/api",
                "https://openrouter.ai/api/v1/chat/completions",
            ),
            (
                "https://openrouter.ai/api/",
                "https://openrouter.ai/api/v1/chat/completions",
            ),
            (
                "http://localhost:11434",
                "http://localhost:11434/v1/chat/completions",
            ),
        ];

        for (base_url, expected) in test_cases {
            let config = Config {
                port: 3000,
                base_url: Some(base_url.to_string()),
                api_key: None,
                api_key_name: None,
                reasoning_model: None,
                completion_model: None,
                verbose: false,
            };

            let url = config.chat_completions_url();
            assert_eq!(url, expected, "Failed for base_url: {}", base_url);
        }
    }
}
