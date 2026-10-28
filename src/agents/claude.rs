use crate::agents::AgentRunner;
use crate::utils::{ask_user_approval, generate_diff, write_with_backup};
use std::path::PathBuf;
use tokio::process::Command as TokioCommand;
use which::which;

pub struct ClaudeRunner;

impl Default for ClaudeRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl ClaudeRunner {
    pub const fn name_static() -> &'static str {
        "claude"
    }

    pub fn new() -> Self {
        Self
    }

    /// Claude's config directory: `CLAUDE_CONFIG_DIR` or `~/.claude`.
    fn config_dir() -> Option<PathBuf> {
        if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR")
            && !dir.is_empty()
        {
            return Some(PathBuf::from(dir));
        }
        dirs::home_dir().map(|home| home.join(".claude"))
    }

    /// Path of Claude's user-level settings.json.
    fn settings_config_path() -> Option<PathBuf> {
        Self::config_dir().map(|dir| dir.join("settings.json"))
    }

    /// Environment entries that point Claude at klava and at the configured
    /// models. This is the single source of truth for both `run()` (exported
    /// into the child process) and `setup()` (persisted to settings.json), so
    /// the two cannot drift apart.
    ///
    /// Empty when no config is loaded, leaving the existing environment as-is.
    fn model_env() -> Vec<(&'static str, String)> {
        let Ok(config) = crate::config::Config::load() else {
            return Vec::new();
        };
        let completion = config.resolve_completion_model();
        let reasoning = config.resolve_reasoning_model();
        // The heavy aliases (opus/sonnet) should land on the reasoning model;
        // the cheap ones (haiku/small-fast) on the completion model. Either
        // falls back to the other so a single-model config still works.
        let heavy = reasoning.as_deref().or(completion.as_deref());
        let cheap = completion.as_deref().or(reasoning.as_deref());

        let mut env = Vec::new();
        if let Some(model) = heavy {
            env.push(("ANTHROPIC_DEFAULT_OPUS_MODEL", model.to_string()));
            env.push(("ANTHROPIC_DEFAULT_SONNET_MODEL", model.to_string()));
        }
        if let Some(model) = cheap {
            env.push(("ANTHROPIC_DEFAULT_HAIKU_MODEL", model.to_string()));
            env.push(("ANTHROPIC_SMALL_FAST_MODEL", model.to_string()));
        }
        env
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

    fn paths(&self) -> Vec<PathBuf> {
        Self::settings_config_path().into_iter().collect()
    }

    /// Persist the klava connection and model mapping into Claude's user-level
    /// settings.json.
    ///
    /// `run()` already exports these as environment variables, so a plain
    /// `klava launch claude` works without this. The file matters for
    /// `--config` (which never launches Claude) and for users who start
    /// `claude` on its own: the `env` block makes every Claude session pick up
    /// the same base URL and model mapping without going through klava's
    /// launcher. Unrelated settings are preserved.
    async fn setup(&self, proxy_url: &str) -> Result<(), anyhow::Error> {
        let settings_path = Self::settings_config_path()
            .ok_or_else(|| anyhow::anyhow!("Could not find Claude settings.json config path"))?;

        let old_settings_str = if settings_path.exists() {
            tokio::fs::read_to_string(&settings_path)
                .await
                .unwrap_or_default()
        } else {
            String::new()
        };

        let mut settings: serde_json::Map<String, serde_json::Value> =
            if old_settings_str.trim().is_empty() {
                serde_json::Map::new()
            } else {
                serde_json::from_str(&old_settings_str).unwrap_or_default()
            };

        // Merge into an existing `env` block rather than replacing it: users
        // (and other tools) keep their own variables.
        let mut env = settings
            .get("env")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();

        env.insert(
            "ANTHROPIC_BASE_URL".to_string(),
            serde_json::json!(proxy_url),
        );
        env.insert(
            "ANTHROPIC_AUTH_TOKEN".to_string(),
            serde_json::json!("klava"),
        );
        for (key, value) in Self::model_env() {
            env.insert(key.to_string(), serde_json::json!(value));
        }

        settings.insert("env".to_string(), serde_json::Value::Object(env));

        let new_settings_str = serde_json::to_string_pretty(&settings)?;
        if new_settings_str == old_settings_str {
            println!(
                "Claude settings already up to date: {}",
                settings_path.display()
            );
            return Ok(());
        }

        if old_settings_str.is_empty() {
            println!(
                "\nNew configuration will be created at {}:",
                settings_path.display()
            );
            println!("{}", new_settings_str);
        } else {
            let diff = generate_diff(&old_settings_str, &new_settings_str, &settings_path);
            println!("\nProposed changes to {}:", settings_path.display());
            println!("{}", diff);
        }

        if !ask_user_approval("Do you want to apply these changes?")? {
            println!("Changes cancelled.");
            return Ok(());
        }

        write_with_backup(&settings_path, new_settings_str.as_bytes())
            .map_err(|e| anyhow::anyhow!("Failed to write {}: {}", settings_path.display(), e))?;
        println!("✓ Configuration updated: {}", settings_path.display());
        Ok(())
    }

    async fn run(&self, _args: &[String], proxy_url: &str) -> Result<(), anyhow::Error> {
        let mut command = TokioCommand::new(self.name());

        // Set environment variables for Claude
        command
            .env("ANTHROPIC_BASE_URL", proxy_url)
            .env("ANTHROPIC_AUTH_TOKEN", "klava")
            .env("ANTHROPIC_API_KEY", "")
            .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
            .env("CLAUDE_CODE_ATTRIBUTION_HEADER", "0")
            .env("IS_DEMO", "1");

        // Point Claude's built-in model aliases at the configured models.
        //
        // Without this, Claude sends ids like `claude-opus-5-5` which klava
        // only accepts by falling back to the default model. That works, but
        // it makes `/model` misleading (the picker lists Anthropic models that
        // do not exist upstream) and it hides which model is actually serving
        // the request. Mapping the aliases keeps the picker honest: the heavy
        // aliases resolve to the reasoning model, the cheap ones to the
        // completion model.
        for (key, value) in Self::model_env() {
            command.env(key, value);
        }
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
