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
//! [model]: https://huggingface.co/chopratejas/kompress-v2-base

use ort::session::Session;
use ort::value::Tensor;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
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
min_tokens = 200
max_tokens = 1500
min_savings = 0.1
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
    /// Skip messages shorter than this many tokens (fixed cost dominates).
    #[serde(default = "default_min_tokens")]
    pub min_tokens: usize,
    /// Skip messages longer than this many tokens (inference gets expensive).
    #[serde(default = "default_max_tokens")]
    pub max_tokens: usize,
    /// Discard a result unless it removes at least this fraction of tokens.
    ///
    /// Prevents paying inference cost to shave off a token or two.
    #[serde(default = "default_min_savings")]
    pub min_savings: f32,
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
    200
}
fn default_max_tokens() -> usize {
    1500
}
fn default_min_savings() -> f32 {
    0.10
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
// Model
// ---------------------------------------------------------------------------

/// A loaded model. The tokenizer is immutable; the session needs `&mut` to run.
struct Model {
    tokenizer: Tokenizer,
    session: Mutex<Session>,
}

impl std::fmt::Debug for Model {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Model").finish_non_exhaustive()
    }
}

impl Model {
    fn load(dir: &Path) -> Result<Self, String> {
        let tokenizer_path = dir.join(TOKENIZER_FILE);
        let model_path = dir.join(MODEL_FILE);
        let tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| format!("cannot load {}: {e}", tokenizer_path.display()))?;
        let session = Session::builder()
            .map_err(|e| format!("cannot create ONNX session: {e}"))?
            .commit_from_file(&model_path)
            .map_err(|e| format!("cannot load {}: {e}", model_path.display()))?;
        Ok(Self {
            tokenizer,
            session: Mutex::new(session),
        })
    }

    /// Per-token keep probability for `text`.
    fn score(&self, text: &str) -> Result<(Vec<usize>, Vec<f32>), String> {
        let encoding = self
            .tokenizer
            .encode(text, false)
            .map_err(|e| format!("tokenizer failed: {e}"))?;

        let ids: Vec<i64> = encoding.get_ids().iter().map(|&id| id as i64).collect();
        let n = ids.len();
        if n == 0 {
            return Ok((Vec::new(), Vec::new()));
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

        // Byte offsets let us rebuild the text without guessing at whitespace.
        let offsets: Vec<usize> = encoding
            .get_offsets()
            .iter()
            .flat_map(|&(start, end)| [start, end])
            .collect();

        Ok((offsets, scores.to_vec()))
    }
}

// ---------------------------------------------------------------------------
// Extension
// ---------------------------------------------------------------------------

pub struct Compressor {
    config: CompressConfig,
    model: OnceLock<Option<Model>>,
}

impl std::fmt::Debug for Compressor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Compressor")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl Compressor {
    pub fn new(config: CompressConfig) -> Self {
        Self {
            config,
            model: OnceLock::new(),
        }
    }

    /// Load once on first use. A failure is cached too, so a broken install logs
    /// once instead of on every request.
    fn model(&self) -> Option<&Model> {
        self.model
            .get_or_init(|| match Model::load(&self.config.resolved_model_dir()) {
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
    fn compress_text(&self, model: &Model, text: &str) -> Option<(String, usize, usize)> {
        if self.config.skip_structured && looks_structured(text) {
            eprintln!("kompress: skipping structured content");
            return None;
        }

        let (offsets, scores) = match model.score(text) {
            Ok(scored) => scored,
            Err(e) => {
                eprintln!("kompress: {e}");
                return None;
            }
        };

        let tokens = scores.len();
        if offsets.len() != tokens * 2 {
            eprintln!(
                "kompress: {} score(s) but {} offset pair(s); skipping",
                tokens,
                offsets.len() / 2
            );
            return None;
        }
        if tokens < self.config.min_tokens || tokens > self.config.max_tokens {
            eprintln!(
                "kompress: skipping {} token message (window {}..={})",
                tokens, self.config.min_tokens, self.config.max_tokens
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

        Some((rebuilt, tokens, kept))
    }

    /// Compress one `content` value in place. Returns (tokens before, after).
    fn compress_content(&self, model: &Model, content: &mut Value) -> (usize, usize) {
        match content {
            Value::String(text) => match self.compress_text(model, text) {
                Some((next, before, after)) => {
                    *text = next;
                    (before, after)
                }
                None => (0, 0),
            },
            Value::Array(parts) => {
                let mut before = 0;
                let mut after = 0;
                for part in parts {
                    // Multimodal parts (images, files) carry no `text`.
                    if let Some(Value::String(text)) = part.get_mut("text")
                        && let Some((next, b, a)) = self.compress_text(model, text)
                    {
                        *text = next;
                        before += b;
                        after += a;
                    }
                }
                (before, after)
            }
            _ => (0, 0),
        }
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

        let messages = payload.get("messages").and_then(Value::as_array)?;
        let originals = messages.clone();

        let mut payload = payload;
        let mut before_total = 0usize;
        let mut after_total = 0usize;
        let mut touched = 0usize;

        if let Some(list) = payload.get_mut("messages").and_then(Value::as_array_mut) {
            for (message, original) in list.iter_mut().zip(originals.iter()) {
                let role = original.get("role").and_then(Value::as_str).unwrap_or("");
                if !self.config.roles.iter().any(|r| r == role) {
                    continue;
                }
                let Some(content) = message.get_mut("content") else {
                    continue;
                };
                let (before, after) = self.compress_content(model, content);
                if before > 0 && after < before {
                    before_total += before;
                    after_total += after;
                    touched += 1;
                }
            }
        }

        if touched == 0 || after_total >= before_total {
            return None;
        }

        eprintln!(
            "kompress: {touched} message(s), {before_total} -> {after_total} tokens ({:.0}% saved)",
            100.0 * (1.0 - after_total as f32 / before_total as f32)
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
        assert!(cfg.min_tokens < cfg.max_tokens);
    }

    #[test]
    fn config_parses_partial_toml_table() {
        let value = json!({"enabled": true, "threshold": 0.5, "roles": ["tool", "user"]});
        let cfg: CompressConfig = serde_json::from_value(value).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.threshold, 0.5);
        assert_eq!(cfg.roles, vec!["tool", "user"]);
        // Unspecified keys fall back to the documented defaults.
        assert_eq!(cfg.max_tokens, 1500);
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
}
