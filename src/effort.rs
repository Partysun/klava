//! Reasoning-effort policy: which `reasoning_effort` klava sends upstream.
//!
//! Three client surfaces express reasoning differently, and many clients say
//! nothing at all (Claude Code only sends `output_config.effort` when its own
//! model is known to support it, and opencode/pi depend on per-model options
//! the user may never set). This module keeps the decision in one place:
//!
//! * [`EffortRequest`] is what the *client* asked for, read from whichever
//!   surface the request arrived on.
//! * [`EffortPolicy`] is what the *config* says: a per-model default, a global
//!   default, and optional caps.
//!
//! Resolution: the client's level wins, then the per-model default, then the
//! global default; the result is clamped by the cap. A client that explicitly
//! disabled reasoning wins over every default — nothing is added. When nothing
//! is configured anywhere the field is left out of the payload entirely, so
//! endpoints that reject `reasoning_effort` (local Ollama, llama.cpp, some
//! vLLM setups) keep working untouched.

use crate::config::Config;
use crate::models::anthropic::AnthropicRequest;
use crate::models::openai::{OpenAIRequest, ReasoningEffort};
use crate::models::responses::ResponsesRequest;
use crate::providers::Model;
use serde_json::Value;

/// What the client asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EffortRequest {
    /// The client said nothing about reasoning.
    #[default]
    Unspecified,
    /// The client asked for this level.
    Level(ReasoningEffort),
    /// The client explicitly turned reasoning off.
    Suppress,
}

impl EffortRequest {
    /// From an OpenAI chat-completions request. An explicit `none` is an
    /// opt-out, not a level: it must not be overridden by a config default.
    pub fn from_openai(req: &OpenAIRequest) -> Self {
        match req.reasoning_effort {
            None => EffortRequest::Unspecified,
            Some(ReasoningEffort::None) => EffortRequest::Suppress,
            Some(level) => EffortRequest::Level(level),
        }
    }

    /// From a Responses request. `none`/`minimal` match the routing rule in
    /// `responses_request_is_reasoning`, which treats both as "no reasoning".
    pub fn from_responses(req: &ResponsesRequest) -> Self {
        let Some(effort) = req.reasoning.as_ref().and_then(|r| r.effort.as_deref()) else {
            return EffortRequest::Unspecified;
        };
        match effort.trim().to_ascii_lowercase().as_str() {
            "none" | "minimal" => EffortRequest::Suppress,
            other => match ReasoningEffort::parse(other) {
                Some(level) => EffortRequest::Level(level),
                None => {
                    tracing::debug!(
                        "unrecognised reasoning.effort '{other}' - leaving effort unset"
                    );
                    EffortRequest::Unspecified
                }
            },
        }
    }

    /// From an Anthropic request. Claude Code's `/effort`, `--effort` and
    /// `effortLevel` all end up as `output_config.effort`; `thinking.type`
    /// carries the on/off decision.
    pub fn from_anthropic(req: &AnthropicRequest) -> Self {
        if let Some(effort) = req
            .extra
            .get("output_config")
            .and_then(|v| v.get("effort"))
            .and_then(Value::as_str)
        {
            return match ReasoningEffort::parse(effort) {
                Some(level) => EffortRequest::Level(level),
                None => {
                    tracing::debug!(
                        "unrecognised output_config.effort '{effort}' - leaving effort unset"
                    );
                    EffortRequest::Unspecified
                }
            };
        }

        let thinking = req
            .extra
            .get("thinking")
            .and_then(|v| v.as_object())
            .and_then(|o| o.get("type").and_then(Value::as_str));
        if thinking == Some("disabled") {
            return EffortRequest::Suppress;
        }

        EffortRequest::Unspecified
    }
}

/// The configured effort defaults for one model entry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EffortPolicy {
    model_default: Option<ReasoningEffort>,
    global_default: Option<ReasoningEffort>,
    model_cap: Option<ReasoningEffort>,
    global_cap: Option<ReasoningEffort>,
}

impl EffortPolicy {
    /// Build the policy for `entry` from the config it was resolved through.
    /// Values that fail to parse are reported by `Config::validate_complete`;
    /// here they are ignored rather than failing a live request.
    pub fn for_entry(config: &Config, entry: &Model) -> Self {
        Self {
            model_default: parse_level(entry.reasoning_effort.as_deref()),
            global_default: parse_level(config.reasoning_effort.as_deref()),
            model_cap: parse_level(entry.reasoning_effort_cap.as_deref()),
            global_cap: parse_level(config.reasoning_effort_cap.as_deref()),
        }
    }

    /// Resolve what to send. `None` means "add no field".
    pub fn resolve(&self, request: EffortRequest) -> Option<ReasoningEffort> {
        let level = match request {
            EffortRequest::Suppress => return None,
            EffortRequest::Unspecified => self.model_default.or(self.global_default)?,
            EffortRequest::Level(level) => level,
        };
        Some(self.clamp(level))
    }

    /// The level an unconfigured client would get, for the startup banner.
    pub fn default_level(&self) -> Option<ReasoningEffort> {
        self.resolve(EffortRequest::Unspecified)
    }

    /// The cap in effect for this entry, if any.
    pub fn cap(&self) -> Option<ReasoningEffort> {
        self.model_cap.or(self.global_cap)
    }

    /// Whether the config asks for anything at all.
    pub fn is_configured(&self) -> bool {
        self.model_default.is_some() || self.global_default.is_some() || self.cap().is_some()
    }

    /// Lower `level` to the cap when the cap is stricter.
    fn clamp(&self, level: ReasoningEffort) -> ReasoningEffort {
        match self.cap() {
            Some(cap) if cap.rank() < level.rank() => {
                tracing::debug!(
                    "reasoning_effort {} clamped to {} by the configured cap",
                    level.variant_name(),
                    cap.variant_name()
                );
                cap
            }
            _ => level,
        }
    }
}

fn parse_level(value: Option<&str>) -> Option<ReasoningEffort> {
    value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .and_then(ReasoningEffort::parse)
}

/// Validate every effort value in the config, with the entry name in the
/// error so a typo is easy to find.
pub fn validate(config: &Config) -> crate::error::Result<()> {
    let valid: Vec<&str> = ReasoningEffort::ALL
        .iter()
        .map(|l| l.variant_name())
        .collect();
    let expected = valid.join("|");

    for (label, value) in [
        ("reasoning_effort", config.reasoning_effort.as_deref()),
        (
            "reasoning_effort_cap",
            config.reasoning_effort_cap.as_deref(),
        ),
    ] {
        if let Some(value) = value.map(str::trim).filter(|v| !v.is_empty())
            && ReasoningEffort::parse(value).is_none()
        {
            return Err(crate::error::Error::InvalidReasoningEffort(format!(
                "{label} = \"{value}\" (expected {expected})"
            )));
        }
    }

    for entry in &config.models {
        for (label, value) in [
            ("reasoning_effort", entry.reasoning_effort.as_deref()),
            (
                "reasoning_effort_cap",
                entry.reasoning_effort_cap.as_deref(),
            ),
        ] {
            if let Some(value) = value.map(str::trim).filter(|v| !v.is_empty())
                && ReasoningEffort::parse(value).is_none()
            {
                return Err(crate::error::Error::InvalidReasoningEffort(format!(
                    "[[models]] \"{}\": {label} = \"{value}\" (expected {expected})",
                    entry.name
                )));
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::Type as ProviderType;
    use serde_json::json;

    fn entry() -> Model {
        Model {
            name: "~deepseek/deepseek-flash-latest".to_string(),
            provider_type: ProviderType::Klava,
            ..Model::default()
        }
    }

    fn policy(
        model_default: Option<&str>,
        global_default: Option<&str>,
        model_cap: Option<&str>,
        global_cap: Option<&str>,
    ) -> EffortPolicy {
        let config = Config {
            reasoning_effort: global_default.map(str::to_string),
            reasoning_effort_cap: global_cap.map(str::to_string),
            ..Config::default()
        };
        let model = Model {
            reasoning_effort: model_default.map(str::to_string),
            reasoning_effort_cap: model_cap.map(str::to_string),
            ..entry()
        };
        EffortPolicy::for_entry(&config, &model)
    }

    #[test]
    fn nothing_configured_adds_no_field() {
        let p = policy(None, None, None, None);
        assert!(!p.is_configured());
        assert_eq!(p.resolve(EffortRequest::Unspecified), None);
        assert_eq!(p.default_level(), None);
    }

    #[test]
    fn per_model_default_beats_the_global_one() {
        let p = policy(Some("max"), Some("low"), None, None);
        assert_eq!(
            p.resolve(EffortRequest::Unspecified),
            Some(ReasoningEffort::Max)
        );
    }

    #[test]
    fn global_default_is_the_fallback() {
        let p = policy(None, Some("high"), None, None);
        assert_eq!(
            p.resolve(EffortRequest::Unspecified),
            Some(ReasoningEffort::High)
        );
    }

    #[test]
    fn the_clients_level_beats_every_default() {
        let p = policy(Some("low"), Some("low"), None, None);
        assert_eq!(
            p.resolve(EffortRequest::Level(ReasoningEffort::Medium)),
            Some(ReasoningEffort::Medium)
        );
    }

    #[test]
    fn a_client_opt_out_beats_the_default() {
        let p = policy(Some("max"), Some("max"), None, None);
        assert_eq!(p.resolve(EffortRequest::Suppress), None);
    }

    #[test]
    fn the_cap_clamps_client_and_config_levels() {
        let p = policy(Some("max"), None, Some("high"), None);
        assert_eq!(
            p.resolve(EffortRequest::Unspecified),
            Some(ReasoningEffort::High)
        );
        assert_eq!(
            p.resolve(EffortRequest::Level(ReasoningEffort::XHigh)),
            Some(ReasoningEffort::High)
        );
        // A level below the cap is untouched.
        assert_eq!(
            p.resolve(EffortRequest::Level(ReasoningEffort::Low)),
            Some(ReasoningEffort::Low)
        );
    }

    #[test]
    fn the_per_model_cap_beats_the_global_cap() {
        let p = policy(None, Some("max"), Some("medium"), Some("xhigh"));
        assert_eq!(
            p.resolve(EffortRequest::Unspecified),
            Some(ReasoningEffort::Medium)
        );
    }

    #[test]
    fn the_global_cap_applies_when_the_model_has_none() {
        let p = policy(None, Some("max"), None, Some("high"));
        assert_eq!(
            p.resolve(EffortRequest::Unspecified),
            Some(ReasoningEffort::High)
        );
    }

    #[test]
    fn an_unknown_config_value_is_ignored_at_resolve_time() {
        let p = policy(Some("turbo"), None, None, None);
        assert_eq!(p.resolve(EffortRequest::Unspecified), None);
        assert!(!p.is_configured());
    }

    #[test]
    fn anthropic_output_config_effort_is_read() {
        let req: AnthropicRequest = serde_json::from_value(json!({
            "model": "claude-opus-5",
            "max_tokens": 100,
            "messages": [{"role": "user", "content": "hi"}],
            "output_config": {"effort": "max"}
        }))
        .unwrap();
        assert_eq!(
            EffortRequest::from_anthropic(&req),
            EffortRequest::Level(ReasoningEffort::Max)
        );
    }

    #[test]
    fn anthropic_disabled_thinking_suppresses_the_default() {
        let req: AnthropicRequest = serde_json::from_value(json!({
            "model": "claude-opus-5",
            "max_tokens": 100,
            "messages": [{"role": "user", "content": "hi"}],
            "thinking": {"type": "disabled"}
        }))
        .unwrap();
        assert_eq!(EffortRequest::from_anthropic(&req), EffortRequest::Suppress);
    }

    #[test]
    fn anthropic_adaptive_thinking_alone_leaves_the_default_in_charge() {
        let req: AnthropicRequest = serde_json::from_value(json!({
            "model": "claude-opus-5",
            "max_tokens": 100,
            "messages": [{"role": "user", "content": "hi"}],
            "thinking": {"type": "adaptive"}
        }))
        .unwrap();
        assert_eq!(
            EffortRequest::from_anthropic(&req),
            EffortRequest::Unspecified
        );
    }

    #[test]
    fn openai_none_is_an_opt_out() {
        let req: OpenAIRequest = serde_json::from_value(json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hi"}],
            "reasoning_effort": "none"
        }))
        .unwrap();
        assert_eq!(EffortRequest::from_openai(&req), EffortRequest::Suppress);
    }

    #[test]
    fn openai_absent_effort_is_unspecified() {
        let req: OpenAIRequest = serde_json::from_value(json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .unwrap();
        assert_eq!(EffortRequest::from_openai(&req), EffortRequest::Unspecified);
    }

    #[test]
    fn responses_minimal_and_none_are_opt_outs() {
        for effort in ["none", "minimal"] {
            let req: ResponsesRequest = serde_json::from_value(json!({
                "model": "m",
                "input": "hi",
                "reasoning": {"effort": effort}
            }))
            .unwrap();
            assert_eq!(
                EffortRequest::from_responses(&req),
                EffortRequest::Suppress,
                "{effort}"
            );
        }
    }

    #[test]
    fn responses_high_is_a_level() {
        let req: ResponsesRequest = serde_json::from_value(json!({
            "model": "m",
            "input": "hi",
            "reasoning": {"effort": "high"}
        }))
        .unwrap();
        assert_eq!(
            EffortRequest::from_responses(&req),
            EffortRequest::Level(ReasoningEffort::High)
        );
    }

    #[test]
    fn invalid_config_values_are_rejected() {
        let config = Config {
            reasoning_effort: Some("turbo".to_string()),
            ..Config::default()
        };
        let err = validate(&config).unwrap_err();
        assert!(err.to_string().contains("turbo"), "{err}");

        let config = Config {
            models: vec![Model {
                reasoning_effort_cap: Some("11".to_string()),
                ..entry()
            }],
            ..Config::default()
        };
        let err = validate(&config).unwrap_err();
        assert!(
            err.to_string().contains("~deepseek/deepseek-flash-latest"),
            "{err}"
        );
    }

    #[test]
    fn valid_values_pass_validation() {
        let config = Config {
            reasoning_effort: Some("High".to_string()),
            reasoning_effort_cap: Some("xhigh".to_string()),
            models: vec![Model {
                reasoning_effort: Some("max".to_string()),
                ..entry()
            }],
            ..Config::default()
        };
        assert!(validate(&config).is_ok());
    }

    #[test]
    fn ranks_are_ordered() {
        let ranks: Vec<u8> = ReasoningEffort::ALL.iter().map(|l| l.rank()).collect();
        let mut sorted = ranks.clone();
        sorted.sort_unstable();
        assert_eq!(ranks, sorted);
        assert_eq!(ReasoningEffort::None.rank(), 0);
        assert_eq!(ReasoningEffort::Max.rank(), 6);
    }
}
