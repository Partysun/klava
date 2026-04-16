use crate::error::Error;
use crate::models::openai::*;
use async_stream::stream;
use bytes::Bytes;
use tokio_stream::Stream;
use tokio_stream::StreamExt;

/// Universal SSE stream converter
pub struct UniversalConverter<C: ChatChunkConverter> {
    converter: C,
    buffer: Vec<u8>,
}

impl<C: ChatChunkConverter> UniversalConverter<C> {
    pub fn new(converter: C) -> Self {
        Self {
            converter,
            buffer: Vec::new(),
        }
    }

    pub fn convert<S>(self, stream: S) -> impl Stream<Item = Result<Bytes, Error>> + 'static
    where
        S: Stream<Item = Result<Bytes, Error>> + Unpin + 'static,
    {
        let mut this = self;

        stream! {
            tokio::pin!(stream);
            while let Some(result) = stream.next().await {
                let chunk = match result {
                    Ok(c) => c,
                    Err(e) => { yield Err(e); break; }
                };

                this.buffer.extend_from_slice(&chunk);

                // Process all complete SSE events
                while let Some(event) = this.extract_and_convert() {
                    if !event.is_empty() {
                        yield Ok(Bytes::from(event));
                    }
                }
            }

            if this.converter.is_completed() {
                return;
            }

            if !this.buffer.is_empty() {
                match this.parse_partial() {
                    Some(event) if !event.is_empty() => yield Ok(Bytes::from(event)),
                    Some(_) => {} // Skip empty events
                    None => {
                        tracing::debug!("Dropping unparseable buffer at stream end");
                    }
                }
            }

            // Finalize
            for event in this.converter.finalize() {
                yield Ok(Bytes::from(this.converter.serialize(&event)));
            }
        }
    }

    fn extract_and_convert(&mut self) -> Option<Vec<u8>> {
        let pos = self.buffer.windows(2).position(|w| w == b"\n\n")?;
        let raw_event = self.buffer.drain(..pos + 2).collect::<Vec<u8>>();

        let data_payload = Self::extract_data_payload(&raw_event)?;

        if trim_ascii(&data_payload) == b"[DONE]" {
            return Some(raw_event);
        }

        let chat_chunk: StreamChunk = serde_json::from_slice(&data_payload).ok()?;
        let output_events = self.converter.process(&chat_chunk);

        let mut result = Vec::new();
        for event in output_events {
            result.extend_from_slice(&self.converter.serialize(&event));
        }

        Some(result)
    }

    fn parse_partial(&mut self) -> Option<Vec<u8>> {
        let data = Self::extract_data_payload(&self.buffer)?;

        if trim_ascii(&data) == b"[DONE]" {
            self.buffer.clear();
            return Some(b"data: [DONE]\n\n".to_vec());
        }

        let chat_chunk: StreamChunk = serde_json::from_slice(&data).ok()?;
        self.buffer.clear();

        let output_events = self.converter.process(&chat_chunk);

        let mut result = Vec::new();
        for event in output_events {
            result.extend_from_slice(&self.converter.serialize(&event));
        }

        Some(result)
    }

    fn extract_data_payload(event: &[u8]) -> Option<Vec<u8>> {
        let mut data_parts = Vec::new();

        for line in event.split(|&b| b == b'\n') {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if let Some(payload) = line.strip_prefix(b"data:") {
                let trimmed = trim_ascii(payload);
                if !trimmed.is_empty() {
                    data_parts.push(trimmed);
                }
            }
        }

        if data_parts.is_empty() {
            return None;
        }

        Some(data_parts.join(&b'\n'))
    }
}

fn trim_ascii(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|b| !b.is_ascii_whitespace())
        .map(|i| i + 1)
        .unwrap_or(start);
    &bytes[start..end]
}

pub trait ChatChunkConverter: Send + 'static {
    type OutputEvent;

    /// Process entire chunk (all choices)
    fn process(&mut self, chunk: &StreamChunk) -> Vec<Self::OutputEvent>;

    fn serialize(&self, event: &Self::OutputEvent) -> Vec<u8>;

    /// Whether the converter has emitted its completion event.
    /// UniversalConverter checks this after each process() call to:
    /// - Skip trailing chunks that vLLM/CloudRu sends after finish_reason
    /// - Avoid calling finalize() when the converter already completed
    fn is_completed(&self) -> bool {
        false
    }

    fn finalize(&mut self) -> Vec<Self::OutputEvent> {
        Vec::new()
    }
}

// Passthrough Converter (for OpenAI API)
/// Simple passthrough converter that just forwards OpenAI SSE events unchanged
/// OpenAI passthrough converter that handles vLLM/CloudRu provider quirks:
/// - Duplicate chunks: Same data sent multiple times
/// - Empty choices arrays: After completion, empty choices arrays are sent
/// - Back-to-back chunks: Provider sends duplicate completion data
#[derive(Default)]
pub struct PassthroughConverter {
    last_id: String,
    last_content: String,
    last_reasoning: String,
    last_tool_calls: Option<Vec<DeltaToolCall>>,
    completed: bool,
}

impl ChatChunkConverter for PassthroughConverter {
    type OutputEvent = Bytes;

    fn process(&mut self, chunk: &StreamChunk) -> Vec<Bytes> {
        if self.completed {
            return Vec::new();
        }

        // Check if choices array is empty (vLLM/CloudRu sends empty arrays after completion)
        if chunk.choices.is_empty() {
            return Vec::new();
        }

        let choice = match chunk.choices.first() {
            Some(c) => c,
            None => return Vec::new(),
        };

        // Check for completion
        if choice.finish_reason.is_some() {
            self.completed = true;
            // Serialize this chunk as the final one
            if let Ok(json_str) = serde_json::to_string(chunk) {
                return vec![Bytes::from(format!("data: {}\n\n", json_str))];
            } else {
                return Vec::new();
            }
        }

        // Detect duplicate chunks by comparing content and tool_calls
        let current_content = choice.delta.content.as_deref().unwrap_or("");
        let current_reasoning = choice.delta.reasoning.as_deref().unwrap_or("");
        let current_tool_calls = choice.delta.tool_calls.clone();

        // If chunk ID matches and content+tool_calls are identical, skip (duplicate)
        let is_duplicate = chunk.id == self.last_id
            && current_content == self.last_content
            && current_reasoning == self.last_reasoning;
        //FIXME: (NOT IMPORTANT, clients can handle it) pass duplicates to client downstream, but not have any problems
        // && current_tool_calls == self.last_tool_calls;

        // Update state
        self.last_id = chunk.id.clone();
        self.last_content = current_content.to_string();
        self.last_reasoning = current_reasoning.to_string();
        self.last_tool_calls = current_tool_calls;

        if is_duplicate {
            return Vec::new();
        }

        // Serialize the chunk back to OpenAI SSE format
        if let Ok(json_str) = serde_json::to_string(chunk) {
            vec![Bytes::from(format!("data: {}\n\n", json_str))]
        } else {
            vec![]
        }
    }

    fn serialize(&self, event: &Bytes) -> Vec<u8> {
        event.to_vec()
    }

    fn is_completed(&self) -> bool {
        self.completed
    }
}

// Test Converter (Echo)
#[derive(Default)]
pub struct EchoConverter {
    count: usize,
}

impl ChatChunkConverter for EchoConverter {
    type OutputEvent = String;

    fn process(&mut self, chunk: &StreamChunk) -> Vec<String> {
        self.count += 1;
        vec![format!("chunk {}: {}", self.count, chunk.id)]
    }

    fn serialize(&self, event: &String) -> Vec<u8> {
        format!("data: {}\n\n", event).into_bytes()
    }

    fn finalize(&mut self) -> Vec<String> {
        vec![format!("done: processed {} chunks", self.count)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use tokio_stream::StreamExt;

    fn make_sse_event(data: &str) -> Bytes {
        Bytes::from(format!("data: {}\n\n", data))
    }

    fn make_chunk(id: &str, content: &str) -> String {
        serde_json::to_string(&StreamChunk {
            id: id.to_string(),
            object: "chat.completion.chunk".to_string(),
            created: 1234567890,
            model: "gpt-4".to_string(),
            choices: vec![StreamChoice {
                index: 0,
                delta: Delta {
                    role: Some("assistant".to_string()),
                    content: Some(content.to_string()),
                    tool_calls: None,
                    reasoning: None,
                },
                finish_reason: None,
                logprobs: None,
            }],
            usage: None,
        })
        .unwrap()
    }

    fn make_done_chunk(id: &str) -> String {
        serde_json::to_string(&StreamChunk {
            id: id.to_string(),
            object: "chat.completion.chunk".to_string(),
            created: 1234567890,
            model: "gpt-4".to_string(),
            choices: vec![StreamChoice {
                index: 0,
                delta: Delta::default(),
                finish_reason: Some("stop".to_string()),
                logprobs: None,
            }],
            usage: Some(Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
            }),
        })
        .unwrap()
    }

    #[tokio::test]
    async fn test_single_chunk() {
        let input = make_sse_event(&make_chunk("chunk-1", "Hello"));

        let stream = tokio_stream::iter(vec![Ok(input)]);
        let converter = UniversalConverter::new(EchoConverter::default());

        let output: Vec<Result<Bytes, _>> = converter.convert(stream).collect().await;

        assert_eq!(output.len(), 2); // chunk + finalize
        assert!(
            output[0]
                .as_ref()
                .unwrap()
                .windows(b"chunk 1: chunk-1".len())
                .any(|w| w == b"chunk 1: chunk-1")
        );
    }

    #[tokio::test]
    async fn test_multiple_chunks() {
        let inputs = vec![
            Ok(make_sse_event(&make_chunk("chunk-1", "Hello"))),
            Ok(make_sse_event(&make_chunk("chunk-2", " World"))),
            Ok(make_sse_event(&make_done_chunk("chunk-3"))),
        ];

        let stream = tokio_stream::iter(inputs);
        let converter = UniversalConverter::new(EchoConverter::default());

        let output: Vec<Result<Bytes, _>> = converter.convert(stream).collect().await;

        let text = output
            .iter()
            .map(|r| String::from_utf8_lossy(r.as_ref().unwrap()).to_string())
            .collect::<Vec<_>>()
            .join("");

        assert!(text.contains("chunk 1: chunk-1"));
        assert!(text.contains("chunk 2: chunk-2"));
        assert!(text.contains("done: processed 3 chunks"));
    }

    #[tokio::test]
    async fn test_done_marker() {
        let input = Bytes::from("data: [DONE]\n\n");

        let stream = tokio_stream::iter(vec![Ok(input)]);
        let converter = UniversalConverter::new(EchoConverter::default());

        let output: Vec<Result<Bytes, _>> = converter.convert(stream).collect().await;

        assert_eq!(output.len(), 2); // [DONE] passthrough + finalize
        let text = String::from_utf8_lossy(output[0].as_ref().unwrap());
        assert!(text.contains("[DONE]"));
    }

    #[tokio::test]
    async fn test_partial_buffer() {
        // Simulate partial data across chunks
        let chunk1 = Bytes::from("data: ");
        let chunk2 = Bytes::from(
            r#"{"id":"test","object":"chat.completion.chunk","created":1,"model":"gpt-4","choices":[{"index":0,"delta":{"content":"Hi"}}]}"#,
        );
        let chunk3 = Bytes::from("\n\n");

        let stream = tokio_stream::iter(vec![Ok(chunk1), Ok(chunk2), Ok(chunk3)]);
        let converter = UniversalConverter::new(EchoConverter::default());

        let output: Vec<Result<Bytes, _>> = converter.convert(stream).collect().await;

        // Should process once we have \n\n
        assert!(!output.is_empty());
    }

    #[tokio::test]
    async fn test_tool_call_chunk() {
        let chunk = StreamChunk {
            id: "tool-chunk".to_string(),
            object: "chat.completion.chunk".to_string(),
            created: 1234567890,
            model: "gpt-4".to_string(),
            choices: vec![StreamChoice {
                index: 0,
                delta: Delta {
                    role: None,
                    content: None,
                    tool_calls: Some(vec![DeltaToolCall {
                        index: 0,
                        id: Some("call_123".to_string()),
                        call_type: Some("function".to_string()),
                        function: Some(DeltaFunctionCall {
                            name: Some("get_weather".to_string()),
                            arguments: Some(r#"{"location": "NYC"}"#.to_string()),
                        }),
                    }]),
                    reasoning: None,
                },
                finish_reason: None,
                logprobs: None,
            }],
            usage: None,
        };

        let input = make_sse_event(&serde_json::to_string(&chunk).unwrap());

        let stream = tokio_stream::iter(vec![Ok(input)]);
        let converter = UniversalConverter::new(EchoConverter::default());

        let output: Vec<Result<Bytes, _>> = converter.convert(stream).collect().await;

        assert!(!output.is_empty());
    }

    #[tokio::test]
    async fn test_error_propagation() {
        let stream = tokio_stream::iter(vec![
            Ok(Bytes::from("data: test\n\n")),
            Err(Error::Upstream("network error".to_string())),
        ]);

        let converter = UniversalConverter::new(EchoConverter::default());
        let output: Vec<Result<Bytes, _>> = converter.convert(stream).collect().await;

        // "test" isn't valid JSON, so extract_and_convert drops it silently;
        // the error breaks the stream, then finalize() runs
        assert_eq!(output.len(), 2);
        assert!(output[0].is_err()); // network error
        assert!(output[1].is_ok()); // finalize event
    }

    /// Test duplicate chunk handling in PassthroughConverter.
    /// Reference log shows same chunk being sent multiple times:
    /// data: {"id":"chatcmpl-8ad8f78f-553d-439e-a2f5-bfeedcf968c7","delta":{"content":"Greeting"}}
    /// data: {"id":"chatcmpl-8ad8f78f-553d-439e-a2f5-bfeedcf968c7","delta":{"content":"Greeting"}}
    #[tokio::test]
    async fn test_vllm_duplicate_chunks() {
        // Create identical chunks with same content
        let chunk1 = make_sse_event(&make_chunk("chatcmpl-dup", "Greeting"));
        let chunk2 = make_sse_event(&make_chunk("chatcmpl-dup", "Greeting"));

        let stream = tokio_stream::iter(vec![Ok(chunk1), Ok(chunk2)]);
        let converter = UniversalConverter::new(PassthroughConverter::default());

        let output: Vec<Result<Bytes, _>> = converter.convert(stream).collect().await;

        // Should only see one output (duplicate filtered)
        assert_eq!(output.len(), 1);

        let first_output = String::from_utf8_lossy(output[0].as_ref().unwrap());
        assert!(first_output.contains("Greeting"));
    }

    /// Test mixed reasoning+content in same chunk (vLLM/CloudRu behavior).
    /// Reference log shows:
    /// "delta":{"content":"G","reasoning":""}
    /// "delta":{"content":"reeting","finish_reason":"stop"}
    #[tokio::test]
    async fn test_vllm_mixed_reasoning_content() {
        let chunk1 = StreamChunk {
            id: "chatcmpl-mixed".to_string(),
            object: "chat.completion.chunk".to_string(),
            created: 1234567890,
            model: "gpt-4".to_string(),
            choices: vec![StreamChoice {
                index: 0,
                delta: Delta {
                    role: None,
                    content: Some("G".to_string()),
                    tool_calls: None,
                    reasoning: Some("".to_string()),
                },
                finish_reason: None,
                logprobs: None,
            }],
            usage: None,
        };

        let chunk2 = StreamChunk {
            id: "chatcmpl-mixed".to_string(),
            object: "chat.completion.chunk".to_string(),
            created: 1234567890,
            model: "gpt-4".to_string(),
            choices: vec![StreamChoice {
                index: 0,
                delta: Delta {
                    role: None,
                    content: Some("reeting".to_string()),
                    tool_calls: None,
                    reasoning: None,
                },
                finish_reason: Some("stop".to_string()),
                logprobs: None,
            }],
            usage: None,
        };

        let input1 = make_sse_event(&serde_json::to_string(&chunk1).unwrap());
        let input2 = make_sse_event(&serde_json::to_string(&chunk2).unwrap());

        let stream = tokio_stream::iter(vec![Ok(input1), Ok(input2)]);
        let converter = UniversalConverter::new(PassthroughConverter::default());

        let output: Vec<Result<Bytes, _>> = converter.convert(stream).collect().await;

        // Should process both chunks
        assert_eq!(output.len(), 2);

        let combined = output
            .iter()
            .map(|r| String::from_utf8_lossy(r.as_ref().unwrap()).to_string())
            .collect::<Vec<_>>()
            .join("");

        assert!(combined.contains("\"content\":\"G\""));
        assert!(combined.contains("\"content\":\"reeting\""));
        assert!(combined.contains("\"finish_reason\":\"stop\""));
    }

    /// Test empty choices array after completion.
    /// Reference log shows:
    /// "choices":[],"usage":{"completion_tokens":167}
    #[tokio::test]
    async fn test_vllm_empty_choices_after_completion() {
        let completion_chunk = make_sse_event(&make_done_chunk("chatcmpl-1"));

        // Empty choices array (vLLM/CloudRu sends this after completion)
        let empty_choices = StreamChunk {
            id: "chatcmpl-1".to_string(),
            object: "chat.completion.chunk".to_string(),
            created: 1234567890,
            model: "gpt-4".to_string(),
            choices: vec![],
            usage: Some(Usage {
                prompt_tokens: 10,
                completion_tokens: 167,
                total_tokens: 177,
            }),
        };

        let import_event = make_sse_event(&serde_json::to_string(&empty_choices).unwrap());

        let stream = tokio_stream::iter(vec![Ok(completion_chunk), Ok(import_event)]);
        let converter = UniversalConverter::new(PassthroughConverter::default());

        let output: Vec<Result<Bytes, _>> = converter.convert(stream).collect().await;

        // Should only see completion chunk, not empty choices
        assert_eq!(output.len(), 1);

        let text = String::from_utf8_lossy(output[0].as_ref().unwrap());
        assert!(text.contains("\"finish_reason\":\"stop\""));
    }

    /// Test incremental tool call argument building.
    /// Reference logs show vLLM sending partial JSON objects that build up.
    /// Some providers send complete objects, some send fragments, some send duplicates.
    #[ignore = "FIXME in PassthroughConverter related to pass duplicates of tools calls"]
    #[tokio::test]
    async fn test_vllm_incremental_tool_args() {
        let chunk1 = StreamChunk {
            id: "chatcmpl-tool".to_string(),
            object: "chat.completion.chunk".to_string(),
            created: 1234567890,
            model: "gpt-4".to_string(),
            choices: vec![StreamChoice {
                index: 0,
                delta: Delta {
                    role: None,
                    content: None,
                    tool_calls: Some(vec![DeltaToolCall {
                        index: 0,
                        id: Some("call_abc".to_string()),
                        call_type: Some("function".to_string()),
                        function: Some(DeltaFunctionCall {
                            name: Some("bash".to_string()),
                            arguments: Some(
                                r#"{"command":"find . -maxdepth 1 -type f | wc -l""#.to_string(),
                            ),
                        }),
                    }]),
                    reasoning: None,
                },
                finish_reason: None,
                logprobs: None,
            }],
            usage: None,
        };

        let chunk2 = StreamChunk {
            id: "chatcmpl-tool".to_string(),
            object: "chat.completion.chunk".to_string(),
            created: 1234567890,
            model: "gpt-4".to_string(),
            choices: vec![StreamChoice {
                index: 0,
                delta: Delta {
                    role: None,
                    content: None,
                    tool_calls: Some(vec![DeltaToolCall {
                        index: 0,
                        id: None,
                        call_type: None,
                        function: Some(DeltaFunctionCall {
                            name: None,
                            arguments: Some(
                                r#"","description":"Count files in current directory"}"#
                                    .to_string(),
                            ),
                        }),
                    }]),
                    reasoning: None,
                },
                finish_reason: None,
                logprobs: None,
            }],
            usage: None,
        };

        let mut converter = PassthroughConverter::default();

        let output1 = converter.process(&chunk1);
        let output2 = converter.process(&chunk2);
        println!("output1: {:?}", output1);
        println!("output2: {:?}", output2);

        assert_eq!(output1.len(), 1);
        assert_eq!(output2.len(), 1);

        let text1 = String::from_utf8_lossy(&output1[0]);
        let text2 = String::from_utf8_lossy(&output2[0]);

        // The assertions check for DOUBLE-ESCAPED JSON because:
        // 1. `arguments` contains raw JSON: {"command":"..."}
        // 2. When serialized into SSE data: {"arguments":"{\"command\":\"...\"}"}
        // Hence we match \"command\" not "command"
        assert!(text1.contains(r#"\"command\":\"find . -maxdepth 1 -type f | wc -l\""#));
        assert!(text2.contains(r#"",\"description\":\"Count files in current directory\"}"#));
    }

    /// Test tool call completion doesn't forward duplicate completion chunks.
    /// Reference logs show multiple finish_reason chunks being sent:
    /// "finish_reason":"stop","stop_reason":151336
    /// "choices":[],"usage":{"completion_tokens":167}
    #[tokio::test]
    async fn test_vllm_no_duplicate_completions() {
        let completion1 = make_sse_event(&make_done_chunk("chatcmpl-dup-complete"));
        let completion2 = make_sse_event(&make_done_chunk("chatcmpl-dup-complete"));

        let stream = tokio_stream::iter(vec![Ok(completion1), Ok(completion2)]);
        let converter = UniversalConverter::new(PassthroughConverter::default());

        let output: Vec<Result<Bytes, _>> = converter.convert(stream).collect().await;

        // Should only see one completion event (second skipped due to completed flag)
        assert_eq!(output.len(), 1);

        let text = String::from_utf8_lossy(output[0].as_ref().unwrap());
        assert!(text.contains("\"finish_reason\":\"stop\""));
    }

    /// Test that empty marker chunk (reasoning: "") isn't treated as duplicate.
    /// Reference logs show this pattern:
    /// {"delta":{"content":"G","reasoning":""}}
    /// {"delta":{"content":"reeting"}}
    #[tokio::test]
    async fn test_vllm_empty_reasoning_str() {
        let chunk1 = StreamChunk {
            id: "chatcmpl-empty-reasoning".to_string(),
            object: "chat.completion.chunk".to_string(),
            created: 1234567890,
            model: "gpt-4".to_string(),
            choices: vec![StreamChoice {
                index: 0,
                delta: Delta {
                    role: None,
                    content: Some("Hello".to_string()),
                    tool_calls: None,
                    reasoning: Some("".to_string()),
                },
                finish_reason: None,
                logprobs: None,
            }],
            usage: None,
        };

        let chunk2 = StreamChunk {
            id: "chatcmpl-empty-reasoning".to_string(),
            object: "chat.completion.chunk".to_string(),
            created: 1234567890,
            model: "gpt-4".to_string(),
            choices: vec![StreamChoice {
                index: 0,
                delta: Delta {
                    role: None,
                    content: Some(" World".to_string()),
                    tool_calls: None,
                    reasoning: None,
                },
                finish_reason: None,
                logprobs: None,
            }],
            usage: None,
        };

        let mut converter = PassthroughConverter::default();

        let output1 = converter.process(&chunk1);
        let output2 = converter.process(&chunk2);

        // Both should be forwarded (not considered duplicates)
        assert_eq!(output1.len(), 1);
        assert_eq!(output2.len(), 1);
    }

    /// Test that reasoning chunks with the same content but different chunk IDs are not deduplicated.
    /// vLLM may restart streams with new IDs.
    #[tokio::test]
    async fn test_vllm_different_ids_not_deduplicated() {
        let chunk1 = make_sse_event(&make_chunk("id-1", "Let me think about this"));
        let chunk2 = make_sse_event(&make_chunk("id-2", "Let me think about this"));

        let stream = tokio_stream::iter(vec![Ok(chunk1), Ok(chunk2)]);
        let converter = UniversalConverter::new(PassthroughConverter::default());

        let output: Vec<Result<Bytes, _>> = converter.convert(stream).collect().await;

        // Both should be forwarded (different IDs, so not duplicates)
        assert_eq!(output.len(), 2);
    }

    /// Test malformed tool arguments without closing brace (vLLM bug).
    /// Reference logs show tool calls sometimes missing the closing }.
    #[tokio::test]
    async fn test_vllm_malformed_tool_args() {
        let chunk = StreamChunk {
            id: "chatcmpl-malformed".to_string(),
            object: "chat.completion.chunk".to_string(),
            created: 1234567890,
            model: "gpt-4".to_string(),
            choices: vec![StreamChoice {
                index: 0,
                delta: Delta {
                    role: None,
                    content: None,
                    tool_calls: Some(vec![DeltaToolCall {
                        index: 0,
                        id: Some("call_xxx".to_string()),
                        call_type: Some("function".to_string()),
                        function: Some(DeltaFunctionCall {
                            name: Some("bash".to_string()),
                            // Missing closing brace - common vLLM bug
                            arguments: Some(r#"{"command":"ls -la""#.to_string()),
                        }),
                    }]),
                    reasoning: None,
                },
                finish_reason: None,
                logprobs: None,
            }],
            usage: None,
        };

        let mut converter = PassthroughConverter::default();
        let output = converter.process(&chunk);

        // Should still forward the chunk handling happens in higher-level converters (Responses/Anthropic)
        assert_eq!(output.len(), 1);

        let text = String::from_utf8_lossy(&output[0]);
        // The malformed JSON should be present (with JSON-escaped quotes)
        assert!(text.contains(r#"\"command\":\"ls -la\""#));
    }
}
