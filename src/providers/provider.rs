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
            #[cfg(feature = "qwen-code")]
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
            #[cfg(feature = "qwen-code")]
            ProviderType::QwenCode => Some("https://portal.qwen.ai".to_string()),
            _ => None,
        }
    }

    /// Reasoning model
    pub fn reasoning_model(&self) -> Option<String> {
        match self.provider_type {
            #[cfg(feature = "qwen-code")]
            ProviderType::QwenCode => Some("qwen3-coder-plus".to_string()),
            _ => self.reasoning_model.clone(),
        }
    }

    /// Completion model
    pub fn completion_model(&self) -> Option<String> {
        match self.provider_type {
            #[cfg(feature = "qwen-code")]
            ProviderType::QwenCode => Some("qwen3-coder-plus".to_string()),
            _ => self.completion_model.clone(),
        }
    }

    /// Auth headers for a specific provider config
    pub async fn get_auth_headers_for(&self, _config: &Config) -> Result<Option<HeaderMap>> {
        match self.provider_type {
            ProviderType::OpenAICompatible => {
                let api_key = self
                    .api_key
                    .clone()
                    .filter(|k| !k.is_empty())
                    .or_else(|| {
                        self.api_key_name
                            .as_ref()
                            .and_then(|name| std::env::var(name).ok())
                    })
                    .ok_or_else(|| Error::MissingApiKey(self.name.to_string()))?;
                let mut headers = HeaderMap::new();
                headers.insert(
                    reqwest::header::AUTHORIZATION,
                    HeaderValue::from_str(&format!("Bearer {}", api_key))
                        .map_err(|_| Error::MissingApiKey(self.name.to_string()))?,
                );
                Ok(Some(headers))
            }
            #[cfg(feature = "qwen-code")]
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

    /// Auth headers ( delegates to get_auth_headers_for for backward compat )
    pub async fn get_auth_headers(&self, config: &Config) -> Result<Option<HeaderMap>> {
        self.get_auth_headers_for(config).await
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
            #[cfg(feature = "qwen-code")]
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
            #[cfg(feature = "qwen-code")]
            ProviderType::QwenCode => Some("https://portal.qwen.ai".to_string()),
        }
    }

    /// Upstream chat completions URL for THIS provider.
    ///
    /// Defaults to `{base_url}/v1/chat/completions`; providers that expose
    /// the endpoint at a nested path (e.g. Immerse's
    /// `/v1/endpoints/generate/chat/completions`) can override the path via
    /// the `chat_completions_path` config field. Returns `None` when the
    /// provider has no base URL.
    pub fn chat_completions_url(&self) -> Option<String> {
        let base_url = self.resolve_base_url()?;
        let path = self
            .chat_completions_path
            .clone()
            .unwrap_or_else(|| "/v1/chat/completions".to_string());
        let path = if path.starts_with('/') {
            path
        } else {
            format!("/{}", path)
        };
        Some(format!("{}{}", base_url.trim_end_matches('/'), path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::Type as ProviderType;

    fn provider(base_url: &str, path: Option<&str>) -> ProvidersConfig {
        ProvidersConfig {
            name: "test".to_string(),
            provider_type: ProviderType::OpenAICompatible,
            base_url: Some(base_url.to_string()),
            chat_completions_path: path.map(String::from),
            api_key: None,
            api_key_name: None,
            reasoning_model: None,
            completion_model: None,
        }
    }

    #[test]
    fn url_defaults_to_v1_chat_completions() {
        assert_eq!(
            provider("https://openrouter.ai/api", None).chat_completions_url(),
            Some("https://openrouter.ai/api/v1/chat/completions".to_string())
        );
    }

    #[test]
    fn url_uses_custom_path() {
        assert_eq!(
            provider(
                "https://chat.immers.cloud",
                Some("/v1/endpoints/generate/chat/completions")
            )
            .chat_completions_url(),
            Some(
                "https://chat.immers.cloud/v1/endpoints/generate/chat/completions".to_string()
            )
        );
    }

    #[test]
    fn url_handles_trailing_slash_and_missing_leading_slash() {
        assert_eq!(
            provider("https://blocks.gonka.gg/", Some("v1/chat/completions"))
                .chat_completions_url(),
            Some("https://blocks.gonka.gg/v1/chat/completions".to_string())
        );
    }

    #[test]
    fn url_is_none_without_base_url() {
        let p = ProvidersConfig {
            base_url: None,
            ..provider("https://x.example", None)
        };
        assert_eq!(p.chat_completions_url(), None);
    }
}
