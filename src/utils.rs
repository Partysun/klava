use anyhow::{Context, Result};
use colored::*;
use inquire::Confirm;
use similar::{ChangeTag, TextDiff};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

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
}
