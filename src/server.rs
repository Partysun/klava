use crate::{config::Config, hooks::hooks::HookChain};
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

pub async fn run_server(
    config: Arc<Config>,
    client: reqwest::Client,
    hook_chain: Arc<HookChain>,
    log_mode: LogMode,
) -> anyhow::Result<()> {
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

    let addr = format!("0.0.0.0:{}", config.port);
    let cors = create_cors_layer();
    let app = create_app(config, client, cors, hook_chain);
    let listener = tokio::net::TcpListener::bind(&addr).await?;

    tracing::info!("Listening on {}", addr);
    tracing::info!("Proxy is live!");

    axum::serve(listener, app).await?;

    Ok(())
}
