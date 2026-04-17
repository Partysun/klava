//! Qwen Code provider with OAuth authentication

use crate::config::Config;
use crate::error::{Error, Result};
use crate::qwen_auth::QwenAuth;
use inquire::Confirm;

/// Available and supported Qwen models for free quota
pub const SUPPORTED_MODELS: &[&str] = &[
    "qwen3-coder-plus", // Recommended
    "coder-model",      // Maps to qwen3.5-plus
];

pub async fn setup(_config: &mut Config) -> Result<()> {
    let qwen_auth = QwenAuth::new();

    println!("qwen-code provider uses OAuth authentication.");
    println!("No API key, base URL, or models needed.");
    println!();
    println!("Supported models: {}", SUPPORTED_MODELS.join(", "));
    println!();

    // Check if already authenticated
    if qwen_auth.is_authenticated() {
        println!("✓ Already authenticated with Qwen Code");
        println!();
        return Ok(());
    }

    // Ask to login now
    let login_now = Confirm::new("Login to Qwen Code now?")
        .with_default(true)
        .prompt()
        .map_err(|e| Error::Internal(format!("Failed to prompt for login: {}", e)))?;

    if login_now {
        println!();
        let mut auth = QwenAuth::new();
        auth.authenticate()
            .await
            .map_err(|e| Error::Provider(format!("Qwen authentication failed: {}", e)))?;
        println!();
    } else {
        println!();
        println!("🔑 Login later with: klava providers qwen login");
        println!();
    }
    Ok(())
}
