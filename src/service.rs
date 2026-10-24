//! Self-provisioning managed service.
//!
//! `ensure_service` converges a per-port, auto-starting, self-healing
//! background service for the proxy:
//!
//! * **Linux (systemd user session)** — a per-port user unit
//!   `klava-<port>.service` with `Restart=always`, enabled for login.
//! * **macOS (launchd)** — a per-port LaunchAgent
//!   `~/Library/LaunchAgents/klava-<port>.plist` with `KeepAlive` and
//!   `RunAtLoad`.
//!
//! In both cases the file is rewritten when it is missing or stale (binary
//! path or port changed), then loaded idempotently and health-checked
//! (`GET /health`). The proxy survives terminals, agent sessions, crashes
//! and reboots.
//!
//! When neither manager is available (containers, Windows) callers fall back
//! to the legacy spawned/daemonized proxy.

use anyhow::{Context, Result, anyhow};
use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use klava::config::Config;

/// Marker written into generated unit/plist files so we can tell ours apart
/// and rewrite them when the binary or the port changes.
pub const UNIT_MARKER: &str = "# Managed by klava - do not edit manually";

/// Base name shared by the systemd unit and the launchd label for `port`.
pub fn instance_name(port: u16) -> String {
    format!("klava-{port}")
}

/// Name of the systemd unit that manages the proxy on `port`.
pub fn unit_name(port: u16) -> String {
    format!("{}.service", instance_name(port))
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
        .models
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
pub fn render_unit(exe: &Path, port: u16, env: &[(String, String)], search: bool) -> String {
    let search_arg = if search { " --search" } else { "" };
    let mut unit = format!(
        "{UNIT_MARKER}\n\
         [Unit]\n\
         Description=klava proxy (port {port})\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={} up --port {port} --foreground --no-provider-check{search_arg}\n",
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
    let Ok(out) = Command::new(systemctl)
        .args(["--user", "is-system-running"])
        .output()
    else {
        return false;
    };

    // `is-system-running` exits non-zero for every state except `running`, but
    // `degraded`, `starting` and `maintenance` all mean the user manager is
    // reachable and can start units. Only an unreachable manager (no bus) is
    // unusable, and it prints no state at all.
    let state = String::from_utf8_lossy(&out.stdout);
    let state = state.trim();
    matches!(
        state,
        "running" | "degraded" | "starting" | "maintenance" | "initializing"
    )
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

/// Whether a supported service manager is usable from this process.
pub fn manager_available() -> bool {
    systemd_available() || launchd_available()
}

/// Whether a launchd user session is usable from this process (macOS).
#[cfg(target_os = "macos")]
pub fn launchd_available() -> bool {
    which::which("launchctl").is_ok()
}

#[cfg(not(target_os = "macos"))]
pub fn launchd_available() -> bool {
    false
}

/// Converge the managed service for `config.port`: create/refresh the
/// systemd unit or launchd LaunchAgent, enable auto-start at login and make
/// sure it is running and healthy.
pub async fn ensure_service(config: &Config) -> Result<ServiceAction> {
    if systemd_available() {
        ensure_systemd_service(config).await
    } else if launchd_available() {
        ensure_launchd_service(config).await
    } else {
        Err(anyhow!(
            "No supported service manager found (systemd/launchd). \
             Run `klava up` in a terminal, or `klava launch --no-service`."
        ))
    }
}

/// Converge the per-port systemd user unit.
async fn ensure_systemd_service(config: &Config) -> Result<ServiceAction> {
    let port = config.port;
    let exe = exe_path();
    let env = env_entries(config);
    let expected = render_unit(&exe, port, &env, config.enable_websearch);
    let path = unit_path(port);
    let name = unit_name(port);

    if is_active(port)
        && path.exists()
        && fs::read_to_string(&path).ok().as_deref() == Some(expected.as_str())
    {
        return Ok(ServiceAction::AlreadyActive);
    }

    let existed = path.exists();
    let needs_write =
        !existed || fs::read_to_string(&path).ok().as_deref() != Some(expected.as_str());

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
        run_systemctl(&["daemon-reload"]).context("systemctl --user daemon-reload failed")?;
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

    wait_health(port, Duration::from_secs(10))
        .await
        .map_err(|e| {
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
/// missing instance falls back to the legacy PID-based stop.
pub fn stop_service(port: u16) {
    if systemd_available() {
        let _ = run_systemctl(&["stop", &unit_name(port)]);
    } else if launchd_available() {
        let _ = launchctl_unload(&launchd_plist_path(port));
    }
}

/// Restart the managed instance for `port`, keeping its configuration.
///
/// This is deliberately narrower than `stop` + `up`: the unit/plist already
/// exists, so a restart must not rewrite it, re-enable it, or prompt for
/// providers. It stops the running job and starts it again, then waits for
/// health so the caller learns about a crash-loop instead of a false "ok".
///
/// Errors are reported rather than swallowed: unlike [`stop_service`], a failed
/// restart is something the user asked about and must see.
pub async fn restart_service(port: u16) -> Result<()> {
    if systemd_available() {
        restart_systemd(port).await
    } else if launchd_available() {
        restart_launchd(port).await
    } else {
        Err(anyhow!(
            "No supported service manager found (systemd/launchd), so there is \
             no managed klava instance to restart.\nRun `klava up` to start one."
        ))
    }
}

async fn restart_systemd(port: u16) -> Result<()> {
    let name = unit_name(port);
    let path = unit_path(port);

    if !path.exists() {
        return Err(anyhow!(
            "no managed service for port {port} ({name} is missing).\n\
             Start one with `klava up`, which creates it for the configured port."
        ));
    }

    // Clear a crash-loop lockout before restarting, otherwise systemd refuses.
    let _ = run_systemctl(&["reset-failed", &name]);

    let out = run_systemctl(&["restart", &name])?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(anyhow!(
            "failed to restart {name}: {stderr}\n\
             Inspect it with: journalctl --user -u {name} -n 50 --no-pager"
        ));
    }

    wait_health(port, Duration::from_secs(10))
        .await
        .map_err(|e| {
            anyhow!(
                "{e}\nThe unit may be crash-looping. Inspect it with:\n  \
             journalctl --user -u {name} -n 50 --no-pager"
            )
        })?;

    Ok(())
}

async fn restart_launchd(port: u16) -> Result<()> {
    let path = launchd_plist_path(port);
    if !path.exists() {
        return Err(anyhow!(
            "no managed service for port {port} ({} is missing).\n\
             Start one with `klava up`, which creates it for the configured port.",
            path.display()
        ));
    }

    // launchd has no atomic restart; unload then load is the documented way.
    launchctl_unload(&path)?;
    launchctl_load(&path)?;

    wait_health(port, Duration::from_secs(10))
        .await
        .map_err(|e| anyhow!("{e}\nInspect the log with: {}", logs_command(port)))?;

    Ok(())
}

/// Hint for tailing the logs of the managed instance on this platform.
pub fn logs_command(port: u16) -> String {
    #[cfg(target_os = "macos")]
    {
        let log = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("~"))
            .join("Library/Logs")
            .join(format!("{}.log", instance_name(port)));
        format!("tail -f {}", log.display())
    }
    #[cfg(not(target_os = "macos"))]
    {
        format!("journalctl --user -u {} -f", unit_name(port))
    }
}

/// State of the managed service for one port, as the service manager sees it.
///
/// Query only: nothing here starts, stops or rewrites a unit. `klava check`
/// uses it to say *which* layer is broken when a request fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceState {
    /// Name of the unit/agent, e.g. `klava-48017.service`.
    pub unit: String,
    /// Whether a unit/agent file exists at all.
    pub installed: bool,
    /// `active`, `inactive`, `failed`, … or `absent`.
    pub state: String,
    /// `enabled`, `disabled`, … or `unknown`; on launchd, `loaded`/`unloaded`.
    pub enabled: String,
}

impl InstanceState {
    /// Whether the service manager reports the instance as running.
    pub fn is_active(&self) -> bool {
        matches!(self.state.as_str(), "active" | "running")
    }
}

/// Read the service-manager state for `port` without touching it.
pub fn instance_state(port: u16) -> InstanceState {
    if systemd_available() {
        return systemd_instance_state(port);
    }
    if launchd_available() {
        return launchd_instance_state(port);
    }
    InstanceState {
        unit: instance_name(port),
        installed: false,
        state: "absent".into(),
        enabled: "unknown".into(),
    }
}

fn systemd_instance_state(port: u16) -> InstanceState {
    let name = unit_name(port);
    let installed = unit_path(port).exists();
    if !installed {
        return InstanceState {
            unit: name,
            installed: false,
            state: "absent".into(),
            enabled: "unknown".into(),
        };
    }
    // A unit file that has never been loaded reports `inactive`, which is
    // true but reads like a crash; callers only care that it is not running.
    let state = match run_systemctl(&["is-active", &name]) {
        Ok(out) => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        Err(_) => "unknown".into(),
    };
    let enabled = match run_systemctl(&["is-enabled", &name]) {
        Ok(out) => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        Err(_) => "unknown".into(),
    };
    InstanceState {
        unit: name,
        installed,
        state,
        enabled,
    }
}

fn launchd_instance_state(port: u16) -> InstanceState {
    let label = instance_name(port);
    let installed = launchd_plist_path(port).exists();
    if !installed {
        return InstanceState {
            unit: label,
            installed: false,
            state: "absent".into(),
            enabled: "unknown".into(),
        };
    }
    let running = launchd_is_running(&label);
    let loaded = launchd_is_loaded(&label);
    InstanceState {
        unit: label,
        installed,
        state: if running { "running" } else { "stopped" }.into(),
        enabled: if loaded { "loaded" } else { "unloaded" }.into(),
    }
}

/// Report the state of every managed instance.
pub fn status() -> Result<()> {
    if systemd_available() {
        return systemd_status();
    }
    if launchd_available() {
        return launchd_status();
    }
    println!("No supported service manager found (systemd/launchd).");
    Ok(())
}

fn systemd_status() -> Result<()> {
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

// ---------------------------------------------------------------------------
// launchd (macOS)
// ---------------------------------------------------------------------------

/// Directory for user LaunchAgents.
#[cfg(target_os = "macos")]
pub fn launchd_agents_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("~"))
        .join("Library/LaunchAgents")
}

#[cfg(not(target_os = "macos"))]
pub fn launchd_agents_dir() -> PathBuf {
    PathBuf::from(".")
}

/// Plist path for the LaunchAgent managing `port`.
pub fn launchd_plist_path(port: u16) -> PathBuf {
    launchd_agents_dir().join(format!("{}.plist", instance_name(port)))
}

/// Escape a string for use inside a plist `<string>` element.
fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Render the launchd LaunchAgent plist for a proxy on `port`.
pub fn render_plist(
    exe: &Path,
    port: u16,
    env: &[(String, String)],
    log_path: &Path,
    search: bool,
) -> String {
    let label = instance_name(port);
    let log = log_path.display();
    let search_arg = if search {
        "\t\t<string>--search</string>\n"
    } else {
        ""
    };
    let mut plist = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <!-- {UNIT_MARKER} -->\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \t<key>Label</key>\n\
         \t<string>{label}</string>\n\
         \t<key>ProgramArguments</key>\n\
         \t<array>\n\
         \t\t<string>{}</string>\n\
         \t\t<string>up</string>\n\
         \t\t<string>--port</string>\n\
         \t\t<string>{port}</string>\n\
         \t\t<string>--foreground</string>\n\
         \t\t<string>--no-provider-check</string>\n\
         {search_arg}\t</array>\n",
        xml_escape(&exe.display().to_string()),
    );
    if !env.is_empty() {
        plist.push_str("\t<key>EnvironmentVariables</key>\n\t<dict>\n");
        for (key, value) in env {
            plist.push_str(&format!(
                "\t\t<key>{}</key>\n\t\t<string>{}</string>\n",
                xml_escape(key),
                xml_escape(value)
            ));
        }
        plist.push_str("\t</dict>\n");
    }
    plist.push_str(&format!(
        "\t<key>RunAtLoad</key>\n\t<true/>\n\
         \t<key>KeepAlive</key>\n\t<true/>\n\
         \t<key>StandardOutPath</key>\n\t<string>{}</string>\n\
         \t<key>StandardErrorPath</key>\n\t<string>{}</string>\n\
         </dict>\n\
         </plist>\n",
        log, log
    ));
    plist
}

fn launchctl_run<S: AsRef<std::ffi::OsStr>>(args: &[S]) -> Result<std::process::Output> {
    Command::new("launchctl")
        .args(args)
        .output()
        .with_context(|| format!("failed to run launchctl with {} arguments", args.len()))
}

/// Whether a LaunchAgent with `label` is loaded into the user's launchd.
fn launchd_is_loaded(label: &str) -> bool {
    launchctl_run(&["list"])
        .map(|out| {
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .any(|line| line.split_whitespace().last() == Some(label))
        })
        .unwrap_or(false)
}

/// Whether a loaded LaunchAgent is currently running (PID column != "-").
fn launchd_is_running(label: &str) -> bool {
    launchctl_run(&["list"])
        .map(|out| {
            String::from_utf8_lossy(&out.stdout).lines().any(|line| {
                let parts: Vec<&str> = line.split_whitespace().collect();
                parts.len() >= 3 && parts[2] == label && parts[0] != "-"
            })
        })
        .unwrap_or(false)
}

/// Load a LaunchAgent and mark it enabled (survives logout/login).
/// Already-loaded agents are left alone (idempotent start).
fn launchctl_load(path: &Path) -> Result<()> {
    let args = [
        std::ffi::OsStr::new("load"),
        std::ffi::OsStr::new("-w"),
        path.as_os_str(),
    ];
    let out = launchctl_run(&args)?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr.contains("already loaded") {
        return Ok(());
    }
    Err(anyhow!(
        "launchctl load -w {} failed: {}",
        path.display(),
        stderr.trim()
    ))
}

/// Unload a LaunchAgent; errors are ignored (job may not exist).
fn launchctl_unload(path: &Path) -> Result<()> {
    let args = [std::ffi::OsStr::new("unload"), path.as_os_str()];
    let _ = launchctl_run(&args)?;
    Ok(())
}

/// Converge the per-port launchd LaunchAgent.
async fn ensure_launchd_service(config: &Config) -> Result<ServiceAction> {
    let port = config.port;
    let exe = exe_path();
    let env = env_entries(config);
    let path = launchd_plist_path(port);
    let label = instance_name(port);
    let log_dir = dirs::home_dir()
        .ok_or_else(|| anyhow!("cannot determine home directory"))?
        .join("Library/Logs");
    let log_path = log_dir.join(format!("{label}.log"));
    let expected = render_plist(&exe, port, &env, &log_path, config.enable_websearch);

    let loaded = launchd_is_loaded(&label);
    if loaded
        && path.exists()
        && fs::read_to_string(&path).ok().as_deref() == Some(expected.as_str())
        && launchd_is_running(&label)
    {
        return Ok(ServiceAction::AlreadyActive);
    }

    let existed = path.exists();
    let needs_write =
        !existed || fs::read_to_string(&path).ok().as_deref() != Some(expected.as_str());

    if needs_write {
        if !loaded {
            stop_legacy_daemon();
            if port_in_use(port) {
                return Err(anyhow!(
                    "port {port} is already bound by another process (e.g. `klava up` \
                     in a terminal or another tool). Stop it first, then retry."
                ));
            }
        }
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::create_dir_all(&log_dir)?;
        fs::write(&path, &expected)
            .with_context(|| format!("failed to write LaunchAgent {}", path.display()))?;
        // launchd refuses world/group-writable agent plists.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o644));
        }
        // Reload so the new arguments take effect.
        let _ = launchctl_unload(&path);
    }

    launchctl_load(&path)?;
    if !launchd_is_loaded(&label) {
        return Err(anyhow!(
            "failed to load LaunchAgent {}: check `launchctl list`",
            path.display()
        ));
    }

    wait_health(port, Duration::from_secs(10))
        .await
        .map_err(|e| {
            anyhow!(
                "{e}\nThe LaunchAgent may be crash-looping. Inspect it with:\n  \t{}",
                logs_command(port)
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

/// Report launchd-managed instances.
fn launchd_status() -> Result<()> {
    let dir = launchd_agents_dir();
    if !dir.exists() {
        println!("No managed klava service instances.");
        return Ok(());
    }
    let mut found = false;
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("klava-") || !name.ends_with(".plist") {
            continue;
        }
        found = true;
        let label = name.trim_end_matches(".plist");
        let state = launchd_is_running(label);
        let loaded = launchd_is_loaded(label);
        let icon = if state { "✓" } else { "✗" };
        println!(
            "{icon} {name}  [{}{}]",
            if loaded { "loaded, " } else { "" },
            if state { "running" } else { "stopped" }
        );
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
        assert_eq!(instance_name(48017), "klava-48017");
    }

    #[test]
    fn plist_has_marker_label_args_and_keepalive() {
        let plist = render_plist(
            Path::new("/opt/klava"),
            48017,
            &[],
            Path::new("/tmp/klava-48017.log"),
            false,
        );
        assert!(plist.contains(UNIT_MARKER));
        assert!(plist.contains("<string>klava-48017</string>"));
        assert!(plist.contains("<string>/opt/klava</string>"));
        assert!(plist.contains("<string>--no-provider-check</string>"));
        assert!(plist.contains("<key>KeepAlive</key>"));
        assert!(plist.contains("<key>RunAtLoad</key>"));
        assert!(plist.contains("<string>/tmp/klava-48017.log</string>"));
    }

    #[test]
    fn plist_escapes_env_values() {
        let plist = render_plist(
            Path::new("/opt/klava"),
            48017,
            &[("KEY".to_string(), "a&b<c>d\"e".to_string())],
            Path::new("/tmp/klava-48017.log"),
            false,
        );
        assert!(plist.contains("<key>KEY</key>"));
        assert!(plist.contains("<string>a&amp;b&lt;c&gt;d&quot;e</string>"));
        assert!(!plist.contains("a&b<c>d\"e"));
    }

    #[test]
    fn xml_escape_handles_all_specials() {
        assert_eq!(xml_escape("&<>\"'"), "&amp;&lt;&gt;&quot;&apos;");
        assert_eq!(xml_escape("plain"), "plain");
    }

    #[test]
    fn plist_path_is_under_launchagents_dir() {
        let p = launchd_plist_path(48017);
        assert!(p.to_string_lossy().ends_with("klava-48017.plist"));
        #[cfg(target_os = "macos")]
        assert!(
            p.to_string_lossy()
                .ends_with("LaunchAgents/klava-48017.plist")
        );
    }

    #[test]
    fn unit_path_is_under_user_systemd_dir() {
        let p = unit_path(48017);
        assert!(
            p.to_string_lossy()
                .ends_with("systemd/user/klava-48017.service")
        );
    }

    #[test]
    fn render_unit_has_marker_port_and_no_provider_check() {
        let unit = render_unit(Path::new("/opt/klava"), 48017, &[], false);
        assert!(unit.contains(UNIT_MARKER));
        assert!(unit.contains("Description=klava proxy (port 48017)"));
        assert!(
            unit.contains("ExecStart=/opt/klava up --port 48017 --foreground --no-provider-check")
        );
        assert!(unit.contains("Restart=always"));
        assert!(unit.contains("RestartSec=2"));
        assert!(unit.contains("WantedBy=default.target"));
    }

    #[test]
    fn render_unit_adds_search_flag_when_enabled() {
        let unit = render_unit(Path::new("/opt/klava"), 48017, &[], true);
        assert!(
            unit.contains(
                "ExecStart=/opt/klava up --port 48017 --foreground --no-provider-check --search"
            ),
            "unit: {unit}"
        );

        let plist = render_plist(
            Path::new("/opt/klava"),
            48017,
            &[],
            Path::new("/tmp/klava-48017.log"),
            true,
        );
        assert!(
            plist.contains("<string>--search</string>"),
            "plist: {plist}"
        );
    }

    #[test]
    fn render_unit_includes_environment_entries() {
        let unit = render_unit(
            Path::new("/opt/klava"),
            48017,
            &[("OPENROUTER_API_KEY".to_string(), "sk-123".to_string())],
            false,
        );
        assert!(unit.contains("Environment=OPENROUTER_API_KEY=sk-123"));
    }

    #[test]
    fn env_entries_only_include_set_vars() {
        let config = Config {
            models: vec![
                klava::providers::Model {
                    name: "with-env".into(),
                    provider_type: klava::providers::Type::OpenAICompatible,
                    api_key_name: Some("KLAVA_TEST_AUTH_KEY_A".into()),
                    ..klava::providers::Model::default()
                },
                klava::providers::Model {
                    name: "no-env".into(),
                    provider_type: klava::providers::Type::OpenAICompatible,
                    api_key: Some("inline".into()),
                    ..klava::providers::Model::default()
                },
            ],
            ..Config::default()
        };
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
