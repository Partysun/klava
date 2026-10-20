//! The decide model (System One / jigor) as a host capability.
//!
//! klava owns the classifier call: the model, the credentials, the timeout and
//! the cost accounting. Extensions own the *question* and the *state*, and ask
//! for a judgment over the extension protocol (`decision: "decide"`). That
//! way a third-party extension can use the decide model without linking klava,
//! holding a key, or knowing which backend serves the model.
//!
//! Every call is bounded: the extension supplies typed questions, never a raw
//! prompt, so the classifier cannot be asked to do arbitrary work through this
//! path.

use crate::config::Config;
#[cfg(feature = "decide")]
use crate::ext_api::DecideQuestion;
use crate::ext_api::{DecideRequest, DecideResult};
#[cfg(feature = "decide")]
use std::collections::HashMap;

/// Model used when the config leaves `decide_model` empty.
pub const DEFAULT_DECIDE_MODEL: &str = "typesafe/jev-1.13";

/// How long a decision call may take before it is abandoned.
pub const DECIDE_TIMEOUT_SECS: u32 = 8;

/// Upper bound on questions per call, so an extension cannot turn one request
/// into an unbounded classifier bill.
pub const MAX_QUESTIONS: usize = 16;

/// Key for a decision call.
///
/// Comes from the `decide_model` entry (its `api_key`, or the env var named by
/// `api_key_name`), then `KLAVA_API_KEY`, then the saved `klava login`
/// credentials. The backend itself is detected from the entry.
#[cfg(feature = "decide")]
pub(crate) fn api_key(config: &Config) -> Option<String> {
    let from_entry = config
        .decide_entry()
        .ok()
        .flatten()
        .and_then(|entry| entry.api_key());
    from_entry
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

/// The jigor gateway that answers `decide` calls for a model.
#[cfg(feature = "decide")]
#[derive(Debug)]
enum Backend {
    Klava(jigor::KlavaBackend),
    OpenRouter(jigor::OpenRouterBackend),
    Cloudflare(jigor::CloudflareBackend),
}

/// Turn a chat-completions base URL into the matching decisions endpoint.
///
/// jigor wants the full decisions URL; a `[[models]]` entry only names the
/// base its chat requests go to, so the path is appended here. An explicit
/// `…/decisions` URL is left untouched.
#[cfg(feature = "decide")]
fn decisions_url(base: &str, backend: crate::providers::JigorBackend) -> String {
    use crate::providers::JigorBackend;

    let base = base.trim_end_matches('/');
    if base.ends_with("/decisions") {
        return base.to_string();
    }
    match backend {
        JigorBackend::Openrouter => format!("{base}/alpha/decisions"),
        // Klava and self-hosted klava-wire gateways.
        JigorBackend::Klava | JigorBackend::Cloudflare => {
            if base.contains("/client/v4/accounts/") {
                // Cloudflare is handled by the caller (it needs the account id).
                base.to_string()
            } else if base.ends_with("/v1") {
                format!("{base}/alpha/decisions")
            } else {
                format!("{base}/v1/alpha/decisions")
            }
        }
    }
}

/// Build the jigor backend for the configured decide model.
///
/// Without a `decide_model` entry the klava gateway is used with the saved
/// login — the zero-config default. With one, its type and base URL select
/// the backend automatically (klava / OpenRouter / Cloudflare Workers AI).
#[cfg(feature = "decide")]
fn build_backend(config: &Config, model: &str) -> Result<Backend, String> {
    use crate::providers::JigorBackend;
    #[cfg(feature = "qwen-code")]
    use crate::providers::Type;

    let entry = config
        .decide_entry()
        .map_err(|e| format!("decide model is ambiguous: {e}"))?;

    let Some(entry) = entry else {
        // No entry: the klava gateway with the saved login.
        let key = api_key(config)
            .ok_or_else(|| "no klava key available for the decide model".to_string())?;
        let mut backend = jigor::KlavaBackend::for_model(model);
        backend.api_key = key;
        backend.timeout_secs = DECIDE_TIMEOUT_SECS;
        return Ok(Backend::Klava(backend));
    };

    #[cfg(feature = "qwen-code")]
    if entry.provider_type == Type::QwenCode {
        return Err(format!(
            "decide model '{}' is a qwen-code model, which cannot answer decisions; \
             point decide_model at a klava, OpenRouter or Cloudflare model",
            entry.name
        ));
    }

    let upstream = entry.upstream_model();
    let backend_kind = entry.jigor_backend().unwrap_or(JigorBackend::Klava);
    let base = entry.base_url.clone();
    let entry_key = entry.api_key();

    match backend_kind {
        JigorBackend::Klava => {
            let mut backend = jigor::KlavaBackend::for_model(&upstream);
            if let Some(base) = base {
                backend.base_url = decisions_url(&base, backend_kind);
            }
            backend.api_key = entry_key
                .or_else(|| api_key(config))
                .ok_or_else(|| format!("no key available for decide model '{}'", entry.name))?;
            backend.timeout_secs = DECIDE_TIMEOUT_SECS;
            Ok(Backend::Klava(backend))
        }
        JigorBackend::Openrouter => {
            let mut backend = jigor::OpenRouterBackend::for_model(&upstream);
            if let Some(base) = base {
                backend.base_url = decisions_url(&base, backend_kind);
            }
            if let Some(key) = entry_key {
                backend.api_key = key;
                backend.api_key_env = entry.api_key_name.clone().unwrap_or_default();
            }
            backend.timeout_secs = DECIDE_TIMEOUT_SECS;
            if backend.api_key.is_empty() {
                return Err(format!(
                    "no OpenRouter key available for decide model '{}'",
                    entry.name
                ));
            }
            Ok(Backend::OpenRouter(backend))
        }
        JigorBackend::Cloudflare => {
            let mut backend = jigor::CloudflareBackend::for_model(&upstream);
            match entry.cloudflare_account_id() {
                Some(account) => {
                    backend.account_id = account.clone();
                    backend.base_url =
                        format!("https://api.cloudflare.com/client/v4/accounts/{account}/ai/run");
                }
                None => {
                    return Err(format!(
                        "decide model '{}' has a Cloudflare-type URL but no \"/accounts/<id>/ai/\" \
                         segment; set base_url to the account's Workers AI endpoint",
                        entry.name
                    ));
                }
            }
            if let Some(key) = entry_key {
                backend.api_token = key;
                backend.token_env = entry.api_key_name.clone().unwrap_or_default();
            }
            backend.timeout_secs = DECIDE_TIMEOUT_SECS;
            if backend.api_token.is_empty() {
                return Err("no Cloudflare API token available for the decide model".to_string());
            }
            Ok(Backend::Cloudflare(backend))
        }
    }
}

/// One blocking classification call over the configured decide model.
///
/// Shared by the extension `decide` capability and the web-search tuner so
/// both always agree on the model, backend and credentials. Callers run this
/// under [`tokio::task::spawn_blocking`] (jigor is blocking).
#[cfg(feature = "decide")]
pub(crate) fn ask_blocking(
    config: &Config,
    model: &str,
    state: &serde_json::Value,
    questions: &[jigor::Question],
) -> Result<jigor::Asks, String> {
    let backend = build_backend(config, model)?;
    let result = match backend {
        Backend::Klava(b) => b.ask(state, questions, None),
        Backend::OpenRouter(b) => b.ask(state, questions, None),
        Backend::Cloudflare(b) => b.ask(state, questions, None),
    };
    result.map_err(|e| e.to_string())
}

/// Validate an extension's decide request before spending on it.
pub fn validate(request: &DecideRequest) -> Result<(), String> {
    if request.questions.is_empty() {
        return Err("decide request has no questions".to_string());
    }
    if request.questions.len() > MAX_QUESTIONS {
        return Err(format!(
            "decide request has {} questions, max is {MAX_QUESTIONS}",
            request.questions.len()
        ));
    }
    for question in &request.questions {
        if question.id.trim().is_empty() {
            return Err("decide question has an empty id".to_string());
        }
        if question.instructions.trim().is_empty() {
            return Err(format!(
                "decide question '{}' has empty instructions",
                question.id
            ));
        }
        match question.kind.as_str() {
            "noul" => {}
            "choice" | "score" => {
                let options = question
                    .criteria
                    .as_ref()
                    .and_then(|c| match c {
                        serde_json::Value::Object(map) => Some(map.len()),
                        serde_json::Value::Array(items) => Some(items.len()),
                        _ => None,
                    })
                    .unwrap_or(0);
                if options == 0 {
                    return Err(format!(
                        "decide question '{}' is a {} but declares no criteria",
                        question.id, question.kind
                    ));
                }
            }
            other => {
                return Err(format!(
                    "decide question '{}' has unknown type '{other}'",
                    question.id
                ));
            }
        }
    }
    if let Some(id) = &request.approve_question
        && !request.questions.iter().any(|q| &q.id == id)
    {
        return Err(format!(
            "approve_question '{id}' is not one of the requested questions"
        ));
    }
    Ok(())
}

/// Which question decides `approve`: the named one, else the first `noul`.
#[cfg(feature = "decide")]
fn decide_question_id(request: &DecideRequest) -> Option<&str> {
    match &request.approve_question {
        Some(id) => Some(id.as_str()),
        None => request
            .questions
            .iter()
            .find(|q| q.kind == "noul")
            .map(|q| q.id.as_str()),
    }
}

/// Reject a decide model that cannot answer, before spending on it.
#[cfg(feature = "decide")]
fn validate_decide_model(config: &Config) -> Result<(), String> {
    build_backend(config, &config.resolve_decide_model()).map(|_| ())
}

/// Run the host's decide model over an extension-supplied request.
///
/// Errors are returned as `Err` (never a silent "yes"), so the host can leave
/// the request unchanged and log why. The extension is only re-run with a
/// result when the call actually succeeded.
#[cfg(feature = "decide")]
pub async fn ask(config: &Config, request: &DecideRequest) -> Result<DecideResult, String> {
    use jigor::{Answer, Question};

    validate(request)?;

    let model = config.resolve_decide_model();
    validate_decide_model(config)?;

    let questions: Vec<Question> = request
        .questions
        .iter()
        .map(question_to_jigor)
        .collect::<Result<_, _>>()?;
    let state = request.state.clone();
    let config = config.clone();
    let model_for_call = model.clone();
    let threshold = request.threshold.clamp(0.0, 1.0);
    let approve_id = decide_question_id(request).map(str::to_string);

    let asks = tokio::task::spawn_blocking(move || {
        ask_blocking(&config, &model_for_call, &state, &questions)
    })
    .await
    .map_err(|e| format!("decide task failed: {e}"))?
    .map_err(|e| format!("decide call failed: {e}"))?;

    let mut answers: HashMap<String, serde_json::Value> = HashMap::new();
    for question in &request.questions {
        let answer = asks
            .answers
            .get(&question.id)
            .ok_or_else(|| format!("decide model returned no answer for '{}'", question.id))?;
        answers.insert(question.id.clone(), jigor::answer_to_json(answer));
    }

    let (approve, probability) = match approve_id {
        Some(id) => match asks.answers.get(&id) {
            Some(Answer::Noul { probability }) => (*probability >= threshold, Some(*probability)),
            Some(other) => {
                return Err(format!(
                    "approve question '{id}' answered with {}, expected noul",
                    other.kind()
                ));
            }
            None => (false, None),
        },
        // No `noul` and no named question: no explicit approval.
        None => (false, None),
    };

    Ok(DecideResult {
        answers,
        approve,
        probability,
        cost_usd: asks.usage.map(|u| u.cost),
        model: asks.model,
    })
}

/// Without the `decide` feature there is no classifier, so every call fails.
#[cfg(not(feature = "decide"))]
pub async fn ask(_config: &Config, _request: &DecideRequest) -> Result<DecideResult, String> {
    Err("built without the 'decide' feature".to_string())
}

#[cfg(feature = "decide")]
fn question_to_jigor(question: &DecideQuestion) -> Result<jigor::Question, String> {
    Ok(match question.kind.as_str() {
        "noul" => jigor::noul(&question.id, &question.instructions),
        "choice" => {
            let options = string_options(question)?;
            let refs: Vec<&str> = options.iter().map(String::as_str).collect();
            jigor::choice(&question.id, &question.instructions, &refs)
        }
        "score" => {
            let levels = string_options(question)?;
            let refs: Vec<&str> = levels.iter().map(String::as_str).collect();
            jigor::score(&question.id, &question.instructions, &refs)
        }
        other => return Err(format!("unknown question type '{other}'")),
    })
}

#[cfg(feature = "decide")]
fn string_options(question: &DecideQuestion) -> Result<Vec<String>, String> {
    let criteria = question
        .criteria
        .as_ref()
        .ok_or_else(|| format!("question '{}' declares no criteria", question.id))?;
    match criteria {
        serde_json::Value::Object(map) => Ok(map.keys().cloned().collect()),
        serde_json::Value::Array(items) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| format!("question '{}' criteria must be strings", question.id))
            })
            .collect(),
        _ => Err(format!(
            "question '{}' criteria must be an object or an array",
            question.id
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ext_api::DecideQuestion;
    use serde_json::json;

    fn request() -> DecideRequest {
        DecideRequest {
            state: json!({"error": "disk full"}),
            questions: vec![DecideQuestion {
                id: "intervene".to_string(),
                kind: "noul".to_string(),
                instructions: "Does this need a human?".to_string(),
                criteria: None,
            }],
            threshold: 0.7,
            approve_question: None,
        }
    }

    #[test]
    fn validate_accepts_a_typed_noul_request() {
        assert!(validate(&request()).is_ok());
    }

    #[test]
    fn validate_rejects_empty_question_set() {
        let mut req = request();
        req.questions.clear();
        assert!(validate(&req).is_err());
    }

    #[test]
    fn validate_rejects_unknown_kinds() {
        let mut req = request();
        req.questions[0].kind = "freeform".to_string();
        assert!(validate(&req).is_err());
    }

    #[test]
    fn validate_rejects_choice_without_options() {
        let mut req = request();
        req.questions[0].kind = "choice".to_string();
        assert!(validate(&req).is_err());
    }

    #[test]
    fn validate_rejects_unknown_approve_question() {
        let mut req = request();
        req.approve_question = Some("missing".to_string());
        assert!(validate(&req).is_err());
    }

    #[cfg(feature = "decide")]
    #[test]
    fn decisions_url_appends_the_backend_path() {
        use crate::providers::JigorBackend;

        assert_eq!(
            decisions_url("https://klava.zatsepin.dev/api", JigorBackend::Klava),
            "https://klava.zatsepin.dev/api/v1/alpha/decisions"
        );
        assert_eq!(
            decisions_url("https://klava.zatsepin.dev/api/v1/", JigorBackend::Klava),
            "https://klava.zatsepin.dev/api/v1/alpha/decisions"
        );
        assert_eq!(
            decisions_url("https://openrouter.ai/api", JigorBackend::Openrouter),
            "https://openrouter.ai/api/alpha/decisions"
        );
        // An explicit endpoint is never rewritten.
        assert_eq!(
            decisions_url("https://gw.example/alpha/decisions", JigorBackend::Klava),
            "https://gw.example/alpha/decisions"
        );
    }

    #[cfg(feature = "decide")]
    fn entry(name: &str, base: Option<&str>) -> crate::providers::Model {
        crate::providers::Model {
            name: name.to_string(),
            base_url: base.map(str::to_string),
            api_key: Some("k".to_string()),
            ..crate::providers::Model::default()
        }
    }

    #[cfg(feature = "decide")]
    #[test]
    fn a_decide_entry_selects_its_backend_from_the_url() {
        use crate::providers::Model;

        let config = Config {
            decide_model: "m".to_string(),
            models: vec![entry("m", Some("https://openrouter.ai/api"))],
            ..Config::default()
        };
        let backend = build_backend(&config, "m").unwrap();
        match backend {
            Backend::OpenRouter(b) => assert_eq!(b.api_key, "k"),
            _ => panic!("expected the OpenRouter backend"),
        }

        let config = Config {
            decide_model: "m".to_string(),
            models: vec![entry(
                "m",
                Some("https://api.cloudflare.com/client/v4/accounts/abc/ai/v1"),
            )],
            ..Config::default()
        };
        match build_backend(&config, "m").unwrap() {
            Backend::Cloudflare(b) => {
                assert_eq!(b.account_id, "abc");
                assert_eq!(b.api_token, "k");
            }
            _ => panic!("expected the Cloudflare backend"),
        }

        // A generic OpenAI-compatible endpoint falls back to the klava wire.
        let config = Config {
            decide_model: "m".to_string(),
            models: vec![entry("m", Some("https://gw.example"))],
            ..Config::default()
        };
        match build_backend(&config, "m").unwrap() {
            Backend::Klava(b) => {
                assert_eq!(b.base_url, "https://gw.example/v1/alpha/decisions")
            }
            _ => panic!("expected the klava backend"),
        }

        // A Cloudflare-type entry without an account id is a config error.
        let config = Config {
            decide_model: "m".to_string(),
            models: vec![Model {
                base_url: Some("https://api.cloudflare.com/client/v4".to_string()),
                jigor: Some("cloudflare".to_string()),
                ..entry("m", None)
            }],
            ..Config::default()
        };
        let err = build_backend(&config, "m").unwrap_err();
        assert!(err.contains("accounts"), "{err}");
    }

    #[cfg(feature = "decide")]
    #[test]
    fn an_ambiguous_decide_model_is_rejected() {
        let config = Config {
            decide_model: "m".to_string(),
            models: vec![
                entry("m", Some("https://a.example")),
                entry("m", Some("https://b.example")),
            ],
            ..Config::default()
        };
        let err = build_backend(&config, "m").unwrap_err();
        assert!(err.contains("ambiguous"), "{err}");
    }

    #[cfg(feature = "decide")]
    #[test]
    fn approve_defaults_to_the_first_noul_question() {
        let req = request();
        assert_eq!(decide_question_id(&req), Some("intervene"));
    }

    #[cfg(feature = "decide")]
    #[test]
    fn approve_is_none_without_a_noul_question() {
        let mut req = request();
        req.questions[0].kind = "choice".to_string();
        req.questions[0].criteria = Some(json!({"a": null, "b": null}));
        assert_eq!(decide_question_id(&req), None);
    }
}
