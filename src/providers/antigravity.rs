//! Google Antigravity quota, read with the sign-in the Antigravity CLI (`agy`)
//! keeps in the OS credential store, from the same endpoints `agy /usage` uses.
//!
//! agy itself is never run: a signed-out agy opens a browser to sign in. With no
//! stored sign-in, or an account without Antigravity, the row stays hidden.
//!
//! Refreshing agy's hour-long access token needs agy's OAuth client secret. It
//! ships inside agy, so it is read from the installed agy rather than kept here.

use super::{FetchError, Meter, Problem, Unit, Usage, agent};
use crate::timeutil::{now_unix, parse_rfc3339};
use serde_json::{Value, json};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const BASE: &str = "https://daily-cloudcode-pa.googleapis.com/v1internal";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
/// agy's OAuth client. Its secret is found in the agy executable.
const CLIENT_ID: &str = "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com";
const SECRET_PREFIX: &[u8] = b"GOCSPX-";
const SECRET_LEN: usize = 35;
/// The quota endpoints answer for Antigravity only when the client says it is.
const USER_AGENT: &str = "antigravity";
/// go-keyring's service and user for agy's sign-in.
const KEYRING_SERVICE: &str = "gemini";
const KEYRING_USER: &str = "antigravity";
const SIGN_IN: &str = "Sign in with the Antigravity CLI (agy) to show its quota.";
const SIGN_IN_AGAIN: &str = "Antigravity sign-in expired; run agy and sign in again.";

/// An access token refreshed from agy's refresh token, kept in memory only so
/// agy's stored sign-in is never changed: (refresh token, access token, expiry).
static REFRESHED: Mutex<Option<(String, String, i64)>> = Mutex::new(None);

/// The client secrets found in one agy executable, the working one first once
/// known. Rescanned when agy is updated.
struct Secrets {
    exe: PathBuf,
    stamp: Option<(u64, std::time::SystemTime)>,
    found: Vec<String>,
}

static SECRETS: Mutex<Option<Secrets>> = Mutex::new(None);

pub(super) fn fetch() -> Result<Usage, FetchError> {
    let stored = parse_sign_in(&stored_sign_in()?)?;
    let token = access_token(&stored)?;
    let auth = format!("Bearer {token}");
    let load = post(
        &auth,
        "loadCodeAssist",
        &json!({"metadata": {"ideType": "ANTIGRAVITY", "pluginType": "GEMINI"}}),
    )?;
    let project = project(&load).ok_or_else(|| {
        FetchError::new(
            Problem::NotAvailable,
            "Antigravity is not set up for this account; finish setting it up in agy.",
        )
    })?;
    let body = json!({ "project": project });
    let summary = post(&auth, "retrieveUserQuotaSummary", &body)?;
    // Gemini's limit only shows in each model's quota, so it is read from there.
    let models = post(&auth, "fetchAvailableModels", &body).ok();
    parse(&load, &summary, models.as_ref(), now_unix())
}

#[derive(Debug, PartialEq)]
struct SignIn {
    access_token: String,
    refresh_token: String,
    expires_at: Option<i64>,
}

fn parse_sign_in(text: &str) -> Result<SignIn, FetchError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(FetchError::new(Problem::NotAvailable, SIGN_IN));
    }
    let value: Value = serde_json::from_str(trimmed)
        .map_err(|e| FetchError::from(format!("bad Antigravity sign-in entry: {e}")))?;
    let token = &value["token"];
    let field = |name: &str| token[name].as_str().unwrap_or_default().to_string();
    let sign_in = SignIn {
        access_token: field("access_token"),
        refresh_token: field("refresh_token"),
        expires_at: token["expiry"].as_str().and_then(parse_rfc3339),
    };
    if sign_in.access_token.is_empty() && sign_in.refresh_token.is_empty() {
        return Err(FetchError::new(Problem::NotAvailable, SIGN_IN));
    }
    Ok(sign_in)
}

/// The stored access token while it has a minute left, else one refreshed from
/// the stored refresh token (and reused until it too runs out).
fn access_token(stored: &SignIn) -> Result<String, FetchError> {
    let now = now_unix();
    if !stored.access_token.is_empty() && stored.expires_at.is_some_and(|at| at - 60 > now) {
        return Ok(stored.access_token.clone());
    }
    let mut cache = REFRESHED.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((refresh, access, at)) = cache.as_ref()
        && *refresh == stored.refresh_token
        && at - 60 > now
    {
        return Ok(access.clone());
    }
    if stored.refresh_token.is_empty() {
        return Err(FetchError::new(Problem::TokenExpired, SIGN_IN_AGAIN));
    }
    let (access, expires_in) = refresh(&stored.refresh_token)?;
    *cache = Some((
        stored.refresh_token.clone(),
        access.clone(),
        now + expires_in,
    ));
    Ok(access)
}

/// A new access token from agy's refresh token, trying each client secret found
/// in agy until Google accepts one, and remembering which it was.
fn refresh(refresh_token: &str) -> Result<(String, i64), FetchError> {
    let exe = executable().ok_or_else(|| {
        FetchError::new(
            Problem::TokenExpired,
            "Antigravity sign-in expired and agy was not found to renew it; run agy.",
        )
    })?;
    let mut secrets = SECRETS.lock().unwrap_or_else(|e| e.into_inner());
    let stamp = std::fs::metadata(&exe)
        .ok()
        .and_then(|m| Some((m.len(), m.modified().ok()?)));
    if secrets
        .as_ref()
        .is_none_or(|s| s.exe != exe || s.stamp != stamp)
    {
        let found = std::fs::File::open(&exe)
            .map(|f| find_secrets(f, 1 << 20))
            .map_err(|e| format!("cannot read agy ({e})"))?;
        *secrets = Some(Secrets { exe, stamp, found });
    }
    let found = &mut secrets.as_mut().expect("just set").found;
    for i in 0..found.len() {
        let mut resp = agent()
            .post(TOKEN_URL)
            .send_form([
                ("client_id", CLIENT_ID),
                ("client_secret", found[i].as_str()),
                ("refresh_token", refresh_token),
                ("grant_type", "refresh_token"),
            ])
            .map_err(|e| FetchError::new(Problem::Connection, format!("request failed: {e}")))?;
        let status = resp.status().as_u16();
        let json: Value = resp.body_mut().read_json().unwrap_or(Value::Null);
        if let Some(token) = refreshed(status, &json)? {
            found.swap(0, i);
            return Ok(token);
        }
    }
    Err(FetchError::new(
        Problem::TokenExpired,
        "Antigravity sign-in expired and could not be renewed; run agy.",
    ))
}

/// The new token, or `None` when Google rejected the client secret.
fn refreshed(status: u16, json: &Value) -> Result<Option<(String, i64)>, FetchError> {
    match status {
        200 => json["access_token"]
            .as_str()
            .filter(|t| !t.is_empty())
            .map(|t| Some((t.to_string(), json["expires_in"].as_i64().unwrap_or(3600))))
            .ok_or_else(|| {
                FetchError::new(Problem::BadResponse, "Google returned no access token.")
            }),
        _ if json["error"] == "invalid_client" => Ok(None),
        // invalid_grant: the sign-in was revoked or has lapsed.
        400 | 401 => Err(FetchError::new(Problem::TokenExpired, SIGN_IN_AGAIN)),
        429 => Err(FetchError::new(
            Problem::RateLimited,
            "Google rate limited the sign-in.",
        )),
        s => Err(FetchError::new(
            Problem::Service,
            format!("Google sign-in returned HTTP {s}"),
        )),
    }
}

fn secret_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_'
}

/// Every distinct Google client secret in `reader`, read `chunk` bytes at a time.
fn find_secrets(mut reader: impl Read, chunk: usize) -> Vec<String> {
    let mut found = Vec::new();
    let mut window: Vec<u8> = Vec::with_capacity(chunk + SECRET_LEN);
    let mut buf = vec![0; chunk];
    let mut done = false;
    while !done {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => done = true,
            Ok(n) => window.extend_from_slice(&buf[..n]),
        }
        let mut i = 0;
        while let Some(at) = find(&window[i..], SECRET_PREFIX) {
            let start = i + at;
            // Wait for more bytes unless the input has ended.
            if window.len() < start + SECRET_LEN && !done {
                break;
            }
            let candidate = &window[start..(start + SECRET_LEN).min(window.len())];
            // Strings in the executable run together, so what follows is ignored.
            let complete = candidate.len() == SECRET_LEN
                && candidate[SECRET_PREFIX.len()..]
                    .iter()
                    .all(|&b| secret_byte(b));
            if complete {
                let s = String::from_utf8_lossy(candidate).into_owned();
                if !found.contains(&s) {
                    found.push(s);
                }
            }
            i = start + 1;
        }
        // Keep a tail that could hold a secret split across reads.
        let keep_from = window
            .len()
            .saturating_sub(SECRET_LEN)
            .min(find(&window[i..], SECRET_PREFIX).map_or(usize::MAX, |at| i + at));
        window.drain(..keep_from);
    }
    found
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// The installed agy: on PATH, then where its installers put it.
fn executable() -> Option<PathBuf> {
    let name = if cfg!(windows) { "agy.exe" } else { "agy" };
    let path = std::env::var_os("PATH").unwrap_or_default();
    let on_path = std::env::split_paths(&path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name));
    let standard = if cfg!(windows) {
        crate::paths::data_local_dir().map(|dir| dir.join("agy/bin/agy.exe"))
    } else {
        std::env::home_dir().map(|dir| dir.join(".local/bin/agy"))
    };
    let homebrew = ["/opt/homebrew/bin/agy", "/usr/local/bin/agy"];
    on_path
        .chain(standard)
        .chain(
            homebrew
                .iter()
                .filter(|_| cfg!(target_os = "macos"))
                .map(PathBuf::from),
        )
        .find(|path| Path::new(path).is_file())
}

fn post(auth: &str, method: &str, body: &Value) -> Result<Value, FetchError> {
    let mut resp = agent()
        .post(&format!("{BASE}:{method}"))
        .header("Authorization", auth)
        .header("User-Agent", USER_AGENT)
        .send_json(body)
        .map_err(|e| FetchError::new(Problem::Connection, format!("request failed: {e}")))?;
    let status = resp.status().as_u16();
    let json: Value = resp.body_mut().read_json().unwrap_or(Value::Null);
    checked(status, json)
}

fn checked(status: u16, json: Value) -> Result<Value, FetchError> {
    match status {
        200 => Ok(json),
        401 => Err(FetchError::new(Problem::TokenRejected, SIGN_IN_AGAIN)),
        // No Antigravity licence on this account, or no project for it.
        403 => Err(FetchError::new(
            Problem::NotAvailable,
            json["error"]["message"]
                .as_str()
                .unwrap_or("This account has no Antigravity quota.")
                .chars()
                .take(300)
                .collect::<String>(),
        )),
        429 => Err(FetchError::new(
            Problem::RateLimited,
            "Antigravity rate limited the request.",
        )),
        s => Err(FetchError::new(
            Problem::Service,
            format!("Antigravity returned HTTP {s}"),
        )),
    }
}

fn project(load: &Value) -> Option<String> {
    let project = &load["cloudaicompanionProject"];
    project
        .as_str()
        .or_else(|| project["id"].as_str())
        .filter(|p| !p.is_empty())
        .map(String::from)
        .or_else(|| {
            ["GOOGLE_CLOUD_PROJECT", "GOOGLE_CLOUD_PROJECT_ID"]
                .into_iter()
                .find_map(|k| std::env::var(k).ok().filter(|p| !p.trim().is_empty()))
        })
}

/// The tier the account pays for, e.g. "Google AI Pro" or "Gemini Code Assist".
fn plan(load: &Value) -> Option<String> {
    let tier = [&load["paidTier"], &load["currentTier"]]
        .into_iter()
        .find(|t| t["id"].is_string())?;
    if tier["id"] == "free-tier" {
        return Some("Free".into());
    }
    tier["name"]
        .as_str()
        .filter(|n| !n.is_empty())
        .map(String::from)
}

fn meter(group: &str, window: &str, remaining: f64, reset: Option<&str>) -> Option<Meter> {
    if group.is_empty() || window.is_empty() || !(0.0..=1.0).contains(&remaining) {
        return None;
    }
    let lower = window.to_ascii_lowercase();
    let window = if lower == "5h" || lower.contains("five hour") {
        "5h"
    } else if lower.contains("week") {
        "weekly"
    } else {
        window.strip_suffix(" Remaining").unwrap_or(window)
    };
    Some(Meter {
        label: Some(format!("{group} {window}")),
        used: (1.0 - remaining) * 100.0,
        total: 100.0,
        unit: Unit::Percent,
        resets_at: reset.and_then(parse_rfc3339),
    })
}

fn parse(
    load: &Value,
    summary: &Value,
    models: Option<&Value>,
    now: i64,
) -> Result<Usage, FetchError> {
    let mut meters = Vec::new();
    for group in summary["groups"].as_array().into_iter().flatten() {
        let name = group["displayName"].as_str().unwrap_or_default();
        for bucket in group["buckets"].as_array().into_iter().flatten() {
            let window = bucket["window"]
                .as_str()
                .or_else(|| bucket["displayName"].as_str())
                .unwrap_or_default();
            if let Some(m) = bucket["remainingFraction"]
                .as_f64()
                .and_then(|r| meter(name, window, r, bucket["resetTime"].as_str()))
            {
                meters.push(m);
            }
        }
    }
    let has_gemini = meters
        .iter()
        .any(|m| m.label.as_deref().is_some_and(|l| l.contains("Gemini")));
    let plan = plan(load);
    // The free tier's Gemini quota is its weekly window.
    let free = plan.as_deref() == Some("Free");
    if !has_gemini && let Some(m) = models.and_then(|m| gemini_meter(m, now, free)) {
        meters.insert(0, m);
    }
    if meters.is_empty() {
        return Err(FetchError::new(
            Problem::NotAvailable,
            "This account has no Antigravity quota windows.",
        ));
    }
    Ok(Usage {
        personal: true,
        plan,
        cycle: None,
        note: None,
        estimated: vec![],
        api_spend: None,
        meters,
    })
}

/// The Gemini models share one limit, which each model's quota reports. The
/// fullest one is the shared limit. On paid plans it can be the 5-hour window,
/// which its reset time shows.
fn gemini_meter(models: &Value, now: i64, weekly: bool) -> Option<Meter> {
    models["models"]
        .as_object()?
        .values()
        .filter(|m| {
            m["modelProvider"] == "MODEL_PROVIDER_GOOGLE"
                && m["isInternal"] != true
                && m["displayName"].is_string()
                && m["quotaInfo"]["resetTime"].is_string()
        })
        .filter_map(|m| {
            let quota = &m["quotaInfo"];
            Some((
                quota["remainingFraction"].as_f64()?,
                quota["resetTime"].as_str(),
            ))
        })
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .and_then(|(remaining, reset)| {
            let left = reset.and_then(parse_rfc3339).map(|at| at - now);
            let window = if !weekly && left.is_some_and(|s| s <= 5 * 3600) {
                "5h"
            } else {
                "weekly"
            };
            meter("Gemini Models", window, remaining, reset)
        })
}

/// agy's sign-in JSON from the Windows Credential Manager.
#[cfg(windows)]
fn stored_sign_in() -> Result<String, FetchError> {
    use windows_sys::Win32::Security::Credentials::{
        CRED_TYPE_GENERIC, CREDENTIALW, CredFree, CredReadW,
    };
    let target: Vec<u16> = format!("{KEYRING_SERVICE}:{KEYRING_USER}")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut cred: *mut CREDENTIALW = std::ptr::null_mut();
    // SAFETY: `target` is NUL-terminated and `cred` is freed with CredFree below.
    if unsafe { CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut cred) } == 0 {
        return Err(FetchError::new(Problem::NotAvailable, SIGN_IN));
    }
    // SAFETY: CredReadW succeeded, so `cred` points at a credential whose blob
    // holds `CredentialBlobSize` bytes.
    let bytes = unsafe {
        let c = &*cred;
        let blob = if c.CredentialBlob.is_null() {
            Vec::new()
        } else {
            std::slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize).to_vec()
        };
        CredFree(cred as *const _);
        blob
    };
    String::from_utf8(bytes)
        .map_err(|_| FetchError::from("bad Antigravity sign-in entry".to_string()))
}

/// agy's sign-in JSON from the login keychain. go-keyring writes it with the
/// `security` tool, so reading it the same way doesn't prompt.
#[cfg(target_os = "macos")]
fn stored_sign_in() -> Result<String, FetchError> {
    let out = std::process::Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            KEYRING_SERVICE,
            "-a",
            KEYRING_USER,
            "-w",
        ])
        .output()
        .map_err(|e| format!("cannot run `security` ({e})"))?;
    if !out.status.success() {
        return Err(FetchError::new(Problem::NotAvailable, SIGN_IN));
    }
    decode_keyring(String::from_utf8_lossy(&out.stdout).trim())
}

#[cfg(not(any(windows, target_os = "macos")))]
fn stored_sign_in() -> Result<String, FetchError> {
    Err(FetchError::new(
        Problem::NotAvailable,
        "Antigravity quota is read on Windows and macOS only.",
    ))
}

/// go-keyring on macOS may store the value base64- or hex-encoded behind a prefix.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn decode_keyring(value: &str) -> Result<String, FetchError> {
    use base64::Engine;
    let bad = || FetchError::from("bad Antigravity sign-in entry".to_string());
    if let Some(b64) = value.strip_prefix("go-keyring-base64:") {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|_| bad())?;
        return String::from_utf8(bytes).map_err(|_| bad());
    }
    if let Some(hex) = value.strip_prefix("go-keyring-encoded:") {
        let bytes = (0..hex.len())
            .step_by(2)
            .map(|i| {
                hex.get(i..i + 2)
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
            })
            .collect::<Option<Vec<u8>>>()
            .ok_or_else(bad)?;
        return String::from_utf8(bytes).map_err(|_| bad());
    }
    Ok(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-11-25T06:26:40Z, a week before the fixtures' weekly resets.
    const NOW: i64 = 1_795_588_000;

    fn load(tier: &str, name: &str) -> Value {
        json!({
            "currentTier": {"id": tier, "name": name},
            "paidTier": {"id": tier, "name": name},
            "cloudaicompanionProject": "aicode-consumers"
        })
    }

    fn summary() -> Value {
        json!({"groups": [{
            "displayName": "Claude and GPT models",
            "buckets": [{
                "bucketId": "3p-weekly", "displayName": "Weekly Limit Remaining",
                "window": "weekly", "resetTime": "2026-12-02T00:12:54Z", "remainingFraction": 0.75
            }]
        }]})
    }

    fn models() -> Value {
        json!({"models": {
            "gemini-flash": {
                "displayName": "Gemini 3.8 Flash", "modelProvider": "MODEL_PROVIDER_GOOGLE",
                "quotaInfo": {"remainingFraction": 0.988, "resetTime": "2026-12-02T00:11:20Z"}
            },
            "gemini-pro": {
                "displayName": "Gemini 4 Hydrogen", "modelProvider": "MODEL_PROVIDER_GOOGLE",
                "quotaInfo": {"remainingFraction": 0.9, "resetTime": "2026-12-02T00:11:20Z"}
            },
            "internal": {
                "modelProvider": "MODEL_PROVIDER_GOOGLE", "isInternal": true,
                "quotaInfo": {"remainingFraction": 0.0}
            },
            "claude": {
                "displayName": "Claude Sonnet", "modelProvider": "MODEL_PROVIDER_ANTHROPIC",
                "quotaInfo": {"remainingFraction": 0.1, "resetTime": "2026-12-02T00:12:54Z"}
            }
        }})
    }

    #[test]
    fn reads_group_windows_and_the_shared_gemini_limit() {
        let usage = parse(
            &load("free-tier", "Antigravity"),
            &summary(),
            Some(&models()),
            NOW,
        )
        .unwrap();
        let labels: Vec<_> = usage
            .meters
            .iter()
            .map(|m| m.label.clone().unwrap())
            .collect();
        assert_eq!(
            labels,
            ["Gemini Models weekly", "Claude and GPT models weekly"]
        );
        assert!((usage.meters[0].used - 10.0).abs() < 1e-9);
        assert!((usage.meters[1].used - 25.0).abs() < 1e-9);
        assert_eq!(usage.meters[1].resets_at, Some(1_796_170_374));
        assert_eq!(usage.plan.as_deref(), Some("Free"));
    }

    #[test]
    fn a_gemini_quota_resetting_within_hours_is_the_five_hour_window() {
        let mut m = models();
        for id in ["gemini-flash", "gemini-pro"] {
            m["models"][id]["quotaInfo"]["resetTime"] = json!("2026-11-25T10:08:21Z");
        }
        let usage = parse(
            &load("g1-pro-tier", "Google AI Pro"),
            &summary(),
            Some(&m),
            NOW,
        )
        .unwrap();
        assert_eq!(usage.meters[0].label.as_deref(), Some("Gemini Models 5h"));
        let free = parse(&load("free-tier", "Antigravity"), &summary(), Some(&m), NOW).unwrap();
        assert_eq!(
            free.meters[0].label.as_deref(),
            Some("Gemini Models weekly")
        );
    }

    #[test]
    fn a_gemini_group_in_the_summary_is_not_doubled() {
        let mut s = summary();
        s["groups"][0]["displayName"] = json!("Gemini Models");
        let usage = parse(
            &load("standard-tier", "Gemini Code Assist"),
            &s,
            Some(&models()),
            NOW,
        )
        .unwrap();
        assert_eq!(usage.meters.len(), 1);
        assert_eq!(usage.plan.as_deref(), Some("Gemini Code Assist"));
    }

    #[test]
    fn no_quota_windows_hides_the_row() {
        let err = parse(&load("free-tier", "Antigravity"), &json!({}), None, NOW)
            .err()
            .unwrap();
        assert_eq!(err.problem, Problem::NotAvailable);
    }

    #[test]
    fn missing_licence_hides_and_revoked_sign_in_shows() {
        let no_licence = json!({"error": {"message": "You do not have a valid license"}});
        assert_eq!(
            checked(403, no_licence).unwrap_err().problem,
            Problem::NotAvailable
        );
        assert_eq!(
            checked(401, Value::Null).unwrap_err().problem,
            Problem::TokenRejected
        );
        let revoked = json!({"error": "invalid_grant"});
        assert_eq!(
            refreshed(400, &revoked).unwrap_err().problem,
            Problem::TokenExpired
        );
        let ok = json!({"access_token": "new", "expires_in": 3599});
        assert_eq!(
            refreshed(200, &ok).unwrap(),
            Some(("new".to_string(), 3599))
        );
        let wrong_client = json!({"error": "invalid_client"});
        assert_eq!(refreshed(401, &wrong_client).unwrap(), None);
    }

    #[test]
    fn reads_the_stored_sign_in() {
        let text = r#"{"token":{"access_token":"a","token_type":"Bearer","refresh_token":"r",
            "expiry":"2026-11-25T12:09:33.3490763+11:00","expires_in":3599},
            "auth_method":"consumer"}"#;
        let sign_in = parse_sign_in(text).unwrap();
        assert_eq!(sign_in.access_token, "a");
        assert_eq!(sign_in.refresh_token, "r");
        assert_eq!(sign_in.expires_at, Some(1_795_568_973));
        let empty = parse_sign_in(r#"{"token":{}}"#).unwrap_err();
        assert_eq!(empty.problem, Problem::NotAvailable);
        let blank = parse_sign_in("   ").unwrap_err();
        assert_eq!(blank.problem, Problem::NotAvailable);
    }

    #[test]
    fn a_fresh_stored_token_is_used_without_refreshing() {
        let stored = SignIn {
            access_token: "fresh".into(),
            refresh_token: String::new(),
            expires_at: Some(now_unix() + 600),
        };
        assert_eq!(access_token(&stored).unwrap(), "fresh");
        let lapsed = SignIn {
            expires_at: Some(now_unix() - 1),
            ..stored
        };
        assert_eq!(
            access_token(&lapsed).unwrap_err().problem,
            Problem::TokenExpired
        );
    }

    #[test]
    fn finds_client_secrets_split_across_reads() {
        // Built at run time so the source holds nothing that looks like a secret.
        let a = format!("{}{}", "GOCSPX-", "a".repeat(28));
        let b = format!("{}{}", "GOCSPX-", "B_-9".repeat(7));
        let data = format!("xx{a}yy {}short z{b} {a}", "GOCSPX-");
        for chunk in [1, 7, 34, 35, 36, 4096] {
            assert_eq!(
                find_secrets(data.as_bytes(), chunk),
                [a.clone(), b.clone()],
                "chunk {chunk}"
            );
        }
        let at_end = format!("..{b}");
        assert_eq!(find_secrets(at_end.as_bytes(), 5), std::slice::from_ref(&b));
    }

    #[test]
    fn decodes_go_keyring_values() {
        assert_eq!(decode_keyring("{}").unwrap(), "{}");
        assert_eq!(decode_keyring("go-keyring-base64:e30=").unwrap(), "{}");
        assert_eq!(decode_keyring("go-keyring-encoded:7b7d").unwrap(), "{}");
    }
}
