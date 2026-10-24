//! Diagnostic module for testing model connectivity
//!
//! Sends small, cheap test requests to every configured `[[models]]` entry
//! to verify that the connection settings and the API are working.

use crate::config::Config;
use crate::providers::Model;
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
    /// Model-entry name that was tested.
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

/// What the service manager says about the proxy on the configured port.
///
/// Kept separate from `service::InstanceState` so this module stays free of
/// systemd/launchd: the binary, which owns service management, fills it in.
/// That also lets the reporting logic be tested without a service manager.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceStatus {
    /// Unit or agent name, e.g. `klava-48017.service`.
    pub unit: String,
    /// Whether a unit/agent file exists.
    pub installed: bool,
    /// Whether the manager reports it running.
    pub active: bool,
    /// `active`, `inactive`, `failed`, `absent`, …
    pub state: String,
    /// Command that shows this instance's logs, e.g. `journalctl --user -u …`
    /// or `tail -f …`. The platform-specific form is decided by the binary.
    pub logs_command: String,
}

impl ServiceStatus {
    /// The state a machine with no service manager reports.
    pub fn absent(port: u16) -> Self {
        Self {
            unit: format!("klava-{port}"),
            installed: false,
            active: false,
            state: "absent".into(),
            logs_command: "klava logs".to_string(),
        }
    }

    /// A one-line description for the report.
    pub fn summary(&self) -> String {
        if !self.installed {
            return "no managed service installed".to_string();
        }
        format!("{} is {}", self.unit, self.state)
    }
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

/// Test one model entry by sending a minimal request
pub async fn test_provider(
    client: &Client,
    config: &Config,
    provider_config: &Model,
    model: Option<&str>,
) -> Result<TestResult, Error> {
    let start = std::time::Instant::now();
    let checks = Vec::new();
    let mut result = TestResult {
        success: false,
        provider: provider_config.name.clone(),
        model: provider_config.upstream_model(),
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
        .unwrap_or_else(|| provider_config.upstream_model());
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

/// Test every model entry, or only those named `model_name`.
pub async fn run_tests(
    config: &Config,
    model_name: Option<&str>,
    model: Option<&str>,
    service: ServiceStatus,
) {
    let client = Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("Failed to build HTTP client");

    let providers_to_test: Vec<_> = if let Some(name) = model_name {
        config.models.iter().filter(|p| p.name == name).collect()
    } else {
        config.models.iter().collect()
    };

    if providers_to_test.is_empty() {
        eprintln!("No models configured. Run 'klava providers add' to set up.");
        std::process::exit(1);
    }

    // The proxy first. Testing a provider directly answers "can we reach the
    // API"; it says nothing about the thing the agent actually talks to. A dead
    // proxy with a working API is the failure this check exists to catch, and
    // the old output called it healthy.
    let proxy = check_proxy(config, model, &service).await;
    proxy.print_summary();
    let mut all_passed = proxy.success;

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

/// Check the layer an agent actually talks to: the local proxy.
///
/// Three things can be independently wrong, and telling them apart is the whole
/// point of this check:
///
/// 1. the managed service is not running (`klava restart` fixes that);
/// 2. nothing is listening on the port (an ad-hoc `klava up`, a firewall, or a
///    different port in the agent's environment);
/// 3. the proxy answers but cannot complete a request — which is what a broken
///    routing table, a failing hook or an extension that fails open looks like.
///
/// Only the third needs a model call, so a dead proxy is reported in
/// milliseconds instead of after a 30-second provider timeout.
pub async fn check_proxy(
    config: &Config,
    model: Option<&str>,
    service: &ServiceStatus,
) -> TestResult {
    let port = config.port;
    let base = format!("http://127.0.0.1:{port}");

    let mut result = TestResult {
        success: false,
        provider: "proxy".to_string(),
        model: config
            .resolve_completion_model()
            .unwrap_or_else(|| "<none>".to_string()),
        url: base.clone(),
        latency_ms: 0,
        response_preview: String::new(),
        error: None,
        checks: Vec::new(),
    };

    // Does the managed service think it is up?
    if service.active {
        result.checks.push(CheckResult {
            name: "Service".to_string(),
            passed: true,
            message: format!("{} is running", service.unit),
        });
    } else {
        result.checks.push(CheckResult {
            name: "Service".to_string(),
            passed: false,
            message: format!(
                "{}; {}",
                service.summary(),
                if service.installed {
                    "start it with: klava restart"
                } else {
                    "create it with: klava launch"
                }
            ),
        });
    }

    // Is anything listening at all? `/health` is a constant "OK" served by the
    // proxy process, so a success here proves the process is alive and
    // reachable — not that it can route.
    let client = match Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(5))
        .build()
    {
        Ok(client) => client,
        Err(e) => {
            result.error = Some(format!("cannot build HTTP client: {e}"));
            return result;
        }
    };

    let started = std::time::Instant::now();
    let health = client.get(format!("{base}/health")).send().await;
    result.latency_ms = started.elapsed().as_millis() as u64;

    match health {
        Ok(resp) if resp.status().is_success() => {
            result.checks.push(CheckResult {
                name: "Port".to_string(),
                passed: true,
                message: format!("something answers on {base}"),
            });
        }
        Ok(resp) => {
            result.checks.push(CheckResult {
                name: "Port".to_string(),
                passed: false,
                message: format!("{base}/health answered HTTP {}", resp.status()),
            });
            result.error = Some(if service.active {
                "the service is running but /health does not answer with 2xx".to_string()
            } else {
                "nothing healthy is listening on the configured port".to_string()
            });
            return result;
        }
        Err(e) => {
            result.checks.push(CheckResult {
                name: "Port".to_string(),
                passed: false,
                message: format!("nothing answers on {base} ({})", short_error(&e)),
            });
            result.error = Some(alignment_hint(service, port));
            return result;
        }
    }

    // It is listening. Does it actually route? Only this step proves the agent
    // will work, so it is the one that decides `success`.
    let probe_model = model
        .map(str::to_string)
        .or_else(|| config.resolve_completion_model())
        .unwrap_or_default();

    if probe_model.is_empty() {
        result.checks.push(CheckResult {
            name: "Routing".to_string(),
            passed: false,
            message: "no model configured to route a probe through".to_string(),
        });
        result.error = Some("no models configured - run 'klava providers add'".to_string());
        return result;
    }

    let body = json!({
        "model": probe_model,
        "messages": [
            {"role": "system", "content": "Reply with exactly one word, nothing else."},
            {"role": "user", "content": "verification"},
        ],
        "max_tokens": 256,
        "temperature": 0.0,
        "stream": false,
    });

    let started = std::time::Instant::now();
    let response = client
        .post(format!("{base}/v1/chat/completions"))
        .json(&body)
        .send()
        .await;
    result.latency_ms = started.elapsed().as_millis() as u64;

    match response {
        Ok(resp) if resp.status().is_success() => {
            let body: Value = match resp.json().await {
                Ok(body) => body,
                Err(e) => {
                    result.checks.push(CheckResult {
                        name: "Routing".to_string(),
                        passed: false,
                        message: format!("proxy answered with unparseable JSON: {e}"),
                    });
                    result.error = Some("the proxy replied, but not with a chat completion".into());
                    return result;
                }
            };
            match validate_api_response(&body) {
                Ok(preview) => {
                    result.response_preview = preview.clone();
                    result.checks.push(CheckResult {
                        name: "Routing".to_string(),
                        passed: true,
                        message: format!(
                            "proxy routed a completion: \"{}\"",
                            preview.chars().take(50).collect::<String>()
                        ),
                    });
                    result.success = true;
                }
                Err(reason) => {
                    result.checks.push(CheckResult {
                        name: "Routing".to_string(),
                        passed: false,
                        message: reason.clone(),
                    });
                    result.error = Some(format!(
                        "the proxy is reachable but returned no usable answer: {reason}"
                    ));
                }
            }
        }
        Ok(resp) => {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            result.checks.push(CheckResult {
                name: "Routing".to_string(),
                passed: false,
                message: format!("proxy returned HTTP {status}"),
            });
            result.error = Some(format!(
                "{}\n{}",
                explain_proxy_status(status, service),
                first_line(&text)
            ));
        }
        Err(e) => {
            result.checks.push(CheckResult {
                name: "Routing".to_string(),
                passed: false,
                message: format!("request to the proxy failed ({})", short_error(&e)),
            });
            result.error = Some(format!(
                "the port answers /health but the completion request did not complete; \
                 the proxy may have died mid-request - see `{}`",
                service.logs_command
            ));
        }
    }

    result
}

/// Why a proxy listen failure might not mean "the proxy is down".
fn alignment_hint(service: &ServiceStatus, port: u16) -> String {
    if service.active {
        // The manager says the unit is up but the port refuses connections,
        // which almost always means the unit and the config disagree.
        format!(
            "{} is active but nothing listens on port {port}. \
             The unit may be starting, or the agent points at another port; \
             check `klava status` and `{}`",
            service.unit, service.logs_command
        )
    } else if service.installed {
        format!(
            "{} exists but is not running, and nothing listens on port {port}. \
             Start it with: klava restart",
            service.unit
        )
    } else {
        format!(
            "no proxy on port {port} and no managed service installed. \
             Start one with: klava launch, or klava up for a foreground proxy"
        )
    }
}

/// Turn a proxy HTTP error into the command that usually fixes it.
fn explain_proxy_status(status: reqwest::StatusCode, service: &ServiceStatus) -> String {
    match status {
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN => {
            "the proxy rejected the request (auth); run `klava login`".to_string()
        }
        reqwest::StatusCode::TOO_MANY_REQUESTS => {
            "the provider rate-limited the probe; retry later".to_string()
        }
        s if s.is_server_error() => {
            format!(
                "the proxy failed upstream or internally (HTTP {s}); see `{}`",
                service.logs_command
            )
        }
        s => format!("the proxy returned HTTP {s}"),
    }
}

fn first_line(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    trimmed
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// reqwest errors chain into a paragraph; the outermost line is the useful one.
fn short_error(error: &reqwest::Error) -> String {
    if error.is_connect() {
        return "connection refused".to_string();
    }
    if error.is_timeout() {
        return "timed out".to_string();
    }
    first_line(&error.to_string())
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
            validate_api_response(&response_with_content(Some("Sure, here is a summary.")))
                .unwrap(),
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

    // ---- service/routing report helpers -----------------------------------

    fn installed(state: &str, active: bool) -> ServiceStatus {
        ServiceStatus {
            unit: "klava-48017.service".to_string(),
            installed: true,
            active,
            state: state.to_string(),
            logs_command: "journalctl --user -u klava-48017.service -f".to_string(),
        }
    }

    #[test]
    fn absent_service_says_so() {
        let status = ServiceStatus::absent(48017);
        assert_eq!(status.summary(), "no managed service installed");
        assert_eq!(status.unit, "klava-48017");
        assert!(!status.active);
    }

    #[test]
    fn installed_service_summary_names_unit_and_state() {
        assert_eq!(
            installed("inactive", false).summary(),
            "klava-48017.service is inactive"
        );
        assert_eq!(
            installed("active", true).summary(),
            "klava-48017.service is active"
        );
    }

    #[test]
    fn listen_failure_distinguishes_service_layers() {
        // Manager says active but the port is dead: unit/config mismatch.
        let hint = alignment_hint(&installed("active", true), 48017);
        assert!(
            hint.contains("is active but nothing listens on port 48017"),
            "{hint}"
        );
        assert!(hint.contains("journalctl"), "{hint}");

        // Installed but stopped: the fix is `klava restart`.
        let hint = alignment_hint(&installed("inactive", false), 48017);
        assert!(hint.contains("is not running"), "{hint}");
        assert!(hint.contains("klava restart"), "{hint}");

        // Nothing installed at all: neither command is `restart`.
        let hint = alignment_hint(&ServiceStatus::absent(48017), 48017);
        assert!(hint.contains("no managed service installed"), "{hint}");
        assert!(hint.contains("klava launch"), "{hint}");
    }

    #[test]
    fn proxy_http_errors_name_the_next_command() {
        let service = installed("active", true);
        assert!(
            explain_proxy_status(reqwest::StatusCode::UNAUTHORIZED, &service)
                .contains("klava login")
        );
        assert!(
            explain_proxy_status(reqwest::StatusCode::TOO_MANY_REQUESTS, &service)
                .contains("retry later")
        );
        let server = explain_proxy_status(reqwest::StatusCode::INTERNAL_SERVER_ERROR, &service);
        assert!(server.contains("journalctl"), "{server}");
        assert!(explain_proxy_status(reqwest::StatusCode::BAD_REQUEST, &service).contains("400"));
    }

    #[test]
    fn first_line_keeps_only_the_useful_line() {
        assert_eq!(first_line("  error: boom  \nsecond"), "error: boom");
        assert_eq!(first_line("   "), "");
        assert_eq!(first_line("single"), "single");
    }
}
