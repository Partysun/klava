//! Diagnostic module for testing provider connectivity
//!
//! Sends small, cheap test requests to verify that configurations
//! and LLM APIs are working correctly.

use crate::config::Config;
use crate::providers::Config as ProviderConfig;
use crate::{error::Error, models::openai};
use colored::*;
use reqwest::Client;
use serde_json::Value;
use serde_json::json;
use std::time::Duration;

/// Result of a diagnostic test
#[derive(Debug, Clone)]
pub struct TestResult {
    pub success: bool,
    pub provider: String,
    pub model: String,
    pub url: String,
    pub latency_ms: u64,
    pub response_preview: String,
    pub error: Option<String>,
    pub checks: Vec<CheckResult>,
}

/// Individual check result
#[derive(Debug, Clone)]
pub struct CheckResult {
    pub name: String,
    pub passed: bool,
    pub message: String,
}

impl TestResult {
    pub fn print_summary(&self) {
        // Compact header
        let status = if self.success {
            "✓".green()
        } else {
            "✗".red()
        };
        println!(
            "{} {} ({}) → {} → {}ms",
            status, self.provider, self.model, self.url, self.latency_ms
        );

        // Checks inline
        for check in &self.checks {
            let symbol = if check.passed {
                "✓".green()
            } else {
                "✗".red()
            };
            println!("  {} {}", symbol, check.message);
        }

        // Error (compact)
        if let Some(error) = &self.error {
            // Show first line of error only
            let first_line = error.lines().next().unwrap_or(error.as_str());
            println!("  {} {}", "✗".red(), first_line.red());
        }
    }
}

/// Test a provider configuration by sending a minimal request
pub async fn test_provider(
    client: &Client,
    config: &Config,
    provider_config: &ProviderConfig,
    model: Option<&str>,
) -> Result<TestResult, Error> {
    let start = std::time::Instant::now();
    let checks = Vec::new();
    let mut result = TestResult {
        success: false,
        provider: provider_config.name.clone(),
        model: model.unwrap_or("unknown").to_string(),
        url: "N/A".to_string(),
        latency_ms: 0,
        response_preview: String::new(),
        error: None,
        checks,
    };

    // Check 1: Config validation
    if let Err(e) = provider_config.validate_config(config) {
        result.checks.push(CheckResult {
            name: "Configuration".to_string(),
            passed: false,
            message: format!("Invalid: {}", e),
        });
        result.error = Some(format!("Configuration error: {}", e));
        return Ok(result);
    }

    result.checks.push(CheckResult {
        name: "Configuration".to_string(),
        passed: true,
        message: "Valid".to_string(),
    });

    // Check 2: Auth headers
    let auth_headers = provider_config
        .get_auth_headers(config)
        .await
        .map_err(|e| Error::Provider(format!("Auth error: {}", e)))?;

    if auth_headers.is_some() {
        result.checks.push(CheckResult {
            name: "Authentication".to_string(),
            passed: true,
            message: "Auth headers ready".to_string(),
        });
    } else {
        result.checks.push(CheckResult {
            name: "Authentication".to_string(),
            passed: true,
            message: "No auth required".to_string(),
        });
    }

    // Determine URL and model
    let url = provider_config
        .chat_completions_url()
        .ok_or(Error::MissingBaseUrl)?;
    result.url = url.clone();

    let test_model = model
        .map(|m| m.to_string())
        .or(provider_config.completion_model())
        .or(provider_config.reasoning_model())
        .unwrap_or_else(|| "gpt-3.5-turbo".to_string());
    result.model = test_model.clone();

    let test_req = openai::OpenAIRequest {
        model: test_model,
        messages: vec![
            openai::Message {
                role: "system".to_string(),
                content: Some(openai::MessageContent::Text(
                    "Reply with exactly one word, nothing else.".to_string(),
                )),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            },
            openai::Message {
                role: "user".to_string(),
                content: Some(openai::MessageContent::Text("verification".to_string())),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            },
        ],
        // Keep a headroom budget: thinking models (OpenRouter/DeepSeek/Qwen)
        // burn tokens on chain-of-thought before producing visible content,
        // and an exhausted budget returns content: null.
        max_tokens: Some(256),
        temperature: Some(0.0),
        top_p: None,
        stop: None,
        stream: Some(false),
        tools: None,
        tool_choice: None,
        reasoning_effort: None,
        extra: json!({}),
    };

    // Build request payload (same transformations as proxy.rs)
    let mut request_payload = serde_json::to_value(&test_req)?;

    // Qwen-specific payload modifications (mirror proxy.rs logic)
    if url.contains("dashscope")
        || url.contains("qwen")
        || result.model.to_lowercase().contains("qwen")
    {
        if test_req.stream.unwrap_or(false) {
            request_payload["incremental_output"] = serde_json::Value::Bool(true);
        }
        if let Some(messages) = request_payload["messages"].as_array_mut() {
            let has_system = messages
                .iter()
                .any(|msg| msg.get("role").and_then(|r| r.as_str()) == Some("system"));
            if !has_system {
                messages.insert(
                    0,
                    json!({"role": "system", "content": "You are a helpful assistant."}),
                );
            }
        }
    }

    // Send request (mirroring proxy.rs logic)
    let mut req_builder = client
        .post(&url)
        .json(&request_payload)
        .timeout(Duration::from_secs(30));

    if let Some(headers) = auth_headers {
        req_builder = req_builder.headers(headers);
    }

    // OpenRouter-specific headers (mirror proxy.rs)
    if url.contains("openrouter") {
        req_builder = req_builder
            .header("X-OpenRouter-Title", "Klava")
            .header("X-OpenRouter-Categories", "cli-agent-proxy");
    }

    let response = req_builder
        .send()
        .await
        .map_err(|e| Error::Upstream(format!("Request failed: {}", e)))?;

    let status = response.status();
    let body: Value = response
        .json()
        .await
        .map_err(|e| Error::Upstream(format!("Failed to parse response: {}", e)))?;

    result.latency_ms = start.elapsed().as_millis() as u64;

    // Check if response contains an error field even with 200 status
    if let Some(error) = body.get("error") {
        let error_msg = error
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("Unknown API error");
        let error_type = error
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("unknown_error");

        result.checks.push(CheckResult {
            name: "API Response".to_string(),
            passed: false,
            message: format!(
                "{{\"error\": \"{}\", \"type\": \"{}\"}}",
                error_msg, error_type
            ),
        });
        result.error = Some(format!(
            "API returned error: {} (type: {})",
            error_msg, error_type
        ));
        return Ok(result);
    }

    if !status.is_success() {
        result.checks.push(CheckResult {
            name: "API Response".to_string(),
            passed: false,
            message: format!("HTTP {}", status),
        });
        result.error = Some(format!(
            "HTTP {}: {}",
            status,
            serde_json::to_string_pretty(&body).unwrap_or_default()
        ));
        return Ok(result);
    }

    // Validate response structure
    match validate_api_response(&body) {
        Ok(preview) => {
            result.response_preview = preview.clone();
            result.checks.push(CheckResult {
                name: "API Response".to_string(),
                passed: true,
                message: format!(
                    "Got response: \"{}\"",
                    preview.chars().take(50).collect::<String>()
                ),
            });
            result.success = true;
        }
        Err(reason) => {
            result.checks.push(CheckResult {
                name: "API Response".to_string(),
                passed: false,
                message: reason,
            });
            result.error = Some(format!(
                "Response: {}",
                serde_json::to_string_pretty(&body).unwrap_or_default()
            ));
        }
    }

    Ok(result)
}

/// Validate a chat completions response body.
///
/// A response is valid when the model produced *any* non-empty content —
/// either visible text (`content`) or reasoning output (`reasoning` /
/// `reasoning_content`, used by thinking models on OpenRouter/DeepSeek/
/// Qwen when the output budget is consumed by chain-of-thought).
/// The exact answer is not checked — the model answers freely, and failures
/// surface as HTTP errors, `error` payloads, or empty/missing content.
fn validate_api_response(body: &Value) -> Result<String, String> {
    let choices = body.get("choices").and_then(|c| c.as_array());
    if choices.is_none() || choices.unwrap().is_empty() {
        return Err("No choices in response".to_string());
    }

    let message = choices.unwrap()[0].get("message");
    if message.is_none() || !message.unwrap().is_object() {
        return Err("No message in response".to_string());
    }
    let message = message.unwrap();

    let candidates = [
        message.get("content").and_then(Value::as_str),
        message.get("reasoning").and_then(Value::as_str),
        message.get("reasoning_content").and_then(Value::as_str),
    ];

    for text in candidates.into_iter().flatten() {
        let cleaned = text.trim().replace('\n', " ").replace('\r', "");
        let preview = cleaned.trim();
        if !preview.is_empty() {
            return Ok(preview.to_string());
        }
    }

    Err("No message content in response".to_string())
}

/// Test all providers or a specific one
pub async fn run_tests(config: &Config, provider_name: Option<&str>, model: Option<&str>) {
    let client = Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("Failed to build HTTP client");

    let providers_to_test: Vec<_> = if let Some(name) = provider_name {
        config.providers.iter().filter(|p| p.name == name).collect()
    } else {
        config.providers.iter().collect()
    };

    if providers_to_test.is_empty() {
        eprintln!("No providers configured. Run 'klava providers' to set up.");
        std::process::exit(1);
    }

    let mut all_passed = true;
    for provider in providers_to_test {
        match test_provider(&client, config, provider, model).await {
            Ok(result) => {
                result.print_summary();
                if !result.success {
                    all_passed = false;
                }
            }
            Err(e) => {
                eprintln!("  ✗ {}: {}", provider.name, e);
                all_passed = false;
            }
        }
    }

    if !all_passed {
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response_with_content(content: Option<&str>) -> Value {
        let mut body = json!({
            "id": "chatcmpl-1",
            "object": "chat.completion",
            "choices": []
        });
        if let Some(text) = content {
            body["choices"] = json!([{
                "index": 0,
                "message": {"role": "assistant", "content": text}
            }]);
        }
        body
    }

    fn response_with_reasoning_only(reasoning: &str) -> Value {
        json!({
            "id": "gen-1",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "finish_reason": "length",
                "message": {
                    "role": "assistant",
                    "content": null,
                    "reasoning": reasoning,
                    "reasoning_details": [{"type": "reasoning.text", "text": reasoning}]
                }
            }]
        })
    }

    #[test]
    fn any_non_empty_answer_is_valid() {
        assert_eq!(
            validate_api_response(&response_with_content(Some("This is a test"))).unwrap(),
            "This is a test"
        );
    }

    #[test]
    fn answer_not_matching_prompt_is_still_valid() {
        // Regression: previously the answer had to contain the prompt echo
        // ("verification"), which rejected legitimate model responses.
        assert_eq!(
            validate_api_response(&response_with_content(Some("42"))).unwrap(),
            "42"
        );
        assert_eq!(
            validate_api_response(&response_with_content(Some("Sure, here is a summary."))).unwrap(),
            "Sure, here is a summary."
        );
    }

    #[test]
    fn multiline_answer_is_normalized_for_preview() {
        assert_eq!(
            validate_api_response(&response_with_content(Some("line1\nline2"))).unwrap(),
            "line1 line2"
        );
    }

    #[test]
    fn reasoning_only_response_is_valid() {
        // OpenRouter reasoning models return content: null when the output
        // budget is consumed by chain-of-thought (e.g. max_tokens too low).
        assert_eq!(
            validate_api_response(&response_with_reasoning_only(
                "Okay, the user just said \"Hi.\""
            ))
            .unwrap(),
            "Okay, the user just said \"Hi.\""
        );
    }

    #[test]
    fn deepseek_style_reasoning_content_is_valid() {
        let body = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "reasoning_content": "Let me think about this..."
                }
            }]
        });
        assert_eq!(
            validate_api_response(&body).unwrap(),
            "Let me think about this..."
        );
    }

    #[test]
    fn visible_content_preferred_over_reasoning() {
        let body = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "Hello!",
                    "reasoning": "Some chain of thought"
                }
            }]
        });
        assert_eq!(validate_api_response(&body).unwrap(), "Hello!");
    }

    #[test]
    fn whitespace_only_content_is_rejected() {
        assert!(validate_api_response(&response_with_content(Some(" \n "))).is_err());
    }

    #[test]
    fn missing_content_is_rejected() {
        assert!(validate_api_response(&response_with_content(None)).is_err());
    }

    #[test]
    fn empty_choices_is_rejected() {
        assert!(validate_api_response(&json!({"choices": []})).is_err());
    }

    #[test]
    fn error_payload_is_rejected_by_caller() {
        // `validate_api_response` only sees successful-response bodies; the
        // `error` field is guarded earlier in `test_provider`.
        let body = json!({"error": {"message": "bad key", "type": "authentication_error"}});
        assert!(validate_api_response(&body).is_err());
    }
}
