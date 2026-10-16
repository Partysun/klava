//! Decision model: tuning search settings with a System One classifier.
//!
//! The decision model (`jigor`, model `search_decide_model`) answers a small
//! set of bounded questions about the query — how fresh, how broad, how much
//! context — and this module maps the typed answers onto [`SearchSettings`].
//!
//! The classifier cannot rewrite the query or invent domains; it only picks
//! among the option sets klava supplies. Everything here is **fail-open**: any
//! error (no key, network, timeout, unparseable answer) returns the base
//! settings unchanged, so a search still runs.
//!
//! `jigor` is blocking (`ureq`), so the call runs under
//! [`tokio::task::spawn_blocking`] with a short timeout.

use crate::config::Config;
use crate::search::settings::{Breadth, ContextSize, Engine, SearchSettings};
use jigor::{Answer, KlavaBackend};
use std::collections::HashMap;

/// How long the decision model may take before klava falls back to defaults.
const DECIDE_TIMEOUT_SECS: u32 = 8;

/// Question id: needs very recent information?
const Q_FRESHNESS: &str = "freshness";
/// Question id: how much page context?
const Q_CONTEXT: &str = "context";
/// Question id: how broad should the retrieval be?
const Q_BREADTH: &str = "breadth";
/// Question id: which search family fits?
const Q_ENGINE: &str = "engine_family";

/// Klava API key for the decision call. The gateway key is the same one the
/// `klava` provider uses: the env var named by the provider's `api_key_name`
/// (e.g. `KLAVA_API_KEY`), the provider's `api_key` field, or the saved
/// `klava login` credentials.
fn api_key(config: &Config) -> Option<String> {
    config
        .resolve_api_key()
        .or_else(|| std::env::var("KLAVA_API_KEY").ok())
        .or_else(|| {
            #[cfg(feature = "klava")]
            {
                let mut auth = crate::klava_auth::KlavaAuth::new();
                auth.get_api_key().ok()
            }
            #[cfg(not(feature = "klava"))]
            {
                None
            }
        })
        .filter(|k| !k.is_empty())
}

/// Decide and apply tuning for `query`, starting from `base`.
///
/// Returns `base` unchanged on any failure. Never panics, never blocks the
/// async runtime (the blocking call runs on a worker thread).
pub async fn tune(config: &Config, query: &str, base: SearchSettings) -> SearchSettings {
    if query.trim().is_empty() {
        return base;
    }

    let model = config.search_decide_model.clone();
    let Some(key) = api_key(config) else {
        tracing::debug!("web search: no klava key available; skipping decision model");
        return base;
    };
    let state = decision_state(query);

    let answers = tokio::task::spawn_blocking(move || run_decision(&model, &key, &state)).await;
    match answers {
        Ok(Some(answers)) => apply(base, &answers),
        Ok(None) => base,
        Err(e) => {
            tracing::debug!("web search: decision task failed: {e}");
            base
        }
    }
}

/// The JSON state the decision model judges: the query plus light framing.
fn decision_state(query: &str) -> serde_json::Value {
    serde_json::json!({
        "task": "Choose web-search settings for the agent's search query.",
        "query": query,
    })
}

/// Run the blocking decision call. Returns typed answers, or `None` on any
/// failure (fail-open).
fn run_decision(
    model: &str,
    key: &str,
    state: &serde_json::Value,
) -> Option<HashMap<String, Answer>> {
    let mut backend = KlavaBackend::for_model(model);
    // Prefer the resolved key (env or config) over jigor's own env lookup.
    backend.api_key = key.to_string();
    backend.timeout_secs = DECIDE_TIMEOUT_SECS;

    let questions = questions();
    match backend.ask(state, &questions, None) {
        Ok(asks) => {
            tracing::debug!(
                "web search: decision model {} answered {} question(s)",
                asks.model,
                asks.answers.len()
            );
            Some(asks.answers)
        }
        Err(e) => {
            tracing::debug!("web search: decision model call failed: {e}");
            None
        }
    }
}

/// The bounded question set offered to the classifier.
fn questions() -> Vec<jigor::Question> {
    vec![
        jigor::noul(
            Q_FRESHNESS,
            "Does answering this query require information from the last few days or weeks \
             (news, releases, prices, current events)?",
        ),
        jigor::choice(
            Q_CONTEXT,
            "How much page content should be retrieved to answer this query well?",
            &["low", "medium", "high"],
        ),
        jigor::score(
            Q_BREADTH,
            "How broad should the web retrieval be?",
            &["focused", "standard", "broad"],
        ),
        jigor::choice(
            Q_ENGINE,
            "Which retrieval style fits this query best?",
            &["web_fast", "web_deep", "auto"],
        ),
    ]
}

/// Map typed answers onto `base`, leaving any knob the model didn't answer.
fn apply(mut base: SearchSettings, answers: &HashMap<String, Answer>) -> SearchSettings {
    let fresh = matches!(
        answers.get(Q_FRESHNESS),
        Some(Answer::Noul { probability }) if *probability >= 0.5
    );

    if let Some(Answer::Choice { choice, .. }) = answers.get(Q_CONTEXT) {
        base.context = ContextSize::parse(choice);
    }

    if let Some(Answer::Score { score, .. }) = answers.get(Q_BREADTH) {
        base.max_results = score_to_breadth(*score).max_results();
    }

    if let Some(Answer::Choice { choice, .. }) = answers.get(Q_ENGINE) {
        base.engine = Engine::parse(choice);
    }

    // Freshness biases breadth up when the model was neutral on it.
    if fresh && !answers.contains_key(Q_BREADTH) {
        base.max_results = Breadth::Broad.max_results();
    }

    base.clamped()
}

/// Interpret a `score` answer (a 0-based level index into the three-level
/// scale) as one of three breadth levels.
fn score_to_breadth(score: f32) -> Breadth {
    match score.round() as i32 {
        i32::MIN..=0 => Breadth::Focused,
        1 => Breadth::Standard,
        _ => Breadth::Broad,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answers(pairs: Vec<(&str, Answer)>) -> HashMap<String, Answer> {
        pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
    }

    #[test]
    fn apply_maps_every_knob() {
        let a = answers(vec![
            (
                Q_CONTEXT,
                Answer::Choice {
                    choice: "high".to_string(),
                    confidence: 0.9,
                    probabilities: HashMap::new(),
                },
            ),
            (
                Q_BREADTH,
                Answer::Score {
                    score: 2.0,
                    confidence: 0.9,
                    probabilities: HashMap::new(),
                    legend: HashMap::new(),
                },
            ),
            (
                Q_ENGINE,
                Answer::Choice {
                    choice: "web_deep".to_string(),
                    confidence: 0.9,
                    probabilities: HashMap::new(),
                },
            ),
        ]);

        let out = apply(SearchSettings::default(), &a);
        assert_eq!(out.context, ContextSize::High);
        assert_eq!(out.max_results, Breadth::Broad.max_results());
        assert_eq!(out.engine, Engine::WebDeep);
    }

    #[test]
    fn apply_fail_open_on_empty_answers() {
        let out = apply(SearchSettings::default(), &HashMap::new());
        assert_eq!(out, SearchSettings::default());
    }

    #[test]
    fn apply_freshness_biases_breadth_when_unset() {
        let a = answers(vec![(Q_FRESHNESS, Answer::Noul { probability: 0.9 })]);
        let out = apply(SearchSettings::default(), &a);
        assert_eq!(out.max_results, Breadth::Broad.max_results());
    }

    #[test]
    fn apply_freshness_not_yes_stays_default() {
        let a = answers(vec![(Q_FRESHNESS, Answer::Noul { probability: 0.1 })]);
        let out = apply(SearchSettings::default(), &a);
        assert_eq!(out, SearchSettings::default());
    }

    #[test]
    fn score_to_breadth_uses_zero_based_level_index() {
        // jigor `score` answers are a 0-based index into the supplied levels.
        assert_eq!(score_to_breadth(0.0), Breadth::Focused);
        assert_eq!(score_to_breadth(1.0), Breadth::Standard);
        assert_eq!(score_to_breadth(2.0), Breadth::Broad);
        // Rounded, so 1.99 -> 2.
        assert_eq!(score_to_breadth(1.99), Breadth::Broad);
        // Out-of-range values clamp to the ends.
        assert_eq!(score_to_breadth(-3.0), Breadth::Focused);
        assert_eq!(score_to_breadth(9.0), Breadth::Broad);
    }

    #[test]
    fn question_set_is_bounded_and_typed() {
        let qs = questions();
        assert_eq!(qs.len(), 4);
        let kinds: Vec<&str> = qs.iter().map(|q| q.kind.as_str()).collect();
        assert!(kinds.contains(&"noul"));
        assert!(kinds.contains(&"choice"));
        assert!(kinds.contains(&"score"));
    }

    #[test]
    fn decision_state_carries_query() {
        let s = decision_state("rust 1.90 release date");
        assert_eq!(s["query"], "rust 1.90 release date");
    }
}
