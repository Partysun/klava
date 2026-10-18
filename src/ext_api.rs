//! The extension protocol vocabulary.
//!
//! These are the types klava puts on the wire. The wire format is plain JSON,
//! documented in `EXTENSIONS.md`; an extension in any language implements it
//! without depending on klava or on any Rust crate.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Why the client is making this request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RequestReason {
    /// First request after a user message.
    #[default]
    User,
    /// Any other request in the agent loop (after tool results).
    Continuation,
    /// Automatic retry after a failure.
    Retry,
    /// Request outside the agent loop (compaction, nested call).
    Direct,
}

/// Which client API the request arrived on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceApi {
    OpenAi,
    Anthropic,
    Responses,
}

/// How a nested model answer is folded back into the conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FoldMode {
    /// Append the answer as a message; the original model continues.
    #[default]
    Append,
    /// Return the answer as the assistant turn.
    TakeOver,
}

/// Parameters for one nested model call, as the host performs it.
#[derive(Debug, Clone, PartialEq)]
pub struct NestedCallRequest {
    /// Provider name; `None` selects the active provider.
    pub provider: Option<String>,
    pub model: String,
    pub mode: FoldMode,
    /// System prompt for the nested call.
    pub system: String,
    /// The task handed to the nested model.
    pub instruction: String,
    pub max_tokens: Option<u32>,
}

/// What an extension wants the proxy to do with this request.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum Decision {
    /// Leave the request untouched.
    #[default]
    Continue,
    /// Replace the payload and continue with the next extension.
    Replace(Value),
    /// Append messages to the conversation before sending.
    Append(Vec<Value>),
    /// Reject the request.
    Deny { reason: String },
    /// Ask the proxy to call another model and fold its answer back in.
    NestedCall(NestedCallRequest),
}
