//! Interactive CLI configuration setup
//!
//! Handles user prompts for provider-specific configuration.

use anyhow::Result;
use anyhow::anyhow;
use inquire::{Select, Text};
use klava::config::Config;
use klava::error::Error;
use klava::providers::setup_qwen;

/// Interactive configuration setup
pub struct InteractiveSetup {
    config: Config,
}

impl InteractiveSetup {
    pub fn new(config: Config) -> Self {
        Self { config }
    }

    pub async fn run(&mut self) -> Result<Config> {
        // If no active provider is configured, ask user to choose one first
        if self.config.active_provider.is_empty() {
            self.select_provider()?;
        }

        let provider_name = self.config.active_provider.as_str();

        // Find the active provider config
        let active_provider_config = self.config.get_active_provider_config().ok_or_else(|| {
            anyhow!(
                "Active provider '{}' not found",
                self.config.active_provider
            )
        })?;

        let provider_type = &active_provider_config.provider_type;

        println!("\nKlava Setup");
        println!();
        println!("Provider: {}", provider_name);
        println!();

        match provider_type {
            klava::providers::Type::QwenCode => {
                self.setup_qwen_free().await?;
            }
            klava::providers::Type::OpenAICompatible => {
                self.setup_default().await?;
            }
        }

        self.display_summary();

        Ok(self.config.clone())
    }

    /// Setup for qwen-code provider - just tell user to login
    async fn setup_qwen_free(&mut self) -> Result<()> {
        setup_qwen(&mut self.config).await?;

        Ok(())
    }

    /// Prompt user to select a provider when none is configured
    fn select_provider(&mut self) -> Result<()> {
        println!("\nNo provider configured.");
        println!();

        let options: Vec<String> =
            vec!["OpenAI-compatible (OpenRouter, OpenAI, DeepSeek, etc.)".to_string()];

        let selected = Select::new("Select a provider to configure:", options)
            .prompt()
            .map_err(|e| anyhow!("Failed to select provider: {}", e))?;

        let (provider_name, provider_type) = if selected.contains("OpenAI") {
            (
                "openai-compatible".to_string(),
                klava::providers::Type::OpenAICompatible,
            )
        } else {
            ("qwen".to_string(), klava::providers::Type::QwenCode)
        };

        // Set as active provider and add to providers list
        self.config.active_provider = provider_name.clone();

        use klava::providers::Config as ProviderConfig;
        let provider_config = ProviderConfig {
            name: provider_name.clone(),
            provider_type,
            base_url: None,
            api_key: None,
            api_key_name: None,
            reasoning_model: None,
            completion_model: None,
        };

        self.config.providers.push(provider_config);
        self.config.save()?;

        println!();

        Ok(())
    }

    /// Setup for default provider - prompt for base_url, api_key, models
    async fn setup_default(&mut self) -> Result<()> {
        // Base URL
        while self.config.resolve_base_url().is_none() {
            self.prompt_for_base_url().await?;
        }

        // API Key
        while self.config.resolve_api_key().is_none() {
            self.prompt_for_api_key().await?;
        }

        // Models (optional, will use defaults if not set)
        self.prompt_for_models().await?;

        Ok(())
    }

    /// Prompt for base URL
    async fn prompt_for_base_url(&mut self) -> Result<()> {
        println!("Enter your OpenAI-compatible API base URL:");
        println!("  • OpenRouter: https://openrouter.ai/api");
        println!("  • OpenAI: https://api.openai.com");
        println!("  • Local LLM: http://localhost:");
        println!();

        let answer = Text::new("Base URL:")
            .with_help_message("e.g., https://openrouter.ai/api")
            .prompt()?;

        if answer.trim().is_empty() {
            return Err(anyhow!("Base URL is required"));
        }

        let base_url = answer.trim().to_string();

        // Validate URL format
        if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
            return Err(anyhow!(
                "Invalid URL format. Must start with http:// or https://"
            ));
        }

        // Check and warn about /v1 suffix
        if base_url.ends_with("/v1") {
            println!("\n⚠️  WARNING: Base URL ends with '/v1'");
            println!("   Consider using: {}", base_url.trim_end_matches("/v1"));
        }

        // Update the active provider config
        self.config.update_active_provider_config(|c| {
            c.base_url = Some(base_url);
        })?;

        println!();

        Ok(())
    }

    /// Prompt for API key
    async fn prompt_for_api_key(&mut self) -> Result<()> {
        println!("Enter your API key:");
        println!("  • For OpenRouter: Get from https://openrouter.ai/keys");
        println!("  • For OpenAI: Get from https://platform.openai.com/api-keys");
        println!();

        let answer = Text::new("API key:")
            .with_help_message("Your OpenRouter/OpenAI API key")
            .prompt()?;

        if answer.trim().is_empty() {
            return Err(anyhow!("API key is required"));
        }

        // Update the active provider config
        self.config.update_active_provider_config(|c| {
            c.api_key = Some(answer.trim().to_string());
        })?;

        println!();

        Ok(())
    }

    /// Prompt for models (optional)
    async fn prompt_for_models(&mut self) -> Result<()> {
        println!("Configure models (optional - press Enter to skip):");
        println!();

        let reasoning_answer = Text::new("Reasoning model:")
            .with_help_message("e.g., anthropic/claude-opus")
            .prompt()?;

        if !reasoning_answer.trim().is_empty() {
            // Update the active provider config
            self.config.update_active_provider_config(|c| {
                c.reasoning_model = Some(reasoning_answer.trim().to_string());
            })?;
        }

        let completion_answer = Text::new("Completion model:")
            .with_help_message("e.g., anthropic/claude-sonnet")
            .prompt()?;

        if !completion_answer.trim().is_empty() {
            // Update the active provider config
            self.config.update_active_provider_config(|c| {
                c.completion_model = Some(completion_answer.trim().to_string());
            })?;
        }

        println!();

        Ok(())
    }

    /// Display configuration summary
    fn display_summary(&self) {
        let config = &self.config;
        let provider_name = &self.config.active_provider;

        println!("✓ Configuration complete:");
        println!();
        println!("  Provider: {}", provider_name);

        if let Some(active_config) = config.get_active_provider_config() {
            if let Some(base_url) = &active_config.base_url {
                println!("  Base URL: {}", base_url);
            }

            if let Some(model) = &active_config.reasoning_model {
                println!("  Reasoning model: {}", model);
            }

            if let Some(model) = &active_config.completion_model {
                println!("  Completion model: {}", model);
            }

            if active_config.needs_api_key() {
                println!(
                    "  API Key: {}",
                    mask_api_key(active_config.api_key.as_deref())
                );
            }
        } else {
            println!("  Warning: Active provider configuration not found");
        }
    }
}

/// Build config with interactive setup on validation errors
pub async fn build_config_interactive() -> Result<Config> {
    let persistent = match Config::load() {
        Ok(config) => config,
        Err(_) => {
            // If config doesn't exist, create a default one first
            Config::ensure_exists(false)
                .map_err(|e| anyhow!("Failed to create default config: {}", e))?;
            Config::load().map_err(|e| anyhow!("Failed to load config after creation: {}", e))?
        }
    };

    // Validate the loaded config directly
    if let Err(e) = persistent.validate_complete() {
        match e {
            Error::ActiveProviderNotSetup | Error::MissingBaseUrl | Error::MissingApiKey(_) => {
                let mut setup = InteractiveSetup::new(persistent);
                return setup.run().await;
            }
            _ => return Err(anyhow!("Configuration error: {}", e)),
        }
    }

    // If validation passes, return a config with environment variables applied
    // by creating a resolved version similar to build_resolved
    let resolved_config = Config {
        port: persistent.port,
        active_provider: persistent.active_provider.clone(),
        providers: persistent.providers.clone(),
        verbose: persistent.verbose,
    };

    Ok(resolved_config)
}

/// Force run interactive setup (for explicit setup)
pub async fn run_interactive_setup() -> Result<Config> {
    let persistent = Config::load()?;

    let mut setup = InteractiveSetup::new(persistent);
    setup.run().await
}

/// Mask API key for safe display
fn mask_api_key(api_key: Option<&str>) -> String {
    match api_key {
        Some(key) if !key.is_empty() => {
            if key.len() >= 8 {
                format!("{}***", &key[..8])
            } else {
                format!("{}***", key)
            }
        }
        _ => "***".to_string(),
    }
}
