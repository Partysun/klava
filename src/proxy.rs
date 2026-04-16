use crate::anthropic::AnthropicStreamConverter;
use crate::anthropic::transform::{anthropic_to_openai, openai_to_anthropic};
use crate::config::Config;
use crate::error::{Error, Result};
use crate::hooks::{HookChain, HookStage};
use crate::models::{anthropic, openai};
use crate::responses::ResponsesStreamConverter;
use crate::responses::responses_to_openai;
use crate::stream_converter::{PassthroughConverter, UniversalConverter};
use axum::{
    Extension, Json,
    body::Body,
    extract::Request,
    http::{HeaderMap, HeaderValue},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use futures::stream::{Stream, StreamExt};
use reqwest::Client;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

/// Apply model override based on config for OpenAI requests
/// Detects reasoning requests by checking reasoning_effort parameter
fn apply_openai_model_override(
    mut req: openai::OpenAIRequest,
    reasoning_model: Option<String>,
    completion_model: Option<String>,
) -> openai::OpenAIRequest {
    // Check if this is a reasoning request based on reasoning_effort
    let is_reasoning = req.reasoning_effort.as_ref().map_or(false, |effort| {
        !matches!(effort, openai::ReasoningEffort::None)
    });

    // Override model if provided
    let model = if is_reasoning {
        reasoning_model.clone().unwrap_or_else(|| req.model.clone())
    } else {
        completion_model
            .clone()
            .unwrap_or_else(|| req.model.clone())
    };

    req.model = model;
    req
}

/// Handler for Anthropic-compatible requests (/v1/messages)
/// - Parses Anthropic request format
/// - Transforms to OpenAI for upstream
/// - Transforms response back to Anthropic
pub async fn proxy_anthropic(
    Extension(config): Extension<Arc<Config>>,
    Extension(client): Extension<Client>,
    Extension(hook_chain): Extension<Arc<HookChain>>,
    request: Request,
) -> Result<Response> {
    let body_bytes = read_body_bytes(request).await?;
    let anthropic_req: anthropic::AnthropicRequest = serde_json::from_slice(&body_bytes)?;

    tracing::debug!(
        "Detected Anthropic format request for model: {}",
        anthropic_req.model
    );
    let is_streaming = anthropic_req.stream.unwrap_or(false);

    // Run hooks on the original Anthropic request
    let data: Value = serde_json::to_value(&anthropic_req)?;
    let data = hook_chain.execute(HookStage::RequestReceived, data, &config)?;
    let anthropic_req: anthropic::AnthropicRequest = serde_json::from_value(data)?;

    if config.verbose {
        tracing::trace!(
            "Request payload: {}",
            serde_json::to_string_pretty(&anthropic_req).unwrap_or_default()
        );
    }

    let openai_req = anthropic_to_openai(
        anthropic_req,
        config.resolve_reasoning_model(),
        config.resolve_completion_model(),
    )?;
    tracing::debug!(
        "Transformed to OpenAI request for model: {}",
        openai_req.model
    );

    if config.verbose {
        tracing::trace!(
            "Transformed OpenAI request: {}",
            serde_json::to_string_pretty(&openai_req).unwrap_or_default()
        );
    }

    let response = send_request(&client, &config, &openai_req, &hook_chain).await?;

    if config.verbose {
        tracing::trace!(
            "Upstream request sent, received response status: {}",
            response.status()
        );
    }

    if !response.status().is_success() {
        return Err(handle_upstream_error(response).await);
    }

    let response = if is_streaming {
        handle_streaming_anthropic(response, &config).await?
    } else {
        handle_non_streaming_anthropic(response, &hook_chain, &config).await?
    };

    Ok(response)
}

/// Handler for OpenAI-compatible requests (/v1/chat/completions)
/// - Parses OpenAI request format
/// - Sends as-is to upstream
/// - Returns OpenAI response as-is (passthrough)
pub async fn proxy_openai(
    Extension(config): Extension<Arc<Config>>,
    Extension(client): Extension<Client>,
    Extension(hook_chain): Extension<Arc<HookChain>>,
    request: Request,
) -> Result<Response> {
    let body_bytes = read_body_bytes(request).await?;
    let openai_req: openai::OpenAIRequest = serde_json::from_slice(&body_bytes)?;

    tracing::debug!(
        "Detected OpenAI format request for model: {}",
        openai_req.model
    );
    let is_streaming = openai_req.stream.unwrap_or(false);

    // Run hooks on the request
    let data: Value = serde_json::to_value(&openai_req)?;
    let data = hook_chain.execute(HookStage::RequestReceived, data, &config)?;
    let openai_req: openai::OpenAIRequest = serde_json::from_value(data)?;

    // Override model based on config (reasoning vs completion model)
    let openai_req = apply_openai_model_override(
        openai_req,
        config.resolve_reasoning_model(),
        config.resolve_completion_model(),
    );

    if config.verbose {
        tracing::trace!(
            "Request payload: {}",
            serde_json::to_string_pretty(&openai_req).unwrap_or_default()
        );
    }

    let response = send_request(&client, &config, &openai_req, &hook_chain).await?;

    if !response.status().is_success() {
        return Err(handle_upstream_error(response).await);
    }

    let response = if is_streaming {
        handle_streaming_openai(response, &config).await?
    } else {
        handle_non_streaming_openai(response, &hook_chain, &config).await?
    };

    Ok(response)
}

async fn read_body_bytes(request: Request) -> Result<bytes::Bytes> {
    axum::body::to_bytes(request.into_body(), 1024 * 1024)
        .await
        .map_err(|e| Error::Upstream(format!("Failed to read request body: {}", e)))
}

/// Send request to upstream API
async fn send_request(
    client: &Client,
    config: &Config,
    openai_req: &openai::OpenAIRequest,
    hook_chain: &HookChain,
) -> Result<reqwest::Response> {
    let url = config.chat_completions_url();
    tracing::debug!("Sending request to {} for model: {}", url, openai_req.model);

    // Create the request body, potentially modifying it based on the provider
    let mut request_payload = serde_json::to_value(openai_req)?;

    // Special handling for Qwen models that require specific parameters
    if url.contains("dashscope") || url.contains("qwen") || openai_req.model.contains("qwen") {
        // For Qwen models, add enable_thinking: false for non-streaming requests
        if openai_req.stream.unwrap_or(false) {
            request_payload["enable_thinking"] = serde_json::Value::Bool(false);
        } else {
            // For streaming requests with Qwen, ensure incremental_output is true if thinking is enabled
            request_payload["incremental_output"] = serde_json::Value::Bool(true);
        }

        // Check if we need to add a system message for Qwen models
        if let Some(messages_value) = request_payload["messages"].as_array_mut() {
            // Check if there's already a system message
            let has_system_message = messages_value
                .iter()
                .any(|msg| msg.get("role").and_then(|r| r.as_str()) == Some("system"));

            if !has_system_message {
                // Add a default system message
                let system_message = json!({
                    "role": "system",
                    "content": "You are a helpful assistant."
                });

                messages_value.insert(0, system_message);
            }
        }
    }

    let mut req_builder = client
        .post(&url)
        .json(&request_payload)
        .timeout(Duration::from_secs(300));

    // Conditionally add OpenRouter headers if base URL contains "openrouter"
    if url.contains("openrouter") {
        req_builder = req_builder
            .header("X-OpenRouter-Title", "Klava")
            .header("X-OpenRouter-Categories", "cli-agent-proxy");
    }

    let provider = config.get_active_provider_config().ok_or_else(|| {
        Error::Internal(format!(
            "Active provider '{}' not found in configuration",
            config.active_provider
        ))
    })?;

    if let Some(headers) = provider.get_auth_headers(config).await? {
        req_builder = req_builder.headers(headers);
    }

    let data: Value = request_payload;
    hook_chain.execute(HookStage::BeforeUpstream, data, config)?;
    req_builder
        .send()
        .await
        .map_err(|e| Error::Upstream(format!("Failed to send request to upstream: {}", e)))
}

/// Handle upstream error response
async fn handle_upstream_error(response: reqwest::Response) -> Error {
    let status = response.status();
    let error_text = response
        .text()
        .await
        .unwrap_or_else(|_| "Unknown error".to_string());
    tracing::error!("Upstream error ({}): {}", status, error_text);
    Error::Upstream(format!("Upstream returned {}: {}", status, error_text))
}

/// Build SSE response with proper headers
fn build_sse_response(stream: impl Stream<Item = Result<Bytes>> + Send + 'static) -> Response {
    let stream = stream.inspect(|chunk| {
        if let Ok(bytes) = chunk {
            tracing::trace!("[DOWNSTREAM] {}", String::from_utf8_lossy(bytes).trim());
        }
    });

    let mut headers = HeaderMap::new();
    headers.insert(
        "Content-Type",
        HeaderValue::from_static("text/event-stream"),
    );
    headers.insert("Cache-Control", HeaderValue::from_static("no-cache"));
    headers.insert("Connection", HeaderValue::from_static("keep-alive"));

    let body = Body::from_stream(stream);
    (headers, body).into_response()
}

// ===== Anthropic-specific handlers =====

async fn handle_non_streaming_anthropic(
    response: reqwest::Response,
    hook_chain: &HookChain,
    config: &Config,
) -> Result<Response> {
    let openai_resp: openai::OpenAIResponse = response.json().await?;
    let anthropic_resp = openai_to_anthropic(openai_resp)?;

    let data: Value = serde_json::to_value(anthropic_resp)?;
    let resp = hook_chain.execute(HookStage::BeforeResponse, data, config)?;
    Ok(Json(resp).into_response())
}

async fn handle_streaming_anthropic(
    response: reqwest::Response,
    _config: &Config,
) -> Result<Response> {
    let stream = response.bytes_stream().inspect(|chunk| {
        if let Ok(bytes) = chunk {
            tracing::trace!("[UPSTREAM] {}", String::from_utf8_lossy(bytes).trim());
        }
    });

    let upstream = stream.map(|result| result.map_err(Error::Http));
    let converter = AnthropicStreamConverter::new();
    let universal = UniversalConverter::new(converter);

    Ok(build_sse_response(universal.convert(upstream)))
}

// ===== OpenAI-specific handlers =====

async fn handle_non_streaming_openai(
    response: reqwest::Response,
    hook_chain: &HookChain,
    config: &Config,
) -> Result<Response> {
    let openai_resp: openai::OpenAIResponse = response.json().await?;

    let data: Value = serde_json::to_value(openai_resp)?;
    let resp = hook_chain.execute(HookStage::BeforeResponse, data, config)?;
    Ok(Json(resp).into_response())
}

async fn handle_streaming_openai(
    response: reqwest::Response,
    _config: &Config,
) -> Result<Response> {
    let stream = response.bytes_stream().inspect(|chunk| {
        if let Ok(bytes) = chunk {
            tracing::trace!("[UPSTREAM] {}", String::from_utf8_lossy(bytes).trim());
        }
    });

    let upstream = stream.map(|result| result.map_err(Error::Http));
    let converter = PassthroughConverter::default();
    let universal = UniversalConverter::new(converter);

    Ok(build_sse_response(universal.convert(upstream)))
}

// ===== Responses API handler =====

/// Handler for Responses API requests (/v1/responses)
/// - Parses Responses API request format
/// - Transforms to OpenAI chat completions format
/// - Sends to upstream as OpenAI
/// - For streaming: converts OpenAI SSE chunks to Responses API SSE events
///   using ResponsesStreamConverter via UniversalConverter
pub async fn proxy_responses(
    Extension(config): Extension<Arc<Config>>,
    Extension(client): Extension<Client>,
    Extension(hook_chain): Extension<Arc<HookChain>>,
    request: Request,
) -> Result<Response> {
    let body_bytes = read_body_bytes(request).await?;
    let responses_req: crate::models::responses::ResponsesRequest =
        serde_json::from_slice(&body_bytes)?;

    tracing::debug!(
        "Detected Responses format request for model: {}",
        responses_req.model
    );
    let is_streaming = responses_req.stream.unwrap_or(false);

    // Run hooks on the original Responses request
    let data: Value = serde_json::to_value(&responses_req)?;
    let data = hook_chain.execute(HookStage::RequestReceived, data, &config)?;
    let responses_req: crate::models::responses::ResponsesRequest = serde_json::from_value(data)?;

    if config.verbose {
        tracing::trace!(
            "Request payload: {}",
            serde_json::to_string_pretty(&responses_req).unwrap_or_default()
        );
    }

    let openai_req = responses_to_openai(responses_req)?;

    // Apply model overrides
    let openai_req = apply_openai_model_override(
        openai_req,
        config.resolve_reasoning_model(),
        config.resolve_completion_model(),
    );

    tracing::debug!(
        "Transformed to OpenAI request for model: {}",
        openai_req.model
    );

    let response = send_request(&client, &config, &openai_req, &hook_chain).await?;

    if !response.status().is_success() {
        return Err(handle_upstream_error(response).await);
    }

    let response = if is_streaming {
        handle_streaming_responses(response, &config).await?
    } else {
        // Non-streaming: return error — not yet supported
        return Err(Error::Transform(
            "Non-streaming Responses API requests are not yet supported".to_string(),
        ));
    };

    Ok(response)
}

/// Handle streaming response by converting OpenAI SSE to Responses API SSE
async fn handle_streaming_responses(
    response: reqwest::Response,
    _config: &Config,
) -> Result<Response> {
    let response_id = format!("resp_{}", uuid::Uuid::new_v4().as_simple());
    let item_id = format!("msg_{}", uuid::Uuid::new_v4().as_simple());
    let model = response
        .headers()
        .get("x-model")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string();

    let converter = ResponsesStreamConverter::new(response_id, item_id, model);
    let universal = UniversalConverter::new(converter);

    let upstream = response
        .bytes_stream()
        .map(|result| result.map_err(Error::Http));

    let stream = universal.convert(upstream);

    Ok(build_sse_response(stream))
}
