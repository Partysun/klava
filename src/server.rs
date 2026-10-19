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
    let extensions = Arc::new(crate::extensions::Extensions::from_config(&config));
    if !extensions.is_empty() {
        tracing::info!("Extensions active: {}", extensions.names().join(", "));
    }

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
        .layer(Extension(extensions))
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
        // `port + 1` may itself be taken by a parallel test's ephemeral socket,
        // so the contract is "the next port that is actually free", not
        // "exactly one more".
        assert_ne!(port, occupied_port);
        assert!(port > occupied_port);
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
        /// Web-search loop: first call returns a `web_search` tool call; the
        /// search request (carrying `plugins`) returns annotated content; the
        /// follow-up call (carrying a `tool` message) returns the final answer.
        SearchLoop,
        /// Web-search loop where the first turn also asks for a client-owned
        /// tool; klava must run its search but hand the other call back.
        SearchMixed,
        /// Web-search loop served over SSE: round 1 streams a `web_search`
        /// tool call (role delta + tool-call deltas + finish), the search
        /// request (non-streaming, carrying `plugins`) returns annotated
        /// content, and round 2 streams the final answer.
        SearchLoopStreaming,
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

                    // Web-search loop: distinguish the three request kinds by
                    // shape — search request (has `plugins`), follow-up (has a
                    // `tool` message), or the first call (tool call request).
                    if matches!(mode, FakeMode::SearchLoop | FakeMode::SearchMixed) {
                        let has_plugins = v.get("plugins").is_some();
                        let has_tool_result = v["messages"]
                            .as_array()
                            .is_some_and(|m| m.iter().any(|msg| msg.get("role").and_then(|r| r.as_str()) == Some("tool")));

                        if has_plugins {
                            return json_response(&json!({
                                "id": "search-1",
                                "object": "chat.completion",
                                "created": 0,
                                "model": "m",
                                "choices": [{
                                    "index": 0,
                                    "message": {
                                        "role": "assistant",
                                        "content": "Rust 1.90 shipped today.",
                                        "annotations": [
                                            {"type": "url_citation", "url_citation": {"url": "https://blog.rust-lang.org", "title": "Rust Blog"}}
                                        ]
                                    },
                                    "finish_reason": "stop"
                                }],
                                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                            }));
                        }

                        if has_tool_result {
                            return json_response(&json!({
                                "id": "final-1",
                                "object": "chat.completion",
                                "created": 0,
                                "model": "m",
                                "choices": [{
                                    "index": 0,
                                    "message": {"role": "assistant", "content": "Rust 1.90 was released today."},
                                    "finish_reason": "stop"
                                }],
                                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                            }));
                        }

                        let tool_calls = if matches!(mode, FakeMode::SearchMixed) {
                            json!([
                                {
                                    "id": "call_search_1",
                                    "type": "function",
                                    "function": {"name": "web_search", "arguments": "{\"query\":\"latest rust release\"}"}
                                },
                                {
                                    "id": "call_bash_1",
                                    "type": "function",
                                    "function": {"name": "bash", "arguments": "{\"cmd\":\"rustc --version\"}"}
                                }
                            ])
                        } else {
                            json!([{
                                "id": "call_search_1",
                                "type": "function",
                                "function": {"name": "web_search", "arguments": "{\"query\":\"latest rust release\"}"}
                            }])
                        };

                        return json_response(&json!({
                            "id": "call-1",
                            "object": "chat.completion",
                            "created": 0,
                            "model": "m",
                            "choices": [{
                                "index": 0,
                                "message": {
                                    "role": "assistant",
                                    "content": null,
                                    "tool_calls": tool_calls
                                },
                                "finish_reason": "tool_calls"
                            }],
                            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                        }));
                    }

                    if matches!(mode, FakeMode::SearchLoopStreaming) {
                        let has_plugins = v.get("plugins").is_some();
                        let has_tool_result = v["messages"]
                            .as_array()
                            .is_some_and(|m| m.iter().any(|msg| msg.get("role").and_then(|r| r.as_str()) == Some("tool")));

                        if has_plugins {
                            return json_response(&json!({
                                "id": "search-1",
                                "object": "chat.completion",
                                "created": 0,
                                "model": "m",
                                "choices": [{
                                    "index": 0,
                                    "message": {
                                        "role": "assistant",
                                        "content": "Rust 1.90 shipped today.",
                                        "annotations": [
                                            {"type": "url_citation", "url_citation": {"url": "https://blog.rust-lang.org", "title": "Rust Blog"}}
                                        ]
                                    },
                                    "finish_reason": "stop"
                                }],
                                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                            }));
                        }

                        if has_tool_result {
                            return sse_response(&format!(
                                "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                                content_chunk("Rust 1.90 was released today."),
                                stop_chunk()
                            ));
                        }

                        // Round 1: stream a web_search tool call.
                        let role_chunk = json!({
                            "id": "c1",
                            "object": "chat.completion.chunk",
                            "created": 0,
                            "model": "m",
                            "choices": [{"index": 0, "delta": {"role": "assistant"}, "finish_reason": null}]
                        });
                        let call_start = json!({
                            "id": "c1",
                            "object": "chat.completion.chunk",
                            "created": 0,
                            "model": "m",
                            "choices": [{"index": 0, "delta": {"tool_calls": [
                                {"index": 0, "id": "call_search_1", "type": "function", "function": {"name": "web_search", "arguments": ""}}
                            ]}, "finish_reason": null}]
                        });
                        let call_args = json!({
                            "id": "c1",
                            "object": "chat.completion.chunk",
                            "created": 0,
                            "model": "m",
                            "choices": [{"index": 0, "delta": {"tool_calls": [
                                {"index": 0, "function": {"arguments": "{\"query\":\"latest rust release\"}"}}
                            ]}, "finish_reason": null}]
                        });
                        let finish_chunk = json!({
                            "id": "c1",
                            "object": "chat.completion.chunk",
                            "created": 0,
                            "model": "m",
                            "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]
                        });
                        return sse_response(&format!(
                            "data: {role_chunk}\n\ndata: {call_start}\n\ndata: {call_args}\n\ndata: {finish_chunk}\n\ndata: [DONE]\n\n"
                        ));
                    }

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
                        FakeMode::ChatJson
                        | FakeMode::ChatJsonContentFilter
                        | FakeMode::SearchLoop
                        | FakeMode::SearchMixed
                        | FakeMode::SearchLoopStreaming => json_response(&json!({
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
        build_proxy_opts(base_url, false)
    }

    /// Build the klava router with web search toggled. The fake provider is
    /// typed as klava (with an explicit api key) so the web-search gate
    /// passes in tests; the base URL points at the fake upstream. Decision
    /// tuning is always disabled here so tests never touch the network.
    fn build_proxy_opts(base_url: &str, enable_websearch: bool) -> Router {
        #[cfg(feature = "klava")]
        let provider_type = ProviderType::Klava;
        #[cfg(not(feature = "klava"))]
        let provider_type = ProviderType::OpenAICompatible;

        let config = Arc::new(Config {
            port: 0,
            verbose: false,
            enable_pii: false,
            enable_token_stats: false,
            active_provider: "fake".to_string(),
            providers: vec![ProviderConfig {
                name: "fake".to_string(),
                provider_type,
                base_url: Some(base_url.to_string()),
                chat_completions_path: None,
                api_key: Some("test-key".to_string()),
                api_key_name: None,
                reasoning_model: Some("reasoning-model".to_string()),
                completion_model: Some("completion-model".to_string()),
            }],
            enable_websearch,
            search_decide: false,
            ..Config::default()
        });
        let client = reqwest::Client::new();
        let cors = create_cors_layer();
        let chain = Arc::new(HookChain::new());
        create_app(config, client, cors, chain)
    }

    /// Like [`build_proxy_opts`], but with the legacy PII flag kept on so the
    /// installed PII extension actually runs.
    fn build_proxy_opts_enable_pii(base_url: &str) -> Router {
        let mut config = Config {
            port: 0,
            verbose: false,
            enable_pii: true,
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
            ..Config::default()
        };
        config
            .extensions
            .settings
            .insert("klava-pii-guardrail".to_string(), json!({"enabled": true}));
        let config = Arc::new(config);
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

    /// An extension installed as a real executable, exercised through the proxy.
    ///
    /// This is the integration the whole design exists for: klava is not rebuilt,
    /// and the extension is a separate process found by scanning a directory.
    /// The fixture is a shell script, proving any language works.
    #[cfg(unix)]
    /// The real PII extension, installed as an executable, must redact an
    /// email before the request reaches upstream. This is the production
    /// security property end to end.
    #[cfg(unix)]
    #[tokio::test]
    async fn installed_pii_extension_redacts_before_upstream() {
        use std::os::unix::fs::PermissionsExt;

        let exe = std::env::var("KLAVA_PII_EXT").unwrap_or_default();
        if exe.is_empty() {
            eprintln!("skipping: set KLAVA_PII_EXT to the built klava-pii-guardrail binary");
            return;
        }

        let dir = std::env::temp_dir().join(format!("klava-pii-proc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let script = dir.join("klava-pii-guardrail");
        std::fs::copy(&exe, &script).unwrap();
        for sfx in ["toml", "source.toml"] {
            let _ = std::fs::remove_file(dir.join(format!("klava-pii-guardrail.{sfx}")));
        }
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();
        std::fs::write(dir.join("klava-pii-guardrail.toml"), "enabled = true\n").unwrap();

        // SAFETY: only this test touches the variable, and it restores it below.
        unsafe { std::env::set_var("KLAVA_EXTENSIONS_DIR", &dir) };

        let log = Arc::new(Mutex::new(vec![]));
        let port = spawn_fake_upstream(FakeMode::ChatJson, log.clone()).await;
        let router = build_proxy_opts_enable_pii(&format!("http://127.0.0.1:{port}"));

        let (status, _body) = send(
            &router,
            "POST",
            "/v1/chat/completions",
            json!({
                "model": "m",
                "messages": [{"role": "user", "content": "contact alice@example.com now"}]
            }),
        )
        .await;
        assert_eq!(status, 200);

        let sent = log.lock().unwrap();
        let content = sent[0]["messages"][0]["content"]
            .as_str()
            .unwrap_or_default();
        assert!(
            content.contains("<EMAIL>"),
            "PII extension did not redact; upstream saw: {content}"
        );
        assert!(!content.contains("alice@"), "email leaked: {content}");

        unsafe { std::env::remove_var("KLAVA_EXTENSIONS_DIR") };
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_installed_extension_process_rewrites_the_request() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("klava-ext-proc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let script = dir.join("marker");
        std::fs::write(
            &script,
            r#"#!/usr/bin/env python3
import json, sys
request = json.load(sys.stdin)
payload = request["payload"]
for message in reversed(payload.get("messages", [])):
    if message.get("role") == "user" and isinstance(message.get("content"), str):
        message["content"] += " [marked]"
        break
json.dump({"decision": "replace", "payload": payload}, sys.stdout)
"#,
        )
        .unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();
        std::fs::write(dir.join("marker.toml"), "enabled = true\n").unwrap();

        // SAFETY: only this test touches the variable, and it restores it below.
        unsafe { std::env::set_var("KLAVA_EXTENSIONS_DIR", &dir) };

        let log = Arc::new(Mutex::new(vec![]));
        let port = spawn_fake_upstream(FakeMode::ChatJson, log.clone()).await;
        let router = build_proxy(&format!("http://127.0.0.1:{port}"));

        let (status, _body) = send(
            &router,
            "POST",
            "/v1/chat/completions",
            json!({"model": "m", "messages": [{"role": "user", "content": "hello"}]}),
        )
        .await;
        assert_eq!(status, 200);

        let sent = log.lock().unwrap();
        let content = sent[0]["messages"][0]["content"]
            .as_str()
            .unwrap_or_default();
        assert!(
            content.contains("[marked]"),
            "extension process did not run; upstream saw: {content}"
        );

        unsafe { std::env::remove_var("KLAVA_EXTENSIONS_DIR") };
        let _ = std::fs::remove_dir_all(&dir);
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
    async fn anthropic_server_tools_do_not_fail_the_request() {
        // Regression: Claude Code advertises server-side tools such as
        // `web_search_20250305` (identified by `type`, with no `input_schema`)
        // next to its own function tools. klava used to 400 the whole request
        // with "JSON error: missing field `input_schema`". The request must now
        // succeed, and only the client function tool may reach upstream.
        let log = Arc::new(Mutex::new(vec![]));
        let port = spawn_fake_upstream(FakeMode::ChatJson, log.clone()).await;
        let router = build_proxy(&format!("http://127.0.0.1:{port}"));

        let (status, body) = send(
            &router,
            "POST",
            "/v1/messages",
            json!({
                "model": "claude-opus-5",
                "max_tokens": 100,
                "messages": [{"role": "user", "content": "Who is the CEO of Instinct AI?"}],
                "tools": [
                    {"type": "web_search_20250305", "name": "web_search", "max_uses": 8},
                    {
                        "name": "get_weather",
                        "description": "Look up the weather",
                        "input_schema": {"type": "object"}
                    }
                ]
            }),
        )
        .await;
        assert_eq!(status, 200, "body: {body}");

        let sent = log.lock().unwrap();
        let tools = sent[0]["tools"].as_array().expect("client tool forwarded");
        assert_eq!(tools.len(), 1, "{tools:#?}");
        assert_eq!(tools[0]["function"]["name"], json!("get_weather"));
    }

    #[tokio::test]
    async fn anthropic_native_web_search_sub_request_returns_native_blocks() {
        // Claude Code's built-in WebSearch tool does not call a model: it
        // sends a native sub-request carrying the `web_search_20250305` server
        // tool plus a forced `tool_choice`. klava must answer with native
        // `server_tool_use` + `web_search_tool_result` blocks (what Claude
        // counts as "Did N searches"), not plain text.
        let log = Arc::new(Mutex::new(vec![]));
        let port = spawn_fake_upstream(FakeMode::SearchLoop, log.clone()).await;
        let router = build_proxy_opts(&format!("http://127.0.0.1:{port}"), true);

        let (status, body) = send(
            &router,
            "POST",
            "/v1/messages",
            json!({
                "model": "claude-opus-5",
                "max_tokens": 4096,
                "messages": [{
                    "role": "user",
                    "content": "Perform a web search for the query: latest rust release"
                }],
                "tools": [{"type": "web_search_20250305", "name": "web_search", "max_uses": 8}],
                "tool_choice": {"type": "tool", "name": "web_search"}
            }),
        )
        .await;
        assert_eq!(status, 200, "body: {body}");

        let body: Value = serde_json::from_str(&body).unwrap();
        let content = body["content"].as_array().expect("content blocks");
        assert_eq!(content.len(), 3, "{content:#?}");

        assert_eq!(content[0]["type"], json!("server_tool_use"));
        assert_eq!(content[0]["name"], json!("web_search"));
        assert_eq!(content[0]["input"]["query"], json!("latest rust release"));

        assert_eq!(content[1]["type"], json!("web_search_tool_result"));
        assert_eq!(content[1]["tool_use_id"], content[0]["id"]);
        assert_eq!(content[1]["content"][0]["type"], json!("web_search_result"));
        assert_eq!(content[1]["content"][0]["title"], json!("Rust Blog"));
        assert_eq!(
            content[1]["content"][0]["url"],
            json!("https://blog.rust-lang.org")
        );

        assert_eq!(content[2]["type"], json!("text"));
        assert!(content[2]["text"].as_str().is_some());

        assert_eq!(body["stop_reason"], json!("end_turn"));
        assert_eq!(
            body["usage"]["server_tool_use"]["web_search_requests"],
            json!(1)
        );
    }

    #[tokio::test]
    async fn anthropic_native_web_search_sub_request_streams_native_blocks() {
        // The sub-request can also be sent with `stream: true`; the native
        // blocks must be delivered as Anthropic SSE events.
        let log = Arc::new(Mutex::new(vec![]));
        let port = spawn_fake_upstream(FakeMode::SearchLoop, log.clone()).await;
        let router = build_proxy_opts(&format!("http://127.0.0.1:{port}"), true);

        let (status, body) = send(
            &router,
            "POST",
            "/v1/messages",
            json!({
                "model": "claude-opus-5",
                "max_tokens": 4096,
                "stream": true,
                "messages": [{
                    "role": "user",
                    "content": "Perform a web search for the query: latest rust release"
                }],
                "tools": [{"type": "web_search_20250305", "name": "web_search", "max_uses": 8}],
                "tool_choice": {"type": "tool", "name": "web_search"}
            }),
        )
        .await;
        assert_eq!(status, 200, "body: {body}");

        assert!(body.contains("\"type\":\"server_tool_use\""), "{body}");
        assert!(body.contains("\"type\":\"input_json_delta\""), "{body}");
        assert!(
            body.contains("\"type\":\"web_search_tool_result\""),
            "{body}"
        );
        assert!(body.contains("\"type\":\"web_search_result\""), "{body}");
        assert!(body.contains("\"type\":\"text_delta\""), "{body}");
        assert!(body.contains("\"type\":\"message_stop\""), "{body}");
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

    #[tokio::test]
    async fn websearch_loop_runs_search_and_hides_the_tool_call() {
        let log = Arc::new(Mutex::new(vec![]));
        let port = spawn_fake_upstream(FakeMode::SearchLoop, log.clone()).await;
        let router = build_proxy_opts(&format!("http://127.0.0.1:{port}"), true);

        let (status, body) = send(
            &router,
            "POST",
            "/v1/chat/completions",
            json!({"model": "m", "messages": [{"role": "user", "content": "latest rust release?"}]}),
        )
        .await;
        assert_eq!(status, 200);
        let v: Value = serde_json::from_str(&body).unwrap();
        // The client sees only the final answer — never the intercepted call.
        assert_eq!(
            v["choices"][0]["message"]["content"],
            "Rust 1.90 was released today."
        );
        assert!(v["choices"][0]["message"].get("tool_calls").is_none());

        let sent = log.lock().unwrap();
        assert_eq!(sent.len(), 3, "call -> search -> final: {sent:#?}");

        // The search request carried the search plugin + context option.
        let search = &sent[1];
        assert_eq!(search["plugins"][0]["id"], json!("web"));
        assert_eq!(
            search["messages"][0]["content"],
            json!("latest rust release")
        );
        assert_eq!(
            search["web_search_options"]["search_context_size"],
            json!("medium")
        );

        // The follow-up carried the tool result and kept the search tool def.
        let follow = &sent[2];
        assert!(
            follow["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["role"] == json!("tool")),
            "follow-up must carry the tool result: {follow:#?}"
        );
        assert!(
            follow["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["content"]
                    .as_str()
                    .is_some_and(|c| c.contains("blog.rust-lang.org"))),
            "tool result must include the cited source: {follow:#?}"
        );
        let tools = follow["tools"].as_array().unwrap();
        assert!(
            tools
                .iter()
                .any(|t| t["function"]["name"] == json!("web_search"))
        );
    }

    #[tokio::test]
    async fn websearch_mixed_turn_keeps_client_tool_call() {
        let log = Arc::new(Mutex::new(vec![]));
        let port = spawn_fake_upstream(FakeMode::SearchMixed, log.clone()).await;
        let router = build_proxy_opts(&format!("http://127.0.0.1:{port}"), true);

        let (status, body) = send(
            &router,
            "POST",
            "/v1/chat/completions",
            json!({
                "model": "m",
                "tools": [{"type": "function", "function": {"name": "bash", "parameters": {"type": "object"}}}],
                "messages": [{"role": "user", "content": "latest rust release?"}]
            }),
        )
        .await;
        assert_eq!(status, 200);
        let v: Value = serde_json::from_str(&body).unwrap();
        // The client's own tool call survives; klava's search call does not.
        let calls = v["choices"][0]["message"]["tool_calls"].as_array().unwrap();
        assert_eq!(calls.len(), 1, "expected only the client tool: {v:#?}");
        assert_eq!(calls[0]["function"]["name"], "bash");

        let sent = log.lock().unwrap();
        // call -> search -> return-to-client (no follow-up re-run).
        assert_eq!(sent.len(), 2, "{sent:#?}");
        assert_eq!(sent[1]["plugins"][0]["id"], json!("web"));
    }

    #[tokio::test]
    async fn websearch_streaming_loop_streams_answer_without_leaking_tool() {
        let log = Arc::new(Mutex::new(vec![]));
        let port = spawn_fake_upstream(FakeMode::SearchLoopStreaming, log.clone()).await;
        let router = build_proxy_opts(&format!("http://127.0.0.1:{port}"), true);

        let (status, body) = send(
            &router,
            "POST",
            "/v1/chat/completions",
            json!({"model": "m", "stream": true, "messages": [{"role": "user", "content": "latest rust release?"}]}),
        )
        .await;
        assert_eq!(status, 200);
        assert!(
            body.contains("Rust 1.90 was released today."),
            "body: {body}"
        );
        assert!(body.contains("\"finish_reason\":\"stop\""), "body: {body}");
        assert!(body.contains("[DONE]"), "body: {body}");
        // No web_search tool call may leak to the client.
        assert!(!body.contains("web_search"), "body: {body}");
        assert!(!body.contains("call_search_1"), "body: {body}");

        // Three upstream requests: streaming round 1 (tool call), non-streaming
        // search, streaming round 2 (answer).
        let sent = log.lock().unwrap();
        assert_eq!(sent.len(), 3, "{sent:#?}");
        assert_eq!(sent[0]["stream"], json!(true));
        assert_eq!(sent[1]["plugins"][0]["id"], json!("web"));
        assert_eq!(sent[2]["stream"], json!(true));
        // Round 2 carried the tool result with the cited source.
        assert!(
            sent[2]["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["content"]
                    .as_str()
                    .is_some_and(|c| c.contains("blog.rust-lang.org"))),
            "round 2 must carry search result: {:#?}",
            sent[2]
        );
    }

    #[tokio::test]
    async fn websearch_anthropic_client_gets_anthropic_message() {
        let log = Arc::new(Mutex::new(vec![]));
        let port = spawn_fake_upstream(FakeMode::SearchLoop, log.clone()).await;
        let router = build_proxy_opts(&format!("http://127.0.0.1:{port}"), true);

        let (status, body) = send(
            &router,
            "POST",
            "/v1/messages",
            json!({"model": "claude-3", "max_tokens": 100, "messages": [{"role": "user", "content": "latest rust release?"}]}),
        )
        .await;
        assert_eq!(status, 200);
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["type"], "message");
        assert_eq!(v["content"][0]["text"], "Rust 1.90 was released today.");
        assert_eq!(v["stop_reason"], "end_turn");
    }

    #[tokio::test]
    async fn websearch_anthropic_streaming_gets_anthropic_sse() {
        let log = Arc::new(Mutex::new(vec![]));
        let port = spawn_fake_upstream(FakeMode::SearchLoopStreaming, log.clone()).await;
        let router = build_proxy_opts(&format!("http://127.0.0.1:{port}"), true);

        let (status, body) = send(
            &router,
            "POST",
            "/v1/messages",
            json!({"model": "claude-3", "max_tokens": 100, "stream": true, "messages": [{"role": "user", "content": "latest rust release?"}]}),
        )
        .await;
        assert_eq!(status, 200);
        // Exactly one message_start; the final answer arrives as text deltas;
        // no tool_use content block may leak.
        assert_eq!(
            body.matches("event: message_start").count(),
            1,
            "body: {body}"
        );
        assert!(
            body.contains("Rust 1.90 was released today."),
            "body: {body}"
        );
        assert!(body.contains("event: message_stop"), "body: {body}");
        assert!(!body.contains("tool_use"), "body: {body}");
    }

    #[tokio::test]
    async fn websearch_off_leaves_requests_untouched() {
        let log = Arc::new(Mutex::new(vec![]));
        let port = spawn_fake_upstream(FakeMode::ChatJson, log.clone()).await;
        let router = build_proxy_opts(&format!("http://127.0.0.1:{port}"), false);

        let (status, _) = send(
            &router,
            "POST",
            "/v1/chat/completions",
            json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
        assert_eq!(status, 200);
        let sent = log.lock().unwrap();
        assert_eq!(sent.len(), 1);
        // No search tool was injected.
        assert!(sent[0].get("tools").is_none(), "{:#?}", sent[0]);
    }
}
