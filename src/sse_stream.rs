use crate::models::openai::{DeltaToolCall, StreamChunk};
use bytes::Bytes;
use futures::stream::{Stream, StreamExt};
use serde_json::Value;
use serde_json::json;

/// Manages SSE streaming state and event generation
pub(crate) struct SseStreamBuilder {
    buffer: String,
    message_id: Option<String>,
    current_model: Option<String>,
    content_index: usize,
    tool_call_id: Option<String>,
    tool_call_name: Option<String>,
    tool_call_args: String,
    has_sent_message_start: bool,
    current_block_type: Option<String>,
}

impl SseStreamBuilder {
    fn new() -> Self {
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
        }
    }

    /// Build SSE event string
    fn sse_event(&self, event_type: &str, data: &Value) -> String {
        let payload = serde_json::to_string(data).unwrap_or_default();
        format!("event: {}\ndata: {}\n\n", event_type, payload)
    }

    /// Process a line and yield SSE events
    fn process_line(&mut self, line: &str) -> Vec<Bytes> {
        let mut events = Vec::new();

        let data = match line.strip_prefix("data: ") {
            Some(d) => d,
            None => return events,
        };

        if data.trim() == "[DONE]" {
            if self.current_block_type.is_some() {
                events.push(self.close_current_block());
            }
            events.push(Bytes::from(
                self.sse_event("message_stop", &json!({"type": "message_stop"})),
            ));
            return events;
        }

        let chunk = match serde_json::from_str::<StreamChunk>(data) {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!("Failed to parse SSE chunk: {}", e);
                return events;
            }
        };

        // Initialize state from first chunk
        if self.message_id.is_none() {
            self.message_id = Some(chunk.id.clone());
        }
        if self.current_model.is_none() {
            self.current_model = Some(chunk.model.clone());
        }

        let choice = match chunk.choices.first() {
            Some(c) => c,
            None => return events,
        };

        // Send message_start if not sent
        if !self.has_sent_message_start {
            events.push(self.create_message_start_event());
            self.has_sent_message_start = true;
        }

        // Handle reasoning (thinking)
        if let Some(reasoning) = &choice.delta.reasoning {
            events.extend(self.handle_reasoning(reasoning));
        }

        // Handle content
        if let Some(content) = &choice.delta.content
            && !content.is_empty()
        {
            events.extend(self.handle_content(content));
        }

        // Handle tool calls
        if let Some(tool_calls) = &choice.delta.tool_calls {
            events.extend(self.handle_tool_calls(tool_calls));
        }

        // Handle finish reason
        if let Some(finish_reason) = &choice.finish_reason {
            events.extend(self.handle_finish_reason(finish_reason, chunk.usage.as_ref()));
        }

        events
    }

    fn create_message_start_event(&self) -> Bytes {
        let event = crate::models::anthropic::StreamEvent::MessageStart {
            message: crate::models::anthropic::MessageStartData {
                id: self.message_id.clone().unwrap_or_default(),
                message_type: "message".to_string(),
                role: "assistant".to_string(),
                model: self.current_model.clone().unwrap_or_default(),
                usage: crate::models::anthropic::Usage {
                    input_tokens: 0,
                    output_tokens: 0,
                },
            },
        };
        Bytes::from(self.sse_event(
            "message_start",
            &serde_json::to_value(&event).unwrap_or_default(),
        ))
    }

    fn handle_reasoning(&mut self, reasoning: &str) -> Vec<Bytes> {
        let mut events = Vec::new();

        // Close any non-thinking block before starting a thinking block
        if self.current_block_type.as_deref() != Some("thinking") {
            if self.current_block_type.is_some() {
                events.push(self.close_current_block());
                self.content_index += 1;
            }
        }

        if self.current_block_type.is_none() {
            let event = json!({
                "type": "content_block_start",
                "index": self.content_index,
                "content_block": { "type": "thinking", "thinking": "" }
            });
            events.push(Bytes::from(self.sse_event("content_block_start", &event)));
            self.current_block_type = Some("thinking".to_string());
        }

        let event = json!({
            "type": "content_block_delta",
            "index": self.content_index,
            "delta": { "type": "thinking_delta", "thinking": reasoning }
        });
        events.push(Bytes::from(self.sse_event("content_block_delta", &event)));

        events
    }

    fn handle_content(&mut self, content: &str) -> Vec<Bytes> {
        let mut events = Vec::new();

        if self.current_block_type.as_deref() != Some("text") {
            if self.current_block_type.is_some() {
                events.push(self.close_current_block());
                self.content_index += 1;
            }

            // Start text block
            let event = json!({
                "type": "content_block_start",
                "index": self.content_index,
                "content_block": { "type": "text", "text": "" }
            });
            events.push(Bytes::from(self.sse_event("content_block_start", &event)));
            self.current_block_type = Some("text".to_string());
        }

        // Send text delta
        let event = json!({
            "type": "content_block_delta",
            "index": self.content_index,
            "delta": { "type": "text_delta", "text": content }
        });
        events.push(Bytes::from(self.sse_event("content_block_delta", &event)));

        events
    }

    fn handle_tool_calls(&mut self, tool_calls: &[DeltaToolCall]) -> Vec<Bytes> {
        let mut events = Vec::new();

        for tool_call in tool_calls {
            if let Some(id) = &tool_call.id {
                if self.current_block_type.is_some() {
                    events.push(self.close_current_block());
                    self.content_index += 1;
                }
                self.tool_call_id = Some(id.clone());
                self.tool_call_args.clear();
            }

            if let Some(function) = &tool_call.function {
                if let Some(name) = &function.name {
                    self.tool_call_name = Some(name.clone());

                    let event = json!({
                        "type": "content_block_start",
                        "index": self.content_index,
                        "content_block": {
                            "type": "tool_use",
                            "id": self.tool_call_id.clone().unwrap_or_default(),
                            "name": name
                        }
                    });
                    events.push(Bytes::from(self.sse_event("content_block_start", &event)));
                    self.current_block_type = Some("tool_use".to_string());
                }

                if let Some(args) = &function.arguments {
                    // Skip empty arguments
                    if args.is_empty() {
                        continue;
                    }

                    // 1. Check if the incoming chunk is a complete JSON Object
                    if let Ok(_full_json) = serde_json::from_str::<serde_json::Value>(args) {
                        // It is a full object.

                        if self.tool_call_args.is_empty() {
                            // Case A: We have nothing buffered yet.
                            // Accept this full object as the first (and only) chunk.
                            self.tool_call_args.push_str(args);
                            events.push(self.emit_arg_delta(args));
                        } else {
                            // Case B: We are streaming. This might be a duplicate OR a completion.

                            // Check if the full object starts with what we have buffered so far.
                            if args.starts_with(&self.tool_call_args) {
                                // It is a continuation/completion!
                                // We only need to send the part we haven't sent yet.
                                let missing_part = &args[self.tool_call_args.len()..];

                                if !missing_part.is_empty() {
                                    self.tool_call_args.push_str(missing_part);
                                    events.push(self.emit_arg_delta(missing_part));
                                }
                                // If missing_part is empty, it was a true duplicate, so we do nothing.
                            } else {
                                // It doesn't start with our buffer.
                                // This is likely a duplicate with different formatting (e.g., spaces).
                                // We ignore it to prevent corruption.
                            }
                        }
                    } else {
                        // 2. It's a fragment (not a full object). Always accept.
                        self.tool_call_args.push_str(args);
                        events.push(self.emit_arg_delta(args));
                    }
                }
            }
        }

        events
    }

    // Helper to reduce code duplication
    fn emit_arg_delta(&self, partial_json: &str) -> Bytes {
        let event = json!({
            "type": "content_block_delta",
            "index": self.content_index,
            "delta": { "type": "input_json_delta", "partial_json": partial_json }
        });
        Bytes::from(self.sse_event("content_block_delta", &event))
    }

    fn handle_finish_reason(
        &mut self,
        finish_reason: &str,
        usage: Option<&crate::models::openai::Usage>,
    ) -> Vec<Bytes> {
        let mut events = Vec::new();

        if self.current_block_type.as_deref() == Some("tool_use") && !self.tool_call_args.is_empty()
        {
            let is_valid_json =
                serde_json::from_str::<serde_json::Value>(&self.tool_call_args).is_ok();

            if !is_valid_json {
                // If invalid, try to append a closing brace and check again.
                let potential_fix = format!("{} }}", self.tool_call_args.trim_end());

                if serde_json::from_str::<serde_json::Value>(&potential_fix).is_ok() {
                    // The missing brace fixes it! Emit the closing brace.
                    self.tool_call_args.push('}');

                    let event = json!({
                        "type": "content_block_delta",
                        "index": self.content_index,
                        "delta": { "type": "input_json_delta", "partial_json": "}" }
                    });
                    events.push(Bytes::from(self.sse_event("content_block_delta", &event)));
                }
            }
        }

        // Close current content block
        if self.current_block_type.is_some() {
            events.push(self.close_current_block());
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
        events.push(Bytes::from(self.sse_event("message_delta", &event)));

        events
    }

    fn close_current_block(&mut self) -> Bytes {
        let index = self.content_index;
        self.current_block_type = None;

        let event = json!({
            "type": "content_block_stop",
            "index": index
        });
        Bytes::from(self.sse_event("content_block_stop", &event))
    }
}

/// Passthrough stream that buffers complete SSE events before yielding
/// Use this when no transformation is needed (e.g., server-sent-events already in correct format)
pub fn sse_passthrough_stream(
    stream: impl Stream<Item = std::result::Result<Bytes, reqwest::Error>> + Send + 'static,
) -> impl Stream<Item = std::result::Result<Bytes, crate::error::Error>> + Send {
    async_stream::stream! {
        tokio::pin!(stream);
        let mut buffer = String::new();

        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(bytes) => {
                    let text = String::from_utf8_lossy(&bytes);
                    buffer.push_str(&text);

                    // Yield complete SSE events (delimited by \n\n)
                    while let Some(pos) = buffer.find("\n\n") {
                        let event = buffer[..pos].to_string();
                        buffer = buffer[pos + 2..].to_string();
                        if !event.trim().is_empty() {
                            yield Ok(Bytes::from(format!("{}\n\n", event)));
                        }
                    }
                }
                Err(e) => {
                    yield Err(crate::error::Error::Upstream(format!("Stream error: {}", e)));
                    break;
                }
            }
        }

        // Yield any remaining buffer at end of stream
        if !buffer.trim().is_empty() {
            yield Ok(Bytes::from(buffer));
        }
    }
}

/// Convert OpenAI SSE stream to Responses API SSE stream
pub fn sse_openai_to_responses_stream(
    stream: impl Stream<Item = std::result::Result<Bytes, reqwest::Error>> + Send + 'static,
) -> impl Stream<Item = std::result::Result<Bytes, crate::error::Error>> + Send {
    async_stream::stream! {
        tokio::pin!(stream);
        let mut buffer = String::new();

        // State variables to track the streaming process
        let mut response_created_sent = false;
        let mut response_in_progress_sent = false;
        let mut output_index = 0;
        let content_index = 0;
        let mut accumulated_content = String::new();
        let mut has_content_started = false;
        let mut response_id = format!("resp_{}", uuid::Uuid::new_v4());
        let item_id = format!("msg_{}", uuid::Uuid::new_v4());

        // Reasoning/thinking state
        let mut accumulated_thinking = String::new();
        let mut reasoning_started = false;
        let mut reasoning_done = false;
        let mut reasoning_item_id = String::new();
        let mut sequence_number = 0;

        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(bytes) => {
                    let text = String::from_utf8_lossy(&bytes);
                    buffer.push_str(&text);

                    // Process complete SSE events (delimited by \n\n)
                    while let Some(pos) = buffer.find("\n\n") {
                        let event = buffer[..pos].to_string();
                        buffer = buffer[pos + 2..].to_string();

                        if event.trim().is_empty() {
                            continue;
                        }

                        if event.starts_with("data: ") {
                            let data_str = event.strip_prefix("data: ").unwrap_or("").trim();

                            if data_str == "[DONE]" {
                                // Send completion event
                                let usage = json!({
                                    "input_tokens": 0,
                                    "output_tokens": accumulated_content.split_whitespace().count() as i32,
                                    "total_tokens": accumulated_content.split_whitespace().count() as i32,
                                    "input_tokens_details": {
                                        "cached_tokens": 0
                                    },
                                    "output_tokens_details": {
                                        "reasoning_tokens": if !accumulated_thinking.is_empty() { accumulated_thinking.split_whitespace().count() as i32 } else { 0 }
                                    }
                                });

                                let output = {
                                    let mut output_items = Vec::new();

                                    // Add reasoning item if present
                                    if !accumulated_thinking.is_empty() {
                                        output_items.push(json!({
                                            "id": reasoning_item_id,
                                            "type": "reasoning",
                                            "summary": [{
                                                "type": "summary_text",
                                                "text": accumulated_thinking
                                            }],
                                            "encrypted_content": accumulated_thinking
                                        }));
                                    }

                                    // Add content item if present
                                    if !accumulated_content.is_empty() {
                                        output_items.push(json!({
                                            "id": item_id,
                                            "type": "message",
                                            "status": "completed",
                                            "role": "assistant",
                                            "content": [{
                                                "type": "output_text",
                                                "text": accumulated_content,
                                                "annotations": [],
                                                "logprobs": []
                                            }]
                                        }));
                                    }

                                    output_items
                                };

                                let completed_event = json!({
                                    "response": {
                                        "id": response_id,
                                        "object": "response",
                                        "created_at": chrono::Utc::now().timestamp(),
                                        "completed_at": chrono::Utc::now().timestamp(),
                                        "status": "completed",
                                        "model": "unknown", // Will be updated from actual stream
                                        "output": output,
                                        "tools": [],
                                        "tool_choice": "auto",
                                        "truncation": "disabled",
                                        "parallel_tool_calls": true,
                                        "text": {
                                            "format": {
                                                "type": "text"
                                            }
                                        },
                                        "top_p": 1.0,
                                        "presence_penalty": 0.0,
                                        "frequency_penalty": 0.0,
                                        "top_logprobs": 0,
                                        "temperature": 1.0,
                                        "usage": usage,
                                        "max_output_tokens": null,
                                        "max_tool_calls": null,
                                        "store": false,
                                        "background": false,
                                        "service_tier": "default",
                                        "metadata": {},
                                        "safety_identifier": null,
                                        "prompt_cache_key": null,
                                        "reasoning": null
                                    },
                                    "type": "response.completed",
                                    "sequence_number": sequence_number
                                });
                                sequence_number += 1;

                                let sse_done = format!(
                                    "event: response.completed\n\ndata: {}\n\n",
                                    serde_json::to_string(&completed_event).unwrap_or_default()
                                );
                                yield Ok(Bytes::from(sse_done));
                                continue;
                            }

                            // Parse the OpenAI stream chunk
                            if let Ok(chunk) = serde_json::from_str::<crate::models::openai::StreamChunk>(data_str) {
                                // Update response_id and model from the chunk
                                response_id = chunk.id.clone();
                                let model = &chunk.model;

                                for choice in &chunk.choices {
                                    let delta = &choice.delta;

                                    // Handle reasoning (thinking)
                                    if let Some(reasoning) = &delta.reasoning {
                                        if !reasoning.is_empty() {
                                            // Start reasoning if not started yet
                                            if !reasoning_started {
                                                reasoning_started = true;
                                                reasoning_item_id = format!("rs_{}", uuid::Uuid::new_v4());

                                                // Send response.created event
                                                if !response_created_sent {
                                                    let init_event = json!({
                                                        "response": {
                                                            "id": response_id,
                                                            "object": "response",
                                                            "created_at": chrono::Utc::now().timestamp(),
                                                            "completed_at": null,
                                                            "status": "in_progress",
                                                            "model": model,
                                                            "output": [],
                                                            "tools": [],
                                                            "tool_choice": "auto",
                                                            "truncation": "disabled",
                                                            "parallel_tool_calls": true,
                                                            "text": {
                                                                "format": {
                                                                    "type": "text"
                                                                }
                                                            },
                                                            "top_p": 1.0,
                                                            "presence_penalty": 0.0,
                                                            "frequency_penalty": 0.0,
                                                            "top_logprobs": 0,
                                                            "temperature": 1.0,
                                                            "usage": {
                                                                "input_tokens": 0,
                                                                "output_tokens": 0,
                                                                "total_tokens": 0,
                                                                "input_tokens_details": {
                                                                    "cached_tokens": 0
                                                                },
                                                                "output_tokens_details": {
                                                                    "reasoning_tokens": 0
                                                                }
                                                            },
                                                            "max_output_tokens": null,
                                                            "max_tool_calls": null,
                                                            "store": false,
                                                            "background": false,
                                                            "service_tier": "default",
                                                            "metadata": {},
                                                            "safety_identifier": null,
                                                            "prompt_cache_key": null,
                                                            "reasoning": null
                                                        },
                                                        "type": "response.created",
                                                        "sequence_number": sequence_number
                                                    });
                                                    sequence_number += 1;

                                                    let sse_created = format!(
                                                        "event: response.created\n\ndata: {}\n\n",
                                                        serde_json::to_string(&init_event).unwrap_or_default()
                                                    );
                                                    yield Ok(Bytes::from(sse_created));
                                                    response_created_sent = true;
                                                }

                                                // Send response.in_progress event
                                                if !response_in_progress_sent {
                                                    let progress_event = json!({
                                                        "response": {
                                                            "id": response_id,
                                                            "object": "response",
                                                            "created_at": chrono::Utc::now().timestamp(),
                                                            "completed_at": null,
                                                            "status": "in_progress",
                                                            "model": model,
                                                            "output": [],
                                                            "tools": [],
                                                            "tool_choice": "auto",
                                                            "truncation": "disabled",
                                                            "parallel_tool_calls": true,
                                                            "text": {
                                                                "format": {
                                                                    "type": "text"
                                                                }
                                                            },
                                                            "top_p": 1.0,
                                                            "presence_penalty": 0.0,
                                                            "frequency_penalty": 0.0,
                                                            "top_logprobs": 0,
                                                            "temperature": 1.0,
                                                            "usage": {
                                                                "input_tokens": 0,
                                                                "output_tokens": 0,
                                                                "total_tokens": 0,
                                                                "input_tokens_details": {
                                                                    "cached_tokens": 0
                                                                },
                                                                "output_tokens_details": {
                                                                    "reasoning_tokens": 0
                                                                }
                                                            },
                                                            "max_output_tokens": null,
                                                            "max_tool_calls": null,
                                                            "store": false,
                                                            "background": false,
                                                            "service_tier": "default",
                                                            "metadata": {},
                                                            "safety_identifier": null,
                                                            "prompt_cache_key": null,
                                                            "reasoning": null
                                                        },
                                                        "type": "response.in_progress",
                                                        "sequence_number": sequence_number
                                                    });
                                                    sequence_number += 1;

                                                    let sse_progress = format!(
                                                        "event: response.in_progress\n\ndata: {}\n\n",
                                                        serde_json::to_string(&progress_event).unwrap_or_default()
                                                    );
                                                    yield Ok(Bytes::from(sse_progress));
                                                    response_in_progress_sent = true;
                                                }

                                                // Send output_item.added for reasoning
                                                let reasoning_item_added_event = json!({
                                                    "output_index": output_index,
                                                    "item": {
                                                        "id": reasoning_item_id,
                                                        "type": "reasoning",
                                                        "summary": []
                                                    },
                                                    "type": "response.output_item.added",
                                                    "sequence_number": sequence_number
                                                });
                                                sequence_number += 1;

                                                let sse_reasoning_added = format!(
                                                    "event: response.output_item.added\n\ndata: {}\n\n",
                                                    serde_json::to_string(&reasoning_item_added_event).unwrap_or_default()
                                                );
                                                yield Ok(Bytes::from(sse_reasoning_added));
                                            }

                                            // Accumulate thinking
                                            accumulated_thinking.push_str(reasoning);

                                            // Send reasoning delta
                                            let safe_reasoning = reasoning.replace('\n', "\\n").replace('\r', "\\r");
                                            let reasoning_delta_event = json!({
                                                "item_id": reasoning_item_id,
                                                "output_index": output_index,
                                                "summary_index": 0,
                                                "delta": safe_reasoning,
                                                "type": "response.reasoning_summary_text.delta",
                                                "sequence_number": sequence_number
                                            });
                                            sequence_number += 1;

                                            let sse_reasoning_delta = format!(
                                                "event: response.reasoning_summary_text.delta\n\ndata: {}\n\n",
                                                serde_json::to_string(&reasoning_delta_event).unwrap_or_default()
                                            );
                                            yield Ok(Bytes::from(sse_reasoning_delta));
                                        }
                                    }

                                    // Handle content
                                    if let Some(content) = &delta.content {
                                        if !content.is_empty() {
                                            // If reasoning was started but not completed, finish it first
                                            if reasoning_started && !reasoning_done {
                                                reasoning_done = true;

                                                // Send reasoning summary done
                                                let reasoning_done_event = json!({
                                                    "item_id": reasoning_item_id,
                                                    "output_index": output_index,
                                                    "summary_index": 0,
                                                    "text": accumulated_thinking,
                                                    "type": "response.reasoning_summary_text.done",
                                                    "sequence_number": sequence_number
                                                });
                                                sequence_number += 1;

                                                let sse_reasoning_done = format!(
                                                    "event: response.reasoning_summary_text.done\n\ndata: {}\n\n",
                                                    serde_json::to_string(&reasoning_done_event).unwrap_or_default()
                                                );
                                                yield Ok(Bytes::from(sse_reasoning_done));

                                                // Send reasoning output item done
                                                let reasoning_item_done_event = json!({
                                                    "output_index": output_index,
                                                    "item": {
                                                        "id": reasoning_item_id,
                                                        "type": "reasoning",
                                                        "summary": [{
                                                            "type": "summary_text",
                                                            "text": accumulated_thinking
                                                        }],
                                                        "encrypted_content": accumulated_thinking
                                                    },
                                                    "type": "response.output_item.done",
                                                    "sequence_number": sequence_number
                                                });
                                                sequence_number += 1;

                                                let sse_reasoning_item_done = format!(
                                                    "event: response.output_item.done\n\ndata: {}\n\n",
                                                    serde_json::to_string(&reasoning_item_done_event).unwrap_or_default()
                                                );
                                                yield Ok(Bytes::from(sse_reasoning_item_done));

                                                output_index += 1;
                                            }

                                            // Send initial events if not sent yet
                                            if !response_created_sent {
                                                let init_event = json!({
                                                    "response": {
                                                        "id": response_id,
                                                        "object": "response",
                                                        "created_at": chrono::Utc::now().timestamp(),
                                                        "completed_at": null,
                                                        "status": "in_progress",
                                                        "model": model,
                                                        "output": [],
                                                        "tools": [],
                                                        "tool_choice": "auto",
                                                        "truncation": "disabled",
                                                        "parallel_tool_calls": true,
                                                        "text": {
                                                            "format": {
                                                                "type": "text"
                                                            }
                                                        },
                                                        "top_p": 1.0,
                                                        "presence_penalty": 0.0,
                                                        "frequency_penalty": 0.0,
                                                        "top_logprobs": 0,
                                                        "temperature": 1.0,
                                                        "usage": {
                                                            "input_tokens": 0,
                                                            "output_tokens": 0,
                                                            "total_tokens": 0,
                                                            "input_tokens_details": {
                                                                "cached_tokens": 0
                                                            },
                                                            "output_tokens_details": {
                                                                "reasoning_tokens": 0
                                                            }
                                                        },
                                                        "max_output_tokens": null,
                                                        "max_tool_calls": null,
                                                        "store": false,
                                                        "background": false,
                                                        "service_tier": "default",
                                                        "metadata": {},
                                                        "safety_identifier": null,
                                                        "prompt_cache_key": null,
                                                        "reasoning": null
                                                    },
                                                    "type": "response.created",
                                                    "sequence_number": sequence_number
                                                });
                                                sequence_number += 1;

                                                let sse_created = format!(
                                                    "event: response.created\n\ndata: {}\n\n",
                                                    serde_json::to_string(&init_event).unwrap_or_default()
                                                );
                                                yield Ok(Bytes::from(sse_created));
                                                response_created_sent = true;
                                            }

                                            if !response_in_progress_sent {
                                                let progress_event = json!({
                                                    "response": {
                                                        "id": response_id,
                                                        "object": "response",
                                                        "created_at": chrono::Utc::now().timestamp(),
                                                        "completed_at": null,
                                                        "status": "in_progress",
                                                        "model": model,
                                                        "output": [],
                                                        "tools": [],
                                                        "tool_choice": "auto",
                                                        "truncation": "disabled",
                                                        "parallel_tool_calls": true,
                                                        "text": {
                                                            "format": {
                                                                "type": "text"
                                                            }
                                                        },
                                                        "top_p": 1.0,
                                                        "presence_penalty": 0.0,
                                                        "frequency_penalty": 0.0,
                                                        "top_logprobs": 0,
                                                        "temperature": 1.0,
                                                        "usage": {
                                                            "input_tokens": 0,
                                                            "output_tokens": 0,
                                                            "total_tokens": 0,
                                                            "input_tokens_details": {
                                                                "cached_tokens": 0
                                                            },
                                                            "output_tokens_details": {
                                                                "reasoning_tokens": 0
                                                            }
                                                        },
                                                        "max_output_tokens": null,
                                                        "max_tool_calls": null,
                                                        "store": false,
                                                        "background": false,
                                                        "service_tier": "default",
                                                        "metadata": {},
                                                        "safety_identifier": null,
                                                        "prompt_cache_key": null,
                                                        "reasoning": null
                                                    },
                                                    "type": "response.in_progress",
                                                    "sequence_number": sequence_number
                                                });
                                                sequence_number += 1;

                                                let sse_progress = format!(
                                                    "event: response.in_progress\n\ndata: {}\n\n",
                                                    serde_json::to_string(&progress_event).unwrap_or_default()
                                                );
                                                yield Ok(Bytes::from(sse_progress));
                                                response_in_progress_sent = true;
                                            }

                                            // Emit content events
                                            if !has_content_started {
                                                has_content_started = true;

                                                // response.output_item.added
                                                let item_added_event = json!({
                                                    "output_index": output_index,
                                                    "item": {
                                                        "id": item_id,
                                                        "type": "message",
                                                        "status": "in_progress",
                                                        "role": "assistant",
                                                        "content": []
                                                    },
                                                    "type": "response.output_item.added",
                                                    "sequence_number": sequence_number
                                                });
                                                sequence_number += 1;

                                                let sse_item_added = format!(
                                                    "event: response.output_item.added\n\ndata: {}\n\n",
                                                    serde_json::to_string(&item_added_event).unwrap_or_default()
                                                );
                                                yield Ok(Bytes::from(sse_item_added));

                                                // response.content_part.added
                                                let part_added_event = json!({
                                                    "item_id": item_id,
                                                    "output_index": output_index,
                                                    "content_index": content_index,
                                                    "part": {
                                                        "type": "output_text",
                                                        "text": "",
                                                        "annotations": [],
                                                        "logprobs": []
                                                    },
                                                    "type": "response.content_part.added",
                                                    "sequence_number": sequence_number
                                                });
                                                sequence_number += 1;

                                                let sse_part_added = format!(
                                                    "event: response.content_part.added\n\ndata: {}\n\n",
                                                    serde_json::to_string(&part_added_event).unwrap_or_default()
                                                );
                                                yield Ok(Bytes::from(sse_part_added));
                                            }

                                            // response.output_text.delta
                                            let safe_content = content.replace('\n', "\\n").replace('\r', "\\r");
                                            let delta_event = json!({
                                                "item_id": item_id,
                                                "output_index": output_index,
                                                "content_index": 0,
                                                "delta": safe_content,
                                                "logprobs": [],
                                                "type": "response.output_text.delta",
                                                "sequence_number": sequence_number
                                            });
                                            sequence_number += 1;

                                            let sse_delta = format!(
                                                "event: response.output_text.delta\n\ndata: {}\n\n",
                                                serde_json::to_string(&delta_event).unwrap_or_default()
                                            );
                                            yield Ok(Bytes::from(sse_delta));

                                            accumulated_content.push_str(content);
                                        }
                                    }

                                    // Handle tool calls
                                    if let Some(tool_calls) = &delta.tool_calls {
                                        // If reasoning was started but not completed, finish it first
                                        if reasoning_started && !reasoning_done {
                                            reasoning_done = true;

                                            // Send reasoning summary done
                                            let reasoning_done_event = json!({
                                                "item_id": reasoning_item_id,
                                                "output_index": output_index,
                                                "summary_index": 0,
                                                "text": accumulated_thinking,
                                                "type": "response.reasoning_summary_text.done",
                                                "sequence_number": sequence_number
                                            });
                                            sequence_number += 1;

                                            let sse_reasoning_done = format!(
                                                "event: response.reasoning_summary_text.done\n\ndata: {}\n\n",
                                                serde_json::to_string(&reasoning_done_event).unwrap_or_default()
                                            );
                                            yield Ok(Bytes::from(sse_reasoning_done));

                                            // Send reasoning output item done
                                            let reasoning_item_done_event = json!({
                                                "output_index": output_index,
                                                "item": {
                                                    "id": reasoning_item_id,
                                                    "type": "reasoning",
                                                    "summary": [{
                                                        "type": "summary_text",
                                                        "text": accumulated_thinking
                                                    }],
                                                    "encrypted_content": accumulated_thinking
                                                },
                                                "type": "response.output_item.done",
                                                "sequence_number": sequence_number
                                            });
                                            sequence_number += 1;

                                            let sse_reasoning_item_done = format!(
                                                "event: response.output_item.done\n\ndata: {}\n\n",
                                                serde_json::to_string(&reasoning_item_done_event).unwrap_or_default()
                                            );
                                            yield Ok(Bytes::from(sse_reasoning_item_done));

                                            output_index += 1;
                                        }

                                        for tool_call in tool_calls {
                                            let call_id = tool_call.id.clone().unwrap_or_else(|| format!("call_{}", uuid::Uuid::new_v4()));
                                            let name = tool_call.function.as_ref().and_then(|f| f.name.clone()).unwrap_or_default();
                                            let arguments = tool_call.function.as_ref().and_then(|f| f.arguments.clone()).unwrap_or_default();

                                            // response.output_item.added for function call
                                            let item_added_event = json!({
                                                "output_index": output_index,
                                                "item": {
                                                    "id": format!("fc_{}", call_id),
                                                    "type": "function_call",
                                                    "status": "in_progress",
                                                    "call_id": call_id,
                                                    "name": name,
                                                    "arguments": ""
                                                },
                                                "type": "response.output_item.added",
                                                "sequence_number": sequence_number
                                            });
                                            sequence_number += 1;

                                            let sse_item_added = format!(
                                                "event: response.output_item.added\n\ndata: {}\n\n",
                                                serde_json::to_string(&item_added_event).unwrap_or_default()
                                            );
                                            yield Ok(Bytes::from(sse_item_added));

                                            // response.function_call_arguments.delta if present
                                            if !arguments.is_empty() {
                                                // Escape any newlines in the arguments to prevent breaking SSE format
                                                let safe_arguments = arguments.replace('\n', "\\n").replace('\r', "\\r");
                                                let args_delta_event = json!({
                                                    "item_id": format!("fc_{}", call_id),
                                                    "output_index": output_index,
                                                    "delta": safe_arguments,
                                                    "type": "response.function_call_arguments.delta",
                                                    "sequence_number": sequence_number
                                                });
                                                sequence_number += 1;

                                                let sse_args_delta = format!(
                                                    "event: response.function_call_arguments.delta\n\ndata: {}\n\n",
                                                    serde_json::to_string(&args_delta_event).unwrap_or_default()
                                                );
                                                yield Ok(Bytes::from(sse_args_delta));
                                            }

                                            // response.function_call_arguments.done
                                            let args_done_event = json!({
                                                "item_id": format!("fc_{}", call_id),
                                                "output_index": output_index,
                                                "arguments": arguments,
                                                "type": "response.function_call_arguments.done",
                                                "sequence_number": sequence_number
                                            });
                                            sequence_number += 1;

                                            let sse_args_done = format!(
                                                "event: response.function_call_arguments.done\n\ndata: {}\n\n",
                                                serde_json::to_string(&args_done_event).unwrap_or_default()
                                            );
                                            yield Ok(Bytes::from(sse_args_done));

                                            // response.output_item.done
                                            let item_done_event = json!({
                                                "output_index": output_index,
                                                "item": {
                                                    "id": format!("fc_{}", call_id),
                                                    "type": "function_call",
                                                    "status": "completed",
                                                    "call_id": call_id,
                                                    "name": name,
                                                    "arguments": arguments
                                                },
                                                "type": "response.output_item.done",
                                                "sequence_number": sequence_number
                                            });
                                            sequence_number += 1;

                                            let sse_item_done = format!(
                                                "event: response.output_item.done\n\ndata: {}\n\n",
                                                serde_json::to_string(&item_done_event).unwrap_or_default()
                                            );
                                            yield Ok(Bytes::from(sse_item_done));

                                            output_index += 1;
                                        }
                                    }

                                    // Handle finish reason - send completion events
                                    if let Some(finish_reason) = &choice.finish_reason {
                                        // If reasoning was started but not completed, finish it first
                                        if reasoning_started && !reasoning_done {
                                            reasoning_done = true;

                                            // Send reasoning summary done
                                            let reasoning_done_event = json!({
                                                "item_id": reasoning_item_id,
                                                "output_index": output_index,
                                                "summary_index": 0,
                                                "text": accumulated_thinking,
                                                "type": "response.reasoning_summary_text.done",
                                                "sequence_number": sequence_number
                                            });
                                            sequence_number += 1;

                                            let sse_reasoning_done = format!(
                                                "event: response.reasoning_summary_text.done\n\ndata: {}\n\n",
                                                serde_json::to_string(&reasoning_done_event).unwrap_or_default()
                                            );
                                            yield Ok(Bytes::from(sse_reasoning_done));

                                            // Send reasoning output item done
                                            let reasoning_item_done_event = json!({
                                                "output_index": output_index,
                                                "item": {
                                                    "id": reasoning_item_id,
                                                    "type": "reasoning",
                                                    "summary": [{
                                                        "type": "summary_text",
                                                        "text": accumulated_thinking
                                                    }],
                                                    "encrypted_content": accumulated_thinking
                                                },
                                                "type": "response.output_item.done",
                                                "sequence_number": sequence_number
                                            });
                                            sequence_number += 1;

                                            let sse_reasoning_item_done = format!(
                                                "event: response.output_item.done\n\ndata: {}\n\n",
                                                serde_json::to_string(&reasoning_item_done_event).unwrap_or_default()
                                            );
                                            yield Ok(Bytes::from(sse_reasoning_item_done));

                                            output_index += 1;
                                        }

                                        // response.output_text.done (if we had content)
                                        if !accumulated_content.is_empty() {
                                            let safe_accumulated_content = accumulated_content.replace('\n', "\\n").replace('\r', "\\r");
                                            let content_done_event = json!({
                                                "item_id": item_id,
                                                "output_index": output_index,
                                                "content_index": 0,
                                                "text": safe_accumulated_content,
                                                "logprobs": [],
                                                "type": "response.output_text.done",
                                                "sequence_number": sequence_number
                                            });
                                            sequence_number += 1;

                                            let sse_content_done = format!(
                                                "event: response.output_text.done\n\ndata: {}\n\n",
                                                serde_json::to_string(&content_done_event).unwrap_or_default()
                                            );
                                            yield Ok(Bytes::from(sse_content_done));

                                            // response.content_part.done
                                            let part_done_event = json!({
                                                "item_id": item_id,
                                                "output_index": output_index,
                                                "content_index": 0,
                                                "part": {
                                                    "type": "output_text",
                                                    "text": safe_accumulated_content,
                                                    "annotations": [],
                                                    "logprobs": []
                                                },
                                                "type": "response.content_part.done",
                                                "sequence_number": sequence_number
                                            });
                                            sequence_number += 1;

                                            let sse_part_done = format!(
                                                "event: response.content_part.done\n\ndata: {}\n\n",
                                                serde_json::to_string(&part_done_event).unwrap_or_default()
                                            );
                                            yield Ok(Bytes::from(sse_part_done));

                                            // response.output_item.done
                                            let item_done_event = json!({
                                                "output_index": output_index,
                                                "item": {
                                                    "id": item_id,
                                                    "type": "message",
                                                    "status": "completed",
                                                    "role": "assistant",
                                                    "content": [{
                                                        "type": "output_text",
                                                        "text": accumulated_content,
                                                        "annotations": [],
                                                        "logprobs": []
                                                    }]
                                                },
                                                "type": "response.output_item.done",
                                                "sequence_number": sequence_number
                                            });
                                            sequence_number += 1;

                                            let sse_item_done = format!(
                                                "event: response.output_item.done\n\ndata: {}\n\n",
                                                serde_json::to_string(&item_done_event).unwrap_or_default()
                                            );
                                            yield Ok(Bytes::from(sse_item_done));
                                        }

                                        // Final completion event with finish reason
                                        let usage = json!({
                                            "input_tokens": 0,
                                            "output_tokens": accumulated_content.split_whitespace().count() as i32,
                                            "total_tokens": accumulated_content.split_whitespace().count() as i32,
                                            "input_tokens_details": {
                                                "cached_tokens": 0
                                            },
                                            "output_tokens_details": {
                                                "reasoning_tokens": if !accumulated_thinking.is_empty() { accumulated_thinking.split_whitespace().count() as i32 } else { 0 }
                                            }
                                        });

                                        let completed_event = json!({
                                            "response": {
                                                "id": response_id,
                                                "object": "response",
                                                "created_at": chrono::Utc::now().timestamp(),
                                                "completed_at": chrono::Utc::now().timestamp(),
                                                "status": "completed",
                                                "model": model,
                                                "output": {
                                                    "id": item_id,
                                                    "type": "message",
                                                    "status": "completed",
                                                    "role": "assistant",
                                                    "content": [{
                                                        "type": "output_text",
                                                        "text": accumulated_content,
                                                        "annotations": [],
                                                        "logprobs": []
                                                    }]
                                                },
                                                "tools": [],
                                                "tool_choice": "auto",
                                                "truncation": "disabled",
                                                "parallel_tool_calls": true,
                                                "text": {
                                                    "format": {
                                                        "type": "text"
                                                    }
                                                },
                                                "top_p": 1.0,
                                                "presence_penalty": 0.0,
                                                "frequency_penalty": 0.0,
                                                "top_logprobs": 0,
                                                "temperature": 1.0,
                                                "usage": usage,
                                                "max_output_tokens": null,
                                                "max_tool_calls": null,
                                                "store": false,
                                                "background": false,
                                                "service_tier": "default",
                                                "metadata": {},
                                                "safety_identifier": null,
                                                "prompt_cache_key": null,
                                                "reasoning": null
                                            },
                                            "type": "response.completed",
                                            "sequence_number": sequence_number
                                        });
                                        sequence_number += 1;

                                        let sse_completed = format!(
                                            "event: response.completed\n\ndata: {}\n\n",
                                            serde_json::to_string(&completed_event).unwrap_or_default()
                                        );
                                        yield Ok(Bytes::from(sse_completed));
                                    }
                                }
                            } else {
                                // If parsing fails, log the error but continue processing other chunks
                                tracing::debug!("Failed to parse OpenAI stream chunk: {}", data_str);
                            }
                        }

                        // Forward any other SSE events as-is (but we should handle them properly)
                        if !event.starts_with("data: [DONE]") && !event.trim().is_empty() {
                            // Just continue processing
                        }
                    }
                }
                Err(e) => {
                    yield Err(crate::error::Error::Upstream(format!("Stream error: {}", e)));
                    break;
                }
            }
        }

        // Yield any remaining buffer at end of stream
        if !buffer.trim().is_empty() {
            yield Ok(Bytes::from(buffer));
        }
    }
}

pub fn sse_openai_to_anthropic_stream(
    stream: impl Stream<Item = std::result::Result<Bytes, reqwest::Error>> + Send + 'static,
) -> impl Stream<Item = std::result::Result<Bytes, crate::error::Error>> + Send {
    async_stream::stream! {
        let mut builder = SseStreamBuilder::new();
        tokio::pin!(stream);

        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(bytes) => {
                    let text = String::from_utf8_lossy(&bytes);
                    builder.buffer.push_str(&text);

                    while let Some(pos) = builder.buffer.find("\n\n") {
                        let line = builder.buffer[..pos].to_string();
                        builder.buffer = builder.buffer[pos + 2..].to_string();

                        if line.trim().is_empty() {
                            continue;
                        }

                        for l in line.lines() {
                            let events = builder.process_line(l);
                            for event in events {
                                // println!("{:?}", event);
                                yield Ok(event);
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("Stream error: {}", e);
                    let error_event = json!({
                        "type": "error",
                        "error": { "type": "stream_error", "message": format!("Stream error: {}", e) }
                    });
                    let sse = format!("event: error\ndata: {}\n\n",
                        serde_json::to_string(&error_event).unwrap_or_default());
                    yield Ok(Bytes::from(sse));
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::openai::{Delta, DeltaFunctionCall, StreamChoice, StreamChunk};
    use futures::stream::{self};

    #[test]
    fn test_content_and_tool_calls_same_chunk() {
        let mut builder = SseStreamBuilder::new();

        let chunk = StreamChunk {
            id: "chatcmpl-test".to_string(),
            object: "chat.completion.chunk".to_string(),
            created: 1234567890,
            model: "gpt-4".to_string(),
            choices: vec![StreamChoice {
                index: 0,
                delta: Delta {
                    role: Some("assistant".to_string()),
                    content: Some("hello".to_string()),
                    tool_calls: Some(vec![DeltaToolCall {
                        index: 0,
                        id: Some("call_123".to_string()),
                        call_type: Some("function".to_string()),
                        function: Some(DeltaFunctionCall {
                            name: Some("get_weather".to_string()),
                            arguments: Some("{}".to_string()),
                        }),
                    }]),
                    reasoning: None,
                },
                finish_reason: None,
            }],
            usage: None,
        };

        let data = serde_json::to_string(&chunk).unwrap();
        let parsed = serde_json::from_str::<StreamChunk>(&data).unwrap();

        // Process content first, then tool_calls (matching actual order)
        let choice = parsed.choices.first().unwrap();
        let content_events = builder.handle_content(choice.delta.content.as_deref().unwrap());
        let tool_events = builder.handle_tool_calls(choice.delta.tool_calls.as_deref().unwrap());

        // Verify text block was at index, tool at index
        let content_str = String::from_utf8_lossy(&content_events[0]);
        // tool_events[0] is the close event for text block, tool starts at index
        let tool_start_str = String::from_utf8_lossy(&tool_events[1]);
        assert!(
            content_str.contains(r#""index":0"#),
            "content should be at index: {}",
            content_str
        );
        assert!(
            tool_start_str.contains(r#""index":1"#),
            "tool should be at index: {}",
            tool_start_str
        );
    }

    #[test]
    fn test_skip_empty_tool_arguments() {
        let mut builder = SseStreamBuilder::new();

        // Simulate tool call chunk with empty arguments (like OpenRouter sends)
        let chunk = StreamChunk {
            id: "chatcmpl-test".to_string(),
            object: "chat.completion.chunk".to_string(),
            created: 0,
            model: "gpt-4".to_string(),
            choices: vec![StreamChoice {
                index: 0,
                delta: Delta {
                    role: Some("assistant".to_string()),
                    content: None,
                    tool_calls: Some(vec![DeltaToolCall {
                        index: 0,
                        id: Some("call_123".to_string()),
                        call_type: Some("function".to_string()),
                        function: Some(DeltaFunctionCall {
                            name: Some("bash".to_string()),
                            arguments: Some("".to_string()),
                        }),
                    }]),
                    reasoning: None,
                },
                finish_reason: None,
            }],
            usage: None,
        };

        let data = serde_json::to_string(&chunk).unwrap();
        let parsed = serde_json::from_str::<StreamChunk>(&data).unwrap();
        let choice = parsed.choices.first().unwrap();
        let tool_events = builder.handle_tool_calls(choice.delta.tool_calls.as_deref().unwrap());

        // Should generate content_block_start but no arg_delta for empty args
        assert_eq!(tool_events.len(), 1, "Should only have content_block_start");
        let tool_start_str = String::from_utf8_lossy(&tool_events[0]);
        assert!(tool_start_str.contains(r#""type":"content_block_start""#));
        assert!(!tool_start_str.contains("input_json_delta"));
    }

    #[ignore]
    #[tokio::test]
    async fn test_sse_malformed_json() {
        let chunks: Vec<Result<Bytes, reqwest::Error>> = vec![
            Ok(Bytes::from(
                r#"data: {"id":"chatcmpl-edb484bd-c952-4dc0-bff6","object":"chat.completion.chunk","created":1774432858,"model":"zai-org/GLM-4.7","choices":[{"index":0,"delta":{"reasoning":" might want"},"logprobs":null,"finish_reason":null,"token_ids":null}],"usage":{"prompt_tokens":27403,"total_tokens":27523,"completion_tokens":120}}"#,
            )),
            Ok(Bytes::from(
                r#"data: {"id":"chatcmpl-edb484bd-c952-4dc0-bff6","object":"chat.completion.chunk","created":1774432858,"model":"zai-org/GLM-4.7","choices":[{"index":0,"delta":{"reasoning":" apply it."},"logprobs":null,"finish_reason":null,"token_ids":null}],"usage":{"prompt_tokens":27403,"total_tokens":27528,"completion_tokens":125}}"#,
            )),
            Ok(Bytes::from(
                r#"data: {"id":"chatcmpl-be8688b5-c092e9fd2","object":"chat.completion.chunk","created":17
  70,"model":"zai-org/GLM.7","choices":[{"index":0,"delta":{"reasoning":" they
  want"},"logprobs":null,"finish_reason":null,"token_ids":null}],"usage":{"prompt_tokens":21612,"total_tokens
  ":21777,"completion_tokens":165}}\n\n"#,
            )),
        ];

        let stream = stream::iter(chunks);
        let result = sse_openai_to_anthropic_stream(stream).inspect(|result| {
            if let Ok(bytes) = result {
                eprintln!("[Output] {}", String::from_utf8_lossy(bytes).trim());
            }
        });

        // Collect all items from the stream
        let items: Vec<Result<Bytes, crate::error::Error>> = result.collect().await;

        // Extract successful bytes
        let contents: Vec<String> = items
            .into_iter()
            .filter_map(|r| r.ok())
            .map(|bytes| String::from_utf8_lossy(&bytes).to_string())
            .collect();

        // Assert on the collected content
        assert_eq!(contents, vec![""]);
    }
}
