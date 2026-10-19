//! `klava extensions` — inspect and configure extensions installed at runtime.
//!
//! klava ships with no extensions. This module scans the extensions directory,
//! so it discovers whatever the user put there. It names no extension and knows
//! no extension's keys; `enabled` is the only key klava itself interprets.

use crate::config::Config;
use crate::ext_install::{self, Source, SourceRecord};
use crate::ext_setup::{self, Consent, Request, SetupOutcome};
use crate::ext_setup_store;
use crate::extensions::{self, InstalledExtension};
use anyhow::{Result, bail};
use colored::Colorize;
use serde_json::Value;

pub fn run(config: &mut Config, args: Vec<String>) -> Result<()> {
    let (action, rest) = match args.split_first() {
        Some((a, rest)) => (a.as_str(), rest),
        None => ("list", &[][..]),
    };

    match action {
        "list" | "ls" => list(config),
        "path" | "paths" => path(),
        "install" | "add" => install(config, rest),
        "remove" | "uninstall" | "rm" => remove(config, rest),
        "verify" => verify(rest),
        "show" => {
            let name = rest
                .first()
                .ok_or_else(|| anyhow::anyhow!("usage: klava extensions show <name>"))?;
            show(config, name)
        }
        "setup" => setup(config, rest),
        "enable" => set_enabled(config, rest, true),
        "disable" => set_enabled(config, rest, false),
        "set" => set_key(config, rest),
        other => bail!(
            "unknown action '{other}'; expected list, path, install, remove, verify, \
             show, setup, enable, disable or set"
        ),
    }
}

/// `klava extensions path` — where klava looks, in order.
fn path() -> Result<()> {
    println!("{:<8} {:<10} DIRECTORY", "ORDER", "EXISTS");
    for (index, entry) in extensions::search_path().iter().enumerate() {
        let exists = if entry.dir.is_dir() {
            "yes".green().to_string()
        } else {
            "no".dimmed().to_string()
        };
        println!(
            "{:<8} {:<10} {}  ({})",
            index + 1,
            exists,
            entry.dir.display(),
            entry.origin
        );
    }
    println!();
    println!("Installs go to: {}", extensions::install_dir().display());
    Ok(())
}

/// `klava extensions install <source> [--name N] [--trust] [--no-setup] [--no-enable]`
fn install(config: &mut Config, rest: &[String]) -> Result<()> {
    let flags = ext_setup::take_flags(rest);

    let mut source: Option<String> = None;
    let mut name: Option<String> = None;
    let mut enable = true;

    let mut args = flags.rest.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--no-enable" => enable = false,
            "--name" | "-n" => {
                name = Some(
                    args.next()
                        .ok_or_else(|| anyhow::anyhow!("--name needs a value"))?
                        .clone(),
                );
            }
            other if other.starts_with('-') => bail!("unknown flag '{other}'"),
            other => {
                if source.is_some() {
                    bail!("install takes exactly one source");
                }
                source = Some(other.to_string());
            }
        }
    }

    let raw = source.ok_or_else(|| {
        anyhow::anyhow!(
            "usage: klava extensions install <source> [--trust] [--no-setup] [--no-enable]\n\
             \n\
             sources:\n  \
             ./path or /abs/path      a local executable\n  \
             cargo:<crate>[@ver]      cargo install\n  \
             cargo-git:<url>[#rev]    cargo install from git\n  \
             npm:<pkg>[@ver]          npm pack, take the bin\n\
             \n\
             --trust also consents to the extension's [setup] hook (e.g. a model\n\
             download); --no-setup installs and enables without running it."
        )
    })?;

    let parsed = Source::parse(&raw)?;
    let label = parsed.label();
    let dest = extensions::install_dir();

    println!("Source:  {label}");
    println!("Target:  {}", dest.display());

    // The trust gate. Installing runs third-party code, so say so plainly.
    if !flags.trust {
        use std::io::{IsTerminal, Write};
        if !std::io::stdin().is_terminal() {
            bail!(
                "refusing to install without --trust\n\
                 re-run with --trust to confirm you want to install and run '{label}'"
            );
        }
        print!("Install and run '{label}' as your user? [y/N] ");
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes" | "YES") {
            bail!("cancelled");
        }
    }

    println!();
    let installed = ext_install::install(&parsed, &dest, name.as_deref())?;

    // Record provenance beside the artifact.
    let record = SourceRecord {
        source: label.clone(),
        ..installed.record
    };
    record.save(&dest, &installed.name)?;

    println!();
    println!(
        "{} {} ({})",
        "Installed".green(),
        installed.name,
        installed.path.display()
    );
    println!("  sha256: {}", record.sha256);
    if let Some(version) = &record.version {
        println!("  version: {version}");
    }

    if enable {
        set_in_memory(config, &installed.name, "enabled", Value::Bool(true))?;
        config.save()?;
        println!("  enabled in {}", Config::get_path().display());

        // `--trust` (or `--yes`) already means "install and run this", so it is
        // the consent for the manifest's setup hook too; `--no-setup` still
        // opts out. Re-discover so the spec comes from the artifact that just
        // landed, not a stale listing.
        if let Some(ext) = find(&installed.name) {
            run_setup(config, &ext, flags.consent, Request::IfNeeded);
        }
    } else {
        println!(
            "  run 'klava extensions enable {}' to activate it",
            installed.name
        );
    }
    Ok(())
}

/// Run an extension's setup hook and report a failure as a warning, not an error.
///
/// The extension is enabled regardless: it fails open at request time, and a
/// failed download should not undo an otherwise successful install.
fn run_setup(
    config: &Config,
    ext: &InstalledExtension,
    consent: Consent,
    request: Request,
) -> SetupOutcome {
    let outcome = ext_setup::run_with(ext, consent, request, Some(&ext.effective_config(config)));
    if outcome == SetupOutcome::Declined && ext.setup_spec().is_some() {
        println!(
            "  setup was not run; do it later with: {}",
            format!("klava extensions setup {}", ext.name).cyan()
        );
    }
    outcome
}

/// `klava extensions setup <name>` — run the provisioning hook on demand.
///
/// This is the retry path: after a failed or skipped download, and for an
/// extension installed before it grew a setup hook.
fn setup(config: &mut Config, rest: &[String]) -> Result<()> {
    let mut name: Option<String> = None;

    for arg in rest {
        match arg.as_str() {
            "--yes" | "-y" => {}
            other if other.starts_with('-') => bail!("unknown flag '{other}'"),
            other => {
                if name.is_some() {
                    bail!("setup takes exactly one extension name");
                }
                name = Some(other.to_string());
            }
        }
    }

    let name = name.ok_or_else(|| anyhow::anyhow!("usage: klava extensions setup <name>"))?;
    let ext =
        find(&name).ok_or_else(|| anyhow::anyhow!("no extension named '{name}' is installed"))?;

    if ext.setup_spec().is_none() {
        println!("{} declares no setup hook; nothing to do", name);
        return Ok(());
    }

    // Asking for setup explicitly is the consent *and* an override of "already
    // done": this is the documented way to re-run a hook.
    match run_setup(config, &ext, Consent::Granted, Request::Always) {
        SetupOutcome::Failed { message } => bail!("setup failed: {message}"),
        _ => Ok(()),
    }
}

/// `klava extensions remove <name>`
fn remove(config: &mut Config, rest: &[String]) -> Result<()> {
    let name = rest
        .first()
        .ok_or_else(|| anyhow::anyhow!("usage: klava extensions remove <name>"))?;

    let path = ext_install::remove(name)?;
    println!("{} {} ({})", "Removed".green(), name, path.display());

    if config.extensions.settings.remove(name).is_some() {
        config.save()?;
        println!(
            "  removed its settings from {}",
            Config::get_path().display()
        );
    }
    Ok(())
}

/// `klava extensions verify [name]`
fn verify(rest: &[String]) -> Result<()> {
    let targets: Vec<InstalledExtension> = match rest.first() {
        Some(name) => vec![
            find(name)
                .ok_or_else(|| anyhow::anyhow!("no extension named '{name}' is installed"))?,
        ],
        None => installed(),
    };

    if targets.is_empty() {
        println!("No extensions installed.");
        return Ok(());
    }

    let mut mismatches = 0usize;
    for ext in &targets {
        let dir = ext.path.parent().unwrap_or(std::path::Path::new("."));
        match SourceRecord::load(dir, &ext.name) {
            Some(record) => {
                let actual = ext_install::sha256_of(&ext.path)?;
                if actual == record.sha256 {
                    println!("{} {} ok", "✓".green(), ext.name);
                } else {
                    mismatches += 1;
                    println!("{} {} changed since install", "✗".red(), ext.name);
                    println!("    recorded: {}", record.sha256);
                    println!("    actual:   {actual}");
                }
            }
            None => {
                println!("{} {} has no recorded checksum", "?".yellow(), ext.name);
            }
        }
    }

    if mismatches > 0 {
        bail!("{mismatches} extension(s) changed on disk");
    }
    Ok(())
}

/// Every installed extension, sorted by name for stable output.
fn installed() -> Vec<InstalledExtension> {
    extensions::Extensions::installed()
}

fn find(name: &str) -> Option<InstalledExtension> {
    installed().into_iter().find(|e| e.name == name)
}

/// Effective config: manifest overlaid with the user's table.
fn effective(config: &Config, ext: &InstalledExtension) -> Value {
    ext.effective_config(config)
}

fn is_enabled(config: &Config, ext: &InstalledExtension) -> bool {
    ext.is_enabled(config)
}

fn list(config: &Config) -> Result<()> {
    let all = installed();
    if all.is_empty() {
        println!("No extensions installed.");
        println!();
        println!("klava ships without extensions. Install one with:");
        println!();
        println!("  klava extensions install ./my-extension --trust");
        println!("  klava extensions install cargo:<crate> --trust");
        println!("  klava extensions install npm:<package> --trust");
        println!();
        println!("Run 'klava extensions path' to see where klava looks.");
        println!("See EXTENSIONS.md for the protocol.");
        return Ok(());
    }

    println!(
        "{:<20} {:<9} {:<8} {:<10} {:<16} PATH",
        "NAME", "STATUS", "SETUP", "VERSION", "SOURCE"
    );
    for ext in &all {
        let status = if is_enabled(config, ext) {
            "enabled".green().to_string()
        } else {
            "disabled".yellow().to_string()
        };
        let setup = setup_label(ext);
        let dir = ext.path.parent().unwrap_or(std::path::Path::new("."));
        let record = SourceRecord::load(dir, &ext.name);
        let version = record
            .as_ref()
            .and_then(|r| r.version.clone())
            .unwrap_or_else(|| "-".to_string());
        let source = record
            .as_ref()
            .map(|r| r.source.clone())
            .unwrap_or_else(|| ext.origin.unwrap_or("-").to_string());
        println!(
            "{:<20} {:<9} {:<8} {:<10} {:<16} {}",
            ext.name,
            status,
            setup,
            version,
            source,
            ext.path.display()
        );
    }

    println!();
    println!("Config: {}", Config::get_path().display());
    println!("Keys:   [extensions.settings.<name>]");
    Ok(())
}

/// One short word for the `list` table.
fn setup_label(ext: &InstalledExtension) -> &'static str {
    ext_setup_store::state(ext).label()
}

/// A one-line explanation for `show`.
fn setup_detail(ext: &InstalledExtension) -> String {
    match ext_setup_store::state(ext) {
        ext_setup_store::State::NoHook => {
            "none (the manifest declares no [setup] hook)".to_string()
        }
        ext_setup_store::State::Pending => format!(
            "pending: {}",
            format!("klava extensions setup {}", ext.name).cyan()
        ),
        ext_setup_store::State::Done { at } => format!("done (at {at})"),
        ext_setup_store::State::Stale => format!(
            "stale: ran for an earlier build or argument list; {}",
            format!("klava extensions setup {}", ext.name).cyan()
        ),
    }
}

fn show(config: &Config, name: &str) -> Result<()> {
    let ext = find(name).ok_or_else(|| anyhow::anyhow!("no extension named '{name}' installed"))?;

    println!("{}", ext.name.bold());
    println!(
        "  status: {}",
        if is_enabled(config, &ext) {
            "enabled".green().to_string()
        } else {
            "disabled".yellow().to_string()
        }
    );
    println!("  path:   {}", ext.path.display());
    println!(
        "  policy: {}",
        if ext.fail_closed {
            "fail-closed"
        } else {
            "fail-open"
        }
    );
    println!("  setup:  {}", setup_detail(&ext));
    println!();
    println!("Effective configuration:");
    println!(
        "{}",
        serde_json::to_string_pretty(&effective(config, &ext))?
    );
    Ok(())
}

fn set_enabled(config: &mut Config, rest: &[String], enabled: bool) -> Result<()> {
    let flags = ext_setup::take_flags(rest);

    // `--no-setup` and `--trust` describe provisioning, which only happens when
    // an extension turns on. Accepting them on `disable` would suggest they do
    // something.
    if !enabled && flags.consent != Consent::Ask {
        bail!("--no-setup/--trust only apply to enable");
    }

    let mut name: Option<&String> = None;
    for arg in &flags.rest {
        if arg.starts_with('-') {
            bail!("unknown flag '{arg}'");
        }
        name = Some(arg);
    }

    let name =
        name.ok_or_else(|| anyhow::anyhow!("usage: klava extensions enable|disable <name>"))?;
    let ext = find(name).ok_or_else(|| anyhow::anyhow!("no extension named '{name}' installed"))?;
    write_key(config, &ext.name, "enabled", Value::Bool(enabled))?;
    println!(
        "{} extension '{}'",
        if enabled {
            "Enabled".green()
        } else {
            "Disabled".yellow()
        },
        ext.name
    );

    // Provision on enable, not on disable: a model download is only meaningful
    // for an extension that is about to run.
    if enabled {
        run_setup(config, &ext, flags.consent, Request::IfNeeded);
    }
    Ok(())
}

/// `klava extensions set <name> key=value [key=value …]`
///
/// Values are parsed as JSON when possible, so `enabled=false`, `n=3` and
/// `entities=["Email"]` all work; anything else becomes a string.
fn set_key(config: &mut Config, rest: &[String]) -> Result<()> {
    let (name, pairs) = rest
        .split_first()
        .ok_or_else(|| anyhow::anyhow!("usage: klava extensions set <name> key=value"))?;
    if pairs.is_empty() {
        bail!("usage: klava extensions set <name> key=value [key=value …]");
    }
    let ext = find(name).ok_or_else(|| anyhow::anyhow!("no extension named '{name}' installed"))?;

    for pair in pairs {
        let (key, raw) = pair
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("expected key=value, got '{pair}'"))?;
        if key.is_empty() {
            bail!("empty key in '{pair}'");
        }
        let value = serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()));
        write_key(config, &ext.name, key, value.clone())?;
        println!("{} {} = {}", ext.name, key, value);
    }
    Ok(())
}

/// Set one key in memory. Separated from saving so tests never touch the
/// user's real config file.
fn set_in_memory(config: &mut Config, name: &str, key: &str, value: Value) -> Result<()> {
    let table = config
        .extensions
        .settings
        .entry(name.to_string())
        .or_insert_with(|| Value::Object(Default::default()));

    let Some(map) = table.as_object_mut() else {
        bail!("extension '{name}' config is not a table");
    };
    map.insert(key.to_string(), value);
    Ok(())
}

/// Set one key and persist the config.
fn write_key(config: &mut Config, name: &str, key: &str, value: Value) -> Result<()> {
    set_in_memory(config, name, key, value)?;
    config.save()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config {
            port: 48017,
            ..Config::default()
        }
    }

    #[test]
    fn effective_config_merges_user_over_manifest() {
        let ext = InstalledExtension {
            name: "demo".into(),
            path: std::path::PathBuf::from("/bin/true"),
            config: serde_json::json!({"enabled": false, "threshold": 0.5}),
            fail_closed: false,
            origin: None,
        };
        let mut c = config();
        set_in_memory(&mut c, "demo", "threshold", serde_json::json!(0.9)).expect("set");
        assert_eq!(effective(&c, &ext)["threshold"], 0.9);
    }

    #[test]
    fn enabling_writes_only_the_enabled_key() {
        let mut c = config();
        set_in_memory(&mut c, "demo", "enabled", Value::Bool(true)).expect("set");
        assert_eq!(c.extensions.settings["demo"]["enabled"], true);
    }

    #[test]
    fn an_unknown_extension_is_an_error() {
        let c = config();
        assert!(show(&c, "definitely-not-installed").is_err());
    }
}
