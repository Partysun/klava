//! Provider enum for managing API providers

use crate::config::Config;
use crate::error::{Error, Result};
use crate::providers::{Config as ProvidersConfig, Type as ProviderType};
use reqwest::header::{HeaderMap, HeaderValue};

impl ProvidersConfig {
    /// Description
    pub fn description(&self) -> &str {
        match self.provider_type {
            ProviderType::OpenAICompatible => {
                "OpenAI-compatible APIs (OpenRouter, OpenAI, local LLMs)"
            }
            #[cfg(feature = "qwen-free")]
            ProviderType::QwenCode => "Qwen Code (Free*) with OAuth authentication",
        }
    }

    /// Needs API key?
    pub fn needs_api_key(&self) -> bool {
        matches!(self.provider_type, ProviderType::OpenAICompatible)
    }

    /// Needs base URL?
    pub fn needs_base_url(&self) -> bool {
        matches!(self.provider_type, ProviderType::OpenAICompatible)
    }

    /// Base URL
    pub fn base_url(&self) -> Option<String> {
        match self.provider_type {
            #[cfg(feature = "qwen-free")]
            ProviderType::QwenCode => Some("https://portal.qwen.ai".to_string()),
            _ => None,
        }
    }

    /// Reasoning model
    pub fn reasoning_model(&self) -> Option<String> {
        match self.provider_type {
            #[cfg(feature = "qwen-free")]
            ProviderType::QwenCode => Some("qwen3-coder-plus".to_string()),
            _ => self.reasoning_model.clone(),
        }
    }

    /// Completion model
    pub fn completion_model(&self) -> Option<String> {
        match self.provider_type {
            #[cfg(feature = "qwen-free")]
            ProviderType::QwenCode => Some("qwen3-coder-plus".to_string()),
            _ => self.completion_model.clone(),
        }
    }

    /// Auth headers
    pub async fn get_auth_headers(&self, config: &Config) -> Result<Option<HeaderMap>> {
        match self.provider_type {
            ProviderType::OpenAICompatible => {
                let api_key = config
                    .resolve_api_key()
                    .ok_or_else(|| Error::MissingApiKey("default".to_string()))?;
                let mut headers = HeaderMap::new();
                headers.insert(
                    reqwest::header::AUTHORIZATION,
                    HeaderValue::from_str(&format!("Bearer {}", api_key))
                        .map_err(|_| Error::MissingApiKey("default".to_string()))?,
                );
                Ok(Some(headers))
            }
            #[cfg(feature = "qwen-free")]
            ProviderType::QwenCode => {
                let mut qwen_auth = crate::qwen_auth::QwenAuth::new();
                qwen_auth
                    .get_auth_headers()
                    .await
                    .map(Some)
                    .map_err(|e| Error::Provider(format!("Qwen auth failed: {}", e)))
            }
        }
    }

    /// Validate config
    pub fn validate_config(&self, config: &Config) -> Result<()> {
        let active_provider_config = config.get_active_provider_config().ok_or_else(|| {
            Error::Internal(format!(
                "Active provider config '{}' not found",
                config.active_provider
            ))
        })?;

        match self.provider_type {
            ProviderType::OpenAICompatible => {
                let base_url = config
                    .resolve_base_url()
                    .ok_or_else(|| Error::MissingBaseUrl)?;

                if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
                    return Err(Error::InvalidBaseUrl(base_url));
                }

                if active_provider_config.api_key.is_none() && config.resolve_api_key().is_none() {
                    return Err(Error::MissingApiKey(self.name.to_string()));
                }
            }
            #[cfg(feature = "qwen-free")]
            ProviderType::QwenCode => {
                // Qwen uses OAuth, no config validation needed
            }
        }
        Ok(())
    }

    /// Resolve base URL for this provider
    pub fn resolve_base_url(&self) -> Option<String> {
        match self.provider_type {
            ProviderType::OpenAICompatible => self.base_url.clone(),
            #[cfg(feature = "qwen-free")]
            ProviderType::QwenCode => Some("https://portal.qwen.ai".to_string()),
        }
    }
}
