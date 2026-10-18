//! `klava extensions` — inspect and configure extensions installed at runtime.
//!
//! klava ships with no extensions. This module scans the extensions directory,
//! so it discovers whatever the user put there. It names no extension and knows
//! no extension's keys; `enabled` is the only key klava itself interprets.

use crate::config::Config;
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
        "show" => {
            let name = rest
                .first()
                .ok_or_else(|| anyhow::anyhow!("usage: klava extensions show <name>"))?;
            show(config, name)
        }
        "enable" => set_enabled(config, rest, true),
        "disable" => set_enabled(config, rest, false),
        "set" => set_key(config, rest),
        other => bail!("unknown action '{other}'; expected list, show, enable, disable or set"),
    }
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
    let dir = extensions::extensions_dir();
    let all = installed();
    if all.is_empty() {
        println!("No extensions installed.");
        println!();
        println!("klava ships without extensions. Install one by placing an");
        println!("executable in:");
        println!();
        println!("  {}", dir.display());
        println!();
        println!("See EXTENSIONS.md for the protocol.");
        return Ok(());
    }

    println!("{:<20} {:<9} PATH", "NAME", "STATUS");
    for ext in &all {
        let status = if is_enabled(config, ext) {
            "enabled".green().to_string()
        } else {
            "disabled".yellow().to_string()
        };
        println!("{:<20} {:<9} {}", ext.name, status, ext.path.display());
    }

    println!();
    println!("Directory: {}", dir.display());
    println!("Config:    {}", Config::get_path().display());
    println!("Keys:      [extensions.settings.<name>]");
    Ok(())
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
    println!();
    println!("Effective configuration:");
    println!(
        "{}",
        serde_json::to_string_pretty(&effective(config, &ext))?
    );
    Ok(())
}

fn set_enabled(config: &mut Config, rest: &[String], enabled: bool) -> Result<()> {
    let name = rest
        .first()
        .ok_or_else(|| anyhow::anyhow!("usage: klava extensions enable|disable <name>"))?;
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
