use crate::error::{Error, Result};
use crate::providers::Config as ProviderConfig;
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub port: u16,
    #[serde(default)]
    pub verbose: bool,
    /// Scan request payloads for PII (emails, crypto addresses) and redact them.
    #[serde(default = "default_enable_pii")]
    pub enable_pii: bool,
    /// Log an estimated token count for each request.
    #[serde(default)]
    pub enable_token_stats: bool,
    #[serde(default)]
    pub active_provider: String,
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,

    // ---- Web search -------------------------------------------------------
    /// Offer a web-search tool to the agent and run searches through the
    /// active provider. On by default; it only makes sense for providers
    /// that carry a search backend.
    #[serde(default = "default_enable_websearch")]
    pub enable_websearch: bool,
    /// Model used to run the search request. Defaults to the active
    /// completion model when unset.
    #[serde(default)]
    pub search_model: Option<String>,
    /// Search backend family (klava vocabulary): `web_fast`, `web_deep`,
    /// `web_native`, `yandex`, or `auto`. Maps to the internal engine/mode
    /// pair. `yandex` runs through the klava gateway (Yandex Search + a
    /// synthesis completion) and is only available with the klava provider.
    #[serde(default = "default_search_engine")]
    pub search_engine: String,
    /// How much page context to pull into the answer: `low`, `medium`, `high`.
    #[serde(default = "default_search_context")]
    pub search_context: String,
    /// Upper bound on results the backend may consider.
    #[serde(default = "default_search_max_results")]
    pub search_max_results: u32,
    /// Instruction prepended to the generated search query.
    #[serde(default)]
    pub search_prompt: Option<String>,
    /// Restrict the search to these domains (empty = no restriction).
    #[serde(default)]
    pub search_include_domains: Vec<String>,
    /// Exclude these domains from the search (empty = no restriction).
    #[serde(default)]
    pub search_exclude_domains: Vec<String>,
    /// Let the decision model tune the search settings per query. When off,
    /// the static config values above are used as-is.
    #[serde(default = "default_search_decide")]
    pub search_decide: bool,
    /// Decision model id used to tune the search settings.
    #[serde(default = "default_search_decide_model")]
    pub search_decide_model: String,

    /// Built-in, always-compiled extensions (see `crate::extensions`).
    #[serde(default)]
    pub extensions: ExtensionsConfig,
}

/// Extension settings (`[extensions]`).
///
/// Deliberately opaque: klava stores whatever each extension crate declares
/// under `settings.<name>`, so adding an extension never touches this file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExtensionsConfig {
    /// Per-extension tables, keyed by whatever the user installed.
    ///
    /// Every table may carry `enabled` (the only key the host interprets);
    /// all other keys are opaque and passed to the extension as-is.
    #[serde(default)]
    pub settings: std::collections::BTreeMap<String, serde_json::Value>,

    /// Legacy `enable_pii = false`, expressed without naming an extension.
    ///
    /// Handed to every extension as part of its config; only one that cares
    /// about PII will act on it.
    #[serde(default)]
    pub disable_pii: bool,
}

fn default_enable_pii() -> bool {
    true
}

fn default_enable_websearch() -> bool {
    true
}

fn default_search_engine() -> String {
    "auto".to_string()
}

fn default_search_context() -> String {
    "medium".to_string()
}

fn default_search_max_results() -> u32 {
    5
}

fn default_search_decide() -> bool {
    true
}

fn default_search_decide_model() -> String {
    "typesafe/jev-1.13".to_string()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: 48017,
            active_provider: "".to_string(),
            providers: vec![],
            verbose: false,
            enable_pii: true,
            enable_token_stats: false,
            enable_websearch: default_enable_websearch(),
            search_model: None,
            search_engine: default_search_engine(),
            search_context: default_search_context(),
            search_max_results: default_search_max_results(),
            search_prompt: None,
            search_include_domains: vec![],
            search_exclude_domains: vec![],
            search_decide: default_search_decide(),
            search_decide_model: default_search_decide_model(),
            extensions: ExtensionsConfig::default(),
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
        println!("enable_websearch: Offer the agent a web-search tool (default: true)");
        println!("                  Only active with the klava provider and klava login");
        println!();
    }

    /// Write the config file template with commented examples
    fn write_template() -> Result<()> {
        let path = Self::get_path();
        let template = r#"# Klava Configuration File
# Multi-provider configuration
# Uncomment and configure the provider you want to use

# Currently active provider (uncomment and set your provider name)
# active_provider = "openai-compatible"

# OpenAI-compatible provider (e.g., OpenRouter, OpenAI, DeepSeek)
# [[providers]]
# name = "openai-compatible"
# type = "openai-compatible"
# base_url = "https://openrouter.ai/api"
# api_key = "your-api-key-here"

# Qwen Code provider (Mimic as qwen-code, you need to have qwen-code paid plan)
# [[providers]]
# name = "qwen"
# type = "qwen-code"

# Klava provider (klava.zatsepin.dev) — sign in with `klava login` (browser
# device OAuth). The key and default model are written automatically.
# [[providers]]
# name = "klava"
# type = "klava"

# Port to listen on (default: 48017)
# Can also be set via PORT env var
#port = 48017

# Enable verbose logging (logs full request/response bodies)
#verbose = false

# Scan request payloads for PII (emails, crypto addresses) and redact them (default: true)
#enable_pii = true

# Log an estimated token count for each request (default: false)
#enable_token_stats = false

# ---- Web search ----------------------------------------------------------
# Offer a web-search tool to agents. Only active with the klava provider
# (the search backend is reachable only through the klava gateway), and only
# when klava is signed in (`klava login`) or KLAVA_API_KEY is set. When the
# model calls the tool, klava intercepts it, optionally asks a decision model
# to tune the settings, runs the search, and feeds the result back — the
# agent never sees the tool call itself.
#enable_websearch = true

# Model used to run the search. Defaults to the active completion model.
#search_model = ""

# Search backend family: "auto", "web_fast", "web_deep", "web_native" or
# "yandex" (Yandex Search via the klava gateway).
#search_engine = "auto"

# How much page context to pull into the answer: "low", "medium" or "high".
#search_context = "medium"

# Upper bound on results the backend may consider (default: 5).
#search_max_results = 5

# Instruction prepended to the generated search query.
#search_prompt = ""

# Restrict the search to these domains (empty = no restriction).
#search_include_domains = []

# Exclude these domains from the search (empty = no restriction).
#search_exclude_domains = []

# Let a decision model tune the search settings for each query (default: true).
#search_decide = true

# Decision model id used to tune the search settings.
#search_decide_model = "typesafe/jev-1.13"

# ---- Extensions ----------------------------------------------------------
# klava ships with no extensions. Install one with:
#
#   klava extensions install cargo:<crate> --trust
#   klava extensions install npm:<package> --trust
#   klava extensions install ./my-extension --trust
#
# Extensions live in ~/.local/share/klava/extensions (or the equivalent data
# directory) and are discovered at runtime; installing one never rebuilds klava.
# Run 'klava extensions path' to see every directory searched.
#
# klava only interprets `enabled`. Every other key under a settings table is
# passed to the extension untouched, so klava never needs to know what an
# extension supports. See EXTENSIONS.md for the protocol and an example.
#
# [extensions.settings.<name>]
# enabled = true
"#;

        fs::write(&path, template)?;
        Ok(())
    }

    pub fn load() -> Result<Config> {
        let path = Config::get_path();
        let mut cfg: Config = confy::load_path(path)?;
        cfg.apply_legacy_aliases();
        Ok(cfg)
    }

    /// Translate deprecated top-level flags into extension behaviour.
    ///
    /// `enable_pii` predates the extension system. klava must not name a
    /// specific extension, so instead of writing into another crate's config
    /// table this leaves a global switch that the extension chain consults.
    /// A PII extension reads it as one input; extensions that have nothing to
    /// do with PII ignore it.
    fn apply_legacy_aliases(&mut self) {
        if !self.enable_pii {
            self.extensions.disable_pii = true;
        }
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
        // If active provider is empty, tell user to run setup
        if self.active_provider.is_empty() {
            return Err(Error::ActiveProviderNotSetup);
        }

        // Find the active provider config
        let active_provider_config = self.get_active_provider_config().ok_or_else(|| {
            Error::MissingBaseUrl // Treat missing provider config as needing setup
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

    /// Upstream chat completions URL for the active provider.
    pub fn chat_completions_url(&self) -> String {
        self.get_active_provider_config()
            .and_then(|p| p.chat_completions_url())
            .expect("Base URL not configured for active provider")
    }

    /// Model used for web-search requests: the explicit `search_model` when
    /// set, otherwise the completion model of the active provider.
    pub fn resolve_search_model(&self) -> Option<String> {
        self.search_model
            .clone()
            .filter(|m| !m.is_empty())
            .or_else(|| self.resolve_completion_model())
    }

    /// Whether web search should be active for this setup: enabled in config,
    /// the active provider is the klava gateway, and klava is authenticated
    /// (saved `klava login` session or an explicit `KLAVA_API_KEY`-style key).
    /// The search backend is only reachable through the klava gateway, so the
    /// feature stays off for every other provider.
    pub fn websearch_available(&self) -> bool {
        if !self.enable_websearch {
            return false;
        }
        self.get_active_provider_config()
            .map(|p| p.is_klava() && p.has_klava_auth())
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::Type as ProviderType;

    #[test]
    fn test_config_default_creates_empty_providers() {
        let config = Config::default();
        assert_eq!(config.active_provider, "");
        assert_eq!(config.providers.len(), 0);
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

                    chat_completions_path: None,
                    api_key: None,
                    api_key_name: None,
                    reasoning_model: Some("alisyao".to_string()),
                    completion_model: None,
                },
                ProviderConfig {
                    name: "other_provider".to_string(),
                    provider_type: ProviderType::OpenAICompatible,
                    base_url: Some("https://other.example.com".to_string()),

                    chat_completions_path: None,
                    api_key: None,
                    api_key_name: None,
                    reasoning_model: None,
                    completion_model: None,
                },
            ],
            verbose: false,
            enable_pii: true,
            enable_token_stats: false,
            ..Config::default()
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

                    chat_completions_path: None,
                    api_key: None,
                    api_key_name: None,
                    reasoning_model: None,
                    completion_model: None,
                },
                ProviderConfig {
                    name: "second_provider".to_string(),
                    provider_type: ProviderType::OpenAICompatible,
                    base_url: Some("https://second.example.com".to_string()),

                    chat_completions_path: None,
                    api_key: None,
                    api_key_name: None,
                    reasoning_model: None,
                    completion_model: None,
                },
            ],
            verbose: false,
            enable_pii: true,
            enable_token_stats: false,
            ..Config::default()
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
                chat_completions_path: None,
                api_key: Some("test-key".to_string()),
                api_key_name: None,
                reasoning_model: Some("test-reasoning-model".to_string()),
                completion_model: Some("test-completion-model".to_string()),
            }],
            verbose: false,
            enable_pii: true,
            enable_token_stats: false,
            ..Config::default()
        };

        let active_config = config.get_active_provider_config().unwrap();
        assert_eq!(active_config.name, "test");
        assert_eq!(config.port, 3000);
    }

    #[test]
    fn chat_completions_url_defaults_to_v1_chat_completions() {
        let config = Config {
            port: 3000,
            active_provider: "test".to_string(),
            providers: vec![ProviderConfig {
                name: "test".to_string(),
                provider_type: ProviderType::OpenAICompatible,
                base_url: Some("https://chat.example.com".to_string()),
                chat_completions_path: None,
                api_key: None,
                api_key_name: None,
                reasoning_model: None,
                completion_model: None,
            }],
            verbose: false,
            enable_pii: true,
            enable_token_stats: false,
            ..Config::default()
        };

        assert_eq!(
            config.chat_completions_url(),
            "https://chat.example.com/v1/chat/completions"
        );
    }

    #[test]
    fn chat_completions_url_uses_custom_path() {
        let config = Config {
            port: 3000,
            active_provider: "immerse".to_string(),
            providers: vec![ProviderConfig {
                name: "immerse".to_string(),
                provider_type: ProviderType::OpenAICompatible,
                base_url: Some("https://chat.immers.cloud/".to_string()),
                chat_completions_path: Some("/v1/endpoints/generate/chat/completions".to_string()),
                api_key: None,
                api_key_name: Some("IMMERSE_TOKEN".to_string()),
                reasoning_model: Some("deepseek-v4-flash-0731".to_string()),
                completion_model: Some("deepseek-v4-flash-0731".to_string()),
            }],
            verbose: false,
            enable_pii: true,
            enable_token_stats: false,
            ..Config::default()
        };

        // Trailing slash on base_url and custom path are handled.
        assert_eq!(
            config.chat_completions_url(),
            "https://chat.immers.cloud/v1/endpoints/generate/chat/completions"
        );
    }

    #[test]
    fn chat_completions_url_accepts_path_without_leading_slash() {
        let config = Config {
            port: 3000,
            active_provider: "custom".to_string(),
            providers: vec![ProviderConfig {
                name: "custom".to_string(),
                provider_type: ProviderType::OpenAICompatible,
                base_url: Some("http://localhost:11434".to_string()),
                chat_completions_path: Some("v1/chat/completions".to_string()),
                api_key: None,
                api_key_name: None,
                reasoning_model: None,
                completion_model: None,
            }],
            verbose: false,
            enable_pii: true,
            enable_token_stats: false,
            ..Config::default()
        };

        assert_eq!(
            config.chat_completions_url(),
            "http://localhost:11434/v1/chat/completions"
        );
    }

    #[cfg(feature = "klava")]
    fn klava_provider(name: &str) -> ProviderConfig {
        ProviderConfig {
            name: name.to_string(),
            provider_type: ProviderType::Klava,
            base_url: None,
            chat_completions_path: None,
            api_key: None,
            api_key_name: None,
            reasoning_model: None,
            completion_model: None,
        }
    }

    #[cfg(feature = "klava")]
    fn openai_provider(name: &str) -> ProviderConfig {
        ProviderConfig {
            name: name.to_string(),
            provider_type: ProviderType::OpenAICompatible,
            base_url: Some("https://test.example.com".to_string()),
            chat_completions_path: None,
            api_key: None,
            api_key_name: None,
            reasoning_model: None,
            completion_model: None,
        }
    }

    #[cfg(feature = "klava")]
    #[test]
    fn websearch_available_requires_klava_provider() {
        let _guard = AUTH_FILE_TEST_LOCK.lock().unwrap();
        let config = Config {
            port: 3000,
            active_provider: "openai".to_string(),
            providers: vec![openai_provider("openai")],
            enable_websearch: true,
            ..Config::default()
        };
        // Non-klava active provider → no web search, even with auth env set.
        unsafe {
            std::env::set_var("KLAVA_API_KEY", "sk-test");
        }
        assert!(!config.websearch_available());
        unsafe {
            std::env::remove_var("KLAVA_API_KEY");
        }
    }

    #[cfg(feature = "klava")]
    #[test]
    fn websearch_available_requires_enable_flag() {
        let config = Config {
            port: 3000,
            active_provider: "klava".to_string(),
            providers: vec![ProviderConfig {
                api_key: Some("sk-test".to_string()),
                ..klava_provider("klava")
            }],
            enable_websearch: false,
            ..Config::default()
        };
        assert!(!config.websearch_available());
    }

    #[cfg(feature = "klava")]
    #[test]
    fn websearch_available_true_with_explicit_key() {
        let config = Config {
            port: 3000,
            active_provider: "klava".to_string(),
            providers: vec![ProviderConfig {
                api_key: Some("sk-test".to_string()),
                ..klava_provider("klava")
            }],
            enable_websearch: true,
            ..Config::default()
        };
        assert!(config.websearch_available());
    }

    #[cfg(feature = "klava")]
    #[test]
    fn websearch_available_true_with_env_key() {
        let _guard = AUTH_FILE_TEST_LOCK.lock().unwrap();
        let config = Config {
            port: 3000,
            active_provider: "klava".to_string(),
            providers: vec![ProviderConfig {
                api_key_name: Some("KLAVA_API_KEY".to_string()),
                ..klava_provider("klava")
            }],
            enable_websearch: true,
            ..Config::default()
        };
        unsafe {
            std::env::set_var("KLAVA_API_KEY", "sk-env-key");
        }
        assert!(config.websearch_available());
        unsafe {
            std::env::remove_var("KLAVA_API_KEY");
        }
    }

    /// Serializes tests that point `KLAVA_AUTH_FILE` at temp files: the env
    /// var is process-global and cargo runs tests in parallel.
    #[cfg(feature = "klava")]
    static AUTH_FILE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[cfg(feature = "klava")]
    #[test]
    fn websearch_available_false_without_any_auth() {
        let _guard = AUTH_FILE_TEST_LOCK.lock().unwrap();
        // Point the saved-login check at a nonexistent file so the test is
        // hermetic (the dev machine may have a real ~/.local/share login).
        let missing = std::env::temp_dir().join(format!("klava-noauth-{}", std::process::id()));
        unsafe {
            std::env::set_var("KLAVA_AUTH_FILE", &missing);
        }
        let config = Config {
            port: 3000,
            active_provider: "klava".to_string(),
            providers: vec![klava_provider("klava")],
            enable_websearch: true,
            ..Config::default()
        };
        // No api_key, no api_key_name, no saved login at the pointed path.
        assert!(!config.websearch_available());
        unsafe {
            std::env::remove_var("KLAVA_AUTH_FILE");
        }
        let _ = std::fs::remove_file(&missing);
    }

    #[cfg(feature = "klava")]
    #[test]
    fn websearch_available_true_with_saved_login() {
        let _guard = AUTH_FILE_TEST_LOCK.lock().unwrap();
        // Simulate a `klava login`: a valid credentials file at the pointed path.
        use std::io::Write;
        let path = std::env::temp_dir().join(format!("klava-auth-{}", std::process::id()));
        let creds = serde_json::json!({
            "api_key": "sk_saved_login",
            "key_preview": "sk_saved…",
            "email": "t@example.com",
            "base_url": "https://klava.zatsepin.dev",
            "created_at": "2026-01-01T00:00:00Z"
        });
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(creds.to_string().as_bytes()).unwrap();
        unsafe {
            std::env::set_var("KLAVA_AUTH_FILE", &path);
        }
        let config = Config {
            port: 3000,
            active_provider: "klava".to_string(),
            providers: vec![klava_provider("klava")],
            enable_websearch: true,
            ..Config::default()
        };
        assert!(config.websearch_available());
        unsafe {
            std::env::remove_var("KLAVA_AUTH_FILE");
        }
        let _ = std::fs::remove_file(&path);
    }

    #[cfg(feature = "klava")]
    #[test]
    fn websearch_available_false_when_no_active_provider() {
        let config = Config {
            port: 3000,
            active_provider: "missing".to_string(),
            providers: vec![klava_provider("klava")],
            enable_websearch: true,
            ..Config::default()
        };
        assert!(!config.websearch_available());
    }

    #[cfg(feature = "klava")]
    #[test]
    fn klava_provider_resolves_default_base_url_without_config() {
        let config = Config {
            port: 3000,
            active_provider: "klava".to_string(),
            providers: vec![klava_provider("klava")],
            enable_websearch: true,
            ..Config::default()
        };
        assert_eq!(
            config.chat_completions_url(),
            "https://klava.zatsepin.dev/api/v1/chat/completions"
        );
    }

    #[cfg(feature = "klava")]
    #[test]
    fn klava_provider_resolves_custom_base_url_when_configured() {
        let config = Config {
            port: 3000,
            active_provider: "klava".to_string(),
            providers: vec![ProviderConfig {
                base_url: Some("https://dev.example.com/api".to_string()),
                ..klava_provider("klava")
            }],
            enable_websearch: true,
            ..Config::default()
        };
        assert_eq!(
            config.chat_completions_url(),
            "https://dev.example.com/api/v1/chat/completions"
        );
    }
}
