use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

#[derive(Debug, Serialize)]
pub struct InstallPayload {
    pub version: String,
    pub os: String,
    pub arch: String,
    pub unique_id: String,
}

#[derive(Debug, Deserialize)]
struct CratesIoVersion {
    num: String,
}

#[derive(Debug, Deserialize)]
struct CratesIoVersionsResponse {
    versions: Vec<CratesIoVersion>,
}

// Separate the version comparison logic for easier testing
fn compare_versions(current_version: &str, latest_version: &str) -> bool {
    // Simple semver comparison
    fn parse_version(v: &str) -> Vec<u32> {
        v.split('.').map(|s| s.parse().unwrap_or(0)).collect()
    }

    let current = parse_version(current_version);
    let latest = parse_version(latest_version);

    let comparison = current
        .iter()
        .zip(latest.iter())
        .map(|(a, b)| a.cmp(b))
        .find(|&ord| ord != Ordering::Equal)
        .unwrap_or_else(|| current.len().cmp(&latest.len()));

    comparison == Ordering::Less
}

async fn fetch_latest_version_from_crates_io() -> Result<Option<String>> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        // USER AGENT is must have for query request to crate.io
        .user_agent(format!("klava/{}", env!("CARGO_PKG_VERSION")))
        .build()?;

    // Fetch from crates.io API
    let url = "https://crates.io/api/v1/crates/klava/versions";

    let response = match client.get(url).send().await {
        Ok(response) => response,
        Err(_) => return Ok(None), // Return None on network request failures
    };

    if !response.status().is_success() {
        return Ok(None);
    }

    let versions_data: CratesIoVersionsResponse = match response.json().await {
        Ok(data) => data,
        Err(_) => return Ok(None), // Return None on JSON parsing failures
    };

    // Get the latest version from the list
    if let Some(latest_version_info) = versions_data.versions.first() {
        Ok(Some(latest_version_info.num.clone()))
    } else {
        Ok(None) // No versions found
    }
}

pub async fn check_for_update(current_version: &str) -> Result<bool> {
    match fetch_latest_version_from_crates_io().await {
        Ok(Some(latest_version)) => Ok(compare_versions(current_version, &latest_version)),
        Ok(None) => Ok(false), // No version found or error
        Err(_) => Ok(false),   // Error occurred
    }
}

#[cfg(feature = "telemetry")]
pub async fn send_install_event(version: &str) -> anyhow::Result<bool> {
    use confy::get_configuration_file_path;
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::time::SystemTime;

    // Check if config file exists (first run detection)
    let config_path =
        get_configuration_file_path("klava", Some("config")).expect("Failed to get config path");

    if config_path.exists() {
        tracing::debug!("Config exists, skipping telemetry (already sent)");
        return Ok(false);
    }

    // Generate unique ID based on system characteristics
    let mut hasher = DefaultHasher::new();

    if let Ok(time) = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
        time.as_nanos().hash(&mut hasher);
    }

    if let Ok(hostname) = std::env::var("HOSTNAME") {
        hostname.hash(&mut hasher);
    } else if let Ok(hostname) = std::env::var("COMPUTERNAME") {
        hostname.hash(&mut hasher);
    }

    if let Ok(current_dir) = std::env::current_dir() {
        current_dir.to_string_lossy().hash(&mut hasher);
    }

    std::process::id().hash(&mut hasher);
    let unique_id = format!("{:016x}", hasher.finish())[..16].to_string();

    // Get OS and architecture
    let os = std::env::consts::OS.to_string();
    let arch = std::env::consts::ARCH.to_string();

    let payload = InstallPayload {
        version: version.to_string(),
        os: os.clone(),
        arch: arch.clone(),
        unique_id,
    };

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;

    let url = "https://klava.zatsepin.dev/telemetry/install";

    tracing::info!("Sending telemetry install event...");

    let response = client
        .post(url)
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
        .await;

    match response {
        Ok(resp) if resp.status().is_success() => {
            tracing::info!("✓ Telemetry install event sent successfully");
            Ok(true)
        }
        Ok(resp) => {
            let status = resp.status();
            tracing::debug!("Telemetry server returned status: {}", status);
            Ok(true)
        }
        Err(e) => {
            tracing::debug!("Failed to send telemetry install event: {}", e);
            Ok(true) // Still return true so we don't keep trying
        }
    }
}

#[cfg(not(feature = "telemetry"))]
pub async fn send_install_event(_version: &str) -> anyhow::Result<bool> {
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compare_versions_new_version_available() {
        assert!(compare_versions("0.2.0", "0.3.0"));
        assert!(compare_versions("0.2.9", "0.3.0"));
        assert!(compare_versions("0.9.9", "1.0.0"));
    }

    #[test]
    fn test_compare_versions_no_update_needed() {
        assert!(!compare_versions("0.2.0", "0.2.0"));
        assert!(!compare_versions("0.3.0", "0.2.0"));
        assert!(!compare_versions("1.0.0", "0.9.9"));
    }

    #[test]
    fn test_compare_versions_edge_cases() {
        assert!(compare_versions("0.0.1", "0.2.0"));
        assert!(compare_versions("0.2.5", "0.2.6"));
        assert!(!compare_versions("0.2.0", "0.0.1"));
        assert!(!compare_versions("0.2.6", "0.2.5"));
        assert!(!compare_versions("0.2.5", "0.2.5"));
    }
}
