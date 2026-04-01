// Example: OpenAI-only proxy server for testing the actual proxy handler
//
// This creates a minimal axum server using the actual proxy_openai handler
// from the klava library.
// Run with: cargo run --example test_openai_proxy
//
// Configuration is loaded from:
//   - ~/.config/klava/config.toml
//   - Environment variables (KLAVA_BASE_URL, KLAVA_API_KEY, etc.)

use axum::{
    Extension, Router,
    routing::{get, post},
};
use reqwest::Client;
use std::sync::Arc;
use tokio::net::TcpListener;
use tower_http::trace::TraceLayer;

use klava::config::Config;
use klava::hooks::default_chain;
use klava::proxy::proxy_openai;
use klava::server::{LogMode, configure_logging, create_cors_layer};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Configure logging to stdout for debugging
    configure_logging(true, LogMode::STDOUT);

    // Load config from file and env vars (same as main.rs)
    let mut config = Config::from_config_and_env()?;
    config.verbose = true;

    let config = Arc::new(config);
    let hook_chain = Arc::new(default_chain());
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()?;

    // Build router with only the OpenAI proxy route
    let app = Router::new()
        .route("/v1/chat/completions", post(proxy_openai))
        .route("/health", get(|| async { "OK" }))
        .layer(Extension(config.clone()))
        .layer(Extension(hook_chain))
        .layer(Extension(client))
        .layer(TraceLayer::new_for_http())
        .layer(create_cors_layer());

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let port = addr.port();

    println!(
        "OpenAI-only proxy server running on http://127.0.0.1:{}",
        port
    );
    println!(
        "Base URL: {}",
        config.base_url.as_ref().unwrap_or(&"N/A".to_string())
    );
    if config.api_key.is_some() {
        println!("API Key: configured");
    } else {
        println!("API Key: not set");
    }

    println!("\nTest with curl:");
    println!(
        "curl -X POST http://127.0.0.1:{}/v1/chat/completions \\",
        port
    );
    println!("  -H 'Content-Type: application/json' \\");
    println!("  -d '{{");
    println!("    \"model\": \"gpto\",");
    println!("    \"messages\": [{{\"role\": \"user\", \"content\": \"Hello!\"}}]");
    println!("  }}'");

    println!("\nTest streaming thinking request (reasoning tokens):");
    println!(
        "curl -X POST http://127.0.0.1:{}/v1/chat/completions \\",
        port
    );
    println!("  -H 'Content-Type: application/json' \\");
    println!("  -d '{{");
    println!("    \"model\": \"gpto\",");
    println!("    \"messages\": [{{\"role\": \"user\", \"content\": \"Explain recursion\"}}],");
    println!("    \"stream\": true,");
    println!("    \"max_tokens\": 0,");
    println!("    \"reasoning_effort\": \"medium\"");
    println!("  }}'");

    axum::serve(listener, app).await?;
    Ok(())
}
