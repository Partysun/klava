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

/// One classifier question an extension asks klava to resolve.
///
/// Mirrors the System One wire (`noul`, `choice`, `score`) without exposing
/// the backend: the extension states what to judge, klava decides how to ask.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecideQuestion {
    /// Answer key chosen by the extension.
    pub id: String,
    /// `noul` (yes/no), `choice` (one of `criteria` keys), or `score`.
    #[serde(rename = "type")]
    pub kind: String,
    /// How to judge the state.
    pub instructions: String,
    /// `choice`/`score` options, when the kind needs them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub criteria: Option<Value>,
}

/// A request to run the host's decide model over extension-supplied state.
///
/// The extension owns the question and the evidence; klava owns the model,
/// credentials, timeout, and cost accounting. This is what lets a third-party
/// extension ask for a judgment without linking klava or holding a key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecideRequest {
    /// The state to judge. Any JSON value; backends judge it as text.
    pub state: Value,
    /// One or more bounded questions about `state`.
    pub questions: Vec<DecideQuestion>,
    /// `noul` answers at or above this probability count as approval.
    #[serde(default = "default_decide_threshold")]
    pub threshold: f32,
    /// Question whose answer decides `approve`. Defaults to the first `noul`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approve_question: Option<String>,
}

fn default_decide_threshold() -> f32 {
    0.5
}

/// The host's answer, handed back to the extension on its next run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecideResult {
    /// Answer key -> the typed answer, in the System One wire shape.
    pub answers: std::collections::HashMap<String, Value>,
    /// Whether the approve question cleared the threshold.
    pub approve: bool,
    /// Probability the approve question reported, when it was `noul`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probability: Option<f32>,
    /// Billed cost of the decision call in USD, when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    /// Model that answered.
    pub model: String,
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
    /// Ask the host to classify extension-supplied state; the extension is
    /// re-run with the answer.
    Decide(DecideRequest),
    /// Ask the proxy to call another model and fold its answer back in.
    NestedCall(NestedCallRequest),
}
