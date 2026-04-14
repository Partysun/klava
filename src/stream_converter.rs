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
                    yield Ok(Bytes::from(event));
                }
            }

            // Stream ended
            if !this.buffer.is_empty() {
                if let Some(event) = this.parse_partial() {
                    yield Ok(Bytes::from(event));
                } else {
                    yield Ok(Bytes::from(std::mem::take(&mut this.buffer)));
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

// ============================================================================
// Trait
// ============================================================================

pub trait ChatChunkConverter: Send + 'static {
    type OutputEvent;

    /// Process entire chunk (all choices)
    fn process(&mut self, chunk: &StreamChunk) -> Vec<Self::OutputEvent>;

    fn serialize(&self, event: &Self::OutputEvent) -> Vec<u8>;

    fn finalize(&mut self) -> Vec<Self::OutputEvent> {
        Vec::new()
    }
}

// ============================================================================
// Test Converter (Echo)
// ============================================================================

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

// ============================================================================
// Tests
// ============================================================================

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
}
