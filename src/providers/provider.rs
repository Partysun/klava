//! Per-model connection behaviour: URL, auth headers, validation.

use crate::config::Config;
use crate::error::{Error, Result};
use crate::providers::{Model, Type as ProviderType};
use reqwest::header::{HeaderMap, HeaderValue};

impl Model {
    /// Description of the provider family this entry belongs to.
    pub fn description(&self) -> &str {
        match self.provider_type {
            ProviderType::OpenAICompatible => {
                "OpenAI-compatible APIs (OpenRouter, Cloudflare, local LLMs)"
            }
            #[cfg(feature = "qwen-code")]
            ProviderType::QwenCode => "Qwen Code (Free*) with OAuth authentication",
            #[cfg(feature = "klava")]
            ProviderType::Klava => "Klava (klava.zatsepin.dev) with device-flow login",
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

    /// Reasoning model for this entry (legacy `[[providers]]` field).
    pub fn reasoning_model(&self) -> Option<String> {
        match self.provider_type {
            #[cfg(feature = "qwen-code")]
            ProviderType::QwenCode => Some(self.upstream_model()),
            _ => self.reasoning_model.clone().or(Some(self.upstream_model())),
        }
    }

    /// Completion model for this entry (legacy `[[providers]]` field).
    pub fn completion_model(&self) -> Option<String> {
        match self.provider_type {
            #[cfg(feature = "qwen-code")]
            ProviderType::QwenCode => Some(self.upstream_model()),
            _ => self
                .completion_model
                .clone()
                .or(Some(self.upstream_model())),
        }
    }

    /// Auth headers for this model entry.
    pub async fn get_auth_headers_for(&self, _config: &Config) -> Result<Option<HeaderMap>> {
        match self.provider_type {
            ProviderType::OpenAICompatible => {
                // Naming an `api_key_name` is a promise that the variable
                // exists: a missing value is an error. With neither field set
                // the endpoint is treated as unauthenticated (a local server
                // such as Ollama), and no Authorization header is sent.
                let api_key = match self.api_key.clone().filter(|k| !k.is_empty()) {
                    Some(key) => Some(key),
                    None => match &self.api_key_name {
                        Some(name) => Some(
                            std::env::var(name)
                                .ok()
                                .filter(|k| !k.is_empty())
                                .ok_or_else(|| Error::MissingApiKey(self.name.to_string()))?,
                        ),
                        None => None,
                    },
                };
                let Some(api_key) = api_key else {
                    return Ok(None);
                };
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
            #[cfg(feature = "klava")]
            ProviderType::Klava => {
                // An explicit key wins: the config `api_key` field, or the
                // env var named by `api_key_name` (e.g. KLAVA_API_KEY).
                // Otherwise fall back to the credentials saved by `klava login`.
                let explicit = self.api_key.clone().filter(|k| !k.is_empty()).or_else(|| {
                    self.api_key_name
                        .as_ref()
                        .and_then(|name| std::env::var(name).ok())
                });
                if let Some(key) = explicit.filter(|k| !k.is_empty()) {
                    let mut headers = HeaderMap::new();
                    headers.insert(
                        reqwest::header::AUTHORIZATION,
                        HeaderValue::from_str(&format!("Bearer {key}"))
                            .map_err(|_| Error::Provider("Invalid API key".to_string()))?,
                    );
                    return Ok(Some(headers));
                }
                let mut klava_auth = crate::klava_auth::KlavaAuth::new();
                klava_auth
                    .get_auth_headers()
                    .map(Some)
                    .map_err(|e| Error::Provider(format!("Klava auth failed: {}", e)))
            }
        }
    }

    /// Auth headers ( delegates to get_auth_headers_for for backward compat )
    pub async fn get_auth_headers(&self, config: &Config) -> Result<Option<HeaderMap>> {
        self.get_auth_headers_for(config).await
    }

    /// The API key this entry would use, when it can be resolved without
    /// network access (explicit key, env var, or a saved klava login).
    pub fn api_key(&self) -> Option<String> {
        if let Some(key) = self.api_key.clone().filter(|k| !k.is_empty()) {
            return Some(key);
        }
        if let Some(name) = &self.api_key_name
            && let Ok(key) = std::env::var(name)
            && !key.is_empty()
        {
            return Some(key);
        }
        #[cfg(feature = "klava")]
        if self.provider_type == ProviderType::Klava {
            let mut auth = crate::klava_auth::KlavaAuth::new();
            return auth.get_api_key().ok().filter(|k| !k.is_empty());
        }
        None
    }

    /// Whether this entry is the klava gateway provider.
    pub fn is_klava(&self) -> bool {
        #[cfg(feature = "klava")]
        {
            self.provider_type == ProviderType::Klava
        }
        #[cfg(not(feature = "klava"))]
        {
            false
        }
    }

    /// Whether klava is usable right now: an explicit key (config `api_key`,
    /// or the `api_key_name` env var such as `KLAVA_API_KEY`) or a saved
    /// `klava login` session.
    pub fn has_klava_auth(&self) -> bool {
        if self.api_key.as_ref().is_some_and(|k| !k.is_empty()) {
            return true;
        }
        if self
            .api_key_name
            .as_ref()
            .is_some_and(|n| std::env::var(n).is_ok_and(|k| !k.is_empty()))
        {
            return true;
        }
        #[cfg(feature = "klava")]
        {
            crate::klava_auth::KlavaAuth::new().is_authenticated()
        }
        #[cfg(not(feature = "klava"))]
        {
            false
        }
    }

    /// Validate this entry's settings.
    pub fn validate_config(&self, _config: &Config) -> Result<()> {
        match self.provider_type {
            ProviderType::OpenAICompatible => {
                let base_url = self.resolve_base_url().ok_or(Error::MissingBaseUrl)?;

                if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
                    return Err(Error::InvalidBaseUrl(base_url));
                }

                // No key field at all means "this endpoint needs no auth".
                // A named env var that is empty is a mistake worth failing.
                if let Some(name) = &self.api_key_name
                    && self.api_key().is_none()
                {
                    return Err(Error::MissingApiKey(format!(
                        "{} (${} is not set)",
                        self.name, name
                    )));
                }
            }
            #[cfg(feature = "qwen-code")]
            ProviderType::QwenCode => {
                // Qwen uses OAuth, no config validation needed
            }
            #[cfg(feature = "klava")]
            ProviderType::Klava => {
                // Klava uses OAuth: the API key comes from the local auth
                // file (klava login) — nothing to validate in the config.
            }
        }
        Ok(())
    }

    /// Resolve base URL for this entry
    pub fn resolve_base_url(&self) -> Option<String> {
        match self.provider_type {
            ProviderType::OpenAICompatible => self.base_url.clone(),
            #[cfg(feature = "qwen-code")]
            ProviderType::QwenCode => Some("https://portal.qwen.ai".to_string()),
            #[cfg(feature = "klava")]
            ProviderType::Klava => Some(
                self.base_url
                    .clone()
                    .unwrap_or_else(|| "https://klava.zatsepin.dev/api".to_string()),
            ),
        }
    }

    /// Upstream chat completions URL for THIS entry.
    ///
    /// Defaults to `{base_url}/v1/chat/completions`; providers that expose
    /// the endpoint at a nested path (e.g. Immerse's
    /// `/v1/endpoints/generate/chat/completions`) can override the path via
    /// the `chat_completions_path` config field. Returns `None` when the
    /// entry has no base URL.
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

    fn entry(base_url: &str, path: Option<&str>) -> Model {
        Model {
            name: "test".to_string(),
            provider_type: ProviderType::OpenAICompatible,
            base_url: Some(base_url.to_string()),
            chat_completions_path: path.map(String::from),
            api_key: None,
            api_key_name: None,
            ..Model::default()
        }
    }

    #[test]
    fn url_defaults_to_v1_chat_completions() {
        assert_eq!(
            entry("https://openrouter.ai/api", None).chat_completions_url(),
            Some("https://openrouter.ai/api/v1/chat/completions".to_string())
        );
    }

    #[test]
    fn url_uses_custom_path() {
        assert_eq!(
            entry(
                "https://chat.immers.cloud",
                Some("/v1/endpoints/generate/chat/completions")
            )
            .chat_completions_url(),
            Some("https://chat.immers.cloud/v1/endpoints/generate/chat/completions".to_string())
        );
    }

    #[test]
    fn url_handles_trailing_slash_and_missing_leading_slash() {
        assert_eq!(
            entry("https://blocks.gonka.gg/", Some("v1/chat/completions")).chat_completions_url(),
            Some("https://blocks.gonka.gg/v1/chat/completions".to_string())
        );
    }

    #[test]
    fn url_is_none_without_base_url() {
        let p = Model {
            base_url: None,
            ..entry("https://x.example", None)
        };
        assert_eq!(p.chat_completions_url(), None);
    }

    #[test]
    fn upstream_model_defaults_to_the_name() {
        assert_eq!(entry("https://x.example", None).upstream_model(), "test");
        let aliased = Model {
            model: Some("real-id".to_string()),
            ..entry("https://x.example", None)
        };
        assert_eq!(aliased.upstream_model(), "real-id");

        let empty = Model {
            model: Some(String::new()),
            ..entry("https://x.example", None)
        };
        assert_eq!(empty.upstream_model(), "test");
    }

    fn minimal(name: &str) -> Model {
        Model {
            name: name.to_string(),
            provider_type: ProviderType::OpenAICompatible,
            base_url: Some("https://x.example".to_string()),
            ..Model::default()
        }
    }

    #[tokio::test]
    async fn auth_headers_use_configured_api_key() {
        let p = Model {
            api_key: Some("sk-test".to_string()),
            ..minimal("t")
        };
        let headers = p
            .get_auth_headers_for(&Config::default())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(headers.get("authorization").unwrap(), "Bearer sk-test");
    }

    #[tokio::test]
    async fn auth_headers_read_api_key_from_env_name() {
        // Safety: setting a process-wide env var is fine in a single test;
        // the name is unique to avoid races with other tests.
        unsafe {
            std::env::set_var("KLAVA_TEST_AUTH_KEY", "env-key");
        }
        let p = Model {
            api_key_name: Some("KLAVA_TEST_AUTH_KEY".to_string()),
            ..minimal("t")
        };
        let headers = p
            .get_auth_headers_for(&Config::default())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(headers.get("authorization").unwrap(), "Bearer env-key");
        unsafe {
            std::env::remove_var("KLAVA_TEST_AUTH_KEY");
        }
    }

    #[tokio::test]
    async fn auth_headers_missing_env_var_is_error() {
        let p = Model {
            api_key_name: Some("KLAVA_TEST_AUTH_KEY_ABSENT".to_string()),
            ..minimal("t")
        };
        let err = p
            .get_auth_headers_for(&Config::default())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::MissingApiKey(_)));
    }

    #[tokio::test]
    async fn auth_headers_without_any_key_are_omitted() {
        // A local endpoint with no auth: no key field, no header, no error.
        let p = minimal("t");
        assert!(
            p.get_auth_headers_for(&Config::default())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn validate_config_rejects_missing_base_url() {
        let p = Model {
            base_url: None,
            api_key: Some("k".to_string()),
            ..minimal("t")
        };
        let err = p.validate_config(&Config::default()).unwrap_err();
        assert!(matches!(err, Error::MissingBaseUrl));
    }

    #[tokio::test]
    async fn validate_config_rejects_non_http_base_url() {
        let p = Model {
            base_url: Some("ftp://x.example".to_string()),
            api_key: Some("k".to_string()),
            ..minimal("t")
        };
        let err = p.validate_config(&Config::default()).unwrap_err();
        assert!(matches!(err, Error::InvalidBaseUrl(_)));
    }

    #[tokio::test]
    async fn validate_config_rejects_a_named_but_unset_key_env() {
        let p = Model {
            api_key_name: Some("KLAVA_TEST_AUTH_KEY_ABSENT".to_string()),
            ..minimal("t")
        };
        let err = p.validate_config(&Config::default()).unwrap_err();
        assert!(matches!(err, Error::MissingApiKey(_)));
    }

    #[tokio::test]
    async fn validate_config_allows_an_unauthenticated_endpoint() {
        assert!(minimal("t").validate_config(&Config::default()).is_ok());
    }

    #[test]
    fn type_parse_and_as_str_round_trip() {
        assert_eq!(
            ProviderType::parse("openai-compatible"),
            Some(ProviderType::OpenAICompatible)
        );
        assert_eq!(
            ProviderType::parse("openai"),
            Some(ProviderType::OpenAICompatible)
        );
        assert_eq!(
            ProviderType::parse("default"),
            Some(ProviderType::OpenAICompatible)
        );
        assert_eq!(ProviderType::parse("nonsense"), None);
        assert_eq!(ProviderType::OpenAICompatible.as_str(), "openai-compatible");
    }

    #[test]
    fn jigor_backend_is_detected_from_type_and_url() {
        use crate::providers::JigorBackend;

        // OpenRouter endpoint -> OpenRouter backend.
        let or = entry("https://openrouter.ai/api", None);
        assert_eq!(or.jigor_backend(), Some(JigorBackend::Openrouter));

        // Cloudflare account endpoint -> Cloudflare backend + account id.
        let cf = entry(
            "https://api.cloudflare.com/client/v4/accounts/abc123/ai/v1",
            None,
        );
        assert_eq!(cf.jigor_backend(), Some(JigorBackend::Cloudflare));
        assert_eq!(cf.cloudflare_account_id().as_deref(), Some("abc123"));

        // Unknown OpenAI-compatible endpoint -> klava (the default).
        let custom = entry("https://chat.example.com", None);
        assert_eq!(custom.jigor_backend(), Some(JigorBackend::Klava));
    }

    #[cfg(feature = "klava")]
    #[test]
    fn jigor_backend_defaults_to_klava_for_the_klava_type() {
        use crate::providers::JigorBackend;
        let k = Model {
            provider_type: ProviderType::Klava,
            ..Model::default()
        };
        assert_eq!(k.jigor_backend(), Some(JigorBackend::Klava));
    }

    #[test]
    fn explicit_jigor_field_overrides_detection() {
        use crate::providers::JigorBackend;
        let custom = Model {
            jigor: Some("openrouter".to_string()),
            ..entry("https://gateway.internal", None)
        };
        assert_eq!(custom.jigor_backend(), Some(JigorBackend::Openrouter));
    }
}
