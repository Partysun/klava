//! Extension runtime.
//!
//! **klava knows no extension by name.** It discovers executables at runtime,
//! launches them as child processes, and talks to them over stdio using JSON.
//! No extension is compiled in, and installing one never requires rebuilding
//! klava.
//!
//! ## Why processes, not libraries
//!
//! An extension must be installable *after* klava is installed — no rebuilding,
//! no matching compiler, no shared ABI. That rules out everything that links
//! into klava:
//!
//! * a compiled-in crate needs the user to build klava themselves;
//! * a `.so` needs a stable ABI, which Rust does not provide, and cannot work
//!   with `panic = "abort"` (klava's release profile);
//! * a WASM host would add a large runtime to every install for a feature most
//!   users do not enable.
//!
//! A child process is language-agnostic, isolated by the OS, and killable on
//! timeout. The cost is one `spawn` per request that needs an extension, which
//! is irrelevant next to a model call.
//!
//! ## Discovery
//!
//! Extensions are files in the extensions directory:
//!
//! ```text
//! ~/.config/klava/extensions/<name>            # executable, any language
//! ~/.config/klava/extensions/<name>.toml       # optional manifest
//! ```
//!
//! Nothing is compiled in, and klava never names a specific extension. If the
//! directory is missing or empty, the subsystem does nothing.
//!
//! ## Protocol
//!
//! klava writes one JSON object to the child's stdin and reads one JSON object
//! from stdout. Anything on stderr is forwarded as warning logs. Full details
//! are in `EXTENSIONS.md`; the shapes are [`ExtensionRequest`] and
//! [`ExtensionResponse`].
//!
//! Extensions fail **open**: a crash, a timeout, malformed output, or a nonzero
//! exit leaves the request untouched. A gate must say so explicitly.

use crate::config::Config;
use crate::error::Result;
use crate::ext_api::{Decision, FoldMode, NestedCallRequest, RequestReason, SourceApi};
use crate::models::openai;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

/// How long one extension gets to answer, including process startup.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);

/// Caps so a runaway extension cannot exhaust memory.
const MAX_STDOUT: usize = 8 * 1024 * 1024;
const MAX_STDERR: usize = 64 * 1024;

/// How many times one extension may re-run with a decide answer in a single
/// client request. Bounds a classifier loop an extension could otherwise cause.
const MAX_DECIDE_ROUNDS: usize = 2;

// ---------------------------------------------------------------------------
// Wire protocol
// ---------------------------------------------------------------------------

/// What klava sends to an extension, on stdin, as one JSON object.
#[derive(Debug, Clone, Serialize)]
pub struct ExtensionRequest {
    /// Protocol version. Extensions should reject what they do not understand.
    pub protocol: u32,
    /// The extension's own settings table, already merged with its defaults.
    pub config: Value,
    /// The OpenAI-shaped chat-completions payload.
    pub payload: Value,
    /// Why the client sent this request.
    pub reason: RequestReason,
    /// Which client API it arrived on.
    pub source_api: SourceApi,
    /// The model the request will use.
    pub model: String,
    /// Nested calls already made for this client request.
    pub nested_calls_used: u32,
    /// Whether the host will accept a `nested_call` only after an approved
    /// `decide`. The extension asks for the judgment only when this is true, so
    /// there is one policy switch (the host's) rather than two.
    pub require_decide: bool,
    /// Set on the second run of an extension that asked for a `decide` call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decide: Option<crate::ext_api::DecideResult>,
}

/// What an extension may reply, on stdout, as one JSON object.
///
/// `decision` is one of `continue`, `replace`, `append`, `deny`, `decide`, or
/// `nested_call`. Unknown decisions are treated as `continue`.
#[derive(Debug, Clone, Deserialize)]
pub struct ExtensionResponse {
    #[serde(default)]
    pub decision: String,
    #[serde(default)]
    pub payload: Option<Value>,
    #[serde(default)]
    pub messages: Option<Vec<Value>>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub fail_closed: bool,
    #[serde(default)]
    pub decide: Option<crate::ext_api::DecideRequest>,
    #[serde(default)]
    pub nested_call: Option<NestedCall>,
}

/// Parameters for a nested model call, as an extension expresses them.
#[derive(Debug, Clone, Deserialize)]
pub struct NestedCall {
    /// Name of the `[[models]]` entry to call. Omitted or not configured
    /// falls back to the completion model.
    #[serde(default)]
    pub model: String,
    /// `append` (default) or `take_over`.
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub system: String,
    #[serde(default)]
    pub instruction: String,
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

impl ExtensionResponse {
    /// Translate the wire form into the internal decision.
    fn into_decision(self) -> Decision {
        match self.decision.as_str() {
            "replace" => match self.payload {
                Some(payload) => Decision::Replace(payload),
                None => Decision::Continue,
            },
            "append" => Decision::Append(self.messages.unwrap_or_default()),
            "deny" => Decision::Deny {
                reason: self.reason.unwrap_or_else(|| "denied by extension".into()),
            },
            "decide" => match self.decide {
                Some(request) => Decision::Decide(request),
                None => Decision::Continue,
            },
            "nested_call" => match self.nested_call {
                Some(call) => Decision::NestedCall(NestedCallRequest {
                    model: call.model,
                    mode: match call.mode.as_deref() {
                        Some("take_over") => FoldMode::TakeOver,
                        _ => FoldMode::Append,
                    },
                    system: call.system,
                    instruction: call.instruction,
                    max_tokens: call.max_tokens,
                }),
                None => Decision::Continue,
            },
            _ => Decision::Continue,
        }
    }
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

/// One installed extension.
#[derive(Debug, Clone)]
pub struct InstalledExtension {
    /// File stem: `~/.config/klava/extensions/<name>` → `<name>`.
    pub name: String,
    /// Absolute path to the executable.
    pub path: PathBuf,
    /// Manifest values, or an empty object when there is no `.toml`.
    pub config: Value,
    /// Whether to reject the request when this extension fails.
    pub fail_closed: bool,
    /// Search-path origin, filled in by [`discover_all`].
    pub origin: Option<&'static str>,
}

impl InstalledExtension {
    /// Whether the user's config enables this extension.
    ///
    /// The rule is the same for every extension and lives entirely in klava:
    /// `[extensions.settings.<name>].enabled`, defaulting to the manifest's
    /// `enabled`, defaulting to `false`. An extension is never active unless
    /// something says so.
    pub fn is_enabled(&self, config: &Config) -> bool {
        config
            .extensions
            .settings
            .get(&self.name)
            .and_then(|s| s.get("enabled"))
            .and_then(Value::as_bool)
            .or_else(|| self.config.get("enabled").and_then(Value::as_bool))
            .unwrap_or(false)
    }

    /// The settings table handed to the extension: manifest, then user
    /// overrides.
    ///
    /// Host-only manifest keys (`[settings]` and `notice`, which describe the
    /// extension to klava, and `[setup]`, which names its provisioning hook) are
    /// removed: they tell klava what the extension accepts and how to install
    /// it, they are not configuration the extension asked for.
    pub fn effective_config(&self, config: &Config) -> Value {
        let mut effective = self.config.clone();
        if let Some(map) = effective.as_object_mut() {
            map.remove(SETTINGS_KEY);
            map.remove(NOTICE_KEY);
            map.remove("setup");
            // The manifest's `fail_closed` is a host policy, not an extension
            // setting; `policy:` already reports it.
            map.remove("fail_closed");
        }
        if let Some(user) = config.extensions.settings.get(&self.name) {
            merge(&mut effective, user);
        }
        effective
    }

    /// What this extension accepts, as declared by its manifest's `[settings]`.
    ///
    /// Empty when the extension declares nothing (or has no manifest), which is
    /// every extension installed before manifests carried this.
    pub fn declared_settings(&self) -> Vec<DeclaredSetting> {
        let Some(table) = self.config.get(SETTINGS_KEY).and_then(Value::as_object) else {
            return Vec::new();
        };
        table
            .iter()
            .map(|(key, value)| DeclaredSetting {
                key: key.clone(),
                default: value.clone(),
            })
            .collect()
    }

    /// A warning the extension wants the user to read before enabling it.
    pub fn notice(&self) -> Option<&str> {
        self.config
            .get(NOTICE_KEY)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
    }

    /// How to provision this extension, from the manifest's `[setup]` table.
    ///
    /// klava runs the hook once, right after the extension is enabled: some
    /// extensions need a model or another large asset that should not be
    /// downloaded by the install itself. A bad table is a warning, never a
    /// failure — the extension still loads.
    pub fn setup_spec(&self) -> Option<SetupSpec> {
        let table = self.config.get("setup")?.as_object()?;

        let args: Vec<String> = match table.get("args") {
            Some(Value::Array(items)) => {
                let parsed: Option<Vec<String>> = items
                    .iter()
                    .map(|v| v.as_str().map(str::to_string))
                    .collect();
                match parsed {
                    Some(args) => args,
                    None => {
                        tracing::warn!(
                            "extension '{}': setup.args must be an array of strings; skipping setup",
                            self.name
                        );
                        return None;
                    }
                }
            }
            Some(_) => {
                tracing::warn!(
                    "extension '{}': setup.args must be an array of strings; skipping setup",
                    self.name
                );
                return None;
            }
            None => Vec::new(),
        };

        let summary = table
            .get("summary")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let network = table
            .get("network")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let timeout = table
            .get("timeout_secs")
            .and_then(Value::as_u64)
            .filter(|secs| *secs > 0)
            .unwrap_or(DEFAULT_SETUP_TIMEOUT.as_secs());

        Some(SetupSpec {
            args,
            summary,
            network,
            timeout: Duration::from_secs(timeout),
        })
    }
}

/// One extension's provisioning hook, as declared by its manifest.
///
/// The hook is the extension's own executable with these arguments, so the
/// trust already granted by `install --trust` covers it exactly: no shell, no
/// arbitrary command string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupSpec {
    /// Arguments passed to the extension binary itself.
    pub args: Vec<String>,
    /// One line describing what setup does, shown before it runs.
    pub summary: String,
    /// Whether setup accesses the network; `--trust` is its consent.
    pub network: bool,
    /// How long setup may run.
    pub timeout: Duration,
}

/// Default time a setup hook may run. Large downloads need minutes, not the
/// 20 seconds a request gets.
pub const DEFAULT_SETUP_TIMEOUT: Duration = Duration::from_secs(300);

/// Manifest key declaring the settings the extension accepts.
///
/// Stripped from the config handed to the extension: it is documentation for
/// the user and the host, not a setting.
pub const SETTINGS_KEY: &str = "settings";

/// Manifest key carrying a warning shown before the extension is enabled.
///
/// Same rule as [`SETTINGS_KEY`]: host-only, never sent to the extension.
pub const NOTICE_KEY: &str = "notice";

/// One setting an extension declares, with its default.
///
/// The value is the extension's own default, opaque to klava: it is printed as
/// documentation and never interpreted.
#[derive(Debug, Clone, PartialEq)]
pub struct DeclaredSetting {
    pub key: String,
    pub default: Value,
}

/// One directory klava looks in for extensions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchPath {
    pub dir: PathBuf,
    /// Why this directory is in the list.
    pub origin: &'static str,
}

/// Data directory for klava, where installed extensions live.
///
/// XDG says executables and user data belong under `$XDG_DATA_HOME`, not under
/// `$XDG_CONFIG_HOME`. Falls back to `~/.local/share/klava` on Unix and
/// `%LOCALAPPDATA%\klava` on Windows.
pub fn data_dir() -> PathBuf {
    if let Some(dir) = dirs::data_local_dir() {
        return dir.join("klava");
    }
    // Last resort: next to the config file, which at least exists.
    Config::get_path()
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The canonical install target.
///
/// `$KLAVA_EXTENSIONS_DIR` wins when set, so a sandboxed or portable setup can
/// keep both discovery and installation in one place.
pub fn install_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("KLAVA_EXTENSIONS_DIR")
        && !dir.trim().is_empty()
    {
        return PathBuf::from(dir);
    }
    data_dir().join("extensions")
}

/// Every directory klava searches, in priority order.
///
/// A name found earlier wins. This is deliberately a list rather than one path
/// (see `EXTENSIONS.md`):
///
/// 1. `$KLAVA_EXTENSIONS_DIR` — explicit override, and the only entry when set.
/// 2. `<data>/extensions` — where `klava extensions install` writes.
/// 3. `/usr/local/share/klava/extensions` — installed system-wide by an admin.
/// 4. `/usr/share/klava/extensions` — distribution packages.
pub fn search_path() -> Vec<SearchPath> {
    if let Ok(dir) = std::env::var("KLAVA_EXTENSIONS_DIR")
        && !dir.trim().is_empty()
    {
        return vec![SearchPath {
            dir: PathBuf::from(dir),
            origin: "KLAVA_EXTENSIONS_DIR",
        }];
    }

    default_search_path()
}

/// The search path with no environment override applied.
///
/// Split out so it can be tested without mutating process-wide state, which is
/// unsafe under a parallel test runner.
fn default_search_path() -> Vec<SearchPath> {
    let mut paths = vec![SearchPath {
        dir: install_dir(),
        origin: "data",
    }];

    for dir in [
        "/usr/local/share/klava/extensions",
        "/usr/share/klava/extensions",
    ] {
        paths.push(SearchPath {
            dir: PathBuf::from(dir),
            origin: "system",
        });
    }

    paths
}

/// The first directory in the search path. Kept for callers that only need one.
pub fn extensions_dir() -> PathBuf {
    search_path()
        .into_iter()
        .next()
        .map(|p| p.dir)
        .unwrap_or_else(install_dir)
}

/// Every extension across the whole search path.
///
/// Names are deduplicated with the search path acting as priority: the first
/// directory that has a given name wins, and later ones are ignored (not merged).
pub fn discover_all() -> Vec<InstalledExtension> {
    discover_in(&search_path())
}

/// [`discover_all`], against an explicit search path (used by tests).
pub fn discover_in(search: &[SearchPath]) -> Vec<InstalledExtension> {
    let mut found: Vec<InstalledExtension> = Vec::new();
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    for entry in search {
        for ext in discover(&entry.dir) {
            if seen.insert(ext.name.clone()) {
                found.push(InstalledExtension {
                    origin: Some(entry.origin),
                    ..ext
                });
            } else {
                tracing::debug!(
                    "extension '{}' in {} is shadowed by an earlier directory",
                    ext.name,
                    entry.dir.display()
                );
            }
        }
    }

    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

/// Every runnable extension in `dir`, sorted by name.
///
/// A file is an extension when it is executable and is not a manifest
/// (`*.toml`), a hidden file, or a backup. Unreadable directories yield nothing:
/// a missing extensions directory is normal, not an error.
pub fn discover(dir: &Path) -> Vec<InstalledExtension> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut found: Vec<InstalledExtension> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        if name.starts_with('.') || path.extension().and_then(|e| e.to_str()) == Some("toml") {
            continue;
        }
        if !is_executable(&path) {
            tracing::debug!(
                "extension candidate {} is not executable; skipping",
                path.display()
            );
            continue;
        }

        let manifest = dir.join(format!("{name}.toml"));
        let (config, fail_closed) = match std::fs::read_to_string(&manifest) {
            Ok(text) => match toml_to_json(&text) {
                Ok(value) => {
                    let fail_closed = value
                        .get("fail_closed")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    (value, fail_closed)
                }
                Err(e) => {
                    tracing::warn!("extension '{name}': bad manifest {manifest:?}: {e}");
                    continue;
                }
            },
            Err(_) => (json!({}), false),
        };

        found.push(InstalledExtension {
            name,
            path,
            config,
            fail_closed,
            origin: None,
        });
    }

    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

/// Parse a TOML manifest into the same JSON shape used for settings.
fn toml_to_json(text: &str) -> std::result::Result<Value, String> {
    let value: toml::Value = toml::from_str(text).map_err(|e| e.to_string())?;
    serde_json::to_value(value).map_err(|e| e.to_string())
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.is_file()
        && std::fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Recursively overlay `user` onto `base`. Objects merge; arrays and scalars
/// replace.
fn merge(base: &mut Value, user: &Value) {
    match (base, user) {
        (Value::Object(base_map), Value::Object(user_map)) => {
            for (k, v) in user_map {
                merge(base_map.entry(k.clone()).or_insert(Value::Null), v);
            }
        }
        (base, user) => *base = user.clone(),
    }
}

// ---------------------------------------------------------------------------
// Running
// ---------------------------------------------------------------------------

/// Result of running the request-side extension chain.
pub struct RequestOutcome {
    /// The payload to send upstream.
    pub payload: Value,
    /// Set when an extension already produced the answer (a nested call folded
    /// in "take over" mode); the caller renders this instead of calling the
    /// provider.
    pub canned_response: Option<Value>,
}

/// The extension set, resolved once per proxy instance.
pub struct Extensions {
    active: Vec<InstalledExtension>,
}

impl Extensions {
    /// Discover and activate installed extensions.
    pub fn from_config(config: &Config) -> Self {
        let installed = discover_all();
        if installed.is_empty() {
            tracing::debug!("no extensions installed");
            return Self { active: Vec::new() };
        }

        let active: Vec<InstalledExtension> = installed
            .into_iter()
            .filter(|ext| {
                let enabled = ext.is_enabled(config);
                if enabled {
                    tracing::info!("extension '{}' active: {}", ext.name, ext.path.display());
                } else {
                    tracing::debug!("extension '{}' installed but disabled", ext.name);
                }
                enabled
            })
            .collect();

        Self { active }
    }

    /// Every installed extension, whether enabled or not. Used by the CLI.
    pub fn installed() -> Vec<InstalledExtension> {
        discover_all()
    }

    /// Whether any extension is active.
    pub fn is_empty(&self) -> bool {
        self.active.is_empty()
    }

    /// Names of active extensions, for startup logging.
    pub fn names(&self) -> Vec<&str> {
        self.active.iter().map(|e| e.name.as_str()).collect()
    }

    /// Run the request-side extension chain in name order.
    ///
    /// Later extensions see earlier extensions' output. A nested call may need a
    /// nested model call, so this takes the HTTP client.
    ///
    /// An extension may ask the host to run the **decide model** over state it
    /// supplies (`decision: "decide"`). The host answers by re-running that same
    /// extension with `decide` filled in, so the question and the evidence stay
    /// in the extension while the model, credentials and cost stay in klava.
    pub async fn on_request(
        &self,
        client: &reqwest::Client,
        config: &Config,
        mut payload: Value,
        reason: RequestReason,
        source_api: SourceApi,
    ) -> Result<RequestOutcome> {
        let mut nested_calls_used = 0u32;

        for ext in &self.active {
            let name = ext.name.as_str();
            let mut decide_result: Option<crate::ext_api::DecideResult> = None;
            let mut decide_rounds = 0usize;

            loop {
                let request = ExtensionRequest {
                    protocol: 1,
                    config: ext.effective_config(config),
                    payload: payload.clone(),
                    reason,
                    source_api,
                    model: payload
                        .get("model")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    nested_calls_used,
                    require_decide: config.require_decide_for_nested_calls,
                    decide: decide_result.clone(),
                };

                let response = match run_extension(ext, &request).await {
                    Ok(response) => response,
                    Err(e) => {
                        if ext.fail_closed {
                            return Err(crate::error::Error::Transform(format!(
                                "extension '{name}' failed: {e}"
                            )));
                        }
                        tracing::warn!("extension '{name}' failed, payload unchanged: {e}");
                        break;
                    }
                };

                // The extension may override its own failure policy for this call.
                let fail_closed = response.fail_closed || ext.fail_closed;
                let decision = response.into_decision();

                match decision {
                    Decision::Continue => break,
                    Decision::Replace(next) => {
                        payload = next;
                        break;
                    }
                    Decision::Append(messages) => {
                        if let Some(list) =
                            payload.get_mut("messages").and_then(Value::as_array_mut)
                        {
                            list.extend(messages);
                        }
                        break;
                    }
                    Decision::Deny { reason } => {
                        if fail_closed {
                            return Err(crate::error::Error::Transform(format!(
                                "extension '{name}' denied the request: {reason}"
                            )));
                        }
                        tracing::warn!("extension '{name}' denied but is fail-open; continuing");
                        break;
                    }
                    Decision::Decide(request) => {
                        if decide_rounds >= MAX_DECIDE_ROUNDS {
                            tracing::warn!(
                                "extension '{name}' asked for decide {} times in one request; \
                                 leaving the request unchanged",
                                decide_rounds
                            );
                            break;
                        }
                        decide_rounds += 1;

                        match crate::decision::ask(config, &request).await {
                            Ok(result) => {
                                tracing::info!(
                                    "extension '{name}' decide: {} question(s), approve={} p={:?} model={}",
                                    result.answers.len(),
                                    result.approve,
                                    result.probability,
                                    result.model
                                );
                                if let Some(cost) = result.cost_usd {
                                    tracing::info!("extension '{name}' decide cost: ${cost:.6}");
                                }
                                decide_result = Some(result);
                                // Re-run the extension with its answer.
                            }
                            Err(e) => {
                                tracing::warn!("extension '{name}' decide failed: {e}");
                                break;
                            }
                        }
                    }
                    Decision::NestedCall(req) => {
                        // A nested call is the most expensive thing an
                        // extension can trigger. By default the extension must
                        // have asked the decide model and received "yes" in
                        // this request; klava never invents the question.
                        if config.require_decide_for_nested_calls
                            && !decide_result.as_ref().is_some_and(|d| d.approve)
                        {
                            tracing::warn!(
                                "extension '{name}' requested a nested call to {} without an \
                                 approved decide; staying on the cheap model",
                                req.model
                            );
                            break;
                        }
                        if !config.require_decide_for_nested_calls {
                            tracing::warn!(
                                "extension '{name}' nested call to {} runs UNVERIFIED \
                                 (require_decide_for_nested_calls = false)",
                                req.model
                            );
                        }

                        match nested_call(client, config, payload, &req).await? {
                            NestedOutcome::Appended(next) => {
                                payload = next;
                                nested_calls_used += 1;
                                tracing::debug!(
                                    "{name}: {nested_calls_used} nested call(s) used this request"
                                );
                            }
                            NestedOutcome::TakenOver {
                                payload: next,
                                response,
                            } => {
                                return Ok(RequestOutcome {
                                    payload: next,
                                    canned_response: Some(response),
                                });
                            }
                            NestedOutcome::Failed(next) => payload = next,
                        }
                        break;
                    }
                }
            }
        }

        Ok(RequestOutcome {
            payload,
            canned_response: None,
        })
    }
}

/// Spawn one extension, write the request, read the response.
///
/// The child is killed on timeout, so a hung extension cannot stall the proxy.
async fn run_extension(
    ext: &InstalledExtension,
    request: &ExtensionRequest,
) -> std::result::Result<ExtensionResponse, String> {
    use tokio::io::AsyncWriteExt;

    let timeout = ext
        .config
        .get("timeout_secs")
        .and_then(Value::as_u64)
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_TIMEOUT);

    let mut command = tokio::process::Command::new(&ext.path);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    // `ETXTBSY` can appear when a concurrently forked child still holds the
    // freshly written binary open; see `utils::spawn_retrying_busy`.
    let mut attempt = 0;
    let mut child = loop {
        match command.spawn() {
            Ok(child) => break child,
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy && attempt + 1 < 40 => {
                attempt += 1;
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            Err(e) => return Err(format!("cannot start {}: {e}", ext.path.display())),
        }
    };

    let mut stdin = child.stdin.take().ok_or("extension has no stdin")?;
    let mut stdout = child.stdout.take().ok_or("extension has no stdout")?;
    let mut stderr = child.stderr.take().ok_or("extension has no stderr")?;

    let body = serde_json::to_vec(request).map_err(|e| format!("cannot serialize request: {e}"))?;

    let exchange = async {
        stdin
            .write_all(&body)
            .await
            .map_err(|e| format!("cannot write request: {e}"))?;
        stdin
            .write_all(b"\n")
            .await
            .map_err(|e| format!("cannot write request: {e}"))?;
        drop(stdin);

        let mut out = Vec::new();
        let mut err = Vec::new();
        let read_out = read_capped(&mut stdout, &mut out, MAX_STDOUT);
        let read_err = read_capped(&mut stderr, &mut err, MAX_STDERR);
        let (out_result, _) = tokio::join!(read_out, read_err);
        out_result?;

        let status = child
            .wait()
            .await
            .map_err(|e| format!("wait failed: {e}"))?;
        Ok::<_, String>((out, err, status))
    };

    let (out, err, status) = match tokio::time::timeout(timeout, exchange).await {
        Ok(result) => result?,
        Err(_) => {
            let _ = child.kill().await;
            return Err(format!("timed out after {}s", timeout.as_secs()));
        }
    };

    log_stderr(&ext.name, &err);

    if !status.success() {
        return Err(format!("exited with {status}"));
    }

    // Extensions must print exactly one JSON object; trailing newlines are fine.
    let text = String::from_utf8_lossy(&out);
    let text = text.trim();
    if text.is_empty() {
        return Err("produced no output".into());
    }

    serde_json::from_str(text).map_err(|e| format!("invalid response JSON: {e}"))
}

/// Read until EOF or `cap` bytes.
async fn read_capped<R>(
    reader: &mut R,
    out: &mut Vec<u8>,
    cap: usize,
) -> std::result::Result<(), String>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;

    let mut buffer = [0u8; 8192];
    loop {
        let read = reader
            .read(&mut buffer)
            .await
            .map_err(|e| format!("read failed: {e}"))?;
        if read == 0 {
            return Ok(());
        }
        let remaining = cap.saturating_sub(out.len());
        if remaining == 0 {
            return Err(format!("output exceeds {cap} bytes"));
        }
        out.extend_from_slice(&buffer[..read.min(remaining)]);
    }
}

/// Forward an extension's stderr as warnings, one line each.
fn log_stderr(name: &str, stderr: &[u8]) {
    if stderr.is_empty() {
        return;
    }
    let text = String::from_utf8_lossy(stderr);
    for line in text.lines().filter(|l| !l.trim().is_empty()).take(20) {
        tracing::warn!("extension '{name}': {line}");
    }
}

// ---------------------------------------------------------------------------
// Nested model calls
// ---------------------------------------------------------------------------

/// What a nested model call produced.
enum NestedOutcome {
    /// The answer was folded into the conversation; send normally.
    Appended(Value),
    /// The answer *is* the response; the caller renders it.
    TakenOver { payload: Value, response: Value },
    /// Nothing changed (any failure path).
    Failed(Value),
}

/// Perform one nested model call and fold the answer back in.
async fn nested_call(
    client: &reqwest::Client,
    config: &Config,
    payload: Value,
    req: &NestedCallRequest,
) -> Result<NestedOutcome> {
    let Some(entry) = resolve_model(config, req) else {
        tracing::warn!(
            "nested call: model {:?} not found, staying on {}",
            req.model,
            req.model
        );
        return Ok(NestedOutcome::Failed(payload));
    };

    // Reuse the conversation, swap the model, add the reviewer system prompt.
    // Tools are dropped: a reviewer must not act.
    let mut nested = payload.clone();
    nested["model"] = json!(entry.upstream_model());
    nested["stream"] = json!(false);
    nested["tools"] = Value::Null;
    nested["tool_choice"] = Value::Null;
    if let Some(max) = req.max_tokens {
        nested["max_tokens"] = json!(max);
    }
    prepend_system(&mut nested, &req.system);
    append_user(&mut nested, &req.instruction);

    let typed: openai::OpenAIRequest = match serde_json::from_value(nested) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("nested call: could not build request: {e}");
            return Ok(NestedOutcome::Failed(payload));
        }
    };

    // The hook-free sender is the recursion boundary: extensions never see this
    // request, so a nested call cannot recurse.
    let response =
        match crate::proxy::send_openai_request_via(client, config, &typed, Some(&entry)).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("nested call: request failed, continuing: {e}");
                return Ok(NestedOutcome::Failed(payload));
            }
        };

    if !response.status().is_success() {
        tracing::warn!("nested call: returned {}, continuing", response.status());
        return Ok(NestedOutcome::Failed(payload));
    }

    let value: Value = match response.json().await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("nested call: unreadable response: {e}");
            return Ok(NestedOutcome::Failed(payload));
        }
    };
    let Some(text) = extract_text(&value) else {
        tracing::warn!("nested call: response had no content");
        return Ok(NestedOutcome::Failed(payload));
    };

    match req.mode {
        FoldMode::Append => {
            // A user message reads naturally ("here are reviewer remarks") and
            // keeps the assistant's authorship unambiguous.
            let mut out = payload;
            append_user(
                &mut out,
                &format!("A senior reviewer looked at this and noted:\n\n{text}"),
            );
            tracing::info!(
                "nested call: answer from {} folded back ({} chars)",
                req.model,
                text.len()
            );
            Ok(NestedOutcome::Appended(out))
        }
        FoldMode::TakeOver => {
            tracing::info!("nested call: took over with {}", req.model);
            Ok(NestedOutcome::TakenOver {
                payload,
                response: json!({
                    "id": format!("chatcmpl-esc-{}", uuid::Uuid::new_v4().as_simple()),
                    "object": "chat.completion",
                    "created": chrono::Utc::now().timestamp(),
                    "model": req.model,
                    "choices": [{
                        "index": 0,
                        "message": {"role": "assistant", "content": text},
                        "finish_reason": "stop"
                    }],
                    "usage": {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0}
                }),
            })
        }
    }
}

/// Resolve the connection a nested call runs on.
///
/// The nested call names a model (its `model` field); a `[[models]]` entry by
/// that name wins. An unknown or empty name falls back to the configured
/// completion model.
fn resolve_model(config: &Config, req: &NestedCallRequest) -> Option<crate::providers::Model> {
    config
        .resolve_model(&req.model)
        .cloned()
        .or_else(|| config.default_completion().cloned())
}

fn prepend_system(payload: &mut Value, system: &str) {
    let Some(messages) = payload.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    messages.insert(0, json!({"role": "system", "content": system}));
}

fn append_user(payload: &mut Value, text: &str) {
    if let Some(messages) = payload.get_mut("messages").and_then(Value::as_array_mut) {
        messages.push(json!({"role": "user", "content": text}));
    }
}

/// Pull the assistant text out of an OpenAI chat-completions response.
fn extract_text(value: &Value) -> Option<String> {
    let text = value
        .get("choices")?
        .as_array()?
        .first()?
        .get("message")?
        .get("content")?
        .as_str()?
        .to_string();
    (!text.trim().is_empty()).then_some(text)
}

/// Infer why the client is making this request.
///
/// klava sees one HTTP request at a time, so this is a heuristic: a trailing
/// tool message means the loop is continuing after a tool result.
pub fn reason_from_request(req: &openai::OpenAIRequest) -> RequestReason {
    match req.messages.last().map(|m| m.role.as_str()) {
        Some("tool") => RequestReason::Continuation,
        _ => RequestReason::User,
    }
}

/// Run the extension chain over a normalized request.
pub async fn run_request_extensions(
    extensions: &Extensions,
    client: &reqwest::Client,
    config: &Config,
    payload: Value,
    reason: RequestReason,
    source_api: SourceApi,
) -> Result<RequestOutcome> {
    if extensions.is_empty() {
        return Ok(RequestOutcome {
            payload,
            canned_response: None,
        });
    }
    extensions
        .on_request(client, config, payload, reason, source_api)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_overlays_scalars_and_objects() {
        let mut base = json!({"a": 1, "b": {"c": 2, "d": 3}});
        merge(&mut base, &json!({"b": {"c": 9}, "e": 5}));
        assert_eq!(base, json!({"a": 1, "b": {"c": 9, "d": 3}, "e": 5}));
    }

    #[test]
    fn merge_replaces_arrays() {
        let mut base = json!({"list": [1, 2, 3]});
        merge(&mut base, &json!({"list": [9]}));
        assert_eq!(base["list"], json!([9]));
    }

    #[test]
    fn extract_text_reads_the_first_choice() {
        let v = json!({"choices": [{"message": {"content": "hello"}}]});
        assert_eq!(extract_text(&v).as_deref(), Some("hello"));
    }

    #[test]
    fn extract_text_rejects_empty() {
        let v = json!({"choices": [{"message": {"content": "   "}}]});
        assert!(extract_text(&v).is_none());
    }

    #[test]
    fn append_user_preserves_existing_messages() {
        let mut p = json!({"messages": [{"role": "user", "content": "hi"}]});
        append_user(&mut p, "note");
        assert_eq!(p["messages"].as_array().unwrap().len(), 2);
        assert_eq!(p["messages"][1]["content"], "note");
    }

    #[test]
    fn prepend_system_goes_first() {
        let mut p = json!({"messages": [{"role": "user", "content": "hi"}]});
        prepend_system(&mut p, "be terse");
        assert_eq!(p["messages"][0]["role"], "system");
        assert_eq!(p["messages"].as_array().unwrap().len(), 2);
    }

    fn response(json: &str) -> ExtensionResponse {
        serde_json::from_str(json).expect("valid response")
    }

    #[test]
    fn response_continue_is_the_default() {
        assert_eq!(response("{}").into_decision(), Decision::Continue);
    }

    #[test]
    fn response_replace_carries_the_payload() {
        let d = response(r#"{"decision":"replace","payload":{"messages":[]}}"#).into_decision();
        match d {
            Decision::Replace(v) => assert!(v["messages"].is_array()),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn response_append_carries_messages() {
        let d = response(r#"{"decision":"append","messages":[{"role":"user","content":"x"}]}"#)
            .into_decision();
        match d {
            Decision::Append(m) => assert_eq!(m.len(), 1),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn response_deny_carries_a_reason() {
        let d = response(r#"{"decision":"deny","reason":"blocked"}"#).into_decision();
        assert_eq!(
            d,
            Decision::Deny {
                reason: "blocked".into()
            }
        );
    }

    #[test]
    fn response_decide_carries_the_extension_question() {
        let d = response(
            r#"{"decision":"decide","decide":{
                "state":{"error":"disk full"},
                "questions":[{"id":"need_help","type":"noul","instructions":"Needs help?"}],
                "threshold":0.7
            }}"#,
        )
        .into_decision();
        match d {
            Decision::Decide(req) => {
                assert_eq!(req.questions.len(), 1);
                assert_eq!(req.questions[0].id, "need_help");
                assert_eq!(req.threshold, 0.7);
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn response_decide_without_a_request_continues() {
        assert_eq!(
            response(r#"{"decision":"decide"}"#).into_decision(),
            Decision::Continue
        );
    }

    #[test]
    fn decide_result_is_omitted_from_the_first_request() {
        let req = ExtensionRequest {
            protocol: 1,
            config: json!({}),
            payload: json!({}),
            reason: RequestReason::User,
            source_api: SourceApi::OpenAi,
            model: "m".into(),
            nested_calls_used: 0,
            require_decide: true,
            decide: None,
        };
        let v = serde_json::to_value(&req).unwrap();
        assert!(v.get("decide").is_none());
    }

    #[test]
    fn decide_result_is_sent_on_the_second_run() {
        let req = ExtensionRequest {
            protocol: 1,
            config: json!({}),
            payload: json!({}),
            reason: RequestReason::User,
            source_api: SourceApi::OpenAi,
            model: "m".into(),
            nested_calls_used: 0,
            require_decide: true,
            decide: Some(crate::ext_api::DecideResult {
                answers: std::collections::HashMap::new(),
                approve: true,
                probability: Some(0.91),
                cost_usd: Some(0.00002),
                model: "typesafe/jev-1.13".into(),
            }),
        };
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["decide"]["approve"], true);
        assert_eq!(v["decide"]["probability"], 0.91);
    }

    #[test]
    fn response_nested_call_defaults_to_append_mode() {
        let d = response(
            r#"{"decision":"nested_call","nested_call":{"model":"strong","instruction":"look"}}"#,
        )
        .into_decision();
        match d {
            Decision::NestedCall(req) => {
                assert_eq!(req.model, "strong");
                assert_eq!(req.mode, FoldMode::Append);
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn response_nested_call_honours_take_over() {
        let d = response(
            r#"{"decision":"nested_call","nested_call":{"model":"m","mode":"take_over"}}"#,
        )
        .into_decision();
        match d {
            Decision::NestedCall(req) => assert_eq!(req.mode, FoldMode::TakeOver),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn unknown_decision_is_ignored_not_fatal() {
        assert_eq!(
            response(r#"{"decision":"something_new"}"#).into_decision(),
            Decision::Continue
        );
    }

    #[test]
    fn missing_replace_payload_does_not_blank_the_request() {
        assert_eq!(
            response(r#"{"decision":"replace"}"#).into_decision(),
            Decision::Continue
        );
    }

    #[test]
    fn protocol_request_serializes_expected_fields() {
        let req = ExtensionRequest {
            protocol: 1,
            config: json!({"enabled": true}),
            payload: json!({"messages": []}),
            reason: RequestReason::Continuation,
            source_api: SourceApi::Anthropic,
            model: "m".into(),
            nested_calls_used: 0,
            require_decide: true,
            decide: None,
        };
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["protocol"], 1);
        assert_eq!(v["require_decide"], true);
        assert_eq!(v["reason"], "continuation");
        assert_eq!(v["source_api"], "anthropic");
    }

    #[test]
    fn toml_manifest_parses_into_json() {
        let v = toml_to_json("enabled = true\ntimeout_secs = 5\n").unwrap();
        assert_eq!(v["enabled"], true);
        assert_eq!(v["timeout_secs"], 5);
    }

    #[test]
    fn broken_manifest_is_rejected() {
        assert!(toml_to_json("this is not toml ][").is_err());
    }

    /// A malformed decide request is rejected before any model call, the
    /// extension runs exactly once, and the payload is untouched. This is the
    /// deterministic half of the round-trip: validation happens in the host.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_malformed_decide_is_rejected_without_spending() {
        let dir = std::env::temp_dir().join(format!("klava-decide-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("runs.log");
        let path = dir.join("asker");
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\ncat >/dev/null\nprintf 'run\\n' >> '{log}'\n\
                 echo '{{\"decision\":\"decide\",\"decide\":{{\"state\":{{}},\"questions\":[]}}}}'\n",
                log = log.display()
            ),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
        }

        let extensions = Extensions {
            active: vec![InstalledExtension {
                name: "asker".into(),
                path,
                config: json!({}),
                fail_closed: false,
                origin: None,
            }],
        };
        let config = Config::default();
        let payload = json!({"model": "weak", "messages": []});
        let outcome = extensions
            .on_request(
                &reqwest::Client::new(),
                &config,
                payload.clone(),
                RequestReason::User,
                SourceApi::OpenAi,
            )
            .await
            .expect("request runs");
        assert_eq!(outcome.payload, payload);
        assert_eq!(
            std::fs::read_to_string(&log).unwrap().lines().count(),
            1,
            "an invalid decide must not re-run the extension"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An extension that always asks for a nested call must not get one
    /// without an approved decide while the host policy requires it. The
    /// refusal happens before any model is resolved, so this test never
    /// touches the network.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_unapproved_nested_call_is_refused_by_host_policy() {
        let dir = std::env::temp_dir().join(format!("klava-gate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("always-nested");
        std::fs::write(
            &path,
            "#!/bin/sh\ncat >/dev/null\necho '{\"decision\":\"nested_call\",\"nested_call\":{\"model\":\"strong\"}}'\n",
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
        }

        let extensions = Extensions {
            active: vec![InstalledExtension {
                name: "always-nested".into(),
                path,
                config: json!({}),
                fail_closed: false,
                origin: None,
            }],
        };
        let config = Config {
            require_decide_for_nested_calls: true,
            ..Config::default()
        };
        let payload = json!({"model": "weak", "messages": [{"role": "user", "content": "hi"}]});

        let outcome = extensions
            .on_request(
                &reqwest::Client::new(),
                &config,
                payload.clone(),
                RequestReason::User,
                SourceApi::OpenAi,
            )
            .await
            .expect("request runs");

        assert_eq!(outcome.payload, payload, "payload must be unchanged");
        assert!(outcome.canned_response.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    fn write_exe(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, "#!/bin/sh\n").unwrap();
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).unwrap();
    }

    #[test]
    fn discover_ignores_non_executables_and_manifests() {
        let dir = std::env::temp_dir().join(format!("klava-disc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        std::fs::write(dir.join("plain.txt"), "x").unwrap();
        std::fs::write(dir.join("manifest.toml"), "enabled = true\n").unwrap();
        std::fs::write(dir.join(".hidden"), "x").unwrap();
        #[cfg(unix)]
        write_exe(&dir.join("real"));

        let found = discover(&dir);
        let names: Vec<&str> = found.iter().map(|e| e.name.as_str()).collect();
        #[cfg(unix)]
        assert_eq!(names, vec!["real"]);
        #[cfg(not(unix))]
        assert!(names.contains(&"plain.txt"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn discover_reads_a_manifest_beside_the_executable() {
        let dir = std::env::temp_dir().join(format!("klava-man-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        write_exe(&dir.join("demo"));
        std::fs::write(
            dir.join("demo.toml"),
            "enabled = true\nfail_closed = true\nthreshold = 0.5\n",
        )
        .unwrap();

        let found = discover(&dir);
        assert_eq!(found.len(), 1);
        assert!(found[0].fail_closed);
        assert_eq!(found[0].config["threshold"], 0.5);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn discover_skips_a_broken_manifest_instead_of_crashing() {
        let dir = std::env::temp_dir().join(format!("klava-badman-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        #[cfg(unix)]
        write_exe(&dir.join("demo"));
        std::fs::write(dir.join("demo.toml"), "not valid toml ][").unwrap();

        assert!(discover(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn discover_of_a_missing_directory_is_empty_not_an_error() {
        let missing = std::env::temp_dir().join("klava-definitely-missing-dir-xyz");
        let _ = std::fs::remove_dir_all(&missing);
        assert!(discover(&missing).is_empty());
    }

    #[test]
    fn default_search_path_starts_with_the_data_dir_then_system_dirs() {
        let paths = default_search_path();
        assert_eq!(paths[0].dir, install_dir());
        assert_eq!(paths[0].origin, "data");
        assert!(
            paths
                .iter()
                .any(|p| p.dir == *"/usr/share/klava/extensions"),
            "distribution path must be searched"
        );
        // No directory may appear twice.
        let mut seen = std::collections::BTreeSet::new();
        for entry in &paths {
            assert!(
                seen.insert(entry.dir.clone()),
                "duplicate {}",
                entry.dir.display()
            );
        }
    }

    /// The only test that touches `KLAVA_EXTENSIONS_DIR`. Kept single so tests
    /// that read it cannot race with each other; `default_search_path` covers
    /// the rest without any environment mutation.
    #[test]
    fn search_path_honours_the_override_exclusively() {
        // SAFETY: no other test in this crate reads or writes this variable
        // concurrently, and it is restored before returning.
        let previous = std::env::var("KLAVA_EXTENSIONS_DIR").ok();
        unsafe { std::env::set_var("KLAVA_EXTENSIONS_DIR", "/tmp/klava-override") };

        let paths = search_path();
        assert_eq!(paths.len(), 1, "override must replace the whole path");
        assert_eq!(paths[0].dir, PathBuf::from("/tmp/klava-override"));
        assert_eq!(paths[0].origin, "KLAVA_EXTENSIONS_DIR");

        match previous {
            Some(value) => unsafe { std::env::set_var("KLAVA_EXTENSIONS_DIR", value) },
            None => unsafe { std::env::remove_var("KLAVA_EXTENSIONS_DIR") },
        }
    }

    #[test]
    fn nothing_is_enabled_without_explicit_config() {
        let ext = InstalledExtension {
            name: "x".into(),
            path: PathBuf::from("/bin/true"),
            config: json!({}),
            fail_closed: false,
            origin: None,
        };
        let config = Config {
            port: 48017,
            ..Config::default()
        };
        assert!(!ext.is_enabled(&config), "nothing is enabled implicitly");
    }

    #[test]
    fn manifest_can_enable_and_user_can_override() {
        let ext = InstalledExtension {
            name: "x".into(),
            path: PathBuf::from("/bin/true"),
            config: json!({"enabled": true, "a": 1}),
            fail_closed: false,
            origin: None,
        };
        let mut config = Config {
            port: 48017,
            ..Config::default()
        };
        assert!(ext.is_enabled(&config), "manifest enables it");

        config
            .extensions
            .settings
            .insert("x".into(), json!({"enabled": false}));
        assert!(!ext.is_enabled(&config), "user override wins");
    }

    #[test]
    fn effective_config_merges_user_over_manifest() {
        let ext = InstalledExtension {
            name: "x".into(),
            path: PathBuf::from("/bin/true"),
            config: json!({"a": 1, "b": 2}),
            fail_closed: false,
            origin: None,
        };
        let mut config = Config {
            port: 48017,
            ..Config::default()
        };
        config
            .extensions
            .settings
            .insert("x".into(), json!({"b": 9, "c": 3}));
        let eff = ext.effective_config(&config);
        assert_eq!(eff["a"], 1);
        assert_eq!(eff["b"], 9);
        assert_eq!(eff["c"], 3);
    }

    #[test]
    fn host_only_manifest_keys_never_reach_the_extension() {
        let ext = InstalledExtension {
            name: "x".into(),
            path: PathBuf::from("/bin/true"),
            config: json!({
                "enabled": true,
                "fail_closed": true,
                "notice": "read me",
                "setup": {"args": ["--fetch"]},
                "settings": {"threshold": 0.7},
                "threshold": 0.5,
            }),
            fail_closed: true,
            origin: None,
        };
        let config = Config {
            port: 48017,
            ..Config::default()
        };
        let eff = ext.effective_config(&config);

        assert_eq!(eff["threshold"], 0.5, "a real setting survives");
        for key in ["settings", "notice", "setup", "fail_closed"] {
            assert!(
                eff.get(key).is_none(),
                "host-only key '{key}' must not be sent: {eff}"
            );
        }
    }

    #[test]
    fn declared_settings_are_listed_with_their_defaults() {
        let ext = InstalledExtension {
            name: "x".into(),
            path: PathBuf::from("/bin/true"),
            config: json!({"settings": {"threshold": 0.7, "roles": ["tool"]}}),
            fail_closed: false,
            origin: None,
        };
        let declared = ext.declared_settings();
        assert_eq!(declared.len(), 2);
        let threshold = declared.iter().find(|s| s.key == "threshold").unwrap();
        assert_eq!(threshold.default, json!(0.7));
        let roles = declared.iter().find(|s| s.key == "roles").unwrap();
        assert_eq!(roles.default, json!(["tool"]));
    }

    #[test]
    fn an_extension_that_declares_nothing_has_no_settings_list() {
        let ext = InstalledExtension {
            name: "x".into(),
            path: PathBuf::from("/bin/true"),
            config: json!({"enabled": true}),
            fail_closed: false,
            origin: None,
        };
        assert!(ext.declared_settings().is_empty());
    }

    #[test]
    fn a_malformed_settings_table_is_ignored_rather_than_fatal() {
        let ext = InstalledExtension {
            name: "x".into(),
            path: PathBuf::from("/bin/true"),
            config: json!({"settings": "not a table"}),
            fail_closed: false,
            origin: None,
        };
        assert!(ext.declared_settings().is_empty());
        assert!(
            ext.effective_config(&Config::default())
                .get("settings")
                .is_none()
        );
    }

    #[test]
    fn a_notice_is_trimmed_or_absent() {
        let with = InstalledExtension {
            name: "x".into(),
            path: PathBuf::from("/bin/true"),
            config: json!({"notice": "\n  costs money  \n"}),
            fail_closed: false,
            origin: None,
        };
        assert_eq!(with.notice(), Some("costs money"));

        for blank in [json!({"notice": "   "}), json!({"notice": 5}), json!({})] {
            let ext = InstalledExtension {
                name: "x".into(),
                path: PathBuf::from("/bin/true"),
                config: blank,
                fail_closed: false,
                origin: None,
            };
            assert!(ext.notice().is_none());
        }
    }
}
