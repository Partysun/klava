use crate::error::{Error, Result};
use crate::providers::{Config as ProviderConfig, Model};
use serde::{Deserialize, Serialize};
use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};

/// How klava picks one entry when several `[[models]]` share a name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Routing {
    /// Random pick per request (default).
    #[default]
    Shuffle,
    /// Cycle through the pool in declaration order.
    RoundRobin,
    /// Always the first entry (declaration order = failover priority).
    First,
}

impl Routing {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "shuffle" | "random" | "" => Some(Self::Shuffle),
            "round_robin" | "round-robin" | "rr" => Some(Self::RoundRobin),
            "first" | "failover" => Some(Self::First),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Shuffle => "shuffle",
            Self::RoundRobin => "round_robin",
            Self::First => "first",
        }
    }
}

fn default_routing() -> String {
    Routing::Shuffle.as_str().to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub port: u16,
    #[serde(default)]
    pub verbose: bool,
    /// Log an estimated token count for each request.
    #[serde(default)]
    pub enable_token_stats: bool,

    // ---- Active models ----------------------------------------------------
    /// Model used for ordinary (non-reasoning) requests. Names a `[[models]]`
    /// entry.
    #[serde(default)]
    pub completion_model: Option<String>,
    /// Model used when the client asks for reasoning/thinking. Names a
    /// `[[models]]` entry; falls back to `completion_model`.
    #[serde(default)]
    pub reasoning_model: Option<String>,
    /// Default reasoning effort sent upstream, e.g. `"high"` or `"max"`.
    ///
    /// Applied when the client asks for nothing (many harnesses never send an
    /// effort) and clamped by `reasoning_effort_cap`. A client that explicitly
    /// disables reasoning is never overridden, and when this is unset the
    /// field is omitted from the request entirely — which is what endpoints
    /// that reject `reasoning_effort` need. A `[[models]]` entry may override
    /// it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Highest effort any request may use, e.g. `"high"`. Useful when one
    /// config serves several models and some do not accept `xhigh`/`max`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort_cap: Option<String>,
    /// How duplicate `[[models]]` names are routed: `shuffle` (default),
    /// `round_robin` or `first`.
    #[serde(default = "default_routing")]
    pub routing: String,

    /// The routable models. Each entry carries its own connection settings;
    /// entries that share `name` form a pool.
    #[serde(default)]
    pub models: Vec<Model>,

    // ---- Web search -------------------------------------------------------
    /// Offer a web-search tool to the agent. On by default; it only makes
    /// sense for models that carry a search backend (the klava gateway).
    #[serde(default = "default_enable_websearch")]
    pub enable_websearch: bool,
    /// Model used to run the search request. Defaults to the active
    /// completion model when unset.
    #[serde(default)]
    pub search_model: Option<String>,
    /// Search backend family (klava vocabulary): `web_fast`, `web_deep`,
    /// `web_native`, `yandex`, or `auto`. Maps to the internal engine/mode
    /// pair. `yandex` runs through the klava gateway (Yandex Search + a
    /// synthesis completion) and is only available with the klava provider.
    #[serde(default = "default_search_engine")]
    pub search_engine: String,
    /// How much page context to pull into the answer: `low`, `medium`, `high`.
    #[serde(default = "default_search_context")]
    pub search_context: String,
    /// Upper bound on results the backend may consider.
    #[serde(default = "default_search_max_results")]
    pub search_max_results: u32,
    /// Instruction prepended to the generated search query.
    #[serde(default)]
    pub search_prompt: Option<String>,
    /// Restrict the search to these domains (empty = no restriction).
    #[serde(default)]
    pub search_include_domains: Vec<String>,
    /// Exclude these domains from the search (empty = no restriction).
    #[serde(default)]
    pub search_exclude_domains: Vec<String>,
    /// Let the decision model tune the search settings per query. When off,
    /// the static config values above are used as-is.
    #[serde(default = "default_search_decide")]
    pub search_decide: bool,

    // ---- Decision model ---------------------------------------------------
    /// The universal decide model id (jigor / System One), naming exactly one
    /// `[[models]]` entry. Every classifier call klava makes — web-search
    /// tuning, and any extension that asks for a `decide` — uses this one
    /// model, so there is a single place to change it. The entry's type and
    /// URL select the jigor backend automatically.
    ///
    /// `search_decide_model` is accepted as a legacy alias when loading.
    #[serde(default = "default_decide_model", alias = "search_decide_model")]
    pub decide_model: String,

    /// Require an approved `decide` from an extension before the host performs
    /// its `nested_call`.
    #[serde(default = "default_require_decide_for_nested_calls")]
    pub require_decide_for_nested_calls: bool,

    /// Built-in, always-compiled extensions (see `crate::extensions`).
    #[serde(default)]
    pub extensions: ExtensionsConfig,

    // ---- Legacy (pre-[[models]]) ------------------------------------------
    /// Legacy `[[providers]]` tables, read once and migrated into `models`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<ProviderConfig>,
    /// Legacy `active_provider` name, read once during migration.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub active_provider: String,
}

/// Extension settings (`[extensions]`).
///
/// Deliberately opaque: klava stores whatever each extension crate declares
/// under `settings.<name>`, so adding an extension never touches this file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExtensionsConfig {
    /// Per-extension tables, keyed by whatever the user installed.
    ///
    /// Every table may carry `enabled` (the only key the host interprets);
    /// all other keys are opaque and passed to the extension as-is.
    #[serde(default)]
    pub settings: std::collections::BTreeMap<String, serde_json::Value>,
}

fn default_enable_websearch() -> bool {
    true
}

/// Strip a harness-appended context-window tag from a requested model name.
///
/// Claude Code spells a 1M-context request as `<model>[1m]`. The tag is not
/// part of the model id and no `[[models]]` entry carries it, so matching
/// against it always misses — which would silently route the request to the
/// default model instead of the one the user picked.
fn strip_context_suffix(name: &str) -> &str {
    name.strip_suffix("[1m]").unwrap_or(name).trim_end()
}

fn default_search_engine() -> String {
    "auto".to_string()
}

fn default_search_context() -> String {
    "medium".to_string()
}

fn default_search_max_results() -> u32 {
    5
}

fn default_search_decide() -> bool {
    true
}

fn default_decide_model() -> String {
    crate::decision::DEFAULT_DECIDE_MODEL.to_string()
}

fn default_require_decide_for_nested_calls() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: 48017,
            verbose: false,
            enable_token_stats: false,
            completion_model: None,
            reasoning_model: None,
            reasoning_effort: None,
            reasoning_effort_cap: None,
            routing: default_routing(),
            models: vec![],
            enable_websearch: default_enable_websearch(),
            search_model: None,
            search_engine: default_search_engine(),
            search_context: default_search_context(),
            search_max_results: default_search_max_results(),
            search_prompt: None,
            search_include_domains: vec![],
            search_exclude_domains: vec![],
            search_decide: default_search_decide(),
            decide_model: default_decide_model(),
            require_decide_for_nested_calls: default_require_decide_for_nested_calls(),
            extensions: ExtensionsConfig::default(),
            providers: vec![],
            active_provider: String::new(),
        }
    }
}

/// Round-robin cursor shared by every config clone in the process.
static ROUND_ROBIN: AtomicUsize = AtomicUsize::new(0);

impl Config {
    pub const APP_NAME: &str = "klava";

    /// Get the config file path
    pub fn get_path() -> std::path::PathBuf {
        // `KLAVA_CONFIG_FILE` pins the config to an explicit path: useful for
        // a second profile (e.g. a test workspace) and for hermetic tests,
        // which must never rewrite a user's real config.
        if let Some(path) = std::env::var_os("KLAVA_CONFIG_FILE").filter(|p| !p.is_empty()) {
            return std::path::PathBuf::from(path);
        }
        confy::get_configuration_file_path(Self::APP_NAME, Some("config"))
            .expect("Failed to get path")
    }

    /// Create the config directory if it doesn't exist
    pub fn ensure_dir() -> Result<()> {
        let path = Self::get_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        Ok(())
    }

    /// Create config file with default values if it doesn't exist
    pub fn ensure_exists(force: bool) -> Result<std::path::PathBuf> {
        let path = Self::get_path();

        if force || !path.exists() {
            Self::ensure_dir()?;
            Self::write_template()?;
            println!("✓ Created config file at: {}", path.display());
            println!();
        } else {
            println!("Config file already exists at: {}", path.display());
            println!();
        }

        // Show configuration details
        Self::show_config_details();

        Ok(path)
    }

    /// Show detailed information about configuration fields
    fn show_config_details() {
        println!("📝 Configuration fields:");
        println!();
        println!("completion_model: Default model for ordinary requests");
        println!("reasoning_model:  Default model when the client asks for reasoning");
        println!("reasoning_effort: Default effort sent upstream: none|minimal|low|");
        println!("                  medium|high|xhigh|max. Omitted when unset.");
        println!("reasoning_effort_cap: Clamp every request to this level or below");
        println!("models:           List of [[models]] entries (name, type, base_url, ...)");
        println!("                  Add entries with 'klava providers add'");
        println!();
        println!("port:             Port to listen on");
        println!("                  (default: 48017), can be set via PORT env var");
        println!();
        println!("verbose:          Enable verbose logging");
        println!("                  Logs full request/response bodies");
        println!();
        println!("enable_websearch: Offer the agent a web-search tool (default: true)");
        println!("                  Only active with a klava-type search model");
        println!();
    }

    /// Write the config file template with commented examples
    fn write_template() -> Result<()> {
        let path = Self::get_path();
        let template = r#"# Klava Configuration File
#
# Routing is per model: every [[models]] entry is self-contained and the
# model name the agent sends selects its entry. Entries that share a name
# form a pool, picked per request according to `routing`.
#
# The easiest way to fill this in:
#   klava providers add      # Klava, OpenRouter, Cloudflare, Qwen, custom
#   klava providers list
#   klava providers check

# Default models (they name entries below).
#completion_model = "~deepseek/deepseek-flash-latest"
#reasoning_model = "~deepseek/deepseek-flash-latest"

# Default reasoning effort sent upstream when the client does not ask for one.
# A client's own level (opencode variants, Claude Code /effort, codex
# reasoning.effort) always wins; this only fills the gap. Leave it unset to
# send no `reasoning_effort` at all, which is what endpoints that reject the
# field need.
#reasoning_effort = "high"
# Highest level any request may use, clamping both this default and a level the
# client asked for. Useful when one config serves models that reject xhigh/max.
#reasoning_effort_cap = "max"

# How duplicate model names are routed: "shuffle" (default), "round_robin",
# or "first" (declaration order = failover priority).
#routing = "shuffle"

# Klava provider (klava.zatsepin.dev) — sign in with `klava login` (browser
# device OAuth). The key and models are written by `klava providers add klava`.
# [[models]]
# name = "~deepseek/deepseek-flash-latest"
# type = "klava"
# reasoning_effort = "max"       # overrides the top-level default

# OpenAI-compatible model (OpenRouter, Cloudflare, DeepSeek, local LLMs)
# [[models]]
# name = "~deepseek/deepseek-flash-latest"
# type = "openai-compatible"
# base_url = "https://openrouter.ai/api"
# api_key_name = "OPENROUTER_API_KEY"
# # optional: real upstream model id when the public name differs
# # model = "deepseek/deepseek-flash-latest"

# Qwen Code model (mimics qwen-code; you need a qwen-code paid plan)
# [[models]]
# name = "qwen3-coder-plus"
# type = "qwen-code"

# Port to listen on (default: 48017)
# Can also be set via PORT env var
#port = 48017

# Enable verbose logging (logs full request/response bodies)
#verbose = false

# Log an estimated token count for each request (default: false)
#enable_token_stats = false

# ---- Web search ----------------------------------------------------------
# Offer a web-search tool to agents. Only active when the search model is a
# klava-type model (the search backend is reachable only through the klava
# gateway) and klava is signed in (`klava login`) or KLAVA_API_KEY is set.
#enable_websearch = true

# Model used to run the search. Defaults to the completion model. The search
# runs on a klava-type entry with that name (a mixed pool still uses its klava
# entry, because the backend is only reachable through the gateway).
#search_model = ""

# Search backend family: "auto", "web_fast", "web_deep", "web_native" or
# "yandex" (Yandex Search via the klava gateway).
#search_engine = "auto"

# How much page context to pull into the answer: "low", "medium" or "high".
#search_context = "medium"

# Upper bound on results the backend may consider (default: 5).
#search_max_results = 5

# Instruction prepended to the generated search query.
#search_prompt = ""

# Restrict the search to these domains (empty = no restriction).
#search_include_domains = []

# Exclude these domains from the search (empty = no restriction).
#search_exclude_domains = []

# Let a decision model tune the search settings for each query (default: true).
#search_decide = true

# ---- Decision model ------------------------------------------------------
# One decide model (jigor / System One) serves every classifier call klava
# makes. It must name exactly one [[models]] entry; the jigor backend
# (klava / openrouter / cloudflare) is detected from that entry's type and
# base URL. Without an entry, the klava gateway is used with the saved login.
#decide_model = "typesafe/jev-1.13"

# Require an approved decide before an extension's nested call runs (default:
# true). The extension supplies the question and threshold; klava only checks
# that one was answered "yes". false marks such calls as unverified in the log.
#require_decide_for_nested_calls = true

# ---- Extensions ----------------------------------------------------------
# klava ships with no extensions. Install one with:
#
#   klava extensions install cargo:<crate> --trust
#
# Extensions live in ~/.local/share/klava/extensions (or the equivalent data
# directory) and are discovered at runtime. See EXTENSIONS.md.
#
# [extensions.settings.<name>]
# enabled = true
"#;

        fs::write(&path, template)?;
        Ok(())
    }

    pub fn load() -> Result<Config> {
        let path = Config::get_path();
        let mut cfg: Config = confy::load_path(&path)?;
        let migrated = cfg.apply_legacy_aliases();
        if migrated && path.exists() {
            // Keep the pre-migration file around: `[[providers]]` entries are
            // rewritten as `[[models]]` and cannot be recovered by hand.
            let backup = path.with_extension("toml.bak");
            let _ = fs::copy(&path, &backup);
            if let Err(e) = cfg.save() {
                tracing::warn!("could not write migrated config: {e}");
            }
        }
        Ok(cfg)
    }

    /// Normalise a loaded config in place.
    ///
    /// Translates deprecated settings and migrates the pre-`[[models]]`
    /// `[[providers]]` + `active_provider` layout into self-contained model
    /// entries. Returns `true` when the config should be written back.
    pub fn apply_legacy_aliases(&mut self) -> bool {
        let mut changed = false;

        if !self.providers.is_empty() {
            self.migrate_providers();
            changed = true;
        }

        // A malformed routing value must not make every request fail.
        if Routing::parse(&self.routing).is_none() {
            tracing::warn!(
                "unknown routing '{}'; falling back to shuffle",
                self.routing
            );
            self.routing = default_routing();
            changed = true;
        }

        changed
    }

    /// Turn legacy `[[providers]]` entries into `[[models]]` entries.
    ///
    /// Every distinct model id a provider named (completion and reasoning)
    /// becomes one self-contained entry with that provider's connection
    /// settings. The active provider supplies the top-level defaults.
    fn migrate_providers(&mut self) {
        let active_name = self.active_provider.clone();
        let providers = std::mem::take(&mut self.providers);

        let active_models: Option<(Option<String>, Option<String>)> = providers
            .iter()
            .find(|p| p.name == active_name)
            .map(|p| (p.completion_model(), p.reasoning_model()));

        for provider in &providers {
            let mut ids: Vec<String> = Vec::new();
            for id in [provider.completion_model(), provider.reasoning_model()]
                .into_iter()
                .flatten()
            {
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
            // A provider with no model ids is not routable; keep it under its
            // provider name so its connection settings survive the migration.
            if ids.is_empty() {
                ids.push(provider.name.clone());
            }

            for id in ids {
                let entry = Model {
                    name: id,
                    provider_type: provider.provider_type.clone(),
                    base_url: provider.base_url.clone(),
                    chat_completions_path: provider.chat_completions_path.clone(),
                    api_key: provider.api_key.clone(),
                    api_key_name: provider.api_key_name.clone(),
                    ..Model::default()
                };
                // A provider configured with several ids maps to several
                // entries; upstream ids stay as written.
                if !self
                    .models
                    .iter()
                    .any(|m| m.name == entry.name && m.base_url == entry.base_url)
                {
                    self.models.push(entry);
                }
            }
        }

        if let Some((completion, reasoning)) = active_models {
            self.completion_model = self.completion_model.take().or(completion);
            self.reasoning_model = self.reasoning_model.take().or(reasoning);
        }
        if self.reasoning_model.is_none() {
            self.reasoning_model = self.completion_model.clone();
        }
        self.active_provider = String::new();

        if !providers.is_empty() {
            tracing::info!(
                "migrated {} provider(s) into {} model entr{}",
                providers.len(),
                self.models.len(),
                if self.models.len() == 1 { "y" } else { "ies" }
            );
        }
    }

    /// Every entry named `name`, in declaration order (the pool).
    pub fn pool(&self, name: &str) -> Vec<&Model> {
        self.models.iter().filter(|m| m.name == name).collect()
    }

    /// The routing strategy in effect.
    pub fn routing_strategy(&self) -> Routing {
        Routing::parse(&self.routing).unwrap_or_default()
    }

    /// Pick one entry from the pool named `name` according to `routing`.
    pub fn pick(&self, name: &str) -> Option<&Model> {
        let pool = self.pool(name);
        match pool.len() {
            0 => None,
            1 => Some(pool[0]),
            len => {
                let index = match self.routing_strategy() {
                    Routing::First => 0,
                    Routing::RoundRobin => ROUND_ROBIN.fetch_add(1, Ordering::Relaxed) % len,
                    Routing::Shuffle => {
                        // uuid v4 gives cheap per-process entropy without a
                        // dedicated RNG dependency.
                        (uuid::Uuid::new_v4().as_u128() as usize) % len
                    }
                };
                Some(pool[index])
            }
        }
    }

    /// Model used for ordinary requests.
    pub fn default_completion(&self) -> Option<&Model> {
        self.completion_model
            .as_deref()
            .and_then(|name| self.pick(name))
            .or_else(|| self.models.first())
    }

    /// Model used for reasoning requests; falls back to the completion one.
    pub fn default_reasoning(&self) -> Option<&Model> {
        self.reasoning_model
            .as_deref()
            .and_then(|name| self.pick(name))
            .or_else(|| self.default_completion())
    }

    /// Resolve the model for one client request.
    ///
    /// A model name the client sent that matches a `[[models]]` entry routes
    /// to that pool; anything else (unknown name, empty, the placeholder ids
    /// some harnesses send) falls back to the configured defaults.
    ///
    /// Claude Code appends a `[1m]` tag to request the 1M-context variant
    /// (`moonshotai/kimi-k3.1-pro[1m]`). The tag is not part of the model id,
    /// so it is stripped before matching — otherwise the request silently
    /// falls back to the default model instead of the one the user picked.
    pub fn resolve_request_model(&self, requested: &str, is_reasoning: bool) -> Option<&Model> {
        let requested = strip_context_suffix(requested.trim());
        if !requested.is_empty()
            && let Some(model) = self.pick(requested)
        {
            return Some(model);
        }
        if is_reasoning {
            self.default_reasoning()
        } else {
            self.default_completion()
        }
    }

    /// The model behind `name` (pool routing applies). Used by nested calls
    /// and extensions that name a model.
    pub fn resolve_model(&self, name: &str) -> Option<&Model> {
        self.pick(strip_context_suffix(name.trim()))
    }

    /// Distinct model names, in declaration order — what harnesses see.
    pub fn model_names(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for model in &self.models {
            if !names.contains(&model.name) {
                names.push(model.name.clone());
            }
        }
        names
    }

    /// The decide model entry. `None` means "use the klava gateway with the
    /// saved login" (the zero-config default); an ambiguous name yields
    /// `Err` because a classifier must be deterministic.
    pub fn decide_entry(&self) -> Result<Option<&Model>> {
        let name = self.resolve_decide_model();
        let pool: Vec<&Model> = self.models.iter().filter(|m| m.name == name).collect();
        match pool.len() {
            0 => Ok(None),
            1 => Ok(Some(pool[0])),
            n => Err(Error::AmbiguousModel(name, n)),
        }
    }

    /// Resolve the active completion model for the default entry.
    pub fn resolve_completion_model(&self) -> Option<String> {
        self.default_completion().map(|m| m.name.clone())
    }

    /// Resolve the active reasoning model for the default entry.
    pub fn resolve_reasoning_model(&self) -> Option<String> {
        self.default_reasoning().map(|m| m.name.clone())
    }

    /// Base URL of the default completion model.
    pub fn resolve_base_url(&self) -> Option<String> {
        self.default_completion()?.resolve_base_url()
    }

    /// API key of the default completion model.
    pub fn resolve_api_key(&self) -> Option<String> {
        self.default_completion()?.api_key()
    }

    pub fn save(&self) -> Result<()> {
        let path = Config::get_path();
        confy::store_path(path, self)?;
        Ok(())
    }

    /// Partial update of config (only writes provided fields)
    pub fn update<F>(mut self, update_fn: F) -> Result<Self>
    where
        F: FnOnce(&mut Self),
    {
        update_fn(&mut self);
        self.save()?;
        Ok(self)
    }

    /// Validate base URL format
    pub fn validate_base_url(&self, base_url: &str) -> Result<()> {
        if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
            return Err(Error::InvalidBaseUrl(base_url.to_string()));
        }
        Ok(())
    }

    /// Validate configuration is complete for running
    pub fn validate_complete(&self) -> Result<()> {
        if self.models.is_empty() {
            return Err(Error::NoModelsConfigured);
        }

        for name in [&self.completion_model, &self.reasoning_model]
            .into_iter()
            .flatten()
        {
            if self.pool(name).is_empty() {
                return Err(Error::MissingModel(name.clone()));
            }
        }

        // Every entry that can actually be picked must be usable, so a broken
        // pool member surfaces at startup instead of on some random request.
        let mut checked: Vec<String> = Vec::new();
        for name in [
            self.resolve_completion_model(),
            self.resolve_reasoning_model(),
            self.resolve_search_model(),
        ]
        .into_iter()
        .flatten()
        {
            if checked.contains(&name) {
                continue;
            }
            let pool = self.pool(&name);
            if pool.is_empty() {
                return Err(Error::MissingModel(name));
            }
            for entry in pool {
                entry.validate_config(self)?;
            }
            checked.push(name);
        }

        // The decide model must be unambiguous when the config names one of
        // the configured entries.
        self.decide_entry()?;

        // Effort values are free-form strings in TOML; catch a typo here
        // rather than silently ignoring it on every request.
        crate::effort::validate(self)?;

        Ok(())
    }

    /// Upstream chat completions URL for the default model.
    pub fn chat_completions_url(&self) -> String {
        self.default_completion()
            .and_then(|m| m.chat_completions_url())
            .expect("Base URL not configured for the default model")
    }

    /// Model used for web-search requests: the explicit `search_model` when
    /// set, otherwise the completion model.
    ///
    /// A name that is not configured is returned as-is (so the caller reports
    /// `MissingModel` instead of silently searching through some other model).
    pub fn resolve_search_model(&self) -> Option<String> {
        if let Some(search) = self.search_model.as_ref().filter(|m| !m.is_empty()) {
            return Some(search.clone());
        }
        if let Some(completion) = self.completion_model.as_ref().filter(|m| !m.is_empty()) {
            return Some(completion.clone());
        }
        self.models.first().map(|m| m.name.clone())
    }

    /// The decide model used by every classifier call (search tuning and the
    /// nested-call gate). Empty falls back to the built-in default.
    pub fn resolve_decide_model(&self) -> String {
        let chosen = self.decide_model.trim();
        if chosen.is_empty() {
            crate::decision::DEFAULT_DECIDE_MODEL.to_string()
        } else {
            chosen.to_string()
        }
    }

    /// Whether web search should be active for this setup: enabled in config,
    /// the search model is served by the klava gateway, and klava is
    /// authenticated (saved `klava login` session or an explicit key).
    /// The search backend is only reachable through the klava gateway, so the
    /// feature stays off for every other model.
    pub fn websearch_available(&self) -> bool {
        self.enable_websearch && self.search_entry().is_some()
    }

    /// The connection a web search runs on: a `type = "klava"` entry from the
    /// search model's pool that has credentials.
    ///
    /// Pools may mix endpoints (the same model name on klava and OpenRouter),
    /// but the search backend is a klava gateway feature, so the search does
    /// not participate in `routing` — it always uses a klava entry.
    pub fn search_entry(&self) -> Option<&Model> {
        let name = self.resolve_search_model()?;
        self.pool(&name)
            .into_iter()
            .find(|m| m.is_klava() && m.has_klava_auth())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{JigorBackend, Type as ProviderType};

    fn model(name: &str, base_url: &str) -> Model {
        Model {
            name: name.to_string(),
            provider_type: ProviderType::OpenAICompatible,
            base_url: Some(base_url.to_string()),
            ..Model::default()
        }
    }

    #[test]
    fn test_config_default_creates_empty_models() {
        let config = Config::default();
        assert!(config.models.is_empty());
        assert!(config.completion_model.is_none());
        assert_eq!(config.routing_strategy(), Routing::Shuffle);
    }

    #[test]
    fn the_decide_model_is_shared_and_the_nested_call_proof_is_on_by_default() {
        let config = Config::default();
        assert!(config.require_decide_for_nested_calls);
        assert_eq!(
            config.resolve_decide_model(),
            crate::decision::DEFAULT_DECIDE_MODEL
        );
        // No configured entry: the decide model falls back to the gateway.
        assert!(config.decide_entry().unwrap().is_none());
    }

    #[test]
    fn decide_model_is_universal_and_falls_back_to_the_default() {
        let mut config = Config {
            decide_model: "custom/decider".to_string(),
            ..Config::default()
        };
        assert_eq!(config.resolve_decide_model(), "custom/decider");

        config.decide_model = "  ".to_string();
        assert_eq!(
            config.resolve_decide_model(),
            crate::decision::DEFAULT_DECIDE_MODEL
        );
    }

    #[test]
    fn legacy_search_decide_model_is_accepted_as_an_alias() {
        let config: Config = toml::from_str(
            r#"
port = 48017
search_decide_model = "legacy/decider"
"#,
        )
        .expect("legacy config parses");
        assert_eq!(config.decide_model, "legacy/decider");
        assert_eq!(config.resolve_decide_model(), "legacy/decider");
    }

    #[test]
    fn decide_model_entry_is_found_by_its_model_name() {
        let mut config = Config {
            models: vec![
                model("chat", "https://openrouter.ai/api"),
                model("typesafe/jev-1.13", "https://openrouter.ai/api"),
            ],
            ..Config::default()
        };
        config.decide_model = "typesafe/jev-1.13".to_string();
        let entry = config.decide_entry().unwrap().unwrap();
        assert_eq!(entry.jigor_backend(), Some(JigorBackend::Openrouter));
    }

    #[test]
    fn an_ambiguous_decide_model_is_an_error() {
        let mut config = Config {
            models: vec![
                model("decider", "https://openrouter.ai/api"),
                model(
                    "decider",
                    "https://api.cloudflare.com/client/v4/accounts/a/ai/v1",
                ),
            ],
            ..Config::default()
        };
        config.decide_model = "decider".to_string();
        assert!(matches!(
            config.decide_entry(),
            Err(Error::AmbiguousModel(_, 2))
        ));
    }

    #[test]
    fn request_model_wins_over_the_default() {
        let config = Config {
            completion_model: Some("default".to_string()),
            models: vec![
                model("default", "https://a.example"),
                model("special", "https://b.example"),
            ],
            ..Config::default()
        };
        assert_eq!(
            config
                .resolve_request_model("special", false)
                .unwrap()
                .base_url
                .as_deref(),
            Some("https://b.example")
        );
    }

    #[test]
    fn context_suffix_is_stripped_before_matching() {
        let config = Config {
            completion_model: Some("chat".to_string()),
            reasoning_model: Some("think".to_string()),
            models: vec![
                model("chat", "https://chat.example"),
                model("think", "https://think.example"),
            ],
            ..Config::default()
        };
        // Claude Code appends `[1m]` for 1M-context requests. The tag must not
        // defeat routing: without stripping, this falls back to `chat`.
        assert_eq!(
            config
                .resolve_request_model("think[1m]", false)
                .unwrap()
                .name,
            "think"
        );
        assert_eq!(
            config.resolve_request_model("chat[1m]", true).unwrap().name,
            "chat"
        );
        assert_eq!(config.resolve_model("think[1m]").unwrap().name, "think");
    }

    #[test]
    fn unknown_request_model_falls_back_to_the_active_defaults() {
        let config = Config {
            completion_model: Some("chat".to_string()),
            reasoning_model: Some("think".to_string()),
            models: vec![
                model("chat", "https://chat.example"),
                model("think", "https://think.example"),
            ],
            ..Config::default()
        };
        assert_eq!(
            config.resolve_request_model("klava", false).unwrap().name,
            "chat"
        );
        assert_eq!(
            config
                .resolve_request_model("claude-opus-5", true)
                .unwrap()
                .name,
            "think"
        );
        assert_eq!(
            config.resolve_request_model("", false).unwrap().name,
            "chat"
        );
    }

    #[test]
    fn reasoning_defaults_to_the_completion_model() {
        let config = Config {
            completion_model: Some("chat".to_string()),
            models: vec![model("chat", "https://chat.example")],
            ..Config::default()
        };
        assert_eq!(config.default_reasoning().unwrap().name, "chat");
    }

    #[test]
    fn routing_first_picks_the_first_entry_of_a_pool() {
        let config = Config {
            routing: "first".to_string(),
            models: vec![
                model("dup", "https://one.example"),
                model("dup", "https://two.example"),
            ],
            ..Config::default()
        };
        for _ in 0..5 {
            assert_eq!(
                config.pick("dup").unwrap().base_url.as_deref(),
                Some("https://one.example")
            );
        }
    }

    #[test]
    fn routing_round_robin_cycles_through_the_pool() {
        let config = Config {
            routing: "round_robin".to_string(),
            models: vec![
                model("dup", "https://one.example"),
                model("dup", "https://two.example"),
            ],
            ..Config::default()
        };
        let picks: Vec<String> = (0..4)
            .map(|_| config.pick("dup").unwrap().base_url.clone().unwrap())
            .collect();
        assert_eq!(
            picks,
            vec![
                "https://one.example",
                "https://two.example",
                "https://one.example",
                "https://two.example"
            ]
        );
    }

    #[test]
    fn routing_shuffle_stays_inside_the_pool() {
        let config = Config {
            models: vec![
                model("dup", "https://one.example"),
                model("dup", "https://two.example"),
            ],
            ..Config::default()
        };
        for _ in 0..20 {
            let picked = config.pick("dup").unwrap();
            assert!(
                picked.base_url.as_deref() == Some("https://one.example")
                    || picked.base_url.as_deref() == Some("https://two.example")
            );
        }
    }

    #[test]
    fn model_names_are_distinct_and_ordered() {
        let config = Config {
            models: vec![
                model("b", "https://one.example"),
                model("a", "https://two.example"),
                model("b", "https://three.example"),
            ],
            ..Config::default()
        };
        assert_eq!(config.model_names(), vec!["b", "a"]);
    }

    #[test]
    fn chat_completions_url_uses_the_default_model() {
        let config = Config {
            completion_model: Some("immerse".to_string()),
            models: vec![Model {
                chat_completions_path: Some("/v1/endpoints/generate/chat/completions".to_string()),
                ..model("immerse", "https://chat.immers.cloud/")
            }],
            ..Config::default()
        };
        assert_eq!(
            config.chat_completions_url(),
            "https://chat.immers.cloud/v1/endpoints/generate/chat/completions"
        );
    }

    #[test]
    fn validate_rejects_an_empty_config() {
        let err = Config::default().validate_complete().unwrap_err();
        assert!(matches!(err, Error::NoModelsConfigured));
    }

    #[test]
    fn validate_rejects_a_dangling_default_model_name() {
        let config = Config {
            completion_model: Some("missing".to_string()),
            models: vec![model("chat", "https://chat.example")],
            ..Config::default()
        };
        assert!(matches!(
            config.validate_complete(),
            Err(Error::MissingModel(name)) if name == "missing"
        ));
    }

    #[test]
    fn legacy_providers_migrate_into_models() {
        let mut config: Config = toml::from_str(
            r#"
port = 48017
active_provider = "immers"

[[providers]]
name = "immers"
type = "openai-compatible"
base_url = "https://chat.immers.cloud"
api_key_name = "IMMERSE_TOKEN"
reasoning_model = "deepseek-v4-flash"
completion_model = "deepseek-v4-flash"

[[providers]]
name = "openrouter"
type = "openai-compatible"
base_url = "https://openrouter.ai/api"
api_key_name = "OPENROUTER_API_KEY"
completion_model = "glm-5.1"
reasoning_model = "glm-5.1"
"#,
        )
        .unwrap();

        assert!(config.apply_legacy_aliases());
        assert!(config.providers.is_empty());
        assert!(config.active_provider.is_empty());
        assert_eq!(config.model_names(), vec!["deepseek-v4-flash", "glm-5.1"]);
        assert_eq!(
            config.completion_model.as_deref(),
            Some("deepseek-v4-flash")
        );
        assert_eq!(config.reasoning_model.as_deref(), Some("deepseek-v4-flash"));

        let immersive = config.pick("deepseek-v4-flash").unwrap();
        assert_eq!(
            immersive.base_url.as_deref(),
            Some("https://chat.immers.cloud")
        );
        assert_eq!(immersive.api_key_name.as_deref(), Some("IMMERSE_TOKEN"));
        assert!(
            immersive.reasoning_model.is_none(),
            "legacy fields are cleared"
        );
    }

    #[test]
    fn migration_keeps_an_active_providers_distinct_models() {
        let mut config: Config = toml::from_str(
            r#"
port = 48017
active_provider = "p"

[[providers]]
name = "p"
type = "openai-compatible"
base_url = "https://p.example"
api_key = "k"
completion_model = "chat"
reasoning_model = "think"
"#,
        )
        .unwrap();
        config.apply_legacy_aliases();
        assert_eq!(config.model_names(), vec!["chat", "think"]);
        assert_eq!(config.completion_model.as_deref(), Some("chat"));
        assert_eq!(config.reasoning_model.as_deref(), Some("think"));
    }

    #[test]
    fn migrating_twice_is_a_no_op() {
        let mut config = Config {
            models: vec![model("chat", "https://chat.example")],
            completion_model: Some("chat".to_string()),
            ..Config::default()
        };
        assert!(!config.apply_legacy_aliases());
        assert_eq!(config.models.len(), 1);
    }

    #[test]
    fn model_entries_round_trip_through_toml() {
        let config = Config {
            completion_model: Some("~deepseek/deepseek-flash-latest".to_string()),
            reasoning_model: Some("~deepseek/deepseek-flash-latest".to_string()),
            models: vec![Model {
                model: Some("deepseek/deepseek-flash-latest".to_string()),
                ..model(
                    "~deepseek/deepseek-flash-latest",
                    "https://openrouter.ai/api",
                )
            }],
            ..Config::default()
        };
        let text = toml::to_string_pretty(&config).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.model_names(), config.model_names());
        assert_eq!(
            back.pick("~deepseek/deepseek-flash-latest")
                .unwrap()
                .upstream_model(),
            "deepseek/deepseek-flash-latest"
        );
        assert!(
            text.contains("model = \"deepseek/deepseek-flash-latest\""),
            "{text}"
        );
        assert!(!text.contains("upstream_model"), "{text}");
        assert!(!text.contains("providers"), "{text}");
    }

    #[test]
    fn legacy_upstream_model_deserializes_as_model() {
        let config: Config = toml::from_str(
            r#"
port = 48017

[[models]]
name = "friendly"
type = "openai-compatible"
base_url = "https://openrouter.ai/api"
upstream_model = "vendor/real-id"
"#,
        )
        .unwrap();

        let entry = &config.models[0];
        assert_eq!(entry.name, "friendly");
        assert_eq!(entry.model.as_deref(), Some("vendor/real-id"));
        assert_eq!(entry.upstream_model(), "vendor/real-id");

        let text = toml::to_string_pretty(&config).unwrap();
        assert!(text.contains("model = \"vendor/real-id\""), "{text}");
        assert!(!text.contains("upstream_model"), "{text}");
    }

    #[test]
    fn an_omitted_model_falls_back_to_the_public_name() {
        let config: Config = toml::from_str(
            r#"
port = 48017

[[models]]
name = "vendor/real-id"
type = "openai-compatible"
base_url = "https://openrouter.ai/api"
"#,
        )
        .unwrap();

        let entry = &config.models[0];
        assert_eq!(entry.model, None);
        assert_eq!(entry.upstream_model(), "vendor/real-id");
        let text = toml::to_string_pretty(&config).unwrap();
        assert!(
            !text.lines().any(|line| line.starts_with("model = ")),
            "{text}"
        );
    }

    #[test]
    fn a_config_with_models_never_serializes_legacy_keys() {
        let config = Config {
            routing: "round_robin".to_string(),
            models: vec![Model {
                api_key_name: Some("OPENROUTER_API_KEY".to_string()),
                ..model("glm-5.1", "https://openrouter.ai/api")
            }],
            completion_model: Some("glm-5.1".to_string()),
            ..Config::default()
        };
        let text = toml::to_string_pretty(&config).unwrap();
        assert!(text.contains("routing = \"round_robin\""), "{text}");
        assert!(text.contains("[[models]]"), "{text}");
        assert!(!text.contains("active_provider"), "{text}");
        assert!(!text.contains("[[providers]]"), "{text}");
    }

    #[cfg(feature = "klava")]
    fn klava_entry(name: &str) -> Model {
        Model {
            name: name.to_string(),
            provider_type: ProviderType::Klava,
            ..Model::default()
        }
    }

    #[cfg(feature = "klava")]
    fn openai_entry(name: &str) -> Model {
        model(name, "https://test.example.com")
    }

    #[cfg(feature = "klava")]
    #[test]
    fn websearch_uses_a_klava_entry_from_the_pool() {
        // The same name on klava and OpenRouter: the search backend is the
        // klava entry, not whichever `routing` picks for chat.
        let _guard = AUTH_FILE_TEST_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var("KLAVA_API_KEY", "sk-test");
        }
        let mut klava = klava_entry("model");
        klava.api_key_name = Some("KLAVA_API_KEY".to_string());
        let config = Config {
            completion_model: Some("model".to_string()),
            routing: "first".to_string(),
            models: vec![openai_entry("model"), klava],
            enable_websearch: true,
            ..Config::default()
        };
        assert_eq!(config.pool("model").len(), 2);
        // Despite `routing = "first"` landing on the OpenRouter entry…
        assert!(!config.pick("model").unwrap().is_klava());
        // …the search entry is the klava one.
        assert!(config.search_entry().unwrap().is_klava());
        assert!(config.websearch_available());
        unsafe {
            std::env::remove_var("KLAVA_API_KEY");
        }
    }

    #[cfg(feature = "klava")]
    #[test]
    fn websearch_available_requires_a_klava_search_model() {
        let _guard = AUTH_FILE_TEST_LOCK.lock().unwrap();
        let config = Config {
            completion_model: Some("openai".to_string()),
            models: vec![openai_entry("openai")],
            enable_websearch: true,
            ..Config::default()
        };
        // Non-klava active model → no web search, even with auth env set.
        unsafe {
            std::env::set_var("KLAVA_API_KEY", "sk-test");
        }
        assert!(!config.websearch_available());
        unsafe {
            std::env::remove_var("KLAVA_API_KEY");
        }
    }

    #[cfg(feature = "klava")]
    #[test]
    fn websearch_available_requires_enable_flag() {
        let config = Config {
            completion_model: Some("klava".to_string()),
            models: vec![Model {
                api_key: Some("sk-test".to_string()),
                ..klava_entry("klava")
            }],
            enable_websearch: false,
            ..Config::default()
        };
        assert!(!config.websearch_available());
    }

    #[cfg(feature = "klava")]
    #[test]
    fn websearch_available_true_with_explicit_key() {
        let config = Config {
            completion_model: Some("klava".to_string()),
            models: vec![Model {
                api_key: Some("sk-test".to_string()),
                ..klava_entry("klava")
            }],
            enable_websearch: true,
            ..Config::default()
        };
        assert!(config.websearch_available());
    }

    #[cfg(feature = "klava")]
    #[test]
    fn websearch_available_true_with_env_key() {
        let _guard = AUTH_FILE_TEST_LOCK.lock().unwrap();
        let config = Config {
            completion_model: Some("klava".to_string()),
            models: vec![Model {
                api_key_name: Some("KLAVA_API_KEY".to_string()),
                ..klava_entry("klava")
            }],
            enable_websearch: true,
            ..Config::default()
        };
        unsafe {
            std::env::set_var("KLAVA_API_KEY", "sk-env-key");
        }
        assert!(config.websearch_available());
        unsafe {
            std::env::remove_var("KLAVA_API_KEY");
        }
    }

    /// Serializes tests that point `KLAVA_AUTH_FILE` at temp files: the env
    /// var is process-global and cargo runs tests in parallel.
    #[cfg(feature = "klava")]
    static AUTH_FILE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[cfg(feature = "klava")]
    #[test]
    fn websearch_available_false_without_any_auth() {
        let _guard = AUTH_FILE_TEST_LOCK.lock().unwrap();
        // Point the saved-login check at a nonexistent file so the test is
        // hermetic (the dev machine may have a real ~/.local/share login).
        let missing = std::env::temp_dir().join(format!("klava-noauth-{}", std::process::id()));
        unsafe {
            std::env::set_var("KLAVA_AUTH_FILE", &missing);
        }
        let config = Config {
            completion_model: Some("klava".to_string()),
            models: vec![klava_entry("klava")],
            enable_websearch: true,
            ..Config::default()
        };
        // No api_key, no api_key_name, no saved login at the pointed path.
        assert!(!config.websearch_available());
        unsafe {
            std::env::remove_var("KLAVA_AUTH_FILE");
        }
        let _ = std::fs::remove_file(&missing);
    }

    #[cfg(feature = "klava")]
    #[test]
    fn websearch_available_true_with_saved_login() {
        let _guard = AUTH_FILE_TEST_LOCK.lock().unwrap();
        // Simulate a `klava login`: a valid credentials file at the pointed path.
        use std::io::Write;
        let path = std::env::temp_dir().join(format!("klava-auth-{}", std::process::id()));
        let creds = serde_json::json!({
            "api_key": "sk_saved_login",
            "key_preview": "sk_saved…",
            "email": "t@example.com",
            "base_url": "https://klava.zatsepin.dev",
            "created_at": "2026-01-01T00:00:00Z"
        });
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(creds.to_string().as_bytes()).unwrap();
        unsafe {
            std::env::set_var("KLAVA_AUTH_FILE", &path);
        }
        let config = Config {
            completion_model: Some("klava".to_string()),
            models: vec![klava_entry("klava")],
            enable_websearch: true,
            ..Config::default()
        };
        assert!(config.websearch_available());
        unsafe {
            std::env::remove_var("KLAVA_AUTH_FILE");
        }
        let _ = std::fs::remove_file(&path);
    }

    #[cfg(feature = "klava")]
    #[test]
    fn websearch_available_false_when_no_active_model() {
        let config = Config {
            completion_model: Some("missing".to_string()),
            models: vec![klava_entry("klava")],
            enable_websearch: true,
            ..Config::default()
        };
        assert!(!config.websearch_available());
    }

    #[cfg(feature = "klava")]
    #[test]
    fn klava_model_resolves_default_base_url_without_config() {
        let config = Config {
            completion_model: Some("klava".to_string()),
            models: vec![klava_entry("klava")],
            ..Config::default()
        };
        assert_eq!(
            config.chat_completions_url(),
            "https://klava.zatsepin.dev/api/v1/chat/completions"
        );
    }

    #[cfg(feature = "klava")]
    #[test]
    fn klava_model_resolves_custom_base_url_when_configured() {
        let config = Config {
            completion_model: Some("klava".to_string()),
            models: vec![Model {
                base_url: Some("https://dev.example.com/api".to_string()),
                ..klava_entry("klava")
            }],
            ..Config::default()
        };
        assert_eq!(
            config.chat_completions_url(),
            "https://dev.example.com/api/v1/chat/completions"
        );
    }
}
