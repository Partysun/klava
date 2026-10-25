use crate::anthropic::AnthropicStreamConverter;
use crate::anthropic::transform::{anthropic_to_openai, openai_to_anthropic};
use crate::config::Config;
use crate::error::{Error, Result};
use crate::hooks::{HookChain, HookStage};
use crate::models::{anthropic, openai};
use crate::openai_stream::openai_passthrough;
use crate::responses::ResponsesStreamConverter;
use crate::responses::{openai_to_responses, responses_to_openai};
use crate::search;
use crate::stream_converter::UniversalConverter;
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
use std::pin::Pin;
use std::sync::Arc;

/// Anchors a client's model name to the entry it routed to.
///
/// Hooks keep getting one payload; before the send we write the upstream id
/// into `model`, and just before answering we put the client-facing name back
/// so a harness only ever sees the model it asked for (or our default).
#[derive(Clone)]
struct ModelRoute {
    entry: Arc<crate::providers::Model>,
    client_model: String,
}

impl ModelRoute {
    fn new(config: &Config, requested: &str, is_reasoning: bool) -> Result<Self> {
        let entry = config
            .resolve_request_model(requested, is_reasoning)
            .ok_or(Error::NoModelsConfigured)?;
        Ok(Self {
            entry: Arc::new(entry.clone()),
            client_model: requested.trim().to_string(),
        })
    }

    fn upstream_model(&self) -> String {
        self.entry.upstream_model()
    }

    /// Point a parsed request at this route's upstream model.
    fn apply(&self, mut req: openai::OpenAIRequest) -> openai::OpenAIRequest {
        req.model = self.upstream_model();
        req
    }

    /// Put the client-facing model name back into an upstream response.
    fn restore(&self, mut value: Value) -> Value {
        if self.client_model.is_empty() {
            return value;
        }
        if let Some(obj) = value.as_object_mut()
            && obj.contains_key("model")
        {
            obj.insert("model".to_string(), json!(self.client_model));
        }
        value
    }

    /// Same as [`Self::restore`], for OpenAI SSE text (best effort: each
    /// `data:` event that carries a `model` field is rewritten).
    fn restore_sse(&self, bytes: Bytes) -> Bytes {
        if self.client_model.is_empty() {
            return bytes;
        }
        let Ok(text) = std::str::from_utf8(&bytes) else {
            return bytes;
        };
        let mut out = String::with_capacity(text.len());
        for line in text.split_inclusive('\n') {
            let trimmed = line.trim_start();
            if let Some(payload) = trimmed.strip_prefix("data: ")
                && let Ok(mut chunk) = serde_json::from_str::<Value>(payload.trim())
                && let Some(obj) = chunk.as_object_mut()
                && obj.get("model").and_then(Value::as_str).is_some()
            {
                obj.insert("model".to_string(), json!(self.client_model));
                out.push_str(&format!("data: {chunk}\n"));
                continue;
            }
            out.push_str(line);
        }
        Bytes::from(out)
    }
}

/// Whether an OpenAI request asks for reasoning. klava looks at
/// `reasoning_effort`; anything above `none` selects the reasoning model.
fn openai_request_is_reasoning(req: &openai::OpenAIRequest) -> bool {
    req.reasoning_effort
        .as_ref()
        .is_some_and(|effort| !matches!(effort, openai::ReasoningEffort::None))
}

/// Whether a Responses request asks for reasoning. Codex sends
/// `reasoning: {"effort": ...}`; any explicit effort above the default
/// (`none`/`minimal`) selects the configured reasoning model.
fn responses_request_is_reasoning(req: &crate::models::responses::ResponsesRequest) -> bool {
    req.reasoning
        .as_ref()
        .and_then(|r| r.effort.as_deref())
        .is_some_and(|e| !matches!(e, "none" | "minimal"))
}

/// Whether an Anthropic request asks for thinking (`enabled` or `adaptive`).
fn anthropic_request_is_reasoning(req: &anthropic::AnthropicRequest) -> bool {
    req.extra
        .get("thinking")
        .and_then(|v| v.as_object())
        .and_then(|o| o.get("type").and_then(|t| t.as_str()))
        .is_some_and(|t| matches!(t, "enabled" | "adaptive"))
}

/// Handler for Anthropic-compatible requests (/v1/messages)
/// - Parses Anthropic request format
/// - Transforms to OpenAI for upstream
/// - Transforms response back to Anthropic
pub async fn proxy_anthropic(
    Extension(config): Extension<Arc<Config>>,
    Extension(client): Extension<Client>,
    Extension(hook_chain): Extension<Arc<HookChain>>,
    Extension(extensions): Extension<Arc<crate::extensions::Extensions>>,
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

    // Claude Code's built-in WebSearch tool sends a native sub-request (a
    // `web_search_20250305` server tool + forced `tool_choice`). It expects
    // the answer as native `server_tool_use`/`web_search_tool_result` blocks,
    // so handle it before the transform consumes the request — the generic
    // tool loop would return plain text and Claude would report "Did 0
    // searches".
    if config.websearch_available()
        && let Some(query) = search::native::detect(&anthropic_req)
    {
        return proxy_native_search(
            &client,
            &config,
            &hook_chain,
            &anthropic_req,
            &query,
            is_streaming,
        )
        .await;
    }

    // Anthropic carries thinking in `thinking.type` (not `reasoning_effort`),
    // so decide the reasoning/completion default before transforming.
    let is_reasoning = anthropic_request_is_reasoning(&anthropic_req);
    let openai_req = anthropic_to_openai(anthropic_req, None, None)?;
    let route = ModelRoute::new(&config, &openai_req.model, is_reasoning)?;
    let openai_req = route.apply(openai_req);
    tracing::debug!(
        "Transformed to OpenAI request for model: {} (routed to {})",
        openai_req.model,
        route.entry.name
    );

    // Extension chain. Runs on the normalized OpenAI payload so one extension
    // serves every client API; see `proxy_openai` for the rationale.
    let mut openai_req = openai_req;
    if !extensions.is_empty() {
        let payload = serde_json::to_value(&openai_req)?;
        let outcome = crate::extensions::run_request_extensions(
            &extensions,
            &client,
            &config,
            payload,
            crate::extensions::reason_from_request(&openai_req),
            crate::ext_api::SourceApi::Anthropic,
        )
        .await?;

        if let Some(response) = outcome.canned_response {
            return render_canned(
                response,
                is_streaming,
                ClientFormat::Anthropic,
                &hook_chain,
                &config,
                &route,
            );
        }
        openai_req = serde_json::from_value(outcome.payload)?;
    }

    if config.verbose {
        tracing::trace!(
            "Transformed OpenAI request: {}",
            serde_json::to_string_pretty(&openai_req).unwrap_or_default()
        );
    }

    if config.websearch_available() {
        if is_streaming {
            return proxy_streaming_search(
                &client,
                &config,
                &hook_chain,
                openai_req,
                &route,
                ClientFormat::Anthropic,
                None,
            )
            .await;
        }
        return proxy_with_search(
            &client,
            &config,
            &hook_chain,
            openai_req,
            &route,
            ClientFormat::Anthropic,
            is_streaming,
            None,
        )
        .await;
    }

    let response = send_request(&client, &config, &route, &openai_req, &hook_chain).await?;

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
        handle_streaming_anthropic(response, &config, &route).await?
    } else {
        handle_non_streaming_anthropic(response, &hook_chain, &config, &route).await?
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
    Extension(extensions): Extension<Arc<crate::extensions::Extensions>>,
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

    // Route on the model the client sent; unknown names fall back to the
    // configured defaults (see `Config::resolve_request_model`).
    let route = ModelRoute::new(
        &config,
        &openai_req.model,
        openai_request_is_reasoning(&openai_req),
    )?;
    let mut openai_req = route.apply(openai_req);
    tracing::debug!(
        "Requested model '{}' routed to entry '{}' (upstream {})",
        route.client_model,
        route.entry.name,
        openai_req.model
    );

    // Extension chain: transforms and escalation.
    //
    // Escalation may fold reviewer remarks into the conversation, so it runs
    // here — after model selection, before any send — and works for streaming
    // clients too, because the request body is complete at this point.
    if !extensions.is_empty() {
        let payload = serde_json::to_value(&openai_req)?;
        let outcome = crate::extensions::run_request_extensions(
            &extensions,
            &client,
            &config,
            payload,
            crate::extensions::reason_from_request(&openai_req),
            crate::ext_api::SourceApi::OpenAi,
        )
        .await?;

        // `take_over` already produced the answer: render it instead of calling
        // the provider. Use the original client preference for the shape.
        if let Some(response) = outcome.canned_response {
            return render_canned(
                response,
                is_streaming,
                ClientFormat::OpenAi,
                &hook_chain,
                &config,
                &route,
            );
        }
        openai_req = serde_json::from_value(outcome.payload)?;
    }

    // if config.verbose {
    //     tracing::trace!(
    //         "Request payload: {}",
    //         serde_json::to_string_pretty(&openai_req).unwrap_or_default()
    //     );
    // }

    if config.websearch_available() {
        if is_streaming {
            return proxy_streaming_search(
                &client,
                &config,
                &hook_chain,
                openai_req,
                &route,
                ClientFormat::OpenAi,
                None,
            )
            .await;
        }
        return proxy_with_search(
            &client,
            &config,
            &hook_chain,
            openai_req,
            &route,
            ClientFormat::OpenAi,
            is_streaming,
            None,
        )
        .await;
    }

    let response = send_request(&client, &config, &route, &openai_req, &hook_chain).await?;

    if !response.status().is_success() {
        return Err(handle_upstream_error(response).await);
    }

    let response = if is_streaming {
        handle_streaming_openai(response, &config, &route).await?
    } else {
        handle_non_streaming_openai(response, &hook_chain, &config, &route).await?
    };

    Ok(response)
}

/// Render an already-produced response (escalation `take_over`), honouring the
/// client's stream preference and the real hook chain and config.
fn render_canned(
    value: Value,
    client_streaming: bool,
    format: ClientFormat,
    hook_chain: &HookChain,
    config: &Config,
    route: &ModelRoute,
) -> Result<Response> {
    render_final(
        route.restore(value),
        format,
        client_streaming,
        None,
        hook_chain,
        config,
    )
}

/// Max request body we accept from a client. Code-agent clients (opencode,
/// Claude Code) can send very large conversations with tool results and file
/// contents; 1 MiB was too small and produced 502s on legitimate requests.
/// Matches the upstream rupilot proxy's `MAX_BODY`.
const MAX_BODY: usize = 128 * 1024 * 1024;

async fn read_body_bytes(request: Request) -> Result<bytes::Bytes> {
    read_body_bytes_limited(request, MAX_BODY).await
}

/// Body reader with an explicit byte cap. Kept separate from `read_body_bytes`
/// so tests can exercise the limit without allocating a 128 MiB request.
async fn read_body_bytes_limited(request: Request, limit: usize) -> Result<bytes::Bytes> {
    axum::body::to_bytes(request.into_body(), limit)
        .await
        .map_err(|e| Error::Upstream(format!("Failed to read request body: {}", e)))
}

/// Send request to upstream API
async fn send_request(
    client: &Client,
    config: &Config,
    route: &ModelRoute,
    openai_req: &openai::OpenAIRequest,
    hook_chain: &HookChain,
) -> Result<reqwest::Response> {
    let request_payload = build_upstream_payload(route, openai_req);

    // The hook result must be the payload we send: a `BeforeUpstream` hook is
    // a transform, and discarding its return value silently disabled every
    // request-side rewrite (guardrails, routing).
    let request_payload = hook_chain.execute(HookStage::BeforeUpstream, request_payload, config)?;

    send_openai_payload_via(client, config, &route.entry, openai_req, request_payload).await
}

/// Serialize a typed request into the upstream JSON payload, applying the
/// provider-specific tweaks (Qwen incremental output / default system message).
fn build_upstream_payload(route: &ModelRoute, openai_req: &openai::OpenAIRequest) -> Value {
    let url = route.entry.chat_completions_url().unwrap_or_default();
    build_upstream_payload_for(openai_req, &url)
}

/// Same as [`build_upstream_payload`], for an explicit upstream URL.
fn build_upstream_payload_for(openai_req: &openai::OpenAIRequest, url: &str) -> Value {
    let mut request_payload = serde_json::to_value(openai_req).unwrap_or_else(|_| json!({}));

    // Special handling for Qwen models that require specific parameters.
    // Thinking stays enabled (these are reasoning models) — we only set
    // incremental_output so reasoning/content streams as SSE deltas.
    // `incremental_output` is meaningless for non-streaming requests, so it is
    // only applied when streaming.
    if url.contains("dashscope") || url.contains("qwen") || openai_req.model.contains("qwen") {
        if openai_req.stream.unwrap_or(false) {
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

    request_payload
}

/// POST an already-built JSON payload to one model entry. Used by the normal
/// proxy path and by the web-search loop (both send chat-completions requests
/// to the same endpoint with the same auth).
pub(crate) async fn send_openai_payload_via(
    client: &Client,
    config: &Config,
    entry: &crate::providers::Model,
    openai_req: &openai::OpenAIRequest,
    request_payload: Value,
) -> Result<reqwest::Response> {
    let url = entry
        .chat_completions_url()
        .ok_or_else(|| Error::Internal(format!("model '{}' has no base URL", entry.name)))?;
    tracing::debug!("Sending request to {} for model: {}", url, openai_req.model);

    let mut req_builder = client.post(&url).json(&request_payload);

    // Note: no per-request `.timeout()` here — a hard total timeout would cut
    // long streaming responses mid-generation. The client's `read_timeout`
    // (main.rs build_http_client) bounds silent connections instead.

    // Conditionally add OpenRouter headers if base URL contains "openrouter"
    if url.contains("openrouter") {
        req_builder = req_builder
            .header("X-OpenRouter-Title", "Klava")
            .header("X-OpenRouter-Categories", "cli-agent-proxy");
    }

    if let Some(headers) = entry.get_auth_headers(config).await? {
        req_builder = req_builder.headers(headers);
    }

    req_builder
        .send()
        .await
        .map_err(|e| Error::Upstream(format!("Failed to send request to upstream ({url}): {e} — check provider connectivity with `klava check`")))
}

/// Send a typed OpenAI request through an explicit model entry (or the
/// configured default when `entry` is `None`). Runs no hooks — this is the
/// recursion boundary used by nested calls such as web search and model
/// escalation.
pub(crate) async fn send_openai_request_via(
    client: &Client,
    config: &Config,
    openai_req: &openai::OpenAIRequest,
    entry: Option<&crate::providers::Model>,
) -> Result<reqwest::Response> {
    let owned;
    let entry = match entry {
        Some(entry) => entry,
        None => {
            owned = config
                .default_completion()
                .ok_or(Error::NoModelsConfigured)?
                .clone();
            &owned
        }
    };
    let url = entry
        .chat_completions_url()
        .ok_or_else(|| Error::Internal(format!("model '{}' has no base URL", entry.name)))?;
    let payload = build_upstream_payload_for(openai_req, &url);
    send_openai_payload_via(client, config, entry, openai_req, payload).await
}

// ===== Web search loop =====

/// Which client API a search-enabled request came in on, so the loop can
/// render the final answer back in the right shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClientFormat {
    OpenAi,
    Anthropic,
    Responses,
}

/// How many search executions one client request may trigger before klava
/// stops and returns whatever the model has.
const MAX_SEARCH_ROUNDS: usize = 3;

/// Drive a search-enabled, streaming request to completion.
///
/// The first upstream round streams (preserving live token streaming); the
/// [`search::stream::drive`] loop intercepts any `web_search` tool calls at the
/// SSE level, runs the searches, and splices the follow-up rounds into the same
/// converter so the client sees one continuous answer.
async fn proxy_streaming_search(
    client: &Client,
    config: &Arc<Config>,
    hook_chain: &HookChain,
    mut openai_req: openai::OpenAIRequest,
    route: &ModelRoute,
    format: ClientFormat,
    responses_req: Option<&crate::models::responses::ResponsesRequest>,
) -> Result<Response> {
    search::inject_tool(&mut openai_req);
    let model = if route.client_model.is_empty() {
        openai_req.model.clone()
    } else {
        route.client_model.clone()
    };

    let response = send_request(client, config, route, &openai_req, hook_chain).await?;
    if !response.status().is_success() {
        return Err(handle_upstream_error(response).await);
    }

    let stream = search::stream::drive(
        client.clone(),
        config.clone(),
        openai_req,
        route.entry.as_ref().clone(),
        response,
    );
    // The loop forwards upstream SSE bytes; rewrite the model id on each event
    // so the answer carries the name the client asked for.
    let route = route.clone();
    let stream = stream.map(move |chunk| chunk.map(|bytes| route.restore_sse(bytes)));

    let converted: Pin<Box<dyn Stream<Item = Result<Bytes>> + Send>> = match format {
        ClientFormat::OpenAi => Box::pin(openai_passthrough(stream)),
        ClientFormat::Anthropic => {
            let converter = AnthropicStreamConverter::new();
            Box::pin(UniversalConverter::new(converter).convert(stream))
        }
        ClientFormat::Responses => {
            let response_id = format!("resp_{}", uuid::Uuid::new_v4().as_simple());
            let item_id = format!("msg_{}", uuid::Uuid::new_v4().as_simple());
            let converter = ResponsesStreamConverter::new(response_id, item_id, model);
            Box::pin(UniversalConverter::new(converter).convert(stream))
        }
    };
    let _ = responses_req;
    Ok(build_sse_response(converted))
}

/// Answer a Claude Code WebSearch sub-request with native Anthropic blocks.
///
/// The sub-request carries the `web_search_20250305` *server* tool and a forced
/// `tool_choice`; it expects the response to be `server_tool_use` +
/// `web_search_tool_result` + text (what Anthropic's own backend returns).
/// klava runs the search directly and rebuilds that native shape, so Claude
/// counts the search and surfaces the cited sources.
async fn proxy_native_search(
    client: &Client,
    config: &Config,
    hook_chain: &HookChain,
    anthropic_req: &anthropic::AnthropicRequest,
    query: &str,
    client_streaming: bool,
) -> Result<Response> {
    let settings = search::resolve_settings(config, query, None).await;
    tracing::info!(
        "Web search [1]: query={:?} engine={} context={} max_results={}",
        query,
        settings.engine.as_str(),
        settings.context.as_str(),
        settings.max_results
    );

    let outcome = match search::execute_outcome(client, config, query, &settings).await {
        Ok(outcome) => outcome,
        Err(e) => {
            tracing::warn!("Web search failed: {e}");
            // Keep the search counted and hand Claude a visible failure rather
            // than a stream that never completes.
            search::wire::SearchOutcome {
                content: format!("Web search failed: {e}"),
                sources: vec![],
            }
        }
    };

    let result = search::native::NativeSearchResult {
        query: query.to_string(),
        outcome,
    };
    // The search itself ran through a separate upstream request, so klava has
    // no token accounting for this one; the values are informational.
    let input_tokens = 0;
    let output_tokens = 0;

    if client_streaming {
        let sse = search::native::response_sse(
            &anthropic_req.model,
            &result,
            input_tokens,
            output_tokens,
        );
        let stream = tokio_stream::iter(vec![Ok::<Bytes, Error>(Bytes::from(sse))]);
        return Ok(build_sse_response(stream));
    }

    let body =
        search::native::response_body(&anthropic_req.model, &result, input_tokens, output_tokens);
    let body = hook_chain.execute(HookStage::BeforeResponse, body, config)?;
    Ok(Json(body).into_response())
}

/// Drive a search-enabled request to completion.
///
/// The tool loop is owned entirely by klava: the upstream request always runs
/// non-streaming so tool calls can be intercepted, searches are executed, and
/// the final answer is rendered in the client's format (synthesizing an SSE
/// stream for streaming clients). The client never sees the search tool
/// definition or the intercepted tool calls.
#[allow(clippy::too_many_arguments)]
async fn proxy_with_search(
    client: &Client,
    config: &Config,
    hook_chain: &HookChain,
    mut openai_req: openai::OpenAIRequest,
    route: &ModelRoute,
    format: ClientFormat,
    client_streaming: bool,
    responses_req: Option<&crate::models::responses::ResponsesRequest>,
) -> Result<Response> {
    search::inject_tool(&mut openai_req);
    // The user's own wording for this turn, so an explicit "use Yandex" is
    // not lost when the harness model rewrites the search query.
    let user_context = search::user_context(&openai_req);
    // The loop inspects tool calls, which requires a non-streaming upstream
    // response. The client's stream preference is honored when rendering.
    openai_req.stream = Some(false);

    let mut response = send_request(client, config, route, &openai_req, hook_chain).await?;
    if !response.status().is_success() {
        return Err(handle_upstream_error(response).await);
    }

    let mut rounds = 0usize;
    loop {
        let value: Value = response.json().await?;

        let split = search::split_tool_calls(&value);
        if !split.needs_search() {
            return render_final(
                route.restore(value),
                format,
                client_streaming,
                responses_req,
                hook_chain,
                config,
            );
        }

        // Other tools in the same turn belong to the client. klava runs its
        // searches, drops its own calls, and hands the rest back so the client
        // can answer them — the model's turn is not re-run.
        let other_calls_present = split.has_other_calls;

        let searches: Vec<_> = split
            .searches
            .into_iter()
            .filter(|call| !call.query.trim().is_empty())
            .collect();

        if rounds >= MAX_SEARCH_ROUNDS || searches.is_empty() {
            // Give up on searching: hand the model's text back without the
            // unresolved tool call so the client isn't left with a dangling
            // call it has no definition for.
            return render_final(
                route.restore(strip_search_call(value)),
                format,
                client_streaming,
                responses_req,
                hook_chain,
                config,
            );
        }
        rounds += 1;

        let mut results: Vec<(search::SearchCall, String)> = Vec::with_capacity(searches.len());
        for call in searches {
            let settings =
                search::resolve_settings(config, &call.query, user_context.as_deref()).await;
            tracing::info!(
                "Web search [{}]: query={:?} engine={} context={} max_results={}",
                rounds,
                call.query,
                settings.engine.as_str(),
                settings.context.as_str(),
                settings.max_results
            );

            let result = match search::execute(client, config, &call.query, &settings).await {
                Ok(result) => result,
                Err(e) => {
                    tracing::warn!("Web search failed: {e}");
                    format!("Web search failed: {e}")
                }
            };
            results.push((call, result));
        }

        search::append_search_results(&mut openai_req, &results);

        if other_calls_present {
            // The client must answer its own tools; return the model's turn
            // with only the search calls removed.
            return render_final(
                route.restore(strip_search_call(value)),
                format,
                client_streaming,
                responses_req,
                hook_chain,
                config,
            );
        }

        response = send_openai_request_via(client, config, &openai_req, Some(&route.entry)).await?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(Error::Upstream(format!(
                "Upstream returned {status} after web search: {body}"
            )));
        }
    }
}

/// Drop the intercepted `web_search` tool calls from a final response, keeping
/// any other tool calls and any text the model produced alongside them.
fn strip_search_call(mut value: Value) -> Value {
    let Some(message) = value
        .get_mut("choices")
        .and_then(Value::as_array_mut)
        .and_then(|c| c.first_mut())
        .and_then(|c| c.get_mut("message"))
        .and_then(Value::as_object_mut)
    else {
        return value;
    };

    let remaining: Vec<Value> = message
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|calls| {
            calls
                .iter()
                .filter(|call| {
                    call.get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(Value::as_str)
                        != Some(crate::search::TOOL_NAME)
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default();

    if remaining.is_empty() {
        message.remove("tool_calls");
        // Only substitute placeholder text when the model produced nothing.
        if message.get("content").is_none_or(Value::is_null) {
            message.insert(
                "content".to_string(),
                Value::String("I couldn't retrieve web results for that query.".to_string()),
            );
        }
    } else {
        message.insert("tool_calls".to_string(), Value::Array(remaining));
    }

    value
}

/// Render the final upstream answer in the client's API shape.
fn render_final(
    value: Value,
    format: ClientFormat,
    client_streaming: bool,
    responses_req: Option<&crate::models::responses::ResponsesRequest>,
    hook_chain: &HookChain,
    config: &Config,
) -> Result<Response> {
    if client_streaming {
        let bytes = synthetic_openai_sse(&value);
        let upstream = tokio_stream::iter(vec![Ok::<Bytes, Error>(bytes)]);
        return Ok(match format {
            ClientFormat::OpenAi => build_sse_response(upstream),
            ClientFormat::Anthropic => {
                let converter = AnthropicStreamConverter::new();
                build_sse_response(UniversalConverter::new(converter).convert(upstream))
            }
            ClientFormat::Responses => {
                let model = value
                    .get("model")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let response_id = format!("resp_{}", uuid::Uuid::new_v4().as_simple());
                let item_id = format!("msg_{}", uuid::Uuid::new_v4().as_simple());
                let converter = ResponsesStreamConverter::new(response_id, item_id, model);
                build_sse_response(UniversalConverter::new(converter).convert(upstream))
            }
        });
    }

    let typed: openai::OpenAIResponse = serde_json::from_value(value.clone())?;
    let body = match format {
        ClientFormat::OpenAi => value,
        ClientFormat::Anthropic => serde_json::to_value(openai_to_anthropic(typed)?)?,
        ClientFormat::Responses => {
            serde_json::to_value(openai_to_responses(typed, responses_req)?)?
        }
    };
    let body = hook_chain.execute(HookStage::BeforeResponse, body, config)?;
    Ok(Json(body).into_response())
}

/// Synthesize an OpenAI SSE stream that carries a complete final answer, so
/// streaming clients receive a valid stream for a non-streamed upstream reply.
fn synthetic_openai_sse(value: &Value) -> Bytes {
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("chatcmpl-search");
    let model = value.get("model").and_then(Value::as_str).unwrap_or("");
    let created = value.get("created").and_then(Value::as_u64).unwrap_or(0);
    let choice = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first());
    let message = choice.and_then(|c| c.get("message"));
    let content = message
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let reasoning = message
        .and_then(|m| m.get("reasoning_content"))
        .and_then(Value::as_str);
    let finish = choice
        .and_then(|c| c.get("finish_reason"))
        .and_then(Value::as_str)
        .unwrap_or("stop");

    let mut delta = json!({ "role": "assistant", "content": content });
    if let Some(reasoning) = reasoning {
        delta["reasoning"] = json!(reasoning);
    }

    let mut out = String::new();
    for chunk in [
        json!({
            "id": id, "object": "chat.completion.chunk", "created": created, "model": model,
            "choices": [{"index": 0, "delta": delta, "finish_reason": null}]
        }),
        json!({
            "id": id, "object": "chat.completion.chunk", "created": created, "model": model,
            "choices": [{"index": 0, "delta": {}, "finish_reason": finish}]
        }),
    ] {
        out.push_str(&format!("data: {}\n\n", chunk));
    }
    out.push_str("data: [DONE]\n\n");
    Bytes::from(out)
}

/// Handle upstream error response, annotating it with an actionable hint so
/// the client sees a clear message instead of a bare status code.
async fn handle_upstream_error(response: reqwest::Response) -> Error {
    let status = response.status();
    let error_text = response
        .text()
        .await
        .unwrap_or_else(|_| "Unknown error".to_string());
    let hint = match status.as_u16() {
        401 | 403 => " - provider rejected the credentials: run `klava login` or `klava check`",
        429 => " - provider rate limit reached: wait a moment and retry",
        500..=599 => " - upstream provider issue: run `klava check` for diagnostics",
        _ => "",
    };
    tracing::error!("Upstream error ({}): {}", status, error_text);
    Error::Upstream(format!(
        "Upstream returned {}: {}{}",
        status, error_text, hint
    ))
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
    route: &ModelRoute,
) -> Result<Response> {
    let openai_resp: openai::OpenAIResponse = response.json().await?;
    let anthropic_resp = openai_to_anthropic(openai_resp)?;

    let data: Value = route.restore(serde_json::to_value(anthropic_resp)?);
    let resp = hook_chain.execute(HookStage::BeforeResponse, data, config)?;
    Ok(Json(resp).into_response())
}

async fn handle_streaming_anthropic(
    response: reqwest::Response,
    _config: &Config,
    route: &ModelRoute,
) -> Result<Response> {
    let stream = response.bytes_stream().inspect(|chunk| {
        if let Ok(bytes) = chunk {
            tracing::trace!("[UPSTREAM] {}", String::from_utf8_lossy(bytes).trim());
        }
    });

    let route = route.clone();
    let upstream =
        stream.map(move |result| result.map(|b| route.restore_sse(b)).map_err(Error::Http));
    let converter = AnthropicStreamConverter::new();
    let universal = UniversalConverter::new(converter);

    Ok(build_sse_response(universal.convert(upstream)))
}

// ===== OpenAI-specific handlers =====

async fn handle_non_streaming_openai(
    response: reqwest::Response,
    hook_chain: &HookChain,
    config: &Config,
    route: &ModelRoute,
) -> Result<Response> {
    let openai_resp: openai::OpenAIResponse = response.json().await?;

    let data: Value = route.restore(serde_json::to_value(openai_resp)?);
    let resp = hook_chain.execute(HookStage::BeforeResponse, data, config)?;
    Ok(Json(resp).into_response())
}

async fn handle_streaming_openai(
    response: reqwest::Response,
    _config: &Config,
    route: &ModelRoute,
) -> Result<Response> {
    let stream = response.bytes_stream();
    let route = route.clone();
    let upstream =
        stream.map(move |result| result.map(|b| route.restore_sse(b)).map_err(Error::Http));
    let stream = openai_passthrough(Box::pin(upstream));

    Ok(build_sse_response(stream))
}

// ===== Responses API handler =====

async fn handle_non_streaming_responses(
    response: reqwest::Response,
    hook_chain: &HookChain,
    config: &Config,
    route: &ModelRoute,
    req: Option<crate::models::responses::ResponsesRequest>,
) -> Result<Response> {
    let openai_resp: openai::OpenAIResponse = response.json().await?;
    let responses_resp = openai_to_responses(openai_resp, req.as_ref())?;

    let data: Value = route.restore(serde_json::to_value(responses_resp)?);
    let resp = hook_chain.execute(HookStage::BeforeResponse, data, config)?;
    Ok(Json(resp).into_response())
}

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
    Extension(extensions): Extension<Arc<crate::extensions::Extensions>>,
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

    // Detect reasoning requests from the Responses `reasoning.effort` field.
    // The transformed OpenAI request keeps `reasoning_effort` unset (many
    // OpenAI-compatible providers reject the field), so the decision is made
    // here instead of inside `apply_openai_model_override`.
    let is_reasoning = responses_request_is_reasoning(&responses_req);

    let openai_req = responses_to_openai(responses_req.clone())?;

    // Route on the model the client sent (see `proxy_openai`).
    let route = ModelRoute::new(&config, &openai_req.model, is_reasoning)?;
    let openai_req = route.apply(openai_req);

    tracing::debug!(
        "Requested model '{}' routed to entry '{}' (upstream {})",
        route.client_model,
        route.entry.name,
        openai_req.model
    );

    // Extension chain, on the normalized OpenAI payload (see `proxy_openai`).
    let mut openai_req = openai_req;
    if !extensions.is_empty() {
        let payload = serde_json::to_value(&openai_req)?;
        let outcome = crate::extensions::run_request_extensions(
            &extensions,
            &client,
            &config,
            payload,
            crate::extensions::reason_from_request(&openai_req),
            crate::ext_api::SourceApi::Responses,
        )
        .await?;

        if let Some(response) = outcome.canned_response {
            return render_canned(
                response,
                is_streaming,
                ClientFormat::Responses,
                &hook_chain,
                &config,
                &route,
            );
        }
        openai_req = serde_json::from_value(outcome.payload)?;
    }

    if config.websearch_available() {
        if is_streaming {
            return proxy_streaming_search(
                &client,
                &config,
                &hook_chain,
                openai_req,
                &route,
                ClientFormat::Responses,
                Some(&responses_req),
            )
            .await;
        }
        return proxy_with_search(
            &client,
            &config,
            &hook_chain,
            openai_req,
            &route,
            ClientFormat::Responses,
            is_streaming,
            Some(&responses_req),
        )
        .await;
    }

    let response = send_request(&client, &config, &route, &openai_req, &hook_chain).await?;

    if !response.status().is_success() {
        return Err(handle_upstream_error(response).await);
    }

    let response = if is_streaming {
        let model = if route.client_model.is_empty() {
            openai_req.model.clone()
        } else {
            route.client_model.clone()
        };
        handle_streaming_responses(response, model).await?
    } else {
        handle_non_streaming_responses(response, &hook_chain, &config, &route, Some(responses_req))
            .await?
    };

    Ok(response)
}

/// Handle streaming response by converting OpenAI SSE to Responses API SSE
async fn handle_streaming_responses(
    response: reqwest::Response,
    model: String,
) -> Result<Response> {
    let response_id = format!("resp_{}", uuid::Uuid::new_v4().as_simple());
    let item_id = format!("msg_{}", uuid::Uuid::new_v4().as_simple());

    let converter = ResponsesStreamConverter::new(response_id, item_id, model);
    let universal = UniversalConverter::new(converter);

    let upstream = response
        .bytes_stream()
        .map(|result| result.map_err(Error::Http));

    let stream = universal.convert(upstream);

    Ok(build_sse_response(stream))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::responses::{ResponsesInput, ResponsesReasoning, ResponsesRequest};

    fn responses_req_with_effort(effort: Option<&str>) -> ResponsesRequest {
        ResponsesRequest {
            model: "gpt-4".to_string(),
            background: false,
            conversation: None,
            include: vec![],
            input: ResponsesInput::Text("hi".to_string()),
            instructions: None,
            max_output_tokens: None,
            reasoning: effort.map(|e| ResponsesReasoning {
                effort: Some(e.to_string()),
                generate_summary: None,
                summary: None,
            }),
            temperature: None,
            text: None,
            top_p: None,
            truncation: None,
            tools: vec![],
            stream: None,
            extra: json!({}),
        }
    }

    #[test]
    fn responses_request_reasoning_detection() {
        assert!(responses_request_is_reasoning(&responses_req_with_effort(
            Some("high")
        )));
        assert!(responses_request_is_reasoning(&responses_req_with_effort(
            Some("low")
        )));
        assert!(!responses_request_is_reasoning(&responses_req_with_effort(
            Some("none")
        )));
        assert!(!responses_request_is_reasoning(&responses_req_with_effort(
            Some("minimal")
        )));
        assert!(!responses_request_is_reasoning(&responses_req_with_effort(
            None
        )));
    }

    fn route_config(completion: &str, reasoning: &str) -> Config {
        use crate::providers::{Model, Type as ProviderType};
        Config {
            port: 0,
            verbose: false,
            enable_token_stats: false,
            completion_model: Some(completion.to_string()),
            reasoning_model: Some(reasoning.to_string()),
            models: vec![
                Model {
                    name: completion.to_string(),
                    provider_type: ProviderType::OpenAICompatible,
                    base_url: Some("https://chat.example".to_string()),
                    api_key: Some("k".to_string()),
                    ..Model::default()
                },
                Model {
                    name: reasoning.to_string(),
                    provider_type: ProviderType::OpenAICompatible,
                    base_url: Some("https://think.example".to_string()),
                    api_key: Some("k".to_string()),
                    ..Model::default()
                },
            ],
            ..Config::default()
        }
    }

    #[test]
    fn route_selects_the_default_by_reasoning_flag() {
        let config = route_config("chat-model", "thinking-model");
        let reasoning = ModelRoute::new(&config, "klava", true).unwrap();
        assert_eq!(reasoning.entry.name, "thinking-model");

        let completion = ModelRoute::new(&config, "klava", false).unwrap();
        assert_eq!(completion.entry.name, "chat-model");
    }

    #[test]
    fn route_honours_a_model_name_the_client_sent() {
        let config = route_config("chat-model", "thinking-model");
        let route = ModelRoute::new(&config, "thinking-model", false).unwrap();
        assert_eq!(route.entry.name, "thinking-model");
        // The client keeps seeing the name it asked for.
        assert_eq!(route.client_model, "thinking-model");
    }

    #[test]
    fn route_writes_the_upstream_id_and_restores_the_client_name() {
        let mut config = route_config("chat-model", "chat-model");
        config.models[0].model = Some("vendor/real-id".to_string());
        config
            .models
            .retain(|m| m.name != "chat-model" || m.model.is_some());
        let route = ModelRoute::new(&config, "chat-model", false).unwrap();
        let req = openai::OpenAIRequest {
            model: "chat-model".to_string(),
            messages: vec![],
            max_tokens: None,
            temperature: None,
            top_p: None,
            stop: None,
            stream: None,
            tools: None,
            tool_choice: None,
            reasoning_effort: None,
            extra: json!({}),
        };
        assert_eq!(route.apply(req).model, "vendor/real-id");

        let restored = route.restore(json!({"model": "vendor/real-id", "choices": []}));
        assert_eq!(restored["model"], json!("chat-model"));
    }

    #[test]
    fn route_requires_a_configured_model() {
        let config = Config::default();
        assert!(matches!(
            ModelRoute::new(&config, "x", false),
            Err(Error::NoModelsConfigured)
        ));
    }

    #[test]
    fn openai_reasoning_flag_follows_the_effort() {
        fn req(effort: Option<openai::ReasoningEffort>) -> openai::OpenAIRequest {
            openai::OpenAIRequest {
                model: "m".to_string(),
                messages: vec![],
                max_tokens: None,
                temperature: None,
                top_p: None,
                stop: None,
                stream: None,
                tools: None,
                tool_choice: None,
                reasoning_effort: effort,
                extra: json!({}),
            }
        }

        assert!(openai_request_is_reasoning(&req(Some(
            openai::ReasoningEffort::High
        ))));
        assert!(openai_request_is_reasoning(&req(Some(
            openai::ReasoningEffort::Medium
        ))));
        assert!(!openai_request_is_reasoning(&req(None)));
        assert!(!openai_request_is_reasoning(&req(Some(
            openai::ReasoningEffort::None
        ))));
    }

    #[tokio::test]
    async fn read_body_bytes_under_limit_ok() {
        let req = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from("x".repeat(100)))
            .unwrap();
        let bytes = read_body_bytes_limited(req, 1000).await.unwrap();
        assert_eq!(bytes.len(), 100);
    }

    #[tokio::test]
    async fn read_body_bytes_over_limit_errors() {
        let req = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .body(Body::from("x".repeat(100)))
            .unwrap();
        let err = read_body_bytes_limited(req, 10).await.unwrap_err();
        assert!(
            err.to_string().contains("Failed to read request body"),
            "unexpected error: {err}"
        );
    }
}
