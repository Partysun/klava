//! Provider system for API backends
//!
//! Enum-based providers for runtime usage, separate setup modules for each provider.

mod provider;
mod qwen;

use serde::{Deserialize, Serialize};

/// Provider type enum for configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Type {
    #[serde(rename = "openai-compatible")]
    #[serde(alias = "openai")]
    OpenAICompatible,
    #[cfg(feature = "qwen-free")]
    #[serde(rename = "qwen-code")]
    QwenCode,
}

impl Default for Type {
    fn default() -> Self {
        Self::OpenAICompatible
    }
}

impl Type {
    pub fn from_str(name: &str) -> Option<Self> {
        match name {
            "openai-compatible" | "openai" | "default" => Some(Self::OpenAICompatible),
            #[cfg(feature = "qwen-free")]
            "qwen-code" | "qwen" | "qwen-free" => Some(Self::QwenCode),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::OpenAICompatible => "openai-compatible",
            #[cfg(feature = "qwen-free")]
            Self::QwenCode => "qwen-code",
        }
    }
}

/// Provider configuration struct
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    pub name: String,
    #[serde(rename = "type")]
    pub provider_type: Type,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub api_key_name: Option<String>,
    #[serde(alias = "reasoning")]
    pub reasoning_model: Option<String>,
    #[serde(alias = "completion")]
    pub completion_model: Option<String>,
}

pub use crate::qwen_auth::QwenAuth;
pub use qwen::setup as setup_qwen;
