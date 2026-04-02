//! Example demonstrating how to use the check_for_update function
//!
//! To run this example:
//!
//! With telemetry feature:
//! cargo run --example check_update --features telemetry
//!
//! Without telemetry feature:
//! cargo run --example check_update

use klava::telemetry::check_for_update;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Use the current crate version as defined in Cargo.toml
    let current_version = env!("CARGO_PKG_VERSION");

    println!("Current version: {}", current_version);
    println!("Checking for updates...");

    match check_for_update(current_version).await {
        Ok(true) => {
            println!("🔄 A new version is available!");
            println!("Please update using your package manager.");
        }
        Ok(false) => {
            println!("✅ You are using the latest version.");
        }
        Err(e) => {
            println!("⚠️  Could not check for updates: {}", e);
            println!("This might be due to network connectivity issues.");
        }
    }

    Ok(())
}
