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
struct VersionResponse {
    pub latest_version: String,
    pub published_at: String,
}

#[cfg(feature = "telemetry")]
pub async fn check_for_update(current_version: &str) -> Result<bool> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?;

    let url = "https://klava.zatsepin.dev/cli/version";

    let response = client.get(url).send().await?;

    if !response.status().is_success() {
        return Ok(false);
    }

    let version_data: VersionResponse = response.json().await?;

    // Simple semver comparison
    fn parse_version(v: &str) -> Vec<u32> {
        v.split('.').map(|s| s.parse().unwrap_or(0)).collect()
    }

    let current = parse_version(current_version);
    let latest = parse_version(&version_data.latest_version);

    let comparison = current
        .iter()
        .zip(latest.iter())
        .map(|(a, b)| a.cmp(b))
        .find(|&ord| ord != Ordering::Equal)
        .unwrap_or_else(|| current.len().cmp(&latest.len()));

    Ok(comparison == Ordering::Less)
}

#[cfg(not(feature = "telemetry"))]
pub async fn check_for_update(_: &str) -> Result<bool> {
    Ok(false)
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
