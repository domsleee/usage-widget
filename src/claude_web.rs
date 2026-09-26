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
pub fn cycle(_org: Option<&str>) -> Result<Cycle, String> {
    Err("reading browser cookies is only supported on macOS".into())
}

/// A claude.ai timestamp: RFC 3339, unix seconds or milliseconds, or a bare date.
/// A bare date is a calendar day, not a time, so it becomes local midnight and
/// the second value says so (`Cycle::date_only`).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn timestamp(v: &serde_json::Value) -> Option<(i64, bool)> {
    use crate::timeutil::parse_rfc3339;
    use chrono::{Local, NaiveDate, TimeZone};
    match v {
        serde_json::Value::Number(n) => n
            .as_i64()
            .map(|t| (if t > 100_000_000_000 { t / 1000 } else { t }, false)),
        serde_json::Value::String(s) => parse_rfc3339(s).map(|t| (t, false)).or_else(|| {
            let day = NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()?;
            let midnight = Local
                .from_local_datetime(&day.and_hms_opt(0, 0, 0)?)
                .earliest()?;
            Some((midnight.timestamp(), true))
        }),
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

    /// When the plan next renews, or when it ends if cancelled. `org` is Claude
    /// Code's organization; each signed-in browser profile is tried until one can
    /// see it, so a browser logged in to another account is never used. Without
    /// it, the first profile's last active organization is used.
    pub fn cycle(org: Option<&str>) -> Result<Cycle, String> {
        let mut result = None;
        each_session(|session, active_org| {
            let r = subscription(&session, org.unwrap_or(&active_org));
            // Without an organization to match, the first login is the answer.
            let done = r.is_ok() || org.is_none();
            result = Some(r);
            done
        })?;
        match result {
            Some(Err(e)) if org.is_some() => Err(format!(
                "no browser login can see Claude Code's organization ({e})"
            )),
            Some(r) => r,
            None => Err("no claude.ai login in Chrome, Arc, Brave or Edge".into()),
        }
    }

    fn subscription(session: &str, org: &str) -> Result<Cycle, String> {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .tls_config(crate::providers::tls())
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

        if let Some((at, date_only)) = timestamp(&json["plan_ending_at"]) {
            return Ok(Cycle {
                date_only,
                verb: "ends".into(),
                at,
            });
        }
        timestamp(&json["next_charge_at"])
            .or_else(|| timestamp(&json["next_charge_date"]))
            .map(|(at, date_only)| Cycle {
                date_only,
                verb: "renews".into(),
                at,
            })
            .ok_or_else(|| "no next charge date from claude.ai".into())
    }

    /// Calls `f` with the claude.ai `sessionKey` and `lastActiveOrg` cookies of each
    /// browser profile that has them, until it returns true. Each browser's key is
    /// read from the keychain only when one of its profiles is reached.
    fn each_session(mut f: impl FnMut(String, String) -> bool) -> Result<(), String> {
        let base = std::env::home_dir()
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
            let mut key = None;
            for store in stores {
                let Ok((version, cookies)) = read_cookies(&store) else {
                    continue;
                };
                let (Some(session), Some(org)) =
                    (cookies.get("sessionKey"), cookies.get("lastActiveOrg"))
                else {
                    continue;
                };
                if key.is_none() {
                    key = Some(browser_key(service)?);
                }
                let key = key.as_ref().expect("set above");
                let decrypt =
                    |v| decrypt(v, key, version).ok_or("cannot decrypt the claude.ai cookie");
                if f(decrypt(session)?, decrypt(org)?) {
                    return Ok(());
                }
            }
        }
        Ok(())
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
        pbkdf2_sha1(password.trim().as_bytes(), b"saltysalt", 1003)
            .ok_or_else(|| "cannot derive the browser's cookie key".into())
    }

    /// Chromium's macOS cookie format: "v10" + AES-128-CBC with a blank IV. Since
    /// cookie store version 24 the plaintext starts with SHA-256 of the host.
    fn decrypt(value: &[u8], key: &[u8; 16], version: u32) -> Option<String> {
        let plain = aes_cbc(K_CC_DECRYPT, key, value.strip_prefix(b"v10")?)?;
        let plain = if version >= 24 {
            plain.get(32..)?
        } else {
            &plain
        };
        String::from_utf8(plain.to_vec()).ok()
    }

    // The crypto comes from CommonCrypto, which is part of macOS.
    const K_CC_PBKDF2: u32 = 2;
    const K_CC_PRF_HMAC_SHA1: u32 = 1;
    #[cfg(test)]
    const K_CC_ENCRYPT: u32 = 0;
    const K_CC_DECRYPT: u32 = 1;
    const K_CC_ALGORITHM_AES: u32 = 0;
    const K_CC_OPTION_PKCS7_PADDING: u32 = 1;

    unsafe extern "C" {
        fn CCKeyDerivationPBKDF(
            algorithm: u32,
            password: *const u8,
            password_len: usize,
            salt: *const u8,
            salt_len: usize,
            prf: u32,
            rounds: u32,
            derived_key: *mut u8,
            derived_key_len: usize,
        ) -> i32;
        fn CCCrypt(
            op: u32,
            algorithm: u32,
            options: u32,
            key: *const u8,
            key_len: usize,
            iv: *const u8,
            data_in: *const u8,
            data_in_len: usize,
            data_out: *mut u8,
            data_out_available: usize,
            data_out_moved: *mut usize,
        ) -> i32;
    }

    fn pbkdf2_sha1(password: &[u8], salt: &[u8], rounds: u32) -> Option<[u8; 16]> {
        let mut key = [0u8; 16];
        // SAFETY: every pointer is paired with the length of the buffer it points to.
        let status = unsafe {
            CCKeyDerivationPBKDF(
                K_CC_PBKDF2,
                password.as_ptr(),
                password.len(),
                salt.as_ptr(),
                salt.len(),
                K_CC_PRF_HMAC_SHA1,
                rounds,
                key.as_mut_ptr(),
                key.len(),
            )
        };
        (status == 0).then_some(key)
    }

    /// AES-128-CBC with PKCS#7 padding and Chromium's IV of 16 spaces.
    fn aes_cbc(op: u32, key: &[u8; 16], data: &[u8]) -> Option<Vec<u8>> {
        let iv = [b' '; 16];
        // Padding adds at most one block.
        let mut out = vec![0u8; data.len() + 16];
        let mut moved = 0;
        // SAFETY: every pointer is paired with the length of the buffer it points to.
        let status = unsafe {
            CCCrypt(
                op,
                K_CC_ALGORITHM_AES,
                K_CC_OPTION_PKCS7_PADDING,
                key.as_ptr(),
                key.len(),
                iv.as_ptr(),
                data.as_ptr(),
                data.len(),
                out.as_mut_ptr(),
                out.len(),
                &mut moved,
            )
        };
        (status == 0).then(|| {
            out.truncate(moved);
            out
        })
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

        #[test]
        fn derives_keys_like_pbkdf2_hmac_sha1() {
            // RFC 6070, two rounds, first 16 bytes.
            assert_eq!(
                pbkdf2_sha1(b"password", b"salt", 2),
                unhex("ea6c014dc72d6f8ccd1ed92ace1d41f0")
                    .and_then(|k| k.try_into().ok())
            );
        }

        #[test]
        fn decrypts_chromium_cookie() {
            let key = [7u8; 16];
            let mut plain = vec![0xAB; 32]; // stands in for sha256(host)
            plain.extend_from_slice(b"sk-ant-sid01-test");
            let mut value = b"v10".to_vec();
            value.extend_from_slice(&aes_cbc(K_CC_ENCRYPT, &key, &plain).unwrap());
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
        use chrono::{Local, TimeZone};
        // A bare date is local midnight, whatever the time zone.
        let midnight = Local.with_ymd_and_hms(2026, 9, 23, 0, 0, 0).unwrap();
        assert_eq!(
            timestamp(&json!("2026-09-23")),
            Some((midnight.timestamp(), true))
        );
        assert_eq!(
            timestamp(&json!("2026-09-23T08:01:16Z")),
            Some((1_790_150_476, false))
        );
        assert_eq!(
            timestamp(&json!(1_790_150_476)),
            Some((1_790_150_476, false))
        );
        assert_eq!(
            timestamp(&json!(1_790_150_476_000_i64)),
            Some((1_790_150_476, false))
        );
        assert_eq!(timestamp(&json!(null)), None);
    }
}
