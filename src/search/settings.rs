//! Search settings: the klava-named knob vocabulary and its mapping onto the
//! search backend fields.
//!
//! Users configure search with klava vocabulary (`web_fast`, `web_deep`,
//! `web_native`, `auto`) and a context size (`low`/`medium`/`high`). This
//! module is the single place that translates those knobs into the concrete
//! fields the search backend understands, and that clamps user/decision
//! values into their legal ranges.

use serde_json::{Value, json};

/// Search engine family, in klava vocabulary.
///
/// The klava names (`web_fast`, `web_deep`, `web_native`) are the user- and
/// decision-facing vocabulary. Each maps onto the backend's own
/// engine-plus-mode pair; the raw engine names are also accepted so a config
/// can select a specific backend directly. [`Engine::Auto`] leaves the choice
/// to the backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    /// Let the backend choose (no engine/mode sent).
    Auto,
    /// Fast, low-latency retrieval (Exa in `fast` mode).
    WebFast,
    /// Deeper multi-step retrieval (Exa in `deep` mode).
    WebDeep,
    /// The model's own native search — only some models support it.
    WebNative,
    /// Exa, backend-chosen mode.
    Exa,
    /// Firecrawl.
    Firecrawl,
    /// Parallel.
    Parallel,
    /// Perplexity.
    Perplexity,
}

impl Engine {
    /// Parse a klava vocabulary name (or a raw backend engine). Unknown values
    /// fall back to [`Engine::Auto`].
    pub fn parse(name: &str) -> Self {
        match name.trim().to_ascii_lowercase().as_str() {
            "web_fast" | "fast" => Engine::WebFast,
            "web_deep" | "deep" => Engine::WebDeep,
            "web_native" | "native" => Engine::WebNative,
            "exa" => Engine::Exa,
            "firecrawl" => Engine::Firecrawl,
            "parallel" => Engine::Parallel,
            "perplexity" => Engine::Perplexity,
            _ => Engine::Auto,
        }
    }

    /// Canonical klava name.
    pub fn as_str(&self) -> &'static str {
        match self {
            Engine::Auto => "auto",
            Engine::WebFast => "web_fast",
            Engine::WebDeep => "web_deep",
            Engine::WebNative => "web_native",
            Engine::Exa => "exa",
            Engine::Firecrawl => "firecrawl",
            Engine::Parallel => "parallel",
            Engine::Perplexity => "perplexity",
        }
    }

    /// The backend engine/mode pair this family selects. `Auto` leaves both
    /// unset so the backend picks.
    pub fn wire(&self) -> (Option<&'static str>, Option<&'static str>) {
        match self {
            Engine::Auto => (None, None),
            Engine::WebFast => (Some("exa"), Some("fast")),
            Engine::WebDeep => (Some("exa"), Some("deep")),
            Engine::WebNative => (Some("native"), None),
            Engine::Exa => (Some("exa"), None),
            Engine::Firecrawl => (Some("firecrawl"), None),
            Engine::Parallel => (Some("parallel"), None),
            Engine::Perplexity => (Some("perplexity"), None),
        }
    }
}

/// Amount of page context pulled into the answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextSize {
    Low,
    Medium,
    High,
}

impl ContextSize {
    /// Parse a size name. Unknown values fall back to [`ContextSize::Medium`].
    pub fn parse(name: &str) -> Self {
        match name.trim().to_ascii_lowercase().as_str() {
            "low" => ContextSize::Low,
            "high" => ContextSize::High,
            _ => ContextSize::Medium,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            ContextSize::Low => "low",
            ContextSize::Medium => "medium",
            ContextSize::High => "high",
        }
    }
}

/// How much retrieval breadth the search should use, in klava vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Breadth {
    Focused,
    Standard,
    Broad,
}

impl Breadth {
    pub fn max_results(&self) -> u32 {
        match self {
            Breadth::Focused => 3,
            Breadth::Standard => 5,
            Breadth::Broad => 10,
        }
    }
}

/// Fully-resolved search settings for one request.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchSettings {
    pub engine: Engine,
    pub context: ContextSize,
    pub max_results: u32,
    pub search_prompt: Option<String>,
    pub include_domains: Vec<String>,
    pub exclude_domains: Vec<String>,
}

impl Default for SearchSettings {
    fn default() -> Self {
        Self {
            engine: Engine::Auto,
            context: ContextSize::Medium,
            max_results: 5,
            search_prompt: None,
            include_domains: vec![],
            exclude_domains: vec![],
        }
    }
}

/// Legal clamp for the result count, mirrored from the backend's own bounds.
pub const MAX_RESULTS_MIN: u32 = 1;
pub const MAX_RESULTS_MAX: u32 = 20;

impl SearchSettings {
    /// Clamp every field into its legal range. Called after the decision model
    /// (and after config load) so nothing out-of-range reaches the wire.
    pub fn clamped(mut self) -> Self {
        self.max_results = self.max_results.clamp(MAX_RESULTS_MIN, MAX_RESULTS_MAX);
        self
    }

    /// The `web_search_options` object (context size).
    pub fn web_search_options(&self) -> Value {
        json!({ "search_context_size": self.context.as_str() })
    }

    /// The search plugin descriptor.
    pub fn plugin(&self) -> Value {
        let (engine, mode) = self.engine.wire();
        let mut plugin = serde_json::Map::new();
        plugin.insert("id".to_string(), json!("web"));
        if let Some(engine) = engine {
            plugin.insert("engine".to_string(), json!(engine));
        }
        if let Some(mode) = mode {
            plugin.insert("mode".to_string(), json!(mode));
        }
        plugin.insert("max_results".to_string(), json!(self.max_results));
        if let Some(prompt) = &self.search_prompt
            && !prompt.is_empty()
        {
            plugin.insert("search_prompt".to_string(), json!(prompt));
        }
        if !self.include_domains.is_empty() {
            plugin.insert("include_domains".to_string(), json!(self.include_domains));
        }
        if !self.exclude_domains.is_empty() {
            plugin.insert("exclude_domains".to_string(), json!(self.exclude_domains));
        }
        Value::Object(plugin)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_parses_klava_vocabulary_and_aliases() {
        assert_eq!(Engine::parse("web_fast"), Engine::WebFast);
        assert_eq!(Engine::parse("WEB_DEEP"), Engine::WebDeep);
        assert_eq!(Engine::parse("native"), Engine::WebNative);
        assert_eq!(Engine::parse("exa"), Engine::Exa);
        assert_eq!(Engine::parse("auto"), Engine::Auto);
        assert_eq!(Engine::parse("nonsense"), Engine::Auto);
    }

    #[test]
    fn engine_round_trips_names() {
        for e in [
            Engine::Auto,
            Engine::WebFast,
            Engine::WebDeep,
            Engine::WebNative,
            Engine::Exa,
            Engine::Firecrawl,
            Engine::Parallel,
            Engine::Perplexity,
        ] {
            assert_eq!(Engine::parse(e.as_str()), e);
        }
    }

    #[test]
    fn engine_wire_maps_families() {
        assert_eq!(Engine::Auto.wire(), (None, None));
        assert_eq!(Engine::WebFast.wire(), (Some("exa"), Some("fast")));
        assert_eq!(Engine::WebDeep.wire(), (Some("exa"), Some("deep")));
        assert_eq!(Engine::WebNative.wire(), (Some("native"), None));
        assert_eq!(Engine::Exa.wire(), (Some("exa"), None));
        assert_eq!(Engine::Parallel.wire(), (Some("parallel"), None));
    }

    #[test]
    fn context_parses_and_defaults_to_medium() {
        assert_eq!(ContextSize::parse("low"), ContextSize::Low);
        assert_eq!(ContextSize::parse("HIGH"), ContextSize::High);
        assert_eq!(ContextSize::parse("huge"), ContextSize::Medium);
    }

    #[test]
    fn breadth_scales_result_count() {
        assert_eq!(Breadth::Focused.max_results(), 3);
        assert_eq!(Breadth::Standard.max_results(), 5);
        assert_eq!(Breadth::Broad.max_results(), 10);
    }

    #[test]
    fn clamped_enforces_result_bounds() {
        let lo = SearchSettings {
            max_results: 0,
            ..SearchSettings::default()
        }
        .clamped();
        assert_eq!(lo.max_results, MAX_RESULTS_MIN);

        let hi = SearchSettings {
            max_results: 999,
            ..SearchSettings::default()
        }
        .clamped();
        assert_eq!(hi.max_results, MAX_RESULTS_MAX);
    }

    #[test]
    fn plugin_carries_full_surface() {
        let s = SearchSettings {
            engine: Engine::WebDeep,
            context: ContextSize::High,
            max_results: 7,
            search_prompt: Some("prefer primary sources".to_string()),
            include_domains: vec!["docs.rs".to_string()],
            exclude_domains: vec!["example.com".to_string()],
        };
        let p = s.plugin();
        assert_eq!(p["id"], json!("web"));
        assert_eq!(p["engine"], json!("exa"));
        assert_eq!(p["mode"], json!("deep"));
        assert_eq!(p["max_results"], json!(7));
        assert_eq!(p["search_prompt"], json!("prefer primary sources"));
        assert_eq!(p["include_domains"], json!(["docs.rs"]));
        assert_eq!(p["exclude_domains"], json!(["example.com"]));
        assert_eq!(
            s.web_search_options(),
            json!({"search_context_size": "high"})
        );
    }

    #[test]
    fn plugin_omits_auto_engine_and_empty_lists() {
        let p = SearchSettings::default().plugin();
        assert!(p.get("engine").is_none());
        assert!(p.get("mode").is_none());
        assert!(p.get("search_prompt").is_none());
        assert!(p.get("include_domains").is_none());
        assert!(p.get("exclude_domains").is_none());
        assert_eq!(p["max_results"], json!(5));
    }
}
