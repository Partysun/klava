use crate::config::Config;
use crate::error::Result;
use serde_json::Value;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

/// Log JSON values to a JSONL file in the tests/fixtures directory
/// Format matches OpenAI SSE chunks as shown in test fixtures
pub fn log_upstream_stream_hook(data: Value, _config: &Config) -> Result<Value> {
    log_jsonl(data)
}

/// Log JSON values to a JSONL file in the system temp directory
/// This is the core logging function used by the hook
pub fn log_jsonl(data: Value) -> Result<Value> {
    let log_dir = std::env::temp_dir().join("klava_jsonl");

    if !log_dir.exists() {
        std::fs::create_dir_all(&log_dir).map_err(|e| {
            crate::error::Error::Internal(format!("Failed to create log directory: {}", e))
        })?;
    }

    let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");
    let filename = format!("upstream_stream_{}.jsonl", timestamp);
    let filepath = log_dir.join(filename);

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&filepath)
        .map_err(|e| {
            crate::error::Error::Internal(format!(
                "Failed to open log file {}: {}",
                filepath.display(),
                e
            ))
        })?;

    let json_line = data.to_string();
    writeln!(file, "{}", json_line).map_err(|e| {
        crate::error::Error::Internal(format!("Failed to write to log file: {}", e))
    })?;

    tracing::debug!("Logged upstream chunk to: {}", filepath.display());

    Ok(data)
}

/// Simple streaming logger that appends JSON lines to a file
#[derive(Clone)]
pub struct StreamLogger {
    filepath: PathBuf,
}

impl StreamLogger {
    pub fn new() -> std::io::Result<Self> {
        let log_dir = std::env::temp_dir().join("klava_jsonl");
        std::fs::create_dir_all(&log_dir)?;
        let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");
        let filename = format!("upstream_stream_{}.jsonl", timestamp);
        let filepath = log_dir.join(filename);
        Ok(Self { filepath })
    }

    pub fn log(&self, json_str: &str) -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.filepath)?;
        writeln!(file, "{}", json_str)
    }

    pub fn filepath(&self) -> &PathBuf {
        &self.filepath
    }

    pub fn log_sse_line(&self, line: &str) -> std::io::Result<()> {
        let trimmed = line.trim();

        if trimmed.is_empty() {
            return Ok(());
        }

        if let Some(json_part) = trimmed.strip_prefix("data: ") {
            let json_part = json_part.trim();

            if json_part == "[DONE]" {
                return Ok(());
            }

            if serde_json::from_str::<Value>(json_part).is_ok() {
                self.log(json_part)?;
                tracing::trace!("Logged SSE chunk to JSONL");
            } else {
                tracing::trace!("Skipping non-JSON SSE line: {}", json_part);
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_log_upstream_stream_hook() {
        let data = json!({
            "id":"chatcmpl-test",
            "object":"chat.completion.chunk",
            "created": 123,
            "choices":[{
                "index":0,
                "delta":{"content":"hello"},
                "logprobs":null,
                "finish_reason":null
            }]
        });

        let config = Config::default();
        let result = log_upstream_stream_hook(data, &config);

        assert!(result.is_ok());

        // Clean up the jsonl file created by the hook
        let log_dir = std::env::temp_dir().join("klava_jsonl");
        if let Ok(entries) = std::fs::read_dir(&log_dir) {
            for entry in entries.flatten() {
                if entry.path().extension().is_some_and(|ext| ext == "jsonl") {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }
}
