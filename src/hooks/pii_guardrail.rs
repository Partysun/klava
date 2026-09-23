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

pub fn pii_guardrail_hook(data: Value, _config: &Config) -> Result<Value> {
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
    let redacted_json: Value = serde_json::from_str(&redacted.text)?;
    Ok(redacted_json)
}
