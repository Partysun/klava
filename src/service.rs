//! Self-provisioning managed service.
//!
//! On Linux with a systemd user session, the proxy is managed as a per-port
//! user unit (`klava-<port>.service`):
//!
//! * `ensure_service` converges the unit (writes it when missing or stale —
//!   binary path or port changed), enables it for login auto-start and
//!   starts it. The unit carries `Restart=always`, so the proxy heals itself
//!   and survives terminals, agent sessions and reboots.
//! * `status` reports every managed instance.
//! * `stop_service` stops one instance.
//!
//! When systemd is unavailable (containers, macOS, Windows) callers fall back
//! to the legacy spawned/daemonized proxy.

use anyhow::{anyhow, Context, Result};
use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use klava::config::Config;

/// Marker written into generated unit files so we can tell ours apart and
/// rewrite them when the binary or the port changes.
pub const UNIT_MARKER: &str = "# Managed by klava - do not edit manually";

/// Name of the unit that manages the proxy on `port`.
pub fn unit_name(port: u16) -> String {
    format!("klava-{port}.service")
}

/// Absolute path of the unit that manages the proxy on `port`.
pub fn unit_path(port: u16) -> PathBuf {
    dirs::config_dir()
        .map(|dir| dir.join("systemd").join("user"))
        .unwrap_or_else(|| PathBuf::from("."))
        .join(unit_name(port))
}

/// Directory containing all managed klava units.
pub fn units_dir() -> PathBuf {
    dirs::config_dir()
        .map(|dir| dir.join("systemd").join("user"))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The binary that should own the managed service.
pub fn exe_path() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("klava"))
}

/// Environment entries (api_key_name vars that are actually set) so the
/// managed service process sees the same keys as the launching shell.
/// Only OpenAI-compatible providers are included: the klava and qwen
/// providers read their credentials from their own local auth stores.
fn env_entries(config: &Config) -> Vec<(String, String)> {
    config
        .providers
        .iter()
        .filter(|p| matches!(p.provider_type, klava::providers::Type::OpenAICompatible))
        .filter_map(|p| {
            let name = p.api_key_name.as_ref()?;
            let value = std::env::var(name).ok()?;
            if value.is_empty() {
                None
            } else {
                Some((name.clone(), value))
            }
        })
        .collect()
}

/// Render the systemd user unit for a proxy on `port`.
pub fn render_unit(exe: &Path, port: u16, env: &[(String, String)]) -> String {
    let mut unit = format!(
        "{UNIT_MARKER}\n\
         [Unit]\n\
         Description=klava proxy (port {port})\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={} up --port {port} --foreground --no-provider-check\n",
        exe.display(),
    );
    for (key, value) in env {
        // Values may contain spaces/quotes; systemd Environment= accepts
        // simple `KEY=value` pairs and we only ever inject env var values.
        unit.push_str(&format!("Environment={}={}\n", key, value));
    }
    unit.push_str(
        "Restart=always\n\
         RestartSec=2\n\
         StartLimitIntervalSec=30\n\
         StartLimitBurst=10\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
    );
    unit
}

/// Whether a systemd user session is usable from this process.
pub fn systemd_available() -> bool {
    if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
        return false;
    }
    let Ok(systemctl) = which::which("systemctl") else {
        return false;
    };
    match Command::new(systemctl).args(["--user", "is-system-running"]).output() {
        Ok(out) => out.status.success(),
        Err(_) => false,
    }
}

fn run_systemctl(args: &[&str]) -> Result<std::process::Output> {
    Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .with_context(|| format!("failed to run systemctl --user {}", args.join(" ")))
}

fn is_active(port: u16) -> bool {
    match run_systemctl(&["is-active", &unit_name(port)]) {
        Ok(out) => String::from_utf8_lossy(&out.stdout).trim() == "active",
        Err(_) => false,
    }
}

/// Stop a legacy klava daemon (PID file from the old `--daemon` path) if the
/// process is alive, so the managed service can take over the port.
fn stop_legacy_daemon() {
    let pid_path = Path::new("/tmp/klava.pid");
    let Ok(content) = fs::read_to_string(pid_path) else {
        return;
    };
    let Ok(pid) = content.trim().parse::<i32>() else {
        let _ = fs::remove_file(pid_path);
        return;
    };
    let alive = Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if alive {
        tracing::info!("Stopping legacy klava daemon (PID {pid}) to free the port");
        let _ = Command::new("kill").arg(pid.to_string()).output();
    }
    let _ = fs::remove_file(pid_path);
}

/// True when something already accepts connections on the port — i.e. an
/// ad-hoc foreground `klava up` outside of our management.
fn port_in_use(port: u16) -> bool {
    // Binding 127.0.0.1 catches a local listener; the proxy itself binds all
    // interfaces, so a free loopback port implies the service port is free.
    TcpListener::bind(("127.0.0.1", port)).is_err()
}

/// Poll `GET /health` until the proxy answers or `timeout` elapses.
async fn wait_health(port: u16, timeout: Duration) -> Result<()> {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_millis(500))
        .timeout(Duration::from_millis(500))
        .build()?;
    let url = format!("http://127.0.0.1:{port}/health");
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        match client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => return Ok(()),
            _ => tokio::time::sleep(Duration::from_millis(250)).await,
        }
    }
    Err(anyhow!(
        "proxy service did not become healthy within {}s",
        timeout.as_secs()
    ))
}

/// What the convergence step ended up doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceAction {
    /// Unit did not exist; created, enabled and started.
    Installed,
    /// Unit existed but was stale; rewritten and restarted.
    Rewritten,
    /// Unit already active and current.
    AlreadyActive,
    /// Unit was current but not running; started.
    Started,
}

/// Converge the managed service for `config.port`: create/refresh the unit,
/// enable auto-start at login and make sure it is running and healthy.
pub async fn ensure_service(config: &Config) -> Result<ServiceAction> {
    let port = config.port;
    let exe = exe_path();
    let env = env_entries(config);
    let expected = render_unit(&exe, port, &env);
    let path = unit_path(port);
    let name = unit_name(port);

    if is_active(port) && path.exists() && fs::read_to_string(&path).ok().as_deref() == Some(&expected)
    {
        return Ok(ServiceAction::AlreadyActive);
    }

    let existed = path.exists();
    let needs_write = !existed || fs::read_to_string(&path).ok().as_deref() != Some(&expected);

    // Occupy the port before rewriting: a stale unit may point at an old
    // binary that still runs.
    if needs_write {
        if !is_active(port) {
            stop_legacy_daemon();
            if port_in_use(port) {
                return Err(anyhow!(
                    "port {port} is already bound by another process (e.g. `klava up` \
                     in a terminal or another tool). Stop it first, then retry."
                ));
            }
            let _ = run_systemctl(&["stop", &name]); // stop old instance, if any
        }
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(&path, &expected)
            .with_context(|| format!("failed to write unit file {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
        }
        run_systemctl(&["daemon-reload"])
            .context("systemctl --user daemon-reload failed")?;
        if existed {
            tracing::info!("Refreshed {}", name);
            let _ = run_systemctl(&["reset-failed", &name]);
        }
    }

    // Auto-start at login (idempotent; harmless when already enabled).
    let _ = run_systemctl(&["enable", &name]);

    let start = run_systemctl(&["start", &name])?;
    if !start.status.success() {
        let stderr = String::from_utf8_lossy(&start.stderr).trim().to_string();
        return Err(anyhow!(
            "failed to start {name}: {stderr}\n\
             Check the unit log with: journalctl --user -u {name} -n 50 --no-pager"
        ));
    }

    wait_health(port, Duration::from_secs(10)).await.map_err(|e| {
        anyhow!(
            "{e}\nThe unit may be crash-looping. Inspect it with:\n  \
             journalctl --user -u {name} -n 50 --no-pager"
        )
    })?;

    Ok(if !existed {
        ServiceAction::Installed
    } else if needs_write {
        ServiceAction::Rewritten
    } else {
        ServiceAction::Started
    })
}

/// Stop a managed instance (used by `klava stop`). Errors are ignored so a
/// missing unit falls back to the legacy PID-based stop.
pub fn stop_service(port: u16) {
    let _ = run_systemctl(&["stop", &unit_name(port)]);
}

/// Report the state of every managed instance.
pub fn status() -> Result<()> {
    let dir = units_dir();
    if !dir.exists() {
        println!("No managed klava service instances.");
        return Ok(());
    }
    let mut found = false;
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("klava-") || !name.ends_with(".service") {
            continue;
        }
        found = true;
        let state = match run_systemctl(&["is-active", &name]) {
            Ok(out) => String::from_utf8_lossy(&out.stdout).trim().to_string(),
            Err(_) => "unknown".to_string(),
        };
        let enabled = match run_systemctl(&["is-enabled", &name]) {
            Ok(out) => String::from_utf8_lossy(&out.stdout).trim().to_string(),
            Err(_) => "unknown".to_string(),
        };
        let icon = if state == "active" { "✓" } else { "✗" };
        println!("{icon} {name}  [{state}, {enabled}]");
    }
    if !found {
        println!("No managed klava service instances. `klava launch` creates one per port.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_name_embeds_port() {
        assert_eq!(unit_name(48017), "klava-48017.service");
        assert_eq!(unit_name(48018), "klava-48018.service");
    }

    #[test]
    fn unit_path_is_under_user_systemd_dir() {
        let p = unit_path(48017);
        assert!(p.to_string_lossy().ends_with("systemd/user/klava-48017.service"));
    }

    #[test]
    fn render_unit_has_marker_port_and_no_provider_check() {
        let unit = render_unit(Path::new("/opt/klava"), 48017, &[]);
        assert!(unit.contains(UNIT_MARKER));
        assert!(unit.contains("Description=klava proxy (port 48017)"));
        assert!(unit.contains("ExecStart=/opt/klava up --port 48017 --foreground --no-provider-check"));
        assert!(unit.contains("Restart=always"));
        assert!(unit.contains("RestartSec=2"));
        assert!(unit.contains("WantedBy=default.target"));
    }

    #[test]
    fn render_unit_includes_environment_entries() {
        let unit = render_unit(
            Path::new("/opt/klava"),
            48017,
            &[("OPENROUTER_API_KEY".to_string(), "sk-123".to_string())],
        );
        assert!(unit.contains("Environment=OPENROUTER_API_KEY=sk-123"));
    }

    #[test]
    fn env_entries_only_include_set_vars() {
        let mut config = Config::default();
        config.providers = vec![
            klava::providers::Config {
                name: "with-env".into(),
                provider_type: klava::providers::Type::OpenAICompatible,
                base_url: None,
                chat_completions_path: None,
                api_key: None,
                api_key_name: Some("KLAVA_TEST_AUTH_KEY_A".into()),
                reasoning_model: None,
                completion_model: None,
            },
            klava::providers::Config {
                name: "no-env".into(),
                provider_type: klava::providers::Type::OpenAICompatible,
                base_url: None,
                chat_completions_path: None,
                api_key: Some("inline".into()),
                api_key_name: None,
                reasoning_model: None,
                completion_model: None,
            },
        ];
        unsafe {
            std::env::set_var("KLAVA_TEST_AUTH_KEY_A", "set-value");
        }
        let entries = env_entries(&config);
        unsafe {
            std::env::remove_var("KLAVA_TEST_AUTH_KEY_A");
        }
        assert_eq!(
            entries,
            vec![("KLAVA_TEST_AUTH_KEY_A".to_string(), "set-value".to_string())]
        );
    }
}