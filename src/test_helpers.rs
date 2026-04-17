#[cfg(test)]
use crate::error::Error;
#[cfg(test)]
use crate::models::openai::*;
#[cfg(test)]
use bytes::Bytes;

#[cfg(test)]
/// Helper: Load SSE events from JSONL fixture file
pub fn load_fixture_sse_events(fixture_path: &str) -> Vec<Result<Bytes, Error>> {
    let file_content = std::fs::read_to_string(fixture_path)
        .unwrap_or_else(|_| panic!("Failed to read fixture: {}", fixture_path));

    file_content
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return None;
            }

            // Handle tab-separated format: "1\t{...}" -> "{...}"
            let json_str = if let Some(tab_pos) = trimmed.find('\t') {
                let after_tab = tab_pos + 1; // Skip the tab character itself
                trimmed.get(after_tab..).unwrap_or(trimmed)
            } else {
                trimmed
            };

            if json_str == "[DONE]" {
                return Some(Ok(Bytes::from("data: [DONE]\n\n")));
            }
            if let Ok(chunk) = serde_json::from_str::<StreamChunk>(json_str) {
                let json_str = serde_json::to_string(&chunk).unwrap();
                Some(Ok(Bytes::from(format!("data: {}\n\n", json_str))))
            } else {
                eprintln!("Failed to parse line as StreamChunk: {}", json_str);
                Some(Err(Error::Upstream(format!(
                    "Invalid JSON line: {}",
                    json_str
                ))))
            }
        })
        .collect()
}

#[cfg(test)]
pub fn make_sse_event(data: &str) -> Bytes {
    Bytes::from(format!("data: {}\n\n", data))
}

#[cfg(test)]
pub fn make_chunk(id: &str, content: &str) -> String {
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

#[cfg(test)]
pub fn make_done_chunk(id: &str) -> String {
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
