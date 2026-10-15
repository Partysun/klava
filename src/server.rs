use crate::{config::Config, hooks::chain::HookChain};
use axum::{Extension, Router, routing::get};
use reqwest::Client;
use std::sync::Arc;
use tower_http::{
    cors::{Any, CorsLayer},
    trace::TraceLayer,
};
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

/// Create CORS layer with default configuration
pub fn create_cors_layer() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any)
}

/// Create a health router
pub fn create_health_router() -> Router {
    Router::new().route("/health", get(|| async { "OK" }))
}

/// Create and configure the application router
pub fn create_app(
    config: Arc<Config>,
    client: Client,
    cors: CorsLayer,
    hook_chain: Arc<HookChain>,
) -> Router {
    Router::new()
        .route(
            "/v1/messages",
            axum::routing::post(crate::proxy::proxy_anthropic),
        )
        .route(
            "/v1/chat/completions",
            axum::routing::post(crate::proxy::proxy_openai),
        )
        .route(
            "/v1/responses",
            axum::routing::post(crate::proxy::proxy_responses),
        )
        .merge(create_health_router())
        // .layer(axum::middleware::from_fn(
        //     |req: axum::http::Request<axum::body::Body>, next: axum::middleware::Next| async move {
        //         println!(">>> Request: {} {}", req.method(), req.uri());
        //         println!(">>> Headers: {:?}", req.headers());
        //         next.run(req).await
        //     },
        // ))
        .layer(Extension(config))
        .layer(Extension(hook_chain))
        .layer(Extension(client))
        .layer(TraceLayer::new_for_http())
        .layer(cors)
}

/// Logging output mode for the proxy server.
///
/// Composable via `|` — e.g. `LogMode::STDOUT | LogMode::FILE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogMode(u8);

impl Default for LogMode {
    fn default() -> Self {
        Self::STDOUT
    }
}

impl LogMode {
    /// Log to stdout/stderr.
    pub const STDOUT: Self = Self(1 << 0);
    /// Log to file in tmp dir.
    pub const FILE: Self = Self(1 << 1);
    /// Log to file only (used during `klava launch`).
    pub const SILENT: Self = Self::FILE;
}

impl std::ops::BitOr for LogMode {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

/// Load logging configuration
pub fn configure_logging(verbose: bool, mode: LogMode) {
    let log_level = if verbose {
        tracing::Level::TRACE
    } else {
        tracing::Level::INFO
    };

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| format!("klava={}", log_level).into());

    let registry = tracing_subscriber::registry().with(env_filter);

    match mode {
        LogMode::STDOUT => {
            registry.with(tracing_subscriber::fmt::layer()).init();
        }
        LogMode::FILE => {
            let file_appender = RollingFileAppender::new(
                Rotation::WEEKLY,
                std::env::temp_dir(),
                "klava_server.log",
            );
            let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
            registry
                .with(tracing_subscriber::fmt::layer().with_writer(non_blocking))
                .init();
            Box::leak(Box::new(guard));
        }
        _ => {
            // STDOUT | FILE — apply both layers, then init once
            let file_appender = RollingFileAppender::new(
                Rotation::WEEKLY,
                std::env::temp_dir(),
                "klava_server.log",
            );
            let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
            registry
                .with(tracing_subscriber::fmt::layer())
                .with(tracing_subscriber::fmt::layer().with_writer(non_blocking))
                .init();
            Box::leak(Box::new(guard));
        }
    }
}

/// Bind a TCP listener to the requested port, trying subsequent ports (+1)
/// if the requested one is already in use.
///
/// Returns the listener and the port it was actually bound to.
pub async fn bind_listener(start_port: u16) -> std::io::Result<(tokio::net::TcpListener, u16)> {
    let mut port = start_port;
    loop {
        match tokio::net::TcpListener::bind(("0.0.0.0", port)).await {
            Ok(listener) => {
                let bound_port = listener.local_addr()?.port();
                return Ok((listener, bound_port));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && port < u16::MAX => {
                tracing::warn!("Port {} is already in use, trying port {}", port, port + 1);
                port += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

pub async fn run_server(
    config: Arc<Config>,
    client: reqwest::Client,
    hook_chain: Arc<HookChain>,
    log_mode: LogMode,
    port_tx: Option<tokio::sync::oneshot::Sender<u16>>,
) -> anyhow::Result<u16> {
    configure_logging(config.verbose, log_mode);

    tracing::info!("Starting Klava Proxy v{}", env!("CARGO_PKG_VERSION"));
    tracing::info!("Port: {}", config.port);

    // Get the active provider config for logging
    if let Some(active_config) = config.get_active_provider_config() {
        if let Some(ref url) = config.resolve_base_url() {
            tracing::info!("Upstream URL: {}", url);
        }

        if let Some(ref model) = config.resolve_reasoning_model() {
            tracing::info!("Reasoning Model Override: {}", model);
        }

        if let Some(ref model) = config.resolve_completion_model() {
            tracing::info!("Completion Model Override: {}", model);
        }

        if active_config.api_key.is_some() {
            tracing::info!("API Key: configured");
        } else if config.resolve_api_key().is_some() {
            tracing::info!("API Key: configured (via env var)");
        } else {
            tracing::info!("API Key: not set (using unauthenticated endpoint)");
        }
    }

    let cors = create_cors_layer();
    let (listener, port) = bind_listener(config.port).await?;
    let app = create_app(config, client, cors, hook_chain);

    if let Some(tx) = port_tx {
        let _ = tx.send(port);
    }

    tracing::info!("Listening on 0.0.0.0:{}", port);
    tracing::info!("Proxy is live!");

    axum::serve(listener, app).await?;

    Ok(port)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bind_listener_uses_requested_port_when_free() {
        let (listener, port) = bind_listener(0).await.unwrap();
        assert_eq!(listener.local_addr().unwrap().port(), port);
        assert_ne!(port, 0);
        drop(listener);
    }

    #[tokio::test]
    async fn bind_listener_skips_occupied_port() {
        let occupied = tokio::net::TcpListener::bind(("0.0.0.0", 0)).await.unwrap();
        let occupied_port = occupied.local_addr().unwrap().port();

        let (listener, port) = bind_listener(occupied_port).await.unwrap();
        assert_eq!(port, occupied_port + 1);
        assert_eq!(listener.local_addr().unwrap().port(), port);
        drop(listener);
    }
}

/// End-to-end tests for the whole request pipeline (client request ->
/// proxy handler -> upstream -> response transform), exercised through
/// `create_app` against an in-process fake upstream server.
#[cfg(test)]
mod integration_tests {
    use super::*;
    use crate::config::Config;
    use crate::hooks::chain::HookChain;
    use crate::providers::{Config as ProviderConfig, Type as ProviderType};
    use axum::body::{Body, to_bytes};
    use axum::extract::Request as HttpRequest;
    use axum::http::Response;
    use axum::routing::post;
    use serde_json::{Value, json};
    use std::sync::{Arc, Mutex};
    use tokio::net::TcpListener;
    use tower::ServiceExt;

    #[derive(Clone, Copy, Debug)]
    enum FakeMode {
        /// Valid non-streaming `chat.completion` JSON.
        ChatJson,
        /// Streaming: content chunk -> finish_reason:"stop" -> [DONE].
        ChatSse,
        /// Streaming: upstream error event + [DONE] (OpenRouter on provider failure).
        SseError,
        /// Streaming: content -> empty-choices usage chunk -> finish_reason -> [DONE].
        SseUsageThenStop,
        /// Non-streaming `chat.completion` with `finish_reason: "content_filter"`.
        ChatJsonContentFilter,
        /// Streaming: reasoning delta -> content -> finish_reason -> [DONE].
        SseReasoning,
    }

    fn json_response(v: &Value) -> Response<Body> {
        Response::builder()
            .header("content-type", "application/json")
            .body(Body::from(v.to_string()))
            .unwrap()
    }

    fn sse_response(text: &str) -> Response<Body> {
        Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::from(text.to_string()))
            .unwrap()
    }

    fn content_chunk(content: &str) -> String {
        json!({
            "id": "c1",
            "object": "chat.completion.chunk",
            "created": 0,
            "model": "m",
            "choices": [{"index": 0, "delta": {"role": "assistant", "content": content}, "finish_reason": null}]
        })
        .to_string()
    }

    fn stop_chunk() -> String {
        json!({
            "id": "c1",
            "object": "chat.completion.chunk",
            "created": 0,
            "model": "m",
            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
        })
        .to_string()
    }

    fn reasoning_chunk(reasoning: &str) -> String {
        json!({
            "id": "c1",
            "object": "chat.completion.chunk",
            "created": 0,
            "model": "m",
            "choices": [{"index": 0, "delta": {"reasoning": reasoning}, "finish_reason": null}]
        })
        .to_string()
    }

    /// Spawn the fake upstream and return its bound port.
    async fn spawn_fake_upstream(mode: FakeMode, log: Arc<Mutex<Vec<Value>>>) -> u16 {
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move |req: HttpRequest<Body>| {
                let log = log.clone();
                async move {
                    let body = to_bytes(req.into_body(), 8 * 1024 * 1024).await.unwrap();
                    let v: Value = serde_json::from_slice(&body).unwrap();
                    log.lock().unwrap().push(v.clone());

                    let streaming = v.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);
                    if !streaming {
                        let finish_reason = match mode {
                            FakeMode::ChatJsonContentFilter => "content_filter",
                            _ => "stop",
                        };
                        return json_response(&json!({
                            "id": "c1",
                            "object": "chat.completion",
                            "created": 0,
                            "model": "m",
                            "choices": [{
                                "index": 0,
                                "message": {"role": "assistant", "content": "hi"},
                                "finish_reason": finish_reason
                            }],
                            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                        }));
                    }

                    match mode {
                        FakeMode::ChatSse => sse_response(&format!(
                            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                            content_chunk("hi"),
                            stop_chunk()
                        )),
                        FakeMode::SseError => {
                            sse_response("data: {\"error\":{\"message\":\"upstream boom\",\"code\":502}}\n\ndata: [DONE]\n\n")
                        }
                        FakeMode::SseUsageThenStop => {
                            let usage_chunk = json!({
                                "id": "c1",
                                "object": "chat.completion.chunk",
                                "created": 0,
                                "model": "m",
                                "choices": [],
                                "usage": {"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8}
                            });
                            sse_response(&format!(
                                "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                                content_chunk("hi"),
                                usage_chunk,
                                stop_chunk()
                            ))
                        }
                        FakeMode::SseReasoning => sse_response(&format!(
                            "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                            reasoning_chunk("let me think"),
                            content_chunk("hi"),
                            stop_chunk()
                        )),
                        FakeMode::ChatJson | FakeMode::ChatJsonContentFilter => json_response(&json!({
                            "id": "c1",
                            "object": "chat.completion",
                            "created": 0,
                            "model": "m",
                            "choices": [{
                                "index": 0,
                                "message": {"role": "assistant", "content": "hi"},
                                "finish_reason": "stop"
                            }],
                            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                        })),
                    }
                }
            }),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        port
    }

    /// Build the klava router pointed at `base_url`.
    fn build_proxy(base_url: &str) -> Router {
        let config = Arc::new(Config {
            port: 0,
            verbose: false,
            enable_pii: false,
            enable_token_stats: false,
            active_provider: "fake".to_string(),
            providers: vec![ProviderConfig {
                name: "fake".to_string(),
                provider_type: ProviderType::OpenAICompatible,
                base_url: Some(base_url.to_string()),
                chat_completions_path: None,
                api_key: Some("test-key".to_string()),
                api_key_name: None,
                reasoning_model: Some("reasoning-model".to_string()),
                completion_model: Some("completion-model".to_string()),
            }],
        });
        let client = reqwest::Client::new();
        let cors = create_cors_layer();
        let chain = Arc::new(HookChain::new());
        create_app(config, client, cors, chain)
    }

    async fn send(router: &Router, method: &str, path: &str, body: Value) -> (u16, String) {
        let req = HttpRequest::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        let status = resp.status().as_u16();
        let bytes = to_bytes(resp.into_body(), 8 * 1024 * 1024).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    #[tokio::test]
    async fn chat_completions_non_streaming_passthrough() {
        let log = Arc::new(Mutex::new(vec![]));
        let port = spawn_fake_upstream(FakeMode::ChatJson, log.clone()).await;
        let router = build_proxy(&format!("http://127.0.0.1:{port}"));

        let (status, body) = send(
            &router,
            "POST",
            "/v1/chat/completions",
            json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
        assert_eq!(status, 200);
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["choices"][0]["message"]["content"], "hi");
        // The completion model override was applied before forwarding upstream.
        let sent = log.lock().unwrap();
        assert_eq!(sent[0]["model"], "completion-model");
    }

    #[tokio::test]
    async fn chat_completions_streaming_forwards_finish_reason_and_done() {
        let log = Arc::new(Mutex::new(vec![]));
        let port = spawn_fake_upstream(FakeMode::ChatSse, log.clone()).await;
        let router = build_proxy(&format!("http://127.0.0.1:{port}"));

        let (status, body) = send(
            &router,
            "POST",
            "/v1/chat/completions",
            json!({"model": "m", "stream": true, "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
        assert_eq!(status, 200);
        assert!(body.contains("hi"), "body: {body}");
        assert!(body.contains("\"finish_reason\":\"stop\""), "body: {body}");
        assert!(body.contains("[DONE]"), "body: {body}");
    }

    #[tokio::test]
    async fn streaming_upstream_error_event_is_forwarded() {
        // Regression: klava used to swallow `data: {"error":...}` events,
        // which made opencode report "OpenAI Chat stream ended without
        // finish_reason" instead of the real upstream error.
        let log = Arc::new(Mutex::new(vec![]));
        let port = spawn_fake_upstream(FakeMode::SseError, log.clone()).await;
        let router = build_proxy(&format!("http://127.0.0.1:{port}"));

        let (status, body) = send(
            &router,
            "POST",
            "/v1/chat/completions",
            json!({"model": "m", "stream": true, "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
        assert_eq!(status, 200);
        assert!(body.contains("\"error\""), "body: {body}");
        assert!(body.contains("upstream boom"), "body: {body}");
        assert!(body.contains("[DONE]"), "body: {body}");
    }

    #[tokio::test]
    async fn streaming_usage_chunk_with_empty_choices_is_filtered() {
        let log = Arc::new(Mutex::new(vec![]));
        let port = spawn_fake_upstream(FakeMode::SseUsageThenStop, log.clone()).await;
        let router = build_proxy(&format!("http://127.0.0.1:{port}"));

        let (status, body) = send(
            &router,
            "POST",
            "/v1/chat/completions",
            json!({"model": "m", "stream": true, "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
        assert_eq!(status, 200);
        assert!(body.contains("\"finish_reason\":\"stop\""), "body: {body}");
        assert!(body.contains("[DONE]"), "body: {body}");
        assert!(
            !body.contains("prompt_tokens"),
            "empty-choices usage chunk should be dropped: {body}"
        );
    }

    #[tokio::test]
    async fn reasoning_effort_selects_reasoning_model_upstream() {
        let log = Arc::new(Mutex::new(vec![]));
        let port = spawn_fake_upstream(FakeMode::ChatJson, log.clone()).await;
        let router = build_proxy(&format!("http://127.0.0.1:{port}"));

        let (status, _) = send(
            &router,
            "POST",
            "/v1/chat/completions",
            json!({"model": "m", "reasoning_effort": "high", "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
        assert_eq!(status, 200);
        let sent = log.lock().unwrap();
        assert_eq!(sent[0]["model"], "reasoning-model");
    }

    #[tokio::test]
    async fn anthropic_messages_non_streaming_transforms_to_anthropic() {
        let log = Arc::new(Mutex::new(vec![]));
        let port = spawn_fake_upstream(FakeMode::ChatJson, log.clone()).await;
        let router = build_proxy(&format!("http://127.0.0.1:{port}"));

        let (status, body) = send(
            &router,
            "POST",
            "/v1/messages",
            json!({"model": "claude-3", "max_tokens": 100, "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
        assert_eq!(status, 200);
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["type"], "message");
        assert_eq!(v["content"][0]["text"], "hi");
    }

    #[tokio::test]
    async fn anthropic_messages_streaming_emits_anthropic_sse() {
        let log = Arc::new(Mutex::new(vec![]));
        let port = spawn_fake_upstream(FakeMode::ChatSse, log.clone()).await;
        let router = build_proxy(&format!("http://127.0.0.1:{port}"));

        let (status, body) = send(
            &router,
            "POST",
            "/v1/messages",
            json!({"model": "claude-3", "max_tokens": 100, "stream": true, "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
        assert_eq!(status, 200);
        assert!(body.contains("event: message_start"), "body: {body}");
        assert!(body.contains("event: message_stop"), "body: {body}");
    }

    #[tokio::test]
    async fn responses_streaming_emits_responses_sse() {
        let log = Arc::new(Mutex::new(vec![]));
        let port = spawn_fake_upstream(FakeMode::ChatSse, log.clone()).await;
        let router = build_proxy(&format!("http://127.0.0.1:{port}"));

        let (status, body) = send(
            &router,
            "POST",
            "/v1/responses",
            json!({"model": "m", "input": "hi", "stream": true}),
        )
        .await;
        assert_eq!(status, 200);
        assert!(body.contains("response.completed"), "body: {body}");
    }

    #[tokio::test]
    async fn anthropic_adaptive_thinking_selects_reasoning_model_upstream() {
        // Newer Claude models request `thinking.type: "adaptive"`; it must pick
        // the configured reasoning model, just like `enabled`.
        let log = Arc::new(Mutex::new(vec![]));
        let port = spawn_fake_upstream(FakeMode::ChatJson, log.clone()).await;
        let router = build_proxy(&format!("http://127.0.0.1:{port}"));

        let (status, _) = send(
            &router,
            "POST",
            "/v1/messages",
            json!({
                "model": "claude-opus-5",
                "max_tokens": 100,
                "thinking": {"type": "adaptive"},
                "messages": [{"role": "user", "content": "hi"}]
            }),
        )
        .await;
        assert_eq!(status, 200);
        let sent = log.lock().unwrap();
        assert_eq!(sent[0]["model"], "reasoning-model");
    }

    #[tokio::test]
    async fn anthropic_forwards_tool_choice_and_metadata_upstream() {
        let log = Arc::new(Mutex::new(vec![]));
        let port = spawn_fake_upstream(FakeMode::ChatJson, log.clone()).await;
        let router = build_proxy(&format!("http://127.0.0.1:{port}"));

        let (status, _) = send(
            &router,
            "POST",
            "/v1/messages",
            json!({
                "model": "claude-opus-5",
                "max_tokens": 100,
                "messages": [{"role": "user", "content": "hi"}],
                "tools": [{
                    "name": "get_weather",
                    "description": "Look up the weather",
                    "input_schema": {"type": "object"}
                }],
                "tool_choice": {"type": "tool", "name": "get_weather", "disable_parallel_tool_use": true},
                "metadata": {"user_id": "u_123"}
            }),
        )
        .await;
        assert_eq!(status, 200);
        let sent = log.lock().unwrap();
        assert_eq!(
            sent[0]["tool_choice"],
            json!({"type": "function", "function": {"name": "get_weather"}})
        );
        assert_eq!(sent[0]["parallel_tool_calls"], json!(false));
        assert_eq!(sent[0]["metadata"]["user_id"], json!("u_123"));
    }

    #[tokio::test]
    async fn responses_instructions_forwarded_upstream() {
        // Regression: top-level `instructions` used to be dropped instead of
        // becoming the leading system message.
        let log = Arc::new(Mutex::new(vec![]));
        let port = spawn_fake_upstream(FakeMode::ChatJson, log.clone()).await;
        let router = build_proxy(&format!("http://127.0.0.1:{port}"));

        let (status, _) = send(
            &router,
            "POST",
            "/v1/responses",
            json!({
                "model": "m",
                "input": "hi",
                "instructions": "You are a helpful agent."
            }),
        )
        .await;
        assert_eq!(status, 200);
        let sent = log.lock().unwrap();
        assert_eq!(sent[0]["messages"][0]["role"], "system");
        assert_eq!(
            sent[0]["messages"][0]["content"],
            "You are a helpful agent."
        );
        assert_eq!(sent[0]["messages"][1]["role"], "user");
    }

    #[tokio::test]
    async fn anthropic_content_filter_maps_to_refusal_and_stop_details() {
        let log = Arc::new(Mutex::new(vec![]));
        let port = spawn_fake_upstream(FakeMode::ChatJsonContentFilter, log.clone()).await;
        let router = build_proxy(&format!("http://127.0.0.1:{port}"));

        let (status, body) = send(
            &router,
            "POST",
            "/v1/messages",
            json!({"model": "claude-opus-5", "max_tokens": 100, "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
        assert_eq!(status, 200);
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["stop_reason"], "refusal");
        assert_eq!(v["stop_details"]["type"], "refusal");
    }

    #[tokio::test]
    async fn responses_streaming_emits_reasoning_summary_part_events() {
        let log = Arc::new(Mutex::new(vec![]));
        let port = spawn_fake_upstream(FakeMode::SseReasoning, log.clone()).await;
        let router = build_proxy(&format!("http://127.0.0.1:{port}"));

        let (status, body) = send(
            &router,
            "POST",
            "/v1/responses",
            json!({"model": "m", "input": "hi", "stream": true}),
        )
        .await;
        assert_eq!(status, 200);
        assert!(
            body.contains("response.reasoning_summary_part.added"),
            "body: {body}"
        );
        assert!(
            body.contains("response.reasoning_summary_part.done"),
            "body: {body}"
        );
    }
}
