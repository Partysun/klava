use crate::config::Config;
use crate::error::{Error, Result};
use crate::hooks::{HookChain, HookStage};
use crate::models::{anthropic, openai};
use crate::sse_stream::{sse_openai_to_anthropic_stream, sse_passthrough_stream};
use crate::transform::{anthropic_to_openai, openai_to_anthropic};
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
use std::sync::Arc;
use std::time::Duration;

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
    let openai_req = crate::transform::apply_openai_model_override(
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

    let mut req_builder = client
        .post(&url)
        .json(openai_req)
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

    let data: Value = serde_json::to_value(openai_req)?;
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

    Ok(build_sse_response(sse_openai_to_anthropic_stream(stream)))
}

// ===== OpenAI-specific handlers =====

async fn handle_non_streaming_openai(
    response: reqwest::Response,
    hook_chain: &HookChain,
    config: &Config,
) -> Result<Response> {
    let openai_resp: openai::OpenAIResponse = response.json().await?;

    let data: Value = serde_json::to_value(openai_resp)?;
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

    Ok(build_sse_response(sse_passthrough_stream(stream)))
}
