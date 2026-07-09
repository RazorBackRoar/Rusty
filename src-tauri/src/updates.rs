//! GitHub Releases update check — razorcore-api-spec `updates` contract.
//! Uses blocking HTTP so we do not pull tokio into the Tauri app.

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::data_dir::app_cache_dir;
use crate::error::{AppError, AppResult};

const GITHUB_ORG: &str = "RazorBackRoar";
const GITHUB_REPO: &str = "Rusty";
const CACHE_DURATION_SECS: u64 = 3600;
const USER_AGENT: &str = "rusty-update-checker/1.0";

#[derive(Debug, Clone, Serialize)]
pub struct UpdateResult {
    pub current_version: String,
    pub latest_version: String,
    pub update_available: bool,
    pub download_url: Option<String>,
    pub release_notes: Option<String>,
    pub release_date: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    html_url: Option<String>,
    body: Option<String>,
    published_at: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CachePayload {
    timestamp: u64,
    latest_version: String,
    download_url: Option<String>,
    release_notes: Option<String>,
    release_date: Option<String>,
}

fn cache_file() -> PathBuf {
    app_cache_dir().join("update_check.json")
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Compare `major.minor.patch` (optional leading `v`). Returns -1 / 0 / 1.
pub fn compare_versions(a: &str, b: &str) -> i32 {
    let pa = parse_version(a);
    let pb = parse_version(b);
    match (pa, pb) {
        (Some(va), Some(vb)) => {
            if va < vb {
                -1
            } else if va > vb {
                1
            } else {
                0
            }
        }
        _ => 0,
    }
}

fn parse_version(version: &str) -> Option<(u64, u64, u64)> {
    let cleaned = version.trim().trim_start_matches('v');
    let mut parts = cleaned.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().unwrap_or(0);
    let patch = parts
        .next()
        .unwrap_or("0")
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .unwrap_or(0);
    Some((major, minor, patch))
}

fn read_cache() -> Option<CachePayload> {
    let path = cache_file();
    let raw = fs::read_to_string(path).ok()?;
    let payload: CachePayload = serde_json::from_str(&raw).ok()?;
    if now_secs().saturating_sub(payload.timestamp) > CACHE_DURATION_SECS {
        return None;
    }
    Some(payload)
}

fn write_cache(payload: &CachePayload) {
    let dir = app_cache_dir();
    let _ = fs::create_dir_all(&dir);
    if let Ok(raw) = serde_json::to_string(payload) {
        let _ = fs::write(cache_file(), raw);
    }
}

fn fetch_latest_release() -> AppResult<GitHubRelease> {
    let url = format!("https://api.github.com/repos/{GITHUB_ORG}/{GITHUB_REPO}/releases/latest");
    let response = reqwest::blocking::Client::new()
        .get(&url)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/vnd.github.v3+json")
        .send()
        .map_err(|e| AppError::BadInput(format!("update check network error: {e}")))?;

    if !response.status().is_success() {
        return Err(AppError::BadInput(format!(
            "GitHub Releases returned HTTP {}",
            response.status()
        )));
    }

    response
        .json()
        .map_err(|e| AppError::BadInput(format!("update check parse error: {e}")))
}

/// Check GitHub Releases for a newer version of Rusty.
pub fn check_for_updates(current_version: &str) -> UpdateResult {
    if let Some(cached) = read_cache() {
        let update_available = compare_versions(current_version, &cached.latest_version) < 0;
        return UpdateResult {
            current_version: current_version.to_string(),
            latest_version: cached.latest_version,
            update_available,
            download_url: cached.download_url,
            release_notes: cached.release_notes,
            release_date: cached.release_date,
            error: None,
        };
    }

    match fetch_latest_release() {
        Ok(release) => {
            let latest = release.tag_name.trim_start_matches('v').to_string();
            let update_available = compare_versions(current_version, &latest) < 0;
            write_cache(&CachePayload {
                timestamp: now_secs(),
                latest_version: latest.clone(),
                download_url: release.html_url.clone(),
                release_notes: release.body.clone(),
                release_date: release.published_at.clone(),
            });
            UpdateResult {
                current_version: current_version.to_string(),
                latest_version: latest,
                update_available,
                download_url: release.html_url,
                release_notes: release.body,
                release_date: release.published_at,
                error: None,
            }
        }
        Err(err) => UpdateResult {
            current_version: current_version.to_string(),
            latest_version: current_version.to_string(),
            update_available: false,
            download_url: None,
            release_notes: None,
            release_date: None,
            error: Some(err.to_string()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compare_versions_orders_semver() {
        assert_eq!(compare_versions("0.2.0", "0.2.1"), -1);
        assert_eq!(compare_versions("v0.3.0", "0.2.9"), 1);
        assert_eq!(compare_versions("1.0.0", "v1.0.0"), 0);
    }
}
