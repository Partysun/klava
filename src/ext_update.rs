//! `klava extensions update` — reinstall installed extensions from the source
//! recorded beside each artifact.
//!
//! `install` writes `<name>.source.toml` next to the executable, so provenance
//! travels with the artifact. Updating is therefore a replay of that recorded
//! source through the same staged, atomic install path — klava never guesses a
//! registry name or re-derives a URL.
//!
//! Two promises shape this module:
//!
//! * **Settings are never touched.** Enabled state and per-extension settings
//!   live in the klava config, which this command only reads.
//! * **A failed update is not an outage.** Every extension is staged and moved
//!   into place on its own; a failure keeps the old artifact and the batch
//!   continues with the next extension.
//!
//! Pins are dropped on update: `cargo:x@0.1.0` moves to the newest release and
//! the record is rewritten as `cargo:x`. Pin again with `install …@ver`.

use crate::config::Config;
use crate::ext_install::{self, Source, SourceRecord};
use crate::ext_setup::{self, Consent, Request};
use crate::extensions::{self, InstalledExtension};
use anyhow::{Result, bail};
use colored::Colorize;
use std::path::{Path, PathBuf};

/// Handle `klava extensions update [name…] [--check] [--force]`.
///
/// `flags` is the result of [`ext_setup::take_flags`], so `--trust`/`-y` and
/// `--no-setup` mean exactly what they mean on `install`.
pub fn run(config: &mut Config, flags: ext_setup::Flags) -> Result<()> {
    run_with(
        config,
        flags,
        Ctx {
            install_dir: extensions::install_dir(),
            search: extensions::search_path(),
        },
    )
}

/// [`run`] against an explicit install directory, so tests never touch the
/// user's data directory or `KLAVA_EXTENSIONS_DIR`.
#[cfg(test)]
fn run_in(config: &mut Config, flags: ext_setup::Flags, install_dir: PathBuf) -> Result<()> {
    run_with(
        config,
        flags,
        Ctx {
            install_dir: install_dir.clone(),
            search: vec![extensions::SearchPath {
                dir: install_dir.clone(),
                origin: "data",
            }],
        },
    )
}

/// Everything a run needs to know about where installed artifacts live.
///
/// The search path is the full discovery list, so a system-installed extension
/// shows up in the table as skipped rather than vanishing; the install dir is
/// the one directory this command may ever write to. Split out so tests can
/// point both at a scratch directory without touching the user's data or
/// `KLAVA_EXTENSIONS_DIR`.
struct Ctx {
    install_dir: PathBuf,
    search: Vec<extensions::SearchPath>,
}

fn run_with(config: &mut Config, flags: ext_setup::Flags, ctx: Ctx) -> Result<()> {
    let mut check = false;
    let mut force = false;
    let mut names: Vec<String> = Vec::new();

    for arg in &flags.rest {
        match arg.as_str() {
            "--check" | "--dry-run" => check = true,
            "--force" => force = true,
            other if other.starts_with('-') => bail!("unknown flag '{other}'"),
            other => names.push(other.to_string()),
        }
    }

    let installed = discover(&ctx);
    if installed.is_empty() {
        println!("No extensions installed.");
        println!();
        println!("Install one with:");
        println!();
        println!("  klava extensions install ./my-extension --trust");
        return Ok(());
    }

    let selected = select(&installed, &names)?;

    // Plan everything before touching anything: `--check` stops here, and the
    // consent prompt can name every source that would be built.
    let mut rows: Vec<Row> = Vec::new();
    let mut work: Vec<(InstalledExtension, Work)> = Vec::new();

    for ext in &selected {
        let dir = dir_of(&ext.path);
        let record = managed(&ctx, ext)
            .then(|| SourceRecord::load(&dir, &ext.name))
            .flatten();
        match plan_for(&ctx, ext, record.as_ref(), force) {
            Plan::Skip { reason } => rows.push(Row::skipped(ext, reason)),
            Plan::Current { version } => rows.push(Row::current(ext, version)),
            Plan::Available {
                target,
                pinned_from,
                from_version,
                to_version,
            } => {
                // A local file that differs is known to have changed; a
                // registry, a local crate, or a pin resolves to a known
                // version. Only a git branch moves without any version
                // changing, so it alone is "version unknown".
                let known = matches!(target, Source::Local(_)) || to_version.is_some();
                rows.push(Row::available(ext, from_version, to_version.clone(), known));
                work.push((
                    ext.clone(),
                    Work {
                        target,
                        pinned_from,
                    },
                ));
            }
        }
    }

    if check {
        print_rows(&rows, true);
        return Ok(());
    }

    if work.is_empty() {
        print_rows(&rows, false);
        return Ok(());
    }

    // The same gate as `install`: building third-party code is what needs
    // consent, and one answer covers the whole batch.
    if !flags.trust && !confirm(&work) {
        bail!(
            "refusing to update without --trust\n\
             re-run with --trust to confirm you want to rebuild {} extension(s)",
            work.len()
        );
    }

    let mut failures = 0usize;
    for (ext, item) in &work {
        println!();
        println!("{} {}", "Updating".green(), ext.name.bold());
        if let Some(pinned) = &item.pinned_from {
            println!(
                "  {} pinned source was '{pinned}'; tracking the latest release now",
                "note:".yellow()
            );
        }
        println!("  source: {}", item.target.label().dimmed());

        match apply(config, ext, item, flags.consent, &ctx) {
            Ok(ApplyResult { changed, from, to }) => {
                let row = rows
                    .iter_mut()
                    .find(|row| row.name == ext.name)
                    .expect("one row per selected extension");
                row.finish(from, to, changed);
            }
            Err(e) => {
                failures += 1;
                let row = rows
                    .iter_mut()
                    .find(|row| row.name == ext.name)
                    .expect("one row per selected extension");
                row.outcome = Outcome::Failed;
                println!("{} {} did not update: {e:#}", "✗".red(), ext.name);
                println!("  the previous build is still in place; retry when the cause is fixed");
            }
        }
    }

    print_rows(&rows, false);
    if failures > 0 {
        bail!("{failures} extension(s) failed to update");
    }
    Ok(())
}

/// Resolve the name arguments against what is actually installed.
fn select(installed: &[InstalledExtension], names: &[String]) -> Result<Vec<InstalledExtension>> {
    if names.is_empty() {
        return Ok(installed.to_vec());
    }

    let mut selected: Vec<InstalledExtension> = Vec::new();
    for name in names {
        let ext = installed
            .iter()
            .find(|ext| ext.name == *name)
            .ok_or_else(|| anyhow::anyhow!("no extension named '{name}' is installed"))?;
        if !selected.iter().any(|chosen| chosen.name == ext.name) {
            selected.push(ext.clone());
        }
    }
    Ok(selected)
}

/// One extension's planned move.
#[derive(Debug, PartialEq)]
enum Plan {
    /// Reinstall from `target`.
    Available {
        target: Source,
        /// The recorded source when it carried a version/rev pin.
        pinned_from: Option<String>,
        from_version: Option<String>,
        to_version: Option<String>,
    },
    /// Nothing to do.
    Current { version: Option<String> },
    /// Cannot be updated by klava.
    Skip { reason: String },
}

/// Decide what to do with one extension.
///
/// `record` is `None` when the extension is not in a directory klava manages.
/// A registry source is skipped when its recorded version is already the latest
/// one — that is what keeps a no-op `cargo:` update from compiling for minutes.
/// Local files are compared by content; a git or local-crate source can only be
/// rebuilt, because their version may not move when their code does.
fn plan_for(
    ctx: &Ctx,
    ext: &InstalledExtension,
    record: Option<&SourceRecord>,
    force: bool,
) -> Plan {
    if !managed(ctx, ext) {
        return Plan::Skip {
            reason: format!(
                "installed outside {}; update it with its package manager",
                ctx.install_dir.display()
            ),
        };
    }

    let Some(record) = record else {
        return Plan::Skip {
            reason: "no recorded source; reinstall it explicitly to make it updatable".into(),
        };
    };

    // A relative local path is resolved against the current directory, exactly
    // as it was at install time. A vanished path is a skip, not a failure: the
    // other extensions still update.
    let source = match Source::parse(&record.source) {
        Ok(source) => source,
        Err(_) => {
            return Plan::Skip {
                reason: format!(
                    "recorded source '{}' no longer resolves; reinstall it explicitly",
                    record.source
                ),
            };
        }
    };

    let target = source.latest();

    // A local file has no registry to ask: identical bytes mean there is
    // nothing to copy.
    if let Source::Local(_) = source {
        if !force
            && let Some(sha) = local_source_sha(&source)
            && sha == record.sha256
        {
            return Plan::Current {
                version: record.version.clone(),
            };
        }
        return Plan::Available {
            target,
            pinned_from: None,
            from_version: record.version.clone(),
            to_version: None,
        };
    }

    let latest = latest_version(&target);

    // Only a registry can answer "latest" authoritatively. A local crate or a
    // git branch can change without a version bump, so those always rebuild.
    if !force
        && registry_source(&source)
        && let Some(latest) = latest.as_deref()
        && record.version.as_deref() == Some(latest)
    {
        return Plan::Current {
            version: record.version.clone(),
        };
    }

    Plan::Available {
        target,
        pinned_from: source.is_pinned().then(|| source.label()),
        from_version: record.version.clone(),
        to_version: latest,
    }
}

/// Reinstall one extension, record the new provenance, and provision it.
fn apply(
    config: &Config,
    ext: &InstalledExtension,
    item: &Work,
    consent: Consent,
    ctx: &Ctx,
) -> Result<ApplyResult> {
    let dir = dir_of(&ext.path);
    let before = SourceRecord::load(&dir, &ext.name);

    let installed = ext_install::install(&item.target, &dir, Some(&ext.name))?;

    let record = SourceRecord {
        source: item.target.label(),
        version: installed.record.version.clone(),
        sha256: installed.record.sha256.clone(),
        installed_at: installed.record.installed_at.clone(),
    };
    record.save(&dir, &ext.name)?;

    // A new build may declare new settings or a changed setup hook. Refresh the
    // manifest only when the binary actually prints one, and say so when that
    // replaces a file the user may have edited.
    let manifest = dir.join(format!("{}.toml", ext.name));
    let before_manifest = std::fs::read_to_string(&manifest).ok();
    let refreshed = ext_install::refresh_manifest_in(&installed.path, &ext.name, &dir);
    let after_manifest = std::fs::read_to_string(&manifest).ok();
    if refreshed.wrote && before_manifest != after_manifest {
        println!(
            "  {} refreshed {}.toml from the new build",
            "manifest:".dimmed(),
            ext.name
        );
        if refreshed.replaced {
            println!(
                "  {} hand edits to the old manifest were replaced; settings in {} are untouched",
                "note:".yellow(),
                Config::get_path().display()
            );
        }
    }

    // Re-read the extension so setup sees the manifest that just landed.
    if let Some(fresh) = find(ctx, &ext.name) {
        if fresh.is_enabled(config) {
            crate::ext_cli::run_setup(config, &fresh, consent, Request::IfNeeded);
        } else if fresh.setup_spec().is_some() {
            println!(
                "  setup not run: '{}' is disabled; it runs on the next enable",
                fresh.name
            );
        }
    }

    let changed = before
        .as_ref()
        .map(|old| old.sha256 != record.sha256)
        .unwrap_or(true);

    Ok(ApplyResult {
        changed,
        from: before.and_then(|old| old.version),
        to: record.version,
    })
}

/// What one update actually did, for the summary table.
struct ApplyResult {
    changed: bool,
    from: Option<String>,
    to: Option<String>,
}

/// One extension that will be reinstalled.
struct Work {
    target: Source,
    pinned_from: Option<String>,
}

/// Whether klava owns the directory this artifact lives in.
///
/// Same rule as `extensions remove`: only the install directory (which is
/// `$KLAVA_EXTENSIONS_DIR` when set) may be written to, never a system dir.
fn managed(ctx: &Ctx, ext: &InstalledExtension) -> bool {
    dir_of(&ext.path) == ctx.install_dir || ext.origin == Some("KLAVA_EXTENSIONS_DIR")
}

fn dir_of(path: &Path) -> PathBuf {
    path.parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The extensions this run considers: everything on the search path, so a
/// system-installed extension is reported as skipped rather than invisible.
fn discover(ctx: &Ctx) -> Vec<InstalledExtension> {
    extensions::discover_in(&ctx.search)
}

fn find(ctx: &Ctx, name: &str) -> Option<InstalledExtension> {
    discover(ctx).into_iter().find(|ext| ext.name == name)
}

/// Whether re-resolving this source against a registry is authoritative.
fn registry_source(source: &Source) -> bool {
    matches!(source, Source::Cargo { .. } | Source::Npm { .. })
}

/// The version a source resolves to right now, when it can be learned cheaply.
///
/// Best effort by design: no cargo/npm on PATH, no network, or an unparseable
/// answer all mean `None`, and the caller falls back to rebuilding.
fn latest_version(source: &Source) -> Option<String> {
    match source {
        Source::Cargo { krate, version } => version
            .clone()
            .or_else(|| command_stdout("cargo", &["search", krate, "--limit", "1"]))
            .and_then(|text| version_from_cargo_search(&text)),
        Source::Npm { package, version } => version
            .clone()
            .or_else(|| command_stdout("npm", &["view", package, "version"]))
            .and_then(|text| version_from_npm_view(&text)),
        Source::CargoPath(dir) => crate_manifest_version(dir),
        Source::CargoGit { .. } | Source::Local(_) => None,
    }
}

pub(crate) fn command_stdout(program: &str, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Parse `cargo search <crate> --limit 1`.
///
/// The answer is one line, `crate = "1.2.3"  # description`, optionally
/// followed by a human `note:` line.
pub(crate) fn version_from_cargo_search(text: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("note:") {
            continue;
        }
        let Some((_, tail)) = line.split_once('=') else {
            continue;
        };
        let value = tail.split('#').next().unwrap_or(tail).trim();
        let value = value.trim_matches('"');
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

/// Parse `npm view <package> version`: the last non-empty line.
///
/// npm may print warnings on stdout in odd configurations; the version is the
/// final line, and a wrong guess is harmless because it only affects skipping.
pub(crate) fn version_from_npm_view(text: &str) -> Option<String> {
    let lines: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();
    lines.last().cloned()
}

/// `[package].version`, or the workspace fallback from a local crate.
fn crate_manifest_version(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join("Cargo.toml")).ok()?;
    let value: toml::Value = toml::from_str(&text).ok()?;
    if let Some(version) = value
        .get("package")
        .and_then(|package| package.get("version"))
        .and_then(toml::Value::as_str)
    {
        return Some(version.to_string());
    }
    value
        .get("workspace")?
        .get("package")?
        .get("version")?
        .as_str()
        .map(str::to_string)
}

/// Fingerprint the executable a local source points at.
fn local_source_sha(source: &Source) -> Option<String> {
    let Source::Local(path) = source else {
        return None;
    };
    let executable = ext_install::resolve_local_executable(path).ok()?;
    ext_install::sha256_of(&executable).ok()
}

/// Ask once, for the whole batch, whether to rebuild the listed sources.
fn confirm(work: &[(InstalledExtension, Work)]) -> bool {
    use std::io::{IsTerminal, Write};

    if !std::io::stdin().is_terminal() {
        return false;
    }

    println!("About to update {} extension(s):", work.len());
    for (ext, item) in work {
        println!("  {:<20} {}", ext.name, item.target.label().dimmed());
    }
    print!("Rebuild and install these sources? [y/N] ");
    let _ = std::io::stdout().flush();

    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .map(|_| matches!(answer.trim(), "y" | "Y" | "yes" | "YES"))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

/// How one row finished.
#[derive(Debug, Clone, PartialEq)]
enum Outcome {
    /// Reinstalled and the artifact changed.
    Updated,
    /// Asked to reinstall, but the bytes are the same.
    Reinstalled,
    /// Already at the newest version.
    Current,
    /// `--check`: a newer artifact exists.
    Available {
        known: bool,
    },
    Skipped(String),
    Failed,
}

struct Row {
    name: String,
    from: Option<String>,
    to: Option<String>,
    outcome: Outcome,
}

impl Row {
    fn available(
        ext: &InstalledExtension,
        from: Option<String>,
        to: Option<String>,
        known: bool,
    ) -> Self {
        Self {
            name: ext.name.clone(),
            from,
            to,
            outcome: Outcome::Available { known },
        }
    }

    fn current(ext: &InstalledExtension, version: Option<String>) -> Self {
        Self {
            name: ext.name.clone(),
            from: version.clone(),
            to: version,
            outcome: Outcome::Current,
        }
    }

    fn skipped(ext: &InstalledExtension, reason: String) -> Self {
        Self {
            name: ext.name.clone(),
            from: None,
            to: None,
            outcome: Outcome::Skipped(reason),
        }
    }

    fn finish(&mut self, from: Option<String>, to: Option<String>, changed: bool) {
        self.outcome = if from.is_some() && from == to {
            Outcome::Reinstalled
        } else if changed {
            Outcome::Updated
        } else {
            Outcome::Reinstalled
        };
        self.from = from;
        self.to = to;
    }

    /// `0.1.6 → 0.1.7`, `0.1.7`, or `-` when the installer never reported one.
    fn version_cell(&self) -> String {
        match (&self.from, &self.to) {
            (Some(from), Some(to)) if from == to => to.clone(),
            (Some(from), Some(to)) => format!("{from} → {to}"),
            (None, Some(to)) => to.clone(),
            (Some(from), None) => from.clone(),
            (None, None) => "-".to_string(),
        }
    }

    fn status_cell(&self, check: bool) -> String {
        match &self.outcome {
            Outcome::Updated => "updated".green().to_string(),
            Outcome::Reinstalled => "reinstalled".green().to_string(),
            Outcome::Current if check => "current".dimmed().to_string(),
            Outcome::Current => "up to date".dimmed().to_string(),
            Outcome::Available { known: true } if check => "available".yellow().to_string(),
            Outcome::Available { known: false } if check => {
                "rebuild (version unknown)".dimmed().to_string()
            }
            Outcome::Available { .. } => "pending".yellow().to_string(),
            Outcome::Skipped(reason) => format!("skipped ({reason})").yellow().to_string(),
            Outcome::Failed => "failed".red().to_string(),
        }
    }
}

fn print_rows(rows: &[Row], check: bool) {
    let width = rows
        .iter()
        .map(|row| row.name.chars().count())
        .max()
        .unwrap_or(4)
        .max(4);
    println!(
        "{:<width$}  {:<18}  {}",
        "NAME",
        "VERSION",
        if check { "STATUS" } else { "RESULT" }
    );
    for row in rows {
        println!(
            "{:<width$}  {:<18}  {}",
            row.name,
            row.version_cell(),
            row.status_cell(check)
        );
    }

    let count =
        |wanted: fn(&Outcome) -> bool| rows.iter().filter(|row| wanted(&row.outcome)).count();
    let skipped = count(|outcome| matches!(outcome, Outcome::Skipped(_)));
    let failed = count(|outcome| matches!(outcome, Outcome::Failed));

    println!();
    if check {
        let available = count(|outcome| matches!(outcome, Outcome::Available { .. }));
        let current = count(|outcome| matches!(outcome, Outcome::Current));
        println!("{available} update(s) available, {current} current, {skipped} skipped");
    } else {
        let updated = count(|outcome| matches!(outcome, Outcome::Updated | Outcome::Reinstalled));
        let current = count(|outcome| matches!(outcome, Outcome::Current));
        println!("{updated} updated, {current} up to date, {skipped} skipped, {failed} failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A scratch install directory per test, so nothing touches the user's.
    fn ctx(tag: &str) -> Ctx {
        let dir = std::env::temp_dir().join(format!("klava-upd-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Ctx {
            install_dir: dir.clone(),
            search: vec![extensions::SearchPath {
                dir,
                origin: "data",
            }],
        }
    }

    fn ext(ctx: &Ctx, name: &str) -> InstalledExtension {
        InstalledExtension {
            name: name.into(),
            path: ctx.install_dir.join(name),
            config: json!({"enabled": true}),
            fail_closed: false,
            origin: None,
        }
    }

    fn record(source: &str, version: Option<&str>, sha: &str) -> SourceRecord {
        SourceRecord {
            source: source.into(),
            version: version.map(str::to_string),
            sha256: sha.into(),
            installed_at: "2026-10-19T00:00:00+00:00".into(),
        }
    }

    #[test]
    fn cargo_search_ignores_the_note_line() {
        let text = "klava-compress = \"0.1.7\"    # compress tool output\n\
                    note: to learn more about a package, run `cargo info <name>`\n";
        assert_eq!(version_from_cargo_search(text), Some("0.1.7".into()));
    }

    #[test]
    fn cargo_search_of_nothing_is_unknown() {
        assert_eq!(version_from_cargo_search("error: no crates found"), None);
        assert_eq!(version_from_cargo_search(""), None);
    }

    #[test]
    fn npm_view_takes_the_last_line() {
        assert_eq!(version_from_npm_view("0.1.5\n"), Some("0.1.5".into()));
        assert_eq!(
            version_from_npm_view("npm warn something\n0.1.5\n"),
            Some("0.1.5".into())
        );
        assert_eq!(version_from_npm_view("   \n"), None);
    }

    #[test]
    fn a_local_crate_version_is_read_from_its_manifest() {
        let dir = std::env::temp_dir().join(format!("klava-upd-crate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"x\"\nversion = \"1.2.3\"\n",
        )
        .unwrap();
        assert_eq!(crate_manifest_version(&dir), Some("1.2.3".into()));

        std::fs::write(
            dir.join("Cargo.toml"),
            "[workspace.package]\nversion = \"0.4.0\"\n",
        )
        .unwrap();
        assert_eq!(crate_manifest_version(&dir), Some("0.4.0".into()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unmanaged_extension_is_skipped() {
        let c = ctx("unmanaged");
        let mut demo = ext(&c, "demo");
        demo.path = PathBuf::from("/usr/share/klava/extensions/demo");
        demo.config = json!({});
        demo.origin = Some("system");
        assert!(matches!(
            plan_for(&c, &demo, Some(&record("cargo:demo", None, "aa")), false),
            Plan::Skip { .. }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_local_extension_is_updated_and_reported_up_to_date_afterwards() {
        let c = ctx("local");
        let sources = std::env::temp_dir().join(format!("klava-upd-src-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&sources);
        std::fs::create_dir_all(&sources).unwrap();

        // First "install": the file plus its provenance record.
        let src = install_local_script(&sources, "demo", "echo one");
        let installed = ext_install::install(&Source::Local(src.clone()), &c.install_dir, None)
            .expect("first install");
        let record = SourceRecord {
            source: src.display().to_string(),
            version: None,
            sha256: installed.record.sha256.clone(),
            installed_at: installed.record.installed_at.clone(),
        };
        record
            .save(&c.install_dir, &installed.name)
            .expect("record");

        // The installed extension, as discovery would see it.
        let demo = ext(&c, "demo");

        // Update with an unchanged source: same bytes, nothing to do.
        let plan = plan_for(
            &c,
            &demo,
            Some(&SourceRecord::load(&c.install_dir, "demo").expect("record")),
            false,
        );
        assert!(matches!(plan, Plan::Current { .. }), "got: {plan:?}");

        // Change the source and update for real.
        let src = install_local_script(&sources, "demo", "echo two; echo changed");
        let plan = plan_for(
            &c,
            &demo,
            Some(&SourceRecord::load(&c.install_dir, "demo").expect("record")),
            false,
        );
        assert!(matches!(plan, Plan::Available { .. }), "got: {plan:?}");

        let flags = ext_setup::Flags {
            trust: true,
            consent: Consent::Granted,
            rest: vec![],
        };
        run_in(&mut config(), flags, c.install_dir.clone()).expect("update ran");

        let after = std::fs::read_to_string(c.install_dir.join("demo")).expect("installed");
        assert!(after.contains("changed"), "got: {after}");
        let record = SourceRecord::load(&c.install_dir, "demo").expect("new record");
        assert_eq!(record.source, src.display().to_string());

        // A second update with the same source is a no-op.
        let plan = plan_for(
            &c,
            &demo,
            Some(&SourceRecord::load(&c.install_dir, "demo").expect("record")),
            false,
        );
        assert!(matches!(plan, Plan::Current { .. }), "got: {plan:?}");

        let _ = std::fs::remove_dir_all(&sources);
        let _ = std::fs::remove_dir_all(&c.install_dir);
    }

    #[test]
    fn a_missing_record_is_skipped() {
        let c = ctx("norecord");
        assert!(matches!(
            plan_for(&c, &ext(&c, "demo"), None, false),
            Plan::Skip { .. }
        ));
    }

    #[test]
    fn an_unknown_registry_version_means_reinstall() {
        // `latest_version` cannot be exercised offline, so an unknown (None)
        // must not turn into "current".
        let c = ctx("unknown");
        let plan = plan_for(
            &c,
            &ext(&c, "demo"),
            Some(&record("cargo:no-such-extension-xyz", Some("1.0.0"), "aa")),
            false,
        );
        assert!(matches!(plan, Plan::Available { .. }));
    }

    #[test]
    fn a_pinned_source_reports_what_it_drops() {
        let c = ctx("pinned");
        let plan = plan_for(
            &c,
            &ext(&c, "demo"),
            Some(&record(
                "cargo:no-such-extension-xyz@1.0.0",
                Some("1.0.0"),
                "aa",
            )),
            false,
        );
        match plan {
            Plan::Available {
                target,
                pinned_from,
                ..
            } => {
                assert_eq!(target.label(), "cargo:no-such-extension-xyz");
                assert_eq!(
                    pinned_from.as_deref(),
                    Some("cargo:no-such-extension-xyz@1.0.0")
                );
            }
            other => panic!("expected an update plan, got {other:?}"),
        }
    }

    #[test]
    fn force_reinstalls_a_pinned_source_even_when_current() {
        let c = ctx("force");
        let plan = plan_for(
            &c,
            &ext(&c, "demo"),
            Some(&record("cargo:x@1.0.0", Some("1.0.0"), "aa")),
            true,
        );
        assert!(matches!(plan, Plan::Available { .. }));
    }

    #[test]
    fn update_selects_everything_by_default_and_deduplicates_names() {
        let c = ctx("select");
        let installed = vec![ext(&c, "a"), ext(&c, "b")];
        assert_eq!(select(&installed, &[]).unwrap().len(), 2);
        let chosen = select(&installed, &["a".into(), "a".into(), "b".into()]).unwrap();
        assert_eq!(chosen.len(), 2);
    }

    #[test]
    fn update_rejects_an_unknown_name() {
        let c = ctx("select-reject");
        let error = select(&[ext(&c, "a")], &["missing".into()])
            .unwrap_err()
            .to_string();
        assert!(error.contains("no extension named"), "got: {error}");
    }

    /// An empty `Config` with no file behind it.
    fn config() -> Config {
        Config {
            port: 48017,
            ..Config::default()
        }
    }

    /// Install `body` as an executable named `name` in `dir`, and record the source.
    fn install_local_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_source_does_not_abort_the_batch() {
        let c = ctx("batch");
        let sources = std::env::temp_dir().join(format!("klava-upd-src-b-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&sources);
        std::fs::create_dir_all(&sources).unwrap();

        // "good" installs fine from an executable file.
        let good_src = install_local_script(&sources, "good.tar", "echo one");
        let good_install =
            ext_install::install(&Source::Local(good_src.clone()), &c.install_dir, None)
                .expect("install good");
        let good_record = SourceRecord {
            source: good_src.display().to_string(),
            version: None,
            sha256: good_install.record.sha256.clone(),
            installed_at: good_install.record.installed_at.clone(),
        };
        good_record
            .save(&c.install_dir, &good_install.name)
            .expect("record good");

        // "bad" is an executable whose recorded source is a directory with no
        // executable in it: the update reaches install and fails there.
        install_local_script(&c.install_dir, "bad", "echo old");
        let empty = sources.join("empty-dir");
        std::fs::create_dir_all(&empty).unwrap();
        let bad_record = SourceRecord {
            source: empty.display().to_string(),
            version: None,
            sha256: "deadbeef".into(),
            installed_at: "2026-10-19T00:00:00+00:00".into(),
        };
        bad_record.save(&c.install_dir, "bad").expect("record bad");

        let flags = ext_setup::Flags {
            trust: true,
            consent: Consent::Granted,
            rest: vec![],
        };
        let outcome = run_in(&mut config(), flags, c.install_dir.clone()).unwrap_err();

        // The batch reports failure and exits non-zero…
        assert!(
            outcome.to_string().contains("failed to update"),
            "got: {outcome}"
        );
        // …but the good extension was still updated.
        let good_now =
            std::fs::read_to_string(c.install_dir.join("good")).expect("good still installed");
        assert!(good_now.contains("one"), "got: {good_now}");
        let _ = std::fs::remove_dir_all(&sources);
        let _ = std::fs::remove_dir_all(&c.install_dir);
    }

    #[test]
    fn a_version_cell_shows_the_move() {
        let row = Row {
            name: "demo".into(),
            from: Some("0.1.6".into()),
            to: Some("0.1.7".into()),
            outcome: Outcome::Updated,
        };
        assert_eq!(row.version_cell(), "0.1.6 → 0.1.7");
    }

    #[test]
    fn an_unchanged_version_is_not_drawn_twice() {
        let row = Row {
            name: "demo".into(),
            from: Some("0.1.7".into()),
            to: Some("0.1.7".into()),
            outcome: Outcome::Current,
        };
        assert_eq!(row.version_cell(), "0.1.7");
    }
}
