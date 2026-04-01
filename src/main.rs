mod cli;
mod daemon;

use clap::Parser;
use cli::{Cli, Command};
use daemon::{check_status, prepare_daemon, stop_daemon};
use klava::agents::CodeAgents;
use klava::config;
use klava::error::Error;
use klava::hooks::default_chain;
use klava::server;
use klava::telemetry::{check_for_update, send_install_event};
use reqwest::Client;
use std::sync::Arc;

/// Build a reqwest client with OpenRouter headers configured
fn build_http_client() -> Result<Client, anyhow::Error> {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        "X-OpenRouter-Title",
        reqwest::header::HeaderValue::from_static("Klava"),
    );
    headers.insert(
        "X-OpenRouter-Categories",
        reqwest::header::HeaderValue::from_static("cli-agent-proxy"),
    );

    Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .connect_timeout(std::time::Duration::from_secs(10))
        .pool_max_idle_per_host(10)
        .default_headers(headers)
        .build()
        .map_err(|e| anyhow::anyhow!("Failed to build HTTP client: {}", e))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let current_version = env!("CARGO_PKG_VERSION");

    // Send telemetry on first run (before config is loaded)
    // This will only send if config file doesn't exist yet
    let _ = send_install_event(current_version).await;

    // Check for updates
    if let Ok(update_available) = check_for_update(current_version).await {
        if update_available {
            eprintln!("🔄 A new version of klava is available! Check for updates.");
        }
    }

    let cli = Cli::parse();

    let mut config = config::Config::from_config_and_env()?;

    let hook_chain = Arc::new(default_chain());

    match cli.command {
        Command::Launch { agent, .. } => {
            use inquire::Select;

            // Use interactive selection if agent is not provided
            let agent = match agent {
                Some(a) => a,
                None => {
                    let options = CodeAgents::all_variants();
                    let options_strings: Vec<String> = options
                        .iter()
                        .map(|a| format!("{} - {}", a.name(), a.description()))
                        .collect();

                    let selected = Select::new("Select code agent to launch:", options_strings)
                        .without_help_message()
                        .prompt()
                        .map_err(|e| anyhow::anyhow!("Failed to select agent: {}", e))?;

                    // Parse back the selected agent
                    if selected.starts_with("claude") {
                        CodeAgents::Claude
                    } else {
                        CodeAgents::Claude
                    }
                }
            };

            agent.check_installation()?;

            // TODO: localhost must be a hostname in feature
            // Set environment variables and launch Claude CLI using async agent.run()
            let proxy_url = format!("http://localhost:{}", config.port);

            if let Err(e) = agent.setup(&proxy_url).await {
                return Err(
                    Error::Internal(format!("Failed to setup {}: {}", &agent.name(), e)).into(),
                );
            }

            //TODO: we need to have an option to start agent without proxy server
            // Start proxy server first
            let config_clone = config.clone();
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
        Command::Logs { follow } => handle_logs_command(follow),
        Command::Config { create } => handle_config_command(create),
    }
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

/// Handle config command - create config file and show path
fn handle_config_command(force: bool) -> anyhow::Result<()> {
    use klava::error::Error;

    config::Config::ensure_exists(force)
        .map_err(|e| Error::Internal(format!("Failed to create config: {}", e)))?;

    Ok(())
}

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
                Command::Config { create } => {
                    assert!(!create);
                }
                _ => panic!("Expected config command"),
            }
        }
    }

    #[test]
    fn cli_parses_config_command_with_create_flag() {
        let parsed = Cli::try_parse_from(["klava", "config", "--create"]);
        assert!(parsed.is_ok());

        if let Ok(cli) = parsed {
            match cli.command {
                Command::Config { create } => {
                    assert!(create);
                }
                _ => panic!("Expected config command"),
            }
        }
    }
}
