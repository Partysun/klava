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

pub fn pii_guardrail_hook(data: Value, _config: &Config) -> Result<Value> {
    let data = serde_json::to_string(&data)?;

    let recognisers = default_recognizers();
    let mut policy = PolicyConfig::default();
    policy.enabled_entities.insert(EntityType::Email);
    policy.enabled_entities.insert(EntityType::CryptoAddress);

    let analyzer = Analyzer::new(
        Box::new(SimpleNlpEngine::default()),
        recognisers,
        Vec::new(),
        policy,
    );
    let result = analyzer.analyze(&data, &Language::from("en")).unwrap();
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
