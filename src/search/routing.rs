//! Explicit search-engine intent.
//!
//! Two things select Yandex, and nothing else does:
//!
//! 1. the user asks for it by name ("search this on Yandex", "найди в
//!    яндексе"), read from the user's own request as well as the search query
//!    the model produced;
//! 2. the query is specifically about Russia or the CIS (the decision model's
//!    `russia_region` question).
//!
//! The script a query is written in is deliberately **not** a signal: a
//! Russian-language question about a global topic is not a reason to use
//! Yandex.
//!
//! Only `auto` is affected: an explicit `search_engine` stays authoritative,
//! and the decision model cannot override the engine selected here.

use crate::search::settings::Engine;

/// Spellings that name the Yandex provider. The Cyrillic word is matched as a
/// substring so all grammatical forms (`яндекс`, `яндекса`, `яндексе`, ...)
/// match; this is matching the provider *name*, not using the script as a
/// signal.
const YANDEX_MARKERS: &[&str] = &["yandex", "яндекс"];

/// Phrases that explicitly decline Yandex, so "don't use Yandex, use Exa"
/// is not read as a request for Yandex.
const YANDEX_NEGATIONS: &[&str] = &[
    "не яндекс",
    "не используй яндекс",
    "не ищи в яндексе",
    "без яндекса",
    "not yandex",
    "without yandex",
    "no yandex",
    "don't use yandex",
    "dont use yandex",
    "do not use yandex",
];

/// The engine explicitly requested by the user, if any.
///
/// `query` is the search query the model produced; `context` is the user's
/// own wording. Either may carry the request ("search this on Yandex"), and
/// the query wins only because it is the closest to the search itself.
///
/// Currently only Yandex is recognized; other providers stay selected through
/// config and the decision model.
pub fn explicit_engine_in(query: &str, context: &[&str]) -> Option<Engine> {
    let mut requested = None;
    for text in std::iter::once(query).chain(context.iter().copied()) {
        if is_negated(text) {
            // A "do not use Yandex" anywhere is a hard veto.
            return None;
        }
        if requested.is_none() && has_marker(text) {
            requested = Some(Engine::Yandex);
        }
    }
    requested
}

/// The engine explicitly requested by `text`, if any.
pub fn explicit_engine(text: &str) -> Option<Engine> {
    if is_negated(text) {
        None
    } else if has_marker(text) {
        Some(Engine::Yandex)
    } else {
        None
    }
}

/// Whether the user explicitly declined Yandex anywhere in this turn. A veto
/// suppresses automatic selection too, not just a named request.
pub fn vetoed(query: &str, context: &[&str]) -> bool {
    std::iter::once(query)
        .chain(context.iter().copied())
        .any(is_negated)
}

fn is_negated(text: &str) -> bool {
    let lower = text.to_lowercase();
    YANDEX_NEGATIONS.iter().any(|phrase| lower.contains(phrase))
}

fn has_marker(text: &str) -> bool {
    let lower = text.to_lowercase();
    YANDEX_MARKERS.iter().any(|marker| lower.contains(marker))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_yandex_in_latin_and_cyrillic() {
        assert_eq!(
            explicit_engine("please search this on yandex"),
            Some(Engine::Yandex)
        );
        assert_eq!(
            explicit_engine("YANDEX: latest rust release"),
            Some(Engine::Yandex)
        );
        assert_eq!(
            explicit_engine("найди в яндексе последний релиз rust"),
            Some(Engine::Yandex)
        );
        assert_eq!(
            explicit_engine("site:yandex.ru новости"),
            Some(Engine::Yandex)
        );
    }

    #[test]
    fn ignores_queries_without_provider_names() {
        assert_eq!(explicit_engine("latest rust release"), None);
        assert_eq!(explicit_engine("google and bing comparison"), None);
        assert_eq!(explicit_engine(""), None);
    }

    #[test]
    fn ignores_negated_yandex_requests() {
        assert_eq!(explicit_engine("don't use yandex, use exa"), None);
        assert_eq!(explicit_engine("не используй яндекс, ищи в exa"), None);
        assert_eq!(explicit_engine("search without yandex"), None);
    }

    #[test]
    fn context_wording_is_consulted() {
        assert_eq!(
            explicit_engine_in("latest rust release", &["use yandex for this"]),
            Some(Engine::Yandex)
        );
        assert_eq!(
            explicit_engine_in("latest rust release", &["search the web"]),
            None
        );
        // A negation anywhere is a hard veto.
        assert_eq!(
            explicit_engine_in("use yandex", &["actually, don't use yandex"]),
            None
        );
        assert_eq!(
            explicit_engine_in("don't use yandex", &["use yandex"]),
            None
        );
    }

    #[test]
    fn veto_is_detected_across_query_and_context() {
        assert!(vetoed("not yandex: rust", &[]));
        assert!(vetoed("rust", &["don't use yandex"]));
        assert!(!vetoed("rust", &["use yandex"]));
        assert!(!vetoed("rust", &[]));
    }

    #[test]
    fn negation_only_applies_to_the_named_provider() {
        // A question that mentions the provider without asking to use it is
        // still treated as intent to use it (the model would phrase a
        // preference differently).
        assert_eq!(
            explicit_engine("compare yandex and google results"),
            Some(Engine::Yandex)
        );
    }
}
