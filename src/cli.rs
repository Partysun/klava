use clap::{Parser, Subcommand};
use klava::agents::CodeAgents;
use std::path::PathBuf;

// Centralized default values
const DEFAULT_PID_FILE: &str = "/tmp/klava.pid";

#[derive(Parser, Debug)]
#[command(
    name = "klava",
    version,
    about = "Klava is a CLI tool for using code agents with any provider. Use Claude Code with your OpenAI-compatible provider. Make code agents more secure by filtering out leaked secret keys and cryptographic keys from your filesystem.",
    long_about = "A versatile CLI tool that enables AI code agents to work with any compatible AI provider. \
                  Acts as a universal proxy translating API requests between different formats, \
                  supporting providers like OpenRouter, Qwen, and OpenAI-compatible services."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Enable verbose logging (logs full request/response bodies)
    #[arg(
        short,
        long,
        default_value_t = false,
        global = true,
        help = "Enable verbose logging (logs full request/response bodies)"
    )]
    pub verbose: bool,

    /// Run as background daemon
    #[arg(
        long,
        default_value_t = false,
        global = true,
        hide = true,
        help = "Run as background daemon"
    )]
    pub daemon: bool,

    /// PID file path (used with daemon commands)
    #[arg(long, hide=true, value_name = "FILE", default_value = DEFAULT_PID_FILE, global = true, help = "PID file path (used with daemon commands)")]
    pub pid_file: PathBuf,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Launch an AI code agent with proxy server
    ///
    /// This command starts the proxy server in the background and launches
    /// the specified AI code agent with appropriate environment variables.
    #[command(after_help = "Supported code agents:\n  \
                     claude    Claude Code (https://code.claude.com)\n  \
                     opencode  OpenCode Agent\n  \
                     pi        Pi Agent (https://pi.dev)\n\n \
                     Examples:\n  \
                     klava launch\n  \
                     klava launch claude\n  \
                     klava launch opencode\n  \
                     klava launch pi\n  \
                     klava launch claude --config")]
    Launch {
        #[arg(value_enum, help = "The code agent to launch")]
        agent: Option<CodeAgents>,

        /// Generate configuration without starting the agent
        #[arg(long, help = "Generate config without starting agents")]
        config: bool,

        /// Set the active provider before launching
        #[arg(
            long,
            value_name = "PROVIDER",
            help = "Set the active provider (e.g., qwen, openrouter)"
        )]
        provider: Option<String>,

        /// Port to listen on (overrides PORT env var and config)
        #[arg(
            short,
            long,
            value_name = "PORT",
            help = "Port to listen on (overrides PORT env var and config)"
        )]
        port: Option<u16>,

        /// Don't use the managed background service; spawn the proxy as a
        /// short-lived child process that stops when the agent exits.
        #[arg(
            long,
            help = "Spawn a child proxy instead of the managed background service"
        )]
        no_service: bool,

        /// Enable web search: the agent gets a search tool that klava runs for
        /// it (no agent-side setup needed).
        #[arg(long, help = "Enable web search for the launched agent")]
        search: bool,

        /// Model used to run web-search requests (implies --search).
        #[arg(
            long,
            value_name = "MODEL",
            help = "Model used for web-search requests (implies --search)"
        )]
        search_model: Option<String>,
    },
    /// Start the proxy server
    ///
    /// Starts the Anthropic to OpenAI proxy server that translates API requests.
    #[command(after_help = "Examples:\n  \
                     klava up\n  \
                     klava up --port 8085\n  \
                     klava up --foreground\n  \
                     klava up --daemon")]
    Up {
        /// Port to listen on (overrides PORT env var and config)
        #[arg(
            short,
            long,
            value_name = "PORT",
            help = "Port to listen on (overrides PORT env var and config)"
        )]
        port: Option<u16>,

        /// Run in foreground (don't daemonize)
        #[arg(
            long,
            default_value_t = true,
            help = "Run in foreground (don't daemonize)"
        )]
        foreground: bool,

        /// Skip the provider readiness check (used by the managed service to
        /// avoid interactive prompts and restart-loops inside systemd).
        #[arg(long, hide = true, help = "Skip the provider readiness check")]
        no_provider_check: bool,

        /// Enable web search (set by `klava launch --search` for the managed
        /// service; normally comes from the config file).
        #[arg(long, hide = true, help = "Enable web search")]
        search: bool,
    },

    /// Stop a running daemon
    ///
    /// Stops the proxy server if it's running as a daemon.
    #[command(
        hide = true,
        after_help = "Examples:\n  \
                     klava stop\n  \
                     klava stop --pid-file /tmp/custom.pid"
    )]
    Stop {
        /// PID file path
        #[arg(long, value_name = "FILE", help = "PID file path")]
        pid_file: Option<PathBuf>,
    },
    /// Restart the managed proxy service
    ///
    /// Restarts the background service for the configured port without
    /// rewriting its unit, re-enabling it, or prompting for providers.
    #[command(after_help = "Examples:\n  \
                     klava restart\n  \
                     klava restart --port 8085")]
    Restart {
        /// Port of the managed instance (defaults to the configured port)
        #[arg(
            short,
            long,
            value_name = "PORT",
            help = "Port of the managed instance (defaults to the configured port)"
        )]
        port: Option<u16>,
    },

    /// Check daemon status
    ///
    /// Checks if the proxy server daemon is currently running.
    #[command(
        hide = true,
        after_help = "Examples:\n  \
                     klava status\n  \
                     klava status --pid-file /tmp/custom.pid"
    )]
    Status {
        /// PID file path
        #[arg(long, value_name = "FILE", help = "PID file path")]
        pid_file: Option<PathBuf>,
    },

    /// View/tail proxy logs
    ///
    /// Displays logs from the proxy server.
    #[command(after_help = "Examples:\n  \
                     klava logs\n  \
                     klava logs -f")]
    Logs {
        /// Follow log output (like tail -f)
        #[arg(short, long, help = "Follow log output")]
        follow: bool,
    },

    /// Create or configure config file
    ///
    /// Creates the config file if it doesn't exist, or runs interactive setup.
    #[command(after_help = "Examples:\n  \
                     klava config\n  \
                     klava config --setup")]
    Config {
        /// Run interactive configuration setup
        #[arg(long, help = "Run interactive configuration setup")]
        setup: bool,
    },

    /// Manage API providers
    ///
    /// Switch between providers or manage provider-specific authentication.
    #[command(after_help = "Examples:\n  \
                     klava providers\n  \
                     klava providers set qwen-code\n  \
                     klava providers qwen-code login\n  \
                     klava providers qwen-code status")]
    Providers {
        /// Raw provider arguments for custom parsing
        #[arg(
            required = false,
            trailing_var_arg = true,
            help = "Provider name or subcommands"
        )]
        args: Vec<String>,
    },

    /// Install and manage extensions
    ///
    /// klava ships with no extensions. An extension is an executable discovered
    /// at runtime; klava only interprets the `enabled` key and passes the rest
    /// through untouched. An extension may declare a `[setup]` hook in its
    /// manifest; `--trust` consents to running it after the extension is enabled.
    #[command(after_help = "Examples:\n  \
                     klava extensions\n  \
                     klava extensions path\n  \
                     klava extensions install ./my-extension --trust\n  \
                     klava extensions install cargo:klava-compress --trust\n  \
                     klava extensions install cargo-path:./my-crate --trust\n  \
                     klava extensions install npm:my-ext --trust\n  \
                     klava extensions install cargo:klava-compress --trust --no-setup\n  \
                     klava extensions list\n  \
                     klava extensions verify\n  \
                     klava extensions setup klava-compress\n  \
                     klava extensions enable <name>\n  \
                     klava extensions remove <name>")]
    Extensions {
        /// Subcommand: list (default), path, install, remove, verify, show, setup, enable, disable, set
        #[arg(
            required = false,
            trailing_var_arg = true,
            help = "Action and arguments"
        )]
        args: Vec<String>,
    },

    /// Check configuration and provider connectivity
    ///
    /// Sends a small, cheap test request to your configured LLM API
    /// to verify everything is working.
    #[command(after_help = "Examples:\n  \
                     klava check\n  \
                     klava check --provider openrouter")]
    Check {
        /// Provider to check (defaults to active provider)
        #[arg(long, value_name = "PROVIDER", help = "Provider to check")]
        provider: Option<String>,

        /// Model to use for the check
        #[arg(short, long, value_name = "MODEL", help = "Model to check with")]
        model: Option<String>,
    },

    /// Sign in to the Klava provider (device OAuth flow)
    ///
    /// Opens a browser for authorization and saves the minted API key
    /// locally; also configures the default model from the catalog.
    #[command(after_help = "Examples:\n  \
                     klava login\n  \
                     klava login --force")]
    Login {
        /// Re-authenticate even if already signed in
        #[arg(long, help = "Re-authenticate even if already signed in")]
        force: bool,
    },

    /// Sign out from the Klava provider and revoke the API key
    #[command(after_help = "Examples:\n  \
                     klava logout\n  \
                     klava logout --force")]
    Logout {
        /// Skip the confirmation prompt
        #[arg(long, help = "Skip the confirmation prompt")]
        force: bool,
    },

    /// Show the current Klava session (account, key, balance)
    #[command(after_help = "Examples:\n  klava whoami")]
    Whoami {},

    /// Show the Klava account balance
    #[command(after_help = "Examples:\n  klava balance")]
    Balance {},
}
