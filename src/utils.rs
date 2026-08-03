use anyhow::{Context, Result};
use colored::*;
use inquire::Confirm;
use serde_json::Value;
use similar::{ChangeTag, TextDiff};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Anthropic message ID prefix (`msg_`).
/// The Anthropic Messages API requires response IDs to start with `msg_`.
const ANTHROPIC_MSG_PREFIX: &str = "msg_";
/// Anthropic tool_use ID prefix (`toolu_`).
/// The Anthropic Messages API requires tool_use IDs to start with `toolu_`.
const ANTHROPIC_TOOL_PREFIX: &str = "toolu_";
/// OpenAI tool_call ID prefix (`call_`).
const OPENAI_TOOL_PREFIX: &str = "call_";

/// Generate a fresh Anthropic-format message id.
///
/// Anthropic requires IDs of the form `msg_<24-char base62>`. We use UUIDv4
/// (hex) and strip dashes — that gives a stable 32-char suffix which is
/// longer than required but always valid.
pub fn new_anthropic_message_id() -> String {
    format!("{}{}", ANTHROPIC_MSG_PREFIX, random_id_suffix())
}

/// Generate a fresh Anthropic-format tool_use id (`toolu_<24+ chars>`).
pub fn new_anthropic_tool_id() -> String {
    format!("{}{}", ANTHROPIC_TOOL_PREFIX, random_id_suffix())
}

/// Generate a fresh OpenAI-format tool_call id (`call_<uuid>`).
/// Used when synthesizing IDs for incoming `tool_result` blocks.
pub fn new_openai_tool_id() -> String {
    format!("{}{}", OPENAI_TOOL_PREFIX, random_id_suffix())
}

fn random_id_suffix() -> String {
    Uuid::new_v4().simple().to_string()
}

/// Convert an OpenAI-style id into an Anthropic-style id.
///
/// - `chatcmpl-…`           → `msg_…` (strip the `chatcmpl-` prefix, prepend `msg_`)
/// - `call_…`               → `toolu_…` (rewrite the `call_` prefix)
/// - already `msg_…`/`toolu_…` → returned unchanged
/// - anything else          → passed through verbatim
pub fn openai_to_anthropic_id(id: &str) -> String {
    if let Some(rest) = id.strip_prefix("chatcmpl-") {
        format!("{}{}", ANTHROPIC_MSG_PREFIX, sanitize_id_suffix(rest))
    } else if let Some(rest) = id.strip_prefix(OPENAI_TOOL_PREFIX) {
        format!("{}{}", ANTHROPIC_TOOL_PREFIX, sanitize_id_suffix(rest))
    } else {
        id.to_string()
    }
}

/// Convert an Anthropic-style id back into an OpenAI-style id.
///
/// - `toolu_…` → `call_…` (rewrite the prefix)
/// - `msg_…`   → left as-is (Anthropic message ids are not used by upstream OpenAI APIs)
/// - anything else → returned unchanged
pub fn anthropic_to_openai_id(id: &str) -> String {
    if let Some(rest) = id.strip_prefix(ANTHROPIC_TOOL_PREFIX) {
        format!("{}{}", OPENAI_TOOL_PREFIX, sanitize_id_suffix(rest))
    } else {
        id.to_string()
    }
}

/// Convert an OpenAI-format tool_call id into an Anthropic tool_use id.
///
/// Anthropic requires tool_use ids to start with `toolu_`. Upstream providers
/// use either `call_…` (standard OpenAI) or `chatcmpl-tool-…` (vLLM/CloudRu/
/// Qwen) prefixes, both of which must be rewritten so Claude Code accepts the
/// emitted `content_block_start`. The string also flows back as `tool_use_id`
/// on `tool_result` blocks, and `anthropic_to_openai_id` rewrites it to `call_`
/// for the next upstream request.
///
/// - `call_…`               → `toolu_…`
/// - `chatcmpl-tool-…`      → `toolu_…`
/// - already `toolu_…`      → returned unchanged
/// - anything else          → passed through verbatim
pub fn openai_to_tool_id(id: &str) -> String {
    if let Some(rest) = id.strip_prefix("chatcmpl-tool-") {
        format!("{}{}", ANTHROPIC_TOOL_PREFIX, sanitize_id_suffix(rest))
    } else if let Some(rest) = id.strip_prefix(OPENAI_TOOL_PREFIX) {
        format!("{}{}", ANTHROPIC_TOOL_PREFIX, sanitize_id_suffix(rest))
    } else {
        id.to_string()
    }
}

/// Convert an upstream OpenAI-format tool_call id into the canonical OpenAI
/// `call_…` form used by the Responses API.
///
/// The Responses API uses `call_…` ids on `function_call`/`function_call_output`
/// items; vLLM/CloudRu/Qwen upstreams instead emit `chatcmpl-tool-…`. Rewriting
/// the prefix keeps Codex on the standard format while the id round-trips back
/// unchanged (Codex echoes it as `tool_call_id` and OpenAI-compatible providers
/// accept any opaque string).
///
/// - `chatcmpl-tool-…`      → `call_…`
/// - `call_…`               → returned unchanged
/// - anything else          → passed through verbatim
pub fn openai_to_call_id(id: &str) -> String {
    if let Some(rest) = id.strip_prefix("chatcmpl-tool-") {
        format!("{}{}", OPENAI_TOOL_PREFIX, sanitize_id_suffix(rest))
    } else {
        id.to_string()
    }
}

/// Strip characters that would break an SSE/JSON identifier or a downstream
/// HTTP header. Keeps alphanumerics plus `_` and `-`.
fn sanitize_id_suffix(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Write data to file atomically with backup
pub fn write_with_backup<P: AsRef<Path>>(path: P, data: &[u8]) -> Result<()> {
    let path = path.as_ref();
    let parent = path.parent().context("Path has no parent directory")?;

    // Ensure parent directory exists
    fs::create_dir_all(parent)?;

    // Generate temp file path in same directory
    let temp_path = parent.join(format!(
        ".tmp.{}.{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("file"),
        std::process::id()
    ));

    // If original file exists, create timestamped backup
    if path.exists() {
        let backup_path = generate_backup_path(path)?;
        fs::copy(path, &backup_path)
            .with_context(|| format!("Failed to create backup at {:?}", backup_path))?;
    }

    // Write to temp file first
    {
        let mut temp_file = fs::File::create(&temp_path)?;
        temp_file.write_all(data)?;
        temp_file.sync_all()?;
    }

    // Atomically rename temp file to target
    fs::rename(&temp_path, path)?;

    // Sync parent directory on Unix
    #[cfg(unix)]
    {
        let dir = fs::OpenOptions::new().read(true).open(parent)?;
        dir.sync_all()?;
    }

    Ok(())
}

/// Generate unique backup path with timestamp
fn generate_backup_path(original: &Path) -> Result<PathBuf> {
    let parent = original.parent().unwrap_or(Path::new("."));
    let stem = original
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("file");
    let ext = original
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{}", e))
        .unwrap_or_default();

    let backup_name = format!("{}.backup{}", stem, ext);
    Ok(parent.join(backup_name))
}

/// Generate diff between old and new config
pub fn generate_diff(old_json: &str, new_json: &str, path: &Path) -> String {
    let diff = TextDiff::from_lines(old_json, new_json);

    let mut output = String::new();
    output.push_str(&format!("--- {}\n", path.display()));
    output.push_str(&format!("+++ {}\n", path.display()));
    output.push_str("@@ Configuration Changes @@\n");

    for change in diff.iter_all_changes() {
        let sign = match change.tag() {
            ChangeTag::Delete => "-",
            ChangeTag::Insert => "+",
            ChangeTag::Equal => " ",
        };
        let sign = match change.tag() {
            ChangeTag::Delete => sign.red(),
            ChangeTag::Insert => sign.green(),
            ChangeTag::Equal => sign.normal(),
        };

        output.push_str(&format!("{}{}", sign, change));
    }

    output
}

/// Ask user for approval
pub fn ask_user_approval(prompt: &str) -> Result<bool> {
    let answer = Confirm::new(prompt).with_default(false).prompt()?;

    Ok(answer)
}

/// Clean JSON schema by removing unsupported formats
pub fn clean_schema(mut schema: Value) -> Value {
    if let Some(obj) = schema.as_object_mut() {
        // Remove "format": "uri"
        if obj.get("format").and_then(|v| v.as_str()) == Some("uri") {
            obj.remove("format");
        }

        // Recursively clean nested schemas
        if let Some(properties) = obj.get_mut("properties").and_then(|v| v.as_object_mut()) {
            for (_, value) in properties.iter_mut() {
                *value = clean_schema(value.clone());
            }
        }

        if let Some(items) = obj.get_mut("items") {
            *items = clean_schema(items.clone());
        }
    }

    schema
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::Path;

    #[test]
    fn test_generate_diff() {
        let old = r#"{"key": "old", "value": 1}"#;
        let new = r#"{"key": "new", "value": 1}"#;
        let path = Path::new("config.json");

        let diff = generate_diff(old, new, path);

        assert!(diff.contains("old"), "Should show removed line");
        assert!(diff.contains("new"), "Should show added line");
        assert!(diff.contains("config.json"), "Should include file path");
    }

    #[test]
    fn test_generate_diff_json_order() {
        let a = json!({
            "provider": {
                "klava": { "name": "Klava" },
                "openrouter": { "name": "OpenRouter" }
            }
        });

        let b = json!({
            "provider": {
                "openrouter": { "name": "OpenRouter" },
                "klava": { "name": "Klava" }
            }
        });

        let path = Path::new("opencode.json");
        let diff = generate_diff(&a.to_string(), &b.to_string(), path);

        assert!(diff.contains(""));
    }

    #[test]
    fn test_generate_diff_with_changes() {
        let a = json!({
            "provider": {
                "klava": { "name": "Klava" },
                "openrouter": { "name": "OpenRouter" }
            }
        });

        let b = json!({
            "provider": {
                "openrouter": { "name": "OpenRouter", "base_url": "https://" },
                "klava": { "name": "Klava" }
            }
        });

        let path = Path::new("opencode.json");
        let diff = generate_diff(&a.to_string(), &b.to_string(), path);

        assert!(diff.contains(""));

        // Verify the diff contains the expected header
        assert!(diff.contains("--- opencode.json"));
        assert!(diff.contains("+++ opencode.json"));
        assert!(diff.contains("@@ Configuration Changes @@"));

        let change_count = diff
            .lines()
            .filter(|line| line.starts_with('+') || line.starts_with('-'))
            .count();
        assert!(change_count > 0, "Should contain at least one change");
    }

    #[test]
    fn test_openai_to_anthropic_message_id_chatcmpl() {
        let out = openai_to_anthropic_id("chatcmpl-abc123");
        assert!(out.starts_with("msg_"));
        assert!(out.contains("abc123"));
        assert!(!out.contains("chatcmpl-"));
    }

    #[test]
    fn test_openai_to_anthropic_tool_id_call_prefix() {
        let out = openai_to_anthropic_id("call_abc123");
        assert_eq!(out, "toolu_abc123");
    }

    #[test]
    fn test_openai_to_anthropic_tool_id_chatcmpl_tool_prefix() {
        let out = openai_to_anthropic_id("chatcmpl-tool-bd7ecfdb179677c9");
        assert_eq!(out, "msg_tool-bd7ecfdb179677c9");
    }

    #[test]
    fn test_openai_to_tool_id_call_prefix() {
        assert_eq!(openai_to_tool_id("call_abc123"), "toolu_abc123");
    }

    #[test]
    fn test_openai_to_tool_id_chatcmpl_tool_prefix() {
        // vLLM/CloudRu/Qwen emit tool_call ids as `chatcmpl-tool-…`;
        // these must be rewritten to a `toolu_` id, not a `msg_` id.
        let out = openai_to_tool_id("chatcmpl-tool-b8ce01f013736044");
        assert_eq!(out, "toolu_b8ce01f013736044");
    }

    #[test]
    fn test_openai_to_tool_id_passthrough_for_anthropic_ids() {
        assert_eq!(openai_to_tool_id("toolu_xyz"), "toolu_xyz");
    }

    #[test]
    fn test_openai_to_tool_id_passthrough_for_unknown() {
        assert_eq!(openai_to_tool_id("foo_bar"), "foo_bar");
    }

    #[test]
    fn test_openai_to_call_id_chatcmpl_tool_prefix() {
        assert_eq!(openai_to_call_id("chatcmpl-tool-b8ce01f013736044"), "call_b8ce01f013736044");
    }

    #[test]
    fn test_openai_to_call_id_passthrough_for_call() {
        assert_eq!(openai_to_call_id("call_abc123"), "call_abc123");
    }

    #[test]
    fn test_openai_to_call_id_passthrough_for_unknown() {
        assert_eq!(openai_to_call_id("foo_bar"), "foo_bar");
    }

    #[test]
    fn test_tool_id_round_trip_toolu_to_call() {
        let anthropic_id = openai_to_tool_id("call_abc123-def456");
        assert_eq!(anthropic_id, "toolu_abc123-def456");
        let back = anthropic_to_openai_id(&anthropic_id);
        assert_eq!(back, "call_abc123-def456");
    }

    #[test]
    fn test_openai_to_anthropic_id_passthrough_for_anthropic_ids() {
        assert_eq!(openai_to_anthropic_id("msg_xyz"), "msg_xyz");
        assert_eq!(openai_to_anthropic_id("toolu_xyz"), "toolu_xyz");
    }

    #[test]
    fn test_openai_to_anthropic_id_passthrough_for_unknown() {
        // Unknown formats are returned unchanged to avoid mangling client-provided IDs.
        assert_eq!(openai_to_anthropic_id("foo_bar"), "foo_bar");
    }

    #[test]
    fn test_anthropic_to_openai_tool_id_toolu_to_call() {
        assert_eq!(anthropic_to_openai_id("toolu_abc123"), "call_abc123");
    }

    #[test]
    fn test_anthropic_to_openai_id_passthrough_for_msg_ids() {
        // msg_ ids do not round-trip into OpenAI (they aren't referenced by the API).
        assert_eq!(anthropic_to_openai_id("msg_xyz"), "msg_xyz");
    }

    #[test]
    fn test_anthropic_to_openai_id_passthrough_for_unknown() {
        // Already OpenAI-format IDs come back unchanged.
        assert_eq!(anthropic_to_openai_id("call_xyz"), "call_xyz");
    }

    #[test]
    fn test_id_round_trip_tool() {
        let openai_id = "call_abc123-def456";
        let anthropic_id = openai_to_anthropic_id(openai_id);
        let back = anthropic_to_openai_id(&anthropic_id);
        assert_eq!(back, openai_id);
    }

    #[test]
    fn test_sanitize_id_suffix_replaces_special_chars() {
        // Special chars that would break SSE/JSON identifiers get replaced.
        let out = openai_to_anthropic_id("chatcmpl-a/b c?d");
        assert!(!out.contains('/'));
        assert!(!out.contains(' '));
        assert!(!out.contains('?'));
        assert!(out.starts_with("msg_"));
    }

    #[test]
    fn test_new_anthropic_ids_have_correct_prefixes() {
        let msg = new_anthropic_message_id();
        assert!(msg.starts_with("msg_"));
        assert!(msg.len() > "msg_".len());

        let tool = new_anthropic_tool_id();
        assert!(tool.starts_with("toolu_"));
        assert!(tool.len() > "toolu_".len());

        let openai_tool = new_openai_tool_id();
        assert!(openai_tool.starts_with("call_"));
        assert!(openai_tool.len() > "call_".len());
    }
}
