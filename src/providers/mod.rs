//! Model entries: one connection configuration per routable model.
//!
//! Historically klava had `[[providers]]` (a connection: URL + key + type)
//! and an `active_provider` switch, with the model chosen by the provider's
//! `reasoning_model`/`completion_model` fields. The unit of routing is now
//! the **model**: every `[[models]]` entry is self-contained (name, type,
//! base URL, key) and a request is routed by the model name the harness
//! sends. Entries that share a name form a pool.

mod provider;
#[cfg(feature = "qwen-code")]
mod qwen;
mod setup;

use serde::{Deserialize, Serialize};

/// Provider type enum for configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub enum Type {
    #[serde(rename = "openai-compatible")]
    #[serde(alias = "openai")]
    #[default]
    OpenAICompatible,
    #[cfg(feature = "qwen-code")]
    #[serde(rename = "qwen-code")]
    QwenCode,
    #[cfg(feature = "klava")]
    #[serde(rename = "klava")]
    Klava,
}

impl Type {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "openai-compatible" | "openai" | "default" => Some(Self::OpenAICompatible),
            #[cfg(feature = "qwen-code")]
            "qwen-code" | "qwen" => Some(Self::QwenCode),
            #[cfg(feature = "klava")]
            "klava" => Some(Self::Klava),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::OpenAICompatible => "openai-compatible",
            #[cfg(feature = "qwen-code")]
            Self::QwenCode => "qwen-code",
            #[cfg(feature = "klava")]
            Self::Klava => "klava",
        }
    }
}

/// Which System One (jigor) gateway answers `decide` calls.
///
/// klava detects this from the model entry — `type = "klava"`, an OpenRouter
/// base URL, or a Cloudflare Workers AI endpoint — so the config normally
/// carries no backend hint at all. The `jigor` field exists only for odd
/// self-hosted gateways that cannot be recognised from their URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JigorBackend {
    Klava,
    Openrouter,
    Cloudflare,
}

impl JigorBackend {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "klava" => Some(Self::Klava),
            "openrouter" | "open-router" => Some(Self::Openrouter),
            "cloudflare" | "cf" => Some(Self::Cloudflare),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Klava => "klava",
            Self::Openrouter => "openrouter",
            Self::Cloudflare => "cloudflare",
        }
    }
}

/// One `[[models]]` entry: everything needed to send a chat-completions
/// request for one model id.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Model {
    /// Public model name. This is what harnesses send and see. Several
    /// entries may share a name — they form a pool.
    pub name: String,
    #[serde(rename = "type")]
    pub provider_type: Type,
    pub base_url: Option<String>,
    /// Optional absolute path for the chat completions endpoint, e.g.
    /// `/v1/endpoints/generate/chat/completions` for providers that route
    /// through a nested path. Defaults to `/v1/chat/completions` when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_completions_path: Option<String>,
    pub api_key: Option<String>,
    pub api_key_name: Option<String>,
    /// Real upstream model id. When omitted or empty, `name` is sent upstream.
    /// The legacy `upstream_model` key is accepted when loading but never
    /// written.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        alias = "upstream_model"
    )]
    pub model: Option<String>,
    /// Explicit jigor backend, overriding URL-based detection. Normally unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jigor: Option<String>,

    // ---- Legacy fields (pre-`[[models]]` `[[providers]]` only) ------------
    // Read for migration and cleared when the config is rewritten; never
    // written by klava.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(alias = "reasoning")]
    pub reasoning_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(alias = "completion")]
    pub completion_model: Option<String>,
}

impl Model {
    /// A minimal OpenAI-compatible entry (used by tests and the wizard).
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            ..Self::default()
        }
    }

    /// The model id sent upstream: `model` when non-empty, otherwise `name`.
    pub fn upstream_model(&self) -> String {
        self.model
            .clone()
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| self.name.clone())
    }

    /// The jigor backend that serves this model, or `None` when this model
    /// cannot answer `decide` calls (e.g. a Qwen OAuth endpoint).
    ///
    /// An explicit `jigor` field wins; otherwise the backend is recognised
    /// from the type and base URL, falling back to klava.
    pub fn jigor_backend(&self) -> Option<JigorBackend> {
        if let Some(explicit) = self.jigor.as_deref().and_then(JigorBackend::parse) {
            return Some(explicit);
        }
        match self.provider_type {
            #[cfg(feature = "klava")]
            Type::Klava => Some(JigorBackend::Klava),
            #[cfg(feature = "qwen-code")]
            Type::QwenCode => None,
            Type::OpenAICompatible => {
                let url = self.resolve_base_url().unwrap_or_default();
                let host = url
                    .split("://")
                    .nth(1)
                    .unwrap_or(&url)
                    .split('/')
                    .next()
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if host.ends_with("openrouter.ai") {
                    Some(JigorBackend::Openrouter)
                } else if host == "api.cloudflare.com" {
                    Some(JigorBackend::Cloudflare)
                } else {
                    // Unknown OpenAI-compatible endpoints (custom gateways
                    // serving the klava wire) default to the klava backend.
                    Some(JigorBackend::Klava)
                }
            }
        }
    }

    /// Cloudflare account id, parsed from the Workers AI endpoint:
    /// `…/accounts/<id>/ai/…`.
    pub fn cloudflare_account_id(&self) -> Option<String> {
        let url = self.resolve_base_url()?;
        let mut parts = url.split('/');
        while let Some(part) = parts.next() {
            if part == "accounts" {
                return parts.next().filter(|id| !id.is_empty()).map(str::to_string);
            }
        }
        None
    }
}

/// Backwards-compatible alias: `providers::Config` is what a model entry used
/// to be called. New code should use [`Model`].
pub type Config = Model;

#[cfg(feature = "qwen-code")]
pub use crate::qwen_auth::QwenAuth;
#[cfg(feature = "qwen-code")]
pub use qwen::setup as setup_qwen;
pub use setup::{
    ProviderKind, add_provider_interactive, check_entry, decide_candidates, providers,
    remove_provider, set_defaults,
};

#[cfg(feature = "klava")]
pub use crate::klava_auth::KlavaAuth;
