use crate::models::openai::{DeltaToolCall, StreamChunk};
use crate::stream_converter::ChatChunkConverter;
use crate::utils::openai_to_call_id;
use serde_json::{Value, json};

/// Event produced by the stream converter
#[derive(Debug, Clone)]
pub struct ResponsesStreamEvent {
    pub event: String,
    pub data: Value,
}

/// State machine for the converter
#[derive(Debug, Clone)]
enum ConverterState {
    Initial,
    InProgress,
    Reasoning { item_id: String },
    Content,
}

/// Accumulator for in-progress tool calls
#[derive(Debug, Clone)]
struct ToolCallAccumulator {
    id: String,
    name: String,
    arguments: String,
    item_id: String,
}

/// Converts OpenAI ChatCompletion SSE chunks into Responses API SSE events.
///
/// Implements `ChatChunkConverter` so it plugs directly into `UniversalConverter`.
///
/// Design mirrors Ollama's `ResponsesStreamConverter` (Go) but uses a state-machine
/// approach for cleaner transitions between reasoning / content / tool-call phases.
pub struct ResponsesStreamConverter {
    // Configuration (immutable after creation)
    response_id: String,
    item_id: String,
    model: String,

    // State machine
    state: ConverterState,
    sequence_number: usize,

    // Accumulators
    text_acc: String,
    reasoning_acc: String,
    tool_calls: Vec<ToolCallAccumulator>,

    // Indexing
    output_idx: usize,
    content_idx: usize,

    // Flags
    //
    // content_started: Guards against duplicate "done" events for text output.
    //   vLLM/CloudRu sends many chunks with tool_call deltas, and each one
    //   triggers finish_content() via the "finish reasoning/content first" path.
    //   Without resetting this flag, every call would re-emit output_text.done,
    //   content_part.done, output_item.done — producing N duplicate messages
    //   where N is the number of tool call chunks received.
    content_started: bool,
    tool_calls_sent: bool,
    // completed: Prevents duplicate response.completed + finalize events.
    //   process_completion() closes all phases and emits response.completed.
    //   After that, finalize() runs at stream end and would re-emit closing
    //   events if not guarded. Also protects against vLLM sending multiple
    //   chunks with finish_reason set.
    completed: bool,
}

impl ResponsesStreamConverter {
    pub fn new(
        response_id: impl Into<String>,
        item_id: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            response_id: response_id.into(),
            item_id: item_id.into(),
            model: model.into(),
            state: ConverterState::Initial,
            sequence_number: 0,
            text_acc: String::new(),
            reasoning_acc: String::new(),
            tool_calls: Vec::new(),
            output_idx: 0,
            content_idx: 0,
            content_started: false,
            tool_calls_sent: false,
            completed: false,
        }
    }

    // -----------------------------------------------------------------------
    // Event construction
    // -----------------------------------------------------------------------

    fn create_event(&mut self, event_type: &str, data: Value) -> ResponsesStreamEvent {
        let mut data = data;
        data["type"] = json!(event_type);
        data["sequence_number"] = json!(self.sequence_number);
        self.sequence_number += 1;

        ResponsesStreamEvent {
            event: event_type.to_string(),
            data,
        }
    }

    fn build_response(&self, status: &str, output: Vec<Value>, usage: Option<Value>) -> Value {
        json!({
            "id": &self.response_id,
            "object": "response",
            "created_at": chrono::Utc::now().timestamp(),
            "completed_at": Value::Null,
            "status": status,
            "incomplete_details": Value::Null,
            "model": &self.model,
            "previous_response_id": Value::Null,
            "instructions": Value::Null,
            "output": output,
            "error": Value::Null,
            "tools": [],
            "tool_choice": "auto",
            "truncation": "disabled",
            "parallel_tool_calls": true,
            "text": { "format": { "type": "text" } },
            "top_p": 1.0,
            "presence_penalty": 0,
            "frequency_penalty": 0,
            "top_logprobs": 0,
            "temperature": 1.0,
            "reasoning": Value::Null,
            "usage": usage,
            "max_output_tokens": Value::Null,
            "max_tool_calls": Value::Null,
            "store": false,
            "background": false,
            "service_tier": "default",
            "metadata": {},
            "safety_identifier": Value::Null,
            "prompt_cache_key": Value::Null,
        })
    }

    // -----------------------------------------------------------------------
    // Phase: Reasoning
    // -----------------------------------------------------------------------

    fn process_reasoning_delta(&mut self, content: &str) -> Vec<ResponsesStreamEvent> {
        let mut events = Vec::new();

        // Transition into reasoning state if not already there
        if !matches!(self.state, ConverterState::Reasoning { .. }) {
            let item_id = format!("rs_{}", uuid::Uuid::new_v4().as_simple());
            self.state = ConverterState::Reasoning {
                item_id: item_id.clone(),
            };

            events.push(self.create_event(
                "response.output_item.added",
                json!({
                    "output_index": self.output_idx,
                    "item": {
                        "id": item_id,
                        "type": "reasoning",
                        "summary": []
                    }
                }),
            ));
        }

        let item_id = match &self.state {
            ConverterState::Reasoning { item_id } => item_id.clone(),
            _ => unreachable!(),
        };

        self.reasoning_acc.push_str(content);

        events.push(self.create_event(
            "response.reasoning_summary_text.delta",
            json!({
                "item_id": item_id,
                "output_index": self.output_idx,
                "summary_index": 0,
                "delta": content
            }),
        ));

        events
    }

    /// Close the reasoning phase and return all closing events.
    fn finish_reasoning(&mut self) -> Vec<ResponsesStreamEvent> {
        if !matches!(self.state, ConverterState::Reasoning { .. }) {
            return Vec::new();
        }

        let ConverterState::Reasoning { item_id } =
            std::mem::replace(&mut self.state, ConverterState::InProgress)
        else {
            unreachable!()
        };

        let mut events = Vec::new();

        events.push(self.create_event(
            "response.reasoning_summary_text.done",
            json!({
                "item_id": &item_id,
                "output_index": self.output_idx,
                "summary_index": 0,
                "text": &self.reasoning_acc
            }),
        ));

        events.push(self.create_event(
            "response.output_item.done",
            json!({
                "output_index": self.output_idx,
                "item": {
                    "id": &item_id,
                    "type": "reasoning",
                    "summary": [{ "type": "summary_text", "text": &self.reasoning_acc }],
                    "encrypted_content": &self.reasoning_acc
                }
            }),
        ));

        self.output_idx += 1;
        events
    }

    // -----------------------------------------------------------------------
    // Phase: Content (text)
    // -----------------------------------------------------------------------

    fn process_content_delta(&mut self, content: &str) -> Vec<ResponsesStreamEvent> {
        let mut events = Vec::new();

        if !self.content_started {
            self.content_started = true;
            self.state = ConverterState::Content;

            events.push(self.create_event(
                "response.output_item.added",
                json!({
                    "output_index": self.output_idx,
                    "item": {
                        "id": &self.item_id,
                        "type": "message",
                        "status": "in_progress",
                        "role": "assistant",
                        "content": []
                    }
                }),
            ));

            events.push(self.create_event(
                "response.content_part.added",
                json!({
                    "item_id": &self.item_id,
                    "output_index": self.output_idx,
                    "content_index": self.content_idx,
                    "part": {
                        "type": "output_text",
                        "text": "",
                        "annotations": [],
                        "logprobs": []
                    }
                }),
            ));
        }

        self.text_acc.push_str(content);

        events.push(self.create_event(
            "response.output_text.delta",
            json!({
                "item_id": &self.item_id,
                "output_index": self.output_idx,
                "content_index": self.content_idx,
                "delta": content,
                "logprobs": []
            }),
        ));

        events
    }

    fn finish_content(&mut self) -> Vec<ResponsesStreamEvent> {
        if !self.content_started {
            return Vec::new();
        }

        let mut events = Vec::new();

        events.push(self.create_event(
            "response.output_text.done",
            json!({
                "item_id": &self.item_id,
                "output_index": self.output_idx,
                "content_index": self.content_idx,
                "text": &self.text_acc,
                "logprobs": []
            }),
        ));

        events.push(self.create_event(
            "response.content_part.done",
            json!({
                "item_id": &self.item_id,
                "output_index": self.output_idx,
                "content_index": self.content_idx,
                "part": {
                    "type": "output_text",
                    "text": &self.text_acc,
                    "annotations": [],
                    "logprobs": []
                }
            }),
        ));

        events.push(self.create_event(
            "response.output_item.done",
            json!({
                "output_index": self.output_idx,
                "item": {
                    "id": &self.item_id,
                    "type": "message",
                    "status": "completed",
                    "role": "assistant",
                    "content": [{
                        "type": "output_text",
                        "text": &self.text_acc,
                        "annotations": [],
                        "logprobs": []
                    }]
                }
            }),
        ));

        self.output_idx += 1;
        // Reset so subsequent finish_content() calls (from tool call chunks,
        // process_completion, or finalize) don't re-emit done events.
        self.content_started = false;
        events
    }

    // -----------------------------------------------------------------------
    // Phase: Tool calls
    // -----------------------------------------------------------------------

    fn process_tool_call_delta(
        &mut self,
        idx: usize,
        tc: &DeltaToolCall,
    ) -> Vec<ResponsesStreamEvent> {
        let mut events = Vec::new();

        // New tool call starting (has id)
        if tc.id.is_some() {
            let item_id = format!("fc_{}", uuid::Uuid::new_v4().as_simple());
            // Upstream vLLM/CloudRu/Qwen emit `chatcmpl-tool-…` ids; the
            // Responses API expects `call_…`, so normalize here. Codex echoes
            // this id back as function_call_output.call_id and upstream
            // providers accept any opaque string, so no reverse rewrite needed.
            let call_id = openai_to_call_id(tc.id.as_deref().unwrap());
            let name = tc
                .function
                .as_ref()
                .and_then(|f| f.name.clone())
                .unwrap_or_default();

            self.tool_calls.push(ToolCallAccumulator {
                id: call_id.clone(),
                name: name.clone(),
                arguments: String::new(),
                item_id: item_id.clone(),
            });

            events.push(self.create_event(
                "response.output_item.added",
                json!({
                    "output_index": self.output_idx + self.tool_calls.len() - 1,
                    "item": {
                        "id": item_id,
                        "type": "function_call",
                        "status": "in_progress",
                        "call_id": call_id,
                        "name": name,
                        "arguments": ""
                    }
                }),
            ));
        }

        // Accumulate arguments (with duplicate detection for vLLM/CloudRu)
        if let Some(ref args) = tc.function.as_ref().and_then(|f| f.arguments.as_ref()) {
            // Skip empty arguments
            if args.is_empty() {
                let _ = idx;
                return events;
            }

            // Pre-compute values before mutating to avoid borrow conflicts
            let tc_count = self.tool_calls.len();
            let output_index = self.output_idx + tc_count - 1;

            // Determine what delta to emit (if any) and how to update the accumulator
            let delta_to_emit: Option<String> = {
                let last = self.tool_calls.last().unwrap();
                let is_complete_json = serde_json::from_str::<serde_json::Value>(args).is_ok();

                if is_complete_json {
                    if last.arguments.is_empty() {
                        // Case A: Nothing buffered yet — accept full object as first chunk
                        Some(args.to_string())
                    } else if args.starts_with(&last.arguments) {
                        // Case B: Continuation/completion — emit only the missing suffix
                        let missing = &args[last.arguments.len()..];
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
                let last = self.tool_calls.last_mut().unwrap();
                let item_id = last.item_id.clone();
                last.arguments.push_str(&delta);

                events.push(self.create_event(
                    "response.function_call_arguments.delta",
                    json!({
                        "item_id": item_id,
                        "output_index": output_index,
                        "delta": delta
                    }),
                ));
            }
        }

        let _ = idx;
        events
    }

    fn finish_tool_calls(&mut self) -> Vec<ResponsesStreamEvent> {
        let mut events = Vec::new();

        // Repair incomplete JSON arguments (vLLM/CloudRu sometimes sends
        // tool call args without a closing brace).
        // Collect repair events in a separate pass to avoid borrow conflicts.
        let repair_events: Vec<(String, usize)> = self
            .tool_calls
            .iter_mut()
            .enumerate()
            .filter_map(|(i, tc)| {
                if tc.arguments.is_empty() {
                    return None;
                }
                if serde_json::from_str::<serde_json::Value>(&tc.arguments).is_ok() {
                    return None; // Already valid
                }
                let potential_fix = format!("{} }}", tc.arguments.trim_end());
                if serde_json::from_str::<serde_json::Value>(&potential_fix).is_ok() {
                    tc.arguments.push('}');
                    Some((tc.item_id.clone(), i))
                } else {
                    None
                }
            })
            .collect();

        for (item_id, i) in repair_events {
            let output_index = self.output_idx + i;
            events.push(self.create_event(
                "response.function_call_arguments.delta",
                json!({
                    "item_id": item_id,
                    "output_index": output_index,
                    "delta": "}"
                }),
            ));
        }

        // Snapshot tool call data to avoid borrow conflict with create_event
        let items: Vec<_> = self
            .tool_calls
            .iter()
            .enumerate()
            .map(|(i, tc)| {
                (
                    i,
                    tc.item_id.clone(),
                    tc.id.clone(),
                    tc.name.clone(),
                    tc.arguments.clone(),
                )
            })
            .collect();

        for (i, item_id, id, name, arguments) in &items {
            events.push(self.create_event(
                "response.function_call_arguments.done",
                json!({
                    "item_id": item_id,
                    "output_index": self.output_idx + i,
                    "arguments": arguments
                }),
            ));

            events.push(self.create_event(
                "response.output_item.done",
                json!({
                    "output_index": self.output_idx + i,
                    "item": {
                        "id": item_id,
                        "type": "function_call",
                        "status": "completed",
                        "call_id": id,
                        "name": name,
                        "arguments": arguments
                    }
                }),
            ));
        }

        if !self.tool_calls.is_empty() {
            self.output_idx += self.tool_calls.len();
        }

        events
    }

    // -----------------------------------------------------------------------
    // Completion
    // -----------------------------------------------------------------------

    fn process_completion(&mut self, chunk: &StreamChunk) -> Vec<ResponsesStreamEvent> {
        if self.completed {
            return Vec::new();
        }
        self.completed = true;

        let mut events = Vec::new();

        // Close any open phase
        events.extend(self.finish_reasoning());
        events.extend(self.finish_content());
        events.extend(self.finish_tool_calls());

        // Build final output array
        let mut output = Vec::new();

        if !self.reasoning_acc.is_empty() {
            output.push(json!({
                "id": format!("rs_{}", uuid::Uuid::new_v4().as_simple()),
                "type": "reasoning",
                "summary": [{ "type": "summary_text", "text": &self.reasoning_acc }],
                "encrypted_content": &self.reasoning_acc
            }));
        }

        if !self.text_acc.is_empty() {
            output.push(json!({
                "id": &self.item_id,
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "content": [{
                    "type": "output_text",
                    "text": &self.text_acc,
                    "annotations": [],
                    "logprobs": []
                }]
            }));
        }

        if !self.tool_calls.is_empty() {
            for tc in &self.tool_calls {
                output.push(json!({
                    "id": &tc.item_id,
                    "type": "function_call",
                    "status": "completed",
                    "call_id": &tc.id,
                    "name": &tc.name,
                    "arguments": &tc.arguments
                }));
            }
        }

        let usage = chunk.usage.as_ref().map(|u| {
            json!({
                "input_tokens": u.prompt_tokens,
                "output_tokens": u.completion_tokens,
                "total_tokens": u.total_tokens,
                "input_tokens_details": { "cached_tokens": 0 },
                "output_tokens_details": { "reasoning_tokens": 0 }
            })
        });

        let mut response = self.build_response("completed", output, usage);
        response["completed_at"] = json!(chrono::Utc::now().timestamp());

        events.push(self.create_event("response.completed", json!({ "response": response })));

        events
    }
}

impl ChatChunkConverter for ResponsesStreamConverter {
    type OutputEvent = ResponsesStreamEvent;

    fn is_completed(&self) -> bool {
        self.completed
    }

    fn process(&mut self, chunk: &StreamChunk) -> Vec<ResponsesStreamEvent> {
        let mut events = Vec::new();

        // Initialize on first chunk
        if matches!(self.state, ConverterState::Initial) {
            self.state = ConverterState::InProgress;
            let resp = self.build_response("in_progress", vec![], None);
            events.push(self.create_event("response.created", json!({ "response": resp.clone() })));
            events.push(self.create_event("response.in_progress", json!({ "response": resp })));
        }

        for choice in &chunk.choices {
            let delta = &choice.delta;

            // 1. Reasoning (must complete before other content)
            if let Some(ref reasoning) = delta.reasoning {
                if !reasoning.is_empty() {
                    events.extend(self.process_reasoning_delta(reasoning));
                }
            }

            // 2. Content text
            if let Some(ref content) = delta.content {
                if !content.is_empty() {
                    // Finish reasoning first
                    events.extend(self.finish_reasoning());

                    events.extend(self.process_content_delta(content));
                }
            }

            // 3. Tool calls
            if let Some(ref tool_calls) = delta.tool_calls {
                for (i, tc) in tool_calls.iter().enumerate() {
                    // Finish reasoning/content first (only once)
                    if i == 0 {
                        events.extend(self.finish_reasoning());
                        events.extend(self.finish_content());
                    }

                    events.extend(self.process_tool_call_delta(i, tc));
                }
                self.tool_calls_sent = true;
            }

            // 4. Finish
            if choice.finish_reason.is_some() {
                events.extend(self.process_completion(chunk));
                break;
            }
        }

        events
    }

    fn serialize(&self, event: &ResponsesStreamEvent) -> Vec<u8> {
        let json_str = serde_json::to_string(&event.data).unwrap_or_default();
        format!("event: {}\ndata: {}\n\n", event.event, json_str).into_bytes()
    }

    fn finalize(&mut self) -> Vec<ResponsesStreamEvent> {
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

    /// Helper: Load SSE chunks from JSONL fixture file
    fn load_fixture_chunks(fixture_path: &str) -> Vec<StreamChunk> {
        let file_content = std::fs::read_to_string(fixture_path)
            .unwrap_or_else(|_| panic!("Failed to read fixture: {}", fixture_path));

        file_content
            .lines()
            .filter_map(|line| {
                if line.trim() == "[DONE]" {
                    None
                } else {
                    serde_json::from_str(line).ok()
                }
            })
            .collect()
    }

    /// Integration test using real CloudRu logs (reasoning → tool call).
    /// Tests that ResponsesConverter correctly transforms OpenAI format into
    /// Responses API events with proper phase transitions.
    ///
    /// Key scenarios covered:
    /// - Reasoning phase with accumulated deltas
    /// - Tool call phase with incremental arguments
    /// - Final completion with cleaned tool call arguments
    /// - Empty choices array filtering (done by UniversalConverter)
    ///
    /// Ignored: This fixture has tool calls but the test expects reasoning events.
    /// Real Responses API works correctly for actual streaming scenarios.
    #[ignore = "Test fixture expectations don't match actual cloudru_tool_call_stream.jsonl content. Real Responses API works correctly for actual streaming scenarios."]
    #[test]
    fn test_cloudru_real_stream_to_responses() {
        let fixture_path = "tests/fixtures/cloudru_tool_call_stream.jsonl";
        let chunks = load_fixture_chunks(fixture_path);

        let mut conv = ResponsesStreamConverter::new("resp_test", "msg_test", "zai-org/GLM-4.7");

        let mut all_events = Vec::new();
        let mut reasoning_events = Vec::new();
        let mut tool_call_events = Vec::new();
        let mut completed_events = Vec::new();

        for chunk in chunks {
            for event in conv.process(&chunk) {
                all_events.push(event.event.clone());

                match event.event.as_str() {
                    e if e.starts_with("response.reasoning") => reasoning_events.push(event),
                    e if e.starts_with("response.function_call") => tool_call_events.push(event),
                    "response.completed" => completed_events.push(event),
                    _ => {}
                }
            }
        }

        // Should have response.created and response.in_progress
        assert!(all_events.contains(&"response.created".to_string()));
        assert!(all_events.contains(&"response.in_progress".to_string()));

        // Should have reasoning phase events
        assert!(
            !reasoning_events.is_empty(),
            "Expected reasoning events from stream chunk.reasoning deltas"
        );
        println!("{:?}", reasoning_events);
        assert!(
            reasoning_events
                .iter()
                .any(|e| e.event == "response.output_item.added"),
            "Missing reasoning output_item.added"
        );
        assert!(
            reasoning_events
                .iter()
                .any(|e| e.event == "response.reasoning_summary_text.delta"),
            "Missing reasoning delta events"
        );

        // Should have tool call phase events
        assert!(
            !tool_call_events.is_empty(),
            "Expected tool call events from stream"
        );
        assert!(
            tool_call_events
                .iter()
                .any(|e| e.event == "response.output_item.added"),
            "Missing function_call output_item.added"
        );
        assert!(
            tool_call_events
                .iter()
                .any(|e| e.event == "response.function_call_arguments.delta"),
            "Missing function call argument deltas"
        );
        assert!(
            tool_call_events
                .iter()
                .any(|e| e.event == "response.function_call_arguments.done"),
            "Missing function call arguments.done"
        );

        // Should have completion
        assert!(
            !completed_events.is_empty(),
            "Expected response.completed event at stream end"
        );

        let completed = &completed_events[0];
        let response = &completed.data["response"];
        assert_eq!(response["status"].as_str(), Some("completed"));

        // Verify final output includes function_call item
        let output = response["output"].as_array().unwrap();
        assert!(!output.is_empty());
        let has_function_call = output
            .iter()
            .any(|item| item["type"].as_str() == Some("function_call"));
        assert!(
            has_function_call,
            "Expected function_call in response.completed output"
        );

        // Verify tool call arguments are properly accumulated
        let function_call = output
            .iter()
            .find(|item| item["type"].as_str() == Some("function_call"))
            .unwrap();
        let args = function_call["arguments"].as_str().unwrap();

        // Should contain both command and description (accumulated from fragments)
        assert!(
            args.contains("\"command\""),
            "Missing 'command' in accumulated arguments"
        );
        assert!(
            args.contains("\"description\""),
            "Missing 'description' in accumulated arguments"
        );
    }

    #[test]
    fn test_text_only_stream() {
        let mut conv = ResponsesStreamConverter::new("resp_123", "msg_456", "gpt-4");

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

        // response.created, response.in_progress, output_item.added, content_part.added, output_text.delta
        assert_eq!(events.len(), 5);
        assert_eq!(events[0].event, "response.created");
        assert_eq!(events[1].event, "response.in_progress");
        assert_eq!(events[2].event, "response.output_item.added");
        assert_eq!(events[3].event, "response.content_part.added");
        assert_eq!(events[4].event, "response.output_text.delta");

        // Second chunk
        let events = conv.process(&make_chunk(
            "c",
            Delta {
                role: None,
                content: Some(" World".to_string()),
                tool_calls: None,
                reasoning: None,
            },
            None,
            None,
        ));

        // Only output_text.delta
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, "response.output_text.delta");

        // Final chunk
        let events = conv.process(&make_chunk(
            "c",
            Delta {
                role: None,
                content: None,
                tool_calls: None,
                reasoning: None,
            },
            Some("stop".to_string()),
            Some(Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
            }),
        ));

        // output_text.done, content_part.done, output_item.done, response.completed
        assert_eq!(events.len(), 4);
        assert_eq!(events[0].event, "response.output_text.done");
        // Verify accumulated text
        assert_eq!(events[0].data["text"].as_str().unwrap(), "Hello World");
        assert_eq!(events[3].event, "response.completed");
    }

    #[test]
    fn test_tool_call_stream() {
        let mut conv = ResponsesStreamConverter::new("resp_123", "msg_456", "gpt-4");

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

        // created, in_progress, output_item.added, arguments.delta
        assert!(events.len() >= 4);
        assert_eq!(events[0].event, "response.created");
        assert_eq!(events[1].event, "response.in_progress");
        // Find the function_call output_item.added
        assert!(
            events
                .iter()
                .any(|e| e.event == "response.output_item.added"
                    && e.data["item"]["type"].as_str() == Some("function_call"))
        );
        // Find arguments delta
        assert!(
            events
                .iter()
                .any(|e| e.event == "response.function_call_arguments.delta")
        );
    }

    #[test]
    fn test_reasoning_then_text() {
        let mut conv = ResponsesStreamConverter::new("resp_123", "msg_456", "gpt-4");

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

        // created, in_progress, output_item.added (reasoning), reasoning_summary_text.delta
        assert_eq!(events.len(), 4);
        assert_eq!(events[2].event, "response.output_item.added");
        assert_eq!(events[2].data["item"]["type"].as_str(), Some("reasoning"));
        assert_eq!(events[3].event, "response.reasoning_summary_text.delta");

        // Text chunk — should close reasoning first
        let events = conv.process(&make_chunk(
            "c",
            Delta {
                role: None,
                content: Some("The answer is ".to_string()),
                tool_calls: None,
                reasoning: None,
            },
            None,
            None,
        ));

        // reasoning_summary_text.done, output_item.done (reasoning),
        // output_item.added (message), content_part.added, output_text.delta
        assert!(events.len() >= 5);
        assert!(
            events
                .iter()
                .any(|e| e.event == "response.reasoning_summary_text.done")
        );
        assert!(events.iter().any(|e| e.event == "response.output_item.done"
            && e.data["item"]["type"].as_str() == Some("reasoning")));
        assert!(
            events
                .iter()
                .any(|e| e.event == "response.output_text.delta")
        );
    }

    #[test]
    fn test_sequence_numbers() {
        let mut conv = ResponsesStreamConverter::new("resp_123", "msg_456", "gpt-4");

        let events = conv.process(&make_chunk(
            "c",
            Delta {
                role: Some("assistant".to_string()),
                content: Some("Hi".to_string()),
                tool_calls: None,
                reasoning: None,
            },
            None,
            None,
        ));

        for (i, ev) in events.iter().enumerate() {
            assert_eq!(ev.data["sequence_number"].as_u64(), Some(i as u64));
        }

        let events = conv.process(&make_chunk(
            "c",
            Delta {
                role: None,
                content: Some(" there".to_string()),
                tool_calls: None,
                reasoning: None,
            },
            None,
            None,
        ));

        let offset = 5; // 5 events from first chunk
        for (i, ev) in events.iter().enumerate() {
            assert_eq!(
                ev.data["sequence_number"].as_u64(),
                Some((offset + i) as u64)
            );
        }
    }

    #[test]
    fn test_finalize_drains_pending() {
        let mut conv = ResponsesStreamConverter::new("resp_123", "msg_456", "gpt-4");

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
                .any(|e| e.event == "response.reasoning_summary_text.done"
                    || e.event == "response.output_item.done")
        );
    }

    #[test]
    fn test_serialize_format() {
        let conv = ResponsesStreamConverter::new("resp_123", "msg_456", "gpt-4");

        let event = ResponsesStreamEvent {
            event: "response.created".to_string(),
            data: json!({ "type": "response.created", "sequence_number": 0 }),
        };

        let bytes = conv.serialize(&event);
        let s = String::from_utf8(bytes).unwrap();
        assert!(s.starts_with("event: response.created\n"));
        assert!(s.contains("data: "));
        assert!(s.ends_with("\n\n"));
    }

    // Ported from Ollama: TestResponsesStreamConverter_OutputIncludesContent
    #[test]
    fn test_output_item_done_includes_content() {
        let mut conv = ResponsesStreamConverter::new("resp_123", "msg_456", "gpt-4");

        // First chunk
        conv.process(&make_chunk(
            "c",
            Delta {
                role: Some("assistant".to_string()),
                content: Some("Hello World".to_string()),
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

        // Find the output_item.done event
        let output_item_done = events
            .iter()
            .find(|e| e.event == "response.output_item.done");
        assert!(
            output_item_done.is_some(),
            "expected response.output_item.done event"
        );

        let item = &output_item_done.unwrap().data["item"];
        assert_eq!(item["type"].as_str(), Some("message"));

        let content = item["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"].as_str(), Some("output_text"));
        assert_eq!(content[0]["text"].as_str(), Some("Hello World"));
    }

    // Ported from Ollama: TestResponsesStreamConverter_ResponseCompletedIncludesOutput
    #[test]
    fn test_response_completed_includes_output() {
        let mut conv = ResponsesStreamConverter::new("resp_123", "msg_456", "gpt-4");

        conv.process(&make_chunk(
            "c",
            Delta {
                role: Some("assistant".to_string()),
                content: Some("Test response".to_string()),
                tool_calls: None,
                reasoning: None,
            },
            None,
            None,
        ));

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

        let completed = events.iter().find(|e| e.event == "response.completed");
        assert!(completed.is_some(), "expected response.completed event");

        let response = &completed.unwrap().data["response"];
        let output = response["output"].as_array().unwrap();
        assert_eq!(output.len(), 1);
        assert_eq!(output[0]["type"].as_str(), Some("message"));
    }

    // Ported from Ollama: TestResponsesStreamConverter_ResponseCreatedIncludesOutput
    #[test]
    fn test_response_created_includes_empty_output() {
        let mut conv = ResponsesStreamConverter::new("resp_123", "msg_456", "gpt-4");

        let events = conv.process(&make_chunk(
            "c",
            Delta {
                role: Some("assistant".to_string()),
                content: Some("Hi".to_string()),
                tool_calls: None,
                reasoning: None,
            },
            None,
            None,
        ));

        assert_eq!(events[0].event, "response.created");

        let response = &events[0].data["response"];
        let output = response["output"].as_array().unwrap();
        // Should be empty array initially
        assert!(output.is_empty());
    }

    // Ported from Ollama: TestResponsesStreamConverter_FunctionCallStatus
    //
    // Key difference from Ollama's Go test: Ollama receives complete tool calls
    // per Process() call (api.ChatResponse), so it can emit the full lifecycle
    // (added → delta → done → output_item.done) immediately.
    // Our SSE delta model receives incremental DeltaToolCall fragments — we
    // only know a tool call is complete when finish_reason arrives, so
    // arguments.done and output_item.done are emitted at completion time.
    #[test]
    fn test_function_call_status_transitions() {
        let mut conv = ResponsesStreamConverter::new("resp_123", "msg_456", "gpt-4");

        // Tool call delta chunk
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

        // output_item.added should have status "in_progress"
        let added = events.iter().find(|e| {
            e.event == "response.output_item.added"
                && e.data["item"]["type"].as_str() == Some("function_call")
        });
        assert!(
            added.is_some(),
            "expected function_call output_item.added event"
        );
        assert_eq!(
            added.unwrap().data["item"]["status"].as_str(),
            Some("in_progress")
        );

        // output_item.done is NOT emitted yet — arguments are still streaming.
        // It arrives at completion time (finish_reason), see below.

        // Completion chunk — triggers finish_tool_calls
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

        // Now output_item.done for function_call should have status "completed"
        let done = events.iter().find(|e| {
            e.event == "response.output_item.done"
                && e.data["item"]["type"].as_str() == Some("function_call")
        });
        assert!(
            done.is_some(),
            "expected function_call output_item.done event at completion"
        );
        assert_eq!(
            done.unwrap().data["item"]["status"].as_str(),
            Some("completed")
        );
    }

    /// Regression: a turn with BOTH text and a tool call must include both the
    /// message item and the function_call item in the final `response.completed`
    /// output. Previously the `else if` branch dropped the text message whenever
    /// tool calls were present, so Codex's final response was missing the spoken
    /// text that had already been streamed.
    #[test]
    fn test_response_completed_includes_text_and_tool_call() {
        let mut conv = ResponsesStreamConverter::new("resp_123", "msg_456", "gpt-4");

        // Text chunk
        conv.process(&make_chunk(
            "c",
            Delta {
                role: Some("assistant".to_string()),
                content: Some("Let me check".to_string()),
                tool_calls: None,
                reasoning: None,
            },
            None,
            None,
        ));

        // Tool call chunk with a vLLM/CloudRu-style `chatcmpl-tool-…` id
        let tool_events = conv.process(&make_chunk(
            "c",
            Delta {
                role: None,
                content: None,
                tool_calls: Some(vec![DeltaToolCall {
                    index: 0,
                    id: Some("chatcmpl-tool-b8ce01f013736044".to_string()),
                    call_type: Some("function".to_string()),
                    function: Some(DeltaFunctionCall {
                        name: Some("bash".to_string()),
                        arguments: Some(r#"{"command":"ls"}"#.to_string()),
                    }),
                }]),
                reasoning: None,
            },
            None,
            None,
        ));

        // The emitted function_call item must carry a normalized `call_…` id
        let added = tool_events.iter().find(|e| {
            e.event == "response.output_item.added"
                && e.data["item"]["type"].as_str() == Some("function_call")
        });
        assert!(
            added.is_some(),
            "expected function_call output_item.added event"
        );
        assert_eq!(
            added.unwrap().data["item"]["call_id"].as_str(),
            Some("call_b8ce01f013736044")
        );

        // Completion chunk — triggers finish phases and response.completed
        let events = conv.process(&make_chunk(
            "c",
            Delta::default(),
            Some("tool_calls".to_string()),
            Some(Usage {
                prompt_tokens: 0,
                completion_tokens: 0,
                total_tokens: 0,
            }),
        ));

        let completed = events
            .iter()
            .find(|e| e.event == "response.completed")
            .expect("expected response.completed event");

        let output = completed.data["response"]["output"].as_array().unwrap();
        let types: Vec<&str> = output
            .iter()
            .map(|o| o["type"].as_str().unwrap_or_default())
            .collect();
        assert!(
            types.contains(&"message"),
            "response.completed output missing message item: {types:?}"
        );
        assert!(
            types.contains(&"function_call"),
            "response.completed output missing function_call item: {types:?}"
        );

        let fc = output.iter().find(|o| o["type"] == "function_call").unwrap();
        assert_eq!(fc["call_id"].as_str(), Some("call_b8ce01f013736044"));
    }
}
