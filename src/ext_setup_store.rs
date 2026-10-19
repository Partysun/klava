//! What klava remembers about an extension's setup run.
//!
//! One TOML file next to the artifact: `<name>.setup.toml`.
//!
//! ```toml
//! args = ["--fetch-model"]
//! artifact_sha256 = "df6e6552…"   # from `<name>.source.toml`, when present
//! ran_at = "2026-10-19T14:40:00+03:00"
//! ```
//!
//! TOML beside the artifact, not a database, for the same reasons
//! `<name>.source.toml` is: klava has no database, one row per extension does
//! not justify the first one, the record travels with the executable if it is
//! copied, and `extensions remove` cleans it up with the other sidecars.
//!
//! The file is written **only after the hook exits 0**, so its presence means
//! "this succeeded", not "this was attempted". A failed or interrupted download
//! leaves no file and setup is retried next time.

use crate::ext_install::SourceRecord;
use crate::extensions::{InstalledExtension, SetupSpec};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Environment variable carrying the extension's effective config to the hook.
///
/// The hook runs as a standalone program that klava does not link against, so
/// this is how it learns the merged settings (manifest + user) — notably
/// `model_dir`, which decides where a downloaded model must land.
pub const CONFIG_ENV: &str = "KLAVA_EXTENSION_CONFIG";

/// A successful setup run, as recorded for one extension.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupRecord {
    /// Arguments the hook ran with. A changed hook means run it again.
    #[serde(default)]
    pub args: Vec<String>,
    /// Fingerprint of the artifact the hook provisioned, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_sha256: Option<String>,
    /// RFC 3339 timestamp of the successful run.
    pub ran_at: String,
}

impl SetupRecord {
    /// Path of the record for `name` inside `dir`.
    pub fn path_for(dir: &Path, name: &str) -> PathBuf {
        dir.join(format!("{name}.setup.toml"))
    }

    /// Read the record for `name`, if any.
    ///
    /// A corrupt record is treated as absent: the worst outcome is one extra
    /// setup run, which is cheaper than refusing to enable the extension.
    pub fn load(dir: &Path, name: &str) -> Option<Self> {
        let text = std::fs::read_to_string(Self::path_for(dir, name)).ok()?;
        toml::from_str(&text).ok()
    }

    /// Write the record atomically.
    pub fn save(&self, dir: &Path, name: &str) -> Result<()> {
        let path = Self::path_for(dir, name);
        let text = toml::to_string_pretty(self)?;
        let temp = path.with_extension("toml.tmp");
        std::fs::write(&temp, text)?;
        std::fs::rename(&temp, &path)?;
        Ok(())
    }

    /// Whether this record covers `spec` for the artifact at `sha`.
    ///
    /// Both parts matter: the same binary with different arguments has not been
    /// provisioned yet, and neither has a new build of it.
    ///
    /// Any disagreement re-runs setup. Provisioning twice costs a download that
    /// notices it already has the file; skipping it when the artifact really did
    /// change leaves the extension permanently unprovisioned.
    pub fn covers(&self, spec: &SetupSpec, sha: Option<&str>) -> bool {
        if self.args != spec.args {
            return false;
        }
        match (self.artifact_sha256.as_deref(), sha) {
            (Some(recorded), Some(sha)) => recorded == sha,
            // A record without a fingerprint covers only a binary whose
            // fingerprint is also unknown: once provenance appears, the artifact
            // may have been replaced under us.
            (None, None) => true,
            (None, Some(_)) => false,
            (Some(_), None) => false,
        }
    }
}

/// The directory holding an extension's sidecars.
fn dir_of(ext: &InstalledExtension) -> PathBuf {
    ext.path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The artifact fingerprint recorded at install time, if there is one.
fn artifact_sha(ext: &InstalledExtension) -> Option<String> {
    SourceRecord::load(&dir_of(ext), &ext.name).map(|r| r.sha256)
}

/// Whether `ext`'s hook already ran successfully for this artifact and these
/// arguments.
pub fn is_current(ext: &InstalledExtension, spec: &SetupSpec) -> bool {
    let sha = artifact_sha(ext);
    SetupRecord::load(&dir_of(ext), &ext.name)
        .map(|record| record.covers(spec, sha.as_deref()))
        .unwrap_or(false)
}

/// What a `show`/`list` needs to report setup state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// The manifest declares no hook.
    NoHook,
    /// A hook exists and has not run for this artifact.
    Pending,
    /// A hook ran successfully; `at` is when.
    Done { at: String },
    /// A hook ran and succeeded, but for a previous build or argument list.
    Stale,
}

impl State {
    /// One short word for a table or a status line.
    pub fn label(&self) -> &'static str {
        match self {
            Self::NoHook => "none",
            Self::Pending => "pending",
            Self::Done { .. } => "done",
            Self::Stale => "stale",
        }
    }
}

/// Classify `ext`'s setup state.
pub fn state(ext: &InstalledExtension) -> State {
    let Some(spec) = ext.setup_spec() else {
        return State::NoHook;
    };
    let Some(record) = SetupRecord::load(&dir_of(ext), &ext.name) else {
        return State::Pending;
    };
    if record.covers(&spec, artifact_sha(ext).as_deref()) {
        State::Done { at: record.ran_at }
    } else {
        State::Stale
    }
}

/// Record a successful run of `spec` against `ext`.
pub fn record(ext: &InstalledExtension, spec: &SetupSpec) -> Result<()> {
    let record = SetupRecord {
        args: spec.args.clone(),
        artifact_sha256: artifact_sha(ext),
        ran_at: chrono::Utc::now().to_rfc3339(),
    };
    record.save(&dir_of(ext), &ext.name)
}

/// An exclusive claim on one extension's setup.
///
/// Two klava processes enabling the same extension at once would otherwise run
/// two downloads into one `.part` file. The file starts with a dot so extension
/// discovery ignores it, and it carries the pid's start time only implicitly:
/// a lock older than the hook's own timeout can only be abandoned.
#[derive(Debug)]
pub struct Lock {
    path: PathBuf,
}

impl Lock {
    /// Path of the lock file for `name` inside `dir`.
    pub fn path_for(dir: &Path, name: &str) -> PathBuf {
        dir.join(format!(".{name}.setup.lock"))
    }

    /// Take the lock, or explain who holds it.
    ///
    /// `stale_after` should be a little longer than the hook's timeout: a lock
    /// that outlives its own hook can only belong to a process that died.
    pub fn acquire(ext: &InstalledExtension, stale_after: Duration) -> Result<Self, String> {
        let dir = dir_of(ext);
        let path = Self::path_for(&dir, &ext.name);

        for attempt in 0..2 {
            match std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&path)
            {
                Ok(mut file) => {
                    use std::io::Write;
                    let _ = writeln!(file, "pid {} started this setup", std::process::id());
                    return Ok(Self { path });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && attempt == 0 => {
                    let age = std::fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|modified| SystemTime::now().duration_since(modified).ok());
                    match age {
                        Some(age) if age >= stale_after => {
                            // Abandoned: take over.
                            let _ = std::fs::remove_file(&path);
                        }
                        _ => {
                            return Err(format!("another setup is running ({})", path.display()));
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    return Err(format!("another setup is running ({})", path.display()));
                }
                Err(e) => {
                    return Err(format!("cannot create {}: {e}", path.display()));
                }
            }
        }
        Err(format!("cannot take {}", path.display()))
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    fn spec(args: &[&str]) -> SetupSpec {
        SetupSpec {
            args: args.iter().map(|s| s.to_string()).collect(),
            summary: String::new(),
            network: true,
            timeout: Duration::from_secs(60),
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("klava-setup-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn ext_in(dir: &Path, config: serde_json::Value) -> InstalledExtension {
        InstalledExtension {
            name: "demo".into(),
            path: dir.join("demo"),
            config,
            fail_closed: false,
            origin: None,
        }
    }

    fn with_hook() -> serde_json::Value {
        json!({"setup": {"args": ["--fetch-model"], "network": true}})
    }

    #[test]
    fn a_record_round_trips() {
        let dir = temp_dir("round");
        let record = SetupRecord {
            args: vec!["--fetch-model".into()],
            artifact_sha256: Some("abc".into()),
            ran_at: "2026-10-19T14:00:00+00:00".into(),
        };
        record.save(&dir, "demo").unwrap();

        let loaded = SetupRecord::load(&dir, "demo").expect("record");
        assert_eq!(loaded, record);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_record_covers_only_the_same_arguments_and_artifact() {
        let record = SetupRecord {
            args: vec!["--fetch-model".into()],
            artifact_sha256: Some("abc".into()),
            ran_at: "now".into(),
        };
        assert!(record.covers(&spec(&["--fetch-model"]), Some("abc")));
        assert!(!record.covers(&spec(&["--other"]), Some("abc")));
        assert!(!record.covers(&spec(&["--fetch-model"]), Some("def")));
    }

    #[test]
    fn a_known_artifact_is_never_matched_by_a_record_without_one() {
        let record = SetupRecord {
            args: vec!["--fetch-model".into()],
            artifact_sha256: None,
            ran_at: "now".into(),
        };
        assert!(record.covers(&spec(&["--fetch-model"]), None));
        assert!(!record.covers(&spec(&["--fetch-model"]), Some("abc")));
    }

    #[test]
    fn no_hook_means_no_setup_state() {
        let dir = temp_dir("nohook");
        let ext = ext_in(&dir, json!({"enabled": true}));
        assert_eq!(state(&ext), State::NoHook);
        assert!(!is_current(&ext, &spec(&["--fetch-model"])));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_fresh_extension_is_pending_until_recorded() {
        let dir = temp_dir("pending");
        let ext = ext_in(&dir, with_hook());
        assert_eq!(state(&ext), State::Pending);

        record(&ext, &ext.setup_spec().unwrap()).unwrap();
        assert!(matches!(state(&ext), State::Done { .. }));
        assert!(is_current(&ext, &ext.setup_spec().unwrap()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_new_artifact_makes_the_record_stale() {
        let dir = temp_dir("stale");
        std::fs::write(dir.join("demo"), "bin").unwrap();
        let sources = SourceRecord {
            source: "cargo:demo".into(),
            version: None,
            sha256: "aaaa".into(),
            installed_at: "2026-10-19T14:00:00+00:00".into(),
        };
        sources.save(&dir, "demo").unwrap();

        let ext = ext_in(&dir, with_hook());
        record(&ext, &ext.setup_spec().unwrap()).unwrap();
        assert!(is_current(&ext, &ext.setup_spec().unwrap()));

        // An upgrade rewrites the provenance record with a new fingerprint.
        let upgraded = SourceRecord {
            sha256: "bbbb".into(),
            ..sources
        };
        upgraded.save(&dir, "demo").unwrap();

        assert_eq!(state(&ext), State::Stale);
        assert!(!is_current(&ext, &ext.setup_spec().unwrap()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_record_is_treated_as_missing() {
        let dir = temp_dir("corrupt");
        std::fs::write(dir.join("demo.setup.toml"), "args = [not toml").unwrap();
        let ext = ext_in(&dir, with_hook());
        assert_eq!(state(&ext), State::Pending);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_lock_is_exclusive_and_removed_on_drop() {
        let dir = temp_dir("lock");
        let ext = ext_in(&dir, with_hook());

        let lock = Lock::acquire(&ext, Duration::from_secs(60)).expect("first");
        let second = Lock::acquire(&ext, Duration::from_secs(60));
        assert!(second.is_err(), "a second setup must not start");

        drop(lock);
        assert!(!Lock::path_for(&dir, "demo").exists());
        assert!(Lock::acquire(&ext, Duration::from_secs(60)).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_abandoned_lock_is_taken_over() {
        let dir = temp_dir("stale-lock");
        let ext = ext_in(&dir, with_hook());
        let path = Lock::path_for(&dir, "demo");
        std::fs::write(&path, "pid 1 started this setup\n").unwrap();

        // Zero grace: any existing lock counts as abandoned.
        let lock = Lock::acquire(&ext, Duration::ZERO).expect("takeover");
        assert!(path.exists());
        drop(lock);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_lock_file_is_hidden_from_discovery() {
        let dir = temp_dir("hidden");
        let ext = ext_in(&dir, with_hook());
        let lock = Lock::acquire(&ext, Duration::from_secs(60)).unwrap();
        assert!(
            crate::extensions::discover(&dir).is_empty(),
            "a lock file must not look like an extension"
        );
        drop(lock);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
