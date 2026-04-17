mod cli;
mod cli_setup;
mod daemon;

use clap::Parser;
use cli::{Cli, Command};
use colored::Colorize;
use daemon::{check_status, prepare_daemon, stop_daemon};
use klava::agents::CodeAgents;
use klava::config;
use klava::error::Error;
use klava::hooks::default_chain;
use klava::server;
use klava::telemetry::{check_for_update, send_install_event};
use reqwest::Client;
use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let current_version = env!("CARGO_PKG_VERSION");

    // Send telemetry on first run (before config is loaded)
    // This will only send if config file doesn't exist yet
    let _ = send_install_event(current_version).await;

    // Check for updates
    if let Ok(update_available) = check_for_update(current_version).await
        && update_available
    {
        eprintln!(
            "{}",
            "A new version of klava is available! Please update using your package manager.\n"
                .truecolor(255, 151, 0)
        );
    }

    let cli = Cli::parse();

    let mut config = cli_setup::build_config_interactive().await?;

    let hook_chain = Arc::new(default_chain());

    match cli.command {
        Command::Launch {
            agent,
            config: config_flag,
            provider,
            port,
        } => {
            use inquire::Select;

            // Create a mutable config to potentially update
            let mut current_config = config.clone();

            // Handle port override if provided
            if let Some(cli_port) = port {
                current_config.port = cli_port;
            }

            // Handle provider switch if provided
            if let Some(provider_name) = provider {
                // Find matching provider
                let selected_provider = current_config
                    .providers
                    .iter()
                    .find(|p| p.name == provider_name)
                    .ok_or_else(|| {
                        Error::Internal(format!(
                            "Invalid provider: '{}'. Available providers: {}",
                            provider_name,
                            current_config
                                .providers
                                .iter()
                                .map(|p| p.name.clone())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ))
                    })?;

                // Update the config with the new active provider
                current_config.active_provider = selected_provider.name.to_string();

                println!("Switched to provider: {}", provider_name);

                // Handle provider-specific authentication if needed
                match selected_provider.provider_type {
                    klava::providers::Type::QwenCode => {
                        handle_qwen_authentication().await?;
                    }
                    klava::providers::Type::OpenAICompatible => {
                        // Validate OpenAI-compatible provider config
                        selected_provider.validate_config(&current_config)?;
                    }
                }
            }

            ensure_provider_ready(&current_config).await?;

            // Use interactive selection if agent is not provided
            let agent = match agent {
                Some(a) => a,
                None => {
                    let options = CodeAgents::all_variants();
                    let options_strings: Vec<String> = options
                        .iter()
                        .map(|a| format!("{} - {}", a.name(), a.description()))
                        .collect();

                    let _selected = Select::new("Select code agent to launch:", options_strings)
                        .without_help_message()
                        .prompt()
                        .map_err(|e| anyhow::anyhow!("Failed to select agent: {}", e))?;

                    // Parse back the selected agent
                    // if selected.starts_with("claude") {
                    CodeAgents::Claude
                }
            };

            if config_flag {
                // Just run setup without launching the agent
                agent.check_installation()?;

                let proxy_url = format!("http://localhost:{}", current_config.port);

                if let Err(e) = agent.setup(&proxy_url).await {
                    return Err(Error::Internal(format!(
                        "Failed to setup {}: {}",
                        &agent.name(),
                        e
                    ))
                    .into());
                }

                println!(
                    "Configuration generated for {} with {} provider",
                    agent.name(),
                    current_config.active_provider
                );
                return Ok(());
            }

            agent.check_installation()?;

            // TODO: localhost must be a hostname in feature
            // Set environment variables and launch Claude CLI using async agent.run()
            let proxy_url = format!("http://localhost:{}", current_config.port);

            if let Err(e) = agent.setup(&proxy_url).await {
                return Err(
                    Error::Internal(format!("Failed to setup {}: {}", &agent.name(), e)).into(),
                );
            }

            //TODO: we need to have an option to start agent without proxy server
            // Start proxy server first
            let config_clone = current_config.clone();
            let server_handle = tokio::spawn(async move {
                let config = Arc::new(config_clone);
                let client = build_http_client()?;

                server::run_server(config, client, hook_chain, server::LogMode::SILENT).await
            });

            // Wait briefly for server to start
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

            tracing::info!(
                "🚀 Launching {} with proxy server at {}",
                agent.name(),
                proxy_url
            );

            // Run agent
            let result = agent.run(&[], &proxy_url).await;

            // Abort the server task when Code Agent exits (always)
            server_handle.abort();

            result?;

            Ok(())
        }
        Command::Stop { pid_file } => {
            let pid_file_path = pid_file.as_ref().unwrap_or(&cli.pid_file);
            stop_daemon(pid_file_path)?;
            tracing::info!("✓ Daemon stopped");
            return Ok(());
        }
        Command::Status { pid_file } => {
            let pid_file_path = pid_file.as_ref().unwrap_or(&cli.pid_file);
            match check_status(pid_file_path) {
                Ok(true) => return Ok(()),
                Ok(false) => {
                    tracing::info!("✗ Daemon is not running");
                    return Ok(());
                }
                Err(e) => {
                    tracing::info!("✗ Failed to check daemon status: {}", e);
                    return Err(e);
                }
            }
        }
        Command::Up { port, .. } => {
            // Daemon mode
            if cli.daemon {
                return handle_daemon_mode().await;
            }

            if cli.verbose {
                config.verbose = true;
            }

            if let Some(cli_port) = port {
                config.port = cli_port;
            }

            ensure_provider_ready(&config).await?;

            let config = Arc::new(config);

            // Build and run the server
            let client = build_http_client()?;

            server::run_server(
                config,
                client,
                hook_chain,
                server::LogMode::STDOUT | server::LogMode::FILE,
            )
            .await
        }
        Command::Logs { follow } => {
            //FIX:
            if cli.verbose {
                tracing::info!(
                    "{}", "Note: -v flag doesn't affect logs command. Use `klava up -v` to enable verbose server logging.".truecolor(255, 151, 0)
                );
            }
            handle_logs_command(follow)
        }
        Command::Config { setup } => handle_config_command(setup).await,
        Command::Providers { args } => handle_providers_command(args).await,
    }
}

// Build a reqwest client with configurable settings
fn build_http_client() -> Result<Client, anyhow::Error> {
    Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .connect_timeout(std::time::Duration::from_secs(10))
        .pool_max_idle_per_host(10)
        .build()
        .map_err(|e| anyhow::anyhow!("Failed to build HTTP client: {}", e))
}

/// Run the daemon mode
async fn handle_daemon_mode() -> anyhow::Result<()> {
    let temp_dir = std::env::temp_dir();
    let log_path = temp_dir.join("klava.log");
    let pid_file = std::path::PathBuf::from("/tmp/klava.pid");

    prepare_daemon(&log_path, &pid_file)?;
    tracing::info!("\nProxy now running in background");
    tracing::info!("  Stop daemon: klava stop {}", pid_file.display());
    tracing::info!("  Check status: klava status {}", pid_file.display());
    tracing::info!("  View logs: klava logs");
    Ok(())
}

/// Handle Qwen authentication with user consent and risk warnings
async fn handle_qwen_authentication() -> anyhow::Result<()> {
    use klava::qwen_auth::QwenAuth;
    let mut qwen_auth = QwenAuth::new();

    if !qwen_auth.is_authenticated() {
        use inquire::Confirm;

        println!("\n⚠️ Qwen Code Authentication Required");
        println!("\n🚨 Potential Risks:");
        println!("   • Your code and queries will be sent to Alibaba Cloud's Qwen service");
        println!(
            "   • Authentication flow involves reverse engineering of the Qwen Code application"
        );
        println!(
            "   • To Qwen's servers, this client will appear as the official Qwen Code application"
        );
        println!("   • Your qwen account may be suspended or terminated for using this tool");
        println!();
        println!("✅ Benefits:");
        println!("   • Qwen code gives you 1,000 free requests per day");
        println!();

        let consent =
            Confirm::new("Do you consent to authenticate with Qwen Code and accept these risks?")
                .with_default(false)
                .prompt()
                .map_err(|e| anyhow::anyhow!("Failed to get user consent: {}", e))?;

        if !consent {
            return Err(anyhow::anyhow!("User declined Qwen Code authentication"));
        }

        println!("🔐 Qwen Code authentication required");
        println!("Starting authentication flow...");

        qwen_auth
            .authenticate()
            .await
            .map_err(|e| Error::Internal(format!("Qwen authentication failed: {}", e)))?;

        println!("✅ Qwen Code authenticated successfully\n");
    }

    Ok(())
}

/// Check if active provider is ready and authenticate if needed
async fn ensure_provider_ready(config: &config::Config) -> anyhow::Result<()> {
    let active_provider_config = config.get_active_provider_config().ok_or_else(|| {
        Error::Internal(format!(
            "Active provider '{}' not found",
            config.active_provider
        ))
    })?;

    match active_provider_config.provider_type {
        klava::providers::Type::QwenCode => {
            handle_qwen_authentication().await?;
        }
        klava::providers::Type::OpenAICompatible => {
            active_provider_config.validate_config(config)?;
        }
    }

    Ok(())
}

/// Handle logs command - view or tail server logs
fn handle_logs_command(follow: bool) -> anyhow::Result<()> {
    let temp_dir = std::env::temp_dir();

    // Find the most recent log file
    let log_dir = temp_dir.as_path();
    let log_files: Vec<_> = std::fs::read_dir(log_dir)?
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("klava_server.log")
        })
        .collect();

    if log_files.is_empty() {
        tracing::info!("No log files found in {}", log_dir.display());
        tracing::info!("Start the server first with: klava up");
        return Ok(());
    }

    // Get the most recent log file
    let most_recent = log_files
        .into_iter()
        .filter_map(|entry| {
            entry
                .metadata()
                .ok()
                .map(|m| (entry.path(), m.modified().ok()))
        })
        .filter_map(|(path, time)| time.map(|t| (path, t)))
        .max_by_key(|(_, time)| *time)
        .map(|(path, _)| path)
        .unwrap();

    #[cfg(windows)]
    {
        tracing::info!("Log file: {}", most_recent.display());
        tracing::info!("Open the file above to view logs");
        Ok(())
    }

    #[cfg(not(windows))]
    {
        tracing::info!("Opening log file: {}", most_recent.display());

        let mut cmd = std::process::Command::new("tail");
        if follow {
            cmd.arg("-f");
        } else {
            cmd.arg("-100"); // Show last lines by default
        }
        cmd.arg(&most_recent);

        cmd.status()
            .map_err(|e| anyhow::anyhow!("Failed to run tail command: {}", e))?;

        Ok(())
    }
}

/// Handle config command - create config file or run interactive setup
async fn handle_config_command(setup: bool) -> anyhow::Result<()> {
    use klava::error::Error;

    if setup {
        // Run interactive setup
        cli_setup::run_interactive_setup().await?;
    } else {
        // Just create config file if it doesn't exist and show path
        config::Config::ensure_exists(false)
            .map_err(|e| Error::Internal(format!("Failed to create config: {}", e)))?;
    }

    Ok(())
}

/// Handle providers command
async fn handle_providers_command(args: Vec<String>) -> anyhow::Result<()> {
    let (provider_name, subcommand, flags) = parse_provider_args(args)?;

    match subcommand.as_deref() {
        Some("login") => {
            #[cfg(feature = "qwen-code")]
            {
                let force =
                    flags.contains(&"--force".to_string()) || flags.contains(&"-f".to_string());
                handle_qwen_login(force).await?;
            }
            #[cfg(not(feature = "qwen-code"))]
            {
                anyhow::bail!("qwen-code feature is not enabled");
            }
        }
        Some("logout") => {
            #[cfg(feature = "qwen-code")]
            {
                let force =
                    flags.contains(&"--force".to_string()) || flags.contains(&"-f".to_string());
                handle_qwen_logout(force).await?;
            }
            #[cfg(not(feature = "qwen-code"))]
            {
                anyhow::bail!("qwen-code feature is not enabled");
            }
        }
        Some("status") => {
            #[cfg(feature = "qwen-code")]
            {
                handle_qwen_status().await?;
            }
            #[cfg(not(feature = "qwen-code"))]
            {
                anyhow::bail!("qwen-code feature is not enabled");
            }
        }
        Some("set") | None => {
            // Set provider or show current
            if let Some(provider_name) = provider_name {
                // Set the provider
                let current_config = config::Config::load()?;
                let providers = current_config.providers;

                // Find matching provider
                let selected_provider = providers
                    .iter()
                    .find(|p| {
                        p.name == provider_name //|| p.aliases().contains(&provider_name.as_str())
                    })
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "Invalid provider: '{}'. Available providers: {}",
                            provider_name,
                            providers
                                .iter()
                                .map(|p| p.name.clone())
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    })?;

                save_provider_config(selected_provider)?;
                handle_qwen_provider_switch(&selected_provider.name).await?;
            } else {
                // Show current and prompt to choose
                let current_config = config::Config::load()?;
                let providers = current_config.providers;

                println!("Current provider: {}", current_config.active_provider);
                println!();
                println!("Available providers:");
                for provider in &providers {
                    let marker = if provider.name == current_config.active_provider {
                        "*"
                    } else {
                        " "
                    };
                    println!(" {} {} - {}", marker, provider.name, provider.description());
                }
                println!();

                use inquire::Select;
                let options: Vec<String> = providers
                    .iter()
                    .map(|p| {
                        let marker = if p.name == current_config.active_provider {
                            "*"
                        } else {
                            " "
                        };
                        format!("{} {} - {}", marker, p.name, p.description())
                    })
                    .collect();

                let selected = Select::new("Select provider:", options)
                    .prompt()
                    .map_err(|e| anyhow::anyhow!("Failed to select provider: {}", e))?;

                // Parse back the selected provider (extract name from selection)
                let selected_provider = providers
                    .iter()
                    .find(|p| selected.contains(&p.name))
                    .ok_or_else(|| anyhow::anyhow!("Failed to determine selected provider"))?;

                save_provider_config(selected_provider)?;
                handle_qwen_provider_switch(&selected_provider.name).await?;
            }
        }
        Some(other) => anyhow::bail!(
            "Unknown subcommand: '{}'. Use: login, logout, status, or omit to set provider",
            other
        ),
    }

    Ok(())
}

fn save_provider_config(provider: &klava::providers::Config) -> anyhow::Result<()> {
    let current_config = config::Config::load()?;
    let mut updated_config = current_config.clone();
    updated_config.active_provider = provider.name.to_string();

    // Save the updated config
    updated_config.save()?;
    println!("✓ Provider changed to: {}", provider.name);
    println!();
    Ok(())
}

/// Handle qwen provider switch: check auth and prompt to login if needed
async fn handle_qwen_provider_switch(provider_name: &str) -> anyhow::Result<()> {
    if provider_name != "qwen" {
        return Ok(());
    }

    #[cfg(feature = "qwen-code")]
    {
        use klava::providers::setup_qwen;

        // Load config, call setup_qwen (it doesn't modify config but needs the param)
        let mut config = config::Config::load()?;
        setup_qwen(&mut config)
            .await
            .map_err(|e| anyhow::anyhow!("Qwen authentication failed: {}", e))?;
    }

    Ok(())
}

/// Handle qwen provider switch (no-op when feature not enabled)
#[cfg(not(feature = "qwen-code"))]
async fn handle_qwen_provider_switch(_provider_name: &str) -> anyhow::Result<()> {
    Ok(())
}

/// Parse provider arguments
fn parse_provider_args(
    args: Vec<String>,
) -> anyhow::Result<(Option<String>, Option<String>, Vec<String>)> {
    let mut provider_name = None;
    let mut subcommand = None;
    let mut flags = Vec::new();

    // Handle: klava providers
    // Handle: klava providers qwen
    // Handle: klava providers set qwen
    // Handle: klava providers qwen login

    if args.is_empty() {
        return Ok((None, None, flags));
    }

    let mut iter = args.iter();
    let first = iter.next().unwrap();

    // Check if first arg is "set"
    if first == "set" {
        // klava providers set <provider>
        if let Some(name) = iter.next() {
            provider_name = Some(name.clone());
        }
        subcommand = Some("set".to_string());
    } else {
        // Could be: klava providers <provider> [subcommand] or klava providers qwen login
        let potential_provider = first.clone();

        provider_name = Some(potential_provider);

        // Check for subcommand
        if let Some(next) = iter.next() {
            // Check if it's a subcommand or flag
            if next.starts_with('-') {
                flags.push(next.clone());
            } else {
                subcommand = Some(next.clone());
            }
        }

        // Collect remaining flags
        for arg in iter {
            if arg.starts_with('-') {
                flags.push(arg.clone());
            }
        }
    }

    Ok((provider_name, subcommand, flags))
}

#[cfg(feature = "qwen-code")]
async fn handle_qwen_login(force: bool) -> anyhow::Result<()> {
    use klava::qwen_auth::QwenAuth;

    let mut qwen_auth = QwenAuth::new();

    if !force && qwen_auth.is_authenticated() {
        println!("Already authenticated with Qwen Code.");
        println!("Use --force to re-authenticate.");
        return Ok(());
    }

    // Check if provider is set to qwen
    let config = config::Config::load()?;
    if config.active_provider != "qwen" {
        println!(
            "⚠️  Warning: Current provider is set to '{}', not 'qwen'",
            config.active_provider
        );
        println!("You can switch providers with: klava providers qwen");
        println!();
    }

    qwen_auth.authenticate().await?;
    Ok(())
}

#[cfg(feature = "qwen-code")]
async fn handle_qwen_logout(force: bool) -> anyhow::Result<()> {
    use klava::qwen_auth::QwenAuth;

    if !force {
        use inquire::Confirm;
        let confirmed = Confirm::new("Are you sure you want to logout from Qwen Code?")
            .with_default(false)
            .prompt()
            .map_err(|e| anyhow::anyhow!("Failed to get confirmation: {}", e))?;

        if !confirmed {
            println!("Logout cancelled.");
            return Ok(());
        }
    }

    let mut qwen_auth = QwenAuth::new();
    qwen_auth.logout()?;
    Ok(())
}

#[cfg(feature = "qwen-code")]
async fn handle_qwen_status() -> anyhow::Result<()> {
    use klava::qwen_auth::QwenAuth;

    let qwen_auth = QwenAuth::new();
    let config = config::Config::load()?;

    println!("Provider status:");
    println!("  Current provider: {}", config.active_provider);
    println!();

    println!("Qwen Code authentication:");
    if qwen_auth.is_authenticated() {
        println!("  Status: ✓ Authenticated");
        if let Some(creds) = qwen_auth.get_credentials() {
            println!(
                "  Expires at: {}",
                creds.expires_at.format("%Y-%m-%d %H:%M:%S UTC")
            );
            if qwen_auth.needs_refresh() {
                println!("  ⚠️  Token needs refresh (will refresh on next use)");
            }
        }
    } else {
        println!("  Status: ✗ Not authenticated");
        println!("  Login with: klava providers qwen login");
    }
    Ok(())
}

/// Handle providers command
#[cfg(test)]
mod tests {
    use super::*;
    use cli::Command;

    #[test]
    fn cli_parses_up_with_custom_port_and_flags() {
        let parsed = Cli::try_parse_from(["klava", "up", "--port", "8080", "--foreground"]);
        assert!(parsed.is_ok());

        if let Ok(cli) = parsed {
            match cli.command {
                Command::Up { port, foreground } => {
                    assert_eq!(port, Some(8080));
                    assert!(foreground);
                }
                _ => panic!("Expected up command"),
            }
        }
    }

    #[test]
    fn cli_parses_stop_command() {
        let parsed = Cli::try_parse_from(["klava", "stop"]);
        assert!(parsed.is_ok());

        if let Ok(cli) = parsed {
            match cli.command {
                Command::Stop { pid_file } => {
                    // When global default is provided, pid_file will be Some with the default value
                    assert_eq!(pid_file, Some(std::path::PathBuf::from("/tmp/klava.pid")));
                }
                _ => panic!("Expected stop command"),
            }
        }
    }

    #[test]
    fn cli_parses_stop_command_with_custom_pid() {
        let parsed = Cli::try_parse_from(["klava", "stop", "--pid-file", "/tmp/custom.pid"]);
        assert!(parsed.is_ok());

        if let Ok(cli) = parsed {
            match cli.command {
                Command::Stop { pid_file } => {
                    assert_eq!(pid_file, Some(std::path::PathBuf::from("/tmp/custom.pid")));
                }
                _ => panic!("Expected stop command"),
            }
        }
    }

    #[test]
    fn cli_parses_status_command() {
        let parsed = Cli::try_parse_from(["klava", "status"]);
        assert!(parsed.is_ok());

        if let Ok(cli) = parsed {
            match cli.command {
                Command::Status { pid_file } => {
                    // When global default is provided, pid_file will be Some with the default value
                    assert_eq!(pid_file, Some(std::path::PathBuf::from("/tmp/klava.pid")));
                }
                _ => panic!("Expected status command"),
            }
        }
    }

    #[test]
    fn cli_parses_status_command_with_custom_pid() {
        let parsed = Cli::try_parse_from(["klava", "status", "--pid-file", "/tmp/custom.pid"]);
        assert!(parsed.is_ok());

        if let Ok(cli) = parsed {
            match cli.command {
                Command::Status { pid_file } => {
                    assert_eq!(pid_file, Some(std::path::PathBuf::from("/tmp/custom.pid")));
                }
                _ => panic!("Expected status command"),
            }
        }
    }

    #[test]
    fn cli_parses_verbose_flag() {
        let parsed = Cli::try_parse_from(["klava", "--verbose", "up"]);
        assert!(parsed.is_ok());

        if let Ok(cli) = parsed {
            assert!(cli.verbose);
        }
    }

    #[test]
    fn cli_parses_daemon_flag() {
        let parsed = Cli::try_parse_from(["klava", "--daemon", "up"]);
        assert!(parsed.is_ok());

        if let Ok(cli) = parsed {
            assert!(cli.daemon);
        }
    }

    #[test]
    fn cli_rejects_unknown_subcommand() {
        let parsed = Cli::try_parse_from(["klava", "unknown"]);
        assert!(parsed.is_err());
    }

    #[test]
    fn cli_parses_config_command() {
        let parsed = Cli::try_parse_from(["klava", "config"]);
        assert!(parsed.is_ok());

        if let Ok(cli) = parsed {
            match cli.command {
                Command::Config { setup } => {
                    assert!(!setup);
                }
                _ => panic!("Expected config command"),
            }
        }
    }

    #[test]
    fn cli_parses_config_command_with_setup_flag() {
        let parsed = Cli::try_parse_from(["klava", "config", "--setup"]);
        assert!(parsed.is_ok());

        if let Ok(cli) = parsed {
            match cli.command {
                Command::Config { setup } => {
                    assert!(setup);
                }
                _ => panic!("Expected config command"),
            }
        }
    }
}
