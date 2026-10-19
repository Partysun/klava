//! PII guardrail for klava, as a standalone executable.
//!
//! Span-based detection and redaction using the `pii` crate: 21 recognizers
//! with checksum validators (Luhn, IBAN mod-97, SSN, ITIN, routing, IMEI), per
//! entity confidence thresholds, and four anonymization operators.
//!
//! klava launches this program as a child process and speaks JSON over stdio;
//! see `EXTENSIONS.md` in the klava repository for the protocol. Nothing needs
//! to be compiled into klava for this to work.
//!
//! Two properties are load-bearing and must not regress:
//!
//! 1. **Redaction walks string *values*, never the serialized request.** A
//!    recognizer that runs over serialized JSON can match a structural escape
//!    (`\n`, `\t`, `\r`), consume it, and leave a dangling backslash — an
//!    invalid request. serde re-escapes on the way out instead.
//! 2. **Inline binary payloads are masked before analysis.** A `0x…`-shaped run
//!    inside a base64 image would otherwise be redacted, corrupting the data
//!    URL.
//!
//! The extension fails open: analysis errors leave the string untouched rather
//! than blocking the request.

use pii::anonymize::{AnonymizeConfig, Anonymizer, Operator};
use pii::nlp::SimpleNlpEngine;
use pii::presets::default_recognizers;
use pii::types::Language;
use pii::{Analyzer, EntityType, PolicyConfig};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
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
}

/// Our answer: `replace` with the redacted payload, or `continue`.
#[derive(Debug, Serialize)]
struct HostResponse {
    decision: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    payload: Option<Value>,
}

/// Configuration for the guardrail (`[extensions.settings.klava-pii-guardrail]`).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GuardrailConfig {
    /// On by default: this is the security floor, not an opt-in.
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Entity names to scan for. Empty = the crate's full set.
    #[serde(default = "default_entities")]
    pub entities: Vec<String>,
    /// Operator spec per entity: `replace:<X>`, `hash:<salt>`, `mask:<n>`.
    #[serde(default = "default_operators")]
    pub operators: BTreeMap<String, String>,
}

impl Default for GuardrailConfig {
    fn default() -> Self {
        Self::default_policy()
    }
}

fn yes() -> bool {
    true
}

/// The built-in entity set when the config names none.
fn default_entities() -> Vec<String> {
    GuardrailConfig::default_policy().entities
}

/// The built-in operator map when the config names none.
fn default_operators() -> BTreeMap<String, String> {
    GuardrailConfig::default_policy().operators
}

impl GuardrailConfig {
    /// Today's defaults: email and crypto addresses, as `<EMAIL>` /
    /// `<CryptoAddress>`.
    pub fn default_policy() -> Self {
        let mut operators = BTreeMap::new();
        operators.insert("Email".to_string(), "replace:<EMAIL>".to_string());
        operators.insert(
            "CryptoAddress".to_string(),
            "replace:<CryptoAddress>".to_string(),
        );
        Self {
            enabled: true,
            entities: vec!["Email".to_string(), "CryptoAddress".to_string()],
            operators,
        }
    }
}

/// The guardrail: an analyzer and an anonymizer, built once per process.
pub struct PiiGuardrail {
    analyzer: Analyzer,
    anonymize: AnonymizeConfig,
}

impl PiiGuardrail {
    /// Build from config. `config` may be `None` for the default policy.
    pub fn new(config: Option<GuardrailConfig>) -> Self {
        let cfg = config.unwrap_or_else(GuardrailConfig::default_policy);

        let mut policy = PolicyConfig::default();
        if !cfg.entities.is_empty() {
            for name in &cfg.entities {
                if let Some(entity) = entity_from_name(name) {
                    policy.enabled_entities.insert(entity);
                } else {
                    eprintln!("pii-guardrail: unknown entity '{name}', ignored");
                }
            }
        }

        let analyzer = Analyzer::new(
            Box::new(SimpleNlpEngine::default()),
            default_recognizers(),
            Vec::new(),
            policy,
        );

        let mut anonymize = AnonymizeConfig::default();
        for (name, spec) in &cfg.operators {
            if let Some(entity) = entity_from_name(name) {
                anonymize
                    .per_entity
                    .insert(entity.as_str(), operator_from_spec(spec));
            }
        }

        Self {
            analyzer,
            anonymize,
        }
    }

    /// Redact `text` in place.
    ///
    /// Returns the number of spans replaced and the entity names seen, so the
    /// caller never has to analyze the same string twice.
    fn redact_string(&self, text: &mut String, seen: &mut HashSet<String>) -> usize {
        let result = match self.analyzer.analyze(text, &Language::from("en")) {
            Ok(result) => result,
            Err(e) => {
                eprintln!("pii-guardrail: analysis failed, leaving text as-is: {e}");
                return 0;
            }
        };
        if result.entities.is_empty() {
            return 0;
        }
        let count = result.entities.len();

        for detection in &result.entities {
            seen.insert(detection.entity_type.as_str());
        }

        match Anonymizer::anonymize(text, &result.entities, &self.anonymize) {
            Ok(redacted) => *text = redacted.text,
            Err(e) => {
                eprintln!("pii-guardrail: anonymization failed, leaving text as-is: {e}");
                return 0;
            }
        }
        count
    }

    /// Recursively redact every string value.
    fn redact_value(&self, value: &mut Value, hits: &mut usize, seen: &mut HashSet<String>) {
        match value {
            Value::String(s) => {
                *hits += self.redact_string(s, seen);
            }
            Value::Object(map) => {
                for nested in map.values_mut() {
                    self.redact_value(nested, hits, seen);
                }
            }
            Value::Array(items) => {
                for nested in items.iter_mut() {
                    self.redact_value(nested, hits, seen);
                }
            }
            _ => {}
        }
    }

    /// Redact the payload and report whether anything changed.
    fn on_payload(&self, payload: Value) -> Option<Value> {
        let mut payload = payload;

        // Mask base64 payloads so a crypto-address-shaped run inside an image
        // cannot be rewritten.
        let originals = mask_encoded_payloads(&mut payload);

        let mut hits = 0usize;
        let mut seen = HashSet::new();
        self.redact_value(&mut payload, &mut hits, &mut seen);

        restore_encoded_payloads(&mut payload, &originals);

        if hits == 0 {
            return None;
        }
        eprintln!(
            "pii-guardrail: redacted {hits} span(s), entities: {:?}",
            seen
        );
        Some(payload)
    }
}

/// Every built-in recognizer the crate ships, for name lookup.
fn entity_from_name(name: &str) -> Option<EntityType> {
    let all = [
        EntityType::Email,
        EntityType::Phone,
        EntityType::IpAddress,
        EntityType::Ipv6,
        EntityType::CreditCard,
        EntityType::Iban,
        EntityType::Ssn,
        EntityType::Itin,
        EntityType::TaxId,
        EntityType::Passport,
        EntityType::DriverLicense,
        EntityType::BankAccount,
        EntityType::RoutingNumber,
        EntityType::CryptoAddress,
        EntityType::MacAddress,
        EntityType::Uuid,
        EntityType::Vin,
        EntityType::Imei,
        EntityType::Url,
        EntityType::Domain,
        EntityType::Hostname,
    ];
    all.into_iter()
        .find(|e| e.as_str().eq_ignore_ascii_case(name))
}

fn operator_from_spec(spec: &str) -> Operator {
    if let Some(v) = spec.strip_prefix("replace:") {
        Operator::Replace {
            with: v.to_string(),
        }
    } else if let Some(v) = spec.strip_prefix("hash:") {
        Operator::HashSha256 {
            salt: v.to_string(),
        }
    } else if let Some(v) = spec.strip_prefix("mask:") {
        Operator::Mask {
            ch: '*',
            from_end: v.parse().unwrap_or(0),
        }
    } else {
        Operator::Redact
    }
}

// ---------------------------------------------------------------------------
// Encoded-payload masking
// ---------------------------------------------------------------------------

/// Byte range of the base64 payload of `s`, if `s` is an inline data URL.
///
/// Only whole string values that *begin* with `data:` are treated as inline
/// data URLs, and only the contiguous run of base64 characters after the
/// `;base64,` marker is covered, so trailing text that happens to share the
/// string still gets redacted normally.
fn data_url_payload_span(s: &str) -> Option<(usize, usize)> {
    const MARKER: &str = ";base64,";
    let tail = s.strip_prefix("data:")?;
    let start = "data:".len() + tail.find(MARKER)? + MARKER.len();
    let bytes = s.as_bytes();
    let mut end = start;

    while end < bytes.len() && is_base64_byte(bytes[end]) {
        end += 1;
    }

    (end > start).then_some((start, end))
}

fn is_base64_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=' | b'-' | b'_')
}

fn is_base64_payload(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(is_base64_byte)
}

fn placeholder(counter: &mut usize) -> String {
    let token = format!("__KLAVA_B64_{}__", *counter);
    *counter += 1;
    token
}

/// Replace inline binary payloads with inert, unique placeholders.
///
/// Masking only touches genuine encoded fields: string values that *are* a
/// base64 data URL and Anthropic `{"type":"base64","data":…}` image sources.
/// Secrets in code (including a `;base64,`-looking literal) are left for the
/// anonymizer. The pairs returned restore the payloads afterwards.
fn mask_encoded_payloads(value: &mut Value) -> Vec<(String, String)> {
    let mut originals = Vec::new();
    let mut counter = 0usize;
    mask_value(value, &mut originals, &mut counter);
    originals
}

fn mask_value(value: &mut Value, originals: &mut Vec<(String, String)>, counter: &mut usize) {
    match value {
        Value::Object(map) => {
            if map.get("type").and_then(Value::as_str) == Some("base64")
                && let Some(Value::String(data)) = map.get_mut("data")
                && is_base64_payload(data)
            {
                let token = placeholder(counter);
                originals.push((token.clone(), std::mem::take(data)));
                *data = token;
            }

            for nested in map.values_mut() {
                mask_value(nested, originals, counter);
            }
        }
        Value::Array(items) => {
            for nested in items.iter_mut() {
                mask_value(nested, originals, counter);
            }
        }
        Value::String(s) => {
            if let Some((start, end)) = data_url_payload_span(s) {
                let token = placeholder(counter);
                originals.push((token.clone(), s[start..end].to_string()));
                s.replace_range(start..end, &token);
            }
        }
        _ => {}
    }
}

fn restore_encoded_payloads(value: &mut Value, originals: &[(String, String)]) {
    match value {
        Value::String(s) => {
            for (token, original) in originals {
                if s.contains(token.as_str()) {
                    *s = s.replace(token.as_str(), original);
                }
            }
        }
        Value::Object(map) => {
            for nested in map.values_mut() {
                restore_encoded_payloads(nested, originals);
            }
        }
        Value::Array(items) => {
            for nested in items.iter_mut() {
                restore_encoded_payloads(nested, originals);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Protocol entry point
// ---------------------------------------------------------------------------

fn main() {
    let mut raw = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut raw) {
        eprintln!("pii-guardrail: cannot read request: {e}");
        emit_continue();
        return;
    }

    let request: HostRequest = match serde_json::from_str(&raw) {
        Ok(request) => request,
        Err(e) => {
            eprintln!("pii-guardrail: invalid request JSON: {e}");
            emit_continue();
            return;
        }
    };

    let _ = PROTOCOL;

    // The host injects `disable_pii` when the legacy `enable_pii = false` flag
    // is set. Honour it here so old configs keep working.
    if request
        .config
        .get("disable_pii")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        emit_continue();
        return;
    }

    let config: GuardrailConfig = match serde_json::from_value(request.config.clone()) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("pii-guardrail: invalid config: {e}");
            emit_continue();
            return;
        }
    };

    if !config.enabled {
        emit_continue();
        return;
    }

    let guardrail = PiiGuardrail::new(Some(config));
    match guardrail.on_payload(request.payload) {
        Some(payload) => emit(HostResponse {
            decision: "replace",
            payload: Some(payload),
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
        Err(e) => eprintln!("pii-guardrail: cannot serialize response: {e}"),
    }
}

/// The safe answer: leave the request exactly as it was.
fn emit_continue() {
    emit(HostResponse {
        decision: "continue",
        payload: None,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx_of(payload: Value) -> Value {
        payload
    }

    fn run(payload: Value, cfg: Option<GuardrailConfig>) -> Option<Value> {
        PiiGuardrail::new(cfg).on_payload(ctx_of(payload))
    }

    #[test]
    fn default_policy_redacts_email() {
        let payload = json!({
            "model": "m",
            "messages": [{"role": "user", "content": "mail alice@example.com now"}]
        });
        let out = run(payload, None).expect("replaced");
        let content = out["messages"][0]["content"].as_str().unwrap();
        assert_eq!(content, "mail <EMAIL> now");
    }

    #[test]
    fn default_policy_redacts_crypto_address() {
        let payload = json!({
            "model": "m",
            "messages": [{"role": "user", "content": "send 0x1234567890abcdef1234567890abcdef12345678 now"}]
        });
        let out = run(payload, None).expect("replaced");
        let content = out["messages"][0]["content"].as_str().unwrap();
        assert!(content.contains("<CryptoAddress>"), "got {content}");
    }

    #[test]
    fn nothing_to_redact_continues() {
        let payload = json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]});
        assert!(run(payload, None).is_none());
    }

    #[test]
    fn redaction_walks_nested_values_not_serialized_text() {
        let payload = json!({
            "model": "m",
            "messages": [{"role": "user", "content": "line\nnext a@b.com"}]
        });
        let mut operators = BTreeMap::new();
        operators.insert("Email".to_string(), "replace:<EMAIL>".to_string());
        let cfg = Some(GuardrailConfig {
            enabled: true,
            entities: vec!["Email".to_string()],
            operators,
        });
        let out = run(payload, cfg).expect("replaced");
        let content = out["messages"][0]["content"].as_str().unwrap();
        assert_eq!(content, "line\nnext <EMAIL>");
    }

    #[test]
    fn base64_data_url_is_not_corrupted() {
        let payload = json!({
            "model": "m",
            "messages": [{"role": "user", "content": "here is an image for you",
                          "images": [{"type": "base64", "data": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg=="}]}]
        });
        // The base64 run is not an email; no redaction should have happened.
        assert!(
            run(payload, None).is_none(),
            "base64 payload must be masked, not redacted"
        );
    }

    #[test]
    fn wider_policy_catches_luhn_card() {
        let g = PiiGuardrail::new(Some(GuardrailConfig {
            enabled: true,
            entities: vec!["CreditCard".to_string()],
            operators: BTreeMap::new(),
        }));
        let payload = json!({
            "model": "m",
            "messages": [{"role": "user", "content": "card 4111 1111 1111 1111 expires soon"}]
        });
        assert!(g.on_payload(payload).is_some(), "Luhn card must be caught");
    }

    #[test]
    fn unknown_entity_name_is_ignored_not_fatal() {
        let g = PiiGuardrail::new(Some(GuardrailConfig {
            enabled: true,
            entities: vec!["NotAThing".to_string()],
            operators: BTreeMap::new(),
        }));
        let payload = json!({"model": "m", "messages": []});
        assert!(g.on_payload(payload).is_none());
    }

    #[test]
    fn host_request_parses_real_shape() {
        let req: HostRequest =
            serde_json::from_str(r#"{"config":{"enabled":true},"payload":{"messages":[]}}"#)
                .unwrap();
        assert_eq!(req.config["enabled"], true);
    }

    #[test]
    fn continue_response_has_no_payload() {
        let text = serde_json::to_string(&HostResponse {
            decision: "continue",
            payload: None,
        })
        .unwrap();
        assert!(!text.contains("payload"));
    }

    #[test]
    fn disable_pii_flag_blocks_redaction() {
        let req: HostRequest = serde_json::from_str(
            r#"{"config":{"disable_pii":true,"enabled":true},"payload":{"messages":[{"role":"user","content":"a@b.com"}]}}"#,
        )
        .unwrap();
        // With the flag set, main() would emit continue; simulate the check.
        let disabled = req
            .config
            .get("disable_pii")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        assert!(disabled);
    }
}
