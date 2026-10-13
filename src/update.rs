//! Self-update from GitHub releases. Each release has a prebuilt
//! `usage-widget.exe` attached by `.github/workflows/release.yml`; a newer one
//! is downloaded and swapped in for the running exe, then the widget restarts.

use crate::providers::agent;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

const LATEST_URL: &str = "https://api.github.com/repos/pepsi-enjoyer/usage-widget/releases/latest";
const ASSET_NAME: &str = "usage-widget.exe";
const MAX_DOWNLOAD_BYTES: u64 = 64 * 1024 * 1024;
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
/// Wait before the first automatic check so it does not compete with startup.
pub const FIRST_CHECK_AFTER: Duration = Duration::from_secs(60);
pub const CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);

pub enum Outcome {
    UpToDate,
    /// The new exe is in place; the caller should restart.
    Installed,
    /// A newer release exists but its exe has not been attached yet.
    NotReady(String),
}

#[derive(Debug)]
struct Release {
    version: String,
    asset: Option<Asset>,
}

#[derive(Debug)]
struct Asset {
    url: String,
    size: u64,
    sha256: Option<String>,
}

/// Set once this process has installed an update, so later checks do not repeat it.
/// The mutex also stops the automatic and menu checks running at once.
static INSTALLED: Mutex<bool> = Mutex::new(false);

/// Checks GitHub for a newer release and, if there is one, installs it over the
/// running exe.
pub fn check_and_install() -> Result<Outcome, String> {
    let mut installed = INSTALLED.lock().unwrap_or_else(|e| e.into_inner());
    if *installed {
        return Ok(Outcome::Installed);
    }
    let release = latest()?;
    if !is_newer(&release.version, CURRENT) {
        return Ok(Outcome::UpToDate);
    }
    let Some(asset) = release.asset else {
        return Ok(Outcome::NotReady(release.version));
    };
    let bytes = download(&asset)?;
    install(&bytes)?;
    *installed = true;
    Ok(Outcome::Installed)
}

/// Starts the freshly installed exe. It waits for this process to exit before
/// it opens its window.
pub fn restart() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    std::process::Command::new(exe)
        .arg("--updated")
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("could not start the new version: {e}"))
}

/// Removes the previous exe left behind by an update. Windows will not delete a
/// running exe, so this happens on the next start.
pub fn cleanup() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(sibling(&exe, "old"));
        let _ = std::fs::remove_file(sibling(&exe, "new"));
    }
}

/// Automatic updates are on for release builds unless `USAGE_WIDGET_AUTO_UPDATE=0`.
pub fn auto_enabled() -> bool {
    !cfg!(debug_assertions)
        && std::env::var("USAGE_WIDGET_AUTO_UPDATE").map_or(true, |v| v.trim() != "0")
}

fn latest() -> Result<Release, String> {
    let mut resp = agent()
        .get(LATEST_URL)
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| format!("request failed: {e}"))?;
    let status = resp.status().as_u16();
    match status {
        200 => {}
        403 | 429 => return Err("GitHub rate limit reached, try again later".into()),
        404 => return Err("no releases found".into()),
        _ => return Err(format!("GitHub returned HTTP {status}")),
    }
    let json: Value = resp
        .body_mut()
        .read_json()
        .map_err(|e| format!("bad response from GitHub: {e}"))?;
    parse_release(&json)
}

fn parse_release(json: &Value) -> Result<Release, String> {
    let tag = json["tag_name"]
        .as_str()
        .ok_or("release has no tag")?
        .trim_start_matches('v')
        .to_string();
    let asset = json["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|a| a["name"].as_str() == Some(ASSET_NAME))
        .and_then(|a| {
            Some(Asset {
                url: a["browser_download_url"].as_str()?.to_string(),
                size: a["size"].as_u64()?,
                sha256: a["digest"]
                    .as_str()
                    .and_then(|d| d.strip_prefix("sha256:"))
                    .map(|d| d.to_ascii_lowercase()),
            })
        });
    Ok(Release {
        version: tag,
        asset,
    })
}

fn parse_version(v: &str) -> Option<Vec<u64>> {
    v.trim_start_matches('v')
        .split('.')
        .map(|p| p.parse().ok())
        .collect()
}

/// True when `latest` is a higher dotted version than `current`. Anything that
/// does not parse (such as a pre-release tag) never counts as newer.
fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(mut l), Some(mut c)) => {
            let len = l.len().max(c.len());
            l.resize(len, 0);
            c.resize(len, 0);
            l > c
        }
        _ => false,
    }
}

fn download(asset: &Asset) -> Result<Vec<u8>, String> {
    if asset.size > MAX_DOWNLOAD_BYTES {
        return Err(format!(
            "download is unexpectedly large ({} bytes)",
            asset.size
        ));
    }
    let mut resp = agent()
        .get(&asset.url)
        .header("Accept", "application/octet-stream")
        .config()
        .timeout_global(Some(Duration::from_secs(300)))
        .build()
        .call()
        .map_err(|e| format!("download failed: {e}"))?;
    let status = resp.status().as_u16();
    if status != 200 {
        return Err(format!("download failed: HTTP {status}"));
    }
    let bytes = resp
        .body_mut()
        .with_config()
        .limit(MAX_DOWNLOAD_BYTES)
        .read_to_vec()
        .map_err(|e| format!("download failed: {e}"))?;
    verify(&bytes, asset)?;
    Ok(bytes)
}

fn verify(bytes: &[u8], asset: &Asset) -> Result<(), String> {
    if bytes.len() as u64 != asset.size {
        return Err(format!(
            "download was incomplete ({} of {} bytes)",
            bytes.len(),
            asset.size
        ));
    }
    if let Some(expected) = &asset.sha256 {
        let actual: String = Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        if &actual != expected {
            return Err("download did not match its checksum".into());
        }
    }
    if !bytes.starts_with(b"MZ") {
        return Err("download is not a Windows program".into());
    }
    Ok(())
}

/// Windows lets a running exe be renamed but not overwritten, so the current exe
/// moves aside to `.old` and the download takes its place.
fn install(bytes: &[u8]) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let new = sibling(&exe, "new");
    let old = sibling(&exe, "old");
    std::fs::write(&new, bytes).map_err(|e| format!("could not save the update: {e}"))?;
    let _ = std::fs::remove_file(&old);
    if let Err(e) = std::fs::rename(&exe, &old) {
        let _ = std::fs::remove_file(&new);
        return Err(format!("could not replace {}: {e}", exe.display()));
    }
    if let Err(e) = std::fs::rename(&new, &exe) {
        let _ = std::fs::rename(&old, &exe);
        let _ = std::fs::remove_file(&new);
        return Err(format!("could not replace {}: {e}", exe.display()));
    }
    Ok(())
}

/// `usage-widget.exe` -> `usage-widget.exe.<suffix>`.
fn sibling(exe: &Path, suffix: &str) -> PathBuf {
    let mut s = exe.as_os_str().to_owned();
    s.push(".");
    s.push(suffix);
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn compares_versions() {
        assert!(is_newer("1.0.3", "1.0.2"));
        assert!(is_newer("v1.1", "1.0.9"));
        assert!(is_newer("1.10.0", "1.9.0"));
        assert!(is_newer("2.0.0", "1.99.99"));
        assert!(!is_newer("1.0.2", "1.0.2"));
        assert!(!is_newer("1.0", "1.0.0"));
        assert!(!is_newer("1.0.1", "1.0.2"));
        assert!(!is_newer("1.0.3-beta", "1.0.2"));
        assert!(!is_newer("", "1.0.2"));
    }

    #[test]
    fn picks_the_exe_asset() {
        let release = parse_release(&json!({
            "tag_name": "v1.0.3",
            "assets": [
                {"name": "notes.txt", "browser_download_url": "https://x/notes.txt", "size": 3},
                {
                    "name": "usage-widget.exe",
                    "browser_download_url": "https://x/usage-widget.exe",
                    "size": 42,
                    "digest": "sha256:ABCDEF"
                }
            ]
        }))
        .unwrap();
        assert_eq!(release.version, "1.0.3");
        let asset = release.asset.unwrap();
        assert_eq!(asset.url, "https://x/usage-widget.exe");
        assert_eq!(asset.size, 42);
        assert_eq!(asset.sha256.as_deref(), Some("abcdef"));
    }

    #[test]
    fn release_without_exe_has_no_asset() {
        let release = parse_release(&json!({"tag_name": "v1.0.3", "assets": []})).unwrap();
        assert!(release.asset.is_none());
        assert!(parse_release(&json!({"assets": []})).is_err());
    }

    #[test]
    fn verifies_downloads() {
        let bytes = b"MZ rest of exe";
        let sha: String = Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let asset = |size, sha256: Option<&str>| Asset {
            url: String::new(),
            size,
            sha256: sha256.map(str::to_string),
        };
        let len = bytes.len() as u64;
        assert!(verify(bytes, &asset(len, Some(&sha))).is_ok());
        assert!(verify(bytes, &asset(len, None)).is_ok());
        assert!(verify(bytes, &asset(len + 1, Some(&sha))).is_err());
        assert!(verify(bytes, &asset(len, Some("00"))).is_err());
        assert!(verify(b"PK zip", &asset(6, None)).is_err());
    }

    #[test]
    fn sibling_appends_suffix() {
        let p = sibling(Path::new(r"C:\bin\usage-widget.exe"), "old");
        assert_eq!(p, PathBuf::from(r"C:\bin\usage-widget.exe.old"));
    }
}
