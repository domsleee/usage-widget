//! Fetches usage for each service using credentials already stored by the
//! corresponding CLI (gh, Claude Code, Codex). Nothing is persisted here.

use crate::timeutil::now_unix;
use base64::Engine;
use serde_json::Value;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const UA: &str = "usage-widget/1.0 (+https://github.com/pepsi-enjoyer/usage-widget)";

/// Copilot premium-request credits: 50,000 credits = $500.
const COPILOT_USD_PER_CREDIT: f64 = 500.0 / 50_000.0;
/// Codex workspace spend credits: 12,500 credits = $500.
const CODEX_USD_PER_CREDIT: f64 = 500.0 / 12_500.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Provider {
    Copilot,
    Claude,
    Codex,
}

impl Provider {
    pub const ALL: [Provider; 3] = [Provider::Copilot, Provider::Claude, Provider::Codex];

    pub fn name(self) -> &'static str {
        match self {
            Provider::Copilot => "Copilot",
            Provider::Claude => "Claude",
            Provider::Codex => "Codex",
        }
    }

    /// Three-letter tag for the minimized view.
    pub fn short_name(self) -> &'static str {
        match self {
            Provider::Copilot => "COP",
            Provider::Claude => "CLD",
            Provider::Codex => "CDX",
        }
    }

    pub fn url(self) -> &'static str {
        match self {
            Provider::Copilot => "https://github.com/settings/copilot/features",
            Provider::Claude => "https://claude.ai/new#settings/usage",
            Provider::Codex => "https://chatgpt.com/#settings/Usage",
        }
    }

    pub fn fetch(self) -> Result<Vec<Meter>, String> {
        match self {
            Provider::Copilot => copilot(),
            Provider::Claude => claude(),
            Provider::Codex => codex(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Unit {
    /// Amount in whole currency units.
    Dollars,
    /// Only a percentage is known.
    Percent,
}

#[derive(Clone, Debug)]
pub struct Meter {
    /// Optional sub-label when a provider has more than one meter (e.g. "5h", "week").
    pub label: Option<String>,
    pub used: f64,
    pub total: f64,
    pub unit: Unit,
}

impl Meter {
    pub fn fraction(&self) -> f32 {
        if self.total <= 0.0 {
            return 0.0;
        }
        (self.used / self.total).clamp(0.0, 1.0) as f32
    }

    pub fn percent(&self) -> f64 {
        if self.total <= 0.0 {
            0.0
        } else {
            (self.used / self.total * 100.0).clamp(0.0, 100.0)
        }
    }

    pub fn summary(&self) -> String {
        match self.unit {
            Unit::Dollars => format!("${} / ${}", money(self.used), money(self.total)),
            Unit::Percent => format!("{:.0}%", self.used),
        }
    }
}

/// Formats a dollar amount with thousands separators; cents only when non-zero.
pub fn money(v: f64) -> String {
    let cents_total = (v * 100.0).round() as i64;
    let whole = cents_total / 100;
    let cents = cents_total % 100;
    let mut w = whole.abs().to_string();
    let mut out = String::new();
    while w.len() > 3 {
        let tail = w.split_off(w.len() - 3);
        out = format!(",{tail}{out}");
    }
    let whole_s = format!("{w}{out}");
    if cents == 0 {
        whole_s
    } else {
        format!("{whole_s}.{cents:02}")
    }
}

// ---------------------------------------------------------------------------
// HTTP plumbing
// ---------------------------------------------------------------------------

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(25)))
        .http_status_as_error(false)
        .user_agent(UA)
        .build()
        .into()
}

fn get_json(url: &str, headers: &[(&str, &str)]) -> Result<(u16, Value), String> {
    let mut req = agent().get(url).header("Accept", "application/json");
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let mut resp = req.call().map_err(|e| format!("request failed: {e}"))?;
    let status = resp.status().as_u16();
    let text = resp
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("read failed: {e}"))?;
    let json = serde_json::from_str::<Value>(&text).unwrap_or(Value::Null);
    Ok((status, json))
}

/// Stops a console window flashing up when a CLI is run from the windowless exe.
fn no_window(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = cmd;
}

fn home() -> Result<PathBuf, String> {
    dirs::home_dir().ok_or_else(|| "cannot resolve home directory".to_string())
}

fn read_json_file(path: &PathBuf) -> Result<Value, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("bad JSON in {}: {e}", path.display()))
}

fn f64_of(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn i64_of(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Reads the `exp` claim from a JWT without verifying it.
fn jwt_exp(token: &str) -> Option<i64> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let v: Value = serde_json::from_slice(&bytes).ok()?;
    i64_of(&v["exp"])
}

// ---------------------------------------------------------------------------
// GitHub Copilot
// ---------------------------------------------------------------------------

fn github_token() -> Result<String, String> {
    for var in ["GITHUB_TOKEN", "GH_TOKEN"] {
        if let Ok(t) = std::env::var(var) {
            if !t.trim().is_empty() {
                return Ok(t.trim().to_string());
            }
        }
    }
    let mut cmd = Command::new("gh");
    cmd.args(["auth", "token"]);
    no_window(&mut cmd);
    let out = cmd
        .output()
        .map_err(|e| format!("gh not found ({e}); set GITHUB_TOKEN or install GitHub CLI"))?;
    if !out.status.success() {
        return Err("`gh auth token` failed; run `gh auth login`".into());
    }
    let tok = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if tok.is_empty() {
        return Err("gh returned an empty token".into());
    }
    Ok(tok)
}

fn copilot() -> Result<Vec<Meter>, String> {
    let token = github_token()?;
    let auth = format!("token {token}");
    let (status, json) = get_json(
        "https://api.github.com/copilot_internal/user",
        &[
            ("Authorization", &auth),
            ("X-GitHub-Api-Version", "2022-11-28"),
        ],
    )?;
    match status {
        200 => {}
        401 | 403 => return Err("GitHub token rejected; run `gh auth login`".into()),
        s => return Err(format!("GitHub returned HTTP {s}")),
    }

    let snap = &json["quota_snapshots"]["premium_interactions"];
    if snap.is_null() {
        return Err("no premium_interactions quota in response".into());
    }

    if snap["unlimited"].as_bool() == Some(true) {
        return Ok(vec![Meter {
            label: Some("unlimited".into()),
            used: 0.0,
            total: 0.0,
            unit: Unit::Percent,
        }]);
    }

    let total = f64_of(&snap["entitlement"]).unwrap_or(0.0);
    let used = f64_of(&snap["credits_used"])
        .or_else(|| f64_of(&snap["remaining"]).map(|r| total - r))
        .unwrap_or(0.0);
    Ok(vec![Meter {
        label: None,
        used: used * COPILOT_USD_PER_CREDIT,
        total: total * COPILOT_USD_PER_CREDIT,
        unit: Unit::Dollars,
    }])
}

// ---------------------------------------------------------------------------
// Claude (Claude Code OAuth token)
// ---------------------------------------------------------------------------

/// Minimum gap between headless `claude` runs, so a persistent auth failure does
/// not spend a prompt on every refresh.
const CLAUDE_REFRESH_COOLDOWN: Duration = Duration::from_secs(15 * 60);

enum ClaudeError {
    /// The stored token is expired or was rejected; a token refresh may fix it.
    Auth(String),
    Other(String),
}

impl From<String> for ClaudeError {
    fn from(e: String) -> Self {
        ClaudeError::Other(e)
    }
}

impl From<&str> for ClaudeError {
    fn from(e: &str) -> Self {
        ClaudeError::Other(e.into())
    }
}

impl From<ClaudeError> for String {
    fn from(e: ClaudeError) -> Self {
        match e {
            ClaudeError::Auth(e) | ClaudeError::Other(e) => e,
        }
    }
}

fn claude() -> Result<Vec<Meter>, String> {
    match claude_once() {
        Err(ClaudeError::Auth(msg)) => match refresh_claude_token() {
            Ok(()) => claude_once().map_err(String::from),
            Err(why) => Err(format!("{msg} ({why})")),
        },
        r => r.map_err(String::from),
    }
}

/// Claude Code only refreshes its OAuth token while it runs, so an idle machine
/// ends up with an expired one. A one-line headless prompt on the smallest model
/// makes it refresh and write the new token back to `.credentials.json`.
fn refresh_claude_token() -> Result<(), String> {
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    {
        let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
        if last.is_some_and(|t| t.elapsed() < CLAUDE_REFRESH_COOLDOWN) {
            return Err("auto-refresh tried recently".into());
        }
        *last = Some(Instant::now());
    }

    let args = [
        "-p",
        "--model",
        "haiku",
        "--max-turns",
        "1",
        "--no-session-persistence",
        "--strict-mcp-config",
        "reply with ok",
    ];
    let spawn = |program: &str| {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .current_dir(std::env::temp_dir())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        no_window(&mut cmd);
        cmd.spawn()
    };
    // npm installs ship a claude.cmd shim, which Command only finds by full name.
    let mut child = spawn("claude")
        .or_else(|_| spawn("claude.cmd"))
        .map_err(|e| format!("could not run claude: {e}"))?;

    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(_)) => return Err("headless claude failed".into()),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(250)),
            _ => {
                let _ = child.kill();
                return Err("headless claude timed out".into());
            }
        }
    }
}

fn claude_once() -> Result<Vec<Meter>, ClaudeError> {
    let path = home()?.join(".claude").join(".credentials.json");
    let creds = read_json_file(&path)?;
    let oauth = &creds["claudeAiOauth"];
    let token = oauth["accessToken"]
        .as_str()
        .ok_or("no claudeAiOauth.accessToken; log in with `claude`")?;
    if let Some(exp_ms) = i64_of(&oauth["expiresAt"]) {
        if exp_ms / 1000 < now_unix() {
            return Err(ClaudeError::Auth(
                "Claude token expired; run `claude` once to refresh".into(),
            ));
        }
    }

    let auth = format!("Bearer {token}");
    let (status, json) = get_json(
        "https://api.anthropic.com/api/oauth/usage",
        &[
            ("Authorization", &auth),
            ("anthropic-beta", "oauth-2025-04-20"),
        ],
    )?;
    match status {
        200 => {}
        401 | 403 => {
            return Err(ClaudeError::Auth(
                "Claude token rejected; run `claude` once to refresh".into(),
            ));
        }
        s => return Err(format!("Anthropic returned HTTP {s}").into()),
    }

    let mut meters = Vec::new();
    for (key, label) in [
        ("five_hour", "5h"),
        ("seven_day", "week"),
        ("seven_day_opus", "opus wk"),
        ("seven_day_sonnet", "sonnet wk"),
    ] {
        let w = &json[key];
        if let Some(util) = f64_of(&w["utilization"]) {
            meters.push(Meter {
                label: Some(label.into()),
                used: util,
                total: 100.0,
                unit: Unit::Percent,
            });
        }
    }

    // Spend against a monthly credit cap (enterprise / extra usage).
    let spend = &json["spend"];
    let spend_label = |meters: &Vec<Meter>| {
        if meters.is_empty() {
            None
        } else {
            Some("spend".to_string())
        }
    };
    if let (Some(used_minor), Some(limit_minor)) = (
        i64_of(&spend["used"]["amount_minor"]),
        i64_of(&spend["limit"]["amount_minor"]),
    ) {
        let exp = i64_of(&spend["used"]["exponent"]).unwrap_or(2) as i32;
        let div = 10f64.powi(exp);
        meters.push(Meter {
            label: spend_label(&meters),
            used: used_minor as f64 / div,
            total: limit_minor as f64 / div,
            unit: Unit::Dollars,
        });
    } else {
        let extra = &json["extra_usage"];
        if let (Some(used), Some(limit)) = (
            f64_of(&extra["used_credits"]),
            f64_of(&extra["monthly_limit"]),
        ) {
            meters.push(Meter {
                label: spend_label(&meters),
                used: used / 100.0,
                total: limit / 100.0,
                unit: Unit::Dollars,
            });
        }
    }

    if meters.is_empty() {
        return Err("no usage windows in response".into());
    }
    Ok(meters)
}

// ---------------------------------------------------------------------------
// Codex (Codex CLI ChatGPT token)
// ---------------------------------------------------------------------------

fn codex() -> Result<Vec<Meter>, String> {
    let path = home()?.join(".codex").join("auth.json");
    let auth_file = read_json_file(&path)?;
    let tokens = &auth_file["tokens"];
    let token = tokens["access_token"]
        .as_str()
        .ok_or("no tokens.access_token; log in with `codex`")?;
    if let Some(exp) = jwt_exp(token) {
        if exp < now_unix() {
            return Err("Codex token expired; run `codex` once to refresh".into());
        }
    }
    let account_id = tokens["account_id"].as_str().unwrap_or("");

    let auth = format!("Bearer {token}");
    let mut headers: Vec<(&str, &str)> = vec![("Authorization", &auth)];
    if !account_id.is_empty() {
        headers.push(("ChatGPT-Account-Id", account_id));
    }
    let (status, json) = get_json("https://chatgpt.com/backend-api/wham/usage", &headers)?;
    match status {
        200 => {}
        401 | 403 => return Err("Codex token rejected; run `codex` once to refresh".into()),
        s => return Err(format!("OpenAI returned HTTP {s}")),
    }

    let mut meters = Vec::new();

    let rl = &json["rate_limit"];
    for (key, fallback) in [("primary_window", "5h"), ("secondary_window", "week")] {
        let w = &rl[key];
        if let Some(pct) = f64_of(&w["used_percent"]) {
            let label = i64_of(&w["limit_window_seconds"])
                .map(window_label)
                .unwrap_or_else(|| fallback.into());
            meters.push(Meter {
                label: Some(label),
                used: pct,
                total: 100.0,
                unit: Unit::Percent,
            });
        }
    }

    let lim = &json["spend_control"]["individual_limit"];
    if let (Some(used), Some(limit)) = (f64_of(&lim["used"]), f64_of(&lim["limit"])) {
        meters.push(Meter {
            label: if meters.is_empty() {
                None
            } else {
                Some("spend".into())
            },
            used: used * CODEX_USD_PER_CREDIT,
            total: limit * CODEX_USD_PER_CREDIT,
            unit: Unit::Dollars,
        });
    }

    if meters.is_empty() {
        if json["credits"]["unlimited"].as_bool() == Some(true) {
            return Ok(vec![Meter {
                label: Some("unlimited".into()),
                used: 0.0,
                total: 0.0,
                unit: Unit::Percent,
            }]);
        }
        return Err("no rate limit or spend data in response".into());
    }
    Ok(meters)
}

fn window_label(secs: i64) -> String {
    if secs >= 6 * 86_400 {
        "week".into()
    } else if secs >= 86_400 {
        format!("{}d", secs / 86_400)
    } else {
        format!("{}h", (secs + 1799) / 3600)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting() {
        assert_eq!(money(62_440.0 * COPILOT_USD_PER_CREDIT), "624.40");
        assert_eq!(money(12_500.0 * CODEX_USD_PER_CREDIT), "500");
        assert_eq!(money(518.94), "518.94");
        assert_eq!(money(1000.0), "1,000");
        assert_eq!(money(12_345.5), "12,345.50");
        assert_eq!(window_label(18_000), "5h");
        assert_eq!(window_label(604_800), "week");
    }
}
