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
//! than blocking the request. The one exception is a payload too large to
//! analyze (see `MAX_ANALYZE_BYTES`), which is refused outright — forwarding it
//! unread would be the leak this program exists to prevent.
//!
//! Operators are applied here, not through `Anonymizer::anonymize`, because
//! that function replaces spans back-to-front with `String::replace_range`:
//! every detection memmoves the rest of the string, so a multi-megabyte tool
//! dump with tens of thousands of matches costs O(hits × len) — minutes of CPU,
//! which the host's 20 s budget turns into a rejected request. The analyzer
//! already returns detections sorted and non-overlapping, so one forward copy
//! is equivalent and linear.

use pii::anonymize::{AnonymizeConfig, Operator};
use pii::nlp::SimpleNlpEngine;
use pii::presets::default_recognizers;
use pii::types::{Detection, Language};
use pii::{Analyzer, EntityType, PolicyConfig};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::io::{Read, Write};

/// The protocol version klava sends. We answer whatever we understand.
const PROTOCOL: u32 = 1;

/// How much text, in bytes, one request may carry before it is refused.
///
/// The cap follows from the two limits klava puts on this extension rather than
/// from a taste for small numbers:
///
/// * A redacted payload is echoed back, and klava rejects a reply over 8 MiB
///   (`MAX_STDOUT`). Redaction can *grow* text — the shortest `Domain`/`Hostname`
///   matches are a few bytes and their placeholder is `<Domain>` — so the input
///   cap must leave room for roughly doubling. 3 MiB measures out at ~6.6 MB
///   even on a payload made only of `a.co`, leaving margin for JSON escaping.
/// * The analyzer tokenizes into one owned token per word, so peak RSS runs
///   ~200 MB per MiB of text; 3 MiB keeps the worst case near 600 MB.
///
/// At 3 MiB the phases stay far inside the 20 s budget: analysis is a few
/// hundred milliseconds and the splice below a few milliseconds. Real prompts
/// and accumulated tool output fit; a multi-megabyte dump does not, and it is
/// refused whole (see `on_payload`) rather than being redacted halfway and then
/// rejected for size — or, worse, forwarded unread.
const MAX_ANALYZE_BYTES: usize = 3 * 1024 * 1024;

/// What `--print-manifest` emits, so klava can show what this extension accepts
/// and how it behaves when it fails.
///
/// Declared entities are the default policy only; the crate recognises more
/// (see its README). Listing all 21 here would bury the useful keys.
const MANIFEST: &str = r#"# Written by klava from `klava-pii-guardrail --print-manifest`.

# A PII filter that stops filtering without saying so is worse than none:
# fail closed so a crash blocks the request instead of leaking it.
fail_closed = true

notice = """
This extension rewrites the content of your requests before they leave the machine. \
Any text it detects as personal data is replaced according to `operators`. Keep it \
enabled: a failure is logged, not silently ignored.
"""

# What this extension accepts. klava prints this table in
# `klava extensions show klava-pii-guardrail` and never sends it to the extension.
[settings]
enabled = true
entities = ["Email", "CryptoAddress"]
operators = { Email = "replace:<EMAIL>", CryptoAddress = "replace:<CryptoAddress>" }
"#;

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
    /// Why the request was refused, for `decision: "deny"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

/// What `on_payload` decided for one request.
#[derive(Debug)]
enum Outcome {
    /// Nothing to redact: leave the request exactly as it was.
    Continue,
    /// Send this rewritten payload instead.
    Replace(Value),
    /// Refuse the request outright.
    ///
    /// Used only for payloads too large to analyze. Passing them through would
    /// mean sending unread text from a machine that has this extension
    /// installed precisely to prevent that, and the host would report it as a
    /// normal request; a refusal names the reason instead.
    Deny(String),
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

        for detection in &result.entities {
            seen.insert(detection.entity_type.as_str());
        }

        let count = self.splice_detections(text, &result.entities);
        if count == 0 {
            eprintln!("pii-guardrail: detection offsets were unusable, leaving text as-is");
        }
        count
    }

    /// Apply the configured operator to every detection in one forward pass.
    ///
    /// `detections` must be sorted by start and non-overlapping, which is what
    /// `Analyzer::analyze` guarantees. A span that violates that (bad offset,
    /// overlap, non-char-boundary) is skipped rather than panicking or
    /// silently shifting later replacements.
    ///
    /// This is the linear equivalent of `Anonymizer::anonymize`, whose
    /// back-to-front `replace_range` loop is quadratic in the number of hits.
    fn splice_detections(&self, text: &mut String, detections: &[Detection]) -> usize {
        let mut out = String::with_capacity(text.len());
        let mut cursor = 0usize;
        let mut count = 0usize;

        for detection in detections {
            let (start, end) = (detection.start, detection.end);
            let usable = start >= cursor
                && end > start
                && end <= text.len()
                && text.is_char_boundary(start)
                && text.is_char_boundary(end);
            if !usable {
                continue;
            }

            let operator = self
                .anonymize
                .per_entity
                .get(&detection.entity_type.as_str())
                .unwrap_or(&self.anonymize.default);

            out.push_str(&text[cursor..start]);
            out.push_str(&apply_operator(operator, &text[start..end]));
            cursor = end;
            count += 1;
        }

        if count == 0 {
            return 0;
        }

        out.push_str(&text[cursor..]);
        *text = out;
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

    /// Redact the payload and report what klava should do with it.
    fn on_payload(&self, payload: Value) -> Outcome {
        let mut payload = payload;

        // Mask base64 payloads so a crypto-address-shaped run inside an image
        // cannot be rewritten.
        let originals = mask_encoded_payloads(&mut payload);

        // Bail out before analysis on oversized dumps: see MAX_ANALYZE_BYTES.
        // Masked binaries no longer count, so a chat with an image cannot trip
        // the limit on the image alone.
        let text_bytes = string_bytes(&payload);
        if text_bytes > MAX_ANALYZE_BYTES {
            return Outcome::Deny(format!(
                "request carries {text_bytes} bytes of text, over this extension's \
                 {MAX_ANALYZE_BYTES}-byte analysis limit; it cannot be checked for \
                 personal data within klava's deadline. Trim the request, or raise \
                 the limit by rebuilding klava-pii-guardrail"
            ));
        }

        let mut hits = 0usize;
        let mut seen = HashSet::new();
        self.redact_value(&mut payload, &mut hits, &mut seen);

        restore_encoded_payloads(&mut payload, &originals);

        if hits == 0 {
            return Outcome::Continue;
        }
        eprintln!(
            "pii-guardrail: redacted {hits} span(s), entities: {:?}",
            seen
        );
        Outcome::Replace(payload)
    }
}

/// Total bytes of every string value in `value`.
///
/// Strings are what the analyzer sees; keys and non-text scalars are tiny by
/// comparison, so counting values alone is the honest measure of the work a
/// payload causes.
fn string_bytes(value: &Value) -> usize {
    match value {
        Value::String(s) => s.len(),
        Value::Object(map) => map.values().map(string_bytes).sum(),
        Value::Array(items) => items.iter().map(string_bytes).sum(),
        _ => 0,
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

/// Apply one operator to one span, byte-for-byte as `pii::anonymize` does.
///
/// The crate keeps these helpers private, so they are mirrored here; the
/// `operator_matches_the_crate` test pins the equivalence.
fn apply_operator(op: &Operator, span: &str) -> String {
    match op {
        Operator::Redact => "<REDACTED>".to_string(),
        Operator::Mask { ch, from_end } => mask(span, *ch, *from_end),
        Operator::Replace { with } => with.clone(),
        Operator::HashSha256 { salt } => hash(span, salt),
    }
}

/// Mask a string, leaving the last `from_end` characters unmasked.
fn mask(value: &str, ch: char, from_end: usize) -> String {
    let chars: Vec<char> = value.chars().collect();
    let keep = from_end.min(chars.len());
    let mask_len = chars.len().saturating_sub(keep);
    let mut result = String::new();
    result.extend(std::iter::repeat_n(ch, mask_len));
    result.extend(chars[chars.len() - keep..].iter());
    result
}

/// Hash a span with SHA-256 and a caller-provided salt.
fn hash(value: &str, salt: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(salt.as_bytes());
    hasher.update(value.as_bytes());
    hex::encode(hasher.finalize())
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
        Outcome::Replace(payload) => emit(HostResponse {
            decision: "replace",
            payload: Some(payload),
            reason: None,
        }),
        Outcome::Continue => emit_continue(),
        Outcome::Deny(reason) => {
            eprintln!("pii-guardrail: denying request: {reason}");
            emit(HostResponse {
                decision: "deny",
                payload: None,
                reason: Some(reason),
            });
        }
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
        reason: None,
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

    #[test]
    fn the_manifest_declares_the_real_default_policy() {
        // `klava extensions show` prints this table as the documentation, so a
        // stale default here would lie to the user.
        let value: toml::Value = toml::from_str(MANIFEST).expect("manifest must parse");
        let declared: GuardrailConfig = value["settings"]
            .clone()
            .try_into()
            .expect("settings table");
        let real = GuardrailConfig::default_policy();
        assert_eq!(declared.enabled, real.enabled);
        assert_eq!(declared.entities, real.entities);
        assert_eq!(declared.operators, real.operators);
    }

    #[test]
    fn the_manifest_fails_closed_and_carries_a_notice() {
        let value: toml::Value = toml::from_str(MANIFEST).expect("manifest must parse");
        assert_eq!(value["fail_closed"].as_bool(), Some(true));
        let notice = value["notice"].as_str().expect("notice");
        assert!(notice.contains("personal data"), "got: {notice}");
    }

    fn run(payload: Value, cfg: Option<GuardrailConfig>) -> Option<Value> {
        match PiiGuardrail::new(cfg).on_payload(ctx_of(payload)) {
            Outcome::Replace(payload) => Some(payload),
            Outcome::Continue => None,
            Outcome::Deny(reason) => panic!("unexpected deny: {reason}"),
        }
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

    /// The regression. `Anonymizer::anonymize` rewrites spans back-to-front with
    /// `String::replace_range`, so N detections in an L-byte string cost
    /// O(N × L); a 16 MB tool dump with 400k matches needed ~57 s, past the
    /// host's 20 s budget, which rejected the request (fail-closed). The
    /// forward splice is one pass.
    ///
    /// The size is chosen to discriminate, not to be big: 400k spans over
    /// 3.6 MB is ~15 s through the crate and ~0.15 s through the splice, so the
    /// budget below cannot be met by the old code even on a fast machine, and
    /// the new code misses it by more than an order of magnitude.
    #[test]
    fn many_detections_splice_in_one_pass() {
        let unit = "<EMAIL> ";
        let spans = 400_000;
        let mut text = unit.repeat(spans);
        let detections: Vec<Detection> = (0..spans)
            .map(|i| Detection {
                entity_type: EntityType::Email,
                start: i * unit.len(),
                end: i * unit.len() + "<EMAIL>".len(),
                score: 1.0,
                recognizer: "test".to_string(),
                explanation: pii::types::DetectionExplanation::Regex {
                    pattern_name: "test".to_string(),
                },
            })
            .collect();

        let g = PiiGuardrail::new(None);
        let started = std::time::Instant::now();
        let hits = g.splice_detections(&mut text, &detections);
        let elapsed = started.elapsed();

        assert_eq!(hits, spans, "every span must be replaced");
        assert_eq!(
            text.matches("<EMAIL>").count(),
            spans,
            "the operator writes the same placeholder it matched, so the text \
             must come out unchanged"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "replacing {spans} spans took {elapsed:?}: the splice has regressed \
             to quadratic (the old path needs ~15 s for this input)"
        );
    }

    #[test]
    fn redact_string_reports_what_it_changed() {
        // The hit count and `seen` set drive the host-facing log line, so they
        // must keep counting real replacements after the splice replaced
        // `Anonymizer::anonymize`'s return value.
        let mut text = String::from("write a@example.com or b@example.com");
        let mut seen = HashSet::new();
        let hits = PiiGuardrail::new(None).redact_string(&mut text, &mut seen);

        assert_eq!(text, "write <EMAIL> or <EMAIL>");
        assert_eq!(hits, 2);
        assert!(seen.contains("Email"), "got {seen:?}");
    }

    #[test]
    fn splice_skips_overlapping_and_out_of_range_spans() {
        // Detections the analyzer would never emit, but a future recognizer
        // might: a bad one must not panic, corrupt the text, or swallow the
        // valid spans around it.
        let g = PiiGuardrail::new(None);
        let mut text = String::from("alpha beta gamma");
        let span = |start, end| Detection {
            entity_type: EntityType::Email,
            start,
            end,
            score: 1.0,
            recognizer: "test".to_string(),
            explanation: pii::types::DetectionExplanation::Regex {
                pattern_name: "test".to_string(),
            },
        };
        let detections = vec![
            span(0, 5),     // "alpha"
            span(3, 7),     // overlaps the previous span
            span(6, 10),    // "beta"
            span(100, 200), // past the end
            span(11, 11),   // empty
        ];
        let hits = g.splice_detections(&mut text, &detections);
        assert_eq!(hits, 2);
        // The default policy replaces Email spans with `<EMAIL>`.
        assert_eq!(text, "<EMAIL> <EMAIL> gamma");
    }

    #[test]
    fn operator_matches_the_crate() {
        // `pii` keeps `apply_operator` private, so the local copy has to be
        // pinned: an upgrade that changes masking or hashing must fail here
        // rather than silently diverge.
        let span = "4111111111111111";
        let cfg = AnonymizeConfig::default();
        let detections = [Detection {
            entity_type: EntityType::CreditCard,
            start: 0,
            end: span.len(),
            score: 1.0,
            recognizer: "test".to_string(),
            explanation: pii::types::DetectionExplanation::Regex {
                pattern_name: "test".to_string(),
            },
        }];
        for operator in [
            Operator::Redact,
            Operator::Mask {
                ch: '*',
                from_end: 4,
            },
            Operator::Replace {
                with: "<CARD>".to_string(),
            },
            Operator::HashSha256 {
                salt: "pepper".to_string(),
            },
        ] {
            let local = apply_operator(&operator, span);
            let mut crate_cfg = cfg.clone();
            crate_cfg.default = operator;
            let from_crate =
                pii::Anonymizer::anonymize(span, &detections, &crate_cfg).expect("anonymize");
            assert_eq!(local, from_crate.text);
        }
    }

    #[test]
    fn oversized_payload_is_refused() {
        // A dump over the cap must be refused, not forwarded: this extension is
        // the thing standing between the prompt and the network, so passing a
        // multi-megabyte tool dump through unanalyzed would be the leak it
        // exists to prevent. The host reports the reason.
        let content = "<EMAIL> ".repeat(MAX_ANALYZE_BYTES / 8 + 1);
        let payload = json!({"model": "m", "messages": [{"role": "user", "content": content}]});
        match PiiGuardrail::new(None).on_payload(payload) {
            Outcome::Deny(reason) => {
                assert!(reason.contains("analysis limit"), "got: {reason}");
            }
            other => panic!("an oversized payload must be refused, got {other:?}"),
        }
    }

    #[test]
    fn base64_images_do_not_count_against_the_limit() {
        // Masked binaries are never analyzed, so they must not consume the text
        // budget: a chat carrying one large screenshot stays checkable.
        let data = "A".repeat(MAX_ANALYZE_BYTES + 1024);
        let payload = json!({
            "model": "m",
            "messages": [{"role": "user", "content": "mail user@example.com now",
                          "images": [{"type": "base64", "data": data}]}]
        });
        let out = run(payload, None).expect("the text is small and redactable");
        assert_eq!(out["messages"][0]["content"], "mail <EMAIL> now");
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
        assert!(
            matches!(g.on_payload(payload), Outcome::Replace(_)),
            "Luhn card must be caught"
        );
    }

    #[test]
    fn unknown_entity_name_is_ignored_not_fatal() {
        let g = PiiGuardrail::new(Some(GuardrailConfig {
            enabled: true,
            entities: vec!["NotAThing".to_string()],
            operators: BTreeMap::new(),
        }));
        let payload = json!({"model": "m", "messages": []});
        assert!(matches!(g.on_payload(payload), Outcome::Continue));
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
            reason: None,
        })
        .unwrap();
        assert!(!text.contains("payload"));
    }
}
