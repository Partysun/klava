//! Running an extension's provisioning hook.
//!
//! Some extensions need a large asset the install itself should not fetch (a
//! ~573 MB model, for example). The extension declares how to get it in its
//! manifest:
//!
//! ```toml
//! [setup]
//! args = ["--fetch-model"]
//! summary = "Download the model (~573 MB)"
//! network = true
//! timeout_secs = 3600
//! ```
//!
//! The hook is the extension's **own binary** with those arguments — never a
//! shell command — so the trust already granted by `install --trust` covers it
//! exactly. klava runs it after the extension is enabled.
//!
//! Setup is best effort: the extension is enabled either way and fails open at
//! request time, so a failed download leaves a working proxy and one clear
//! message with the command to retry.
//!
//! ## When it runs
//!
//! * `install` that enables the extension, `enable`, and the first enable after
//!   an upgrade: run the hook once.
//! * `enable` when the hook already ran for the same arguments and artifact:
//!   skip it, and say how to redo it.
//! * `klava extensions setup <name>`: always run it (explicit intent).
//!
//! The record of a successful run lives in [`SetupRecord`], a TOML file next to
//! the artifact (`<name>.setup.toml`). TOML rather than a database because
//! klava has no database and one row per extension does not justify the first
//! one: this is the same pattern as `<name>.source.toml`, and it is removed by
//! `extensions remove` for free. See [`crate::ext_setup_store`].

use crate::ext_setup_store as store;
use crate::extensions::{InstalledExtension, SetupSpec};
use colored::Colorize;
use std::io::IsTerminal;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub use crate::ext_setup_store::SetupRecord;

/// How setup was authorised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Consent {
    /// The user passed `--trust` (or `--yes`) for this run.
    Granted,
    /// The user did not grant it; ask if there is a terminal.
    Ask,
    /// The user explicitly said no (`--no-setup`).
    Skip,
}

/// What the caller is asking for.
///
/// Kept separate from [`Consent`] because "may I?" and "did it already happen?"
/// are different questions: `setup` overrides the second, `--no-setup` the
/// first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    /// Run it if it has not already run for this artifact and these arguments.
    IfNeeded,
    /// Run it even if it already ran.
    Always,
}

/// Outcome of one setup attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupOutcome {
    /// The extension declares no hook.
    NoHook,
    /// The hook already ran successfully for this artifact; nothing was done.
    AlreadyDone,
    /// The user declined, or there was no terminal to ask.
    Declined,
    /// The hook ran and exited successfully.
    Succeeded,
    /// The hook ran and failed; the extension stays enabled.
    Failed { message: String },
}

impl SetupOutcome {
    /// Whether setup left the extension ready to use.
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::NoHook | Self::Succeeded | Self::AlreadyDone)
    }
}

/// Run `ext`'s setup hook, subject to `consent` and the setup record.
///
/// Never fails the caller: problems are reported as [`SetupOutcome::Failed`] so
/// the extension can stay enabled and the proxy keeps working.
///
/// Test-only: production callers must pass the effective config through
/// [`run_with`], or a hook cannot honour settings such as `model_dir`.
#[cfg(test)]
pub fn run(ext: &InstalledExtension, consent: Consent) -> SetupOutcome {
    run_with(ext, consent, Request::IfNeeded, None)
}

/// [`run`] with an explicit request and the extension's effective config.
///
/// `config` is forwarded to the hook as `$KLAVA_EXTENSION_CONFIG`, so the hook
/// can honour settings such as `model_dir` that klava merged for it. Without it
/// a hook would have to guess, and could download a model into a directory the
/// extension then never reads.
pub fn run_with(
    ext: &InstalledExtension,
    consent: Consent,
    request: Request,
    config: Option<&serde_json::Value>,
) -> SetupOutcome {
    let Some(spec) = ext.setup_spec() else {
        return SetupOutcome::NoHook;
    };

    if consent == Consent::Skip {
        return SetupOutcome::Declined;
    }

    // "Already done" only answers a `enable`-style request; an explicit `setup`
    // means run it.
    if request == Request::IfNeeded && store::is_current(ext, &spec) {
        println!(
            "{} setup for '{}' is already done; {} redoes it",
            "✓".green(),
            ext.name,
            retry_command(ext).cyan()
        );
        return SetupOutcome::AlreadyDone;
    }

    // Announced before asking, so the consent prompt shows what it consents to,
    // and not again after: one honest description per run.
    let announced = announce(ext, &spec);

    if consent == Consent::Ask && !confirm(ext, announced) {
        return SetupOutcome::Declined;
    }

    // Two setups for one extension racing on the same `.part` file would corrupt
    // it. The second caller is told to wait rather than serialize behind a long
    // download it cannot see.
    let lock = match store::Lock::acquire(ext, spec.timeout + Duration::from_secs(60)) {
        Ok(lock) => lock,
        Err(message) => {
            println!(
                "{} setup not started for '{}': {message}",
                "!".yellow(),
                ext.name
            );
            return SetupOutcome::Failed { message };
        }
    };

    // Another process may have finished between our check and the lock.
    if request == Request::IfNeeded && store::is_current(ext, &spec) {
        drop(lock);
        println!("{} setup for '{}' is already done", "✓".green(), ext.name);
        return SetupOutcome::AlreadyDone;
    }

    let result = execute(ext, &spec, config);
    drop(lock);

    match result {
        Ok(()) => {
            if let Err(e) = store::record(ext, &spec) {
                // The download worked; failing the whole command over a marker
                // would be worse than re-running setup once next time.
                println!(
                    "{} could not record setup state for '{}': {e}",
                    "!".yellow(),
                    ext.name
                );
            }
            println!("{} setup finished for '{}'", "✓".green(), ext.name);
            SetupOutcome::Succeeded
        }
        Err(message) => {
            // The extension is deliberately left enabled: it fails open, and a
            // network hiccup must not silently turn a feature off.
            println!(
                "{} setup failed for '{}': {message}",
                "!".yellow(),
                ext.name
            );
            println!("  the extension stays enabled and passes requests through unchanged");
            println!("  retry with: {}", retry_command(ext).cyan());
            SetupOutcome::Failed { message }
        }
    }
}

/// The command a user can run to retry setup.
fn retry_command(ext: &InstalledExtension) -> String {
    format!("klava extensions setup {}", ext.name)
}

/// Print what is about to happen, including the network warning.
///
/// Printed even when `--trust` already granted consent: a hook that fetches
/// hundreds of megabytes should never start invisibly.
///
/// Says "Setup for", not "Running setup": with [`Consent::Ask`] this is printed
/// before the prompt, so it must still be true when the answer is no. Returns
/// how to refer to the planned run, so the prompt can say "Run the setup for X?"
/// without describing it twice.
fn announce(ext: &InstalledExtension, spec: &SetupSpec) -> String {
    let label = if spec.summary.is_empty() {
        format!("'{}'", ext.name)
    } else {
        format!("'{}' ({})", ext.name, spec.summary)
    };
    println!("Setup for {label}");
    println!("  {} {}", ext.path.display(), spec.args.join(" "));
    if spec.network {
        println!(
            "  {} this downloads data from the network",
            "network:".yellow()
        );
    }
    label
}

/// Ask for consent on a terminal; anything but `y` is a no.
///
/// Without a terminal the answer is no: an unattended run must not start a
/// large download because someone forgot a flag.
fn confirm(ext: &InstalledExtension, label: String) -> bool {
    use std::io::Write;

    if !std::io::stdin().is_terminal() {
        println!(
            "Skipping setup for '{}': no terminal to confirm. Run '{}' to do it now.",
            ext.name,
            retry_command(ext)
        );
        return false;
    }

    print!("Run the setup for {label}? [y/N] ");
    let _ = std::io::stdout().flush();

    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim(), "y" | "Y" | "yes" | "YES")
}

/// Spawn the hook with inherited stdio and wait, bounded by the manifest timeout.
///
/// Inherited stdio is deliberate: a download prints its progress, and the
/// request-path output caps (8 MiB) must not apply here.
fn execute(
    ext: &InstalledExtension,
    spec: &SetupSpec,
    config: Option<&serde_json::Value>,
) -> std::result::Result<(), String> {
    let mut command = Command::new(&ext.path);
    command
        .args(&spec.args)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    if let Some(config) = config {
        match serde_json::to_string(config) {
            Ok(text) => {
                command.env(store::CONFIG_ENV, text);
            }
            Err(e) => return Err(format!("cannot pass config to setup: {e}")),
        }
    }

    let mut child = crate::utils::spawn_retrying_busy(&mut command)
        .map_err(|e| format!("cannot start {}: {e}", ext.path.display()))?;

    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                // `status` already renders as "exit status: 7"; saying "exited
                // with" around it reads as "exited with exit status: 7".
                let why = match status.code() {
                    Some(code) => format!("exit code {code}"),
                    None => status.to_string(),
                };
                return Err(why);
            }
            Ok(None) if started.elapsed() >= spec.timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "timed out after {}s (manifest: setup.timeout_secs)",
                    spec.timeout.as_secs()
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(e) => return Err(format!("cannot wait for setup: {e}")),
        }
    }
}

/// The setup-related flags of `install` and `enable`, parsed once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flags {
    /// The user passed `--trust`/`--yes`/`-y`.
    pub trust: bool,
    /// How setup is authorised.
    pub consent: Consent,
    /// The arguments with the setup flags removed, for the caller to parse.
    pub rest: Vec<String>,
}

/// Parse `--trust`/`--yes`/`-y`, `--no-setup` from `args`.
///
/// One parser for both commands: a flag must not mean different things
/// depending on which verb it follows.
pub fn take_flags(args: &[String]) -> Flags {
    let mut trust = false;
    let mut skip = false;
    let mut rest = Vec::with_capacity(args.len());

    for arg in args {
        match arg.as_str() {
            "--trust" | "-y" | "--yes" => trust = true,
            // The narrow opt-out wins regardless of order.
            "--no-setup" => skip = true,
            other => rest.push(other.to_string()),
        }
    }

    let consent = if skip {
        Consent::Skip
    } else if trust {
        Consent::Granted
    } else {
        Consent::Ask
    };

    Flags {
        trust,
        consent,
        rest,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::{Path, PathBuf};

    /// A fresh directory per test, so parallel tests cannot see each other.
    fn temp_dir(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("klava-e2e-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn ext(config: serde_json::Value) -> InstalledExtension {
        InstalledExtension {
            name: "demo".into(),
            path: PathBuf::from("/bin/true"),
            config,
            fail_closed: false,
            origin: None,
        }
    }

    #[test]
    fn no_setup_table_means_no_hook() {
        assert_eq!(
            run(&ext(json!({"enabled": true})), Consent::Granted),
            SetupOutcome::NoHook
        );
        assert_eq!(run(&ext(json!({})), Consent::Ask), SetupOutcome::NoHook);
    }

    #[test]
    fn a_setup_table_is_parsed_into_a_spec() {
        let spec = ext(json!({
            "setup": {
                "args": ["--fetch-model"],
                "summary": "Download the model",
                "network": true,
                "timeout_secs": 3600
            }
        }))
        .setup_spec()
        .expect("spec");

        assert_eq!(spec.args, vec!["--fetch-model"]);
        assert_eq!(spec.summary, "Download the model");
        assert!(spec.network);
        assert_eq!(spec.timeout, Duration::from_secs(3600));
    }

    #[test]
    fn a_setup_table_without_args_still_runs_the_binary() {
        let spec = ext(json!({"setup": {"summary": "warm up"}}))
            .setup_spec()
            .expect("spec");
        assert!(spec.args.is_empty());
        assert!(!spec.network, "network is opt-in");
        assert_eq!(spec.timeout, crate::extensions::DEFAULT_SETUP_TIMEOUT);
    }

    #[test]
    fn a_malformed_setup_table_is_ignored_not_fatal() {
        assert!(
            ext(json!({"setup": {"args": "not-an-array"}}))
                .setup_spec()
                .is_none()
        );
        assert!(
            ext(json!({"setup": {"args": [1, 2]}}))
                .setup_spec()
                .is_none()
        );
        assert!(ext(json!({"setup": "nonsense"})).setup_spec().is_none());
    }

    #[test]
    fn a_zero_timeout_falls_back_to_the_default() {
        let spec = ext(json!({"setup": {"args": [], "timeout_secs": 0}}))
            .setup_spec()
            .expect("spec");
        assert_eq!(spec.timeout, crate::extensions::DEFAULT_SETUP_TIMEOUT);
    }

    #[test]
    fn skip_declines_without_running_anything() {
        let outcome = run(
            &ext(json!({"setup": {"args": ["--fetch-model"]}})),
            Consent::Skip,
        );
        assert_eq!(outcome, SetupOutcome::Declined);
        assert!(!outcome.is_ready());
    }

    #[test]
    fn ask_without_a_terminal_declines() {
        // The test runner is not a terminal, so Ask must not start anything.
        let outcome = run(
            &ext(json!({"setup": {"args": ["--fetch-model"], "network": true}})),
            Consent::Ask,
        );
        assert_eq!(outcome, SetupOutcome::Declined);
    }

    #[test]
    fn trust_flags_are_recognised_and_removed() {
        let args: Vec<String> = ["--trust", "--name", "x"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let flags = take_flags(&args);
        assert!(flags.trust);
        assert_eq!(flags.consent, Consent::Granted);
        assert_eq!(flags.rest, vec!["--name", "x"]);
    }

    #[test]
    fn no_setup_flag_wins_over_trust() {
        for order in [["--trust", "--no-setup"], ["--no-setup", "--trust"]] {
            let args: Vec<String> = order.iter().map(|s| s.to_string()).collect();
            let flags = take_flags(&args);
            assert_eq!(
                flags.consent,
                Consent::Skip,
                "--no-setup must opt out: {order:?}"
            );
            assert!(flags.trust, "--trust is still recorded: {order:?}");
            assert!(flags.rest.is_empty());
        }
    }

    #[test]
    fn plain_arguments_keep_the_ask_default() {
        let args: Vec<String> = ["cargo:thing@1.0.0"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let flags = take_flags(&args);
        assert!(!flags.trust);
        assert_eq!(flags.consent, Consent::Ask);
        assert_eq!(flags.rest, vec!["cargo:thing@1.0.0"]);
    }

    #[test]
    fn already_done_counts_as_ready() {
        assert!(SetupOutcome::AlreadyDone.is_ready());
        assert!(SetupOutcome::Succeeded.is_ready());
        assert!(
            !SetupOutcome::Failed {
                message: "x".into()
            }
            .is_ready()
        );
    }

    // -----------------------------------------------------------------------
    // End to end, through real child processes.
    // -----------------------------------------------------------------------

    #[cfg(unix)]
    fn fake_extension(dir: &Path) -> InstalledExtension {
        use std::os::unix::fs::PermissionsExt;

        let runs = dir.join("runs");
        let path = dir.join("demo");
        // Answers `--print-manifest`, and on `--fetch-model` appends one line to
        // `runs` and prints the config it was handed, which is how the test sees
        // both that the hook ran and what it received.
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\n\
                 if [ \"$1\" = \"--print-manifest\" ]; then\n\
                   printf '[setup]\\nargs = [\"--fetch-model\"]\\nsummary = \"fetch\"\\nnetwork = true\\n'\n\
                   exit 0\n\
                 fi\n\
                 echo run >> {runs}\n\
                 printf '%s' \"$KLAVA_EXTENSION_CONFIG\" > {dir}/config.json\n",
                dir = dir.display(),
                runs = runs.display(),
            ),
        )
        .unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();

        InstalledExtension {
            name: "demo".into(),
            path,
            config: json!({
                "setup": {"args": ["--fetch-model"], "summary": "fetch", "network": true}
            }),
            fail_closed: false,
            origin: None,
        }
    }

    #[cfg(unix)]
    fn run_count(dir: &Path) -> usize {
        std::fs::read_to_string(dir.join("runs"))
            .map(|text| text.lines().count())
            .unwrap_or(0)
    }

    #[cfg(unix)]
    #[test]
    fn install_runs_the_hook_once_and_a_later_enable_skips_it() {
        let dir = temp_dir("e2e-once");
        let ext = fake_extension(&dir);

        assert_eq!(run_count(&dir), 0, "nothing runs before setup is asked for");

        // First activation: runs, records, exits 0.
        let outcome = run_with(&ext, Consent::Granted, Request::IfNeeded, None);
        assert_eq!(outcome, SetupOutcome::Succeeded);
        assert_eq!(run_count(&dir), 1);
        assert!(
            crate::ext_setup_store::SetupRecord::load(&dir, "demo").is_some(),
            "a successful run must be recorded"
        );

        // Second enable: the record says it already happened.
        let outcome = run_with(&ext, Consent::Granted, Request::IfNeeded, None);
        assert_eq!(outcome, SetupOutcome::AlreadyDone);
        assert_eq!(run_count(&dir), 1, "setup must not run twice");

        // `klava extensions setup` overrides the record.
        let outcome = run_with(&ext, Consent::Granted, Request::Always, None);
        assert_eq!(outcome, SetupOutcome::Succeeded);
        assert_eq!(run_count(&dir), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_hook_is_not_recorded_and_can_be_retried() {
        use std::os::unix::fs::PermissionsExt;

        let dir = temp_dir("e2e-fail");
        let ext = fake_extension(&dir);
        // Fail this time, counting attempts the same way.
        let runs = dir.join("runs");
        std::fs::write(
            &ext.path,
            format!("#!/bin/sh\necho run >> {}\nexit 3\n", runs.display()),
        )
        .unwrap();
        let mut perms = std::fs::metadata(&ext.path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&ext.path, perms).unwrap();

        let outcome = run_with(&ext, Consent::Granted, Request::IfNeeded, None);
        assert!(
            matches!(outcome, SetupOutcome::Failed { .. }),
            "got: {outcome:?}"
        );
        assert_eq!(run_count(&dir), 1);
        assert!(
            crate::ext_setup_store::SetupRecord::load(&dir, "demo").is_none(),
            "a failure must not look like success"
        );

        // Still pending, so it is offered again.
        let outcome = run_with(&ext, Consent::Granted, Request::IfNeeded, None);
        assert!(matches!(outcome, SetupOutcome::Failed { .. }));
        assert_eq!(run_count(&dir), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn the_hook_receives_the_effective_config() {
        let dir = temp_dir("e2e-config");
        let ext = fake_extension(&dir);
        let config = json!({"model_dir": "/models/here", "threshold": 0.5});

        assert_eq!(
            run_with(&ext, Consent::Granted, Request::IfNeeded, Some(&config)),
            SetupOutcome::Succeeded
        );

        let seen: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("config.json")).unwrap())
                .expect("hook wrote the config it saw");
        assert_eq!(seen["model_dir"], "/models/here");
        assert_eq!(seen["threshold"], 0.5);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn no_setup_never_starts_the_hook() {
        let dir = temp_dir("e2e-skip");
        let ext = fake_extension(&dir);

        assert_eq!(
            run_with(&ext, Consent::Skip, Request::Always, None),
            SetupOutcome::Declined
        );
        assert_eq!(run_count(&dir), 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn the_lock_file_is_cleaned_up_even_when_the_hook_fails() {
        let dir = temp_dir("e2e-lock");
        let ext = fake_extension(&dir);
        std::fs::write(&ext.path, "#!/bin/sh\nexit 1\n").unwrap();

        let _ = run_with(&ext, Consent::Granted, Request::IfNeeded, None);
        assert!(
            !crate::ext_setup_store::Lock::path_for(&dir, "demo").exists(),
            "a failed run must not leave a lock behind"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
