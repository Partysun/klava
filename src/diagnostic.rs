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
    let base_url = provider_config
        .resolve_base_url()
        .ok_or_else(|| Error::MissingBaseUrl)?;
    let url = format!("{}/v1/chat/completions", base_url.trim_end_matches('/'));
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
        max_tokens: Some(20),
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

    result.latency_ms = start.elapsed().as_millis() as u64;

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
    let choices = body.get("choices").and_then(|c| c.as_array());
    if choices.is_none() || choices.unwrap().is_empty() {
        result.checks.push(CheckResult {
            name: "API Response".to_string(),
            passed: false,
            message: "No choices in response".to_string(),
        });
        result.error = Some(format!(
            "Response: {}",
            serde_json::to_string_pretty(&body).unwrap_or_default()
        ));
        return Ok(result);
    }

    let content = choices.unwrap()[0]
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str());

    match content {
        Some(text) => {
            let cleaned = text
                .trim()
                .replace("\n", " ")
                .replace("\r", "")
                .to_lowercase();
            let trimmed = cleaned.trim();
            if trimmed.is_empty() {
                // Content is whitespace only
                result.checks.push(CheckResult {
                    name: "API Response".to_string(),
                    passed: false,
                    message: "Empty content (whitespace only)".to_string(),
                });
                result.error = Some(format!(
                    "Response: {}",
                    serde_json::to_string_pretty(&body).unwrap_or_default()
                ));
            } else if !trimmed.contains("verification") && !trimmed.contains("verif") {
                // Response doesn't contain "ok" which we asked for
                result.checks.push(CheckResult {
                    name: "API Response".to_string(),
                    passed: false,
                    message: "Response doesn't match expected output".to_string(),
                });
                result.error = Some(format!(
                    "Expected response to contain 'verification', but got: \"{}\"\n\nThis usually means:\n  - The model doesn't exist\n  - The model is misconfigured\n  - The API is mocking responses\n\nFull response: {}",
                    trimmed,
                    serde_json::to_string_pretty(&body).unwrap_or_default()
                ));
            } else {
                // Valid response
                result.response_preview = trimmed.to_string();
                result.checks.push(CheckResult {
                    name: "API Response".to_string(),
                    passed: true,
                    message: format!(
                        "Got response: \"{}\"",
                        result.response_preview.chars().take(50).collect::<String>()
                    ),
                });
                result.success = true;
            }
        }
        None => {
            result.checks.push(CheckResult {
                name: "API Response".to_string(),
                passed: false,
                message: "No message content in response".to_string(),
            });
            result.error = Some(format!(
                "Response: {}",
                serde_json::to_string_pretty(&body).unwrap_or_default()
            ));
        }
    }

    Ok(result)
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
