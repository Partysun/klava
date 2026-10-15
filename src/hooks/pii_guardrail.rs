use crate::config::Config;
use crate::error::Result;
use pii::anonymize::{AnonymizeConfig, Anonymizer};
use pii::nlp::SimpleNlpEngine;
use pii::presets::default_recognizers;
use pii::types::Language;
use pii::{Analyzer, EntityType, PolicyConfig};
use serde_json::Value;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::OnceLock;

/// Build the PII analyzer once per process and reuse it across requests.
///
/// `Analyzer::new` allocates the recognizer set and NLP engine, which is
/// wasteful on the hot path (one request per agent round-trip). The analyzer
/// is safe for concurrent use: recognizers are `Send + Sync` and `analyze()`
/// is a read-only operation.
fn pii_analyzer() -> &'static Analyzer {
    static CELL: OnceLock<Analyzer> = OnceLock::new();
    CELL.get_or_init(|| {
        let mut policy = PolicyConfig::default();
        policy.enabled_entities.insert(EntityType::Email);
        policy.enabled_entities.insert(EntityType::CryptoAddress);

        Analyzer::new(
            Box::new(SimpleNlpEngine::default()),
            default_recognizers(),
            Vec::new(),
            policy,
        )
    })
}

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
/// The request is serialized to a single JSON string before PII analysis, so a
/// recognizer can otherwise match — and thereby corrupt — a run inside an
/// image's base64 data (e.g. a 40-hex-char `0x…` sequence that looks like a
/// crypto address), making the data URL invalid and the provider reject the
/// request. Masking only touches genuine encoded fields: string values that
/// *are* a base64 data URL and Anthropic `{"type":"base64","data":…}` image
/// sources. Secrets in code (including a `;base64,`-looking literal) are left
/// for the anonymizer. The pairs returned restore the payloads afterwards.
fn mask_encoded_payloads(value: &mut Value) -> Vec<(String, String)> {
    let mut originals = Vec::new();
    let mut counter = 0usize;
    mask_value(value, &mut originals, &mut counter);
    originals
}

fn mask_value(value: &mut Value, originals: &mut Vec<(String, String)>, counter: &mut usize) {
    match value {
        Value::Object(map) => {
            // Anthropic image source: {"type":"base64","media_type":…,"data":…}
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

pub fn pii_guardrail_hook(mut data: Value, _config: &Config) -> Result<Value> {
    // Mask inline image/file payloads before analysis so redaction can never
    // corrupt them; everything else (including secrets in source code) is still
    // scanned. See `mask_encoded_payloads`.
    let originals = mask_encoded_payloads(&mut data);
    let data = serde_json::to_string(&data)?;

    let result = pii_analyzer()
        .analyze(&data, &Language::from("en"))
        .unwrap();
    for detection in &result.entities {
        let span = &data[detection.start..detection.end];
        tracing::debug!(
            "type={} start={} end={} value={}",
            detection.entity_type.as_str(),
            detection.start,
            detection.end,
            span
        );
    }
    // println!("{:?}", result);
    let unique_entity_types: HashSet<pii::EntityType> = result
        .entities
        .iter()
        .map(|entity| entity.entity_type.clone())
        .collect();

    tracing::info!("Filtered unique entity types: {:?}", unique_entity_types);

    let mut config = AnonymizeConfig::default();
    let mut per_entity = HashMap::new();
    per_entity.insert(
        "Email".to_string(),
        pii::anonymize::Operator::Replace {
            with: "<EMAIL>".into(),
        },
    );
    per_entity.insert(
        "CryptoAddress".to_string(),
        pii::anonymize::Operator::Replace {
            with: "<CryptoAddress>".into(),
        },
    );
    config.per_entity = per_entity;
    let redacted = Anonymizer::anonymize(&data, &result.entities, &config).unwrap();
    tracing::trace!("{:?}", redacted.items);
    let mut redacted_json: Value = serde_json::from_str(&redacted.text)?;
    restore_encoded_payloads(&mut redacted_json, &originals);
    Ok(redacted_json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run(content: &str) -> String {
        let data = json!({
            "model": "m",
            "messages": [{"role": "user", "content": content}]
        });
        let out = pii_guardrail_hook(data, &Config::default()).unwrap();
        out["messages"][0]["content"].as_str().unwrap().to_string()
    }

    #[test]
    fn redacts_email_addresses() {
        let out = run("reach me at test@example.com please");
        assert!(out.contains("<EMAIL>"), "got: {out}");
        assert!(!out.contains("test@example.com"), "got: {out}");
    }

    #[test]
    fn redacts_ethereum_addresses() {
        let out = run("send to 0x1234567890abcdef1234567890abcdef12345678 now");
        assert!(out.contains("<CryptoAddress>"), "got: {out}");
        assert!(
            !out.contains("0x1234567890abcdef1234567890abcdef12345678"),
            "got: {out}"
        );
    }

    #[test]
    fn leaves_base64_image_payloads_untouched() {
        // A `/` boundary followed by an Ethereum-address-shaped run: valid
        // base64 that must not be redacted, or the data URL becomes invalid.
        let payload = "AAAA/0x1234567890abcdef1234567890abcdef12345678";
        let url = format!("data:image/png;base64,{payload}");
        let out = run(&url);
        assert_eq!(out, url, "data URL payload must be left untouched");
    }

    #[test]
    fn still_redacts_text_around_a_data_url() {
        let payload = "AAAA/0x1234567890abcdef1234567890abcdef12345678";
        let url = format!("data:image/png;base64,{payload}");
        let out = run(&format!("{url} mail me at test@example.com"));
        assert!(out.contains(&url), "payload must survive: {out}");
        assert!(
            out.contains("<EMAIL>"),
            "text email must be redacted: {out}"
        );
    }

    #[test]
    fn redacts_crypto_address_in_code_next_to_a_base64_literal() {
        // Not a data URL value, so it must still be redacted even though a
        // `;base64,` marker directly precedes the address.
        let out = run(r#"let k = ";base64,0x1234567890abcdef1234567890abcdef12345678";"#);
        assert!(out.contains("<CryptoAddress>"), "got: {out}");
        assert!(
            !out.contains("0x1234567890abcdef1234567890abcdef12345678"),
            "got: {out}"
        );
    }

    #[test]
    fn leaves_anthropic_image_source_untouched() {
        let payload = "AAAA/0x1234567890abcdef1234567890abcdef12345678";
        let data = json!({
            "model": "m",
            "messages": [{"role": "user", "content": [
                {"type": "image", "source": {
                    "type": "base64", "media_type": "image/png", "data": payload
                }}
            ]}]
        });

        let out = pii_guardrail_hook(data, &Config::default()).unwrap();
        assert_eq!(out["messages"][0]["content"][0]["source"]["data"], payload);
    }

    #[test]
    fn leaves_responses_input_image_untouched() {
        let payload = "AAAA/0x1234567890abcdef1234567890abcdef12345678";
        let url = format!("data:image/png;base64,{payload}");
        let data = json!({
            "model": "m",
            "input": [{"role": "user", "content": [
                {"type": "input_image", "image_url": url}
            ]}]
        });

        let out = pii_guardrail_hook(data, &Config::default()).unwrap();
        assert_eq!(out["input"][0]["content"][0]["image_url"], url);
    }

    #[test]
    fn leaves_client_supplied_data_urls_untouched() {
        let b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFBQIAX8jx0gAAAABJRU5ErkJggg==";
        let url = format!("data:image/png;base64,{b64}");
        let data = json!({
            "model": "m",
            "messages": [{"role": "user", "content": [
                {"type": "image_url", "image_url": {"url": url}}
            ]}]
        });

        let out = pii_guardrail_hook(data, &Config::default()).unwrap();
        assert_eq!(out["messages"][0]["content"][0]["image_url"]["url"], url);
    }

    #[test]
    fn leaves_plain_text_untouched() {
        let out = run("just a normal instruction to refactor the parser");
        assert!(out.contains("refactor the parser"), "got: {out}");
        assert!(!out.contains("<EMAIL>"), "got: {out}");
        assert!(!out.contains("<CryptoAddress>"), "got: {out}");
    }
}
