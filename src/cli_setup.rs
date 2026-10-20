//! Loading the config for a CLI command.
//!
//! Setup is the same thing as onboarding a provider: `klava providers add`
//! runs the wizard in [`klava::providers::add_provider_interactive`], and this
//! module invokes it on first run when a command needs a working config and
//! finds none.
//!
//! Only commands that actually talk to a model (launch/up) start the wizard
//! automatically. Configuration commands (`providers`, `config`, `check`)
//! handle a missing setup themselves, so `klava providers add openrouter` is
//! never pre-empted by a provider picker, and nothing prompts when stdin is
//! not a terminal.

use anyhow::{Result, anyhow};
use klava::config::Config;
use klava::error::Error;
use std::io::IsTerminal;

/// Commands for which a missing setup should open the wizard.
const AUTO_SETUP_COMMANDS: [&str; 2] = ["launch", "up"];

/// Whether the process should offer to run the wizard for `command`.
pub fn auto_setup_for(command: &str) -> bool {
    AUTO_SETUP_COMMANDS.contains(&command)
}

/// Load the config, running the provider wizard when `allow_setup` is set and
/// the config is not usable yet.
pub async fn build_config_interactive_with_context(
    allow_setup: bool,
    context: Option<&str>,
) -> Result<Config> {
    let mut persistent = match Config::load() {
        Ok(config) => config,
        Err(_) => {
            // If config doesn't exist, create a default one first.
            Config::ensure_exists(false).map_err(|e| anyhow!("Failed to create config: {}", e))?;
            Config::load().map_err(|e| anyhow!("Failed to load config after creation: {}", e))?
        }
    };

    if let Err(e) = persistent.validate_complete() {
        match e {
            Error::NoModelsConfigured
            | Error::MissingBaseUrl
            | Error::MissingApiKey(_)
            | Error::MissingModel(_)
            | Error::AmbiguousModel(_, _) => {
                if !allow_setup {
                    return Ok(persistent);
                }
                if let Some(context) = context {
                    println!("\n{}", context);
                }
                if !std::io::stdin().is_terminal() {
                    // Never hang or render a picker into a pipe.
                    eprintln!(
                        "klava is not configured yet: run `klava providers add` in a terminal."
                    );
                    return Err(anyhow!("{e}"));
                }
                klava::providers::add_provider_interactive(&mut persistent, None)
                    .await
                    .map_err(|e| anyhow!("{e}"))?;
                persistent.validate_complete()?;
            }
            other => return Err(anyhow!("Configuration error: {}", other)),
        }
    }

    Ok(persistent)
}

/// Force run interactive setup (for `klava config --setup`).
pub async fn run_interactive_setup() -> Result<Config> {
    if !std::io::stdin().is_terminal() {
        return Err(anyhow!(
            "setup needs an interactive terminal; edit the config file instead"
        ));
    }
    let mut persistent = Config::load()?;
    klava::providers::add_provider_interactive(&mut persistent, None)
        .await
        .map_err(|e| anyhow!("{e}"))?;
    Ok(persistent)
}
