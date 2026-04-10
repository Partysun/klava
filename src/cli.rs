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
                     opencode  OpenCode Agent\n\n \
                     Examples:\n  \
                     klava launch\n  \
                     klava launch claude\n  \
                     klava launch opencode\n  \
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
                     klava providers set qwen-free\n  \
                     klava providers qwen-free login\n  \
                     klava providers qwen-free status")]
    Providers {
        /// Raw provider arguments for custom parsing
        #[arg(
            required = false,
            trailing_var_arg = true,
            help = "Provider name or subcommands"
        )]
        args: Vec<String>,
    },
}
