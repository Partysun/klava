//! `klava upgrade` — upgrade klava itself, through the package manager that
//! installed it.
//!
//! klava is distributed through cargo, npm, and pip, and each of those records
//! where it puts its executables. `upgrade` finds which manager owns the
//! running binary, prints the exact command it would run, and asks once before
//! running it — the same consent shape as `klava extensions update`, but for
//! the CLI itself. There is no downloaded installer and klava never replaces
//! its own binary: the package manager that installed it is the one that
//! updates it, so the upgrade stays inside the user's own tooling.
//!
//! * **cargo** — `$CARGO_HOME/.crates.toml` (or `~/.cargo/`) lists `klava`.
//!   Upgrading runs `cargo install --force klava`, which replaces the binary
//!   exactly as any `cargo install` does.
//! * **npm** — the running binary lives under the global npm prefix (`npm
//!   prefix --global`). Upgrading runs `npm install --global
//!   @zatsepin/klava@latest`.
//! * **uv / pip** — Python-managed installs. When `uv` is present it wins,
//!   because that is what a current setup most likely used: `uv tool list`
//!   showing `klava` means `uv tool install klava` (upgrade with `uv tool
//!   upgrade klava`), and `uv pip show klava` answering means a virtualenv
//!   (`uv pip install --upgrade klava`). Only when uv is absent does `pip show
//!   klava` decide (`pip install --upgrade klava`).
//! * **anything else** — a source build, a manually placed binary, an unknown
//!   prefix: there is no manager to replay. klava says so and names the
//!   documented install commands instead of guessing.

use crate::ext_install::version_from_crates_toml;
use crate::ext_update::{command_stdout, version_from_cargo_search, version_from_npm_view};
use crate::telemetry::compare_versions;
use anyhow::{Context, Result, bail};
use colored::Colorize;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Which package manager installed the running klava.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Manager {
    Cargo,
    Npm,
    /// `uv tool install klava`.
    Uv,
    /// `uv pip install klava` (a virtualenv).
    UvPip,
    /// plain `pip install klava`.
    Pip,
    Unknown,
}

impl Manager {
    /// Short label for the status line.
    fn job_name(&self) -> &'static str {
        match self {
            Self::Cargo => "cargo",
            Self::Npm => "npm (global)",
            Self::Uv => "uv tool",
            Self::UvPip => "uv pip",
            Self::Pip => "pip",
            Self::Unknown => "unknown",
        }
    }
}

/// The exact command `klava upgrade` would run for a manager.
fn upgrade_command(manager: Manager) -> (String, Vec<String>) {
    match manager {
        Manager::Cargo => (
            "cargo".to_string(),
            vec!["install".into(), "--force".into(), "klava".into()],
        ),
        Manager::Npm => (
            "npm".to_string(),
            vec![
                "install".into(),
                "--global".into(),
                "@zatsepin/klava@latest".into(),
            ],
        ),
        Manager::Uv => (
            "uv".to_string(),
            vec!["tool".into(), "upgrade".into(), "klava".into()],
        ),
        Manager::UvPip => (
            "uv".to_string(),
            vec![
                "pip".into(),
                "install".into(),
                "--upgrade".into(),
                "klava".into(),
            ],
        ),
        Manager::Pip => (
            "pip".to_string(),
            vec!["install".into(), "--upgrade".into(), "klava".into()],
        ),
        Manager::Unknown => ("".to_string(), Vec::new()),
    }
}

fn render_command(program: &str, args: &[String]) -> String {
    format!("{program} {}", args.join(" "))
}

/// Everything detection needs; injected in tests so nothing reads a real
/// `~/.cargo`, npm prefix, or the running binary's location.
#[derive(Debug, Clone)]
struct Ctx {
    /// The running klava executable.
    exe: PathBuf,
    /// `$CARGO_HOME`, or `~/.cargo`.
    cargo_home: PathBuf,
    /// `npm prefix --global`, when npm answered.
    npm_prefix: Option<PathBuf>,
    /// `uv tool list` output, when uv is available.
    uv_tools: Option<String>,
    /// `uv pip show klava` output, when uv is available.
    uv_pip: Option<String>,
    /// The version `pip show klava` reported, when plain pip manages klava.
    pip: Option<String>,
}

/// `klava upgrade [--check] [--yes|-y] [--force]`
///
/// Flags are typed by `clap`; this only receives what the user actually asked for.
pub fn run(check: bool, yes: bool, force: bool) -> Result<()> {
    let exe = std::env::current_exe().with_context(|| "cannot locate the running klava binary")?;
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let cargo_home = std::env::var("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home.join(".cargo"));
    let npm_prefix = command_stdout("npm", &["prefix", "--global"]).and_then(|text| {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    });

    // uv, when present, owns Python installs: probe it before plain pip.
    let uv_available = command_stdout("uv", &["--version"]).is_some();
    let uv_tools = if uv_available {
        command_stdout("uv", &["tool", "list"])
    } else {
        None
    };
    let uv_pip = if uv_available {
        command_stdout("uv", &["pip", "show", "klava"])
    } else {
        None
    };
    let pip =
        command_stdout("pip", &["show", "klava"]).and_then(|text| version_from_pip_show(&text));

    run_with(
        check,
        yes,
        force,
        Ctx {
            exe,
            cargo_home,
            npm_prefix: npm_prefix.map(PathBuf::from),
            uv_tools,
            uv_pip,
            pip,
        },
    )
}

#[cfg(test)]
fn run_in(args: &[String], ctx: Ctx) -> Result<()> {
    let mut check = false;
    let mut yes = false;
    let mut force = false;
    for arg in args {
        match arg.as_str() {
            "--check" => check = true,
            "--yes" | "-y" => yes = true,
            "--force" => force = true,
            other => bail!("unknown flag '{other}'"),
        }
    }
    run_with(check, yes, force, ctx)
}

fn run_with(check: bool, yes: bool, force: bool, ctx: Ctx) -> Result<()> {
    let manager = detect(&ctx);
    let current = env!("CARGO_PKG_VERSION");

    if manager == Manager::Unknown {
        println!("Cannot tell how klava was installed, so `klava upgrade` will not guess.");
        println!();
        println!("The documented ways to upgrade are:");
        println!("  cargo:  cargo install klava --locked");
        println!("  pip:    pip install --upgrade klava");
        println!("  npm:    npm install --global @zatsepin/klava");
        return Ok(());
    }

    let (program, upgrade_args) = upgrade_command(manager);
    let latest = latest_version(manager);

    if check {
        println!("klava {} (installed by {})", current, manager.job_name());
        match latest {
            Some(version) if compare_versions(current, &version) => {
                println!("  latest:  {version} (an upgrade is available)")
            }
            Some(version) => println!("  latest:  {version} (up to date)"),
            None => println!("  latest:  unknown (the registry probe failed)"),
        }
        println!(
            "  would run: {}",
            render_command(&program, &upgrade_args).dimmed()
        );
        return Ok(());
    }

    if !force
        && let Some(latest) = latest.as_deref()
        && !compare_versions(current, latest)
    {
        println!("klava is already up to date ({})", current);
        println!("  latest is also {latest}");
        return Ok(());
    }

    // Consent, like every mutating extension command: run the package manager
    // only after printing exactly what will run.
    let command = render_command(&program, &upgrade_args);
    if !yes && !confirm(command.as_str()) {
        bail!(
            "refusing to upgrade without --yes\n\
             re-run with --yes to let klava run '{command}'"
        );
    }

    println!("Upgrading klava (installed by {})", manager.job_name());
    println!("  {}", command.dimmed());
    let mut child = Command::new(&program);
    child.args(&upgrade_args);
    let status = child
        .status()
        .with_context(|| format!("cannot run '{command}'"))?;
    if !status.success() {
        bail!("'{command}' failed with {status}");
    }

    println!("{} klava upgraded", "✓".green());
    println!(
        "  if the proxy service is running, restart it to pick up the new binary: klava restart"
    );
    Ok(())
}

/// Which manager owns `ctx.exe`.
fn detect(ctx: &Ctx) -> Manager {
    // The cargo ledger is decisive: `cargo install` records every installed
    // package and the binaries it provides (parsed exactly like the extension
    // installer's ledger).
    let ledger = std::fs::read_to_string(ctx.cargo_home.join(".crates.toml")).ok();
    if ledger
        .as_deref()
        .and_then(|text| version_from_crates_toml(text, "klava"))
        .is_some()
    {
        return Manager::Cargo;
    }

    if ctx
        .npm_prefix
        .as_deref()
        .map(|prefix| exe_below(&ctx.exe, prefix))
        .unwrap_or(false)
    {
        return Manager::Npm;
    }

    // uv, when present, is the Python path most likely to have installed a
    // current setup; `uv tool list` beats `uv pip show` (a tool install wins
    // over a throwaway virtualenv), and plain pip is only consulted when uv
    // is absent entirely.
    if ctx
        .uv_tools
        .as_deref()
        .map(|list| list.contains("klava"))
        .unwrap_or(false)
    {
        return Manager::Uv;
    }
    if ctx.uv_pip.is_some() {
        return Manager::UvPip;
    }
    if ctx.pip.is_some() {
        return Manager::Pip;
    }

    Manager::Unknown
}

/// Whether `exe` lives in `<prefix>/bin` (or directly in `<prefix>`, the npm
/// layout on Windows).
fn exe_below(exe: &Path, prefix: &Path) -> bool {
    let exe_canon = std::fs::canonicalize(exe)
        .ok()
        .unwrap_or_else(|| exe.to_path_buf());
    let exe_dir = exe_canon.parent().map(Path::to_path_buf);

    let bin = std::fs::canonicalize(prefix.join("bin"))
        .ok()
        .unwrap_or_else(|| prefix.join("bin"));
    if exe_dir == Some(bin) {
        return true;
    }

    let prefix_canon = std::fs::canonicalize(prefix)
        .ok()
        .unwrap_or_else(|| prefix.to_path_buf());
    exe_dir == Some(prefix_canon)
}

/// The newest klava version the manager's registry reports, when it can be
/// learned cheaply. Best effort: a missing manager, no network, or an
/// unparseable answer all mean `None`, and the caller then just runs the
/// upgrade (which is idempotent).
fn latest_version(manager: Manager) -> Option<String> {
    match manager {
        Manager::Cargo => command_stdout("cargo", &["search", "klava", "--limit", "1"])
            .and_then(|text| version_from_cargo_search(&text)),
        Manager::Npm => command_stdout("npm", &["view", "@zatsepin/klava", "version"])
            .and_then(|text| version_from_npm_view(&text)),
        // uv has no version-listing subcommand; `pip index` (or its absence)
        // is the best effort for both.
        Manager::Uv | Manager::UvPip | Manager::Pip => {
            command_stdout("pip", &["index", "versions", "klava"])
                .and_then(|text| version_from_pip_index(&text))
        }
        Manager::Unknown => None,
    }
}

/// Parse the `Version:` line of `pip show klava`.
fn version_from_pip_show(text: &str) -> Option<String> {
    for line in text.lines() {
        if let Some((_, value)) = line.split_once(':')
            && line.trim_start().starts_with("Version:")
            && !value.trim().is_empty()
        {
            return Some(value.trim().to_string());
        }
    }
    None
}

/// Parse `pip index versions klava`: the last token of the `Available
/// versions:` line.
fn version_from_pip_index(text: &str) -> Option<String> {
    for line in text.lines() {
        if let Some((_, value)) = line.split_once(':')
            && line.trim_start().starts_with("Available versions:")
        {
            let versions: Vec<String> = value
                .split(',')
                .map(str::trim)
                .filter(|token| !token.is_empty())
                .map(str::to_string)
                .collect();
            return versions.last().cloned();
        }
    }
    None
}

/// Ask once for permission to run `command`.
fn confirm(command: &str) -> bool {
    use std::io::{IsTerminal, Write};

    if !std::io::stdin().is_terminal() {
        return false;
    }
    print!("klava will run: {}\nRun it? [y/N] ", command.yellow());
    let _ = std::io::stdout().flush();

    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .map(|_| matches!(answer.trim(), "y" | "Y" | "yes" | "YES"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(exe: PathBuf) -> Ctx {
        Ctx {
            exe,
            cargo_home: PathBuf::from("/tmp/absent-cargo-home-for-tests"),
            npm_prefix: None,
            uv_tools: None,
            uv_pip: None,
            pip: None,
        }
    }

    #[test]
    fn pip_show_version_is_parsed() {
        let text = "WARNING: pip is being invoked by an old script\n\
                    Name: klava\nVersion: 0.3.1\nSummary: Klava is a CLI\n";
        assert_eq!(version_from_pip_show(text), Some("0.3.1".into()));
        assert_eq!(version_from_pip_show("not installed"), None);
    }

    #[test]
    fn pip_index_takes_the_last_version() {
        let text = "WARNING: pip index is currently an experimental command.\n\
                    Available versions: 0.1.0, 0.2.0, 0.3.1\n";
        assert_eq!(version_from_pip_index(text), Some("0.3.1".into()));
        assert_eq!(
            version_from_pip_index("error: no matching distribution"),
            None
        );
    }

    #[test]
    fn a_cargo_ledger_is_decisive() {
        let dir = std::env::temp_dir().join(format!("klava-up-self-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(".crates.toml"),
            "[v1]\n\"klava 0.3.1 (registry+https://github.com/rust-lang/crates.io-index)\" = [\"klava\"]\n",
        )
        .unwrap();
        let cargo = Ctx {
            cargo_home: dir.clone(),
            ..ctx(PathBuf::from("/"))
        };
        assert_eq!(detect(&cargo), Manager::Cargo);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn without_a_manager_everything_is_unknown() {
        assert_eq!(
            detect(&ctx(PathBuf::from("/tmp/whatever/klava"))),
            Manager::Unknown
        );
    }

    #[test]
    fn an_npm_binary_under_the_prefix_is_npm() {
        let root = std::env::temp_dir().join(format!("klava-npm-prefix-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("bin")).unwrap();
        let exe = root.join("bin").join("klava");
        std::fs::write(&exe, "#!/bin/sh\nexit 0\n").unwrap();

        let npm = Ctx {
            npm_prefix: Some(root.to_path_buf()),
            ..ctx(exe.to_path_buf())
        };
        assert_eq!(detect(&npm), Manager::Npm);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn uv_tool_list_wins_over_uv_pip_and_plain_pip() {
        let base = ctx(PathBuf::from("/tmp/whatever/klava"));
        let uv_tool = Ctx {
            uv_tools: Some("Resolved 1 tool\n╰── klava v0.3.0\n".into()),
            uv_pip: Some("Name: klava\n".into()),
            pip: Some("0.3.1".into()),
            ..base.clone()
        };
        assert_eq!(detect(&uv_tool), Manager::Uv);

        // Without the tool entry, an answering `uv pip show` decides.
        let uv_pip = Ctx {
            uv_pip: Some("Name: klava\n".into()),
            pip: Some("0.3.1".into()),
            ..base.clone()
        };
        assert_eq!(detect(&uv_pip), Manager::UvPip);

        // plain pip only when uv is not involved at all.
        let pip = Ctx {
            pip: Some("0.3.1".into()),
            ..base.clone()
        };
        assert_eq!(detect(&pip), Manager::Pip);
    }

    #[test]
    fn upgrade_commands_are_the_documented_ones() {
        assert_eq!(
            upgrade_command(Manager::Cargo),
            (
                "cargo".into(),
                vec!["install".into(), "--force".into(), "klava".into()]
            )
        );
        assert_eq!(
            upgrade_command(Manager::Npm),
            (
                "npm".into(),
                vec![
                    "install".into(),
                    "--global".into(),
                    "@zatsepin/klava@latest".into()
                ],
            )
        );
        assert_eq!(
            upgrade_command(Manager::Uv),
            (
                "uv".into(),
                vec!["tool".into(), "upgrade".into(), "klava".into()]
            )
        );
        assert_eq!(
            upgrade_command(Manager::UvPip),
            (
                "uv".into(),
                vec![
                    "pip".into(),
                    "install".into(),
                    "--upgrade".into(),
                    "klava".into()
                ],
            )
        );
        assert_eq!(
            upgrade_command(Manager::Pip),
            (
                "pip".into(),
                vec!["install".into(), "--upgrade".into(), "klava".into()]
            )
        );
    }

    #[test]
    fn an_unknown_flag_is_rejected() {
        let c = ctx(PathBuf::from("/tmp/whatever/klava"));
        assert!(run_in(&["--frobnicate".into()], c).is_err());
    }
}
