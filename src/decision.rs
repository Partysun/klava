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
use crate::ext_api::{DecideQuestion, DecideRequest, DecideResult};
use std::collections::HashMap;

/// Model used when the config leaves `decide_model` empty.
pub const DEFAULT_DECIDE_MODEL: &str = "typesafe/jev-1.13";

/// How long a decision call may take before it is abandoned.
pub const DECIDE_TIMEOUT_SECS: u32 = 8;

/// Upper bound on questions per call, so an extension cannot turn one request
/// into an unbounded classifier bill.
pub const MAX_QUESTIONS: usize = 16;

/// Klava API key for a decision call.
///
/// The gateway key is the same one the `klava` provider uses: the env var
/// named by the provider's `api_key_name` (e.g. `KLAVA_API_KEY`), the
/// provider's `api_key` field, or the saved `klava login` credentials.
pub(crate) fn api_key(config: &Config) -> Option<String> {
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

/// Run the host's decide model over an extension-supplied request.
///
/// Errors are returned as `Err` (never a silent "yes"), so the host can leave
/// the request unchanged and log why. The extension is only re-run with a
/// result when the call actually succeeded.
#[cfg(feature = "decide")]
pub async fn ask(config: &Config, request: &DecideRequest) -> Result<DecideResult, String> {
    use jigor::{Answer, KlavaBackend, Question};

    validate(request)?;

    let model = config.resolve_decide_model();
    let Some(key) = api_key(config) else {
        return Err("no klava key available for the decide model".to_string());
    };

    let questions: Vec<Question> = request
        .questions
        .iter()
        .map(question_to_jigor)
        .collect::<Result<_, _>>()?;
    let state = request.state.clone();
    let model_for_call = model.clone();
    let threshold = request.threshold.clamp(0.0, 1.0);
    let approve_id = decide_question_id(request).map(str::to_string);

    let asks = tokio::task::spawn_blocking(move || {
        let mut backend = KlavaBackend::for_model(&model_for_call);
        backend.api_key = key;
        backend.timeout_secs = DECIDE_TIMEOUT_SECS;
        backend.ask(&state, &questions, None)
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

    #[test]
    fn approve_defaults_to_the_first_noul_question() {
        let req = request();
        assert_eq!(decide_question_id(&req), Some("intervene"));
    }

    #[test]
    fn approve_is_none_without_a_noul_question() {
        let mut req = request();
        req.questions[0].kind = "choice".to_string();
        req.questions[0].criteria = Some(json!({"a": null, "b": null}));
        assert_eq!(decide_question_id(&req), None);
    }
}
