//! Qwen Auth Flow Example
//!
//! This example demonstrates how to use the Qwen OAuth authentication flow.
//!
//! Run with:
//! ```bash
//! cargo run --example qwen_auth_flow --features qwen-code
//! ```

use klava::qwen_auth::QwenAuth;
use tokio;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    println!("╔════════════════════════════════════════════════════════════╗");
    println!("║           Qwen Code OAuth Authentication Flow             ║");
    println!("║                  Example Application                        ║");
    println!("╚════════════════════════════════════════════════════════════╝");
    println!();

    // Create a new QwenAuth instance
    println!("Step: Initializing QwenAuth client...");
    let mut qwen_auth = QwenAuth::new();
    println!("✓ Client initialized");
    println!();

    // Check if already authenticated
    if qwen_auth.is_authenticated() {
        println!("ℹ️  Already authenticated. Checking credentials...");
        if let Some(creds) = qwen_auth.get_credentials() {
            println!("✓ Found stored credentials");
            println!(
                "  Access token: {}...{}",
                &creds.access_token[..10],
                &creds.access_token[creds.access_token.len() - 10..]
            );
            println!("  Expires at: {}", creds.expires_at);
            println!("  Resource URL: {}", creds.resource_url);
            println!("  Base URL: {}", qwen_auth.get_base_url());
            println!();

            // Test token refresh
            if qwen_auth.needs_refresh() {
                println!("⚠️  Token needs refresh, refreshing now...");
                qwen_auth.get_token().await?;
                println!("✓ Token refreshed successfully");
            } else {
                println!("✓ Token is still valid");
            }
        }
    } else {
        println!("ℹ️  Not authenticated. Starting authentication flow...");
        println!();

        // Start authentication flow
        println!("Step 2: Starting OAuth device flow...");
        println!("This will:");
        println!(" . Request a device code from Qwen");
        println!("  2. Open your browser (if possible)");
        println!(" . Poll for authorization");
        println!(" . Save credentials locally");
        println!();

        qwen_auth.authenticate().await?;

        println!();
        println!("✓ Authentication complete!");
        println!();

        // Show stored credentials
        if let Some(creds) = qwen_auth.get_credentials() {
            println!("Stored credentials:");
            println!(
                "  Access token: {}...{}",
                &creds.access_token[..10],
                &creds.access_token[creds.access_token.len() - 10..]
            );
            println!("  Expires at: {}", creds.expires_at);
            println!("  Resource URL: {}", creds.resource_url);
            println!("  Base URL: {}", qwen_auth.get_base_url());
        }
    }

    println!();
    println!("╔════════════════════════════════════════════════════════════╗");
    println!("║                    Authentication Complete                   ║");
    println!("╚════════════════════════════════════════════════════════════╝");
    println!();
    println!("Next steps:");
    println!("  • Use klava proxy server: `cargo run --bin klava --features qwen-code`");
    println!("  • Make API requests to Qwen Code models");
    println!("  • Token will auto-refresh when needed");
    println!();

    // Demonstrate token retrieval
    let token = qwen_auth.get_token().await?;
    println!(
        "Current access token: {}...{}",
        &token[..10],
        &token[token.len() - 10..]
    );
    println!();

    Ok(())
}
