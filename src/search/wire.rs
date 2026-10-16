//! Wire helpers for the search step: building the search request and parsing
//! the backend response.
//!
//! The search request is an ordinary chat-completions request that carries the
//! resolved [`SearchSettings`] as a search plugin and a context-size option.
//! Both are passed through the unmodelled `extra` field of the typed request,
//! so nothing here is provider-specific at the type level.
//!
//! Responses are read as `serde_json::Value` because the typed response model
//! does not carry source annotations.

use crate::models::openai;
use crate::search::settings::SearchSettings;
use serde_json::{Value, json};

/// One cited source extracted from a search response.
#[derive(Debug, Clone, PartialEq)]
pub struct Source {
    pub url: String,
    pub title: Option<String>,
}

/// The rendered outcome of a search: the model's answer text plus its sources.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchOutcome {
    /// Answer text (may be empty when the backend returned only citations).
    pub content: String,
    /// Cited sources, de-duplicated by URL, in first-seen order.
    pub sources: Vec<Source>,
}

impl SearchOutcome {
    /// Render the tool result the agent sees: the answer, then a `Sources:`
    /// list. Falls back to a clear "no result" line when the backend returned
    /// nothing usable.
    pub fn render(&self) -> String {
        let mut out = String::new();
        if !self.content.trim().is_empty() {
            out.push_str(self.content.trim());
        }
        if !self.sources.is_empty() {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str("Sources:\n");
            for s in &self.sources {
                match &s.title {
                    Some(title) if !title.trim().is_empty() => {
                        out.push_str(&format!("- {} ({})\n", title.trim(), s.url));
                    }
                    _ => out.push_str(&format!("- {}\n", s.url)),
                }
            }
        }
        if out.trim().is_empty() {
            out.push_str("No results found.");
        }
        out
    }
}

/// Build the search request for `query` with `settings`, targeting `model`.
///
/// `extra` carries the search plugin and context-size option; both serialize
/// as top-level request fields because `OpenAIRequest.extra` is flattened.
pub fn build_request(query: &str, model: &str, settings: &SearchSettings) -> openai::OpenAIRequest {
    let mut extra = serde_json::Map::new();
    extra.insert("plugins".to_string(), json!([settings.plugin()]));
    extra.insert(
        "web_search_options".to_string(),
        settings.web_search_options(),
    );

    openai::OpenAIRequest {
        model: model.to_string(),
        messages: vec![openai::Message {
            role: "user".to_string(),
            content: Some(openai::MessageContent::Text(query.to_string())),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }],
        max_tokens: None,
        temperature: None,
        top_p: None,
        stop: None,
        stream: Some(false),
        tools: None,
        tool_choice: None,
        reasoning_effort: None,
        extra: Value::Object(extra),
    }
}

/// Parse a search response: the answer text plus the `url_citation`
/// annotations, de-duplicated by URL.
pub fn parse_response(resp: &Value) -> SearchOutcome {
    let message = resp
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
        .and_then(|c| c.get("message"));

    let content = message
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let mut sources: Vec<Source> = Vec::new();
    if let Some(annotations) = message
        .and_then(|m| m.get("annotations"))
        .and_then(Value::as_array)
    {
        for ann in annotations {
            let url = ann
                .get("url_citation")
                .and_then(|c| c.get("url"))
                .and_then(Value::as_str)
                .or_else(|| ann.get("url").and_then(Value::as_str));
            let Some(url) = url else { continue };
            if sources.iter().any(|s| s.url == url) {
                continue;
            }
            let title = ann
                .get("url_citation")
                .and_then(|c| c.get("title"))
                .and_then(Value::as_str)
                .or_else(|| ann.get("title").and_then(Value::as_str))
                .map(|t| t.to_string());
            sources.push(Source {
                url: url.to_string(),
                title,
            });
        }
    }

    SearchOutcome { content, sources }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_request_carries_plugin_and_context() {
        let settings = SearchSettings {
            max_results: 4,
            ..SearchSettings::default()
        };
        let req = build_request("latest rust release", "search-model", &settings);
        let out = serde_json::to_value(&req).unwrap();

        assert_eq!(out["model"], json!("search-model"));
        assert_eq!(out["stream"], json!(false));
        assert_eq!(out["messages"][0]["role"], json!("user"));
        assert_eq!(out["messages"][0]["content"], json!("latest rust release"));
        assert_eq!(out["plugins"][0]["id"], json!("web"));
        assert_eq!(out["plugins"][0]["max_results"], json!(4));
        assert_eq!(
            out["web_search_options"]["search_context_size"],
            json!("medium")
        );
    }

    #[test]
    fn parse_response_extracts_content_and_dedupes_sources() {
        let resp = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "Rust 1.90 is out.",
                    "annotations": [
                        {"type": "url_citation", "url_citation": {"url": "https://a.example", "title": "A"}},
                        {"type": "url_citation", "url_citation": {"url": "https://b.example"}},
                        {"type": "url_citation", "url_citation": {"url": "https://a.example", "title": "A again"}}
                    ]
                }
            }]
        });

        let outcome = parse_response(&resp);
        assert_eq!(outcome.content, "Rust 1.90 is out.");
        assert_eq!(outcome.sources.len(), 2);
        assert_eq!(outcome.sources[0].url, "https://a.example");
        assert_eq!(outcome.sources[0].title.as_deref(), Some("A"));
        assert_eq!(outcome.sources[1].url, "https://b.example");
        assert_eq!(outcome.sources[1].title, None);
    }

    #[test]
    fn parse_response_tolerates_missing_message() {
        let outcome = parse_response(&json!({"choices": []}));
        assert_eq!(
            outcome,
            SearchOutcome {
                content: String::new(),
                sources: vec![]
            }
        );
    }

    #[test]
    fn render_includes_sources_block() {
        let outcome = SearchOutcome {
            content: "Answer text.".to_string(),
            sources: vec![
                Source {
                    url: "https://a.example".to_string(),
                    title: Some("A".to_string()),
                },
                Source {
                    url: "https://b.example".to_string(),
                    title: None,
                },
            ],
        };
        let rendered = outcome.render();
        assert!(rendered.starts_with("Answer text."));
        assert!(rendered.contains("Sources:"));
        assert!(rendered.contains("- A (https://a.example)"));
        assert!(rendered.contains("- https://b.example"));
    }

    #[test]
    fn render_falls_back_when_empty() {
        let outcome = parse_response(&json!({"choices": []}));
        assert_eq!(outcome.render(), "No results found.");
    }

    #[test]
    fn render_content_only_without_sources() {
        let outcome = SearchOutcome {
            content: "just text".to_string(),
            sources: vec![],
        };
        assert_eq!(outcome.render(), "just text");
    }
}
