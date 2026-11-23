use crate::error::Error;
use crate::models::openai::*;
use crate::stream_converter::{ChatChunkConverter, UniversalConverter};
use bytes::Bytes;
use tokio_stream::Stream;

/// Minimal OpenAI ChatChunkConverter - pure passthrough with null id/type fix.
/// Handles CloudRu/vLLM sending null id/type on continuation chunks by tracking
/// first occurrence and filling in subsequent chunks.
pub struct OpenaiPassthrough {
    /// Maps tool_call index -> id string
    tool_call_ids: std::collections::HashMap<usize, String>,
    /// Maps tool_call index -> type string
    tool_call_types: std::collections::HashMap<usize, String>,
    completed: bool,
}

impl OpenaiPassthrough {
    pub fn new() -> Self {
        Self {
            tool_call_ids: std::collections::HashMap::new(),
            tool_call_types: std::collections::HashMap::new(),
            completed: false,
        }
    }

    /// Fill in null id/type for tool calls based on prior chunks
    fn fix_null_fields(&mut self, chunk: &StreamChunk) -> StreamChunk {
        let Some(choice) = chunk.choices.first() else {
            return chunk.clone();
        };

        let Some(tool_calls) = choice.delta.tool_calls.as_ref() else {
            return chunk.clone();
        };

        let fixed_calls: Vec<DeltaToolCall> = tool_calls
            .iter()
            .map(|tc| {
                // Cache id/type when present
                if let Some(id) = tc.id.as_ref().filter(|i| !i.is_empty()) {
                    self.tool_call_ids.insert(tc.index, id.clone());
                }
                if let Some(call_type) = tc.call_type.as_ref().filter(|t| !t.is_empty()) {
                    self.tool_call_types.insert(tc.index, call_type.clone());
                }

                DeltaToolCall {
                    id: tc
                        .id
                        .clone()
                        .or_else(|| self.tool_call_ids.get(&tc.index).cloned()),
                    call_type: tc
                        .call_type
                        .clone()
                        .or_else(|| self.tool_call_types.get(&tc.index).cloned()),
                    ..tc.clone()
                }
            })
            .collect();

        let mut result = chunk.clone();
        result.choices[0].delta.tool_calls = Some(fixed_calls);
        result
    }

    fn serialize_chunk(&self, chunk: &StreamChunk) -> Vec<Bytes> {
        match serde_json::to_string(chunk) {
            Ok(json_str) => vec![Bytes::from(format!("data: {}\n\n", json_str))],
            Err(_) => Vec::new(),
        }
    }
}

impl Default for OpenaiPassthrough {
    fn default() -> Self {
        Self::new()
    }
}

impl ChatChunkConverter for OpenaiPassthrough {
    type OutputEvent = Bytes;

    fn process(&mut self, chunk: &StreamChunk) -> Vec<Bytes> {
        // A usage-bearing chunk is the only source of billed cost in a stream.
        // Gateways send it either as a `choices: []` chunk or attached to a
        // repeat of the finish chunk, and always *after* the finish chunk, so
        // it must be forwarded even once this converter has completed.
        let has_usage = chunk.usage.is_some();

        // Empty-choices chunks without usage carry nothing for the client.
        if chunk.choices.is_empty() {
            return if has_usage {
                self.serialize_chunk(chunk)
            } else {
                Vec::new()
            };
        }

        // Skip trailing chunks after completion, except the usage-bearing one.
        if self.completed && !has_usage {
            return Vec::new();
        }

        // Check completion
        if let Some(reason) = chunk
            .choices
            .first()
            .and_then(|c| c.finish_reason.as_deref())
            && (reason == "stop" || reason == "tool_calls" || reason == "length")
        {
            self.completed = true;
        }

        // Fix null id/type for tool calls
        let fixed_chunk = self.fix_null_fields(chunk);

        // Serialize back - even if completed (passthrough should send final chunk)
        self.serialize_chunk(&fixed_chunk)
    }

    fn serialize(&self, event: &Bytes) -> Vec<u8> {
        event.to_vec()
    }

    fn is_completed(&self) -> bool {
        self.completed
    }
}

/// Public function to create OpenAI passthrough stream
pub fn openai_passthrough<S>(stream: S) -> impl Stream<Item = Result<Bytes, Error>> + 'static
where
    S: Stream<Item = Result<Bytes, Error>> + Unpin + 'static,
{
    let converter = OpenaiPassthrough::new();
    let universal = UniversalConverter::new(converter);
    universal.convert(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::*;
    use bytes::Bytes;
    use futures::stream::{StreamExt, iter};

    #[tokio::test]
    async fn test_malformed_tool_args_text_handling() {
        // Simulate the bug: "arguments" field contains text instead of valid JSON
        // This happens when CloudRu/GLM sends invalid JSON with extra characters
        let mut converter = OpenaiPassthrough::new();

        // First chunk: normal tool call start
        let chunk1 = StreamChunk {
            id: "test-id".to_string(),
            object: "chat.completion.chunk".to_string(),
            created: 123,
            model: "test-model".to_string(),
            choices: vec![StreamChoice {
                index: 0,
                delta: Delta {
                    role: None,
                    content: None,
                    tool_calls: Some(vec![DeltaToolCall {
                        index: 0,
                        id: Some("tool".to_string()),
                        call_type: Some("function".to_string()),
                        function: Some(DeltaFunctionCall {
                            name: Some("bash".to_string()),
                            arguments: None,
                        }),
                    }]),
                    reasoning: None,
                },
                logprobs: None,
                finish_reason: None,
            }],
            usage: None,
        };

        // Process first chunk - should cache id and type
        let events1 = converter.process(&chunk1);
        assert!(!events1.is_empty(), "First chunk should produce events");

        // Second chunk: malformed arguments (contains a period at the end)
        // This simulates CloudRu's bug: "{"command":"ls","description":"test"}." instead of valid JSON
        let chunk2 = StreamChunk {
            id: "test-id".to_string(),
            object: "chat.completion.chunk".to_string(),
            created: 123,
            model: "test-model".to_string(),
            choices: vec![StreamChoice {
                index: 0,
                delta: Delta {
                    role: None,
                    content: None,
                    reasoning: None,
                    tool_calls: Some(vec![DeltaToolCall {
                        index: 0,
                        id: None,        // Null id on continuation - should be filled in
                        call_type: None, // Null type on continuation - should be filled in
                        function: Some(DeltaFunctionCall {
                            name: None,
                            arguments: Some(
                                "{\"command\":\"ls\",\"description\":\"test\"}.".to_string(),
                            ),
                        }),
                    }]),
                },
                logprobs: None,
                finish_reason: Some("tool_calls".to_string()),
            }],
            usage: None,
        };

        // This should handle the malformed text without crashing
        let events2 = converter.process(&chunk2);
        println!("events: {:?}", events2);
        assert!(
            !events2.is_empty(),
            "Second chunk should produce events even with malformed arguments"
        );

        println!("converter: {:?}", converter.is_completed());

        // Verify completion state
        assert!(converter.is_completed(), "Should be marked as completed");
    }

    #[tokio::test]
    async fn test_usage_chunk_after_completion_is_forwarded() {
        let mut converter = OpenaiPassthrough::new();

        let finish = StreamChunk {
            id: "test-id".to_string(),
            object: "chat.completion.chunk".to_string(),
            created: 1,
            model: "test-model".to_string(),
            choices: vec![StreamChoice {
                index: 0,
                delta: Delta::default(),
                finish_reason: Some("stop".to_string()),
                logprobs: None,
            }],
            usage: None,
        };
        assert!(!converter.process(&finish).is_empty());
        assert!(converter.is_completed());

        // Gateways send the usage chunk after finish_reason; the completion
        // guard must not swallow the only chunk that carries billed cost.
        let usage = StreamChunk {
            id: "test-id".to_string(),
            object: "chat.completion.chunk".to_string(),
            created: 1,
            model: "test-model".to_string(),
            choices: vec![],
            usage: Some(Usage {
                prompt_tokens: 5,
                completion_tokens: 3,
                total_tokens: 8,
                ..Default::default()
            }),
        };

        let events = converter.process(&usage);
        assert_eq!(events.len(), 1, "usage chunk should be forwarded");
        let text = String::from_utf8(events[0].to_vec()).unwrap();
        assert!(text.contains("\"prompt_tokens\":5"), "got: {text}");
        assert!(text.contains("\"choices\":[]"), "got: {text}");
    }

    #[tokio::test]
    async fn test_usage_attached_to_repeated_finish_chunk_is_forwarded() {
        // Real gateway order: a finish chunk, then another finish chunk that
        // carries `usage`. The second one must survive the completion guard,
        // while a later usage-less duplicate is still dropped.
        let mut converter = OpenaiPassthrough::new();
        let finish = |usage: Option<Usage>| StreamChunk {
            id: "test-id".to_string(),
            object: "chat.completion.chunk".to_string(),
            created: 1,
            model: "test-model".to_string(),
            choices: vec![StreamChoice {
                index: 0,
                delta: Delta::default(),
                finish_reason: Some("stop".to_string()),
                logprobs: None,
            }],
            usage,
        };

        assert_eq!(converter.process(&finish(None)).len(), 1);
        assert!(converter.is_completed());

        let mut usage = Usage {
            prompt_tokens: 123,
            completion_tokens: 45,
            total_tokens: 168,
            ..Default::default()
        };
        usage
            .extra
            .insert("cost".to_string(), serde_json::json!("0.012345"));
        let events = converter.process(&finish(Some(usage)));
        assert_eq!(events.len(), 1, "usage-bearing finish chunk was dropped");
        let text = String::from_utf8(events[0].to_vec()).unwrap();
        assert!(text.contains("\"cost\":\"0.012345\""), "got: {text}");

        // A plain trailing finish chunk is still filtered.
        assert!(converter.process(&finish(None)).is_empty());
    }

    #[tokio::test]
    async fn test_empty_choices_chunk_without_usage_is_dropped() {
        let mut converter = OpenaiPassthrough::new();
        let chunk = StreamChunk {
            id: "test-id".to_string(),
            object: "chat.completion.chunk".to_string(),
            created: 1,
            model: "test-model".to_string(),
            choices: vec![],
            usage: None,
        };
        assert!(converter.process(&chunk).is_empty());
    }

    #[tokio::test]
    async fn test_openai_passthrough_with_malformed_fixture() {
        // Test with actual fixture data that would normally cause issues
        let fixture_data = r#"data: {"id":"test-id","object":"chat.completion.chunk","created":123,"model":"test","choices":[{"index":0,"delta":{"role":"assistant"}}]}

data: {"id":"test-id","object":"chat.completion.chunk","created":123,"model":"test","choices":[{"index":0,"delta":{"tool_calls":[{"id":"tool","type":"function","index":0,"function":{"name":"bash","arguments":""}}]}}]}

data: {"id":"test-id","object":"chat.completion.chunk","created":124,"model":"test","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"command\":\"ls\"}"}}]}}]}

data: {"id":"test-id","object":"chat.completion.chunk","created":125,"model":"test","choices":[{"index":0,"delta":{"tool_calls":[{"id":null,"type":null,"index":0,"function":{"name":null,"arguments":"{\"description\":\"test\"}."}}]}],"finish_reason":"tool_calls"}]}

data: [DONE]

"#;

        let stream = iter(
            fixture_data
                .lines()
                .filter(|line| !line.is_empty())
                .map(|line| Ok(Bytes::from(line.to_string() + "\n\n"))),
        );

        let result_stream = openai_passthrough(stream);
        let mut pinned_stream = Box::pin(result_stream);
        let mut results = Vec::new();

        while let Some(result) = pinned_stream.next().await {
            match result {
                Ok(bytes) => {
                    let text = String::from_utf8_lossy(&bytes);
                    results.push(text.to_string());
                }
                Err(e) => panic!("Unexpected error: {}", e),
            }
        }

        // Should process all events without crashing
        assert!(!results.is_empty(), "Should process some events");

        // Check for [DONE] marker
        assert!(
            results.iter().any(|r| r.contains("[DONE]")),
            "Should include [DONE] marker"
        );
    }

    #[tokio::test]
    async fn test_cloudru_example_stream() {
        let fixture_path = "tests/fixtures/cloudru_malformed_tool_args.jsonl";
        let events = load_fixture_sse_events(fixture_path);
        let stream = tokio_stream::iter(events);
        let converter = UniversalConverter::new(OpenaiPassthrough::default());

        let output: Vec<Result<Bytes, _>> = converter.convert(stream).collect().await;
        let output_bytes: Vec<_> = output.into_iter().filter_map(|r| r.ok()).collect();
        let combined = output_bytes
            .iter()
            .map(|b| String::from_utf8_lossy(b).to_string())
            .collect::<Vec<_>>()
            .join("");

        println!("Combined output: {}", combined);
        println!("Combined length: {}", combined.len());
        // Should have reasoning deltas
        assert!(
            combined.contains("\"reasoning\":\"The"),
            "Missing reasoning content in combined output"
        );

        // Should have tool call start
        assert!(combined.contains("\"tool_calls\""));

        // Should have final completed tool call with cleaned id (filled from prior chunk)
        assert!(combined.contains("\"id\":\"chatcmpl-tool-malformed\""));

        // Should have finish_reason
        // TODO:
        // assert!(combined.contains("\"finish_reason\":\"tool_calls\""));
        //
        // // Should NOT have empty choices array (filtered out)
        // assert!(!combined.contains("choices\":[]"));
        //
        // // Should have [DONE]
        // assert!(combined.contains("[DONE]"));
    }
}
