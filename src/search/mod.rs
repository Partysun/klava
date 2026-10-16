//! Model-decided web search.
//!
//! klava offers a search tool to the harness model and drives the tool loop
//! itself: when the model calls the tool, klava intercepts the call (it is
//! never sent to the client), optionally asks a decision model to tune the
//! search settings, runs the search, and feeds the result back upstream until
//! the model answers normally.
//!
//! The search itself runs through the active provider's chat-completions
//! endpoint with a search plugin attached (see [`wire`] and [`settings`]), so
//! it is billed the same way as any other request and no gateway identity
//! leaks into names or messages.

pub mod settings;
pub mod stream;
pub mod wire;

#[cfg(feature = "decide")]
pub mod decide;

pub mod native;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::models::openai;
use reqwest::Client;
use serde_json::{Value, json};
use settings::SearchSettings;

/// Name of the injected tool, as the model sees it.
pub const TOOL_NAME: &str = "web_search";

/// The search tool definition offered to the model.
pub fn tool_definition() -> openai::Tool {
    openai::Tool {
        tool_type: "function".to_string(),
        function: openai::Function {
            name: TOOL_NAME.to_string(),
            description: Some(
                "Search the web for current information. Use this when the answer depends on \
                 recent events, versions, prices, or facts you are not sure about. Returns an \
                 answer with cited sources."
                    .to_string(),
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The search query, phrased as a complete question or a set of keywords."
                    }
                },
                "required": ["query"]
            }),
        },
    }
}

/// Add the search tool to a request if it isn't already present. Returns
/// `true` when the tool was added.
pub fn inject_tool(req: &mut openai::OpenAIRequest) -> bool {
    let tools = req.tools.get_or_insert_with(Vec::new);
    if tools.iter().any(|t| t.function.name == TOOL_NAME) {
        return false;
    }
    tools.push(tool_definition());
    true
}

/// A search tool call the model made: its call id and the extracted query.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchCall {
    pub id: String,
    pub query: String,
}

/// The tool calls in one assistant turn, split into search calls klava owns and
/// everything else the client owns.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolCallSplit {
    /// `web_search` calls, in the order the model emitted them.
    pub searches: Vec<SearchCall>,
    /// Whether the turn also contained calls for tools klava does not own.
    pub has_other_calls: bool,
}

impl ToolCallSplit {
    /// Whether this turn needs search execution before the model can continue.
    pub fn needs_search(&self) -> bool {
        !self.searches.is_empty()
    }
}

/// Split the tool calls of a non-streaming response into search calls and a
/// flag for any other tools. Handles multiple search calls in one turn.
pub fn split_tool_calls(resp: &Value) -> ToolCallSplit {
    let Some(calls) = resp
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("tool_calls"))
        .and_then(Value::as_array)
    else {
        return ToolCallSplit::default();
    };

    let mut split = ToolCallSplit::default();
    for call in calls {
        let name = call
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str);
        if name != Some(TOOL_NAME) {
            split.has_other_calls = true;
            continue;
        }
        let id = call
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let arguments = call
            .get("function")
            .and_then(|f| f.get("arguments"))
            .and_then(Value::as_str)
            .unwrap_or("");
        split.searches.push(SearchCall {
            id,
            query: extract_query(arguments),
        });
    }
    split
}

/// Find the first search tool call in a non-streaming response, if any.
pub fn find_search_call(resp: &Value) -> Option<SearchCall> {
    split_tool_calls(resp).searches.into_iter().next()
}

/// Extract the `query` field from a tool call's JSON arguments. Falls back to
/// the raw argument text when it isn't an object with a `query`.
pub(crate) fn extract_query(arguments: &str) -> String {
    serde_json::from_str::<Value>(arguments)
        .ok()
        .and_then(|v| {
            v.get("query")
                .and_then(Value::as_str)
                .map(|s| s.to_string())
        })
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| arguments.trim().to_string())
}

/// Append one assistant turn containing every search tool call and their
/// results, so the next upstream request carries the search outcomes. All calls
/// from a single model turn are grouped into one assistant message (as the chat
/// protocol requires) followed by one `tool` message per call.
pub fn append_search_results(req: &mut openai::OpenAIRequest, results: &[(SearchCall, String)]) {
    if results.is_empty() {
        return;
    }

    let tool_calls = results
        .iter()
        .map(|(call, _)| openai::ToolCall {
            id: call.id.clone(),
            call_type: "function".to_string(),
            function: openai::FunctionCall {
                name: TOOL_NAME.to_string(),
                arguments: json!({ "query": call.query }).to_string(),
            },
        })
        .collect();

    req.messages.push(openai::Message {
        role: "assistant".to_string(),
        content: None,
        tool_calls: Some(tool_calls),
        tool_call_id: None,
        name: None,
    });

    for (call, result) in results {
        req.messages.push(openai::Message {
            role: "tool".to_string(),
            content: Some(openai::MessageContent::Text(result.clone())),
            tool_calls: None,
            tool_call_id: Some(call.id.clone()),
            name: None,
        });
    }
}

/// Append a single search tool call and its result. Convenience wrapper around
/// [`append_search_results`].
pub fn append_search_result(req: &mut openai::OpenAIRequest, call: &SearchCall, result: &str) {
    append_search_results(req, &[(call.clone(), result.to_string())]);
}

/// Build the base settings from config before any decision tuning.
pub fn settings_from_config(config: &Config) -> SearchSettings {
    SearchSettings {
        engine: settings::Engine::parse(&config.search_engine),
        context: settings::ContextSize::parse(&config.search_context),
        max_results: config.search_max_results,
        search_prompt: config.search_prompt.clone(),
        include_domains: config.search_include_domains.clone(),
        exclude_domains: config.search_exclude_domains.clone(),
    }
    .clamped()
}

/// Resolve the settings for one query: config defaults, optionally tuned by
/// the decision model. Fail-open — any decision failure keeps the defaults.
pub async fn resolve_settings(config: &Config, query: &str) -> SearchSettings {
    let base = settings_from_config(config);
    if !config.search_decide {
        return base;
    }
    #[cfg(feature = "decide")]
    {
        decide::tune(config, query, base).await
    }
    #[cfg(not(feature = "decide"))]
    {
        let _ = query;
        base
    }
}

/// Run one search through the active provider and return its rendered result.
///
/// If the chosen engine is rejected by the backend (some models only support
/// certain engines), the search is retried once with the backend's default
/// engine so a search still happens.
pub async fn execute(
    client: &Client,
    config: &Config,
    query: &str,
    settings: &SearchSettings,
) -> Result<String> {
    Ok(execute_outcome(client, config, query, settings)
        .await?
        .render())
}

/// Run one search and return its structured outcome (answer text plus cited
/// sources). Rendering to the agent-visible string is left to the caller.
pub async fn execute_outcome(
    client: &Client,
    config: &Config,
    query: &str,
    settings: &SearchSettings,
) -> Result<wire::SearchOutcome> {
    let model = config
        .resolve_search_model()
        .ok_or_else(|| Error::Internal("No search model configured".to_string()))?;

    let url = config.chat_completions_url();
    tracing::debug!("Running web search via {} for model {}", url, model);

    let req = wire::build_request(query, &model, settings);
    let response = crate::proxy::send_openai_request(client, config, &req).await?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();

        // An unsupported engine is a configuration mismatch, not a hard
        // failure: fall back to the backend-chosen engine once.
        if status.is_client_error() && settings.engine != settings::Engine::Auto {
            tracing::warn!(
                "web search: engine '{}' rejected ({status}); retrying with the default engine",
                settings.engine.as_str()
            );
            let fallback = SearchSettings {
                engine: settings::Engine::Auto,
                ..settings.clone()
            };
            let req = wire::build_request(query, &model, &fallback);
            let response = crate::proxy::send_openai_request(client, config, &req).await?;
            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                return Err(Error::Upstream(format!(
                    "Search backend returned {status}: {body}"
                )));
            }
            let value: Value = response.json().await?;
            return Ok(wire::parse_response(&value));
        }

        return Err(Error::Upstream(format!(
            "Search backend returned {status}: {body}"
        )));
    }

    let value: Value = response.json().await?;
    Ok(wire::parse_response(&value))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_request() -> openai::OpenAIRequest {
        openai::OpenAIRequest {
            model: "m".to_string(),
            messages: vec![],
            max_tokens: None,
            temperature: None,
            top_p: None,
            stop: None,
            stream: None,
            tools: None,
            tool_choice: None,
            reasoning_effort: None,
            extra: json!({}),
        }
    }

    #[test]
    fn inject_tool_adds_once() {
        let mut req = empty_request();
        assert!(inject_tool(&mut req));
        assert_eq!(req.tools.as_ref().unwrap().len(), 1);
        assert_eq!(req.tools.as_ref().unwrap()[0].function.name, TOOL_NAME);
        // Second injection is a no-op.
        assert!(!inject_tool(&mut req));
        assert_eq!(req.tools.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn inject_tool_preserves_existing_tools() {
        let mut req = empty_request();
        req.tools = Some(vec![openai::Tool {
            tool_type: "function".to_string(),
            function: openai::Function {
                name: "read_file".to_string(),
                description: None,
                parameters: json!({"type": "object"}),
            },
        }]);
        assert!(inject_tool(&mut req));
        let names: Vec<&str> = req
            .tools
            .as_ref()
            .unwrap()
            .iter()
            .map(|t| t.function.name.as_str())
            .collect();
        assert_eq!(names, vec!["read_file", TOOL_NAME]);
    }

    #[test]
    fn find_search_call_extracts_query() {
        let resp = json!({
            "choices": [{
                "message": {
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "web_search", "arguments": "{\"query\":\"rust 1.90\"}"}
                    }]
                }
            }]
        });
        let call = find_search_call(&resp).unwrap();
        assert_eq!(call.id, "call_1");
        assert_eq!(call.query, "rust 1.90");
    }

    #[test]
    fn find_search_call_ignores_other_tools() {
        let resp = json!({
            "choices": [{
                "message": {
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "bash", "arguments": "{\"cmd\":\"ls\"}"}
                    }]
                }
            }]
        });
        assert!(find_search_call(&resp).is_none());
    }

    #[test]
    fn find_search_call_none_without_tool_calls() {
        let resp = json!({"choices": [{"message": {"content": "hi"}}]});
        assert!(find_search_call(&resp).is_none());
    }

    #[test]
    fn split_tool_calls_handles_multiple_searches() {
        let resp = json!({
            "choices": [{
                "message": {
                    "tool_calls": [
                        {"id": "a", "type": "function", "function": {"name": "web_search", "arguments": "{\"query\":\"one\"}"}},
                        {"id": "b", "type": "function", "function": {"name": "web_search", "arguments": "{\"query\":\"two\"}"}}
                    ]
                }
            }]
        });
        let split = split_tool_calls(&resp);
        assert_eq!(split.searches.len(), 2);
        assert_eq!(split.searches[0].query, "one");
        assert_eq!(split.searches[1].query, "two");
        assert!(!split.has_other_calls);
        assert!(split.needs_search());
    }

    #[test]
    fn split_tool_calls_flags_mixed_turns() {
        let resp = json!({
            "choices": [{
                "message": {
                    "tool_calls": [
                        {"id": "a", "type": "function", "function": {"name": "web_search", "arguments": "{\"query\":\"one\"}"}},
                        {"id": "b", "type": "function", "function": {"name": "bash", "arguments": "{\"cmd\":\"ls\"}"}}
                    ]
                }
            }]
        });
        let split = split_tool_calls(&resp);
        assert_eq!(split.searches.len(), 1);
        assert!(split.has_other_calls);
    }

    #[test]
    fn extract_query_falls_back_to_raw_arguments() {
        assert_eq!(extract_query("not json"), "not json");
        assert_eq!(extract_query("{\"other\":1}"), "{\"other\":1}");
    }

    #[test]
    fn append_search_result_pairs_call_and_result() {
        let mut req = empty_request();
        let call = SearchCall {
            id: "call_9".to_string(),
            query: "weather".to_string(),
        };
        append_search_result(&mut req, &call, "sunny\n\nSources:\n- https://a.example\n");

        assert_eq!(req.messages.len(), 2);
        let assistant = &req.messages[0];
        assert_eq!(assistant.role, "assistant");
        let tool_calls = assistant.tool_calls.as_ref().unwrap();
        assert_eq!(tool_calls[0].id, "call_9");
        assert_eq!(tool_calls[0].function.name, TOOL_NAME);

        let tool = &req.messages[1];
        assert_eq!(tool.role, "tool");
        assert_eq!(tool.tool_call_id.as_deref(), Some("call_9"));
        match tool.content.as_ref().unwrap() {
            openai::MessageContent::Text(text) => assert!(text.contains("sunny")),
            other => panic!("expected text content, got {other:?}"),
        }
    }

    #[test]
    fn append_search_results_groups_calls_in_one_assistant_message() {
        let mut req = empty_request();
        let results = vec![
            (
                SearchCall {
                    id: "a".to_string(),
                    query: "one".to_string(),
                },
                "result one".to_string(),
            ),
            (
                SearchCall {
                    id: "b".to_string(),
                    query: "two".to_string(),
                },
                "result two".to_string(),
            ),
        ];
        append_search_results(&mut req, &results);

        // one assistant message with both calls, then one tool message each.
        assert_eq!(req.messages.len(), 3);
        assert_eq!(req.messages[0].role, "assistant");
        assert_eq!(req.messages[0].tool_calls.as_ref().unwrap().len(), 2);
        assert_eq!(req.messages[1].tool_call_id.as_deref(), Some("a"));
        assert_eq!(req.messages[2].tool_call_id.as_deref(), Some("b"));
    }

    #[test]
    fn append_search_results_noop_when_empty() {
        let mut req = empty_request();
        append_search_results(&mut req, &[]);
        assert!(req.messages.is_empty());
    }

    #[test]
    fn settings_from_config_maps_and_clamps() {
        let config = Config {
            search_engine: "web_deep".to_string(),
            search_context: "high".to_string(),
            search_max_results: 999,
            search_prompt: Some("be concise".to_string()),
            search_include_domains: vec!["docs.rs".to_string()],
            ..Config::default()
        };
        let s = settings_from_config(&config);
        assert_eq!(s.engine, settings::Engine::WebDeep);
        assert_eq!(s.context, settings::ContextSize::High);
        assert_eq!(s.max_results, settings::MAX_RESULTS_MAX);
        assert_eq!(s.search_prompt.as_deref(), Some("be concise"));
        assert_eq!(s.include_domains, vec!["docs.rs".to_string()]);
    }

    #[tokio::test]
    async fn resolve_settings_returns_config_defaults_without_decide() {
        let config = Config {
            search_decide: false,
            search_max_results: 6,
            ..Config::default()
        };
        let s = resolve_settings(&config, "anything").await;
        assert_eq!(s.max_results, 6);
    }
}
