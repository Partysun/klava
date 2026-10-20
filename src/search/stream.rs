//! Streaming search loop.
//!
//! The non-streaming path ([`super`]) runs the tool loop by re-sending
//! non-streaming requests until the model answers. For streaming clients that
//! would cost live token streaming, so this module drives the same loop at the
//! SSE level:
//!
//! * content / reasoning / role deltas are forwarded to the client immediately,
//! * tool-call deltas and the finish chunk are buffered,
//! * at the end of a round, if the model called `web_search`, the buffered
//!   deltas are discarded, the searches run, the results are appended, and the
//!   next upstream round is streamed through the *same* converter — the client
//!   sees one continuous answer and never learns about the search tool.
//! * if the round contains no search call (normal answer, or other tools), the
//!   buffered deltas are flushed unchanged and the stream ends.
//!
//! Rounds are bounded by [`MAX_SEARCH_ROUNDS`]; past that, search calls are
//! dropped and whatever the model produced is flushed.

use crate::config::Config;
use crate::error::{Error, Result};
use crate::models::openai;
use crate::search::{
    SearchCall, TOOL_NAME, append_search_results, execute, extract_query, inject_tool,
    resolve_settings, user_context,
};
use async_stream::stream;
use bytes::Bytes;
use reqwest::Client;
use serde_json::Value;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use tokio_stream::{Stream, StreamExt};

/// Upper bound on search executions per client request (mirrors the
/// non-streaming loop).
const MAX_SEARCH_ROUNDS: usize = 3;

/// A parsed SSE frame from an upstream OpenAI-compatible stream.
enum FrameClass {
    /// `data: [DONE]` — end of this round's stream.
    Done,
    /// An upstream error event — forward verbatim (never swallow).
    Error,
    /// Content/reasoning/role — safe to forward immediately.
    Forward(Bytes),
    /// Tool-call or finish delta — buffer until the round is classified.
    Hold,
}

/// Drive a streaming, search-enabled request to completion.
///
/// `openai_req` must already carry the injected search tool; `first_response`
/// is the already-sent upstream response for round one (sent with hooks by the
/// caller). Returns a stream of raw OpenAI SSE bytes ready for the caller's
/// converter (passthrough / Anthropic / Responses).
pub fn drive(
    client: Client,
    config: Arc<Config>,
    mut openai_req: openai::OpenAIRequest,
    entry: crate::providers::Model,
    first_response: reqwest::Response,
) -> Pin<Box<dyn Stream<Item = Result<Bytes>> + Send>> {
    let user_context = user_context(&openai_req);
    Box::pin(stream! {
        inject_tool(&mut openai_req);

        let mut rounds = 0usize;
        let mut response = Some(first_response);
        let mut first = true;

        loop {
            if !first {
                match crate::proxy::send_openai_request_via(
                    &client,
                    &config,
                    &openai_req,
                    Some(&entry),
                )
                .await
                {
                    Ok(resp) => response = Some(resp),
                    Err(e) => {
                        yield Err(e);
                        return;
                    }
                }
            }
            first = false;

            let Some(current) = response.take() else {
                return;
            };
            if !current.status().is_success() {
                let status = current.status();
                let body = current.text().await.unwrap_or_default();
                yield Err(Error::Upstream(format!(
                    "Upstream returned {status} after web search: {body}"
                )));
                return;
            }

            // ---- Consume one round of SSE ----
            let mut body = current.bytes_stream();
            let mut buffer: Vec<u8> = Vec::new();
            let mut buffered: Vec<Bytes> = Vec::new();
            // tool index -> (id, name, accumulated arguments)
            let mut tool_acc: HashMap<usize, (String, String, String)> = HashMap::new();
            let mut saw_done = false;

            while let Some(chunk) = body.next().await {
                let chunk = match chunk {
                    Ok(c) => c,
                    Err(e) => {
                        yield Err(Error::Http(e));
                        return;
                    }
                };
                buffer.extend_from_slice(&chunk);

                while let Some(pos) = buffer.windows(2).position(|w| w == b"\n\n") {
                    let raw: Vec<u8> = buffer.drain(..pos + 2).collect();
                    match classify_frame(&raw) {
                        FrameClass::Done => {
                            buffered.push(Bytes::from(raw));
                            saw_done = true;
                        }
                        FrameClass::Error => yield Ok(Bytes::from(raw)),
                        FrameClass::Forward(b) => yield Ok(b),
                        FrameClass::Hold => {
                            accumulate_tool_calls(&raw, &mut tool_acc);
                            buffered.push(Bytes::from(raw));
                        }
                    }
                    if saw_done {
                        break;
                    }
                }
                if saw_done {
                    break;
                }
            }

            // Flush any trailing partial buffer (mirrors converter behavior).
            if !buffer.is_empty() {
                yield Ok(Bytes::from(std::mem::take(&mut buffer)));
            }

            // ---- Classify the round ----
            let searches: Vec<SearchCall> = tool_acc
                .iter()
                .filter(|(_, (_, name, _))| name == TOOL_NAME)
                .map(|(_, (id, _, args))| SearchCall {
                    id: id.clone(),
                    query: extract_query(args),
                })
                .collect();
            let has_other = tool_acc.values().any(|(_, name, _)| name != TOOL_NAME);

            let can_search = !searches.is_empty()
                && !has_other
                && rounds < MAX_SEARCH_ROUNDS
                && searches.iter().all(|c| !c.query.trim().is_empty());

            if can_search {
                // Execute the searches and re-request; the buffered tool-call
                // deltas and this round's finish chunk are never forwarded.
                rounds += 1;
                let mut results: Vec<(SearchCall, String)> = Vec::new();
                for call in &searches {
                    let settings =
                        resolve_settings(&config, &call.query, user_context.as_deref()).await;
                    tracing::info!(
                        "Web search [{}]: query={:?} engine={} context={} max_results={}",
                        rounds,
                        call.query,
                        settings.engine.as_str(),
                        settings.context.as_str(),
                        settings.max_results
                    );
                    let result = match execute(&client, &config, &call.query, &settings).await {
                        Ok(result) => result,
                        Err(e) => {
                            tracing::warn!("Web search failed: {e}");
                            format!("Web search failed: {e}")
                        }
                    };
                    results.push((call.clone(), result));
                }
                append_search_results(&mut openai_req, &results);
                continue;
            }

            // Not a search round (or we gave up): flush the buffered deltas —
            // other tool calls, the finish chunk, and [DONE] — unchanged, but
            // drop any search tool-call deltas so they never reach the client.
            for frame in buffered {
                if frame_is_search_only(&frame, &tool_acc) {
                    continue;
                }
                yield Ok(frame);
            }
            return;
        }
    })
}

/// Classify a single SSE frame (a `data:` event including its trailing blank
/// line) without doing any stateful conversion.
fn classify_frame(raw: &[u8]) -> FrameClass {
    let Some(payload) = extract_data(raw) else {
        return FrameClass::Forward(Bytes::from(raw.to_vec()));
    };
    let trimmed = trim_ascii(&payload);
    if trimmed == b"[DONE]" {
        return FrameClass::Done;
    }

    let Ok(value) = serde_json::from_slice::<Value>(trimmed) else {
        return FrameClass::Forward(Bytes::from(raw.to_vec()));
    };

    // Never swallow upstream error events (e.g. OpenRouter's
    // `{"error": {...}}` on provider failure).
    if value.get("error").is_some() {
        return FrameClass::Error;
    }

    let choice = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first());
    let delta = choice.and_then(|c| c.get("delta"));

    let has_tool_calls = delta
        .and_then(|d| d.get("tool_calls"))
        .and_then(Value::as_array)
        .is_some_and(|t| !t.is_empty());
    if has_tool_calls {
        return FrameClass::Hold;
    }

    let has_finish = choice
        .and_then(|c| c.get("finish_reason"))
        .and_then(Value::as_str)
        .is_some_and(|f| !f.is_empty());
    if has_finish {
        return FrameClass::Hold;
    }

    FrameClass::Forward(Bytes::from(raw.to_vec()))
}

/// Extract the `data:` payload of an SSE event (joined across continuation
/// lines), mirroring the streaming converter's parser.
fn extract_data(raw: &[u8]) -> Option<Vec<u8>> {
    let mut parts = Vec::new();
    for line in raw.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if let Some(payload) = line.strip_prefix(b"data:") {
            let trimmed = trim_ascii(payload);
            if !trimmed.is_empty() {
                parts.push(trimmed.to_vec());
            }
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(&b'\n'))
    }
}

fn trim_ascii(mut bytes: &[u8]) -> &[u8] {
    while let [first, rest @ ..] = bytes {
        if first.is_ascii_whitespace() {
            bytes = rest;
        } else {
            break;
        }
    }
    while let [rest @ .., last] = bytes {
        if last.is_ascii_whitespace() {
            bytes = rest;
        } else {
            break;
        }
    }
    bytes
}

/// Accumulate the tool-call deltas of one frame into `tool_acc`, keyed by tool
/// index. Arguments are concatenated across frames; id/name are filled on the
/// first frame that carries them.
fn accumulate_tool_calls(raw: &[u8], tool_acc: &mut HashMap<usize, (String, String, String)>) {
    let Some(payload) = extract_data(raw) else {
        return;
    };
    let Ok(value) = serde_json::from_slice::<Value>(trim_ascii(&payload)) else {
        return;
    };
    let Some(calls) = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
        .and_then(|c| c.get("delta"))
        .and_then(|d| d.get("tool_calls"))
        .and_then(Value::as_array)
    else {
        return;
    };
    for call in calls {
        let Some(index) = call
            .get("index")
            .and_then(Value::as_u64)
            .map(|i| i as usize)
        else {
            continue;
        };
        let entry = tool_acc
            .entry(index)
            .or_insert_with(|| (String::new(), String::new(), String::new()));
        if let Some(id) = call
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        {
            entry.0 = id.to_string();
        }
        if let Some(name) = call
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
        {
            entry.1 = name.to_string();
        }
        if let Some(args) = call
            .get("function")
            .and_then(|f| f.get("arguments"))
            .and_then(Value::as_str)
        {
            entry.2.push_str(args);
        }
    }
}

/// Whether a frame's tool-call deltas are all `web_search` calls (and it has
/// at least one) — used to drop search frames from the final flush.
fn frame_is_search_only(raw: &[u8], tool_acc: &HashMap<usize, (String, String, String)>) -> bool {
    let Some(payload) = extract_data(raw) else {
        return false;
    };
    let Ok(value) = serde_json::from_slice::<Value>(trim_ascii(&payload)) else {
        return false;
    };
    let Some(calls) = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
        .and_then(|c| c.get("delta"))
        .and_then(|d| d.get("tool_calls"))
        .and_then(Value::as_array)
    else {
        return false;
    };
    if calls.is_empty() {
        return false;
    }
    calls.iter().all(|call| {
        let Some(index) = call
            .get("index")
            .and_then(Value::as_u64)
            .map(|i| i as usize)
        else {
            return false;
        };
        tool_acc
            .get(&index)
            .map(|(_, name, _)| name == TOOL_NAME)
            .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(payload: &str) -> Vec<u8> {
        format!("data: {payload}\n\n").into_bytes()
    }

    #[test]
    fn classify_content_forwards() {
        let raw =
            frame(r#"{"id":"c","choices":[{"delta":{"content":"hi"},"finish_reason":null}]}"#);
        assert!(matches!(classify_frame(&raw), FrameClass::Forward(_)));
    }

    #[test]
    fn classify_done() {
        assert!(matches!(
            classify_frame(b"data: [DONE]\n\n"),
            FrameClass::Done
        ));
    }

    #[test]
    fn classify_error_forwards() {
        let raw = frame(r#"{"error":{"message":"boom"}}"#);
        assert!(matches!(classify_frame(&raw), FrameClass::Error));
    }

    #[test]
    fn classify_tool_call_holds() {
        let raw = frame(
            r#"{"id":"c","choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"web_search","arguments":""}}]},"finish_reason":null}]}"#,
        );
        assert!(matches!(classify_frame(&raw), FrameClass::Hold));
    }

    #[test]
    fn classify_finish_holds() {
        let raw = frame(r#"{"id":"c","choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#);
        assert!(matches!(classify_frame(&raw), FrameClass::Hold));
    }

    #[test]
    fn accumulate_joins_argument_fragments() {
        let mut acc = HashMap::new();
        accumulate_tool_calls(
            &frame(
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"web_search","arguments":"{\"query\":"}}]}}]}"#,
            ),
            &mut acc,
        );
        accumulate_tool_calls(
            &frame(
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"rust"}}]}}]}"#,
            ),
            &mut acc,
        );
        accumulate_tool_calls(
            &frame(
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":" 1.99\"}"}}]}}]}"#,
            ),
            &mut acc,
        );

        let (id, name, args) = acc.get(&0).unwrap();
        assert_eq!(id, "call_1");
        assert_eq!(name, "web_search");
        assert_eq!(args, r#"{"query":"rust 1.99"}"#);
        assert_eq!(extract_query(args), "rust 1.99");
    }

    #[test]
    fn frame_is_search_only_detects_search_frames() {
        let mut acc = HashMap::new();
        acc.insert(0, ("call_1".into(), "web_search".into(), "{}".into()));
        acc.insert(1, ("call_2".into(), "bash".into(), "{}".into()));

        let search_frame = frame(r#"{"choices":[{"delta":{"tool_calls":[{"index":0}]}}]}"#);
        assert!(frame_is_search_only(&search_frame, &acc));

        let other_frame = frame(r#"{"choices":[{"delta":{"tool_calls":[{"index":1}]}}]}"#);
        assert!(!frame_is_search_only(&other_frame, &acc));
    }

    #[test]
    fn extract_data_joins_continuation_lines() {
        let raw = b"data: line1\ndata: line2\n\n";
        assert_eq!(extract_data(raw).unwrap(), b"line1\nline2");
    }
}
