use crate::agents::AgentRunner;
use tokio::process::Command as TokioCommand;
use which::which;

pub struct ClaudeRunner;

impl ClaudeRunner {
    pub const fn name_static() -> &'static str {
        "claude"
    }

    pub fn new() -> Self {
        Self
    }
}

impl AgentRunner for ClaudeRunner {
    fn name(&self) -> &'static str {
        Self::name_static()
    }

    fn check_installation() -> Result<(), anyhow::Error> {
        which(Self::name_static()).map_err(|_| {
            anyhow::anyhow!(
                "claude is not installed. Install from https://code.claude.com/docs/en/quickstart"
            )
        })?;
        Ok(())
    }

    fn run(
        &self,
        _args: &[String],
        proxy_url: &str,
    ) -> impl std::future::Future<Output = Result<(), anyhow::Error>> + Send {
        async move {
            let mut command = TokioCommand::new(self.name());

            // Set environment variables for Claude
            command
                .env("ANTHROPIC_BASE_URL", proxy_url)
                .env("ANTHROPIC_AUTH_TOKEN", "")
                .env("ANTHROPIC_API_KEY", "")
                .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
                .env("IS_DEMO", "1");

            let mut child = command
                .spawn()
                .map_err(|e| anyhow::anyhow!("Failed to launch Claude CLI: {}", e))?;

            let status = child
                .wait()
                .await
                .map_err(|e| anyhow::anyhow!("Failed to wait for Claude CLI: {}", e))?;

            if !status.success() {
                anyhow::bail!("Claude CLI exited with status: {:?}", status.code());
            }

            Ok(())
        }
    }
}
