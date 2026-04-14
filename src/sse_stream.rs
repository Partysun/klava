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
                logprobs: None,
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
                logprobs: None,
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
