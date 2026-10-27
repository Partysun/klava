//! `klava providers` — the onboarding wizard.
//!
//! Every routable model is a self-contained `[[models]]` entry. This module
//! knows the big providers (Klava, OpenRouter, Cloudflare Workers AI, Qwen
//! Code, anything OpenAI-compatible), checks the connection and turns the
//! models the user picks into config entries.
//!
//! The interactive parts are thin wrappers around pure helpers
//! ([`entry`], [`merge`], [`decide_candidates`]) so the config-writing logic
//! is testable without a terminal.

use crate::config::{Config, Routing};
use crate::providers::{Model, Type};
use anyhow::{Context, anyhow};
use inquire::{Confirm, MultiSelect, Select, Text};
use std::time::Duration;

/// One entry in the provider menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderKind {
    /// Stable id used by `klava providers add <id>`.
    pub id: &'static str,
    /// Menu label.
    pub label: &'static str,
    /// One-line explanation.
    pub hint: &'static str,
    /// Whether the provider needs an API key (env var or pasted).
    pub needs_key: bool,
}

/// Providers the wizard knows about, in menu order.
pub const KINDS: &[ProviderKind] = &[
    ProviderKind {
        id: "klava",
        label: "Klava (recommended)",
        hint: "klava.zatsepin.dev — browser login, no key to manage",
        needs_key: false,
    },
    ProviderKind {
        id: "openrouter",
        label: "OpenRouter",
        hint: "openrouter.ai — one key, hundreds of models",
        needs_key: true,
    },
    ProviderKind {
        id: "cloudflare",
        label: "Cloudflare Workers AI",
        hint: "api.cloudflare.com — account id + API token",
        needs_key: true,
    },
    ProviderKind {
        id: "qwen-code",
        label: "Qwen Code",
        hint: "OAuth login; requires a qwen-code plan",
        needs_key: false,
    },
    ProviderKind {
        id: "custom",
        label: "Custom OpenAI-compatible",
        hint: "any base URL + key (DeepSeek, Ollama, self-hosted, ...)",
        needs_key: true,
    },
];

/// Every known provider kind.
pub fn providers() -> &'static [ProviderKind] {
    KINDS
}

/// Look up a provider kind by id (accepts a few aliases).
pub fn kind(id: &str) -> Option<&'static ProviderKind> {
    let id = id.trim().to_ascii_lowercase();
    let id = match id.as_str() {
        "cf" | "workers-ai" | "workers" => "cloudflare",
        "open-router" | "or" => "openrouter",
        "qwen" => "qwen-code",
        "openai" | "openai-compatible" => "custom",
        other => other,
    };
    KINDS.iter().find(|k| k.id == id)
}

/// A model offered by a provider catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogModel {
    /// Upstream model id.
    pub id: String,
    /// Human-readable label (falls back to the id).
    pub label: String,
}

/// Build the `[[models]]` entry for one picked model.
///
/// `base_url` is the chat base (the wizard stores it account-scoped for
/// Cloudflare, e.g. `…/accounts/<id>/ai/v1`); `key` is stored either inline or
/// as an env var name.
pub fn entry(
    kind: &ProviderKind,
    upstream_id: &str,
    base_url: Option<&str>,
    key: Option<&str>,
    key_env: Option<&str>,
) -> Model {
    // `Type::parse` knows which types this build supports: a build without
    // the klava/qwen features writes a plain OpenAI-compatible entry (the
    // wizard never offers those providers there).
    let provider_type = Type::parse(kind.id).unwrap_or(Type::OpenAICompatible);
    Model {
        name: public_name(upstream_id),
        provider_type,
        base_url: base_url.map(str::to_string),
        chat_completions_path: None,
        api_key: key.map(str::to_string),
        api_key_name: key_env.map(str::to_string),
        model: None,
        jigor: None,
        reasoning_effort: None,
        reasoning_effort_cap: None,
        reasoning_model: None,
        completion_model: None,
    }
}

/// The public model id written into config.
///
/// Klava's catalog ids carry a `~` prefix that marks models served through the
/// gateway's OpenRouter-compatible route; it is kept so the name matches what
/// the gateway accepts.
pub fn public_name(upstream_id: &str) -> String {
    upstream_id.trim().to_string()
}

/// Add or replace entries, keeping other providers untouched.
///
/// An entry is replaced when its name, type, base URL and key reference all
/// match — re-running the wizard for the same provider updates it instead of
/// duplicating it. The same model on a *different* endpoint (another provider,
/// or a second key on the same one) becomes a second pool entry.
pub fn merge(config: &mut Config, entries: Vec<Model>) {
    for entry in entries {
        if let Some(existing) = config.models.iter_mut().find(|m| {
            m.name == entry.name
                && m.provider_type == entry.provider_type
                && m.base_url == entry.base_url
                && m.api_key_name == entry.api_key_name
                && m.api_key == entry.api_key
        }) {
            existing.model = entry.model;
            continue;
        }
        config.models.push(entry);
    }
}

/// Model names the decide picker may offer: entries whose backend can answer
/// a jigor call.
pub fn decide_candidates(config: &Config) -> Vec<String> {
    config
        .model_names()
        .into_iter()
        .filter(|name| config.pick(name).and_then(|m| m.jigor_backend()).is_some())
        .collect()
}

/// Set the active defaults, ignoring names that are not configured.
pub fn set_defaults(config: &mut Config, completion: Option<&str>, reasoning: Option<&str>) {
    let known: Vec<String> = config.model_names();
    if let Some(name) = completion.filter(|n| known.iter().any(|k| k == n)) {
        config.completion_model = Some(name.to_string());
        // Reasoning follows the completion model unless it is set separately;
        // a stale reasoning pointer would be worse than none.
        if config
            .reasoning_model
            .as_deref()
            .is_none_or(|r| !known.iter().any(|k| k == r))
        {
            config.reasoning_model = Some(name.to_string());
        }
    }
    if let Some(name) = reasoning.filter(|n| known.iter().any(|k| k == n)) {
        config.reasoning_model = Some(name.to_string());
    }
}

// ===== Network: catalogs and connection checks =====

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .unwrap_or_default()
}

/// Fetch the provider's model catalog, when it has one.
///
/// Returns `Ok(None)` when the provider simply has no listing endpoint (the
/// caller then asks for model ids by hand).
pub async fn fetch_catalog(
    kind: &ProviderKind,
    base_url: Option<&str>,
    key: Option<&str>,
) -> Result<Option<Vec<CatalogModel>>, String> {
    match kind.id {
        "klava" => {
            #[cfg(feature = "klava")]
            {
                let mut auth = crate::klava_auth::KlavaAuth::new();
                let models = auth
                    .models()
                    .await
                    .map_err(|e| format!("could not list klava models: {e}"))?;
                let mut out: Vec<CatalogModel> = models
                    .into_iter()
                    .filter(|m| m.enabled)
                    .map(|m| CatalogModel {
                        id: public_name(&m.chat_id()),
                        label: if m.label.is_empty() {
                            m.chat_id()
                        } else {
                            m.label
                        },
                    })
                    .collect();
                out.sort_by(|a, b| a.id.cmp(&b.id));
                Ok(Some(out))
            }
            #[cfg(not(feature = "klava"))]
            {
                Err("this build has no klava support".to_string())
            }
        }
        "openrouter" => {
            let base = base_url.unwrap_or("https://openrouter.ai/api");
            let url = format!("{}/v1/models", base.trim_end_matches('/'));
            let mut request = http_client().get(&url);
            if let Some(key) = key {
                request = request.bearer_auth(key);
            }
            let body: serde_json::Value = request
                .send()
                .await
                .map_err(|e| format!("could not reach {url}: {e}"))?
                .json()
                .await
                .map_err(|e| format!("unexpected catalog response from {url}: {e}"))?;
            let mut out: Vec<CatalogModel> = body
                .get("data")
                .and_then(|d| d.as_array())
                .map(|models| {
                    models
                        .iter()
                        .filter_map(|m| {
                            let id = m.get("id").and_then(|v| v.as_str())?;
                            Some(CatalogModel {
                                id: id.to_string(),
                                label: m
                                    .get("name")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or(id)
                                    .to_string(),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            out.sort_by(|a, b| a.id.cmp(&b.id));
            Ok(Some(out))
        }
        "cloudflare" => {
            let cloudflare = CloudflareConfig::from_base(base_url.unwrap_or_default())
                .ok_or_else(|| "account id missing from the Cloudflare base URL".to_string())?;
            let url = format!(
                "https://api.cloudflare.com/client/v4/accounts/{}/ai/models/search?per_page=200",
                cloudflare.account
            );
            let mut request = http_client().get(&url);
            if let Some(key) = key {
                request = request.bearer_auth(key);
            }
            let body: serde_json::Value = request
                .send()
                .await
                .map_err(|e| format!("could not reach {url}: {e}"))?
                .json()
                .await
                .map_err(|e| format!("unexpected catalog response from {url}: {e}"))?;
            let mut out: Vec<CatalogModel> = body
                .get("result")
                .and_then(|d| d.as_array())
                .map(|models| {
                    models
                        .iter()
                        .filter(|m| {
                            // Keep text-generation models when the catalog says
                            // what the model does; unknown tasks are kept.
                            match m.get("task").and_then(|t| t.as_object()) {
                                Some(task) => task
                                    .get("name")
                                    .and_then(|n| n.as_str())
                                    .is_none_or(|n| n.eq_ignore_ascii_case("Text Generation")),
                                None => true,
                            }
                        })
                        .filter_map(|m| {
                            let id = m.get("name").and_then(|v| v.as_str())?;
                            Some(CatalogModel {
                                id: id.to_string(),
                                label: m
                                    .get("description")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or(id)
                                    .to_string(),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            out.sort_by(|a, b| a.id.cmp(&b.id));
            Ok(Some(out))
        }
        "qwen-code" => {
            #[cfg(feature = "qwen-code")]
            {
                Ok(Some(
                    crate::qwen_auth::SUPPORTED_MODELS
                        .iter()
                        .map(|id| CatalogModel {
                            id: id.to_string(),
                            label: id.to_string(),
                        })
                        .collect(),
                ))
            }
            #[cfg(not(feature = "qwen-code"))]
            {
                Err("this build has no qwen-code support".to_string())
            }
        }
        _ => {
            let Some(base) = base_url else {
                return Ok(None);
            };
            let url = format!("{}/v1/models", base.trim_end_matches('/'));
            let mut request = http_client().get(&url);
            if let Some(key) = key {
                request = request.bearer_auth(key);
            }
            let response = match request.send().await {
                Ok(r) if r.status().is_success() => r,
                _ => return Ok(None),
            };
            let body: serde_json::Value = match response.json().await {
                Ok(b) => b,
                Err(_) => return Ok(None),
            };
            let mut out: Vec<CatalogModel> = body
                .get("data")
                .and_then(|d| d.as_array())
                .map(|models| {
                    models
                        .iter()
                        .filter_map(|m| {
                            let id = m.get("id").and_then(|v| v.as_str())?;
                            Some(CatalogModel {
                                id: id.to_string(),
                                label: m
                                    .get("name")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or(id)
                                    .to_string(),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            out.sort_by(|a, b| a.id.cmp(&b.id));
            Ok(Some(out))
        }
    }
}

/// The account id and chat base URL of a Cloudflare Workers AI endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudflareConfig {
    pub account: String,
    /// `https://api.cloudflare.com/client/v4/accounts/<id>/ai/v1`
    pub chat_base: String,
}

impl CloudflareConfig {
    /// Parse `/accounts/<id>/ai/…` out of a base URL.
    pub fn from_base(base: &str) -> Option<Self> {
        let mut parts = base.split('/');
        while let Some(part) = parts.next() {
            if part == "accounts" {
                let account = parts.next().filter(|a| !a.is_empty())?;
                return Some(Self {
                    account: account.to_string(),
                    chat_base: format!(
                        "https://api.cloudflare.com/client/v4/accounts/{account}/ai/v1"
                    ),
                });
            }
        }
        None
    }
}

/// Send one tiny completion through an entry and return a short preview.
pub async fn check_entry(entry: &Model) -> Result<String, String> {
    let url = entry
        .chat_completions_url()
        .ok_or_else(|| format!("model '{}' has no base URL", entry.name))?;
    // A missing key is fine for an unauthenticated local endpoint; a named
    // env var that is unset is not.
    if entry.api_key_name.is_some() && entry.api_key().is_none() {
        return Err(format!(
            "model '{}' names ${} but it is not set",
            entry.name,
            entry.api_key_name.as_deref().unwrap_or_default()
        ));
    }
    let body = serde_json::json!({
        "model": entry.upstream_model(),
        "messages": [{"role": "user", "content": "Reply with the word OK."}],
        "max_tokens": 32,
        "stream": false,
    });
    let mut request = http_client().post(&url);
    if let Some(key) = entry.api_key() {
        request = request.bearer_auth(key);
    }
    let response = request
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("could not reach {url}: {e}"))?;
    let status = response.status();
    let value: serde_json::Value = response.json().await.unwrap_or_default();
    if !status.is_success() {
        let message = value
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
            .unwrap_or("");
        return Err(format!("{status}: {message}"));
    }
    let text = value
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first())
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    Ok(text)
}

// ===== Interactive wizard =====

/// Ask which provider to add.
pub fn pick_kind() -> anyhow::Result<&'static ProviderKind> {
    let options: Vec<String> = KINDS
        .iter()
        .map(|k| format!("{} — {}", k.label, k.hint))
        .collect();
    let selected = Select::new("Which provider do you want to add?", options)
        .prompt()
        .map_err(|e| anyhow!("failed to select a provider: {e}"))?;
    KINDS
        .iter()
        .find(|k| selected.starts_with(k.label))
        .ok_or_else(|| anyhow!("unknown selection"))
}

/// Credentials collected for one provider.
struct Credentials {
    /// Chat base URL stored in every entry.
    base_url: Option<String>,
    /// Inline key, or `None` when only an env var is used.
    api_key: Option<String>,
    /// Env var name the key is read from at runtime.
    key_env: Option<String>,
}

async fn collect_credentials(kind: &ProviderKind) -> anyhow::Result<Credentials> {
    match kind.id {
        "klava" => {
            #[cfg(feature = "klava")]
            {
                let mut auth = crate::klava_auth::KlavaAuth::new();
                if !auth.is_authenticated() {
                    println!();
                    println!("Klava uses a browser login (device flow).");
                    let login = Confirm::new("Log in to Klava now?")
                        .with_default(true)
                        .prompt()
                        .map_err(|e| anyhow!("failed to prompt: {e}"))?;
                    if !login {
                        return Err(anyhow!(
                            "Klava models need a login — run `klava login` later and re-run this wizard"
                        ));
                    }
                    auth.authenticate()
                        .await
                        .map_err(|e| anyhow!("klava login failed: {e}"))?;
                }
                Ok(Credentials {
                    // The gateway default is baked into the klava type; keep the
                    // config free of URLs unless the host is overridden.
                    base_url: None,
                    api_key: None,
                    key_env: None,
                })
            }
            #[cfg(not(feature = "klava"))]
            {
                Err(anyhow!("this build has no klava support"))
            }
        }
        "openrouter" => {
            let key = prompt_secret(
                "OpenRouter API key",
                "https://openrouter.ai/keys",
                "OPENROUTER_API_KEY",
            )?;
            Ok(Credentials {
                base_url: Some("https://openrouter.ai/api".to_string()),
                api_key: key.inline,
                key_env: key.env_name,
            })
        }
        "cloudflare" => {
            let account = Text::new("Cloudflare account id:")
                .with_help_message("found in the Workers AI dashboard URL")
                .prompt()
                .map_err(|e| anyhow!("failed to prompt: {e}"))?;
            let base = format!(
                "https://api.cloudflare.com/client/v4/accounts/{}/ai/v1",
                account.trim()
            );
            let key = prompt_secret(
                "Cloudflare API token",
                "https://dash.cloudflare.com/profile/api-tokens (Workers AI: Read)",
                "CLOUDFLARE_AUTH_TOKEN",
            )?;
            Ok(Credentials {
                base_url: Some(base),
                api_key: key.inline,
                key_env: key.env_name,
            })
        }
        #[cfg(feature = "qwen-code")]
        "qwen-code" => {
            let mut auth = crate::qwen_auth::QwenAuth::new();
            if !auth.is_authenticated() {
                let login = Confirm::new("Log in to Qwen Code now?")
                    .with_default(true)
                    .prompt()
                    .map_err(|e| anyhow!("failed to prompt: {e}"))?;
                if !login {
                    return Err(anyhow!(
                        "Qwen models need a login — run `klava providers qwen login` later"
                    ));
                }
                auth.authenticate()
                    .await
                    .map_err(|e| anyhow!("qwen login failed: {e}"))?;
            }
            Ok(Credentials {
                base_url: None,
                api_key: None,
                key_env: None,
            })
        }
        _ => {
            let base = Text::new("Base URL:")
                .with_help_message("e.g. https://openrouter.ai/api or http://localhost:11434")
                .prompt()
                .map_err(|e| anyhow!("failed to prompt: {e}"))?;
            let base = base.trim().trim_end_matches('/').to_string();
            if !base.starts_with("http://") && !base.starts_with("https://") {
                return Err(anyhow!("the base URL must start with http:// or https://"));
            }
            if base.ends_with("/v1") {
                println!(
                    "note: klava appends /v1/chat/completions, so '{}' is stored as-is",
                    base
                );
            }
            let key = prompt_secret("API key (empty for no auth)", "", "KLAVA_CUSTOM_API_KEY")?;
            Ok(Credentials {
                base_url: Some(base),
                api_key: key.inline,
                key_env: key.env_name,
            })
        }
    }
}

/// A key the user supplied either inline or by env var name.
pub struct Secret {
    pub inline: Option<String>,
    pub env_name: Option<String>,
}

/// Ask for a key, offering an already-set env var first.
pub fn prompt_secret(label: &str, docs: &str, env_name: &str) -> anyhow::Result<Secret> {
    if std::env::var(env_name).is_ok_and(|v| !v.is_empty()) {
        println!("✓ using {} from the environment", env_name);
        return Ok(Secret {
            inline: None,
            env_name: Some(env_name.to_string()),
        });
    }
    if !docs.is_empty() {
        println!("  {docs}");
    }
    let key = Text::new(&format!("{label}:"))
        .with_help_message("leave empty to keep the key out of the config file")
        .prompt()
        .map_err(|e| anyhow!("failed to prompt: {e}"))?;
    let key = key.trim();
    if key.is_empty() {
        return Ok(Secret {
            inline: None,
            env_name: None,
        });
    }
    let store = Select::new(
        "Where should the key live?",
        vec![
            format!("environment variable {env_name} (recommended)"),
            "config file (klava config is 0600)".to_string(),
        ],
    )
    .prompt()
    .map_err(|e| anyhow!("failed to select: {e}"))?;
    if store.starts_with("environment") {
        println!("  export {env_name}=…   (add it to your shell profile)");
        Ok(Secret {
            inline: None,
            env_name: Some(env_name.to_string()),
        })
    } else {
        Ok(Secret {
            inline: Some(key.to_string()),
            env_name: None,
        })
    }
}

/// Ask which models to add from a catalog, or for ids by hand.
fn pick_models(catalog: Option<Vec<CatalogModel>>) -> anyhow::Result<Vec<String>> {
    match catalog {
        Some(models) if !models.is_empty() => {
            let labels: Vec<String> = models
                .iter()
                .map(|m| {
                    if m.label == m.id {
                        m.id.clone()
                    } else {
                        format!("{}  ({})", m.id, m.label)
                    }
                })
                .collect();
            let picked = MultiSelect::new("Which models do you want to add?", labels)
                .with_page_size(20)
                .with_help_message("space to select, enter to confirm")
                .prompt()
                .map_err(|e| anyhow!("failed to select models: {e}"))?;
            Ok(picked
                .into_iter()
                .filter_map(|label| {
                    let id = label.split("  (").next()?.trim().to_string();
                    models.iter().find(|m| m.id == id).map(|m| m.id.clone())
                })
                .collect())
        }
        _ => {
            println!("No catalog available — enter model ids, one per line.");
            let mut ids = Vec::new();
            loop {
                let answer = Text::new(if ids.is_empty() {
                    "Model id:"
                } else {
                    "Another model id (empty to finish):"
                })
                .prompt()
                .map_err(|e| anyhow!("failed to prompt: {e}"))?;
                let answer = answer.trim();
                if answer.is_empty() {
                    break;
                }
                ids.push(public_name(answer));
            }
            Ok(ids)
        }
    }
}

/// Run the wizard and write the result into `config`.
///
/// Returns the names of the entries that were added.
pub async fn add_provider_interactive(
    config: &mut Config,
    requested: Option<&str>,
) -> anyhow::Result<Vec<String>> {
    let kind = match requested {
        Some(id) => kind(id).ok_or_else(|| {
            anyhow!(
                "unknown provider '{id}'. Known providers: {}",
                KINDS.iter().map(|k| k.id).collect::<Vec<_>>().join(", ")
            )
        })?,
        None => pick_kind()?,
    };

    println!();
    println!("Setting up {}", kind.label);
    println!();

    let credentials = collect_credentials(kind).await?;

    // Catalog + key for the listing request.
    let listing_key = credentials.api_key.clone().or_else(|| {
        credentials
            .key_env
            .as_deref()
            .and_then(|name| std::env::var(name).ok())
    });
    let catalog = match fetch_catalog(
        kind,
        credentials.base_url.as_deref(),
        listing_key.as_deref(),
    )
    .await
    {
        Ok(catalog) => catalog,
        Err(e) => {
            println!("⚠ {e}");
            println!("  Falling back to manual model ids.");
            None
        }
    };

    let picked = pick_models(catalog)?;
    if picked.is_empty() {
        return Err(anyhow!("no models selected — nothing was changed"));
    }

    let entries: Vec<Model> = picked
        .iter()
        .map(|id| {
            entry(
                kind,
                id,
                credentials.base_url.as_deref(),
                credentials.api_key.as_deref(),
                credentials.key_env.as_deref(),
            )
        })
        .collect();

    // Connection check: one tiny completion per model.
    println!();
    println!("Checking the connection…");
    let mut reachable: Vec<String> = Vec::new();
    for model in &entries {
        match check_entry(model).await {
            Ok(preview) => {
                println!("  ✓ {} {}", model.name, preview);
                reachable.push(model.name.clone());
            }
            Err(e) => println!("  ✗ {}: {}", model.name, e),
        }
    }
    if reachable.is_empty() {
        let keep = Confirm::new("No model answered. Save them anyway?")
            .with_default(false)
            .prompt()
            .map_err(|e| anyhow!("failed to prompt: {e}"))?;
        if !keep {
            return Err(anyhow!(
                "nothing saved — check the key/URL, then run `klava providers add` again"
            ));
        }
    }
    let candidates = if reachable.is_empty() {
        entries.iter().map(|m| m.name.clone()).collect::<Vec<_>>()
    } else {
        reachable
    };

    merge(config, entries);

    // Defaults: the first reachable model becomes the default.
    let current_completion = config.completion_model.clone();
    let suggested = candidates.first().cloned().unwrap_or_default();
    let make_default = current_completion.is_none()
        || Confirm::new(&format!("Use '{suggested}' as the default model?"))
            .with_default(true)
            .prompt()
            .map_err(|e| anyhow!("failed to prompt: {e}"))?;
    if make_default {
        let completion = Select::new("Default (completion) model:", candidates.clone())
            .with_starting_cursor(
                candidates
                    .iter()
                    .position(|c| Some(c) == current_completion.as_ref())
                    .unwrap_or(0),
            )
            .prompt()
            .map_err(|e| anyhow!("failed to select: {e}"))?;
        let reasoning = Select::new("Reasoning model:", candidates.clone())
            .with_starting_cursor(
                candidates
                    .iter()
                    .position(|c| *c == completion)
                    .unwrap_or(0),
            )
            .prompt()
            .map_err(|e| anyhow!("failed to select: {e}"))?;
        set_defaults(config, Some(&completion), Some(&reasoning));

        // Decide model: one entry, detected backend. Offer only entries whose
        // type/URL can actually answer a jigor call.
        let mut decide_options = decide_candidates(config);
        if decide_options.is_empty() {
            println!();
            println!("No decide-capable model was added; the built-in klava gateway stays in use.");
        } else {
            decide_options.push("keep the current decide model".to_string());
            println!();
            println!(
                "The decide model (jigor / System One) answers every classifier call:\nboth the web-search tuning and extensions that ask for a `decide`."
            );
            let choice = Select::new("Decide model:", decide_options)
                .with_help_message("one model only — it must be unambiguous")
                .prompt()
                .map_err(|e| anyhow!("failed to select: {e}"))?;
            if choice != "keep the current decide model" {
                config.decide_model = choice;
            }
        }
    }

    if config.routing.is_empty() {
        config.routing = Routing::Shuffle.as_str().to_string();
    }

    let saved_names = write(config)?;
    Ok(saved_names)
}

/// Persist the config, reporting where it went.
pub fn write(config: &Config) -> anyhow::Result<Vec<String>> {
    config.save().context("could not write the klava config")?;
    let names = config.model_names();
    println!();
    println!(
        "✓ saved {} model{} to {}",
        names.len(),
        if names.len() == 1 { "" } else { "s" },
        Config::get_path().display()
    );
    Ok(names)
}

/// `klava providers remove <name>` — drop every entry with that name.
pub fn remove_provider(config: &mut Config, name: &str) -> anyhow::Result<usize> {
    let before = config.models.len();
    config.models.retain(|m| m.name != name);
    let removed = before - config.models.len();
    if removed == 0 {
        return Err(anyhow!("no model named '{name}' is configured"));
    }

    // Drop dangling defaults so the next server start does not fail validation.
    let names = config.model_names();
    if config
        .completion_model
        .as_deref()
        .is_none_or(|c| !names.iter().any(|n| n == c))
    {
        config.completion_model = names.first().cloned();
    }
    if config
        .reasoning_model
        .as_deref()
        .is_none_or(|r| !names.iter().any(|n| n == r))
    {
        config.reasoning_model = config.completion_model.clone();
    }
    if config
        .search_model
        .as_deref()
        .is_some_and(|s| !names.iter().any(|n| n == s))
    {
        config.search_model = None;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn klava() -> &'static ProviderKind {
        kind("klava").unwrap()
    }

    #[test]
    fn kind_lookup_accepts_aliases() {
        assert_eq!(kind("cf").unwrap().id, "cloudflare");
        assert_eq!(kind("Qwen").unwrap().id, "qwen-code");
        assert_eq!(kind("openai").unwrap().id, "custom");
        assert!(kind("nope").is_none());
    }

    #[test]
    fn entry_carries_the_connection_for_each_kind() {
        let or = entry(
            kind("openrouter").unwrap(),
            "~deepseek/deepseek-flash-latest",
            Some("https://openrouter.ai/api"),
            None,
            Some("OPENROUTER_API_KEY"),
        );
        assert_eq!(or.name, "~deepseek/deepseek-flash-latest");
        assert_eq!(or.upstream_model(), "~deepseek/deepseek-flash-latest");
        assert_eq!(or.provider_type, Type::OpenAICompatible);
        assert_eq!(or.api_key_name.as_deref(), Some("OPENROUTER_API_KEY"));
        assert_eq!(
            or.chat_completions_url().as_deref(),
            Some("https://openrouter.ai/api/v1/chat/completions")
        );

        let k = entry(klava(), "~deepseek/deepseek-flash-latest", None, None, None);
        #[cfg(feature = "klava")]
        assert_eq!(k.provider_type, Type::Klava);
        assert_eq!(k.base_url, None);
    }

    #[test]
    fn merge_replaces_the_same_endpoint_and_keeps_pools() {
        let mut config = Config::default();
        let a = entry(klava(), "m", None, None, None);
        let b = entry(
            kind("openrouter").unwrap(),
            "m",
            Some("https://openrouter.ai/api"),
            None,
            Some("OPENROUTER_API_KEY"),
        );
        merge(&mut config, vec![a.clone()]);
        merge(&mut config, vec![b.clone()]);
        assert_eq!(
            config.models.len(),
            2,
            "same name on two endpoints is a pool"
        );

        // Re-adding the klava entry updates it in place.
        merge(&mut config, vec![a]);
        assert_eq!(config.models.len(), 2);
        assert_eq!(config.pool("m").len(), 2);
    }

    #[test]
    fn set_defaults_ignores_unknown_names() {
        let mut config = Config {
            models: vec![entry(klava(), "known", None, None, None)],
            ..Config::default()
        };
        set_defaults(&mut config, Some("missing"), Some("known"));
        assert_eq!(config.completion_model, None);
        assert_eq!(config.reasoning_model.as_deref(), Some("known"));

        set_defaults(&mut config, Some("known"), None);
        assert_eq!(config.completion_model.as_deref(), Some("known"));
        assert_eq!(config.reasoning_model.as_deref(), Some("known"));
    }

    #[test]
    fn decide_candidates_exclude_qwen_only_entries() {
        let mut config = Config {
            models: vec![
                entry(klava(), "klava-model", None, None, None),
                entry(
                    kind("qwen-code").unwrap(),
                    "qwen3-coder-plus",
                    None,
                    None,
                    None,
                ),
            ],
            ..Config::default()
        };
        config.models[0].api_key = Some("k".to_string());
        let candidates = decide_candidates(&config);
        assert_eq!(candidates, vec!["klava-model"]);
    }

    #[test]
    fn remove_drops_entries_and_repairs_defaults() {
        let mut config = Config {
            models: vec![
                entry(klava(), "a", None, None, None),
                entry(klava(), "b", None, None, None),
            ],
            completion_model: Some("a".to_string()),
            reasoning_model: Some("a".to_string()),
            ..Config::default()
        };
        let removed = remove_provider(&mut config, "a").unwrap();
        assert_eq!(removed, 1);
        assert_eq!(config.completion_model.as_deref(), Some("b"));
        assert_eq!(config.reasoning_model.as_deref(), Some("b"));
        assert!(remove_provider(&mut config, "a").is_err());
    }

    #[test]
    fn cloudflare_base_parsing() {
        let cf = CloudflareConfig::from_base(
            "https://api.cloudflare.com/client/v4/accounts/abc123/ai/v1",
        )
        .unwrap();
        assert_eq!(cf.account, "abc123");
        assert_eq!(
            cf.chat_base,
            "https://api.cloudflare.com/client/v4/accounts/abc123/ai/v1"
        );
        assert!(CloudflareConfig::from_base("https://openrouter.ai/api").is_none());
    }
}
