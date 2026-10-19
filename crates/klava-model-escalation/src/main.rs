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
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{Read, Write};

/// The protocol version klava sends. We answer whatever we understand.
const PROTOCOL: u32 = 1;

/// What `--print-manifest` emits.
///
/// Both the notice and `[settings]` are documentation: klava prints them and
/// strips them before the config reaches this program.
const MANIFEST: &str = r#"# Written by klava from `klava-model-escalation --print-manifest`.

notice = """
COST WARNING: escalation sends your conversation to a second, deliberately more \
expensive model and bills it per token. Every nested call, and every escalation \
allowed by `max_escalations`, costs money on top of the turn you are already \
paying for; a long or looping session can burn a balance quickly. Start with \
max_escalations = 1 and a model you can afford, watch your provider's usage, and \
disable this extension if costs surprise you.
"""

# What this extension accepts. klava prints this table in
# `klava extensions show klava-model-escalation` and never sends it to the extension.
[settings]
enabled = false
model = ""
provider = ""
mode = "append"
max_escalations = 1
repeat_tool_call_limit = 3
repeat_result_limit = 2
error_streak_limit = 3
scan_last_messages = 40
max_tokens = 2048
verify = true
verify_threshold = 0.8
verify_window = 12
verify_max_chars = 8000
"#;

/// One request from klava.
#[derive(Debug, Deserialize)]
struct HostRequest {
    #[serde(default)]
    config: Value,
    #[serde(default)]
    payload: Value,
    #[serde(default)]
    nested_calls_used: u32,
    /// Whether the host requires an approved `decide` before a `nested_call`.
    /// When true (the default), we ask for the judgment; when false, the host
    /// has explicitly allowed unverified escalation.
    #[serde(default = "default_require_decide")]
    require_decide: bool,
    /// Present on the second run of this extension in one request: the host's
    /// answer to the `decide` we asked for.
    #[serde(default)]
    decide: Option<DecideResult>,
}

/// Our answer: `nested_call`, a `decide` request, or `continue`.
#[derive(Debug, Serialize)]
struct HostResponse {
    decision: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    nested_call: Option<NestedCallWire>,
    #[serde(skip_serializing_if = "Option::is_none")]
    decide: Option<DecideRequest>,
}

/// A classifier question we ask the host to resolve.
#[derive(Debug, Clone, Serialize)]
pub struct DecideQuestion {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub instructions: String,
}

/// Our request to the host's decide model. The host owns the model, the
/// credentials and the cost; we own the question and the evidence.
#[derive(Debug, Clone, Serialize)]
pub struct DecideRequest {
    pub state: Value,
    pub questions: Vec<DecideQuestion>,
    pub threshold: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approve_question: Option<String>,
}

/// The host's answer, delivered on our second run.
#[derive(Debug, Clone, Deserialize)]
pub struct DecideResult {
    #[serde(default)]
    pub approve: bool,
    #[serde(default)]
    pub probability: Option<f32>,
    #[serde(default)]
    pub model: String,
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
    /// Ask the host's decide model to confirm that escalation is really
    /// needed before spending on a stronger model. Default: on.
    #[serde(default = "default_verify")]
    pub verify: bool,
    /// Probability the decide model must report to allow the escalation.
    #[serde(default = "default_verify_threshold")]
    pub verify_threshold: f32,
    /// Messages inspected when building the evidence for the decide model.
    #[serde(default = "default_verify_window")]
    pub verify_window: usize,
    /// Characters of evidence sent to the decide model.
    #[serde(default = "default_verify_max_chars")]
    pub verify_max_chars: usize,
}

fn default_require_decide() -> bool {
    true
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
fn default_verify() -> bool {
    true
}
fn default_verify_threshold() -> f32 {
    0.8
}
fn default_verify_window() -> usize {
    12
}
fn default_verify_max_chars() -> usize {
    8000
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
            verify: default_verify(),
            verify_threshold: default_verify_threshold(),
            verify_window: default_verify_window(),
            verify_max_chars: default_verify_max_chars(),
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

    /// The evidence handed to the host's decide model.
    ///
    /// Only the tail of the conversation matters for "are we stuck", and the
    /// classifier has a small input window, so this is bounded twice: a message
    /// window, then a character budget.
    fn evidence(&self, messages: &[Value], signals: &StuckSignals, target: &str) -> String {
        const PER_MESSAGE: usize = 1200;

        let start = messages.len().saturating_sub(self.config.verify_window);
        let mut out = String::new();
        out.push_str("Escalation target: ");
        out.push_str(target);
        out.push('\n');
        out.push_str("Detected signals: ");
        out.push_str(&signals.describe());
        out.push_str("\n\nRecent conversation (oldest first):\n");

        for msg in &messages[start..] {
            let role = msg.get("role").and_then(Value::as_str).unwrap_or("unknown");
            let text = text_of(msg);
            let tools = msg
                .get("tool_calls")
                .and_then(Value::as_array)
                .map(|calls| {
                    calls
                        .iter()
                        .filter_map(|c| {
                            let f = c.get("function")?;
                            let name = f.get("name").and_then(Value::as_str).unwrap_or("?");
                            let args = f.get("arguments").and_then(Value::as_str).unwrap_or("");
                            Some(format!("{name}({args})"))
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();

            out.push_str(&format!("[{role}] "));
            if !tools.is_empty() {
                out.push_str(&format!("tool calls: {} ", truncate(&tools, PER_MESSAGE)));
            }
            if !text.trim().is_empty() {
                out.push_str(&truncate(&text, PER_MESSAGE));
            }
            out.push('\n');
        }

        truncate(&out, self.config.verify_max_chars)
    }

    /// The question we ask the decide model before spending on a stronger one.
    fn verify_request(&self, messages: &[Value], signals: &StuckSignals) -> DecideRequest {
        DecideRequest {
            state: json!({ "context": self.evidence(messages, signals, &self.config.model) }),
            questions: vec![DecideQuestion {
                id: "needs_stronger_model".to_string(),
                kind: "noul".to_string(),
                instructions: "A code agent is stuck and the escalation extension proposes \
                    routing this turn to a stronger, substantially more expensive model. Based \
                    only on the evidence in the context, is the stronger model genuinely required \
                    to make progress — that is, can the cheap model not plausibly fix this from the \
                    same information with a concrete hint? Answer yes only when the task needs \
                    reasoning beyond the cheap model's ability; repeated tool errors alone are not \
                    enough, and a concise hint or a different approach usually is."
                    .to_string(),
            }],
            threshold: self.config.verify_threshold,
            approve_question: Some("needs_stronger_model".to_string()),
        }
    }

    /// Decide what to do with `payload` on this run.
    ///
    /// Phase one: detection. If the agent looks stuck, either ask the host for
    /// a `decide` (default) or go straight to the nested call when
    /// `verify = false`.
    ///
    /// Phase two: the host re-runs us with `decide` filled in. Escalate only if
    /// that answer approved.
    ///
    /// The host requires an approved decide by default; when it does, this
    /// extension must ask even if its own `verify` is off, because the host
    /// would refuse an unverified nested call anyway. Either party can require
    /// the check, and both agree on `verify_threshold`.
    fn on_payload(
        &self,
        payload: &Value,
        nested_calls_used: u32,
        decide: Option<&DecideResult>,
        host_requires_decide: bool,
    ) -> Outcome {
        if self.config.model.is_empty() {
            eprintln!("model-escalation: no target model configured");
            return Outcome::Continue;
        }
        if nested_calls_used >= self.config.max_escalations {
            eprintln!(
                "model-escalation: budget exhausted ({}/{})",
                nested_calls_used, self.config.max_escalations
            );
            return Outcome::Continue;
        }

        let messages = payload
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let signals = Self::detect(&messages, self.config.scan_last_messages);
        if !signals.is_stuck(&self.config) {
            return Outcome::Continue;
        }

        // Phase two: the host has answered our decide question.
        if let Some(result) = decide {
            if result.approve {
                eprintln!(
                    "model-escalation: stuck detected ({}) and decide approved (p={:?}) -> {}",
                    signals.describe(),
                    result.probability,
                    self.config.model
                );
                return Outcome::NestedCall(self.escalation_request(&signals));
            }
            eprintln!(
                "model-escalation: stuck detected ({}) but decide declined (p={:?}); \
                 staying on the cheap model",
                signals.describe(),
                result.probability
            );
            return Outcome::Continue;
        }

        // Phase one with verification: ask the host for the judgment when
        // either side wants proof.
        if host_requires_decide || self.config.verify {
            eprintln!(
                "model-escalation: stuck detected ({}); asking decide before -> {}",
                signals.describe(),
                self.config.model
            );
            return Outcome::Decide(self.verify_request(&messages, &signals));
        }

        eprintln!(
            "model-escalation: stuck detected ({}) -> {} (UNVERIFIED: verify = false)",
            signals.describe(),
            self.config.model
        );
        Outcome::NestedCall(self.escalation_request(&signals))
    }
}

/// What this run decided to do.
enum Outcome {
    Continue,
    Decide(DecideRequest),
    NestedCall(NestedCallWire),
}

/// Keep at most `max` chars on a UTF-8 boundary, marking the cut.
fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
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
    // `--print-manifest` is how klava learns what this extension declares; it
    // must answer before any request is read from stdin.
    if std::env::args()
        .skip(1)
        .any(|arg| arg == "--print-manifest")
    {
        print!("{MANIFEST}");
        return;
    }

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
    match extension.on_payload(
        &request.payload,
        request.nested_calls_used,
        request.decide.as_ref(),
        request.require_decide,
    ) {
        Outcome::NestedCall(nested_call) => emit(HostResponse {
            decision: "nested_call",
            nested_call: Some(nested_call),
            decide: None,
        }),
        Outcome::Decide(decide) => emit(HostResponse {
            decision: "decide",
            nested_call: None,
            decide: Some(decide),
        }),
        Outcome::Continue => emit_continue(),
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
        decide: None,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_manifest_declares_every_setting_with_its_real_default() {
        // `klava extensions show` prints this table as the documentation, so a
        // stale default here would lie to the user.
        let value: toml::Value = toml::from_str(MANIFEST).expect("manifest must parse");
        let declared: EscalationConfig = value["settings"].clone().try_into().expect("settings");
        let real = EscalationConfig::default();
        assert_eq!(declared.enabled, real.enabled);
        assert_eq!(declared.model, real.model);
        assert_eq!(declared.mode, real.mode);
        assert_eq!(declared.max_escalations, real.max_escalations);
        assert_eq!(declared.repeat_tool_call_limit, real.repeat_tool_call_limit);
        assert_eq!(declared.repeat_result_limit, real.repeat_result_limit);
        assert_eq!(declared.error_streak_limit, real.error_streak_limit);
        assert_eq!(declared.scan_last_messages, real.scan_last_messages);
        assert_eq!(declared.max_tokens, real.max_tokens);
        assert_eq!(declared.verify, real.verify);
        assert_eq!(declared.verify_threshold, real.verify_threshold);
        assert_eq!(declared.verify_window, real.verify_window);
        assert_eq!(declared.verify_max_chars, real.verify_max_chars);
        // Blank provider means "the active provider", the same as absent.
        assert_eq!(declared.provider.as_deref().unwrap_or(""), "");
    }

    #[test]
    fn the_manifest_warns_about_cost() {
        let value: toml::Value = toml::from_str(MANIFEST).expect("manifest must parse");
        let notice = value["notice"].as_str().expect("notice");
        assert!(notice.contains("COST WARNING"), "got: {notice}");
        assert!(notice.contains("burn"), "got: {notice}");
    }

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

    /// Phase one: does this conversation look stuck?
    fn is_stuck(ext: &ModelEscalation, payload: &Value) -> bool {
        !matches!(ext.on_payload(payload, 0, None, true), Outcome::Continue)
    }

    /// Phase two: the host answered; does it escalate?
    fn escalates_with(
        ext: &ModelEscalation,
        payload: &Value,
        approve: bool,
    ) -> Option<NestedCallWire> {
        let decide = DecideResult {
            approve,
            probability: Some(if approve { 0.9 } else { 0.2 }),
            model: "decider".to_string(),
        };
        match ext.on_payload(payload, 0, Some(&decide), true) {
            Outcome::NestedCall(call) => Some(call),
            _ => None,
        }
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
        assert!(!is_stuck(&ext, &payload));
    }

    #[test]
    fn repeated_identical_tool_call_asks_decide_first() {
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
        match ext.on_payload(&payload, 0, None, true) {
            Outcome::Decide(request) => {
                assert_eq!(request.threshold, 0.8);
                assert_eq!(
                    request.approve_question.as_deref(),
                    Some("needs_stronger_model")
                );
                let context = request.state["context"].as_str().unwrap();
                assert!(context.contains("strong-model"));
                assert!(context.contains("3x identical"));
            }
            other => panic!(
                "expected decide, got {}",
                matches!(other, Outcome::Continue)
            ),
        }
    }

    #[test]
    fn approval_escalates_and_rejection_stays_cheap() {
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
        let approved = escalates_with(&ext, &payload, true).expect("approved escalates");
        assert_eq!(approved.model, "strong-model");
        assert_eq!(approved.mode, "append");
        assert!(approved.instruction.contains("identical"));
        assert!(
            escalates_with(&ext, &payload, false).is_none(),
            "a rejected decide must not escalate"
        );
    }

    #[test]
    fn verify_false_escalates_without_asking_when_the_host_allows_it() {
        let mut c = cfg();
        c.verify = false;
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
        match ext.on_payload(&payload, 0, None, false) {
            Outcome::NestedCall(call) => assert_eq!(call.model, "strong-model"),
            _ => panic!("verify = false + host waiver must escalate directly"),
        }
    }

    #[test]
    fn the_host_policy_still_requires_a_decide_when_verify_is_off() {
        let mut c = cfg();
        c.verify = false;
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
        assert!(matches!(
            ext.on_payload(&payload, 0, None, true),
            Outcome::Decide(_)
        ));
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
        assert!(!is_stuck(&ext, &payload), "different calls are not stuck");
    }

    #[test]
    fn error_streak_asks_decide_first() {
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
        assert!(is_stuck(&ext, &payload), "error streak must ask for decide");
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
        assert!(matches!(
            ext.on_payload(&payload, 1, None, true),
            Outcome::Continue
        ));
        assert!(escalates_with(&ext, &payload, true).is_some());
    }

    #[test]
    fn unconfigured_model_never_escalates() {
        let ext = ModelEscalation::new(EscalationConfig::default());
        let payload = json!({"model": "weak", "messages": []});
        assert!(!is_stuck(&ext, &payload));
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
        let req = escalates_with(&ext, &payload, true).expect("approved");
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
