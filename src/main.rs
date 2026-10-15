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

    // Explain why interactive setup runs when a command needs it
    let setup_context = match &cli.command {
        Command::Launch { agent, .. } => Some(match agent {
            Some(agent) => format!(
                "klava launch {} needs an AI provider first. Let's configure one, then we'll launch {}.",
                agent.name(),
                agent.name()
            ),
            None => "klava launch needs an AI provider first. Let's configure one now.".to_string(),
        }),
        _ => None,
    };

    let mut config =
        cli_setup::build_config_interactive_with_context(setup_context.as_deref()).await?;

    let hook_chain = Arc::new(default_chain(&config));

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
                    #[cfg(feature = "klava")]
                    klava::providers::Type::Klava => {
                        ensure_klava_authenticated(false).await?;
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
                        agent.name(),
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

            //TODO: we need to have an option to start agent without proxy server
            // Start proxy server first and report the port it bound to
            let config_clone = current_config.clone();
            let (port_tx, port_rx) = tokio::sync::oneshot::channel::<u16>();
            let server_handle = tokio::spawn(async move {
                let config = Arc::new(config_clone);
                let client = build_http_client()?;

                server::run_server(
                    config,
                    client,
                    hook_chain,
                    server::LogMode::SILENT,
                    Some(port_tx),
                )
                .await?;
                Ok::<(), anyhow::Error>(())
            });

            // Wait for the proxy server to report the port it bound to
            let proxy_port = tokio::time::timeout(tokio::time::Duration::from_secs(10), port_rx)
                .await
                .map_err(|_| anyhow::anyhow!("Proxy server failed to start in time"))?
                .map_err(|_| anyhow::anyhow!("Proxy server failed to start"))?;

            let proxy_url = format!("http://localhost:{}", proxy_port);

            if let Err(e) = agent.setup(&proxy_url).await {
                return Err(
                    Error::Internal(format!("Failed to setup {}: {}", agent.name(), e)).into(),
                );
            }

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

            let port = server::run_server(
                config,
                client,
                hook_chain,
                server::LogMode::STDOUT | server::LogMode::FILE,
                None,
            )
            .await?;

            tracing::info!("Proxy is running on port {}", port);
            Ok(())
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
        Command::Check { provider, model } => {
            if let Some(provider_name) = provider {
                // Check specific provider
                let target_config = config.clone();
                klava::diagnostic::run_tests(
                    &target_config,
                    Some(&provider_name),
                    model.as_deref(),
                )
                .await;
            } else {
                // Check active provider
                klava::diagnostic::run_tests(&config, None, model.as_deref()).await;
            }
            Ok(())
        }
        Command::Login { force } => handle_klava_login(force).await,
        Command::Logout { force } => handle_klava_logout(force).await,
        Command::Whoami {} => handle_klava_whoami().await,
        Command::Balance {} => handle_klava_balance().await,
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
        #[cfg(feature = "klava")]
        klava::providers::Type::Klava => {
            ensure_klava_authenticated(false).await?;
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
            let force = flags.contains(&"--force".to_string()) || flags.contains(&"-f".to_string());
            if provider_name.as_deref() == Some("klava") {
                return handle_klava_login(force).await;
            }
            #[cfg(feature = "qwen-code")]
            {
                handle_qwen_login(force).await?;
            }
            #[cfg(not(feature = "qwen-code"))]
            {
                anyhow::bail!("qwen-code feature is not enabled");
            }
        }
        Some("logout") => {
            let force = flags.contains(&"--force".to_string()) || flags.contains(&"-f".to_string());
            if provider_name.as_deref() == Some("klava") {
                return handle_klava_logout(force).await;
            }
            #[cfg(feature = "qwen-code")]
            {
                handle_qwen_logout(force).await?;
            }
            #[cfg(not(feature = "qwen-code"))]
            {
                anyhow::bail!("qwen-code feature is not enabled");
            }
        }
        Some("status") => {
            if provider_name.as_deref() == Some("klava") {
                return handle_klava_status().await;
            }
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
                handle_provider_switch(&selected_provider.name).await?;
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
                handle_provider_switch(&selected_provider.name).await?;
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

/// Dispatch the "set provider" post-switch hook to the right provider.
async fn handle_provider_switch(provider_name: &str) -> anyhow::Result<()> {
    if provider_name == "klava" {
        return handle_klava_login(false).await;
    }
    handle_qwen_provider_switch(provider_name).await
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

// ============================================================
// Klava provider commands (feature "klava")
// ============================================================

/// Ensure the Klava provider is authenticated, running the device login
/// when no local session exists.
#[cfg(feature = "klava")]
async fn ensure_klava_authenticated(force: bool) -> anyhow::Result<()> {
    use klava::klava_auth::KlavaAuth;

    let mut auth = KlavaAuth::new();
    if auth.is_authenticated() && !force {
        return Ok(());
    }
    if force {
        println!("⚠️  Re-authenticating Klava...");
    }
    handle_klava_login(force).await
}

/// `klava login` / `klava providers klava login` — device flow, then write
/// the provider config (models, base URL) and activate it.
#[cfg(feature = "klava")]
async fn handle_klava_login(force: bool) -> anyhow::Result<()> {
    use inquire::Select;
    use klava::klava_auth::KlavaAuth;
    use klava::providers::Type;

    let mut auth = KlavaAuth::new();

    if !force && auth.is_authenticated() {
        println!("Already signed in to Klava.");
        println!("Use --force to re-authenticate.");
        return Ok(());
    }

    let credentials = auth.authenticate().await?;

    // Pick the default model from the live catalog. Interactive selection
    // only when stdin is a TTY; otherwise honor $KLAVA_MODEL or fall back to
    // the first enabled deepseek model.
    use std::io::IsTerminal;
    let env_model = std::env::var("KLAVA_MODEL").ok().filter(|m| !m.is_empty());
    let interactive = std::io::stdin().is_terminal() && env_model.is_none();

    let model: Option<String> = match auth.models().await {
        Ok(models) => {
            let enabled: Vec<_> = models.into_iter().filter(|m| m.enabled).collect();
            if enabled.is_empty() {
                None
            } else if let Some(m) = env_model {
                Some(m)
            } else if !interactive {
                enabled
                    .iter()
                    .find(|m| m.chat_id().contains("deepseek-flash"))
                    .or_else(|| enabled.iter().find(|m| m.chat_id().contains("deepseek")))
                    .or(enabled.first())
                    .map(|m| m.chat_id())
            } else {
                let options: Vec<String> = enabled
                    .iter()
                    .map(|m| {
                        format!(
                            "{}  {}",
                            m.chat_id(),
                            if m.label.is_empty() {
                                m.openrouter_id.as_str()
                            } else {
                                m.label.as_str()
                            }
                        )
                    })
                    .collect();
                let default_idx = enabled
                    .iter()
                    .position(|m| m.chat_id().contains("deepseek-flash"))
                    .or_else(|| enabled.iter().position(|m| m.chat_id().contains("deepseek")))
                    .unwrap_or(0);
                let selected =
                    Select::new("Select the default model for the klava provider:", options)
                        .with_starting_cursor(default_idx)
                        .prompt()
                        .map_err(|e| anyhow::anyhow!("Failed to select model: {}", e))?;
                enabled
                    .iter()
                    .find(|m| {
                        format!(
                            "{}  {}",
                            m.chat_id(),
                            if m.label.is_empty() {
                                m.openrouter_id.as_str()
                            } else {
                                m.label.as_str()
                            }
                        ) == selected
                    })
                    .map(|m| m.chat_id())
                    .or_else(|| Some(selected.split("  ").next().unwrap_or("").to_string()))
            }
        }
        Err(e) => {
            tracing::warn!("Could not fetch model catalog: {}", e);
            None
        }
    };
    let mut config = config::Config::load()?;

    let entry = config.providers.iter_mut().find(|p| p.name == "klava");
    if let Some(provider) = entry {
        provider.provider_type = Type::Klava;
        provider.base_url = Some(format!("{}/api", auth.base_url()));
        if let Some(m) = &model {
            provider.reasoning_model = Some(m.clone());
            provider.completion_model = Some(m.clone());
        }
    } else {
        config.providers.push(klava::providers::Config {
            name: "klava".to_string(),
            provider_type: Type::Klava,
            base_url: Some(format!("{}/api", auth.base_url())),
            chat_completions_path: None,
            api_key: None,
            api_key_name: None,
            reasoning_model: model.clone(),
            completion_model: model,
        });
    }
    config.active_provider = "klava".to_string();
    config.save()?;

    println!("✓ Provider 'klava' configured and set as active.");
    if !credentials.email.is_empty() {
        println!("  Account: {}", credentials.email);
    }
    if credentials.key_preview.is_empty() {
        println!("  Key: {}", credentials.api_key);
    } else {
        println!(
            "  Key: {} ({} chars)",
            credentials.key_preview,
            credentials.api_key.chars().count()
        );
    }
    println!();
    println!("Next steps:");
    println!("  klava whoami     — account + balance");
    println!("  klava launch pi  — start an agent on the klava provider");
    Ok(())
}

/// `klava logout` / `klava providers klava logout` — revoke + clear.
#[cfg(feature = "klava")]
async fn handle_klava_logout(force: bool) -> anyhow::Result<()> {
    use inquire::Confirm;
    use klava::klava_auth::KlavaAuth;

    let mut auth = KlavaAuth::new();
    if !auth.is_authenticated() {
        println!("Not signed in to Klava.");
        return Ok(());
    }

    if !force {
        let confirmed =
            Confirm::new("Revoke the KLava API key on the server and sign out locally?")
                .with_default(false)
                .prompt()
                .map_err(|e| anyhow::anyhow!("Failed to get confirmation: {}", e))?;
        if !confirmed {
            println!("Logout cancelled.");
            return Ok(());
        }
    }

    auth.logout(true).await?;
    println!("✅ Signed out from Klava");
    Ok(())
}

/// `klava providers klava status` — local session state.
#[cfg(feature = "klava")]
async fn handle_klava_status() -> anyhow::Result<()> {
    use klava::klava_auth::KlavaAuth;

    let mut auth = KlavaAuth::new();
    println!("Klava authentication:");
    match auth.get_credentials() {
        Some(creds) => {
            println!("  Status: ✓ Authenticated");
            println!("  Key: {}", creds.key_preview);
            if !creds.email.is_empty() {
                println!("  Account: {}", creds.email);
            }
            println!("  Key saved at: ~/.local/share/klava/klava-auth.json");
        }
        None => {
            println!("  Status: ✗ Not signed in");
            println!("  Login with: klava login");
        }
    }
    Ok(())
}

/// `klava whoami` — account email, key preview and balance.
#[cfg(feature = "klava")]
async fn handle_klava_whoami() -> anyhow::Result<()> {
    use klava::klava_auth::KlavaAuth;

    let mut auth = KlavaAuth::new();
    let creds = auth
        .get_credentials()
        .ok_or_else(|| anyhow::anyhow!("Not signed in to Klava. Run: klava login"))?;

    println!("Klava session:");
    println!("  Key: {}", creds.key_preview);
    if !creds.email.is_empty() {
        println!("  Account: {}", creds.email);
    }

    match auth.balance().await {
        Ok(balance) => {
            println!(
                "  Balance: {} {}",
                format_rub(balance.amount_rub),
                if balance.currency.is_empty() {
                    "RUB".to_string()
                } else {
                    balance.currency
                }
            );
        }
        Err(e) => {
            println!("  Balance: unavailable ({})", e);
        }
    }
    Ok(())
}

/// `klava balance` — the prepaid RUB balance of the current account.
#[cfg(feature = "klava")]
async fn handle_klava_balance() -> anyhow::Result<()> {
    use klava::klava_auth::KlavaAuth;

    let mut auth = KlavaAuth::new();
    let balance = auth
        .balance()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to load balance: {}", e))?;

    println!(
        "Balance: {} {}",
        format_rub(balance.amount_rub),
        if balance.currency.is_empty() {
            "RUB".to_string()
        } else {
            balance.currency
        }
    );
    Ok(())
}

/// Format a money amount from the proxy ("1234.000000" / 1234.0) for display.
#[cfg(feature = "klava")]
fn format_rub(raw: f64) -> String {
    let s = format!("{raw:.2}");
    let (int_part, frac) = s.split_once('.').unwrap_or((&s, "00"));
    let mut out = String::new();
    let chars: Vec<char> = int_part.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        if i > 0 && (chars.len() - i).is_multiple_of(3) {
            out.push(' ');
        }
        out.push(*c);
    }
    format!("{out},{frac}")
}

// Fallbacks when the klava feature is disabled.
#[cfg(not(feature = "klava"))]
async fn handle_klava_login(_force: bool) -> anyhow::Result<()> {
    anyhow::bail!("klava feature is not enabled in this build")
}

#[cfg(not(feature = "klava"))]
async fn handle_klava_logout(_force: bool) -> anyhow::Result<()> {
    anyhow::bail!("klava feature is not enabled in this build")
}

#[cfg(not(feature = "klava"))]
async fn handle_klava_status() -> anyhow::Result<()> {
    anyhow::bail!("klava feature is not enabled in this build")
}

#[cfg(not(feature = "klava"))]
async fn handle_klava_whoami() -> anyhow::Result<()> {
    anyhow::bail!("klava feature is not enabled in this build")
}

#[cfg(not(feature = "klava"))]
async fn handle_klava_balance() -> anyhow::Result<()> {
    anyhow::bail!("klava feature is not enabled in this build")
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

    #[cfg(feature = "klava")]
    #[test]
    fn format_rub_formats_thousands_and_decimals() {
        assert_eq!(format_rub(0.0), "0,00");
        assert_eq!(format_rub(1234.0), "1 234,00");
        assert_eq!(format_rub(9411.823457), "9 411,82");
    }
}
