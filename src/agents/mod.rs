mod claude;

#[cfg(feature = "opencode")]
mod opencode;

pub use claude::ClaudeRunner;

#[cfg(feature = "opencode")]
pub use opencode::OpencodeRunner;

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

pub trait AgentRunner {
    fn name(&self) -> &'static str;
    fn check_installation() -> Result<(), anyhow::Error>;
    fn run(
        &self,
        args: &[String],
        proxy_url: &str,
    ) -> impl std::future::Future<Output = Result<(), anyhow::Error>> + Send;

    /// Setup configuration before running (idempotent)
    fn setup(
        &self,
        _proxy_url: &str,
    ) -> impl std::future::Future<Output = Result<(), anyhow::Error>> + Send {
        async move { Ok(()) }
    }

    /// Get configuration file paths
    fn paths(&self) -> Vec<std::path::PathBuf> {
        Vec::new()
    }
}

#[derive(ValueEnum, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum CodeAgents {
    Claude,
    #[cfg(feature = "opencode")]
    Opencode,
}

impl CodeAgents {
    pub fn name(&self) -> &'static str {
        match self {
            CodeAgents::Claude => ClaudeRunner::name_static(),
            #[cfg(feature = "opencode")]
            CodeAgents::Opencode => OpencodeRunner::name_static(),
        }
    }

    pub fn description(&self) -> &'static str {
        match self {
            CodeAgents::Claude => "Claude Code (https://code.claude.com)",
            #[cfg(feature = "opencode")]
            CodeAgents::Opencode => "OpenCode Agent",
        }
    }

    pub fn all_variants() -> Vec<Self> {
        let variants = vec![CodeAgents::Claude];
        #[cfg(feature = "opencode")]
        let variants = {
            let mut v = variants;
            v.push(CodeAgents::Opencode);
            v
        };
        variants
    }

    pub fn check_installation(&self) -> Result<(), anyhow::Error> {
        match self {
            CodeAgents::Claude => ClaudeRunner::check_installation(),
            #[cfg(feature = "opencode")]
            CodeAgents::Opencode => OpencodeRunner::check_installation(),
        }
    }

    pub async fn run(&self, args: &[String], proxy_url: &str) -> Result<(), anyhow::Error> {
        match self {
            CodeAgents::Claude => ClaudeRunner::new().run(args, proxy_url).await,
            #[cfg(feature = "opencode")]
            CodeAgents::Opencode => OpencodeRunner::new().run(args, proxy_url).await,
        }
    }

    pub async fn setup(&self, proxy_url: &str) -> Result<(), anyhow::Error> {
        match self {
            CodeAgents::Claude => ClaudeRunner::new().setup(proxy_url).await,
            #[cfg(feature = "opencode")]
            CodeAgents::Opencode => OpencodeRunner::new().setup(proxy_url).await,
        }
    }
}
