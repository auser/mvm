use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};

/// Fetch a URL and return the response body as a string.
pub fn fetch_text(url: &str) -> Result<String> {
    let client = reqwest::blocking::Client::builder()
        .user_agent(concat!("mvm/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("Failed to build HTTP client")?;

    let resp = client
        .get(url)
        .send()
        .with_context(|| format!("HTTP request failed: {}", url))?;

    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("HTTP {} for {}", status, url);
    }

    resp.text()
        .with_context(|| format!("Failed to read response body from {}", url))
}

/// Fetch a URL and parse the response as JSON.
pub fn fetch_json(url: &str) -> Result<serde_json::Value> {
    let client = reqwest::blocking::Client::builder()
        .user_agent(concat!("mvm/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("Failed to build HTTP client")?;

    let resp = client
        .get(url)
        .header("Accept", "application/json")
        .send()
        .with_context(|| format!("HTTP request failed: {}", url))?;

    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("HTTP {} for {}", status, url);
    }

    resp.json::<serde_json::Value>()
        .with_context(|| format!("Failed to parse JSON from {}", url))
}

/// Download a URL to a file on disk. Shows no progress (use for small files).
pub fn download_file(url: &str, dest: &Path) -> Result<()> {
    let client = reqwest::blocking::Client::builder()
        .user_agent(concat!("mvm/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .context("Failed to build HTTP client")?;

    let resp = client
        .get(url)
        .send()
        .with_context(|| format!("Download failed: {}", url))?;

    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("HTTP {} downloading {}", status, url);
    }

    let bytes = resp
        .bytes()
        .with_context(|| format!("Failed to read download body: {}", url))?;

    let mut file = std::fs::File::create(dest)
        .with_context(|| format!("Failed to create file: {}", dest.display()))?;

    file.write_all(&bytes)
        .with_context(|| format!("Failed to write to: {}", dest.display()))?;

    Ok(())
}

/// Return the current UTC timestamp in ISO 8601 format.
pub fn utc_now() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_utc_now_format() {
        let ts = utc_now();
        // Should match: 2024-01-15T12:30:45Z
        assert!(ts.ends_with('Z'));
        assert_eq!(ts.len(), 20);
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[7..8], "-");
        assert_eq!(&ts[10..11], "T");
        assert_eq!(&ts[13..14], ":");
        assert_eq!(&ts[16..17], ":");
    }
}
