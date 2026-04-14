use crate::config::Config;
use crate::models::openai::DeltaToolCall;
use crate::models::openai::StreamChunk;
use serde_json::json;
use std::any::Any;
use std::collections::HashMap;

/// Hook output for stream chunks
#[derive(Debug)]
pub enum StreamHookResult {
    /// Transform to Anthropic format (default)
    Transform(Vec<String>),
    /// Keep original OpenAI format (passthrough)
    PassThrough(String),
    /// Skip this chunk
    Skip,
}

/// Manages stream state - accessible by hooks
pub struct StreamStateManager {
    // Core state
    pub buffer: String,
    pub message_id: Option<String>,
    pub current_model: Option<String>,
    pub content_index: usize,
    pub tool_call_id: Option<String>,
    pub tool_call_name: Option<String>,
    pub tool_call_args: String,
    pub has_sent_message_start: bool,
    pub current_block_type: Option<String>,

    // Extensions for custom hook state
    extensions: HashMap<String, Box<dyn Any + Send>>,
}

impl StreamStateManager {
    pub fn new() -> Self {
        Self {
            buffer: String::new(),
            message_id: None,
            current_model: None,
            content_index: 0,
            tool_call_id: None,
            tool_call_name: None,
            tool_call_args: String::new(),
            has_sent_message_start: false,
            current_block_type: None,
            extensions: HashMap::new(),
        }
    }

    /// Get custom hook state
    pub fn get<T: Any + Send>(&self, key: &str) -> Option<&T> {
        self.extensions.get(key).and_then(|any| any.downcast_ref())
    }

    /// Set custom hook state
    pub fn set<T: Any + Send + 'static>(&mut self, key: String, value: T) {
        self.extensions.insert(key, Box::new(value));
    }
}

/// Default hook: Transform OpenAI chunks to Anthropic format
pub async fn default_stream_transform_hook(
    chunk: &StreamChunk,
    state: &mut StreamStateManager,
    _config: &Config,
) -> Result<StreamHookResult, Box<dyn std::error::Error + Send + Sync>> {
    let mut events = Vec::new();

    // Initialize state from first chunk
    if state.message_id.is_none() {
        state.message_id = Some(chunk.id.clone());
    }
    if state.current_model.is_none() {
        state.current_model = Some(chunk.model.clone());
    }

    let choice = match chunk.choices.first() {
        Some(c) => c,
        None => return Ok(StreamHookResult::Transform(vec![])),
    };

    // Send message_start if not sent
    if !state.has_sent_message_start {
        events.push(create_message_start_event(state));
        state.has_sent_message_start = true;
    }

    // Handle reasoning (thinking)
    if let Some(reasoning) = &choice.delta.reasoning {
        events.extend(handle_stream_reasoning(reasoning, state));
    }

    // Handle content
    if let Some(content) = &choice.delta.content
        && !content.is_empty()
    {
        events.extend(handle_stream_content(content, state));
    }

    // Handle tool calls
    if let Some(tool_calls) = &choice.delta.tool_calls {
        events.extend(handle_stream_tool_calls(tool_calls, state));
    }

    // Handle finish reason
    if let Some(finish_reason) = &choice.finish_reason {
        events.extend(handle_stream_finish_reason(
            finish_reason,
            chunk.usage.as_ref(),
            state,
        ));
    }

    Ok(StreamHookResult::Transform(events))
}

/// Log stream chunks for debugging
pub async fn stream_logging_hook(
    chunk: &StreamChunk,
    _state: &mut StreamStateManager,
    config: &Config,
) -> Result<StreamHookResult, Box<dyn std::error::Error + Send + Sync>> {
    if config.verbose {
        tracing::trace!(
            "Stream chunk: model={}, content={:?}, reasoning={:?}, tool_calls={:?}",
            chunk.model,
            chunk.choices.first().and_then(|c| c.delta.content.as_ref()),
            chunk
                .choices
                .first()
                .and_then(|c| c.delta.reasoning.as_ref()),
            chunk
                .choices
                .first()
                .and_then(|c| c.delta.tool_calls.as_ref()),
        );
    }
    // Pass through - this hook doesn't transform
    Ok(StreamHookResult::Transform(vec![]))
}

/// Keep OpenAI format instead of transforming to Anthropic
pub async fn stream_passthrough_hook(
    chunk: &StreamChunk,
    _state: &mut StreamStateManager,
    _config: &Config,
) -> Result<StreamHookResult, Box<dyn std::error::Error + Send + Sync>> {
    let original_sse = format!("data: {}\n\n", serde_json::to_string(chunk)?);
    Ok(StreamHookResult::PassThrough(original_sse))
}

// Helper functions

fn create_message_start_event(state: &StreamStateManager) -> String {
    let event = json!({
        "type": "message_start",
        "message": {
            "id": state.message_id.clone().unwrap_or_default(),
            "type": "message",
            "role": "assistant",
            "model": state.current_model.clone().unwrap_or_default(),
            "usage": { "input_tokens": 0, "output_tokens": 0 }
        }
    });
    format!(
        "event: message_start\ndata: {}\n\n",
        serde_json::to_string(&event).unwrap_or_default()
    )
}

fn handle_stream_reasoning(reasoning: &str, state: &mut StreamStateManager) -> Vec<String> {
    let mut events = Vec::new();

    if state.current_block_type.is_none() || state.current_block_type.as_deref() != Some("thinking")
    {
        let event = json!({
            "type": "content_block_start",
            "index": state.content_index,
            "content_block": { "type": "thinking", "thinking": "" }
        });
        events.push(format!(
            "event: content_block_start\ndata: {}\n\n",
            serde_json::to_string(&event).unwrap_or_default()
        ));
        state.current_block_type = Some("thinking".to_string());
    }

    let event = json!({
        "type": "content_block_delta",
        "index": state.content_index,
        "delta": { "type": "thinking_delta", "thinking": reasoning }
    });
    events.push(format!(
        "event: content_block_delta\ndata: {}\n\n",
        serde_json::to_string(&event).unwrap_or_default()
    ));

    events
}

fn handle_stream_content(content: &str, state: &mut StreamStateManager) -> Vec<String> {
    let mut events = Vec::new();

    // Check if we need to start a new content block
    if state.current_block_type.is_none() || state.current_block_type.as_deref() != Some("text") {
        if state.current_block_type.is_some() {
            events.push(close_current_block(state));
            state.content_index += 1;
        }

        let event = json!({
            "type": "content_block_start",
            "index": state.content_index,
            "content_block": { "type": "text", "text": "" }
        });
        events.push(format!(
            "event: content_block_start\ndata: {}\n\n",
            serde_json::to_string(&event).unwrap_or_default()
        ));
        state.current_block_type = Some("text".to_string());
    }

    let event = json!({
        "type": "content_block_delta",
        "index": state.content_index,
        "delta": { "type": "text_delta", "text": content }
    });
    events.push(format!(
        "event: content_block_delta\ndata: {}\n\n",
        serde_json::to_string(&event).unwrap_or_default()
    ));

    events
}

fn handle_stream_tool_calls(
    tool_calls: &[DeltaToolCall],
    state: &mut StreamStateManager,
) -> Vec<String> {
    let mut events = Vec::new();

    for tool_call in tool_calls {
        if let Some(id) = &tool_call.id {
            if state.current_block_type.is_some() {
                events.push(close_current_block(state));
                state.content_index += 1;
            }
            state.tool_call_id = Some(id.clone());
            state.tool_call_args = String::new();
        }

        if let Some(function) = &tool_call.function {
            if let Some(name) = &function.name {
                state.tool_call_name = Some(name.clone());

                let event = json!({
                    "type": "content_block_start",
                    "index": state.content_index,
                    "content_block": {
                        "type": "tool_use",
                        "id": state.tool_call_id.clone().unwrap_or_default(),
                        "name": name
                    }
                });
                events.push(format!(
                    "event: content_block_start\ndata: {}\n\n",
                    serde_json::to_string(&event).unwrap_or_default()
                ));
                state.current_block_type = Some("tool_use".to_string());
            }

            if let Some(args) = &function.arguments {
                //. Check if the incoming chunk is a complete JSON Object
                if let Ok(_full_json) = serde_json::from_str::<serde_json::Value>(args) {
                    // It is a full object.

                    if state.tool_call_args.is_empty() {
                        // Case A: We have nothing buffered yet.
                        // Accept this full object as the first (and only) chunk.
                        state.tool_call_args.push_str(args);
                        events.push(emit_arg_delta(state.content_index, args));
                    } else {
                        // Case B: We are streaming. This might be a duplicate OR a completion.

                        // Check if the full object starts with what we have buffered so far.
                        if args.starts_with(&state.tool_call_args) {
                            // It is a continuation/completion!
                            // We only need to send the part we haven't sent yet.
                            let missing_part = &args[state.tool_call_args.len()..];

                            if !missing_part.is_empty() {
                                state.tool_call_args.push_str(missing_part);
                                events.push(emit_arg_delta(state.content_index, missing_part));
                            }
                            // If missing_part is empty, it was a true duplicate, so we do nothing.
                        } else {
                            // It doesn't start with our buffer.
                            // This is likely a duplicate with different formatting (e.g., spaces).
                            // We ignore it to prevent corruption.
                        }
                    }
                } else {
                    //. It's a fragment (not a full object). Always accept.
                    state.tool_call_args.push_str(args);
                    events.push(emit_arg_delta(state.content_index, args));
                }
            }
        }
    }

    events
}

/// Helper to emit argument delta events
fn emit_arg_delta(index: usize, partial_json: &str) -> String {
    let event = json!({
        "type": "content_block_delta",
        "index": index,
        "delta": { "type": "input_json_delta", "partial_json": partial_json }
    });
    format!(
        "event: content_block_delta\ndata: {}\n\n",
        serde_json::to_string(&event).unwrap_or_default()
    )
}

/// Helper to close the current content block
fn close_current_block(state: &mut StreamStateManager) -> String {
    let index = state.content_index;
    state.current_block_type = None;

    let event = json!({
        "type": "content_block_stop",
        "index": index
    });
    format!(
        "event: content_block_stop\ndata: {}\n\n",
        serde_json::to_string(&event).unwrap_or_default()
    )
}

fn handle_stream_finish_reason(
    finish_reason: &str,
    usage: Option<&crate::models::openai::Usage>,
    state: &mut StreamStateManager,
) -> Vec<String> {
    let mut events = Vec::new();

    if state.current_block_type.as_deref() == Some("tool_use") && !state.tool_call_args.is_empty() {
        let is_valid_json =
            serde_json::from_str::<serde_json::Value>(&state.tool_call_args).is_ok();

        if !is_valid_json {
            // If invalid, try to append a closing brace and check again.
            let potential_fix = format!("{} }}", state.tool_call_args.trim_end());

            if serde_json::from_str::<serde_json::Value>(&potential_fix).is_ok() {
                // The missing brace fixes it! Emit the closing brace.
                state.tool_call_args.push('}');

                let event = json!({
                    "type": "content_block_delta",
                    "index": state.content_index,
                    "delta": { "type": "input_json_delta", "partial_json": "}" }
                });
                events.push(format!(
                    "event: content_block_delta\ndata: {}\n\n",
                    serde_json::to_string(&event).unwrap_or_default()
                ));
            }
        }
    }

    // Close current content block
    if state.current_block_type.is_some() {
        events.push(close_current_block(state));
    }

    // Send message_delta with stop_reason
    let stop_reason = crate::transform::map_stop_reason(Some(finish_reason));
    let event = json!({
        "type": "message_delta",
        "delta": {
            "stop_reason": stop_reason,
            "stop_sequence": serde_json::Value::Null
        },
        "usage": usage.map(|u| json!({ "output_tokens": u.completion_tokens }))
    });
    events.push(format!(
        "event: message_delta\ndata: {}\n\n",
        serde_json::to_string(&event).unwrap_or_default()
    ));

    // Send message stop
    let stop_event = json!({
        "type": "message_stop"
    });
    events.push(format!(
        "event: message_stop\ndata: {}\n\n",
        serde_json::to_string(&stop_event).unwrap_or_default()
    ));

    events
}

#[cfg(test)]
mod stream_tests {
    use super::*;
    use crate::models::openai::Delta;
    use crate::models::openai::StreamChoice;

    #[tokio::test]
    async fn test_stream_logging_hook_logs_chunk() {
        let chunk = StreamChunk {
            id: "test-id".to_string(),
            model: "gpt".to_string(),
            choices: vec![StreamChoice {
                index: 0,
                delta: Delta {
                    content: Some("Hello".to_string()),
                    reasoning: None,
                    tool_calls: None,
                    role: None,
                },
                finish_reason: None,
                logprobs: None,
            }],
            ..Default::default()
        };

        let mut state = StreamStateManager::new();
        let config = Config {
            verbose: true,
            ..Default::default()
        };

        let result = stream_logging_hook(&chunk, &mut state, &config).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_stream_transform_hook_transforms_content() {
        let chunk = StreamChunk {
            id: "test-id".to_string(),
            model: "gpt".to_string(),
            choices: vec![StreamChoice {
                index: 0,
                logprobs: None,
                delta: Delta {
                    content: Some("Hello".to_string()),
                    reasoning: None,
                    tool_calls: None,
                    role: None,
                },
                finish_reason: None,
            }],
            ..Default::default()
        };

        let mut state = StreamStateManager::new();
        let config = Config::default();

        let result = default_stream_transform_hook(&chunk, &mut state, &config).await;
        assert!(result.is_ok());

        match result.unwrap() {
            StreamHookResult::Transform(events) => {
                assert!(!events.is_empty());
                assert!(events.iter().any(|e| e.contains("content_block_delta")));
            }
            _ => panic!("Expected Transform result"),
        }
    }

    #[tokio::test]
    async fn test_stream_passthrough_keeps_original() {
        let chunk = StreamChunk {
            id: "test-id".to_string(),
            model: "gpt".to_string(),
            choices: vec![StreamChoice {
                index: 0,
                logprobs: None,
                delta: Delta {
                    content: Some("test".to_string()),
                    reasoning: None,
                    tool_calls: None,
                    role: None,
                },
                finish_reason: None,
            }],
            ..Default::default()
        };

        let mut state = StreamStateManager::new();
        let config = Config::default();

        let result = stream_passthrough_hook(&chunk, &mut state, &config).await;
        assert!(result.is_ok());

        match result.unwrap() {
            StreamHookResult::PassThrough(sse) => {
                assert!(sse.contains("data: "));
                assert!(sse.contains("test-id"));
            }
            _ => panic!("Expected PassThrough result"),
        }
    }
}
