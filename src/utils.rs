use anyhow::{Context, Result};
use colored::*;
use inquire::Confirm;
use serde_json::{Map, Value, json};
use similar::{ChangeTag, TextDiff};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
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

/// Generate a fresh Anthropic-format *server* tool_use id (`srvtoolu_<24+ chars>`).
///
/// Server tool uses (`web_search`, ...) use the `srvtoolu_` prefix, distinct
/// from client `tool_use` ids (`toolu_`).
pub fn new_anthropic_server_tool_id() -> String {
    format!("srvtoolu_{}", random_id_suffix())
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

/// Spawn a child, retrying while the OS reports `ETXTBSY` ("Text file busy").
///
/// A just-written executable can fail to start with `ExecutableFileBusy` when
/// another thread happens to `fork` while the file is still open for writing:
/// the forked child inherits the writable descriptor and the kernel then
/// refuses `execve`. The window is microseconds, and `Command::spawn` does not
/// retry it, so a short bounded retry is the standard cure.
pub fn spawn_retrying_busy(command: &mut Command) -> std::io::Result<std::process::Child> {
    /// ~200 ms in total; nobody holds an executable open that long.
    const ATTEMPTS: u32 = 40;

    let mut attempt = 0;
    loop {
        match command.spawn() {
            Ok(child) => return Ok(child),
            Err(e)
                if e.kind() == std::io::ErrorKind::ExecutableFileBusy && attempt + 1 < ATTEMPTS =>
            {
                attempt += 1;
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(e) => return Err(e),
        }
    }
}

/// Ask user for approval
pub fn ask_user_approval(prompt: &str) -> Result<bool> {
    let answer = Confirm::new(prompt).with_default(false).prompt()?;

    Ok(answer)
}

/// Keyword that carries a *map* of sub-schemas (name -> schema).
const SCHEMA_MAP_KEYWORDS: [&str; 3] = ["properties", "patternProperties", "$defs"];

/// Keywords that carry a single sub-schema.
const SCHEMA_KEYWORDS: [&str; 11] = [
    "items",
    "additionalItems",
    "additionalProperties",
    "propertyNames",
    "not",
    "if",
    "then",
    "else",
    "contains",
    "unevaluatedItems",
    "unevaluatedProperties",
];

/// Keywords that carry a *list* of sub-schemas.
const SCHEMA_LIST_KEYWORDS: [&str; 3] = ["anyOf", "oneOf", "allOf"];

/// Keywords that carry a map of `value -> schema` (discriminated unions).
const SCHEMA_VALUE_MAP_KEYWORDS: [&str; 2] = ["dependentSchemas", "dependencies"];

/// Maximum serialized size of a single tool schema, in bytes.
///
/// Moonshot/Kimi rejects tool schemas larger than roughly 128 KiB with a bare
/// `invalid_request`, so anything we emit must stay under that. The value is
/// kept a little below the observed cliff to leave room for the surrounding
/// request (name, description, sibling tools) and for byte-level variation in
/// escaping.
const MAX_SCHEMA_BYTES: usize = 96 * 1024;

/// Maximum `$ref` expansion depth.
///
/// Guards against pathological or cyclic `$defs` graphs. Real-world tool
/// schemas nest well below this; anything deeper is collapsed to a permissive
/// object rather than dropped, so the model still sees a valid schema.
const MAX_SCHEMA_DEPTH: usize = 24;

/// Clean a JSON schema for providers that reject parts of JSON Schema.
///
/// Three things happen here, all of which real harnesses need:
///
/// 1. `"format": "uri"` is stripped (rejected by some upstreams).
/// 2. `$ref`s into `$defs` are inlined and the `$defs` block is dropped.
///    Harnesses that emit `zod`/`pydantic`-style schemas (Claude Code's MCP
///    tools, for instance) lean heavily on `$ref`; several upstreams reject
///    them outright.
/// 3. The result is capped at [`MAX_SCHEMA_BYTES`] by collapsing the largest
///    sub-schemas to `{"type": "object"}`. Inlining can inflate a schema by
///    an order of magnitude when a definition is referenced repeatedly, and
///    an oversized schema fails the whole request.
///
/// The recursion covers the full schema vocabulary (`properties`, `items`,
/// `oneOf`/`anyOf`/`allOf`, `$defs`, ...), not just `properties` and `items`.
pub fn clean_schema(schema: Value) -> Value {
    let defs = schema
        .get("$defs")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    let mut cleaned = clean_schema_node(schema, &defs, &mut Vec::new(), 0);
    if let Some(obj) = cleaned.as_object_mut() {
        obj.remove("$defs");
    }
    shrink_to_fit(cleaned, MAX_SCHEMA_BYTES)
}

/// Recursive worker for [`clean_schema`].
///
/// `stack` holds the `$defs` names currently being expanded, so a self-
/// referential schema terminates instead of recursing forever.
fn clean_schema_node(
    node: Value,
    defs: &Map<String, Value>,
    stack: &mut Vec<String>,
    depth: usize,
) -> Value {
    if depth > MAX_SCHEMA_DEPTH {
        return json!({"type": "object"});
    }

    match node {
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| clean_schema_node(item, defs, stack, depth + 1))
                .collect(),
        ),
        Value::Object(mut obj) => {
            // Inline a `$ref` into `$defs`, keeping any sibling keywords
            // (`description`, `default`, ...) which JSON Schema allows next to
            // a `$ref` and which carry meaning for the model.
            if let Some(target) = obj
                .get("$ref")
                .and_then(Value::as_str)
                .and_then(|r| r.strip_prefix("#/$defs/"))
                .map(str::to_string)
            {
                let sibling: Vec<(String, Value)> = obj
                    .iter()
                    .filter(|(k, _)| k.as_str() != "$ref")
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();

                let resolved = if stack.iter().any(|seen| seen == &target) {
                    // Recursive definition: stop and leave a permissive object.
                    json!({"type": "object"})
                } else {
                    match defs.get(&target) {
                        Some(definition) => {
                            stack.push(target.clone());
                            let resolved =
                                clean_schema_node(definition.clone(), defs, stack, depth + 1);
                            stack.pop();
                            resolved
                        }
                        // Dangling reference: nothing to inline. Drop the `$ref`
                        // so the provider does not choke on an unresolvable
                        // pointer; siblings still describe the value.
                        None => json!({"type": "object"}),
                    }
                };

                let mut resolved = resolved;
                if let Some(map) = resolved.as_object_mut() {
                    for (key, value) in sibling {
                        map.insert(key, clean_schema_node(value, defs, stack, depth + 1));
                    }
                }
                return resolved;
            }

            // `format: uri` is rejected by some upstreams.
            if obj.get("format").and_then(Value::as_str) == Some("uri") {
                obj.remove("format");
            }

            for keyword in SCHEMA_MAP_KEYWORDS {
                if let Some(map) = obj.get_mut(keyword).and_then(Value::as_object_mut) {
                    for value in map.values_mut() {
                        let taken = std::mem::take(value);
                        *value = clean_schema_node(taken, defs, stack, depth + 1);
                    }
                }
            }

            for keyword in SCHEMA_KEYWORDS {
                if let Some(value) = obj.get_mut(keyword) {
                    let taken = std::mem::take(value);
                    *value = clean_schema_node(taken, defs, stack, depth + 1);
                }
            }

            for keyword in SCHEMA_LIST_KEYWORDS {
                if let Some(list) = obj.get_mut(keyword).and_then(Value::as_array_mut) {
                    for value in list.iter_mut() {
                        let taken = std::mem::take(value);
                        *value = clean_schema_node(taken, defs, stack, depth + 1);
                    }
                }
            }

            for keyword in SCHEMA_VALUE_MAP_KEYWORDS {
                if let Some(map) = obj.get_mut(keyword).and_then(Value::as_object_mut) {
                    for value in map.values_mut() {
                        // `dependencies` may hold an array of property names
                        // rather than a schema; leave those untouched.
                        if value.is_object() {
                            let taken = std::mem::take(value);
                            *value = clean_schema_node(taken, defs, stack, depth + 1);
                        }
                    }
                }
            }

            Value::Object(obj)
        }
        other => other,
    }
}

/// Collapse the biggest sub-schemas until `schema` serializes within `limit`.
///
/// Providers reject oversized tool schemas, and a rejected schema fails the
/// entire request, so losing detail is strictly better than losing the call.
/// Only `properties` and the `oneOf`/`anyOf`/`allOf` branches are collapsed:
/// those are where large tool schemas put their bulk, and replacing one with a
/// permissive object keeps the schema valid while costing the model only the
/// detail of that one field.
fn shrink_to_fit(schema: Value, limit: usize) -> Value {
    let mut schema = schema;
    while serde_json::to_string(&schema).map(|s| s.len()).unwrap_or(0) > limit {
        if !collapse_largest(&mut schema) {
            // Nothing left to collapse: the schema is irreducibly large.
            break;
        }
    }
    schema
}

/// Collapse the single largest collapsible sub-schema. Returns `false` when
/// there is nothing left to collapse.
fn collapse_largest(schema: &mut Value) -> bool {
    let Some(target) = largest_collapsible(schema) else {
        return false;
    };
    if let Some(slot) = schema.pointer_mut(&target) {
        *slot = json!({"type": "object"});
        true
    } else {
        false
    }
}

/// Find the JSON pointer of the largest collapsible sub-schema.
///
/// Only children reachable through `properties`, `oneOf`, `anyOf` and `allOf`
/// are considered, so the result always points at something `collapse_largest`
/// can replace wholesale.
fn largest_collapsible(schema: &Value) -> Option<String> {
    fn walk(node: &Value, path: &str, best: &mut Option<(usize, String)>) {
        if let Some(obj) = node.as_object() {
            for keyword in SCHEMA_MAP_KEYWORDS {
                if let Some(map) = obj.get(keyword).and_then(Value::as_object) {
                    for (key, value) in map {
                        let size = serde_json::to_string(value).map(|s| s.len()).unwrap_or(0);
                        if best.as_ref().map(|(b, _)| size > *b).unwrap_or(true) {
                            *best =
                                Some((size, format!("{path}/{keyword}/{}", escape_pointer(key))));
                        }
                        walk(
                            value,
                            &format!("{path}/{keyword}/{}", escape_pointer(key)),
                            best,
                        );
                    }
                }
            }
            for keyword in SCHEMA_LIST_KEYWORDS {
                if let Some(list) = obj.get(keyword).and_then(Value::as_array) {
                    for (index, value) in list.iter().enumerate() {
                        let size = serde_json::to_string(value).map(|s| s.len()).unwrap_or(0);
                        if best.as_ref().map(|(b, _)| size > *b).unwrap_or(true) {
                            *best = Some((size, format!("{path}/{keyword}/{index}")));
                        }
                        walk(value, &format!("{path}/{keyword}/{index}"), best);
                    }
                }
            }
            for keyword in SCHEMA_KEYWORDS {
                if let Some(value) = obj.get(keyword) {
                    let size = serde_json::to_string(value).map(|s| s.len()).unwrap_or(0);
                    if best.as_ref().map(|(b, _)| size > *b).unwrap_or(true) {
                        *best = Some((size, format!("{path}/{keyword}")));
                    }
                    walk(value, &format!("{path}/{keyword}"), best);
                }
            }
        }
    }

    let mut best = None;
    walk(schema, "", &mut best);
    best.map(|(_, path)| path)
}

/// Escape a JSON pointer reference token (RFC 6901).
fn escape_pointer(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::Path;

    #[test]
    fn clean_schema_inlines_refs_and_drops_defs() {
        let schema = json!({
            "type": "object",
            "properties": {
                "point": { "$ref": "#/$defs/Point" }
            },
            "$defs": {
                "Point": {
                    "type": "object",
                    "properties": { "x": { "type": "number" } }
                }
            }
        });

        let cleaned = clean_schema(schema);
        assert_eq!(cleaned.get("$defs"), None, "$defs must be dropped");
        assert_eq!(cleaned["properties"]["point"]["type"], "object");
        assert_eq!(
            cleaned["properties"]["point"]["properties"]["x"]["type"],
            "number"
        );
    }

    #[test]
    fn clean_schema_keeps_sibling_keywords_next_to_ref() {
        let schema = json!({
            "type": "object",
            "properties": {
                "point": {
                    "$ref": "#/$defs/Point",
                    "description": "a point",
                    "default": null
                }
            },
            "$defs": {
                "Point": { "type": "object", "properties": { "x": { "type": "number" } } }
            }
        });

        let cleaned = clean_schema(schema);
        assert_eq!(cleaned["properties"]["point"]["description"], "a point");
        assert_eq!(cleaned["properties"]["point"]["type"], "object");
    }

    #[test]
    fn clean_schema_terminates_on_recursive_refs() {
        let schema = json!({
            "type": "object",
            "properties": { "node": { "$ref": "#/$defs/Node" } },
            "$defs": {
                "Node": {
                    "type": "object",
                    "properties": { "child": { "$ref": "#/$defs/Node" } }
                }
            }
        });

        // Must not hang or blow the stack.
        let cleaned = clean_schema(schema);
        assert_eq!(cleaned.get("$defs"), None);
    }

    #[test]
    fn clean_schema_recurses_into_unions() {
        let schema = json!({
            "oneOf": [
                { "type": "string", "format": "uri" },
                { "type": "object", "properties": { "u": { "type": "string", "format": "uri" } } }
            ]
        });

        let cleaned = clean_schema(schema);
        assert_eq!(cleaned["oneOf"][0].get("format"), None);
        assert_eq!(cleaned["oneOf"][1]["properties"]["u"].get("format"), None);
    }

    #[test]
    fn clean_schema_handles_dangling_ref() {
        let schema = json!({
            "type": "object",
            "properties": { "x": { "$ref": "#/$defs/Missing" } }
        });

        let cleaned = clean_schema(schema);
        assert_eq!(cleaned["properties"]["x"]["type"], "object");
    }

    #[test]
    fn clean_schema_shrinks_oversized_schemas() {
        // A schema far past the per-tool limit: many large sibling properties.
        let mut properties = serde_json::Map::new();
        for index in 0..400 {
            properties.insert(
                format!("field_{index}"),
                json!({
                    "type": "object",
                    "description": "x".repeat(400),
                    "properties": { "nested": { "type": "string", "description": "y".repeat(400) } }
                }),
            );
        }
        let schema = json!({ "type": "object", "properties": properties });
        assert!(serde_json::to_string(&schema).unwrap().len() > MAX_SCHEMA_BYTES);

        let cleaned = clean_schema(schema);
        let size = serde_json::to_string(&cleaned).unwrap().len();
        assert!(
            size <= MAX_SCHEMA_BYTES,
            "expected <= {MAX_SCHEMA_BYTES} bytes, got {size}"
        );
        // Shrinking must keep the schema structurally valid.
        assert_eq!(cleaned["type"], "object");
        assert!(cleaned["properties"].is_object());
    }

    #[test]
    fn clean_schema_leaves_small_schemas_intact() {
        let schema = json!({
            "type": "object",
            "properties": { "a": { "type": "string" } }
        });
        assert_eq!(clean_schema(schema.clone()), schema);
    }

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
        assert_eq!(
            openai_to_call_id("chatcmpl-tool-b8ce01f013736044"),
            "call_b8ce01f013736044"
        );
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
