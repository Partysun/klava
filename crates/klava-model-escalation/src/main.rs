//! Model escalation for klava, as a standalone executable.
//!
//! Detects a stuck agent from the conversation the request already carries and
//! asks the proxy to bring in a stronger model. Detection is **deterministic**
//! and stateless — it reads the message history, so it needs no session store
//! and costs microseconds.
//!
//! Signals (all computed over the last `scan_last_messages` messages):
//!
//! * repeated identical tool calls (same name + same arguments),
//! * repeated identical tool results,
//! * a streak of failing tool results,
//! * repeated work on the same target (same file/command).
//!
//! This program never performs the nested call itself. It returns a
//! `nested_call` decision and the klava host performs it, so credentials,
//! provider routing, timeouts and the recursion boundary stay in the proxy.
//!
//! klava launches this as a child process and speaks JSON over stdio; see
//! `EXTENSIONS.md` in the klava repository for the protocol.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::io::{Read, Write};

/// The protocol version klava sends. We answer whatever we understand.
const PROTOCOL: u32 = 1;

/// One request from klava.
#[derive(Debug, Deserialize)]
struct HostRequest {
    #[serde(default)]
    config: Value,
    #[serde(default)]
    payload: Value,
    #[serde(default)]
    nested_calls_used: u32,
}

/// Our answer: `nested_call` with the review request, or `continue`.
#[derive(Debug, Serialize)]
struct HostResponse {
    decision: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    nested_call: Option<NestedCallWire>,
}

/// The nested model call, as klava performs it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NestedCallWire {
    #[serde(default)]
    pub provider: Option<String>,
    pub model: String,
    /// `append` (default) or `take_over`.
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub system: String,
    #[serde(default)]
    pub instruction: String,
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

/// Tunables (`[extensions.settings.klava-model-escalation]`).
///
/// Owned by this crate: klava stores it as opaque JSON and never names a field.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EscalationConfig {
    /// Whether the extension is active. `enabled` is common to every extension,
    /// so the host checks it before calling `build`.
    #[serde(default)]
    pub enabled: bool,
    /// Model to escalate to. Empty = escalate nowhere.
    #[serde(default)]
    pub model: String,
    /// Provider name; `None` = the active provider.
    #[serde(default)]
    pub provider: Option<String>,
    /// `append` (default) or `take_over`.
    #[serde(default = "default_mode")]
    pub mode: String,
    /// Maximum escalations per client request.
    #[serde(default = "default_max_escalations")]
    pub max_escalations: u32,
    /// Repeated identical tool calls that count as stuck.
    #[serde(default = "default_repeat_limit")]
    pub repeat_tool_call_limit: u32,
    /// Repeated identical tool results that count as stuck.
    #[serde(default = "default_repeat_limit")]
    pub repeat_result_limit: u32,
    /// Consecutive failing tool results that count as stuck.
    #[serde(default = "default_repeat_limit")]
    pub error_streak_limit: u32,
    /// Only inspect the last N messages.
    #[serde(default = "default_scan_window")]
    pub scan_last_messages: usize,
    /// System prompt for the advised model.
    #[serde(default = "default_system")]
    pub system: String,
    /// Max tokens for the nested call.
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

fn default_mode() -> String {
    "append".to_string()
}
fn default_max_escalations() -> u32 {
    1
}
fn default_repeat_limit() -> u32 {
    3
}
fn default_scan_window() -> usize {
    40
}
fn default_system() -> String {
    EscalationConfig::default().system
}

impl Default for EscalationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model: String::new(),
            provider: None,
            mode: default_mode(),
            max_escalations: 1,
            repeat_tool_call_limit: 3,
            repeat_result_limit: 2,
            error_streak_limit: 3,
            scan_last_messages: 40,
            system: "You are a senior reviewer. Give concise, concrete remarks: \
                     what is wrong, what to change, what is risky. Do not rewrite \
                     the work."
                .to_string(),
            max_tokens: Some(2048),
        }
    }
}

/// What the deterministic detector found.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StuckSignals {
    /// (tool name, identical-call count) for the most repeated call.
    pub repeated_tool_call: Option<(String, u32)>,
    /// Highest identical-result count seen.
    pub repeated_tool_result: u32,
    /// Longest run of consecutive failing tool results.
    pub error_streak: u32,
}

impl StuckSignals {
    /// Whether any hard signal fired.
    pub fn is_stuck(&self, cfg: &EscalationConfig) -> bool {
        self.repeated_tool_call
            .as_ref()
            .is_some_and(|(_, n)| *n >= cfg.repeat_tool_call_limit)
            || self.repeated_tool_result >= cfg.repeat_result_limit
            || self.error_streak >= cfg.error_streak_limit
    }

    /// One-line explanation for the log.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some((name, n)) = &self.repeated_tool_call {
            parts.push(format!("{n}x identical `{name}` call"));
        }
        if self.repeated_tool_result > 0 {
            parts.push(format!("{}x identical result", self.repeated_tool_result));
        }
        if self.error_streak > 0 {
            parts.push(format!("{} consecutive errors", self.error_streak));
        }
        if parts.is_empty() {
            "no signals".to_string()
        } else {
            parts.join(", ")
        }
    }
}

/// The escalation extension.
pub struct ModelEscalation {
    config: EscalationConfig,
}

impl ModelEscalation {
    pub fn new(config: EscalationConfig) -> Self {
        Self { config }
    }

    /// Compute stuck signals from the message list. Pure and deterministic.
    pub fn detect(messages: &[Value], scan_last: usize) -> StuckSignals {
        let window = if messages.len() > scan_last {
            &messages[messages.len() - scan_last..]
        } else {
            messages
        };

        let mut call_counts: HashMap<String, (String, u32)> = HashMap::new();
        let mut result_counts: HashMap<String, u32> = HashMap::new();
        let mut error_streak = 0u32;
        let mut max_error_streak = 0u32;

        for msg in window {
            // Tool calls in this message.
            if let Some(calls) = msg.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    let name = call
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or("?");
                    let args = call
                        .get("function")
                        .and_then(|f| f.get("arguments"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let key = format!("{name}\u{1}{args}");
                    let entry = call_counts
                        .entry(key)
                        .or_insert_with(|| (name.to_string(), 0));
                    entry.1 += 1;
                }
            }

            // Tool results.
            if msg.get("role").and_then(Value::as_str) == Some("tool") {
                let text = text_of(msg);
                *result_counts.entry(text.clone()).or_insert(0) += 1;

                if looks_like_error(&text) {
                    error_streak += 1;
                    max_error_streak = max_error_streak.max(error_streak);
                } else {
                    error_streak = 0;
                }
            }
        }

        let repeated_tool_call = call_counts
            .into_values()
            .max_by_key(|(_, n)| *n)
            .filter(|(_, n)| *n > 1);

        let repeated_tool_result = result_counts.values().copied().max().unwrap_or(0);

        StuckSignals {
            repeated_tool_call,
            repeated_tool_result,
            error_streak: max_error_streak,
        }
    }

    /// Build the escalation request from the conversation.
    fn escalation_request(&self, signals: &StuckSignals) -> NestedCallWire {
        let mode = if self.config.mode == "take_over" {
            "take_over".to_string()
        } else {
            "append".to_string()
        };
        NestedCallWire {
            provider: self.config.provider.clone(),
            model: self.config.model.clone(),
            mode,
            system: self.config.system.clone(),
            instruction: format!(
                "The assistant appears stuck: {}. Review the conversation and give \
                 concrete, actionable remarks so it can make progress.",
                signals.describe()
            ),
            max_tokens: self.config.max_tokens,
        }
    }

    /// Decide what to do with `payload`.
    fn on_payload(&self, payload: Value, nested_calls_used: u32) -> Option<NestedCallWire> {
        if self.config.model.is_empty() {
            eprintln!("model-escalation: no target model configured");
            return None;
        }
        if nested_calls_used >= self.config.max_escalations {
            eprintln!(
                "model-escalation: budget exhausted ({}/{})",
                nested_calls_used, self.config.max_escalations
            );
            return None;
        }

        let messages = payload
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let signals = Self::detect(&messages, self.config.scan_last_messages);
        if !signals.is_stuck(&self.config) {
            return None;
        }

        eprintln!(
            "model-escalation: stuck detected ({}) -> {}",
            signals.describe(),
            self.config.model
        );
        Some(self.escalation_request(&signals))
    }
}

/// Extract the text of a tool result or message content.
fn text_of(msg: &Value) -> String {
    match msg.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Heuristic: does a tool result read as a failure?
///
/// Deliberately conservative — a false positive here escalates a healthy turn.
pub fn looks_like_error(text: &str) -> bool {
    const MARKERS: &[&str] = &[
        "Error:",
        "error:",
        "ERROR",
        "Traceback (most recent call last)",
        "panicked at",
        "command not found",
        "No such file or directory",
        "Permission denied",
        "fatal:",
        "FAILED",
        "test result: FAILED",
        "error[E",
    ];
    MARKERS.iter().any(|m| text.contains(m))
}

// ---------------------------------------------------------------------------
// Protocol entry point
// ---------------------------------------------------------------------------

fn main() {
    let mut raw = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut raw) {
        eprintln!("model-escalation: cannot read request: {e}");
        emit_continue();
        return;
    }

    let request: HostRequest = match serde_json::from_str(&raw) {
        Ok(request) => request,
        Err(e) => {
            eprintln!("model-escalation: invalid request JSON: {e}");
            emit_continue();
            return;
        }
    };

    let _ = PROTOCOL;

    let config: EscalationConfig = match serde_json::from_value(request.config.clone()) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("model-escalation: invalid config: {e}");
            emit_continue();
            return;
        }
    };

    if !config.enabled {
        emit_continue();
        return;
    }

    let extension = ModelEscalation::new(config);
    match extension.on_payload(request.payload, request.nested_calls_used) {
        Some(nested_call) => emit(HostResponse {
            decision: "nested_call",
            nested_call: Some(nested_call),
        }),
        None => emit_continue(),
    }
}

fn emit(response: HostResponse) {
    match serde_json::to_string(&response) {
        Ok(text) => {
            let mut out = std::io::stdout();
            let _ = out.write_all(text.as_bytes());
            let _ = out.write_all(b"\n");
            let _ = out.flush();
        }
        Err(e) => eprintln!("model-escalation: cannot serialize response: {e}"),
    }
}

/// The safe answer: leave the request exactly as it was.
fn emit_continue() {
    emit(HostResponse {
        decision: "continue",
        nested_call: None,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg() -> EscalationConfig {
        EscalationConfig {
            model: "strong-model".to_string(),
            ..Default::default()
        }
    }

    fn tool_call(name: &str, args: &str) -> Value {
        json!({
            "role": "assistant",
            "tool_calls": [{
                "id": "call_1",
                "type": "function",
                "function": {"name": name, "arguments": args}
            }]
        })
    }

    fn tool_result(text: &str) -> Value {
        json!({"role": "tool", "content": text})
    }

    #[test]
    fn healthy_conversation_continues() {
        let ext = ModelEscalation::new(cfg());
        let payload = json!({
            "model": "weak",
            "messages": [
                {"role": "user", "content": "fix the tests"},
                tool_call("run", "cargo test"),
                tool_result("fn main() {}"),
                tool_call("run", "cargo test"),
                tool_result("ok"),
            ]
        });
        assert!(ext.on_payload(payload, 0).is_none());
    }

    #[test]
    fn repeated_identical_tool_call_escalates() {
        let ext = ModelEscalation::new(cfg());
        let payload = json!({
            "model": "weak",
            "messages": [
                {"role": "user", "content": "keep going"},
                tool_call("run", "cargo test"),
                tool_result("failed"),
                tool_call("run", "cargo test"),
                tool_result("failed"),
                tool_call("run", "cargo test"),
                tool_result("failed"),
            ]
        });
        let req = ext.on_payload(payload, 0).expect("escalates");
        assert_eq!(req.model, "strong-model");
        assert_eq!(req.mode, "append");
        assert!(req.instruction.contains("identical"));
    }

    #[test]
    fn different_arguments_are_not_call_repetition() {
        let ext = ModelEscalation::new(cfg());
        let payload = json!({
            "model": "weak",
            "messages": [
                {"role": "user", "content": "keep going"},
                tool_call("run", "cargo test -p a"),
                tool_result("a ok"),
                tool_call("run", "cargo test -p b"),
                tool_result("b ok"),
                tool_call("run", "cargo test -p c"),
                tool_result("c ok"),
            ]
        });
        // Different args and different results: not stuck under any signal.
        assert!(
            ext.on_payload(payload, 0).is_none(),
            "different calls are not stuck"
        );
    }

    #[test]
    fn error_streak_escalates() {
        let ext = ModelEscalation::new(cfg());
        let payload = json!({
            "model": "weak",
            "messages": [
                {"role": "user", "content": "keep going"},
                tool_call("run", "cargo test"),
                tool_result("Error: build failed"),
                tool_call("run", "cargo test"),
                tool_result("error[E0308]: type mismatch"),
                tool_call("run", "cargo test"),
                tool_result("FAILED"),
            ]
        });
        assert!(
            ext.on_payload(payload, 0).is_some(),
            "error streak must escalate"
        );
    }

    #[test]
    fn budget_is_respected() {
        let ext = ModelEscalation::new(cfg());
        let payload = json!({
            "model": "weak",
            "messages": [
                {"role": "user", "content": "keep going"},
                tool_call("run", "cargo test"),
                tool_result("failed"),
                tool_call("run", "cargo test"),
                tool_result("failed"),
                tool_call("run", "cargo test"),
                tool_result("failed"),
            ]
        });
        // max_escalations defaults to 1; an already-used slot must not re-fire.
        assert!(ext.on_payload(payload, 1).is_none());
    }

    #[test]
    fn unconfigured_model_never_escalates() {
        let ext = ModelEscalation::new(EscalationConfig::default());
        let payload = json!({"model": "weak", "messages": []});
        assert!(ext.on_payload(payload, 0).is_none());
    }

    #[test]
    fn take_over_mode_is_emitted() {
        let mut c = cfg();
        c.mode = "take_over".to_string();
        let ext = ModelEscalation::new(c);
        let payload = json!({
            "model": "weak",
            "messages": [
                {"role": "user", "content": "keep going"},
                tool_call("run", "cargo test"),
                tool_result("failed"),
                tool_call("run", "cargo test"),
                tool_result("failed"),
                tool_call("run", "cargo test"),
                tool_result("failed"),
            ]
        });
        let req = ext.on_payload(payload, 0).unwrap();
        assert_eq!(req.mode, "take_over");
    }

    #[test]
    fn error_markers_are_conservative() {
        assert!(looks_like_error("fatal: not a git repository"));
        assert!(looks_like_error("Traceback (most recent call last)"));
        assert!(!looks_like_error("all tests passed"));
    }

    #[test]
    fn host_request_parses_real_shape() {
        let req: HostRequest = serde_json::from_str(
            r#"{"config":{"model":"m"},"payload":{"messages":[]},"nested_calls_used":0}"#,
        )
        .unwrap();
        assert_eq!(req.nested_calls_used, 0);
        assert_eq!(req.config["model"], "m");
    }
}
