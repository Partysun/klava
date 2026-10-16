//! Native Anthropic web-search responses.
//!
//! Claude Code's built-in `WebSearch` tool does not call a model: it sends a
//! small sub-request to `/v1/messages` that carries the Anthropic *server* tool
//! `web_search_20250305` plus `tool_choice: {type: "tool", name: "web_search"}`.
//! The real Anthropic backend answers that sub-request with native content
//! blocks — `server_tool_use` and `web_search_tool_result` — which Claude Code
//! uses to report "Did N searches" and to render citations.
//!
//! When klava answers that sub-request with plain text (the generic search
//! loop's output), Claude Code counts 0 searches even though the search ran.
//! This module detects the sub-request and rebuilds the response in the native
//! shape: `server_tool_use` + `web_search_tool_result` + the answer text, so
//! the search is counted and its sources surface as citations.

use crate::models::anthropic;
use crate::search::wire::{SearchOutcome, Source};
use serde_json::{Value, json};

/// `type` of the Anthropic server tool Claude Code's WebSearch sub-request
/// advertises. The date suffix varies across Claude versions, so matching is a
/// prefix.
const WEB_SEARCH_SERVER_TOOL_TYPE: &str = "web_search";
/// The tool name forced by the sub-request's `tool_choice`.
const WEB_SEARCH_TOOL_NAME: &str = "web_search";
/// Prefix Claude Code uses to embed the query in the sub-request's user turn.
const QUERY_PREFIX: &str = "Perform a web search for the query: ";

/// Detect a Claude Code WebSearch sub-request and extract its query.
///
/// The sub-request is the only Anthropic request that carries a schema-less
/// `web_search*` *server* tool together with a `tool_choice` that forces that
/// tool. Returns `None` for every other request, so ordinary conversations are
/// unaffected.
pub fn detect(req: &anthropic::AnthropicRequest) -> Option<String> {
    let has_server_tool = req.tools.as_ref().is_some_and(|tools| {
        tools.iter().any(|t| {
            t.tool_type
                .as_deref()
                .is_some_and(|ty| ty.starts_with(WEB_SEARCH_SERVER_TOOL_TYPE))
        })
    });
    if !has_server_tool {
        return None;
    }

    let forced = req
        .extra
        .get("tool_choice")
        .and_then(|tc| tc.get("type"))
        .and_then(Value::as_str)
        == Some("tool")
        && req
            .extra
            .get("tool_choice")
            .and_then(|tc| tc.get("name"))
            .and_then(Value::as_str)
            == Some(WEB_SEARCH_TOOL_NAME);
    if !forced {
        return None;
    }

    // The query lives in the (single) user turn, embedded after a fixed
    // prefix. Fall back to the whole turn text so a changed prefix still
    // produces a usable query.
    let query = req
        .messages
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .and_then(|m| match &m.content {
            anthropic::MessageContent::Text(t) => Some(t.clone()),
            anthropic::MessageContent::Blocks(blocks) => {
                let text: String = blocks
                    .iter()
                    .filter_map(|b| match b {
                        anthropic::ContentBlock::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect();
                (!text.is_empty()).then_some(text)
            }
        })
        .map(|text| {
            text.strip_prefix(QUERY_PREFIX)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(text.trim())
                .to_string()
        })
        .filter(|q| !q.is_empty())?;

    Some(query)
}

/// A native web-search answer: the executed query, the search outcome, and the
/// `server_tool_use` id that ties the two blocks together.
#[derive(Debug, Clone)]
pub struct NativeSearchResult {
    pub query: String,
    pub outcome: SearchOutcome,
}

/// Build the native Anthropic response body for one executed search.
///
/// Content order mirrors the Anthropic API: `server_tool_use`, then
/// `web_search_tool_result` with the cited sources, then the answer text.
/// `server_tool_use` and `web_search_tool_result` are the two blocks Claude
/// Code counts (`searchCount = max(#server_tool_use, #web_search_tool_result)`).
pub fn response_body(
    model: &str,
    result: &NativeSearchResult,
    input_tokens: u32,
    output_tokens: u32,
) -> Value {
    let tool_use_id = crate::utils::new_anthropic_server_tool_id();
    let sources = web_search_results(&result.outcome);

    json!({
        "id": crate::utils::new_anthropic_message_id(),
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": [
            {
                "type": "server_tool_use",
                "id": tool_use_id,
                "name": "web_search",
                "input": { "query": result.query }
            },
            {
                "type": "web_search_tool_result",
                "tool_use_id": tool_use_id,
                "content": sources
            },
            {
                "type": "text",
                "text": result.outcome.content
            }
        ],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "stop_details": null,
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
            "server_tool_use": { "web_search_requests": 1, "web_fetch_requests": 0 }
        }
    })
}

/// Synthesize the Anthropic SSE event stream that carries the same native
/// response, so streaming clients (the sub-request can be sent with
/// `stream: true`) receive valid events.
pub fn response_sse(
    model: &str,
    result: &NativeSearchResult,
    input_tokens: u32,
    output_tokens: u32,
) -> String {
    let tool_use_id = crate::utils::new_anthropic_server_tool_id();
    let sources = web_search_results(&result.outcome);
    let text = result.outcome.content.clone();

    let mut out = String::new();

    let event = |out: &mut String, data: &Value| {
        out.push_str("event: content_block\n");
        out.push_str(&format!("data: {}\n\n", data));
    };

    event(
        &mut out,
        &json!({
            "type": "message_start",
            "message": {
                "id": crate::utils::new_anthropic_message_id(),
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {
                    "input_tokens": input_tokens,
                    "output_tokens": output_tokens,
                    "server_tool_use": { "web_search_requests": 1, "web_fetch_requests": 0 }
                }
            }
        }),
    );

    event(
        &mut out,
        &json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {
                "type": "server_tool_use",
                "id": tool_use_id,
                "name": "web_search",
                "input": {}
            }
        }),
    );
    event(
        &mut out,
        &json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {
                "type": "input_json_delta",
                "partial_json": json!({ "query": result.query }).to_string()
            }
        }),
    );
    event(
        &mut out,
        &json!({ "type": "content_block_stop", "index": 0 }),
    );

    event(
        &mut out,
        &json!({
            "type": "content_block_start",
            "index": 1,
            "content_block": {
                "type": "web_search_tool_result",
                "tool_use_id": tool_use_id,
                "content": sources
            }
        }),
    );
    event(
        &mut out,
        &json!({ "type": "content_block_stop", "index": 1 }),
    );

    event(
        &mut out,
        &json!({
            "type": "content_block_start",
            "index": 2,
            "content_block": { "type": "text", "text": "" }
        }),
    );
    if !text.is_empty() {
        event(
            &mut out,
            &json!({
                "type": "content_block_delta",
                "index": 2,
                "delta": { "type": "text_delta", "text": text }
            }),
        );
    }
    event(
        &mut out,
        &json!({ "type": "content_block_stop", "index": 2 }),
    );

    event(
        &mut out,
        &json!({
            "type": "message_delta",
            "delta": { "stop_reason": "end_turn", "stop_sequence": null },
            "usage": { "output_tokens": output_tokens }
        }),
    );
    event(&mut out, &json!({ "type": "message_stop" }));

    out
}

/// The `web_search_result` entries for a search outcome, in citation order.
fn web_search_results(outcome: &SearchOutcome) -> Vec<Value> {
    outcome.sources.iter().map(web_search_result).collect()
}

fn web_search_result(source: &Source) -> Value {
    let mut entry = json!({
        "type": "web_search_result",
        "url": source.url,
    });
    if let Some(title) = source.title.as_deref().filter(|t| !t.trim().is_empty()) {
        entry["title"] = json!(title);
    }
    entry
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nested_request() -> anthropic::AnthropicRequest {
        serde_json::from_value(json!({
            "model": "claude-opus-5",
            "max_tokens": 4096,
            "messages": [{
                "role": "user",
                "content": "Perform a web search for the query: latest rust release"
            }],
            "tools": [{"type": "web_search_20250305", "name": "web_search", "max_uses": 8}],
            "tool_choice": {"type": "tool", "name": "web_search"}
        }))
        .unwrap()
    }

    #[test]
    fn detects_native_sub_request_and_extracts_query() {
        let q = detect(&nested_request());
        assert_eq!(q.as_deref(), Some("latest rust release"));
    }

    #[test]
    fn falls_back_to_whole_user_turn_without_prefix() {
        let mut req = nested_request();
        req.messages[0].content =
            anthropic::MessageContent::Text("latest rust release".to_string());
        let q = detect(&req);
        assert_eq!(q.as_deref(), Some("latest rust release"));
    }

    #[test]
    fn ignores_requests_without_forced_tool_choice() {
        let mut req = nested_request();
        req.extra["tool_choice"] = json!({"type": "auto"});
        assert_eq!(detect(&req), None);
    }

    #[test]
    fn ignores_requests_without_server_tool() {
        let mut req = nested_request();
        req.tools = None;
        assert_eq!(detect(&req), None);
    }

    #[test]
    fn ignores_ordinary_conversations() {
        let req: anthropic::AnthropicRequest = serde_json::from_value(json!({
            "model": "claude-opus-5",
            "max_tokens": 4096,
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .unwrap();
        assert_eq!(detect(&req), None);
    }

    #[test]
    fn response_body_has_native_blocks_in_order() {
        let result = NativeSearchResult {
            query: "latest rust release".to_string(),
            outcome: SearchOutcome {
                content: "Rust 1.90 is out.".to_string(),
                sources: vec![
                    Source {
                        url: "https://a.example".to_string(),
                        title: Some("A".to_string()),
                    },
                    Source {
                        url: "https://b.example".to_string(),
                        title: None,
                    },
                ],
            },
        };
        let body = response_body("claude-opus-5", &result, 10, 20);

        let content = body["content"].as_array().unwrap();
        assert_eq!(content.len(), 3);

        let s = &content[0];
        assert_eq!(s["type"], "server_tool_use");
        assert_eq!(s["name"], "web_search");
        assert_eq!(s["input"]["query"], "latest rust release");

        let r = &content[1];
        assert_eq!(r["type"], "web_search_tool_result");
        assert_eq!(r["tool_use_id"], s["id"]);
        assert_eq!(r["content"][0]["title"], "A");
        assert_eq!(r["content"][0]["url"], "https://a.example");
        assert_eq!(r["content"][1]["title"], Value::Null);

        assert_eq!(content[2]["type"], "text");
        assert_eq!(content[2]["text"], "Rust 1.90 is out.");

        assert_eq!(body["stop_reason"], "end_turn");
        assert_eq!(body["usage"]["server_tool_use"]["web_search_requests"], 1);
    }

    #[test]
    fn sse_emits_native_blocks_and_deltas() {
        let result = NativeSearchResult {
            query: "latest rust release".to_string(),
            outcome: SearchOutcome {
                content: "Rust 1.90 is out.".to_string(),
                sources: vec![Source {
                    url: "https://a.example".to_string(),
                    title: Some("A".to_string()),
                }],
            },
        };
        let sse = response_sse("claude-opus-5", &result, 10, 20);

        assert!(sse.contains("\"type\":\"server_tool_use\""));
        assert!(sse.contains("\"type\":\"input_json_delta\""));
        assert!(sse.contains("\"type\":\"web_search_tool_result\""));
        assert!(sse.contains("\"type\":\"web_search_result\""));
        assert!(sse.contains("\"type\":\"text_delta\""));
        assert!(sse.contains("\"type\":\"message_stop\""));
        assert!(sse.contains("web_search_requests"));
    }
}
