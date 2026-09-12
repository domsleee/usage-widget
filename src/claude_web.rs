//! Claude's next charge date is only served to claude.ai browser sessions, not to
//! the Claude Code OAuth token. On macOS, with `[claude] browser_cookies = true`,
//! this reads the claude.ai session cookie from a Chromium browser's cookie store,
//! decrypts it with that browser's key from the login keychain, and asks claude.ai
//! for the subscription details.

use crate::providers::Cycle;

#[cfg(target_os = "macos")]
pub use mac::cycle;

/// Chromium on Windows seals its cookies to the browser itself, so macOS only.
#[cfg(not(target_os = "macos"))]
pub fn cycle() -> Result<Cycle, String> {
    Err("reading browser cookies is only supported on macOS".into())
}

/// A claude.ai timestamp: RFC 3339, a bare date (taken as midnight UTC), or unix
/// seconds or milliseconds.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn timestamp(v: &serde_json::Value) -> Option<i64> {
    use crate::timeutil::parse_rfc3339;
    match v {
        serde_json::Value::Number(n) => n
            .as_i64()
            .map(|t| if t > 100_000_000_000 { t / 1000 } else { t }),
        serde_json::Value::String(s) => {
            parse_rfc3339(s).or_else(|| parse_rfc3339(&format!("{s}T00:00:00Z")))
        }
        _ => None,
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use super::{Cycle, timestamp};
    use serde_json::Value;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::Duration;

    /// Chromium browsers: profile root under ~/Library/Application Support, and the
    /// keychain item holding the key their cookies are encrypted with.
    const BROWSERS: [(&str, &str); 4] = [
        ("Google/Chrome", "Chrome Safe Storage"),
        ("Arc/User Data", "Arc Safe Storage"),
        ("BraveSoftware/Brave-Browser", "Brave Safe Storage"),
        ("Microsoft Edge", "Microsoft Edge Safe Storage"),
    ];

    /// claude.ai sits behind Cloudflare, which turns away non-browser user agents.
    const BROWSER_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
        AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";

    const QUERY: &str = "select 'version', value from meta where key = 'version'; \
        select name, hex(encrypted_value) from cookies \
        where host_key like '%claude.ai' and name in ('sessionKey', 'lastActiveOrg');";

    /// When the plan next renews, or when it ends if cancelled.
    pub fn cycle() -> Result<Cycle, String> {
        let (session, org) = session()?;
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(25)))
            .http_status_as_error(false)
            .user_agent(BROWSER_UA)
            .build()
            .into();
        let url = format!("https://claude.ai/api/organizations/{org}/subscription_details");
        let mut resp = agent
            .get(&url)
            .header("Accept", "application/json")
            .header("Cookie", &format!("sessionKey={session}"))
            .call()
            .map_err(|e| format!("request failed: {e}"))?;
        match resp.status().as_u16() {
            200 => {}
            s => return Err(format!("claude.ai refused the browser session (HTTP {s})")),
        }
        let text = resp
            .body_mut()
            .read_to_string()
            .map_err(|e| format!("read failed: {e}"))?;
        let json: Value =
            serde_json::from_str(&text).map_err(|e| format!("bad JSON from claude.ai: {e}"))?;

        if let Some(at) = timestamp(&json["plan_ending_at"]) {
            return Ok(Cycle {
                date_only: false,
                verb: "ends".into(),
                at,
            });
        }
        timestamp(&json["next_charge_at"])
            .or_else(|| timestamp(&json["next_charge_date"]))
            .map(|at| Cycle {
                date_only: false,
                verb: "renews".into(),
                at,
            })
            .ok_or_else(|| "no next charge date from claude.ai".into())
    }

    /// The claude.ai `sessionKey` and `lastActiveOrg` cookies from the first browser
    /// profile that has them.
    fn session() -> Result<(String, String), String> {
        let base = dirs::home_dir()
            .ok_or("cannot resolve home directory")?
            .join("Library/Application Support");
        for (dir, service) in BROWSERS {
            let Ok(entries) = std::fs::read_dir(base.join(dir)) else {
                continue;
            };
            let mut stores: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path().join("Cookies"))
                .filter(|p| p.is_file())
                .collect();
            stores.sort(); // "Default" before "Profile 1"
            for store in stores {
                let Ok((version, cookies)) = read_cookies(&store) else {
                    continue;
                };
                let (Some(session), Some(org)) =
                    (cookies.get("sessionKey"), cookies.get("lastActiveOrg"))
                else {
                    continue;
                };
                let key = browser_key(service)?;
                let decrypt =
                    |v| decrypt(v, &key, version).ok_or("cannot decrypt the claude.ai cookie");
                return Ok((decrypt(session)?, decrypt(org)?));
            }
        }
        Err("no claude.ai login in Chrome, Arc, Brave or Edge".into())
    }

    /// Reads the encrypted claude.ai cookies with the system `sqlite3`, from a copy
    /// because the browser keeps its store locked. Also returns the schema version.
    fn read_cookies(store: &Path) -> Result<(u32, HashMap<String, Vec<u8>>), String> {
        let dir = std::env::temp_dir().join(format!("usage-widget-{}", std::process::id()));
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        let copy = dir.join("Cookies");
        let output = (|| {
            std::fs::copy(store, &copy)
                .map_err(|e| format!("cannot copy {}: {e}", store.display()))?;
            let wal = store.with_file_name("Cookies-wal");
            if wal.is_file() {
                let _ = std::fs::copy(&wal, dir.join("Cookies-wal"));
            }
            Command::new("sqlite3")
                .arg(&copy)
                .arg(QUERY)
                .output()
                .map_err(|e| format!("cannot run sqlite3: {e}"))
        })();
        let _ = std::fs::remove_dir_all(&dir);

        let mut version = 0;
        let mut cookies = HashMap::new();
        for line in String::from_utf8_lossy(&output?.stdout).lines() {
            match line.split_once('|') {
                Some(("version", v)) => version = v.parse().unwrap_or(0),
                Some((name, hex)) => {
                    if let Some(bytes) = unhex(hex) {
                        cookies.insert(name.to_string(), bytes);
                    }
                }
                None => {}
            }
        }
        Ok((version, cookies))
    }

    /// The AES key Chromium derives from its "Safe Storage" keychain password.
    /// macOS asks the user once whether `security` may read it.
    fn browser_key(service: &str) -> Result<[u8; 16], String> {
        let out = Command::new("security")
            .args(["find-generic-password", "-w", "-s", service])
            .output()
            .map_err(|e| format!("cannot run `security` ({e})"))?;
        if !out.status.success() {
            return Err(format!(
                "no keychain access to \"{service}\"; allow it when macOS asks"
            ));
        }
        let password = String::from_utf8_lossy(&out.stdout);
        let mut key = [0u8; 16];
        pbkdf2::pbkdf2_hmac::<sha1::Sha1>(password.trim().as_bytes(), b"saltysalt", 1003, &mut key);
        Ok(key)
    }

    /// Chromium's macOS cookie format: "v10" + AES-128-CBC with a blank IV. Since
    /// cookie store version 24 the plaintext starts with SHA-256 of the host.
    fn decrypt(value: &[u8], key: &[u8; 16], version: u32) -> Option<String> {
        use cbc::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
        let mut buf = value.strip_prefix(b"v10")?.to_vec();
        let plain = cbc::Decryptor::<aes::Aes128>::new_from_slices(key, &[b' '; 16])
            .ok()?
            .decrypt_padded_mut::<Pkcs7>(&mut buf)
            .ok()?;
        let plain = if version >= 24 {
            plain.get(32..)?
        } else {
            plain
        };
        String::from_utf8(plain.to_vec()).ok()
    }

    fn unhex(s: &str) -> Option<Vec<u8>> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
            .collect()
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use cbc::cipher::{BlockEncryptMut, KeyIvInit, block_padding::Pkcs7};

        #[test]
        fn decrypts_chromium_cookie() {
            let key = [7u8; 16];
            let mut plain = vec![0xAB; 32]; // stands in for sha256(host)
            plain.extend_from_slice(b"sk-ant-sid01-test");
            let len = plain.len();
            plain.resize(len + 16, 0);
            let mut value = b"v10".to_vec();
            value.extend_from_slice(
                cbc::Encryptor::<aes::Aes128>::new_from_slices(&key, &[b' '; 16])
                    .unwrap()
                    .encrypt_padded_mut::<Pkcs7>(&mut plain, len)
                    .unwrap(),
            );
            assert_eq!(
                decrypt(&value, &key, 24).as_deref(),
                Some("sk-ant-sid01-test")
            );
            assert_eq!(decrypt(b"v11nope", &key, 24), None);
            assert_eq!(unhex("00ff10"), Some(vec![0, 255, 16]));
            assert_eq!(unhex("0g"), None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::timestamp;
    use serde_json::json;

    #[test]
    fn claude_timestamps() {
        // 2026-09-15T20:00:00Z is 1_789_502_400; the 23rd is 7d 4h later.
        assert_eq!(timestamp(&json!("2026-09-23")), Some(1_790_121_600));
        assert_eq!(
            timestamp(&json!("2026-09-23T08:01:16Z")),
            Some(1_790_150_476)
        );
        assert_eq!(timestamp(&json!(1_790_150_476)), Some(1_790_150_476));
        assert_eq!(
            timestamp(&json!(1_790_150_476_000_i64)),
            Some(1_790_150_476)
        );
        assert_eq!(timestamp(&json!(null)), None);
    }
}
