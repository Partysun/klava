//! `kompress` extension for klava.
//!
//! Extractive context compression: large tool results are mostly boilerplate, so
//! a small token-classification model scores every token and this program drops
//! the low-value ones.
//!
//! This is a **standalone executable**, not a library. klava launches it as a
//! child process and speaks JSON over stdio, so installing it needs no rebuild of
//! klava and no matching compiler. See `EXTENSIONS.md` in the klava repository
//! for the protocol.
//!
//! Everything fails **open**: a missing model, an unreadable tokenizer, an
//! inference error, structured content, or a rewrite that would lose meaning all
//! leave the request untouched. Compression is an optimization, never a gate.
//!
//! # Cost, and why there is a planner
//!
//! Inference memory and time are **quadratic** in the token count (attention),
//! and the fixed model load dominates small inputs:
//!
//! | tokens | peak RSS | wall |
//! |--------|----------|------|
//! | 2,000  | 1.2 GB   | 2.6 s |
//! | 4,000  | 2.9 GB   | 6.5 s |
//! | 8,000  | 8.6 GB   | 20 s |
//! | 11,000 | 15.7 GB  | 34 s |
//! | 14,000 | 26.7 GB  | 54 s |
//!
//! So "compress the big tool results" and "do not exhaust the machine" are the
//! same requirement, and a fixed `max_tokens` cannot express it: the right
//! ceiling depends on the machine, on what klava will wait for, and on how many
//! messages the request carries. [`Compressor::plan`] spends a per-request budget
//! of megabytes and seconds, cheapest message first, and never starts an
//! inference it has not paid for. Every check that can reject a message without
//! the graph runs before the graph is mapped.
//!
//! [model]: https://huggingface.co/chopratejas/kompress-v2-base

use ort::session::Session;
use ort::value::Tensor;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;
use tokenizers::Tokenizer;

/// The protocol version klava sends. We answer whatever we understand.
const PROTOCOL: u32 = 1;

/// Everything `--fetch-model` needs, as klava records it in the manifest.
///
/// This is what `--print-manifest` emits, so the host can learn how to provision
/// the model without knowing anything about this extension.
const MANIFEST: &str = r#"# Written by klava from `klava-compress --print-manifest`.
#
# The model is a ~573 MB download, so it is not fetched at install time on
# klava's behalf: klava runs this hook after the extension is enabled.

[setup]
args = ["--fetch-model"]
summary = "Download the kompress model (~573 MB) into the klava cache"
network = true
timeout_secs = 3600

# What this extension accepts. klava prints this table in
# `klava extensions show klava-compress` and never sends it to the extension.
[settings]
enabled = false
model_dir = ""
threshold = 0.7
min_tokens = 0
max_tokens = 0
min_savings = 0.1
budget_mb = 0
budget_ms = 0
roles = ["tool"]
skip_structured = true
"#;

/// Where the model files come from.
const MODEL_BASE_URL: &str = "https://huggingface.co/chopratejas/kompress-v2-base/resolve/main";

/// Model file names inside `model_dir`.
const TOKENIZER_FILE: &str = "tokenizer.json";
const MODEL_FILE: &str = "kompress-fp32.onnx";

/// One request from klava.
#[derive(Debug, Deserialize)]
struct HostRequest {
    #[serde(default)]
    config: Value,
    #[serde(default)]
    payload: Value,
}

/// Our answer: `replace` with the rewritten payload, or `continue`.
#[derive(Debug, Serialize)]
struct HostResponse {
    decision: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    payload: Option<Value>,
}

/// Settings, from the manifest merged with the user's table.
///
/// Owned by this crate: klava stores the table as opaque JSON and never names a
/// field, so adding or renaming one never touches the host.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CompressConfig {
    /// Whether the extension is active. Checked by the host before `build`.
    ///
    /// Off by default: the model is a ~580 MB download, so enabling it is an
    /// explicit choice.
    #[serde(default)]
    pub enabled: bool,
    /// Directory holding `kompress-fp32.onnx` and `tokenizer.json`.
    ///
    /// Empty = `$KLAVA_COMPRESS_MODEL_DIR`, else `$XDG_CACHE_HOME/klava/kompress`
    /// (`~/.cache/klava/kompress` on Linux).
    #[serde(default)]
    pub model_dir: String,
    /// Keep a token when its score is at least this.
    ///
    /// Lower keeps more (and saves less). Measured savings at 0.5 / 0.7 on
    /// 2k-token samples: dense logs 16% / 53%, prose 8% / 22%, source 0% / 17%,
    /// repeated JSON 0% / 4%.
    #[serde(default = "default_threshold")]
    pub threshold: f32,
    /// Hard floor on tokens, or `0` to derive one from [`MIN_SAVED_BYTES`].
    ///
    /// A message smaller than the floor is not worth a forward pass. Deriving it
    /// is better than guessing: at the observed ~50% savings rate, the smallest
    /// message that still removes a useful amount of text is about
    /// `2 * MIN_SAVED_BYTES` bytes, which lands far below the old fixed 200 and
    /// is what makes ordinary tool output eligible.
    #[serde(default = "default_min_tokens")]
    pub min_tokens: usize,
    /// Hard ceiling on tokens for one message, or `0` to let the budget decide.
    ///
    /// Only lower this. The effective ceiling is always the smaller of this and
    /// what [`Self::budget_mb`] allows, because memory is quadratic in the token
    /// count and no configuration may override that.
    #[serde(default = "default_max_tokens")]
    pub max_tokens: usize,
    /// Discard a result unless it removes at least this fraction of tokens.
    ///
    /// Prevents paying inference cost to shave off a token or two.
    #[serde(default = "default_min_savings")]
    pub min_savings: f32,
    /// Megabytes this process may spend on inference, or `0` to detect a budget.
    ///
    /// Detection reads the cgroup v2 limit, then `MemAvailable`, and leaves head-
    /// room so a large message cannot take the desktop down with it. This is the
    /// one number that actually bounds memory; `max_tokens` is derived from it.
    #[serde(default = "default_budget_mb")]
    pub budget_mb: usize,
    /// Milliseconds a whole request may spend on inference, or `0` to derive one.
    ///
    /// The host kills the process at `timeout_secs` (20 by default), so spending
    /// the whole budget risks returning nothing at all. The derived value leaves
    /// room for the model load, the analysis and a reply.
    #[serde(default = "default_budget_ms")]
    pub budget_ms: usize,
    /// Message roles whose content may be rewritten.
    #[serde(default = "default_roles")]
    pub roles: Vec<String>,
    /// Skip content that looks like code, SQL, JSON, or patch data.
    ///
    /// Word removal is safe for prose but not for syntax: it turned
    /// `SELECT * FROM users` into `WHERE ... AND = true` and stripped the body
    /// from a Rust function. Structural content is left untouched.
    #[serde(default = "default_skip_structured")]
    pub skip_structured: bool,
}

fn default_threshold() -> f32 {
    0.7
}
fn default_min_tokens() -> usize {
    0
}
fn default_max_tokens() -> usize {
    0
}
fn default_min_savings() -> f32 {
    0.10
}
fn default_budget_mb() -> usize {
    0
}
fn default_budget_ms() -> usize {
    0
}
fn default_roles() -> Vec<String> {
    vec!["tool".to_string()]
}
fn default_skip_structured() -> bool {
    true
}

impl Default for CompressConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model_dir: String::new(),
            threshold: default_threshold(),
            min_tokens: default_min_tokens(),
            max_tokens: default_max_tokens(),
            min_savings: default_min_savings(),
            budget_mb: default_budget_mb(),
            budget_ms: default_budget_ms(),
            roles: default_roles(),
            skip_structured: default_skip_structured(),
        }
    }
}

impl CompressConfig {
    /// Directory the model is expected in.
    fn resolved_model_dir(&self) -> PathBuf {
        if !self.model_dir.trim().is_empty() {
            return PathBuf::from(shellexpand(&self.model_dir));
        }
        if let Ok(dir) = std::env::var("KLAVA_COMPRESS_MODEL_DIR")
            && !dir.trim().is_empty()
        {
            // Same expansion as `model_dir`: `~/x` must mean the same thing
            // whichever way it was set.
            return PathBuf::from(shellexpand(&dir));
        }
        cache_root().join("kompress")
    }
}

/// `$XDG_CACHE_HOME/klava`, else `$HOME/.cache/klava`.
fn cache_root() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_CACHE_HOME")
        && !dir.trim().is_empty()
    {
        return PathBuf::from(dir).join("klava");
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".cache").join("klava")
}

/// Expand a leading `~`, and only a leading `~`.
fn shellexpand(path: &str) -> String {
    if path == "~" {
        return std::env::var("HOME").unwrap_or_else(|_| path.to_string());
    }
    if let Some(rest) = path.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        return format!("{home}/{rest}");
    }
    path.to_string()
}

// ---------------------------------------------------------------------------
// What inference costs
// ---------------------------------------------------------------------------

/// Resident megabytes of the ONNX graph itself, measured: the same whether the
/// message is 0 or 400 tokens.
const MODEL_RESIDENT_MB: f64 = 1100.0;

/// Extra peak megabytes per token **squared**.
///
/// Attention makes this quadratic. Fitted to the measured peaks (fp32 model,
/// default ONNX Runtime pool): 8k → 8.6 GB, 11k → 15.7 GB, 14k → 26.7 GB. The
/// coefficient is deliberately the largest of those three fits, so the estimate
/// errs high: underestimating is what OOM-kills the desktop.
const PEAK_MB_PER_TOKEN2: f64 = 1.35e-4;

/// Seconds per token squared, fitted the same way: 8k → 20 s, 11k → 34 s,
/// 14k → 54 s.
const SECS_PER_TOKEN2: f64 = 3.0e-7;

/// Fixed cost of mapping the graph and warming it up.
const MODEL_LOAD_SECS: f64 = 1.1;

/// The largest message this extension will *ever* run inference on.
///
/// This is a backstop, not a setting. [`MAX_BUDGET_MB`] already limits the
/// derived ceiling to ~3,300 tokens, so this only binds if the cost model or the
/// budget detection regresses. It exists so that no configuration, detection bug
/// or unusual cgroup layout can reproduce the incident this rewrite is for: a
/// 14k-token message asked ONNX Runtime for 26.7 GB, and ~41k asked for a single
/// 80 GB buffer.
const HARD_MAX_TOKENS: usize = 4000;

/// Ceiling on a detected budget.
///
/// 2560 MB puts the token ceiling near 3,300, which clears the 90th percentile of
/// real tool output (2,716 tokens) instead of landing just under it. Beyond that
/// the message is genuinely too large to run without containment: the observed
/// maximum, ~5,090 tokens, predicts 4.6 GB for a single message.
const MAX_BUDGET_MB: usize = 2560;

/// Floor on a detected budget, used only when detection itself fails.
///
/// Below the graph's own ~1.1 GB resident cost nothing fits at all, so this is
/// that plus room for a small message. A cgroup that really is tighter than this
/// is *not* overridden: detection returns what is actually available, and the
/// planner then declines to run anything, which is the correct fail-open answer.
const MIN_BUDGET_MB: usize = 1_300;

/// Memory held back from the detected budget no matter how little is free.
///
/// Whatever the extension predicts, the machine still has to run a proxy, an
/// editor and a desktop. Without a floor, a momentarily busy machine would detect
/// a tiny budget and the extension would go silent for the rest of the request.
const MIN_RESERVE_MB: usize = 1_024;

/// Milliseconds one request may spend on inference when not configured.
///
/// klava kills an extension at `timeout_secs` (20 by default). Spending all of
/// it means returning nothing at all, which is worse than a partial rewrite, so
/// a third of it is reserved for the model load, the analysis and the reply.
const DEFAULT_BUDGET_MS: usize = 8_000;

/// How much text a rewrite must remove before it is worth having done.
///
/// A percentage floor alone is a trap: 50% of a 40-token message is 20 tokens of
/// noise, while the same inference could have saved 900 tokens somewhere else.
/// Bytes are the unit the user actually pays for in context.
const MIN_SAVED_BYTES: usize = 200;

/// Assumed bytes per token when turning [`MIN_SAVED_BYTES`] into a token floor.
const BYTES_PER_TOKEN: usize = 4;

/// Peak megabytes for one inference of `tokens` tokens.
///
/// Saturates rather than overflowing: `usize::MAX²` as `f64` is merely huge, and
/// the result is only ever compared against a budget.
fn predicted_peak_mb(tokens: usize) -> f64 {
    MODEL_RESIDENT_MB + PEAK_MB_PER_TOKEN2 * (tokens as f64) * (tokens as f64)
}

/// Wall-clock seconds for one inference of `tokens` tokens.
fn predicted_secs(tokens: usize) -> f64 {
    MODEL_LOAD_SECS + SECS_PER_TOKEN2 * (tokens as f64) * (tokens as f64)
}

/// The largest message that fits `budget_mb` on its own.
///
/// The inverse of [`predicted_peak_mb`]. A budget below the graph's resident cost
/// yields `0`, which the planner reads as "nothing fits" — the honest answer,
/// and never an overflow.
fn tokens_that_fit(budget_mb: usize) -> usize {
    let varying = budget_mb as f64 - MODEL_RESIDENT_MB;
    if varying <= 0.0 {
        return 0;
    }
    let tokens = (varying / PEAK_MB_PER_TOKEN2).sqrt();
    if !tokens.is_finite() || tokens < 1.0 {
        return 0;
    }
    (tokens as usize).clamp(1, HARD_MAX_TOKENS)
}

/// How many megabytes of inference this process may spend.
///
/// One budget covers a whole request: the planner charges every message it runs
/// against it. The number comes from what is actually available here, so the
/// extension shrinks inside a tight cgroup instead of ignoring it.
///
/// A quarter of the free memory is held back for everything else on the machine
/// (the proxy, the editor, the desktop), with a floor so a machine that is merely
/// busy still gets a sane reserve. Note that the reserve is subtracted from
/// *available*, not further divided: the graph's own resident cost is part of the
/// budget, so dividing twice would reject a machine that could comfortably run a
/// small message.
///
/// The budget is only as good as [`available_mb`]: on a target where memory
/// cannot be measured, this falls back to [`MIN_BUDGET_MB`], a modest fixed
/// budget. That is the safe direction — an unknown machine gets a conservative
/// ceiling instead of an unbounded one — and `budget_mb` is there to raise it
/// deliberately.
fn detect_budget_mb() -> usize {
    budget_from_available_mb(available_mb())
}

/// Turn a measurement into a budget.
///
/// Split out so the reserve and the two clamps can be exercised without a
/// machine in a particular state: `None` (unmeasurable target or failed syscall)
/// must still produce a usable budget rather than zero.
fn budget_from_available_mb(available: Option<usize>) -> usize {
    match available {
        Some(available) => {
            let reserve = (available / 4).max(MIN_RESERVE_MB);
            available.saturating_sub(reserve).min(MAX_BUDGET_MB)
        }
        None => MIN_BUDGET_MB,
    }
}

/// Megabytes this process could plausibly take on *this* machine.
///
/// Each platform measures a different thing, but all three answer the same
/// question: how much memory could be handed to a new process without swapping?
/// The answer has to be real, not a constant — a 16 GB Mac handed the fallback
/// budget would cap out near 1,200 tokens and silently stop compressing the
/// 90th percentile of tool output (2,716 tokens), which is exactly the traffic
/// this extension exists for.
#[cfg(target_os = "linux")]
fn available_mb() -> Option<usize> {
    // Inside a container or a systemd unit the cgroup limit is what actually
    // binds, not the host's free memory.
    if let Some(mb) = cgroup_available_mb() {
        return Some(mb);
    }
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    meminfo_available_mb(&meminfo)
}

/// Free + inactive + speculative Mach pages, in megabytes.
///
/// macOS has no `/proc`: it exposes VM statistics through Mach. Those three page
/// classes are the ones the kernel may reclaim for a new process without paging
/// out anything live, which is the closest analogue to Linux's `MemAvailable`.
#[cfg(target_os = "macos")]
fn available_mb() -> Option<usize> {
    // `mach_host_self` is deprecated in `libc` in favour of the `mach2` crate,
    // but this is the single Mach call we need and pulling in `mach2` for it
    // would be a second dependency for one page count. The function itself is
    // not going anywhere: it is the documented way to get the host port.
    #[allow(deprecated)]
    // SAFETY: `vm_statistics64` is a plain C struct that is all zeros being a
    // valid value for, `host_statistics64` writes exactly `HOST_VM_INFO64_COUNT`
    // integers into the buffer we hand it, and `count` is a local whose address
    // we pass. Both calls are thread-safe and leak nothing.
    unsafe {
        let mut stats: libc::vm_statistics64 = std::mem::zeroed();
        let mut count = libc::HOST_VM_INFO64_COUNT;
        let result = libc::host_statistics64(
            libc::mach_host_self(),
            libc::HOST_VM_INFO64,
            std::ptr::from_mut(&mut stats).cast::<libc::integer_t>(),
            &mut count,
        );
        if result != libc::KERN_SUCCESS {
            return None;
        }
        let page_size = libc::sysconf(libc::_SC_PAGESIZE);
        if page_size <= 0 {
            return None;
        }
        // The struct is `repr(packed)`, so the fields are copied out rather than
        // borrowed: taking a reference into a packed struct is undefined.
        pages_available_mb(
            stats.free_count as usize,
            stats.inactive_count as usize,
            stats.speculative_count as usize,
            page_size as usize,
        )
    }
}

/// `ullAvailPhys` from `GlobalMemoryStatusEx`, in megabytes.
///
/// Windows reports physical memory that is immediately available, including the
/// standby list, which is the same notion as the other two platforms.
#[cfg(windows)]
fn available_mb() -> Option<usize> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

    // SAFETY: `MEMORYSTATUSEX` is a plain C struct with `dwLength` set to its own
    // size as the API requires, and the buffer outlives the call. The API writes
    // nothing else and returns zero on failure.
    unsafe {
        let mut status = MEMORYSTATUSEX {
            dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
            ..Default::default()
        };
        if GlobalMemoryStatusEx(&mut status) == 0 {
            return None;
        }
        Some(bytes_available_mb(status.ullAvailPhys))
    }
}

/// Targets we cannot measure get no answer, and the caller falls back to
/// [`MIN_BUDGET_MB`] rather than guessing.
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn available_mb() -> Option<usize> {
    None
}

/// Reclaimable pages times the page size, in megabytes.
///
/// Split out from [`available_mb`] so the arithmetic is testable on every target:
/// an overflow or a unit slip here would set the whole budget.
#[cfg(any(target_os = "macos", test))]
fn pages_available_mb(
    free: usize,
    inactive: usize,
    speculative: usize,
    page_size: usize,
) -> Option<usize> {
    let pages = free.checked_add(inactive)?.checked_add(speculative)?;
    Some(pages.checked_mul(page_size)? / (1024 * 1024))
}

/// Bytes to megabytes, rounded down and saturating.
///
/// Saturating keeps a 32-bit build from wrapping a large machine's free memory
/// into a small budget.
#[cfg(any(windows, test))]
fn bytes_available_mb(bytes: u64) -> usize {
    usize::try_from(bytes / (1024 * 1024)).unwrap_or(usize::MAX)
}

/// `memory.max - memory.current` for the cgroup this process lives in.
///
/// Walks up from `/proc/self/cgroup` because the effective limit is the tightest
/// one on the path, and a unit's own directory does not carry a limit when the
/// parent slice is the one that has it.
#[cfg(target_os = "linux")]
fn cgroup_available_mb() -> Option<usize> {
    let cgroup = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let path = cgroup
        .lines()
        .find_map(|line| line.strip_prefix("0::"))?
        .trim()
        .to_string();
    cgroup_headroom_mb(Path::new("/sys/fs/cgroup"), &path)
}

/// The tightest headroom between `path` and `root`, in megabytes.
///
/// Split out from [`cgroup_available_mb`] so the walk can be tested against a
/// synthetic cgroup tree: getting this wrong under-detects a limit, which is the
/// one failure mode that would let the incident repeat.
#[cfg(any(target_os = "linux", test))]
fn cgroup_headroom_mb(root: &Path, path: &str) -> Option<usize> {
    // A limit is only meaningful together with the usage it applies to, and a
    // cgroup with `max` is unlimited, so the tightest *headroom* wins rather
    // than the tightest limit.
    let mut tightest: Option<usize> = None;
    let mut dir = root.to_path_buf();
    // `/proc/self/cgroup` records an *absolute* path inside the hierarchy. Its
    // leading `/` must not be pushed onto the root, or the walk resets to the
    // filesystem root and every read misses.
    for component in Path::new(path).components().filter_map(|c| match c {
        std::path::Component::Normal(name) => Some(name),
        _ => None,
    }) {
        let name = component.to_string_lossy();
        if !name.is_empty() {
            dir.push(name.as_ref());
        }
        if let Ok(text) = std::fs::read_to_string(dir.join("memory.max"))
            && let Some(limit) = parse_cgroup_bytes(&text)
        {
            let used = std::fs::read_to_string(dir.join("memory.current"))
                .ok()
                .and_then(|text| parse_cgroup_bytes(&text))
                .unwrap_or(0);
            let headroom = limit.saturating_sub(used);
            tightest = Some(tightest.map_or(headroom, |best: usize| best.min(headroom)));
        }
    }

    // Bytes in the v2 files, megabytes in the return value: everything above
    // this point budgets in MB, and a silent 1024× here is a silent 1024×
    // overshoot of the cgroup limit.
    tightest.map(|bytes| bytes / (1024 * 1024))
}

/// Parse a cgroup v2 byte counter, or `None` for the literal `max`.
#[cfg(any(target_os = "linux", test))]
fn parse_cgroup_bytes(text: &str) -> Option<usize> {
    let trimmed = text.trim();
    if trimmed == "max" {
        return None;
    }
    trimmed.parse().ok()
}

/// `MemAvailable` from `/proc/meminfo`, in megabytes.
#[cfg(any(target_os = "linux", test))]
fn meminfo_available_mb(meminfo: &str) -> Option<usize> {
    for line in meminfo.lines() {
        let Some(rest) = line.strip_prefix("MemAvailable:") else {
            continue;
        };
        let mut fields = rest.split_whitespace();
        let value: usize = fields.next()?.parse().ok()?;
        let unit = fields.next().unwrap_or("kB");
        return Some(match unit {
            "kB" => value / 1024,
            "MB" => value,
            _ => value / 1024,
        });
    }
    None
}

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

/// The cheap half: the tokenizer alone.
///
/// Kept separate from [`Scorer`] because the two have wildly different costs —
/// tens of milliseconds versus a 1.1 GB graph. A message outside the window is
/// discarded either way, so it has to be rejected here, before the graph is
/// mapped. Loading the model only to decide "this is too long for the model" was
/// the single largest source of wasted work: measured, 93% of requests were
/// skipped *after* paying for a full forward pass.
struct Loaded {
    tokenizer: Tokenizer,
    /// Mapped on first real use, so a request that is entirely rejected never
    /// touches the graph.
    scorer: OnceLock<Option<Scorer>>,
    dir: PathBuf,
}

/// The expensive half: the ONNX graph.
struct Scorer {
    session: Mutex<Session>,
}

impl std::fmt::Debug for Loaded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Loaded").finish_non_exhaustive()
    }
}

impl Loaded {
    fn new(dir: PathBuf) -> Result<Self, String> {
        let tokenizer_path = dir.join(TOKENIZER_FILE);
        let tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| format!("cannot load {}: {e}", tokenizer_path.display()))?;
        Ok(Self {
            tokenizer,
            scorer: OnceLock::new(),
            dir,
        })
    }

    fn encode(&self, text: &str) -> Result<tokenizers::Encoding, String> {
        self.tokenizer
            .encode(text, false)
            .map_err(|e| format!("tokenizer failed: {e}"))
    }

    /// Load once on first use. A failure is cached too, so a broken install logs
    /// once instead of on every message.
    fn scorer(&self) -> Option<&Scorer> {
        self.scorer
            .get_or_init(|| {
                let model_path = self.dir.join(MODEL_FILE);
                match Session::builder()
                    .and_then(|builder| builder.commit_from_file(&model_path))
                {
                    Ok(session) => Some(Scorer {
                        session: Mutex::new(session),
                    }),
                    Err(e) => {
                        eprintln!(
                            "kompress: model unavailable, requests pass through: cannot load {}: {e}",
                            model_path.display()
                        );
                        None
                    }
                }
            })
            .as_ref()
    }
}

impl Scorer {
    /// Per-token keep probability for an already-tokenized message.
    ///
    /// Memory here is quadratic in the token count (see [`predicted_peak_mb`]).
    /// Callers must have checked the size guard first; this function is the one
    /// that can exhaust the machine.
    fn score(&self, encoding: &tokenizers::Encoding) -> Result<Vec<f32>, String> {
        let ids: Vec<i64> = encoding.get_ids().iter().map(|&id| id as i64).collect();
        let n = ids.len();
        if n == 0 {
            return Ok(Vec::new());
        }

        let inputs = ort::inputs![
            "input_ids" => Tensor::from_array(([1usize, n], ids)).map_err(|e| e.to_string())?,
            "attention_mask" => Tensor::from_array(([1usize, n], vec![1i64; n]))
                .map_err(|e| e.to_string())?,
        ];

        let mut session = self
            .session
            .lock()
            .map_err(|_| "model session poisoned".to_string())?;
        let outputs = session
            .run(inputs)
            .map_err(|e| format!("inference failed: {e}"))?;
        let (_, scores) = outputs["final_scores"]
            .try_extract_tensor::<f32>()
            .map_err(|e| format!("unexpected model output: {e}"))?;

        Ok(scores.to_vec())
    }
}

// ---------------------------------------------------------------------------
// Planning: what to run, in what order, within budget
// ---------------------------------------------------------------------------

/// One rewrite opportunity: a string message, or one text part of a multimodal
/// message.
///
/// Collected and measured *before* anything is inferred, because the decision to
/// run inference has to be made on the token count, not on what came back from
/// running it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Candidate {
    /// Index into `payload["messages"]`.
    message: usize,
    /// Index into the message's content array, when the content is a part list.
    part: Option<usize>,
    tokens: usize,
}

/// What the planner decided to run, and what it expects it to cost.
#[derive(Debug, Default, PartialEq)]
struct Selection {
    chosen: Vec<Candidate>,
    /// Predicted *marginal* megabytes on top of the resident graph.
    mb: f64,
    /// Predicted *marginal* seconds, excluding the one-time model load.
    secs: f64,
}

/// Sort key: smallest first.
///
/// Cost per saved token falls as the message shrinks — `n` removed tokens cost
/// `n²` — so under a budget the small messages are always the better buy. It
/// also means a request that runs out of time still returned its best work
/// instead of dying mid-way through the largest item.
fn by_cost(a: &Candidate, b: &Candidate) -> std::cmp::Ordering {
    a.tokens.cmp(&b.tokens).then(a.message.cmp(&b.message))
}

// ---------------------------------------------------------------------------
// Extension
// ---------------------------------------------------------------------------

pub struct Compressor {
    config: CompressConfig,
    /// Ceiling and floor, resolved once: they depend on this machine, not on the
    /// request.
    max_tokens: usize,
    min_tokens: usize,
    budget_mb: usize,
    budget_ms: usize,
    model: OnceLock<Option<Loaded>>,
}

impl std::fmt::Debug for Compressor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Compressor")
            .field("config", &self.config)
            .field("min_tokens", &self.min_tokens)
            .field("max_tokens", &self.max_tokens)
            .field("budget_mb", &self.budget_mb)
            .field("budget_ms", &self.budget_ms)
            .finish_non_exhaustive()
    }
}

impl Compressor {
    pub fn new(config: CompressConfig) -> Self {
        let budget_mb = if config.budget_mb > 0 {
            config.budget_mb.min(MAX_BUDGET_MB)
        } else {
            detect_budget_mb()
        };

        // The ceiling is whatever the budget allows, optionally lowered by the
        // user, and never above the backstop. A too-large `max_tokens` is
        // therefore not a preference to honour: it is a way to exhaust the
        // machine, so it is clamped rather than rejected, keeping the extension
        // fail-open. A budget too small to hold the graph yields a zero ceiling,
        // and the resulting empty window means "run nothing".
        let derived_max = tokens_that_fit(budget_mb);
        let max_tokens = if config.max_tokens == 0 {
            derived_max
        } else {
            config.max_tokens.min(derived_max)
        };

        let min_tokens = if config.min_tokens > 0 {
            config.min_tokens
        } else {
            // At the observed ~50% savings rate, a message has to be this long
            // before it removes MIN_SAVED_BYTES. Deriving it beats the old fixed
            // 200, which rejected the median tool message outright.
            2 * MIN_SAVED_BYTES / BYTES_PER_TOKEN
        };

        let budget_ms = if config.budget_ms > 0 {
            config.budget_ms
        } else {
            DEFAULT_BUDGET_MS
        };

        Self {
            config,
            max_tokens,
            min_tokens,
            budget_mb,
            budget_ms,
            model: OnceLock::new(),
        }
    }

    /// Load once on first use. A failure is cached too, so a broken install logs
    /// once instead of on every request.
    fn model(&self) -> Option<&Loaded> {
        self.model
            .get_or_init(|| match Loaded::new(self.config.resolved_model_dir()) {
                Ok(model) => Some(model),
                Err(e) => {
                    eprintln!("kompress: model unavailable, requests pass through: {e}");
                    None
                }
            })
            .as_ref()
    }

    /// Rewrite `text`, or return `None` when it is not worth it.
    ///
    /// Returns the compressed text and the token counts (before, after).
    fn compress_text(
        &self,
        model: &Loaded,
        encoding: &tokenizers::Encoding,
        text: &str,
    ) -> Option<(String, usize, usize)> {
        let tokens = encoding.get_ids().len();

        let scorer = model.scorer()?;
        let scores = match scorer.score(encoding) {
            Ok(scores) => scores,
            Err(e) => {
                eprintln!("kompress: {e}");
                return None;
            }
        };

        // Byte offsets let us rebuild the text without guessing at whitespace.
        let offsets: Vec<usize> = encoding
            .get_offsets()
            .iter()
            .flat_map(|&(start, end)| [start, end])
            .collect();

        if offsets.len() != scores.len() * 2 || scores.len() != tokens {
            eprintln!(
                "kompress: {} score(s) but {} offset pair(s); skipping",
                scores.len(),
                offsets.len() / 2
            );
            return None;
        }

        let keep: Vec<bool> = scores.iter().map(|s| *s >= self.config.threshold).collect();
        let kept = keep.iter().filter(|k| **k).count();
        let saved = (tokens - kept) as f32 / tokens as f32;
        if saved < self.config.min_savings {
            eprintln!(
                "kompress: only {:.0}% saved ({} of {} tokens), below {:.0}% floor",
                saved * 100.0,
                tokens - kept,
                tokens,
                self.config.min_savings * 100.0
            );
            return None;
        }

        let rebuilt = rebuild(text, &offsets, &scores, self.config.threshold);

        // Removing words must not make the text longer; if it somehow did, the
        // honest answer is "no change".
        if rebuilt.len() >= text.len() {
            return None;
        }

        // The score is a suggestion, not a guarantee. Refuse any rewrite that
        // loses something load-bearing.
        if !meaning_preserved(text, &rebuilt) {
            return None;
        }

        // Cheap rewrites are not worth having made. A percentage floor alone lets
        // a 40-token message "save 50%" and contribute nothing, while the same
        // inference could have removed far more elsewhere.
        if text.len().saturating_sub(rebuilt.len()) < MIN_SAVED_BYTES {
            eprintln!(
                "kompress: only {} bytes saved, below the {} byte floor",
                text.len() - rebuilt.len(),
                MIN_SAVED_BYTES
            );
            return None;
        }

        Some((rebuilt, tokens, kept))
    }

    /// Every text this request is allowed to rewrite, measured but not run.
    ///
    /// Everything that can reject a message cheaply happens here: role, content
    /// shape, structured content, and the token window. The graph is not mapped
    /// until [`Self::plan`] has picked what to run.
    fn candidates(&self, payload: &Value, model: &Loaded) -> Vec<Candidate> {
        let mut out = Vec::new();
        let Some(messages) = payload.get("messages").and_then(Value::as_array) else {
            return out;
        };

        for (message, entry) in messages.iter().enumerate() {
            let role = entry.get("role").and_then(Value::as_str).unwrap_or("");
            if !self.config.roles.iter().any(|r| r == role) {
                continue;
            }
            match entry.get("content") {
                Some(Value::String(text)) => {
                    if let Some(candidate) = self.measure(message, None, text, model) {
                        out.push(candidate);
                    }
                }
                Some(Value::Array(parts)) => {
                    for (index, part) in parts.iter().enumerate() {
                        // Multimodal parts (images, files) carry no `text`.
                        let Some(Value::String(text)) = part.get("text") else {
                            continue;
                        };
                        if let Some(candidate) = self.measure(message, Some(index), text, model) {
                            out.push(candidate);
                        }
                    }
                }
                _ => {}
            }
        }

        out
    }

    /// Whether one text is worth an inference, and what it would cost.
    fn measure(
        &self,
        message: usize,
        part: Option<usize>,
        text: &str,
        model: &Loaded,
    ) -> Option<Candidate> {
        if self.config.skip_structured && looks_structured(text) {
            eprintln!("kompress: skipping structured content");
            return None;
        }

        // A message that cannot possibly clear the byte floor is not worth a
        // forward pass, and this is cheaper than tokenizing it: it is rejected on
        // raw length. `MIN_SAVED_BYTES` is a lower bound on what a rewrite must
        // remove, so anything shorter than it can never qualify.
        if text.len() < MIN_SAVED_BYTES {
            eprintln!(
                "kompress: skipping {} byte message, below the {} byte floor",
                text.len(),
                MIN_SAVED_BYTES
            );
            return None;
        }

        let encoding = match model.encode(text) {
            Ok(encoding) => encoding,
            Err(e) => {
                eprintln!("kompress: {e}");
                return None;
            }
        };
        let tokens = encoding.get_ids().len();

        // The window is checked on the tokenizer's output, before the graph is
        // mapped. Checking it after inference was the original bug: a message
        // destined to be discarded still loaded 1.1 GB and ran a full forward
        // pass, and 93% of messages were discarded.
        if tokens < self.min_tokens || tokens > self.max_tokens {
            eprintln!(
                "kompress: skipping {} token message (window {}..={})",
                tokens, self.min_tokens, self.max_tokens
            );
            return None;
        }

        Some(Candidate {
            message,
            part,
            tokens,
        })
    }

    /// Choose which candidates to run under the memory and time budgets.
    ///
    /// The accounting is deliberately pessimistic about memory. ONNX Runtime's
    /// arena never returns what it took, and it fragments when input shapes
    /// differ: five *identically* sized 2.7k-token messages reused one arena
    /// (2.1 GB total), while a real request with ~20 different sizes reached
    /// 26 GB. Summing the per-message attention cost reproduces the worst case
    /// instead of the best one.
    fn plan(&self, mut candidates: Vec<Candidate>) -> Selection {
        candidates.sort_by(by_cost);

        let mut selection = Selection::default();
        let time_budget = self.budget_ms as f64 / 1000.0;
        // The graph is mapped once and stays; only the attention buffers are
        // marginal. Everything below is charged against those.
        let mut spent_mb = 0.0;
        let mut spent_secs = MODEL_LOAD_SECS;

        for candidate in candidates {
            // Candidates are sorted smallest first and both costs grow with
            // `n`, so once one does not fit, nothing later can either.
            let marginal_mb = predicted_peak_mb(candidate.tokens) - MODEL_RESIDENT_MB;
            let next_mb = MODEL_RESIDENT_MB + spent_mb + marginal_mb;
            if next_mb > self.budget_mb as f64 {
                eprintln!(
                    "kompress: stopping at {} tokens: {:.0} MB predicted, {} MB budget",
                    candidate.tokens, next_mb, self.budget_mb
                );
                break;
            }

            let marginal_secs = predicted_secs(candidate.tokens) - MODEL_LOAD_SECS;
            let next_secs = spent_secs + marginal_secs;
            if next_secs > time_budget {
                eprintln!(
                    "kompress: stopping at {} tokens: {:.1}s predicted, {:.1}s budget",
                    candidate.tokens, next_secs, time_budget
                );
                break;
            }

            spent_mb += marginal_mb;
            spent_secs = next_secs;
            selection.mb += marginal_mb;
            selection.secs += marginal_secs;
            selection.chosen.push(candidate);
        }

        selection
    }
}

// ---------------------------------------------------------------------------
// Provisioning: the model is a large download, so it is fetched on demand.
// ---------------------------------------------------------------------------

/// Files the model consists of, as (remote path, local file name).
const MODEL_FILES: [(&str, &str); 2] = [
    ("onnx/kompress-fp32.onnx", MODEL_FILE),
    ("tokenizer.json", TOKENIZER_FILE),
];

/// What the process was asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Mode {
    /// Read one request on stdin and answer it (the extension protocol).
    Serve,
    /// Download the model into `dir` (or the default cache when `None`).
    FetchModel { dir: Option<PathBuf> },
    /// Print the manifest klava should store beside this binary.
    PrintManifest,
    /// Print usage.
    Help,
}

fn parse_args(args: &[String]) -> Result<Mode, String> {
    let mut mode = Mode::Serve;
    let mut dir: Option<PathBuf> = None;
    let mut iter = args.iter();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--fetch-model" => mode = Mode::FetchModel { dir: None },
            "--print-manifest" => mode = Mode::PrintManifest,
            "-h" | "--help" => mode = Mode::Help,
            "--dir" => {
                let value = iter.next().ok_or("--dir needs a value")?;
                dir = Some(PathBuf::from(shellexpand(value)));
            }
            other => return Err(format!("unknown argument '{other}'")),
        }
    }

    // `--dir` only means something together with `--fetch-model`.
    if let Some(dir) = dir {
        match mode {
            Mode::FetchModel { .. } => mode = Mode::FetchModel { dir: Some(dir) },
            _ => return Err("--dir is only valid with --fetch-model".into()),
        }
    }

    Ok(mode)
}

/// Where klava hands the hook its effective config (manifest merged with the
/// user's settings). Must match `ext_setup_store::CONFIG_ENV` in klava.
const CONFIG_ENV: &str = "KLAVA_EXTENSION_CONFIG";

/// Resolve the download directory the same way the runtime resolves `model_dir`.
///
/// An explicit `--dir` wins; otherwise the config klava passed in
/// `$KLAVA_EXTENSION_CONFIG`; otherwise `KLAVA_COMPRESS_MODEL_DIR`, else the
/// cache directory. All of it goes through [`CompressConfig::resolved_model_dir`]
/// so a fetch and a later run cannot disagree about where the model lives:
/// downloading 573 MB into a directory the runtime never reads is worse than not
/// downloading at all.
///
/// `config_json` is a parameter rather than an environment read so this stays
/// testable without mutating process-wide state.
fn provision_dir(explicit: Option<&Path>, config_json: Option<&str>) -> PathBuf {
    if let Some(dir) = explicit {
        return dir.to_path_buf();
    }
    // A malformed config must not stop provisioning: the default config resolves
    // to the cache, which is what the runtime would use for the same input.
    let config = config_json
        .and_then(|text| serde_json::from_str::<CompressConfig>(text).ok())
        .unwrap_or_default();
    config.resolved_model_dir()
}

/// How to open a `.part` file for the body we are about to receive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PartMode {
    /// No usable prefix: start the file over.
    Truncate,
    /// The server answered the range request: continue after the prefix.
    Append,
}

/// Truncate when starting over, append when resuming.
///
/// Never both: appending after the server ignored our `Range` glues a fresh body
/// onto a stale prefix, which yields a file of the right length and the wrong
/// bytes.
fn part_mode(offset: u64) -> PartMode {
    if offset == 0 {
        PartMode::Truncate
    } else {
        PartMode::Append
    }
}

/// Download `remote` to `dest`, resuming a partial `.part` file.
fn fetch_one(remote: &str, dest: &Path) -> Result<(), String> {
    let url = format!("{MODEL_BASE_URL}/{remote}");
    let part = dest.with_extension("part");

    let mut offset = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);

    // A finished file is never downloaded again: the model is ~573 MB.
    if let Ok(meta) = std::fs::metadata(dest)
        && meta.len() > 0
    {
        println!("have  {} ({} bytes)", dest.display(), meta.len());
        return Ok(());
    }

    let agent = ureq::Agent::config_builder()
        // A big file on a slow link: no deadline at all. `timeout_recv_body` is a
        // budget for the whole body, not an idle timeout, so any fixed value
        // fails on a link slower than size/budget.
        .timeout_global(None)
        .timeout_connect(Some(std::time::Duration::from_secs(30)))
        .timeout_recv_response(Some(std::time::Duration::from_secs(60)))
        .timeout_recv_body(None)
        .max_redirects(10)
        .build();
    let agent = ureq::Agent::new_with_config(agent);

    let mut request = agent.get(&url);
    if offset > 0 {
        request = request.header("Range", format!("bytes={offset}-"));
        println!("resume {} at {offset} bytes", dest.display());
    } else {
        println!("fetch {}", dest.display());
    }

    let response = match request.call() {
        Ok(response) => response,
        // 416 also means the `.part` is *larger* than the remote file, which
        // would be a corrupt file promoted into place. Drop it and let the next
        // run start clean instead of guessing.
        Err(ureq::Error::StatusCode(416)) if offset > 0 => {
            let _ = std::fs::remove_file(&part);
            return Err(format!(
                "{url} rejected the resume at {offset} bytes; removed {} — run again to \
                 download it from scratch",
                part.display()
            ));
        }
        Err(ureq::Error::StatusCode(code)) => {
            return Err(format!("{url} returned HTTP {code}"));
        }
        Err(e) => return Err(format!("cannot fetch {url}: {e}")),
    };

    // The server may ignore `Range`; then the whole body arrives and any `.part`
    // prefix is stale.
    if offset > 0 && response.status() != 206 {
        offset = 0;
        println!("server ignored the range; starting over");
    }

    // `content-length` is the length of *this* body: the whole file on a 200,
    // the remainder on a 206. Counting from the post-reset offset gives the
    // total the finished file must have.
    let expected_total = response
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .map(|len| offset + len);

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(part_mode(offset) == PartMode::Truncate)
        .append(part_mode(offset) == PartMode::Append)
        .open(&part)
        .map_err(|e| format!("cannot open {}: {e}", part.display()))?;

    let mut reader = response.into_body().into_reader();
    let mut buffer = vec![0u8; 1024 * 1024];
    let mut written = offset;
    let mut last_report = written;

    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|e| format!("download of {} failed: {e}", dest.display()))?;
        if read == 0 {
            break;
        }
        file.write_all(&buffer[..read])
            .map_err(|e| format!("cannot write {}: {e}", part.display()))?;
        written += read as u64;

        // Report every 32 MiB so a long download does not look hung.
        if written - last_report >= 32 * 1024 * 1024 {
            last_report = written;
            println!("  {} MiB", written / (1024 * 1024));
        }
    }
    file.flush()
        .map_err(|e| format!("cannot flush {}: {e}", part.display()))?;
    drop(file);

    // A short body becomes a silently broken ONNX file: keep the `.part` so the
    // next run resumes instead of starting the 573 MB over.
    if let Some(expected) = expected_total
        && written != expected
    {
        return Err(format!(
            "{} is incomplete: {written} of {expected} bytes; kept {} to resume",
            dest.display(),
            part.display()
        ));
    }

    std::fs::rename(&part, dest).map_err(|e| {
        format!(
            "cannot move {} into place at {}: {e}",
            part.display(),
            dest.display()
        )
    })?;
    println!("done  {} ({written} bytes)", dest.display());
    Ok(())
}

/// `--fetch-model`: download every model file into the resolved directory.
fn fetch_model(explicit: Option<&Path>, config_json: Option<&str>) -> Result<(), String> {
    let dir = provision_dir(explicit, config_json);
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;

    println!("model dir: {}", dir.display());
    for (remote, name) in MODEL_FILES {
        fetch_one(remote, &dir.join(name))?;
    }

    println!();
    println!("Model ready in: {}", dir.display());
    Ok(())
}

/// Usage, matching what `--help` prints.
fn usage() -> String {
    format!(
        "klava-compress {version}

Context-compression extension for klava.

Usage:
  klava-compress                  # serve one JSON request on stdin (klava does this)
  klava-compress --fetch-model    # download the model (~573 MB)
  klava-compress --print-manifest # print the manifest klava stores beside this binary

Options:
  --fetch-model        Download {} and {} into the model directory.
  --dir <DIR>          With --fetch-model: download into DIR instead of the cache.
  -h, --help           Show this help.

The model directory is, in order: --dir, model_dir from
$KLAVA_EXTENSION_CONFIG (set by klava when it runs the setup hook, with the
manifest merged with your settings), $KLAVA_COMPRESS_MODEL_DIR,
$XDG_CACHE_HOME/klava/kompress, ~/.cache/klava/kompress.
",
        TOKENIZER_FILE,
        MODEL_FILE,
        version = env!("CARGO_PKG_VERSION"),
    )
}

// ---------------------------------------------------------------------------
// Entry point: speak the protocol once per process.
// ---------------------------------------------------------------------------

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = match parse_args(&args) {
        Ok(mode) => mode,
        Err(e) => {
            eprintln!("klava-compress: {e}");
            eprintln!();
            eprint!("{}", usage());
            std::process::exit(2);
        }
    };

    match mode {
        Mode::Serve => serve(),
        Mode::PrintManifest => print!("{MANIFEST}"),
        Mode::Help => print!("{}", usage()),
        Mode::FetchModel { dir } => {
            if let Err(e) = fetch_model(dir.as_deref(), std::env::var(CONFIG_ENV).ok().as_deref()) {
                eprintln!("klava-compress: {e}");
                std::process::exit(1);
            }
        }
    }
}

/// The extension protocol: one request in, one response out.
fn serve() {
    // Diagnostics go to stderr, which klava forwards to its log.
    let mut raw = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut raw) {
        eprintln!("kompress: cannot read request: {e}");
        emit_continue();
        return;
    }

    let request: HostRequest = match serde_json::from_str(&raw) {
        Ok(request) => request,
        Err(e) => {
            eprintln!("kompress: invalid request JSON: {e}");
            emit_continue();
            return;
        }
    };

    let config: CompressConfig = match serde_json::from_value(request.config.clone()) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("kompress: invalid config: {e}");
            emit_continue();
            return;
        }
    };

    // `enabled` is the host's business, but honour it if it reaches us.
    if !config.enabled {
        emit_continue();
        return;
    }

    // The host may pass a global PII switch; it has nothing to do with us.
    let _ = PROTOCOL;

    let compressor = Compressor::new(config);
    match compressor.on_request(request.payload) {
        Some(payload) => emit(HostResponse {
            decision: "replace",
            payload: Some(payload),
        }),
        None => emit_continue(),
    }
}

/// Rewrite the payload in place if compression helps, else return `None`.
impl Compressor {
    fn on_request(&self, payload: Value) -> Option<Value> {
        let model = self.model()?;
        let started = Instant::now();

        // Measure everything first: an inference is only started once the
        // planner has decided it fits the budget. Collecting candidates also
        // applies every cheap rejection (role, shape, structured content, token
        // window) before the graph is ever mapped.
        let candidates = self.candidates(&payload, model);
        if candidates.is_empty() {
            return None;
        }
        let selection = self.plan(candidates);
        if selection.chosen.is_empty() {
            return None;
        }

        let mut payload = payload;
        let mut before_total = 0usize;
        let mut after_total = 0usize;
        let mut touched = 0usize;

        for candidate in &selection.chosen {
            // The planner predicts from measured costs, but the wall clock is
            // the thing klava actually kills us on. A loaded machine can run
            // slower than the fit, so check reality before spending more.
            if started.elapsed().as_millis() as usize >= self.budget_ms {
                eprintln!(
                    "kompress: out of time after {} of {} planned message(s)",
                    touched,
                    selection.chosen.len()
                );
                break;
            }

            // Indices came from this same payload, so a miss means the shape
            // changed under us. Skip it rather than abandoning rewrites already
            // applied — an `?` here would throw away completed work.
            let Some(list) = payload.get_mut("messages").and_then(Value::as_array_mut) else {
                continue;
            };
            let Some(entry) = list.get_mut(candidate.message) else {
                continue;
            };
            let Some(content) = entry.get_mut("content") else {
                continue;
            };
            let text = match candidate.part {
                None => match content {
                    Value::String(text) => text,
                    _ => continue,
                },
                Some(index) => match content.get_mut(index).and_then(|part| part.get_mut("text")) {
                    Some(Value::String(text)) => text,
                    _ => continue,
                },
            };

            // Re-encode rather than keeping every candidate's tokens alive: the
            // text can be large and the bundle is only held for one request.
            let encoding = match model.encode(text) {
                Ok(encoding) => encoding,
                Err(e) => {
                    eprintln!("kompress: {e}");
                    continue;
                }
            };

            let Some((rewritten, before, after)) = self.compress_text(model, &encoding, text)
            else {
                continue;
            };
            *text = rewritten;
            before_total += before;
            after_total += after;
            touched += 1;
        }

        if touched == 0 || after_total >= before_total {
            return None;
        }

        eprintln!(
            "kompress: {touched} message(s), {before_total} -> {after_total} tokens ({:.0}% saved; {:.0} MB and {:.1}s predicted for {} candidate(s))",
            100.0 * (1.0 - after_total as f32 / before_total as f32),
            MODEL_RESIDENT_MB + selection.mb,
            selection.secs,
            selection.chosen.len()
        );
        Some(payload)
    }
}

fn emit(response: HostResponse) {
    match serde_json::to_string(&response) {
        Ok(text) => {
            let mut out = std::io::stdout();
            let _ = out.write_all(text.as_bytes());
            let _ = out.write_all(b"\n");
            let _ = out.flush();
        }
        Err(e) => eprintln!("kompress: cannot serialize response: {e}"),
    }
}

/// The safe answer: leave the request exactly as it was.
fn emit_continue() {
    emit(HostResponse {
        decision: "continue",
        payload: None,
    })
}

/// Whether `text` is structural content that word removal would corrupt.
///
/// This is a conservative *line-share* heuristic, not a parser. Kompress is a
/// prose/log cleaner; code, SQL, JSON and diffs are handed to the model to read
/// literally, and losing one token there changes meaning. The check runs before
/// inference, so skipping also saves the most CPU.
///
/// A message is structural when at least [`STRUCTURED_LINE_SHARE`] of its
/// non-blank lines look syntactic. A single code block inside a long log does
/// not disqualify the whole message.
fn looks_structured(text: &str) -> bool {
    let mut lines = 0usize;
    let mut structured = 0usize;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        lines += 1;
        if is_structural_line(line) {
            structured += 1;
        }
    }
    lines > 0 && structured as f32 / lines as f32 >= STRUCTURED_LINE_SHARE
}

/// Fraction of lines that must look syntactic before a message is skipped.
const STRUCTURED_LINE_SHARE: f32 = 0.30;

/// One line of code, SQL, JSON, diff, or a shell invocation.
fn is_structural_line(line: &str) -> bool {
    // Diff and patch markers.
    if line.starts_with("+++") || line.starts_with("---") || line.starts_with("@@") {
        return true;
    }
    // JSON / JS object lines.
    if line.starts_with('{')
        || line.starts_with('[')
        || line.starts_with("}]")
        || line.starts_with("}, ")
        || line.ends_with('{')
        || line.ends_with("})")
        || line.ends_with(");")
    {
        return true;
    }
    // Bracket-dense lines are syntax even without a recognizable keyword.
    let brackets = line
        .chars()
        .filter(|c| matches!(c, '{' | '}' | '(' | ')' | '[' | ']' | ';'))
        .count();
    if line.len() >= 20 && brackets * 10 >= line.len() {
        return true;
    }
    // SQL and language keywords at the start of a line.
    const KEYWORDS: &[&str] = &[
        "select", "insert", "update", "delete", "from", "where", "join", "group by", "order by",
        "create", "alter", "drop", "fn ", "pub fn", "impl ", "struct ", "enum ", "let ", "const ",
        "use ", "mod ", "def ", "class ", "import ", "package ", "func ", "var ", "return ",
        "if (", "for (", "while (", "#!/", "$ ", ">>> ",
    ];
    let lower = line.to_ascii_lowercase();
    KEYWORDS.iter().any(|k| lower.starts_with(k))
}

/// Words whose loss changes or inverts meaning.
///
/// The model scores these like any other word, so at any useful threshold it
/// drops some of them: measured, `"Configuration is missing the required field
/// api_url, so startup aborts."` became `"Configuration api_url,"`, and
/// `"must never run twice"` became `"never"`. That is not a tuning problem —
/// extractive removal is simply the wrong operation for instructions.
///
/// So instead of trusting the score, every rewrite is checked against the
/// original before it is used. If a rewrite drops anything from this list, or
/// any number, identifier or ALL-CAPS marker, the whole rewrite is discarded and
/// the original text is sent. Compression is skipped, not fudged.
const PROTECTED_WORDS: &[&str] = &[
    // Negation and modality.
    "not",
    "no",
    "never",
    "none",
    "nothing",
    "nobody",
    "neither",
    "nor",
    "without",
    "except",
    "unless",
    "only",
    "must",
    "mustn't",
    "should",
    "shouldn't",
    "cannot",
    "can't",
    "don't",
    "doesn't",
    "didn't",
    "isn't",
    "aren't",
    "won't",
    "wouldn't",
    "couldn't",
    "may",
    "might", // Quantities that bound meaning.
    "all",
    "any",
    "each",
    "every",
    "some",
    "most",
    "always",
    "first",
    "last",
    // Operational severity.
    "warning",
    "warn",
    "error",
    "err",
    "fatal",
    "urgent",
    "critical",
    "important",
    "caution",
    "danger",
    "dangerous",
    "unsafe",
    "deprecated",
    "required",
    "require",
    "requires",
    "missing",
    "invalid",
    "failed",
    "failure",
    "abort",
    "aborts",
    "denied",
    "refused",
    "forbidden",
    "prohibited",
    "mandatory",
];

/// Whether `compressed` still carries everything `original` says that matters.
///
/// Deliberately conservative: a false negative costs a compression opportunity,
/// a false positive corrupts a request.
fn meaning_preserved(original: &str, compressed: &str) -> bool {
    let kept: Vec<&str> = compressed.split_whitespace().collect();
    let kept_lower: Vec<String> = kept.iter().map(|w| w.to_ascii_lowercase()).collect();

    // Split on whitespace only: identifiers and codes rarely contain spaces, and
    // over-splitting would make the checks weaker, not stronger.
    for word in original.split_whitespace() {
        let trimmed = word.trim_matches(|c: char| !c.is_alphanumeric() && c != '_' && c != '-');
        if trimmed.is_empty() {
            continue;
        }
        let lower = trimmed.to_ascii_lowercase();

        // 1. Protected vocabulary.
        if PROTECTED_WORDS.contains(&lower.as_str())
            && !kept_lower.iter().any(|w| w.contains(&lower))
        {
            eprintln!("kompress: refusing rewrite, dropped protected word '{trimmed}'");
            return false;
        }

        // 2. Numbers, error codes, ports, versions, dates.
        if trimmed.chars().any(|c| c.is_ascii_digit()) && !compressed.contains(trimmed) {
            eprintln!("kompress: refusing rewrite, dropped number '{trimmed}'");
            return false;
        }

        // 3. Identifiers: snake_case, kebab-case, paths, dotted names.
        let looks_like_identifier = trimmed.len() > 3
            && (trimmed.contains('_')
                || trimmed.contains('-')
                || trimmed.contains('/')
                || trimmed.contains("::")
                || trimmed.contains('.'));
        if looks_like_identifier && !compressed.contains(trimmed) {
            eprintln!("kompress: refusing rewrite, dropped identifier '{trimmed}'");
            return false;
        }

        // 4. ALL-CAPS markers (URGENT, WARNING, FATAL, TODO).
        let all_caps = trimmed.len() >= 3
            && trimmed.chars().filter(|c| c.is_alphabetic()).count() >= 3
            && trimmed
                .chars()
                .filter(|c| c.is_alphabetic())
                .all(|c| c.is_ascii_uppercase());
        if all_caps && !compressed.contains(trimmed) {
            eprintln!("kompress: refusing rewrite, dropped marker '{trimmed}'");
            return false;
        }
    }

    true
}

/// Rebuild `text` at **word** granularity, dropping whole words the model scored
/// below the threshold.
///
/// Dropping individual tokens is not safe: byte-level BPE splits identifiers and
/// joiners across tokens, so token-level removal turns `EIO-7734` into `E-7734`
/// and merges `information routine` into `informationroutine`. Words are the
/// smallest unit a reader would accept losing.
///
/// Rules that keep this conservative:
///
/// * A word is kept when **any** token overlapping it scores at or above the
///   threshold. Identifiers, error codes and numbers usually score high in one
///   token, so they survive even if their neighbours do not.
/// * A word no token overlaps is kept (we cannot judge it).
/// * When a word goes, so does the space beside it, so two survivors never touch.
///   A newline is never removed, so line structure survives.
///
/// `offsets` is the flat `[start, end, start, end, ...]` byte range list the
/// tokenizer reports, one pair per token.
fn rebuild(text: &str, offsets: &[usize], scores: &[f32], threshold: f32) -> String {
    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut keep = vec![true; len];

    let mut index = 0usize;
    while index < len {
        if bytes[index].is_ascii_whitespace() {
            index += 1;
            continue;
        }

        // A word runs until whitespace. Non-ASCII is never whitespace here,
        // which matches how the model treats prose.
        let start = index;
        while index < len && !bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        let end = index;

        let mut overlapped = false;
        let mut keep_word = false;
        for (&[token_start, token_end], &score) in offsets.as_chunks::<2>().0.iter().zip(scores) {
            // A token belongs to this word if its span intersects it. Special
            // tokens have zero-width spans and therefore never match.
            if token_end <= start || token_start >= end || token_start == token_end {
                continue;
            }
            overlapped = true;
            if score >= threshold {
                keep_word = true;
                break;
            }
        }

        if !overlapped || keep_word {
            continue;
        }

        keep[start..end].fill(false);

        // Remove the space run before the word, unless a newline separates them
        // or the word starts its line. Otherwise remove the run after it.
        let mut before = start;
        while before > 0 && matches!(bytes[before - 1], b' ' | b'\t') {
            before -= 1;
        }
        let newline_before = before > 0 && bytes[before - 1] == b'\n';
        if before < start && !newline_before {
            keep[before..start].fill(false);
        } else {
            let mut after = end;
            while after < len && matches!(bytes[after], b' ' | b'\t') {
                after += 1;
            }
            keep[end..after].fill(false);
        }
    }

    let mut out = String::with_capacity(text.len());
    for (byte_index, ch) in text.char_indices() {
        let ch_end = byte_index + ch.len_utf8();
        // Keep the character if any of its bytes survived, so a split multi-byte
        // character is never truncated into invalid UTF-8.
        if keep[byte_index..ch_end].iter().any(|&k| k) {
            out.push(ch);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn offsets(pairs: &[(usize, usize)]) -> Vec<usize> {
        pairs.iter().flat_map(|&(a, b)| [a, b]).collect()
    }

    /// Build flat offsets and scores for whole-word spans of `words`.
    ///
    /// Each word is one token, which is exactly the case that used to be safe
    /// and is still the easiest to reason about.
    fn word_spans(words: &[&str], keep: &[bool]) -> (String, Vec<usize>, Vec<f32>) {
        let mut text = String::new();
        let mut offsets = Vec::new();
        let mut scores = Vec::new();
        for (i, word) in words.iter().enumerate() {
            if i > 0 {
                text.push(' ');
            }
            let start = text.len();
            text.push_str(word);
            offsets.extend([start, text.len()]);
            scores.push(if keep[i] { 0.9 } else { 0.1 });
        }
        (text, offsets, scores)
    }

    #[test]
    fn drops_low_scoring_words_and_keeps_the_rest() {
        let (text, offsets, scores) =
            word_spans(&["hello", "noise", "world"], &[true, false, true]);
        assert_eq!(rebuild(&text, &offsets, &scores, 0.7), "hello world");
    }

    #[test]
    fn keeps_everything_when_all_words_score_high() {
        let (text, offsets, scores) = word_spans(&["a", "b"], &[true, true]);
        assert_eq!(rebuild(&text, &offsets, &scores, 0.7), "a b");
    }

    #[test]
    fn removes_everything_when_no_word_scores_high() {
        let (text, offsets, scores) = word_spans(&["a", "b"], &[false, false]);
        assert_eq!(rebuild(&text, &offsets, &scores, 0.7), "");
    }

    #[test]
    fn a_word_survives_if_any_of_its_tokens_scores_high() {
        // Regression: byte-level BPE splits `EIO-7734` across tokens. Dropping
        // the low-scoring fragments used to produce `E-7734`.
        let text = "EIO-7734";
        let spans = offsets(&[(0, 2), (2, 3), (3, 8)]);
        let scores = vec![0.1, 0.1, 0.95];
        assert_eq!(rebuild(text, &spans, &scores, 0.7), "EIO-7734");
    }

    #[test]
    fn removing_a_word_never_concatenates_neighbours() {
        // Regression: token-level removal turned `information routine about`
        // into `informationabout`.
        let (text, spans, scores) =
            word_spans(&["information", "routine", "about"], &[true, false, true]);
        assert_eq!(rebuild(&text, &spans, &scores, 0.7), "information about");
    }

    #[test]
    fn removing_the_last_word_leaves_no_trailing_space() {
        let (text, spans, scores) = word_spans(&["keep", "drop"], &[true, false]);
        assert_eq!(rebuild(&text, &spans, &scores, 0.7), "keep");
    }

    #[test]
    fn removing_the_first_word_leaves_no_leading_space() {
        let (text, spans, scores) = word_spans(&["drop", "keep"], &[false, true]);
        assert_eq!(rebuild(&text, &spans, &scores, 0.7), "keep");
    }

    #[test]
    fn newlines_are_preserved_and_never_join_lines() {
        let text = "keep drop\nkeep drop";
        // tokens: keep(0..4) drop(5..9) keep(10..14) drop(15..19)
        let spans = offsets(&[(0, 4), (5, 9), (10, 14), (15, 19)]);
        let scores = vec![0.9, 0.1, 0.9, 0.1];
        assert_eq!(rebuild(text, &spans, &scores, 0.7), "keep\nkeep");
    }

    #[test]
    fn indentation_is_preserved() {
        // The leading space run belongs to a kept word and must survive.
        let text = "    indented drop";
        let spans = offsets(&[(0, 12), (13, 17)]);
        let scores = vec![0.9, 0.1];
        assert_eq!(rebuild(text, &spans, &scores, 0.7), "    indented");
    }

    #[test]
    fn rebuild_never_splits_a_multibyte_character() {
        let text = "héllo wörld";
        let spans = offsets(&[(0, 6), (7, 13)]);
        let scores = vec![0.9, 0.9];
        let out = rebuild(text, &spans, &scores, 0.7);
        assert_eq!(out, "héllo wörld");
        assert!(out.is_char_boundary(out.len()));
    }

    #[test]
    fn rebuild_survives_out_of_range_offsets() {
        let text = "ok";
        let spans = offsets(&[(0, 99)]);
        assert_eq!(rebuild(text, &spans, &[0.9], 0.7), "ok");
    }

    #[test]
    fn rebuild_ignores_zero_width_spans() {
        // Special tokens report a zero-width span at the same position; they
        // must not be treated as overlapping the following word.
        let text = "abc";
        let spans = offsets(&[(0, 0), (0, 3)]);
        assert_eq!(rebuild(text, &spans, &[0.1, 0.9], 0.7), "abc");
    }

    #[test]
    fn words_with_no_token_are_kept() {
        // Defensive: if the tokenizer skipped a word, we cannot judge it, so it
        // survives even though its neighbour was dropped.
        let text = "alpha beta";
        let spans = offsets(&[(0, 5)]);
        let scores = vec![0.1];
        assert_eq!(rebuild(text, &spans, &scores, 0.7), "beta");
    }

    #[test]
    fn sql_is_treated_as_structured() {
        assert!(looks_structured(
            "SELECT * FROM users WHERE id = 42 AND active = true;"
        ));
    }

    #[test]
    fn rust_code_is_treated_as_structured() {
        let code =
            "fn parse(input: &str) -> Result<Vec<Token>, ParseError> {\n    let x = todo!();\n}\n";
        assert!(looks_structured(code));
    }

    #[test]
    fn json_lines_are_treated_as_structured() {
        assert!(looks_structured(
            "{\"id\":1,\"name\":\"a\"}\n{\"id\":2,\"name\":\"b\"}"
        ));
    }

    #[test]
    fn diffs_are_treated_as_structured() {
        assert!(looks_structured(
            "--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n+new"
        ));
    }

    #[test]
    fn ordinary_logs_are_not_structured() {
        let logs = (0..20)
            .map(|i| {
                format!(
                    "2026-10-18T12:00:{:02}Z INFO req={} status=200 ms={}",
                    i % 60,
                    i,
                    20 + i
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!looks_structured(&logs));
    }

    #[test]
    fn prose_is_not_structured() {
        assert!(!looks_structured(
            "The quarterly report shows growth. We examined the data carefully."
        ));
    }

    #[test]
    fn a_single_code_block_does_not_disqualify_a_log() {
        // 9 log lines + 1 code line: below the 30% structural share.
        let mut text = (0..9)
            .map(|i| format!("2026-10-18T12:00:{:02}Z INFO req={} ok", i, i))
            .collect::<Vec<_>>()
            .join("\n");
        text.push_str("\nfn main() {}");
        assert!(!looks_structured(&text));
    }

    #[test]
    fn guard_rejects_dropping_a_negation() {
        // Observed corruption: "must never run twice" -> "never".
        assert!(!meaning_preserved(
            "The migration must never run twice.",
            "migration never"
        ));
    }

    #[test]
    fn guard_rejects_dropping_an_identifier() {
        assert!(!meaning_preserved(
            "Configuration is missing the required field api_url, so startup aborts.",
            "Configuration api_url,"
        ));
    }

    #[test]
    fn guard_rejects_dropping_a_number() {
        assert!(!meaning_preserved(
            "Retry after 3 seconds.",
            "Retry after seconds."
        ));
    }

    #[test]
    fn guard_rejects_dropping_an_all_caps_marker() {
        assert!(!meaning_preserved("URGENT: disk full", "disk full"));
    }

    #[test]
    fn guard_rejects_dropping_a_severity_word() {
        assert!(!meaning_preserved(
            "WARNING: this is deprecated",
            "this is deprecated"
        ));
    }

    #[test]
    fn guard_allows_removing_plain_filler() {
        // No protected word, number, identifier or marker in the removed text.
        assert!(meaning_preserved(
            "routine background maintenance line describing routine events",
            "routine background maintenance line"
        ));
    }

    #[test]
    fn guard_rejects_filler_containing_a_negation() {
        // "no new information" carries "no"; keeping it is the safe call even
        // though the sentence is filler.
        assert!(!meaning_preserved(
            "routine background maintenance line with no new information",
            "routine background maintenance line new information"
        ));
    }

    #[test]
    fn guard_keeps_codes_and_numbers_intact() {
        let text = "2026-10-18T12:00:00Z INFO req=17 status=200 ms=21 path=/api/v1/items/a";
        let spans = offsets(&[(0, text.len())]);
        let scores = vec![0.9];
        let out = rebuild(text, &spans, &scores, 0.7);
        assert!(meaning_preserved(text, &out), "out: {out}");
    }

    #[test]
    fn guard_rejects_a_log_reduction_that_drops_values() {
        // Removing `status=200` also removes `200`, which the guard treats as a
        // fact worth keeping. Conservative by design.
        let original = "2026-10-18T12:00:00Z INFO req=1 status=200";
        let compressed = original.replace("status=200", "");
        assert!(!meaning_preserved(original, &compressed));
    }

    #[test]
    fn guard_allows_a_log_reduction_that_keeps_values() {
        let original = "INFO INFO INFO req=1 status=200";
        let compressed = "INFO req=1 status=200";
        assert!(meaning_preserved(original, compressed));
    }

    #[test]
    fn config_defaults_are_conservative() {
        let cfg = CompressConfig::default();
        assert!(!cfg.enabled, "expensive model must be opt-in");
        assert_eq!(cfg.roles, vec!["tool"]);
        assert_eq!(cfg.threshold, 0.7);
        // Zero means "derive from this machine". A fixed window cannot be right
        // for both a laptop and a 64 GB workstation, and the old 200..=1500
        // default matched none of the real traffic it was meant for.
        assert_eq!(cfg.min_tokens, 0);
        assert_eq!(cfg.max_tokens, 0);
        assert_eq!(cfg.budget_mb, 0);
        assert_eq!(cfg.budget_ms, 0);
    }

    #[test]
    fn config_parses_partial_toml_table() {
        let value = json!({"enabled": true, "threshold": 0.5, "roles": ["tool", "user"]});
        let cfg: CompressConfig = serde_json::from_value(value).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.threshold, 0.5);
        assert_eq!(cfg.roles, vec!["tool", "user"]);
        // Unspecified keys fall back to the documented defaults.
        assert_eq!(cfg.max_tokens, 0);
        assert_eq!(cfg.budget_mb, 0);
    }

    #[test]
    fn model_dir_prefers_explicit_config() {
        let cfg = CompressConfig {
            model_dir: "/tmp/explicit-kompress".into(),
            ..CompressConfig::default()
        };
        assert_eq!(
            cfg.resolved_model_dir(),
            PathBuf::from("/tmp/explicit-kompress")
        );
    }

    #[test]
    fn shellexpand_only_touches_a_leading_tilde() {
        assert_eq!(shellexpand("/abs/~keep"), "/abs/~keep");
        assert_eq!(shellexpand("relative/path"), "relative/path");
        assert_eq!(shellexpand("~"), std::env::var("HOME").unwrap());
    }

    #[test]
    fn continue_response_has_no_payload() {
        let text = serde_json::to_string(&HostResponse {
            decision: "continue",
            payload: None,
        })
        .unwrap();
        assert_eq!(text, r#"{"decision":"continue"}"#);
    }

    #[test]
    fn replace_response_carries_the_payload() {
        let text = serde_json::to_string(&HostResponse {
            decision: "replace",
            payload: Some(json!({"messages": []})),
        })
        .unwrap();
        assert!(text.contains(r#""decision":"replace""#));
        assert!(text.contains(r#""payload""#));
    }

    #[test]
    fn a_host_request_parses_with_missing_fields() {
        // Defensive: an older or newer host must not crash us.
        let req: HostRequest = serde_json::from_str("{}").unwrap();
        assert!(req.config.is_null());
        assert!(req.payload.is_null());
    }

    #[test]
    fn a_host_request_parses_the_real_shape() {
        let req: HostRequest = serde_json::from_str(
            r#"{"protocol":1,"config":{"enabled":true},"payload":{"messages":[]}}"#,
        )
        .unwrap();
        assert_eq!(req.config["enabled"], true);
        assert!(req.payload["messages"].is_array());
    }

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn no_arguments_means_serve() {
        assert_eq!(parse_args(&[]).unwrap(), Mode::Serve);
    }

    #[test]
    fn fetch_model_is_recognised() {
        assert_eq!(
            parse_args(&args(&["--fetch-model"])).unwrap(),
            Mode::FetchModel { dir: None }
        );
    }

    #[test]
    fn fetch_model_accepts_a_directory() {
        assert_eq!(
            parse_args(&args(&["--fetch-model", "--dir", "/tmp/models"])).unwrap(),
            Mode::FetchModel {
                dir: Some(PathBuf::from("/tmp/models"))
            }
        );
    }

    #[test]
    fn print_manifest_is_recognised() {
        assert_eq!(
            parse_args(&args(&["--print-manifest"])).unwrap(),
            Mode::PrintManifest
        );
    }

    #[test]
    fn unknown_arguments_are_rejected() {
        let error = parse_args(&args(&["--wat"])).unwrap_err();
        assert!(error.contains("unknown argument"), "got: {error}");
    }

    #[test]
    fn a_directory_without_fetch_model_is_rejected() {
        let error = parse_args(&args(&["--dir", "/tmp/x"])).unwrap_err();
        assert!(error.contains("--fetch-model"), "got: {error}");
    }

    #[test]
    fn a_directory_without_a_value_is_rejected() {
        let error = parse_args(&args(&["--fetch-model", "--dir"])).unwrap_err();
        assert!(error.contains("needs a value"), "got: {error}");
    }

    #[test]
    fn help_is_recognised() {
        assert_eq!(parse_args(&args(&["--help"])).unwrap(), Mode::Help);
        assert_eq!(parse_args(&args(&["-h"])).unwrap(), Mode::Help);
    }

    #[test]
    fn the_manifest_is_valid_toml_with_a_setup_hook() {
        let value: toml::Value = toml::from_str(MANIFEST).expect("manifest must parse");
        let args = value["setup"]["args"]
            .as_array()
            .expect("setup.args is an array");
        assert_eq!(args.len(), 1);
        assert_eq!(args[0].as_str(), Some("--fetch-model"));
        assert_eq!(value["setup"]["network"].as_bool(), Some(true));
        assert!(value["setup"]["timeout_secs"].as_integer().unwrap() >= 60);
    }

    #[test]
    fn the_manifest_hook_is_one_of_our_own_arguments() {
        // The host runs the manifest's `args` against this binary. If that ever
        // stopped being a valid argument the hook would silently do nothing.
        let value: toml::Value = toml::from_str(MANIFEST).unwrap();
        let rendered: Vec<String> = value["setup"]["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert!(parse_args(&rendered).is_ok());
    }

    #[test]
    fn the_manifest_declares_every_setting_with_its_real_default() {
        // `klava extensions show` prints this table as the documentation, so a
        // stale default here would lie to the user. Parse it into the real
        // config type and compare with `Default`.
        let value: toml::Value = toml::from_str(MANIFEST).unwrap();
        let declared: CompressConfig = value["settings"].clone().try_into().expect("settings");
        let real = CompressConfig::default();
        assert_eq!(declared.threshold, real.threshold);
        assert_eq!(declared.min_tokens, real.min_tokens);
        assert_eq!(declared.max_tokens, real.max_tokens);
        assert_eq!(declared.min_savings, real.min_savings);
        assert_eq!(declared.budget_mb, real.budget_mb);
        assert_eq!(declared.budget_ms, real.budget_ms);
        assert_eq!(declared.roles, real.roles);
        assert_eq!(declared.skip_structured, real.skip_structured);
        assert_eq!(declared.model_dir, real.model_dir);
        assert_eq!(declared.enabled, real.enabled);
    }

    #[test]
    fn provision_dir_prefers_an_explicit_directory() {
        assert_eq!(
            provision_dir(Some(Path::new("/tmp/explicit")), None),
            PathBuf::from("/tmp/explicit")
        );
    }

    #[test]
    fn provision_dir_defaults_to_the_runtime_cache_directory() {
        // Same path the runtime reads from; a fetch must land where a later run
        // looks.
        let expected = CompressConfig::default().resolved_model_dir();
        assert_eq!(provision_dir(None, None), expected);
    }

    #[test]
    fn provision_dir_honours_model_dir_from_the_host_config() {
        // klava passes the merged settings in $KLAVA_EXTENSION_CONFIG. If the
        // fetch ignored `model_dir` it would download 573 MB into a directory
        // the runtime never reads.
        assert_eq!(
            provision_dir(None, Some(r#"{"model_dir":"/tmp/m"}"#)),
            PathBuf::from("/tmp/m")
        );
    }

    #[test]
    fn provision_dir_falls_back_when_the_host_config_is_unreadable() {
        assert_eq!(
            provision_dir(None, Some("not json")),
            CompressConfig::default().resolved_model_dir()
        );
    }

    #[test]
    fn a_fresh_download_truncates_and_a_resume_appends() {
        // Appending after the server ignored our Range glued a fresh body onto a
        // stale prefix: right length, wrong bytes.
        assert_eq!(part_mode(0), PartMode::Truncate);
        assert_eq!(part_mode(5), PartMode::Append);
    }

    #[test]
    fn model_files_cover_both_required_names() {
        let names: Vec<&str> = MODEL_FILES.iter().map(|(_, local)| *local).collect();
        assert!(names.contains(&TOKENIZER_FILE));
        assert!(names.contains(&MODEL_FILE));
    }

    // -----------------------------------------------------------------------
    // Cost model
    // -----------------------------------------------------------------------

    /// Measured peaks on this machine, in megabytes (see the module docs).
    const MEASURED: [(usize, f64); 5] = [
        (2_000, 1_200.0),
        (4_000, 2_900.0),
        (8_000, 8_600.0),
        (11_000, 15_700.0),
        (14_000, 26_700.0),
    ];

    #[test]
    fn the_cost_model_reproduces_the_measured_table() {
        // These numbers are the whole reason for the planner: memory is
        // quadratic, so a fixed token ceiling cannot be right for every machine.
        // If the fit drifts the budget stops bounding anything, so pin it.
        for (tokens, measured) in MEASURED {
            let predicted = predicted_peak_mb(tokens);
            assert!(
                predicted >= measured,
                "{tokens} tokens: predicted {predicted:.0} MB is below the measured {measured:.0} MB; \
                 underestimating is what gets the process OOM-killed"
            );
            assert!(
                predicted <= measured * 1.5,
                "{tokens} tokens: predicted {predicted:.0} MB is far above the measured {measured:.0} MB; \
                 the ceiling would be uselessly low"
            );
        }
    }

    #[test]
    fn the_cost_model_is_quadratic_not_linear() {
        // Doubling the tokens must more than double the cost, or the planner's
        // measurement pass means nothing.
        let small = predicted_peak_mb(2_000) - MODEL_RESIDENT_MB;
        let large = predicted_peak_mb(4_000) - MODEL_RESIDENT_MB;
        assert!((large / small - 4.0).abs() < 0.01, "got {}", large / small);
    }

    #[test]
    fn wall_time_tracks_the_measurements() {
        // 14k tokens measured 54 s; the fit must not promise something the host
        // will kill us for long before.
        let predicted = predicted_secs(14_000);
        assert!((40.0..=70.0).contains(&predicted), "got {predicted:.1}s");
    }

    #[test]
    fn the_budget_ceiling_covers_the_90th_percentile_of_real_traffic() {
        // Real tool output: p50 188, p90 2,716, max 5,090 tokens. A ceiling below
        // p90 rejects a tenth of traffic for an arbitrary reason, which is how
        // the old fixed 1,500 window ended up matching nothing.
        let ceiling = tokens_that_fit(MAX_BUDGET_MB);
        assert!(ceiling >= 2_716, "ceiling {ceiling} is below p90");
        assert!(
            ceiling < HARD_MAX_TOKENS,
            "ceiling {ceiling} should be budget-derived"
        );
    }

    #[test]
    fn the_hard_ceiling_cannot_be_exceeded() {
        // Whatever the budget says, one message never gets more than this. ~41k
        // tokens once asked ONNX Runtime for a single 80 GB buffer.
        assert_eq!(tokens_that_fit(usize::MAX), HARD_MAX_TOKENS);
        const { assert!(HARD_MAX_TOKENS <= 4_000) };
    }

    #[test]
    fn a_larger_budget_never_lowers_the_ceiling() {
        let mut previous = 0;
        for budget in [MIN_BUDGET_MB, 1_500, 2_000, 4_000, 8_000] {
            let ceiling = tokens_that_fit(budget);
            assert!(
                ceiling >= previous,
                "{budget} MB gave {ceiling}, below {previous}"
            );
            previous = ceiling;
        }
    }

    #[test]
    fn a_budget_too_small_for_the_graph_yields_no_ceiling() {
        // Reporting a "usable" non-zero ceiling here would be a lie that turns
        // into an overshoot: the graph alone exceeds the budget.
        assert_eq!(tokens_that_fit(0), 0);
        assert_eq!(tokens_that_fit(MODEL_RESIDENT_MB as usize), 0);
    }

    #[test]
    fn detection_reserves_memory_for_everything_else() {
        // The budget must leave the machine usable, not merely leave a number
        // that happens to fit. On this 30 GB box the cgroup is unlimited, so the
        // answer comes from `MemAvailable`: somewhere between the graph's floor
        // and the cap.
        let budget = detect_budget_mb();
        assert!(budget >= MIN_BUDGET_MB, "detected {budget} MB");
        assert!(budget <= MAX_BUDGET_MB, "detected {budget} MB");
    }

    #[test]
    fn a_busy_machine_still_reserves_a_floor_of_memory() {
        // With only 1 GB free, a quarter is 256 MB — not enough to leave the
        // proxy alive. The reserve floor wins, so the budget goes to zero rather
        // than eating the last gigabyte.
        assert_eq!(budget_from_available_mb(Some(1_024)), 0);
        // Just above the reserve floor there is still nothing left over.
        assert_eq!(budget_from_available_mb(Some(MIN_RESERVE_MB)), 0);
    }

    #[test]
    fn an_unmeasurable_machine_gets_a_usable_conservative_budget() {
        // Windows and macOS used to land here because `available_mb` was Linux
        // only. Even a target we truly cannot measure must not collapse to a
        // zero budget: the extension would go silent, which is worse than a
        // modest ceiling the user can raise with `budget_mb`.
        assert_eq!(budget_from_available_mb(None), MIN_BUDGET_MB);
        assert!(tokens_that_fit(MIN_BUDGET_MB) > 0);
    }

    #[test]
    fn a_roomy_machine_gets_the_cap_and_never_more() {
        // A 128 GB box must not hand out a 100 GB inference just because the
        // memory is there: the cap is what keeps a single message bounded.
        assert_eq!(budget_from_available_mb(Some(131_072)), MAX_BUDGET_MB);
        // And a 16 GB Mac with half its memory reclaimable clears p90.
        let mac = budget_from_available_mb(Some(8 * 1024));
        assert!(mac > MIN_BUDGET_MB, "got {mac} MB");
        assert!(tokens_that_fit(mac) > 2_716, "got {}", tokens_that_fit(mac));
    }

    #[test]
    fn mac_page_counts_become_megabytes() {
        // 4 KiB pages, as on the Intel agent: 1 GB of reclaimable pages is
        // 262,144 pages and must not be reported as 262,144 MB.
        assert_eq!(pages_available_mb(131_072, 131_072, 0, 4096), Some(1_024));
        // On Apple Silicon pages are 16 KiB, and the answer is the same.
        assert_eq!(pages_available_mb(32_768, 32_768, 0, 16_384), Some(1_024));
        // Speculative pages count too: dropping them would under-report a Mac
        // that caches aggressively and shrink its ceiling for no reason.
        assert_eq!(pages_available_mb(0, 0, 262_144, 4096), Some(1_024));
        // No pages at all is a measurement, not a failure.
        assert_eq!(pages_available_mb(0, 0, 0, 4096), Some(0));
    }

    #[test]
    fn mac_page_arithmetic_does_not_overflow() {
        // A nonsense count must yield `None` (so the caller falls back) rather
        // than a wrapped number that reads as an enormous budget.
        assert_eq!(pages_available_mb(usize::MAX, usize::MAX, 0, 4096), None);
        assert_eq!(pages_available_mb(1, 1, 0, usize::MAX), None);
    }

    #[test]
    fn windows_available_bytes_become_megabytes() {
        assert_eq!(bytes_available_mb(8 * 1024 * 1024 * 1024), 8 * 1024);
        assert_eq!(bytes_available_mb(0), 0);
    }

    #[test]
    fn the_planner_is_silent_when_detection_finds_nothing_to_spend() {
        // The fail-open path for a machine with no memory to spare: no inference,
        // no payload, the request passes through untouched.
        let compressor = Compressor::new(CompressConfig {
            budget_mb: 1,
            ..CompressConfig::default()
        });
        assert_eq!(compressor.max_tokens, 0);
        let model = loaded_without_a_model();
        assert!(model.scorer.get().is_none());
    }

    #[test]
    fn cgroup_max_means_unlimited() {
        // `memory.max` is the literal `max` when a cgroup has no limit; treating
        // that as bytes would be a bizarre number.
        assert_eq!(parse_cgroup_bytes("max\n"), None);
        assert_eq!(parse_cgroup_bytes("2147483648\n"), Some(2_147_483_648));
        assert_eq!(parse_cgroup_bytes("not a number"), None);
    }

    #[test]
    fn meminfo_reports_available_memory_in_megabytes() {
        let meminfo = "MemTotal:       32000000 kB\nMemAvailable:    4194304 kB\nMemFree:  1 kB\n";
        assert_eq!(meminfo_available_mb(meminfo), Some(4096));
        assert_eq!(meminfo_available_mb("MemTotal: 1 kB\n"), None);
    }

    /// Build a throwaway cgroup tree: `(relative path, memory.max, memory.current)`.
    fn fake_cgroup(name: &str, dirs: &[(&str, &str, &str)]) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("kompress-cgroup-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        for (path, limit, current) in dirs {
            let dir = root.join(path);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("memory.max"), limit).unwrap();
            std::fs::write(dir.join("memory.current"), current).unwrap();
        }
        root
    }

    #[test]
    fn cgroup_headroom_is_limit_minus_usage() {
        let root = fake_cgroup("basic", &[("unit.slice", "4294967296\n", "1073741824\n")]);
        assert_eq!(
            cgroup_headroom_mb(&root, "/unit.slice"),
            Some(3 * 1024),
            "4 GB limit with 1 GB used is 3 GB of headroom"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn cgroup_walk_keeps_the_tightest_headroom_on_the_path() {
        // The real trap: a unit has no limit of its own while the slice above it
        // does. Reading only the leaf would report "unlimited" and let a 20 GB
        // inference run.
        let root = fake_cgroup(
            "nested",
            &[
                ("user.slice", "4294967296\n", "1073741824\n"),
                ("user.slice/unit.slice", "max\n", "0\n"),
            ],
        );
        assert_eq!(
            cgroup_headroom_mb(&root, "/user.slice/unit.slice"),
            Some(3 * 1024),
            "the parent's limit must still apply"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn cgroup_headroom_uses_the_tighter_of_two_limits() {
        let root = fake_cgroup(
            "two",
            &[
                ("a.slice", "8589934592\n", "0\n"),
                ("a.slice/b.slice", "4294967296\n", "2147483648\n"),
            ],
        );
        // 8 GB free above, 2 GB free at the leaf: the leaf binds.
        assert_eq!(
            cgroup_headroom_mb(&root, "/a.slice/b.slice"),
            Some(2 * 1024)
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn an_unlimited_cgroup_reports_nothing_so_meminfo_is_used() {
        // `max` everywhere means "no cgroup limit"; returning a huge number
        // would defeat the `MemAvailable` fallback entirely.
        let root = fake_cgroup("unlimited", &[("x.slice", "max\n", "0\n")]);
        assert_eq!(cgroup_headroom_mb(&root, "/x.slice"), None);
        let _ = std::fs::remove_dir_all(root);
    }

    // -----------------------------------------------------------------------
    // Configuration: the window is derived, not guessed
    // -----------------------------------------------------------------------

    /// A tokenizer with no model file behind it: one token per whitespace word.
    ///
    /// Every word is `[UNK]`, which is all the size guard needs — it counts
    /// tokens, it does not care what they mean.
    fn tiny_tokenizer() -> Tokenizer {
        let json = r#"{
            "version": "1.0",
            "truncation": null,
            "padding": null,
            "added_tokens": [],
            "normalizer": null,
            "pre_tokenizer": {"type": "Whitespace"},
            "post_processor": null,
            "decoder": null,
            "model": {
                "type": "WordLevel",
                "vocab": {"[UNK]": 0, "kept": 1},
                "unk_token": "[UNK]"
            }
        }"#;
        Tokenizer::from_bytes(json.as_bytes()).expect("test tokenizer")
    }

    fn loaded_without_a_model() -> Loaded {
        Loaded {
            tokenizer: tiny_tokenizer(),
            scorer: OnceLock::new(),
            dir: PathBuf::from("/nonexistent-kompress-test-model"),
        }
    }

    fn compressor_with(budget_mb: usize, budget_ms: usize) -> Compressor {
        Compressor::new(CompressConfig {
            budget_mb,
            budget_ms,
            ..CompressConfig::default()
        })
    }

    fn candidate(message: usize, tokens: usize) -> Candidate {
        Candidate {
            message,
            part: None,
            tokens,
        }
    }

    #[test]
    fn the_floor_admits_ordinary_tool_output() {
        // The old fixed floor of 200 rejected the median real message (188
        // tokens) outright, so the extension compressed nothing.
        let compressor = compressor_with(1_024, DEFAULT_BUDGET_MS);
        assert_eq!(compressor.min_tokens, 2 * MIN_SAVED_BYTES / BYTES_PER_TOKEN);
        assert!(
            compressor.min_tokens <= 188,
            "floor {} is above the median",
            compressor.min_tokens
        );
    }

    #[test]
    fn the_ceiling_comes_from_the_budget_not_from_the_config() {
        let compressor = compressor_with(1_024, DEFAULT_BUDGET_MS);
        assert_eq!(compressor.max_tokens, tokens_that_fit(1_024));
    }

    #[test]
    fn a_configured_ceiling_can_only_lower_the_ceiling() {
        // `max_tokens` is the user's preference, but it may not override physics:
        // whatever it says, the budget wins.
        let low = Compressor::new(CompressConfig {
            max_tokens: 10,
            budget_mb: 2_048,
            ..CompressConfig::default()
        });
        assert_eq!(low.max_tokens, 10);

        let absurd = Compressor::new(CompressConfig {
            max_tokens: 100_000,
            budget_mb: 1_024,
            ..CompressConfig::default()
        });
        assert_eq!(absurd.max_tokens, tokens_that_fit(1_024));
        assert!(absurd.max_tokens <= HARD_MAX_TOKENS);
    }

    #[test]
    fn an_absurd_budget_is_clamped() {
        let compressor = compressor_with(usize::MAX, DEFAULT_BUDGET_MS);
        assert_eq!(compressor.budget_mb, MAX_BUDGET_MB);
        assert_eq!(compressor.max_tokens, tokens_that_fit(MAX_BUDGET_MB));
    }

    // -----------------------------------------------------------------------
    // The window is enforced before the graph exists
    // -----------------------------------------------------------------------

    #[test]
    fn an_out_of_window_message_is_rejected_without_loading_the_graph() {
        // This is the original bug: the window was checked *after* inference, so
        // 93% of requests paid 1.1 GB and a full forward pass to be discarded.
        let model = loaded_without_a_model();
        let compressor = Compressor::new(CompressConfig {
            budget_mb: 1_024,
            budget_ms: DEFAULT_BUDGET_MS,
            max_tokens: 20,
            ..CompressConfig::default()
        });

        let text = "word ".repeat(200);
        assert!(compressor.measure(0, None, &text, &model).is_none());
        assert!(
            model.scorer.get().is_none(),
            "the graph must not be mapped for a message the window rejects"
        );
    }

    #[test]
    fn an_in_window_message_becomes_a_candidate_without_loading_the_graph() {
        let model = loaded_without_a_model();
        let compressor = compressor_with(2_048, DEFAULT_BUDGET_MS);
        let text = "word ".repeat(400);
        let measured = compressor
            .measure(3, Some(1), &text, &model)
            .expect("candidate");
        assert_eq!(measured.message, 3);
        assert_eq!(measured.part, Some(1));
        assert_eq!(measured.tokens, 400);
        assert!(model.scorer.get().is_none());
    }

    #[test]
    fn a_message_too_short_to_clear_the_byte_floor_is_rejected_cheaply() {
        // Rejected on raw length, before tokenizing: no rewrite can remove 200
        // bytes from a message that is not 200 bytes long.
        let model = loaded_without_a_model();
        let compressor = compressor_with(2_048, DEFAULT_BUDGET_MS);
        let text = "short log line";
        assert!(compressor.measure(0, None, text, &model).is_none());
        assert!(model.scorer.get().is_none());
    }

    #[test]
    fn structured_content_is_rejected_before_the_graph() {
        let model = loaded_without_a_model();
        let compressor = compressor_with(2_048, DEFAULT_BUDGET_MS);
        let sql = "SELECT * FROM users WHERE id = 1; ".repeat(20);
        assert!(compressor.measure(0, None, &sql, &model).is_none());
        assert!(model.scorer.get().is_none());
    }

    #[test]
    fn candidates_respects_roles_and_part_shapes() {
        let model = loaded_without_a_model();
        let compressor = compressor_with(2_048, DEFAULT_BUDGET_MS);
        let payload = json!({
            "messages": [
                {"role": "system", "content": "word ".repeat(300)},
                {"role": "assistant", "content": "word ".repeat(300)},
                {"role": "tool", "content": "word ".repeat(300)},
                {"role": "tool", "content": [
                    {"type": "text", "text": "word ".repeat(300)},
                    {"type": "image_url", "image_url": {"url": "x"}},
                    {"type": "text", "text": "word ".repeat(300)}
                ]},
                {"role": "tool", "content": 42}
            ]
        });

        let found = compressor.candidates(&payload, &model);
        assert_eq!(found.len(), 3, "{found:?}");
        assert_eq!(found[0].message, 2);
        assert_eq!(found[0].part, None);
        assert_eq!(found[1].message, 3);
        assert_eq!(found[1].part, Some(0));
        assert_eq!(found[2].part, Some(2));
    }

    // -----------------------------------------------------------------------
    // The planner
    // -----------------------------------------------------------------------

    #[test]
    fn the_planner_runs_the_smallest_messages_first() {
        // Cost per saved token falls as the message shrinks, so under a budget
        // the small ones are always the better buy.
        let compressor = compressor_with(MAX_BUDGET_MB, 60_000);
        let selection = compressor.plan(vec![
            candidate(0, 1_800),
            candidate(1, 120),
            candidate(2, 800),
        ]);
        let order: Vec<usize> = selection.chosen.iter().map(|c| c.tokens).collect();
        assert_eq!(order, vec![120, 800, 1_800]);
    }

    #[test]
    fn the_planner_never_exceeds_the_memory_budget() {
        let budget = 1_300;
        let compressor = compressor_with(budget, 600_000);
        let selection = compressor.plan(vec![
            candidate(0, 100),
            candidate(1, 2_000),
            candidate(2, 1_400),
        ]);

        let peak = MODEL_RESIDENT_MB + selection.mb;
        assert!(
            peak <= budget as f64,
            "planned {peak:.0} MB against {budget} MB"
        );
        assert!(
            !selection.chosen.is_empty(),
            "something small should still fit"
        );
        assert!(
            selection.chosen.iter().all(|c| c.tokens < 2_000),
            "{:?}",
            selection.chosen
        );
    }

    #[test]
    fn the_planner_never_exceeds_the_time_budget() {
        // klava kills the process at 20 s by default, so the planner must return
        // partial work instead of being killed with nothing to show.
        let compressor = compressor_with(MAX_BUDGET_MB, 3_000);
        let selection = compressor.plan(vec![candidate(0, 300), candidate(1, 3_500)]);
        assert_eq!(selection.chosen.len(), 1, "{:?}", selection.chosen);
        assert!(MODEL_LOAD_SECS + selection.secs <= 3.0);
    }

    #[test]
    fn a_budget_below_the_model_selects_nothing() {
        // Under the graph's own footprint every inference is over budget, and
        // the honest answer is to run none rather than to be OOM-killed.
        let compressor = compressor_with(500, 60_000);
        let selection = compressor.plan(vec![candidate(0, 100), candidate(1, 200)]);
        assert!(selection.chosen.is_empty());
        assert_eq!(selection.mb, 0.0);
    }

    #[test]
    fn the_planner_stops_instead_of_skipping_ahead() {
        // Candidates are sorted ascending and both costs grow with `n`, so once
        // one does not fit, nothing after it can. Skipping instead of stopping
        // would only cost a longer walk with the same result.
        let compressor = compressor_with(MAX_BUDGET_MB, 1_500);
        let selection = compressor.plan(vec![candidate(0, 14_000), candidate(1, 15_000)]);
        assert!(selection.chosen.is_empty());
    }

    #[test]
    fn the_planner_reports_marginal_cost_not_the_model_load() {
        // `mb`/`secs` describe what the plan adds on top of the resident graph.
        // Adding the load back in must give the per-message prediction.
        let compressor = compressor_with(MAX_BUDGET_MB, 60_000);
        let selection = compressor.plan(vec![candidate(0, 2_000)]);
        assert_eq!(selection.chosen.len(), 1);
        assert!((MODEL_RESIDENT_MB + selection.mb - predicted_peak_mb(2_000)).abs() < 1e-6);
        assert!((MODEL_LOAD_SECS + selection.secs - predicted_secs(2_000)).abs() < 1e-6);
    }
}
