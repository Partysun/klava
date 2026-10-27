mod cli;
mod cli_setup;
mod daemon;
mod service;

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
            "A new version of klava is available! Run `klava upgrade` or update via your package manager.\n"
                .truecolor(255, 151, 0)
        );
    }

    let cli = Cli::parse();

    // Explain why interactive setup runs when a command needs it
    let setup_context = match &cli.command {
        Command::Launch { agent, .. } => Some(match agent {
            Some(agent) => format!(
                "klava launch {} needs an AI model first. Let's set one up, then we'll launch {}.",
                agent.name(),
                agent.name()
            ),
            None => "klava launch needs an AI model first. Let's set one up now.".to_string(),
        }),
        _ => None,
    };

    let mut config = cli_setup::build_config_interactive_with_context(
        cli_setup::auto_setup_for(subcommand_name(&cli.command)),
        setup_context.as_deref(),
    )
    .await?;

    let hook_chain = Arc::new(default_chain(&config));

    match cli.command {
        Command::Launch {
            agent,
            config: config_flag,
            provider,
            port,
            no_service,
            search,
            search_model,
            effort,
        } => {
            use inquire::Select;

            // Create a mutable config to potentially update
            let mut current_config = config.clone();

            // Handle port override if provided
            if let Some(cli_port) = port {
                current_config.port = cli_port;
            }

            // Web search: --search-model implies --search.
            if search || search_model.is_some() {
                current_config.enable_websearch = true;
            }
            if let Some(model) = search_model {
                current_config.search_model = Some(model);
            }

            // `--effort <level>` sets the default for this launch. Per-model
            // `reasoning_effort` in the config still wins, and a client that
            // asks for its own level still wins over both.
            if let Some(level) = effort {
                current_config.reasoning_effort = Some(level.as_str().to_string());
            }

            // `--provider <model>` switches the active model for this launch.
            if let Some(model_name) = provider {
                if current_config.pool(&model_name).is_empty() {
                    return Err(Error::Internal(format!(
                        "Invalid model: '{}'. Available models: {}",
                        model_name,
                        current_config.model_names().join(", ")
                    ))
                    .into());
                }
                current_config.completion_model = Some(model_name.clone());
                current_config.reasoning_model = Some(model_name.clone());
                println!("Using model: {}", model_name);
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
                    "Configuration generated for {} with model {}",
                    agent.name(),
                    current_config
                        .resolve_completion_model()
                        .unwrap_or_else(|| "none".to_string())
                );
                return Ok(());
            }

            agent.check_installation()?;

            // Prefer the managed background service (systemd user session on
            // Linux, launchd on macOS): the proxy keeps running after the
            // agent exits, survives crashes via Restart=always/KeepAlive and
            // restarts at login. Fall back to spawning a child proxy when no
            // service manager is available or --no-service is set.
            let use_service = !no_service && service::manager_available();

            let (proxy_url, server_handle) = if use_service {
                let action = service::ensure_service(&current_config).await?;
                match action {
                    service::ServiceAction::Installed => tracing::info!(
                        "✓ Installed managed service for port {} (auto-start at login)",
                        current_config.port
                    ),
                    service::ServiceAction::Rewritten => tracing::info!(
                        "✓ Refreshed managed service for port {}",
                        current_config.port
                    ),
                    service::ServiceAction::AlreadyActive | service::ServiceAction::Started => {}
                }
                let url = format!("http://localhost:{}", current_config.port);
                tracing::info!(
                    "✓ klava proxy running as a managed service ({}) — restarts automatically",
                    service::instance_name(current_config.port)
                );
                (url, None)
            } else {
                // Spawn a child proxy whose lifetime is tied to this launch.
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
                let proxy_port =
                    tokio::time::timeout(tokio::time::Duration::from_secs(10), port_rx)
                        .await
                        .map_err(|_| anyhow::anyhow!("Proxy server failed to start in time"))?
                        .map_err(|_| anyhow::anyhow!("Proxy server failed to start"))?;

                let url = format!("http://localhost:{}", proxy_port);
                tracing::info!(
                    "✓ klava proxy started for this launch (child process, stops when the agent exits)"
                );
                (url, Some(server_handle))
            };

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

            // Run agent. The managed service keeps the proxy alive; the
            // child proxy (legacy mode) is aborted when the agent exits.
            let result = agent.run(&[], &proxy_url).await;

            if let Some(handle) = server_handle {
                // Abort the server task when Code Agent exits (legacy behavior)
                handle.abort();
            }

            result?;

            Ok(())
        }
        Command::Stop { pid_file } => {
            let pid_file_path = pid_file.as_ref().unwrap_or(&cli.pid_file);
            let _ = stop_daemon(pid_file_path); // legacy daemon, may not exist
            service::stop_service(config.port); // managed service
            println!("✓ Proxy stopped");
            return Ok(());
        }
        Command::Restart { port } => {
            let target = port.unwrap_or(config.port);
            service::restart_service(target).await?;
            println!(
                "✓ Proxy restarted on port {target} ({})",
                service::instance_name(target)
            );
            return Ok(());
        }
        Command::Status { pid_file } => {
            if service::manager_available() {
                return service::status();
            }
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
        Command::Up {
            port,
            no_provider_check,
            search,
            effort,
            ..
        } => {
            if cli.verbose {
                config.verbose = true;
            }

            if let Some(cli_port) = port {
                config.port = cli_port;
            }

            // The managed service passes --search so the running proxy enables
            // web search even though it only receives a port on its command line.
            if search {
                config.enable_websearch = true;
            }

            // Same for --effort: the unit/plist carries it, and the proxy
            // process re-reads the config file from disk.
            if let Some(level) = effort {
                config.reasoning_effort = Some(level.as_str().to_string());
            }

            // Daemon mode: prefer the managed service (systemd on Linux,
            // launchd on macOS — self-healing, survives terminals and
            // reboots); fall back to the legacy fork-based daemon when no
            // manager exists.
            if cli.daemon {
                if service::manager_available() {
                    let action = service::ensure_service(&config).await?;
                    match action {
                        service::ServiceAction::Installed => tracing::info!(
                            "✓ Installed managed service for port {} (auto-start at login)",
                            config.port
                        ),
                        service::ServiceAction::Rewritten => {
                            tracing::info!("✓ Refreshed managed service for port {}", config.port)
                        }
                        service::ServiceAction::AlreadyActive => tracing::info!(
                            "✓ Managed service already running on port {}",
                            config.port
                        ),
                        service::ServiceAction::Started => {
                            tracing::info!("✓ Managed service started on port {}", config.port)
                        }
                    }
                    tracing::info!(
                        "  Managed by: {} (status: `klava status`, logs: `{}`)",
                        service::instance_name(config.port),
                        service::logs_command(config.port)
                    );
                    return Ok(());
                }
                return handle_daemon_mode().await;
            }

            // The managed service passes --no-provider-check to avoid
            // interactive prompts and restart-loops inside systemd.
            if !no_provider_check {
                ensure_provider_ready(&config).await?;
            }

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
        Command::Extensions { args } => klava::ext_cli::run(&mut config, args),
        Command::Upgrade { check, yes, force } => klava::self_upgrade::run(check, yes, force),
        Command::Check { provider, model } => {
            // `--provider` filters by model-entry name (the old flag name is
            // kept so existing scripts keep working).
            //
            // The service state is read here, in the binary, because service
            // management (systemd/launchd) lives outside the library.
            let instance = service::instance_state(config.port);
            let active = instance.is_active();
            let status = klava::diagnostic::ServiceStatus {
                unit: instance.unit,
                installed: instance.installed,
                active,
                state: instance.state,
                logs_command: service::logs_command(config.port),
            };
            klava::diagnostic::run_tests(&config, provider.as_deref(), model.as_deref(), status)
                .await;
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
        // Read timeout, not a total timeout: a streaming response that is
        // slowly producing tokens must not be cut off at a fixed deadline.
        // Only abort when the connection goes completely silent for this long.
        .read_timeout(std::time::Duration::from_secs(300))
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
#[cfg(feature = "qwen-code")]
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

/// Check that the configured models are ready, authenticating when needed.
///
/// Every entry is validated; the login flows run once per credential kind
/// (klava device flow, qwen OAuth) and never for a model that carries its own
/// key.
async fn ensure_provider_ready(config: &config::Config) -> anyhow::Result<()> {
    config.validate_complete()?;

    #[cfg(feature = "qwen-code")]
    let mut qwen_checked = false;
    #[cfg(feature = "klava")]
    let mut klava_checked = false;

    for model in &config.models {
        match model.provider_type {
            #[cfg(feature = "qwen-code")]
            klava::providers::Type::QwenCode => {
                if !qwen_checked {
                    handle_qwen_authentication().await?;
                    qwen_checked = true;
                }
            }
            #[cfg(feature = "klava")]
            klava::providers::Type::Klava => {
                if !klava_checked && !model.has_klava_auth() {
                    ensure_klava_authenticated(false).await?;
                    klava_checked = true;
                }
            }
            klava::providers::Type::OpenAICompatible => {
                model.validate_config(config)?;
            }
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

/// Error used by the subcommands that only exist with a feature enabled.
#[cfg(not(feature = "qwen-code"))]
fn qwen_not_built() -> anyhow::Result<()> {
    anyhow::bail!("qwen-code support is not enabled in this build")
}

/// The subcommand name, for decisions made before dispatch.
fn subcommand_name(command: &cli::Command) -> &'static str {
    match command {
        cli::Command::Launch { .. } => "launch",
        cli::Command::Up { .. } => "up",
        cli::Command::Stop { .. } => "stop",
        cli::Command::Restart { .. } => "restart",
        cli::Command::Status { .. } => "status",
        cli::Command::Logs { .. } => "logs",
        cli::Command::Config { .. } => "config",
        cli::Command::Providers { .. } => "providers",
        cli::Command::Extensions { .. } => "extensions",
        cli::Command::Upgrade { .. } => "upgrade",
        cli::Command::Check { .. } => "check",
        cli::Command::Login { .. } => "login",
        cli::Command::Logout { .. } => "logout",
        cli::Command::Whoami { .. } => "whoami",
        cli::Command::Balance { .. } => "balance",
    }
}

/// Handle providers command.
///
/// `klava providers` is the onboarding surface: without a subcommand it lists
/// the configured models and offers to add a provider. `klava providers add`
/// runs the wizard; `check`, `remove` and `set` manage what it wrote.
async fn handle_providers_command(args: Vec<String>) -> anyhow::Result<()> {
    let mut args = args.into_iter().peekable();
    let first = args.peek().cloned();

    match first.as_deref() {
        Some("add") => {
            args.next();
            let id = args.next();
            add_provider(id.as_deref()).await?;
            Ok(())
        }
        Some("list") => {
            list_models(&config::Config::load()?);
            Ok(())
        }
        Some("check") => {
            args.next();
            let name = args.next();
            let config = config::Config::load()?;
            let mut all_passed = true;
            let targets: Vec<_> = match name.as_deref() {
                Some(name) => config.models.iter().filter(|m| m.name == name).collect(),
                None => config.models.iter().collect(),
            };
            if targets.is_empty() {
                anyhow::bail!(
                    "no models configured - run 'klava providers add'{}",
                    name.map(|n| format!(" (no model named '{n}')"))
                        .unwrap_or_default()
                );
            }
            let client = build_http_client()?;
            for model in targets {
                match klava::diagnostic::test_provider(&client, &config, model, None).await {
                    Ok(result) => {
                        result.print_summary();
                        all_passed &= result.success;
                    }
                    Err(e) => {
                        println!("  ✗ {}: {}", model.name, e);
                        all_passed = false;
                    }
                }
            }
            if !all_passed {
                std::process::exit(1);
            }
            Ok(())
        }
        Some("remove") | Some("rm") => {
            args.next();
            let name = args
                .next()
                .ok_or_else(|| anyhow::anyhow!("usage: klava providers remove <model>"))?;
            let mut config = config::Config::load()?;
            let removed = klava::providers::remove_provider(&mut config, &name)?;
            config.save()?;
            println!(
                "✓ removed {removed} entr{} named '{name}'",
                if removed == 1 { "y" } else { "ies" }
            );
            if config.models.is_empty() {
                println!("  no models left - run 'klava providers add'");
            }
            Ok(())
        }
        Some("set") => {
            args.next();
            match args.next() {
                Some(name) => set_default_model(&name),
                None => pick_default_model(config::Config::load()?),
            }
        }
        Some("login") | Some("logout") | Some("status") | Some("whoami") => {
            let subcommand = args.next().unwrap_or_default();
            let force = has_flag(&args, "force");
            match subcommand.as_str() {
                "login" => handle_klava_login(force).await,
                "logout" => handle_klava_logout(force).await,
                "status" => handle_klava_status().await,
                _ => handle_klava_whoami().await,
            }
        }
        Some(id) if klava::providers::providers().iter().any(|k| k.id == id) => {
            // `klava providers klava [login|status|...]`
            let id = args.next().unwrap();
            match args.next().as_deref() {
                Some("login") => match id.as_str() {
                    "klava" => handle_klava_login(has_flag(&args, "force")).await,
                    #[cfg(feature = "qwen-code")]
                    "qwen-code" => handle_qwen_login(has_flag(&args, "force")).await,
                    #[cfg(not(feature = "qwen-code"))]
                    "qwen-code" => qwen_not_built(),
                    _ => add_provider(Some(&id)).await,
                },
                Some("logout") => match id.as_str() {
                    "klava" => handle_klava_logout(has_flag(&args, "force")).await,
                    #[cfg(feature = "qwen-code")]
                    "qwen-code" => handle_qwen_logout(has_flag(&args, "force")).await,
                    #[cfg(not(feature = "qwen-code"))]
                    "qwen-code" => qwen_not_built(),
                    _ => anyhow::bail!("'{id}' has no login to clear"),
                },
                Some("status") => handle_provider_status(&id).await,
                Some(other) => anyhow::bail!(
                    "unknown subcommand '{other}' for '{id}'. Use: login, logout, status, or no subcommand to add it"
                ),
                None => add_provider(Some(&id)).await,
            }
        }
        Some(other) => anyhow::bail!(
            "unknown subcommand '{other}'. Use: klava providers [add [provider] | list | check [model] | remove <model> | set [model]]"
        ),
        None => {
            let config = config::Config::load()?;
            list_models(&config);
            if config.models.is_empty() && std::io::IsTerminal::is_terminal(&std::io::stdin()) {
                use inquire::Confirm;
                let add = Confirm::new("No models configured. Add a provider now?")
                    .with_default(true)
                    .prompt()
                    .unwrap_or(false);
                if add {
                    add_provider(None).await?;
                }
            }
            Ok(())
        }
    }
}

/// Whether `--<name>` (or its short form) is among the remaining arguments.
fn has_flag(args: &std::iter::Peekable<std::vec::IntoIter<String>>, name: &str) -> bool {
    args.clone()
        .any(|a| a == format!("--{name}") || (name == "force" && a == "-f"))
}

/// Run the onboarding wizard and report what was written.
async fn add_provider(id: Option<&str>) -> anyhow::Result<()> {
    let mut config = config::Config::load()?;
    klava::providers::add_provider_interactive(&mut config, id).await?;
    print_next_steps(&config);
    Ok(())
}

/// `klava providers list` — the configured model entries.
fn list_models(config: &config::Config) {
    let names = config.model_names();
    if names.is_empty() {
        println!("No models configured.");
        println!();
        println!(
            "Run 'klava providers add' to set up Klava, OpenRouter, Cloudflare, Qwen or a custom endpoint."
        );
        return;
    }

    println!("Models (routing: {}):", config.routing);
    for name in &names {
        let mut marks = Vec::new();
        if config.completion_model.as_deref() == Some(name.as_str()) {
            marks.push("completion");
        }
        if config.reasoning_model.as_deref() == Some(name.as_str()) {
            marks.push("reasoning");
        }
        if config.resolve_decide_model() == *name {
            marks.push("decide");
        }
        let suffix = if marks.is_empty() {
            String::new()
        } else {
            format!("  [{}]", marks.join(", "))
        };
        println!();
        println!("  {name}{suffix}");
        for model in config.pool(name) {
            let key = match (&model.api_key, &model.api_key_name) {
                (Some(_), _) => "key: config".to_string(),
                (_, Some(env)) => format!("key: ${env}"),
                _ => "key: login".to_string(),
            };
            let jigor = model
                .jigor_backend()
                .map(|b| format!(", decide via {}", b.as_str()))
                .unwrap_or_default();
            println!(
                "    {} · {}{}",
                model
                    .chat_completions_url()
                    .unwrap_or_else(|| "no URL".to_string()),
                key,
                jigor
            );
        }
    }

    println!();
    println!(
        "Web search: {}",
        if config.websearch_available() {
            "on"
        } else {
            "off"
        }
    );
    println!("Decide model: {}", config.resolve_decide_model());
}

/// Print the follow-up commands after setup.
fn print_next_steps(config: &config::Config) {
    println!();
    println!("Next steps:");
    println!("  klava providers check      — verify every model answers");
    println!("  klava providers list       — see what is configured");
    if let Some(completion) = &config.completion_model {
        println!("  klava launch               — start an agent on '{completion}'");
    }
}

/// `klava providers set <model>` — make a model the default.
fn set_default_model(name: &str) -> anyhow::Result<()> {
    let mut config = config::Config::load()?;
    if config.pool(name).is_empty() {
        anyhow::bail!(
            "no model named '{name}'. Configured: {}",
            config.model_names().join(", ")
        );
    }
    klava::providers::set_defaults(&mut config, Some(name), Some(name));
    config.save()?;
    println!("✓ default model set to '{name}'");
    Ok(())
}

/// Interactive default picker (the `set` without arguments path).
fn pick_default_model(mut config: config::Config) -> anyhow::Result<()> {
    use inquire::Select;

    let names = config.model_names();
    if names.is_empty() {
        anyhow::bail!("no models configured - run 'klava providers add'");
    }
    let selected = Select::new("Default (completion) model:", names)
        .prompt()
        .map_err(|e| anyhow::anyhow!("failed to select model: {e}"))?;
    klava::providers::set_defaults(&mut config, Some(&selected), Some(&selected));
    config.save()?;
    println!("✓ default model set to '{selected}'");
    Ok(())
}

/// `klava providers <klava|qwen-code> status`.
async fn handle_provider_status(id: &str) -> anyhow::Result<()> {
    match id {
        "klava" => handle_klava_status().await,
        #[cfg(feature = "qwen-code")]
        "qwen-code" => handle_qwen_status().await,
        #[cfg(not(feature = "qwen-code"))]
        "qwen-code" => qwen_not_built(),
        _ => {
            let config = config::Config::load()?;
            let count = config
                .models
                .iter()
                .filter(|m| m.provider_type.as_str() == id)
                .count();
            println!(
                "{id}: {count} model entr{} configured",
                if count == 1 { "y" } else { "ies" }
            );
            Ok(())
        }
    }
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

    // Warn when no Qwen model is configured: the login would not be used.
    let config = config::Config::load()?;
    let has_qwen = config
        .models
        .iter()
        .any(|m| m.provider_type == klava::providers::Type::QwenCode);
    if !has_qwen {
        println!("⚠️  No Qwen Code model is configured.");
        println!("Add one with: klava providers add qwen-code");
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
    println!(
        "  Default model: {}",
        config
            .resolve_completion_model()
            .unwrap_or_else(|| "none".to_string())
    );
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
                    .or_else(|| {
                        enabled
                            .iter()
                            .position(|m| m.chat_id().contains("deepseek"))
                    })
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

    // Write the chosen model as a self-contained entry. Re-running the login
    // refreshes the entry in place; models from other providers are untouched.
    let base_url = format!("{}/api", auth.base_url());
    if let Some(model) = &model {
        let entry = klava::providers::Model {
            name: model.clone(),
            provider_type: Type::Klava,
            base_url: Some(base_url.clone()),
            ..klava::providers::Model::default()
        };
        match config
            .models
            .iter_mut()
            .find(|m| m.provider_type == Type::Klava && m.name == entry.name)
        {
            Some(existing) => existing.base_url = entry.base_url,
            None => config.models.push(entry),
        }
        klava::providers::set_defaults(&mut config, Some(model), Some(model));
    }
    config.save()?;

    match &model {
        Some(model) => println!("✓ Klava model '{model}' configured and set as the default."),
        None => println!("✓ Klava signed in."),
    }
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
    println!("  klava whoami        — account + balance");
    println!("  klava providers list");
    println!("  klava launch pi     — start an agent on the klava model");
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
            Confirm::new("Revoke the Klava API key on the server and sign out locally?")
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
                Command::Up {
                    port,
                    foreground,
                    no_provider_check,
                    search,
                    effort,
                } => {
                    assert_eq!(port, Some(8080));
                    assert!(foreground);
                    assert!(!no_provider_check);
                    assert!(!search);
                    assert!(effort.is_none());
                }
                _ => panic!("Expected up command"),
            }
        }
    }

    #[test]
    fn cli_parses_launch_no_service_flag() {
        let parsed = Cli::try_parse_from(["klava", "launch", "opencode", "--no-service"]);
        assert!(parsed.is_ok());

        if let Ok(cli) = parsed {
            match cli.command {
                Command::Launch {
                    no_service, agent, ..
                } => {
                    assert!(no_service);
                    assert!(agent.is_some());
                }
                _ => panic!("Expected launch command"),
            }
        }
    }

    #[test]
    fn cli_parses_launch_search_flags() {
        let parsed =
            Cli::try_parse_from(["klava", "launch", "opencode", "--search-model", "my/search"]);
        assert!(parsed.is_ok());
        if let Ok(cli) = parsed {
            match cli.command {
                Command::Launch {
                    search,
                    search_model,
                    ..
                } => {
                    assert!(!search);
                    assert_eq!(search_model.as_deref(), Some("my/search"));
                }
                _ => panic!("Expected launch command"),
            }
        }

        let parsed = Cli::try_parse_from(["klava", "launch", "opencode", "--search"]);
        assert!(parsed.is_ok());
        if let Ok(cli) = parsed {
            match cli.command {
                Command::Launch { search, .. } => assert!(search),
                _ => panic!("Expected launch command"),
            }
        }
    }

    #[test]
    fn cli_parses_launch_effort_and_maps_it_to_the_config_name() {
        let parsed = Cli::try_parse_from(["klava", "launch", "opencode", "--effort", "max"]);
        assert!(parsed.is_ok());
        if let Ok(cli) = parsed {
            match cli.command {
                Command::Launch { effort, .. } => {
                    assert_eq!(effort, Some(crate::cli::EffortArg::Max));
                    assert_eq!(effort.unwrap().as_str(), "max");
                }
                _ => panic!("Expected launch command"),
            }
        }

        // The value must be one of the wire levels; a typo is a clap error
        // instead of a silently ignored config value.
        let parsed = Cli::try_parse_from(["klava", "launch", "opencode", "--effort", "turbo"]);
        assert!(parsed.is_err());

        let parsed = Cli::try_parse_from(["klava", "up", "--effort", "xhigh"]);
        assert!(parsed.is_ok());
        if let Ok(cli) = parsed {
            match cli.command {
                Command::Up { effort, .. } => {
                    assert_eq!(effort.map(|e| e.as_str()), Some("xhigh"));
                }
                _ => panic!("Expected up command"),
            }
        }
    }

    #[test]
    fn cli_parses_restart_command() {
        let parsed = Cli::try_parse_from(["klava", "restart"]);
        assert!(parsed.is_ok());
        if let Ok(cli) = parsed {
            match cli.command {
                Command::Restart { port } => assert_eq!(port, None),
                _ => panic!("Expected restart command"),
            }
        }
    }

    #[test]
    fn cli_parses_restart_with_port() {
        let parsed = Cli::try_parse_from(["klava", "restart", "--port", "8085"]);
        assert!(parsed.is_ok());
        if let Ok(cli) = parsed {
            match cli.command {
                Command::Restart { port } => assert_eq!(port, Some(8085)),
                _ => panic!("Expected restart command"),
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
