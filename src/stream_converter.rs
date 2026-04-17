use std::collections::HashMap;

use crate::error::Error;
use crate::models::openai::*;
use async_stream::stream;
use bytes::Bytes;
use serde_json::{Map, Value};
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

/// OpenAI passthrough converter that handles vLLM/CloudRu/Qwen provider quirks:
/// - Duplicate chunks: Same data sent multiple times
/// - Empty choices arrays: After completion, empty choices arrays are sent
/// - Back-to-back chunks: Provider sends duplicate completion data
/// - Null tool call id/type: Providers send id=null/type=null on continuation chunks;
///   we track IDs by index and fill them in so downstream always gets valid strings
/// - Duplicate tool call arguments: Qwen/CloudRu resend the same arguments fragment;
///   we track accumulated args per index and deduplicate by prefix
#[derive(Default)]
pub struct OpenaiConverter {
    last_id: String,
    last_content: String,
    last_reasoning: String,
    last_tool_calls: Option<Vec<DeltaToolCall>>,
    /// Maps tool_call index -> id string. Providers send id only on the first chunk
    /// for a tool call; continuation chunks omit it. We stash the id here so we can
    /// fill it back in on downstream serialization (clients expect a string, not null).
    tool_call_ids: HashMap<usize, String>,
    /// Maps tool_call index -> accumulated arguments string. Used for duplicate detection:
    /// if a new args fragment starts with our accumulated args, we emit only the suffix.
    tool_call_args: HashMap<usize, String>,
    completed: bool,
}

impl OpenaiConverter {
    /// Accumulate arguments with duplicate detection for vLLM/CloudRu/Qwen.
    /// Returns the cleaned tool call with only the new arguments fragment,
    /// or None if this is a pure duplicate (same content we already have).
    fn dedup_tool_call_args(
        &mut self,
        tc: &DeltaToolCall,
        _is_completion: bool,
    ) -> Option<DeltaToolCall> {
        let args = tc.function.as_ref().and_then(|f| f.arguments.as_deref())?;
        if args.is_empty() {
            return Some(tc.clone());
        }

        let index = tc.index;
        let accumulated = self.tool_call_args.entry(index).or_default();

        // Fragments: always accept
        let Ok(new_json) = serde_json::from_str::<Value>(args) else {
            accumulated.push_str(args);
            return Some(self.with_args(tc, args));
        };

        // Fresh start
        if accumulated.is_empty() {
            accumulated.push_str(args);
            return Some(self.with_args(tc, args));
        }

        // When accumulated is complete JSON object
        if accumulated.ends_with('}') {
            let trimmed_args = args.trim_start();

            // Continuation fragment starting with comma - skip comma and append delta
            if trimmed_args.starts_with(',') {
                let delta = &trimmed_args[1..];
                accumulated.push_str(delta);
                return Some(self.with_args(tc, delta));
            }

            if trimmed_args.starts_with('{') {
                // Parse accumulated as JSON
                let Ok(acc_json) = serde_json::from_str::<Value>(accumulated) else {
                    // Accumulated is invalid, replace with new
                    accumulated.clear();
                    accumulated.push_str(args);
                    return Some(self.with_args(tc, args));
                };

                if acc_json.is_object() {
                    // Both are complete JSON objects
                    if acc_json == new_json {
                        // Identical complete objects - skip duplicate
                        return None;
                    }

                    // Different complete objects - extract new keys and append properly
                    let acc_map = acc_json.as_object().unwrap();
                    let new_map = new_json.as_object().unwrap();

                    let mut new_keys = Map::new();
                    for (k, v) in new_map {
                        if !acc_map.contains_key(k) {
                            new_keys.insert(k.clone(), v.clone());
                        }
                    }

                    if new_keys.is_empty() {
                        return None; // All keys exist - duplicate
                    }

                    // FIXED: Instead of string concatenation, build proper merged JSON
                    // Copy accumulated and add new keys
                    let mut merged = acc_json.as_object().unwrap().clone();
                    merged.extend(new_keys);

                    let merged_json = serde_json::to_string(&merged).unwrap_or_default();
                    *accumulated = merged_json.clone();
                    return Some(self.with_args(tc, &merged_json));
                }
            }

            // Other continuation fragments
            accumulated.push_str(trimmed_args);
            return Some(self.with_args(tc, trimmed_args));
        }

        // Continuation: emit only suffix if new starts with accumulated
        if args.starts_with(accumulated.as_str()) {
            let delta = &args[accumulated.len()..];
            if delta.is_empty() {
                return None;
            }
            accumulated.push_str(delta);
            return Some(self.with_args(tc, delta));
        }

        // Overlap check: emit only new keys
        let Ok(acc_json) = serde_json::from_str::<Value>(accumulated) else {
            accumulated.push_str(args);
            return Some(self.with_args(tc, args));
        };

        let (Some(acc_map), Some(new_map)) = (acc_json.as_object(), new_json.as_object()) else {
            return None;
        };

        let mut new_keys = Map::new();
        for (k, v) in new_map {
            if !acc_map.contains_key(k) {
                new_keys.insert(k.clone(), v.clone());
            }
        }

        if new_keys.is_empty() {
            return None;
        }

        // FIXED: Build proper merged JSON instead of string concatenation
        let mut merged = acc_json.as_object().unwrap().clone();
        merged.extend(new_keys);
        let merged_json = serde_json::to_string(&merged).unwrap_or_default();
        *accumulated = merged_json.clone();
        Some(self.with_args(tc, &merged_json))
    }

    fn with_args(&self, tc: &DeltaToolCall, args: &str) -> DeltaToolCall {
        DeltaToolCall {
            function: Some(DeltaFunctionCall {
                name: tc.function.as_ref().and_then(|f| f.name.clone()),
                arguments: Some(args.to_string()),
            }),
            ..tc.clone()
        }
    }

    /// Clean up tool calls by removing null id/type values from the last tool call chunk.
    /// CloudRu upstream sends tool calls with id=null/type=null in final completion chunks.
    /// Also validates and repairs JSON arguments (only on completion).
    ///
    /// For non-completion chunks, this does minimal work to avoid breaking incremental streaming.
    /// Only modifies data when changes are actually needed (null id/type on completion, malformed args).
    ///
    /// CRITICAL: On completion chunks (is_completion=true), we DO NOT emit arguments.
    /// The downstream client has already built complete arguments from incremental chunks.
    /// Emitting arguments on completion would create malformed JSON with duplicates.
    fn clean_tool_calls(&mut self, chunk: &StreamChunk, is_completion: bool) -> StreamChunk {
        // Early return if no tool calls to process
        let tool_calls = match chunk
            .choices
            .first()
            .and_then(|c| c.delta.tool_calls.as_ref())
            .filter(|tcs| !tcs.is_empty())
        {
            Some(tcs) => tcs,
            None => return chunk.clone(),
        };

        let mut cleaned_tcs: Vec<DeltaToolCall> = Vec::new();

        for tc in tool_calls {
            // Cache tool call ID (always)
            if let Some(id) = tc.id.as_ref().filter(|id| !id.is_empty()) {
                self.tool_call_ids.insert(tc.index, id.clone());
            }

            // On completion: just mark as completed, don't emit arguments
            // The client has already built full arguments from incremental chunks
            let function = if is_completion {
                // Omit arguments entirely on completion - just pass name through
                tc.function.as_ref().and_then(|f| {
                    f.name.as_ref().map(|name| DeltaFunctionCall {
                        name: Some(name.clone()),
                        arguments: None, // No arguments on completion!
                    })
                })
            } else {
                // For non-completion chunks, deduplicate arguments normally
                let Some(deduped_tc) = self.dedup_tool_call_args(tc, is_completion) else {
                    continue;
                };
                Self::repair_arguments(&deduped_tc, tc.index);
                deduped_tc.function
            };

            // Resolve id/type from prior state if null
            let resolved_id = tc
                .id
                .clone()
                .or_else(|| self.tool_call_ids.get(&tc.index).cloned());
            let resolved_type = tc.call_type.clone().or_else(|| {
                self.tool_call_ids
                    .contains_key(&tc.index)
                    .then(|| "function".to_string())
            });

            cleaned_tcs.push(DeltaToolCall {
                index: tc.index,
                id: resolved_id,
                call_type: resolved_type,
                function,
            });
        }

        let mut result = chunk.clone();
        result.choices[0].delta.tool_calls = if cleaned_tcs.is_empty() {
            None
        } else {
            Some(cleaned_tcs)
        };
        result
    }

    // Extracted helper for JSON repair
    fn repair_arguments(deduped_tc: &DeltaToolCall, index: usize) -> Option<DeltaFunctionCall> {
        let func = deduped_tc.function.as_ref()?;
        let args_str = func.arguments.as_ref()?;

        // Already valid? Return as-is
        if serde_json::from_str::<serde_json::Value>(args_str).is_ok() {
            return deduped_tc.function.clone();
        }

        let trimmed = args_str.trim_end();

        let mut fixes = Vec::new();

        // Fix 1: Period instead of closing brace - replace trailing period with }
        if trimmed.ends_with('.') {
            fixes.push(format!("{}}}", &trimmed[..trimmed.len() - 1]));
        }

        // Fix 2: Append closing brace
        fixes.push(format!("{} }}", trimmed));

        // Fix 3: Replace trailing punctuation with }
        let last_char = trimmed.chars().last();
        if last_char.map_or(false, |c| c == '.' || c == ',' || c == ':') {
            fixes.push(format!("{}}}", &trimmed[..trimmed.len() - 1]));
        }

        // Fix 4: Replace last char with }}
        fixes.push(format!(
            "{}{{}}",
            &trimmed[..trimmed.len().saturating_sub(1)]
        ));

        for fix in &fixes {
            if serde_json::from_str::<serde_json::Value>(fix).is_ok() {
                tracing::debug!(
                    "Fixed incomplete arguments for tool call index {index}: \"{args_str}\" -> \"{fix}\""
                );
                return Some(DeltaFunctionCall {
                    name: func.name.clone(),
                    arguments: Some(fix.clone()),
                });
            }
        }

        tracing::debug!(
            "Malformed arguments for tool call index {index} on completion: \"{args_str}\". Replacing with empty object."
        );
        Some(DeltaFunctionCall {
            name: func.name.clone(),
            arguments: Some("{}".to_string()),
        })
    }
}

impl ChatChunkConverter for OpenaiConverter {
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
            // Clear accumulated args for all tool calls on completion
            if choice.finish_reason.as_deref() == Some("stop") {
                self.tool_call_args.clear();
            }
            self.completed = true;
            // Clean up tool calls before serialization (remove null id/type values, validate arguments)
            let cleaned_chunk = self.clean_tool_calls(chunk, true);
            // Serialize this chunk as the final one
            if let Ok(json_str) = serde_json::to_string(&cleaned_chunk) {
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
        // Tool call argument chunks must never be treated as duplicates since they carry
        // incremental argument fragments with the same id/content/reasoning as the initial chunk.
        let has_tool_calls = current_tool_calls
            .as_ref()
            .is_some_and(|tcs| !tcs.is_empty());
        let is_duplicate = !has_tool_calls
            && chunk.id == self.last_id
            && current_content == self.last_content
            && current_reasoning == self.last_reasoning;

        // Update state
        self.last_id = chunk.id.clone();
        self.last_content = current_content.to_string();
        self.last_reasoning = current_reasoning.to_string();
        self.last_tool_calls = current_tool_calls;

        if is_duplicate {
            return Vec::new();
        }

        // Clean up tool calls before serialization (remove null id/type values)
        let cleaned_chunk = self.clean_tool_calls(chunk, false);

        // Serialize the chunk back to OpenAI SSE format
        if let Ok(json_str) = serde_json::to_string(&cleaned_chunk) {
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
    use crate::test_helpers::*;
    use bytes::Bytes;
    use tokio_stream::StreamExt;

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

    /// Test duplicate chunk handling in OpenaiConverter.
    /// Reference log shows same chunk being sent multiple times:
    /// data: {"id":"chatcmpl-8ad8f78f-553d-439e-a2f5-bfeedcf968c7","delta":{"content":"Greeting"}}
    /// data: {"id":"chatcmpl-8ad8f78f-553d-439e-a2f5-bfeedcf968c7","delta":{"content":"Greeting"}}
    #[tokio::test]
    async fn test_vllm_duplicate_chunks() {
        // Create identical chunks with same content
        let chunk1 = make_sse_event(&make_chunk("chatcmpl-dup", "Greeting"));
        let chunk2 = make_sse_event(&make_chunk("chatcmpl-dup", "Greeting"));

        let stream = tokio_stream::iter(vec![Ok(chunk1), Ok(chunk2)]);
        let converter = UniversalConverter::new(OpenaiConverter::default());

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
        let converter = UniversalConverter::new(OpenaiConverter::default());

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
        let converter = UniversalConverter::new(OpenaiConverter::default());

        let output: Vec<Result<Bytes, _>> = converter.convert(stream).collect().await;

        // Should only see completion chunk, not empty choices
        assert_eq!(output.len(), 1);

        let text = String::from_utf8_lossy(output[0].as_ref().unwrap());
        assert!(text.contains("\"finish_reason\":\"stop\""));
    }

    /// Test incremental tool call argument building.
    /// Reference logs show vLLM sending partial JSON objects that build up.
    /// Some providers send complete objects, some send fragments, some send duplicates.
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

        let mut converter = OpenaiConverter::default();

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
        let converter = UniversalConverter::new(OpenaiConverter::default());

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

        let mut converter = OpenaiConverter::default();

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
        let converter = UniversalConverter::new(OpenaiConverter::default());

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

        let mut converter = OpenaiConverter::default();
        let output = converter.process(&chunk);

        // Should still forward the chunk handling happens in higher-level converters (Responses/Anthropic)
        assert_eq!(output.len(), 1);

        let text = String::from_utf8_lossy(&output[0]);
        // The malformed JSON should be present (with JSON-escaped quotes)
        assert!(text.contains(r#"\"command\":\"ls -la\""#));
    }

    /// Integration test using real CloudRu logs (reasoning + tool call).
    /// Tests: reasoning delta →tool call start → incremental args → complete args → finish →
    /// empty choices array post-completion → [DONE] marker.
    ///
    /// Fixture contains authentic provider quirks: null id/type on completion chunk,
    /// incremental JSON building, empty choices after finish_reason.
    #[tokio::test]
    async fn test_cloudru_real_stream() {
        let fixture_path = "tests/fixtures/cloudru_tool_call_stream.jsonl";
        let events = load_fixture_sse_events(fixture_path);

        let stream = tokio_stream::iter(events);
        let converter = UniversalConverter::new(OpenaiConverter::default());

        let output: Vec<Result<Bytes, _>> = converter.convert(stream).collect().await;

        // Verify key behaviors:
        // 1. Empty initial content chunk should be filtered (OpenaiConverter dedup)
        // 2. Reasoning chunks should pass through
        // 3. Tool call chunks should pass through with incremental args
        // 4. Completion chunk with null id/type should be cleaned
        // 5. Empty choices array should be filtered
        // 6. [DONE] should pass through
        let output_bytes: Vec<_> = output.into_iter().filter_map(|r| r.ok()).collect();
        let combined = output_bytes
            .iter()
            .map(|b| String::from_utf8_lossy(b).to_string())
            .collect::<Vec<_>>()
            .join("");

        // Should have reasoning deltas
        assert!(combined.contains("\"reasoning\":\"The"));

        // Should have tool call start
        assert!(combined.contains("\"tool_calls\""));

        // Should have final completed tool call with cleaned id (filled from prior chunk)
        assert!(combined.contains("\"id\":\"chatcmpl-tool-bd7ecfdb179677c9\""));

        // Should have finish_reason
        assert!(combined.contains("\"finish_reason\":\"tool_calls\""));

        // Should NOT have empty choices array (filtered out)
        assert!(!combined.contains("choices\":[]"));

        // Should have [DONE]
        assert!(combined.contains("[DONE]"));
    }

    /// Test that clean_tool_calls removes null id/type from completion chunks.
    /// Reference log shows CloudRu sending:
    ///"tool_calls":[{"id":null,"type":null,"index":3,"function":{...}}]
    #[tokio::test]
    async fn test_cloudu_null_tool_call_fields() {
        let chunk = StreamChunk {
            id: "chatcmpl-null-fields".to_string(),
            object: "chat.completion.chunk".to_string(),
            created: 1234567890,
            model: "gpt-4".to_string(),
            choices: vec![StreamChoice {
                index: 0,
                delta: Delta {
                    role: None,
                    content: None,
                    tool_calls: Some(vec![DeltaToolCall {
                        index: 3,
                        id: None,
                        call_type: None,
                        function: Some(DeltaFunctionCall {
                            name: None,
                            arguments: Some(
                                r#"{"filePath":
"/Users/yura/Dev/klava/src/cli.rs"}"#
                                    .to_string(),
                            ),
                        }),
                    }]),
                    reasoning: None,
                },
                finish_reason: Some("tool_calls".to_string()),
                logprobs: None,
            }],
            usage: None,
        };

        let mut converter = OpenaiConverter::default();
        let output = converter.process(&chunk);

        // Should process the completion chunk
        assert_eq!(output.len(), 1);

        let text = String::from_utf8_lossy(&output[0]);
        // Should contain completion marker
        assert!(text.contains("finish_reason"));

        // Extract JSON from SSE format: "data: {json}\n\n"
        let json_start = text.find("data: ").unwrap() + 6; // Skip "data: "
        let json_text = text[json_start..].trim();

        // Verify JSON is valid
        let parsed: serde_json::Value = serde_json::from_str(json_text).unwrap();
        assert!(parsed["choices"][0]["delta"]["tool_calls"].is_array());
    }

    /// Test that malformed tool arguments are replaced with empty object on completion.
    /// Reference log shows CloudRu sending malformed JSON with duplicate keys.
    #[tokio::test]
    #[ignore]
    async fn test_malformed_tool_arguments_replaced() {
        let malformed_args = r#"{"filePath":"/Users/yura/Dev/klava/src/config.rs","limit":10,"offset":95{"filePath": "/Users/yura/Dev/klava/src/config.rs", "limit": 10, "offset": 95}"#;

        // First send a normal chunk to set up the tool call ID
        let first_chunk = StreamChunk {
            id: "chatcmpl-malformed-args".to_string(),
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
                            name: Some("invalid".to_string()),
                            arguments: Some(malformed_args.to_string()),
                        }),
                    }]),
                    reasoning: None,
                },
                finish_reason: Some("tool_calls".to_string()), // This is a completion chunk
                logprobs: None,
            }],
            usage: None,
        };

        let mut converter = OpenaiConverter::default();
        let output = converter.process(&first_chunk);

        // Should process the completion chunk with cleaned arguments
        assert_eq!(output.len(), 1);

        let text = String::from_utf8_lossy(&output[0]);

        // Extract JSON from SSE format: "data: {json}\n\n"
        let json_start = text.find("data: ").unwrap() + 6; // Skip "data: "
        let json_text = text[json_start..].trim();
        println!("{json_text}");

        // Verify JSON is valid
        let parsed: serde_json::Value = serde_json::from_str(json_text).unwrap();
        assert!(parsed["choices"][0]["delta"]["tool_calls"].is_array());

        // Arguments should now be empty object instead of malformed JSON
        let tool_calls = &parsed["choices"][0]["delta"]["tool_calls"];
        let arguments = &tool_calls[0]["function"]["arguments"];

        assert!(arguments.is_string());
        assert_eq!(arguments.as_str().unwrap(), "{}");
    }

    /// Test malformed tool arguments with period instead of closing brace.
    /// Reference error: JSON Parse error: Expected '}
    /// Text: {"command":"ls -la | wc -l","description":"Count files in current directory".
    #[tokio::test]
    #[ignore]
    async fn test_malformed_tool_args_period_instead_of_brace() {
        let malformed_args =
            r#"{"command":"ls -la | wc -l","description":"Count files in current directory"."#;

        let completion_chunk = StreamChunk {
            id: "chatcmpl-period-brace".to_string(),
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
                        id: Some("call_789".to_string()),
                        call_type: Some("function".to_string()),
                        function: Some(DeltaFunctionCall {
                            name: Some("bash".to_string()),
                            arguments: Some(malformed_args.to_string()),
                        }),
                    }]),
                    reasoning: None,
                },
                finish_reason: Some("tool_calls".to_string()),
                logprobs: None,
            }],
            usage: None,
        };

        let mut converter = OpenaiConverter::default();
        let output = converter.process(&completion_chunk);

        // Should process the completion chunk with fixed arguments
        assert_eq!(output.len(), 1);

        let text = String::from_utf8_lossy(&output[0]);

        // Extract JSON from SSE format: "data: {json}\n\n"
        let json_start = text.find("data: ").unwrap() + 6;
        let json_text = text[json_start..].trim();

        // Verify JSON is now valid
        let parsed: serde_json::Value = serde_json::from_str(json_text).unwrap();
        assert!(parsed["choices"][0]["delta"]["tool_calls"].is_array());

        // Arguments should be the fixed complete object
        let tool_calls = &parsed["choices"][0]["delta"]["tool_calls"];
        let arguments = &tool_calls[0]["function"]["arguments"];

        assert!(arguments.is_string());
        let args_str = arguments.as_str().unwrap();

        // Should contain the command and description
        let parsed_args: serde_json::Value = serde_json::from_str(args_str).unwrap();
        assert_eq!(parsed_args["command"], "ls -la | wc -l");
        assert_eq!(
            parsed_args["description"],
            "Count files in current directory"
        );
    }
}
