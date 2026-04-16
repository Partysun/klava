use crate::models::openai::{DeltaToolCall, StreamChunk};
use crate::stream_converter::ChatChunkConverter;
use serde_json::{Value, json};

/// Event produced by the stream converter
#[derive(Debug, Clone)]
pub struct AnthropicStreamEvent {
    pub event_type: String,
    pub data: Value,
}

/// State machine for the converter
#[derive(Debug, Clone)]
enum ConverterState {
    Initial,
    MessageInProgress,
    Reasoning,
    Text,
    ToolUse,
    Completed,
}

/// Anthropic SSE stream converter
///
/// Converts OpenAI ChatCompletion SSE chunks into Anthropic Messages API SSE events.
/// Implements `ChatChunkConverter` so it plugs directly into `UniversalConverter`.
pub struct AnthropicStreamConverter {
    // Configuration
    message_id: Option<String>,
    model: Option<String>,

    // State machine
    state: ConverterState,
    content_index: usize,

    // Tool call state
    tool_call_args: String,

    // Flags
    completed: bool,
}

impl AnthropicStreamConverter {
    pub fn new() -> Self {
        Self {
            message_id: None,
            model: None,
            state: ConverterState::Initial,
            content_index: 0,
            tool_call_args: String::new(),
            completed: false,
        }
    }

    // -----------------------------------------------------------------------
    // State transition helpers
    // -----------------------------------------------------------------------

    fn transition_to(&mut self, new_state: ConverterState) {
        self.state = new_state;
    }

    // -----------------------------------------------------------------------
    // Event construction
    // -----------------------------------------------------------------------

    fn create_event(&self, event_type: &str, data: Value) -> AnthropicStreamEvent {
        AnthropicStreamEvent {
            event_type: event_type.to_string(),
            data,
        }
    }

    // -----------------------------------------------------------------------
    // Phase: Message start
    // -----------------------------------------------------------------------

    fn create_message_start(&mut self, chunk: &StreamChunk) -> Vec<AnthropicStreamEvent> {
        let mut events = Vec::new();

        if matches!(self.state, ConverterState::Initial) {
            // Initialize from first chunk
            if self.message_id.is_none() {
                self.message_id = Some(chunk.id.clone());
            }
            if self.model.is_none() {
                self.model = Some(chunk.model.clone());
            }

            let message_data = json!({
                "id": self.message_id.as_ref().unwrap(),
                "type": "message",
                "role": "assistant",
                "model": self.model.as_ref().unwrap(),
                "usage": {
                    "input_tokens": 0,
                    "output_tokens": 0
                }
            });

            events.push(self.create_event("message_start", json!({ "message": message_data })));

            self.transition_to(ConverterState::MessageInProgress);
        }

        events
    }

    // -----------------------------------------------------------------------
    // Phase: Reasoning
    // -----------------------------------------------------------------------

    fn process_reasoning_delta(&mut self, reasoning: &str) -> Vec<AnthropicStreamEvent> {
        let mut events = Vec::new();

        if !matches!(self.state, ConverterState::Reasoning) {
            // Start reasoning block
            events.push(self.create_event(
                "content_block_start",
                json!({
                    "type": "content_block_start",
                    "index": self.content_index,
                    "content_block": {
                        "type": "thinking",
                        "thinking": ""
                    }
                }),
            ));
            self.transition_to(ConverterState::Reasoning);
        }

        // Emit reasoning delta
        events.push(self.create_event(
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": self.content_index,
                "delta": {
                    "type": "thinking_delta",
                    "thinking": reasoning
                }
            }),
        ));

        events
    }

    fn finish_reasoning(&mut self) -> Vec<AnthropicStreamEvent> {
        if !matches!(self.state, ConverterState::Reasoning) {
            return Vec::new();
        }

        let mut events = Vec::new();

        // Close reasoning block
        events.push(self.create_event(
            "content_block_stop",
            json!({
                "type": "content_block_stop",
                "index": self.content_index,
            }),
        ));

        self.content_index += 1;
        self.transition_to(ConverterState::MessageInProgress);

        events
    }

    // -----------------------------------------------------------------------
    // Phase: Text content
    // -----------------------------------------------------------------------

    fn process_content_delta(&mut self, content: &str) -> Vec<AnthropicStreamEvent> {
        let mut events = Vec::new();

        if !matches!(self.state, ConverterState::Text) {
            // Start text block
            events.push(self.create_event(
                "content_block_start",
                json!({
                    "type": "content_block_start",
                    "index": self.content_index,
                    "content_block": {
                        "type": "text",
                        "text": ""
                    }
                }),
            ));
            self.transition_to(ConverterState::Text);
        }

        // Emit text delta
        events.push(self.create_event(
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": self.content_index,
                "delta": {
                    "type": "text_delta",
                    "text": content
                }
            }),
        ));

        events
    }

    fn finish_content(&mut self) -> Vec<AnthropicStreamEvent> {
        if !matches!(self.state, ConverterState::Text) {
            return Vec::new();
        }

        let mut events = Vec::new();

        // Close text block
        events.push(self.create_event(
            "content_block_stop",
            json!({
                "type": "content_block_stop",
                "index": self.content_index,
            }),
        ));

        self.content_index += 1;
        self.transition_to(ConverterState::MessageInProgress);

        events
    }

    // -----------------------------------------------------------------------
    // Phase: Tool calls
    // -----------------------------------------------------------------------

    fn process_tool_call_delta(&mut self, tc: &DeltaToolCall) -> Vec<AnthropicStreamEvent> {
        let mut events = Vec::new();

        // New tool call starting (has id)
        if tc.id.is_some() {
            let id = tc.id.clone().unwrap();
            let name = tc
                .function
                .as_ref()
                .and_then(|f| f.name.clone())
                .unwrap_or_default();

            // Start tool_use block
            events.push(self.create_event(
                "content_block_start",
                json!({
                    "type": "content_block_start",
                    "index": self.content_index,
                    "content_block": {
                        "type": "tool_use",
                        "id": id,
                        "name": name
                    }
                }),
            ));

            self.tool_call_args.clear();
            self.transition_to(ConverterState::ToolUse);
        }

        // Accumulate arguments (with duplicate detection for vLLM/CloudRu)
        if let Some(ref args) = tc.function.as_ref().and_then(|f| f.arguments.as_ref()) {
            // Skip empty arguments
            if args.is_empty() {
                return events;
            }

            if let ConverterState::ToolUse = &self.state {
                let delta_to_emit: Option<String> = {
                    let is_complete_json = serde_json::from_str::<serde_json::Value>(args).is_ok();

                    if is_complete_json {
                        if self.tool_call_args.is_empty() {
                            // Case A: Nothing buffered yet — accept full object as first chunk
                            Some(args.to_string())
                        } else if args.starts_with(&self.tool_call_args) {
                            // Case B: Continuation/completion — emit only the missing suffix
                            let missing = &args[self.tool_call_args.len()..];
                            if missing.is_empty() {
                                None // True duplicate, skip
                            } else {
                                Some(missing.to_string())
                            }
                        } else {
                            // Duplicate with different formatting — ignore
                            None
                        }
                    } else {
                        // Fragment (not a full object) — always accept
                        Some(args.to_string())
                    }
                };

                if let Some(delta) = delta_to_emit {
                    self.tool_call_args.push_str(&delta);

                    events.push(self.create_event(
                        "content_block_delta",
                        json!({
                            "type": "content_block_delta",
                            "index": self.content_index,
                            "delta": {
                                "type": "input_json_delta",
                                "partial_json": delta
                            }
                        }),
                    ));
                }
            }
        }

        events
    }

    fn finish_tool_calls(&mut self) -> Vec<AnthropicStreamEvent> {
        if !matches!(self.state, ConverterState::ToolUse) {
            return Vec::new();
        }

        let mut events = Vec::new();

        // Repair incomplete JSON arguments (vLLM/CloudRu sometimes sends
        // tool call args without a closing brace).
        if !self.tool_call_args.is_empty() {
            let is_valid_json =
                serde_json::from_str::<serde_json::Value>(&self.tool_call_args).is_ok();

            if !is_valid_json {
                let potential_fix = format!("{} }}", self.tool_call_args.trim_end());

                if serde_json::from_str::<serde_json::Value>(&potential_fix).is_ok() {
                    // The missing brace fixes it! Emit the closing brace.
                    self.tool_call_args.push('}');

                    events.push(self.create_event(
                        "content_block_delta",
                        json!({
                            "type": "content_block_delta",
                            "index": self.content_index,
                            "delta": {
                                "type": "input_json_delta",
                                "partial_json": "}"
                            }
                        }),
                    ));
                }
            }
        }

        // Close tool_use block
        events.push(self.create_event(
            "content_block_stop",
            json!({
                "type": "content_block_stop",
                "index": self.content_index,
            }),
        ));

        self.content_index += 1;
        self.transition_to(ConverterState::MessageInProgress);

        events
    }

    // -----------------------------------------------------------------------
    // Completion
    // -----------------------------------------------------------------------

    fn process_completion(&mut self, chunk: &StreamChunk) -> Vec<AnthropicStreamEvent> {
        if self.completed {
            return Vec::new();
        }
        self.completed = true;

        let mut events = Vec::new();

        // Close any open phase
        events.extend(self.finish_reasoning());
        events.extend(self.finish_content());
        events.extend(self.finish_tool_calls());

        // Send message_delta with stop_reason
        let stop_reason = chunk
            .choices
            .first()
            .and_then(|c| c.finish_reason.as_ref())
            .map(|r| match r.as_str() {
                "tool_calls" => "tool_use",
                "stop" => "end_turn",
                "length" => "max_tokens",
                _ => "end_turn",
            })
            .unwrap_or("end_turn");

        let usage = chunk.usage.as_ref().map(|u| {
            json!({
                "output_tokens": u.completion_tokens
            })
        });

        events.push(self.create_event(
            "message_delta",
            json!({
                "type": "message_delta",
                "delta": {
                    "stop_reason": stop_reason,
                    "stop_sequence": Value::Null
                },
                "usage": usage
            }),
        ));

        events.push(self.create_event("message_stop", json!({ "type": "message_stop" })));

        self.transition_to(ConverterState::Completed);

        events
    }
}

impl ChatChunkConverter for AnthropicStreamConverter {
    type OutputEvent = AnthropicStreamEvent;

    fn is_completed(&self) -> bool {
        self.completed
    }

    fn process(&mut self, chunk: &StreamChunk) -> Vec<AnthropicStreamEvent> {
        let mut events = Vec::new();

        // Initialize state from first chunk
        events.extend(self.create_message_start(chunk));

        for choice in &chunk.choices {
            let delta = &choice.delta;

            //. Reasoning (must complete before other content)
            if let Some(reasoning) = &delta.reasoning {
                if !reasoning.is_empty() {
                    events.extend(self.process_reasoning_delta(reasoning));
                }
            }

            //. Content text
            if let Some(content) = &delta.content {
                if !content.is_empty() {
                    // Finish reasoning first
                    events.extend(self.finish_reasoning());

                    events.extend(self.process_content_delta(content));
                }
            }

            //. Tool calls
            if let Some(tool_calls) = &delta.tool_calls {
                for tc in tool_calls {
                    // Finish reasoning/content first
                    events.extend(self.finish_reasoning());
                    events.extend(self.finish_content());

                    events.extend(self.process_tool_call_delta(tc));
                }
            }

            //. Finish
            if choice.finish_reason.is_some() {
                events.extend(self.process_completion(chunk));
                break;
            }
        }

        events
    }

    fn serialize(&self, event: &AnthropicStreamEvent) -> Vec<u8> {
        let json_str = serde_json::to_string(&event.data).unwrap_or_default();
        format!("event: {}\ndata: {}\n\n", event.event_type, json_str).into_bytes()
    }

    fn finalize(&mut self) -> Vec<AnthropicStreamEvent> {
        let mut events = Vec::new();

        // Safety net: close any open phases
        events.extend(self.finish_reasoning());
        events.extend(self.finish_content());
        events.extend(self.finish_tool_calls());

        events
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::openai::*;

    fn make_chunk(
        id: &str,
        delta: Delta,
        finish_reason: Option<String>,
        usage: Option<Usage>,
    ) -> StreamChunk {
        StreamChunk {
            id: id.to_string(),
            object: "chat.completion.chunk".to_string(),
            created: 0,
            model: "gpt-4".to_string(),
            choices: vec![StreamChoice {
                index: 0,
                delta,
                finish_reason,
                logprobs: None,
            }],
            usage,
        }
    }

    #[test]
    fn test_text_only_stream() {
        let mut conv = AnthropicStreamConverter::new();

        // First chunk
        let events = conv.process(&make_chunk(
            "c",
            Delta {
                role: Some("assistant".to_string()),
                content: Some("Hello".to_string()),
                tool_calls: None,
                reasoning: None,
            },
            None,
            None,
        ));

        // message_start, content_block_start (text), content_block_delta
        assert_eq!(events.len(), 3);

        // Verify message_start event
        let first_event = &events[0];
        assert_eq!(first_event.event_type, "message_start");
        assert!(first_event.data.pointer("/message").is_some());

        // Verify content_block_start event
        let second_event = &events[1];
        assert_eq!(second_event.event_type, "content_block_start");
        assert_eq!(
            second_event.data["content_block"]["type"].as_str(),
            Some("text")
        );

        // Verify content_block_delta event
        let third_event = &events[2];
        assert_eq!(third_event.event_type, "content_block_delta");
        assert_eq!(third_event.data["delta"]["text"].as_str(), Some("Hello"));
    }

    #[test]
    fn test_reasoning_transitions() {
        let mut conv = AnthropicStreamConverter::new();

        // Reasoning chunk
        let events = conv.process(&make_chunk(
            "c",
            Delta {
                role: None,
                content: None,
                tool_calls: None,
                reasoning: Some("Let me think...".to_string()),
            },
            None,
            None,
        ));

        // Should have content_block_start (thinking) and content_block_delta
        assert!(events.len() >= 2);
        assert_eq!(
            events[1].data["content_block"]["type"].as_str(),
            Some("thinking")
        );

        // Text chunk — should close reasoning first
        let events = conv.process(&make_chunk(
            "c",
            Delta {
                role: None,
                content: Some("The answer is 42".to_string()),
                tool_calls: None,
                reasoning: None,
            },
            None,
            None,
        ));

        // Should have content_block_stop (reasoning), content_block_start (text), content_block_delta (text)
        assert!(events.len() >= 3);
        // Find content_block_stop event
        assert!(events.iter().any(|e| e.event_type == "content_block_stop"));
    }

    #[test]
    fn test_tool_call_stream() {
        let mut conv = AnthropicStreamConverter::new();

        let events = conv.process(&make_chunk(
            "c",
            Delta {
                role: Some("assistant".to_string()),
                content: None,
                tool_calls: Some(vec![DeltaToolCall {
                    index: 0,
                    id: Some("call_abc".to_string()),
                    call_type: Some("function".to_string()),
                    function: Some(DeltaFunctionCall {
                        name: Some("get_weather".to_string()),
                        arguments: Some(r#"{"city":"Paris"}"#.to_string()),
                    }),
                }]),
                reasoning: None,
            },
            None,
            None,
        ));

        // message_start, content_block_start (tool_use), content_block_delta
        assert!(events.len() >= 2);
        assert_eq!(events[1].event_type, "content_block_start");
        assert_eq!(
            events[1].data["content_block"]["type"].as_str(),
            Some("tool_use")
        );
        assert_eq!(
            events[1].data["content_block"]["name"].as_str(),
            Some("get_weather")
        );
    }

    #[test]
    fn test_skip_empty_tool_arguments() {
        let mut conv = AnthropicStreamConverter::new();

        let events = conv.process(&make_chunk(
            "c",
            Delta {
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
            None,
            None,
        ));

        // Should generate content_block_start but no arg_delta for empty args
        assert_eq!(events.len(), 2); // message_start + content_block_start
        let tool_start = &events[1];
        assert_eq!(
            tool_start.data["content_block"]["type"].as_str(),
            Some("tool_use")
        );
        // No content_block_delta events
        assert!(!events.iter().any(|e| e.event_type == "content_block_delta"));
    }

    #[test]
    fn test_completion_events() {
        let mut conv = AnthropicStreamConverter::new();

        // First chunk with content
        conv.process(&make_chunk(
            "c",
            Delta {
                role: Some("assistant".to_string()),
                content: Some("Hello".to_string()),
                tool_calls: None,
                reasoning: None,
            },
            None,
            None,
        ));

        // Final chunk
        let events = conv.process(&make_chunk(
            "c",
            Delta::default(),
            Some("stop".to_string()),
            Some(Usage {
                prompt_tokens: 0,
                completion_tokens: 0,
                total_tokens: 0,
            }),
        ));

        // Should have content_block_stop, message_delta, message_stop
        assert_eq!(events.len(), 3);

        let stop_event = events.iter().find(|e| e.event_type == "message_stop");
        assert!(stop_event.is_some());

        let delta_event = events.iter().find(|e| e.event_type == "message_delta");
        assert!(delta_event.is_some());
        assert_eq!(
            delta_event.unwrap().data["delta"]["stop_reason"].as_str(),
            Some("end_turn")
        );
    }

    #[test]
    fn test_serialize_format() {
        let conv = AnthropicStreamConverter::new();

        let event = AnthropicStreamEvent {
            event_type: "message_start".to_string(),
            data: json!({ "message": { "id": "msg_123" } }),
        };

        let bytes = conv.serialize(&event);
        let s = String::from_utf8(bytes).unwrap();
        assert!(s.starts_with("event: message_start\n"));
        assert!(s.contains("data: "));
        assert!(s.ends_with("\n\n"));
    }

    #[test]
    fn test_content_and_tool_calls_same_chunk() {
        let mut conv = AnthropicStreamConverter::new();

        let events = conv.process(&make_chunk(
            "chunk-test",
            Delta {
                role: Some("assistant".to_string()),
                content: Some("Let me".to_string()),
                tool_calls: Some(vec![DeltaToolCall {
                    index: 0,
                    id: Some("call_123".to_string()),
                    call_type: Some("function".to_string()),
                    function: Some(DeltaFunctionCall {
                        name: Some("get_weather".to_string()),
                        arguments: Some(r#"{"city":"Paris"}"#.to_string()),
                    }),
                }]),
                reasoning: None,
            },
            None,
            None,
        ));

        // content should have content_block_start, content_block_delta
        // tool should have content_block_stop (text), content_block_start (tool_use), content_block_delta (args)
        assert!(events.len() >= 5);

        // Verify we have both text and tool_use blocks
        let has_text_start = events.iter().any(|e| {
            e.event_type == "content_block_start"
                && e.data["content_block"]["type"].as_str() == Some("text")
        });
        let has_tool_start = events.iter().any(|e| {
            e.event_type == "content_block_start"
                && e.data["content_block"]["type"].as_str() == Some("tool_use")
        });

        assert!(has_text_start);
        assert!(has_tool_start);
    }

    #[test]
    fn test_is_completed() {
        let mut conv = AnthropicStreamConverter::new();
        assert!(!conv.is_completed());

        // Process a completion chunk
        conv.process(&make_chunk(
            "c",
            Delta::default(),
            Some("stop".to_string()),
            Some(Usage {
                prompt_tokens: 0,
                completion_tokens: 0,
                total_tokens: 0,
            }),
        ));

        assert!(conv.is_completed());
    }

    #[test]
    fn test_finalize_drains_pending() {
        let mut conv = AnthropicStreamConverter::new();

        // Start reasoning but never finish it
        conv.process(&make_chunk(
            "c",
            Delta {
                role: None,
                content: None,
                tool_calls: None,
                reasoning: Some("thinking...".to_string()),
            },
            None,
            None,
        ));

        let final_events = conv.finalize();
        // Should close reasoning phase
        assert!(
            final_events
                .iter()
                .any(|e| e.event_type == "content_block_stop")
        );
    }
}
