//! Fetches usage for each service using credentials already stored by the
//! corresponding CLI (gh, Claude Code, Codex). Nothing is persisted here.

use crate::timeutil::now_unix;
use base64::Engine;
use serde_json::Value;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

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

    /// `Ok(None)` when the account has no paid plan with this service (not signed
    /// in on this machine, or on a free plan), so the widget leaves it out.
    pub fn fetch(self) -> Result<Option<Usage>, String> {
        match self {
            Provider::Copilot => copilot(),
            Provider::Claude => claude(),
            Provider::Codex => codex(),
        }
    }
}

/// What a service reported for the signed-in account.
#[derive(Clone, Debug)]
pub struct Usage {
    pub meters: Vec<Meter>,
    /// A personal subscription (Claude Pro/Max, ChatGPT Plus/Pro, Copilot Pro).
    /// Its meters are percent windows, shown on one line with no dollar amounts.
    pub personal: bool,
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
    use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
    // The OS TLS stack (SChannel on Windows) trusting the OS certificate store, so
    // corporate TLS-inspecting proxies whose root is installed there just work.
    let tls = TlsConfig::builder()
        .provider(TlsProvider::NativeTls)
        .root_certs(RootCerts::PlatformVerifier)
        .build();
    ureq::Agent::config_builder()
        .tls_config(tls)
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
    std::env::home_dir().ok_or_else(|| "cannot resolve home directory".to_string())
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

fn copilot() -> Result<Option<Usage>, String> {
    // No GitHub CLI login (or token variable) on this machine.
    let Ok(token) = github_token() else {
        return Ok(None);
    };
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
        // Signed in to GitHub, but without Copilot.
        404 => return Ok(None),
        s => return Err(format!("GitHub returned HTTP {s}")),
    }

    copilot_usage(&json)
}

fn copilot_usage(json: &Value) -> Result<Option<Usage>, String> {
    let plan = json["copilot_plan"].as_str().unwrap_or("");
    if plan == "free" {
        return Ok(None);
    }
    let snap = &json["quota_snapshots"]["premium_interactions"];
    if snap.is_null() {
        return Err("no premium_interactions quota in response".into());
    }

    let total = f64_of(&snap["entitlement"]).unwrap_or(0.0);
    if total <= 0.0 && snap["unlimited"].as_bool() == Some(true) {
        return Ok(Some(Usage {
            meters: vec![Meter {
                label: Some("unlimited".into()),
                used: 0.0,
                total: 0.0,
                unit: Unit::Percent,
            }],
            personal: false,
        }));
    }

    let used = f64_of(&snap["credits_used"])
        .or_else(|| f64_of(&snap["remaining"]).map(|r| total - r))
        .unwrap_or(0.0);

    // Copilot Pro and Pro+ ("individual", "individual_pro") have no rolling
    // windows, only the monthly allowance.
    if plan.starts_with("individual") {
        return Ok(Some(Usage {
            meters: vec![Meter {
                label: Some("mo".into()),
                used: if total > 0.0 {
                    used / total * 100.0
                } else {
                    0.0
                },
                total: 100.0,
                unit: Unit::Percent,
            }],
            personal: true,
        }));
    }

    Ok(Some(Usage {
        meters: vec![Meter {
            label: None,
            used: used * COPILOT_USD_PER_CREDIT,
            total: total * COPILOT_USD_PER_CREDIT,
            unit: Unit::Dollars,
        }],
        personal: false,
    }))
}

// ---------------------------------------------------------------------------
// Claude (Claude Code OAuth token)
// ---------------------------------------------------------------------------

/// Claude Code only refreshes its OAuth token while it runs, so on an idle machine
/// the token expires. The widget never refreshes it itself (that would rotate the
/// refresh token under Claude Code, or spend usage on a headless prompt), so it
/// asks for Claude Code to be run and picks the new token up on the next refresh.
fn claude() -> Result<Option<Usage>, String> {
    let path = home()?.join(".claude").join(".credentials.json");
    // Claude Code has never been signed in on this machine.
    if !path.exists() {
        return Ok(None);
    }
    let creds = read_json_file(&path)?;
    let oauth = &creds["claudeAiOauth"];
    // Signed in with an API key rather than a Claude plan.
    let Some(token) = oauth["accessToken"].as_str() else {
        return Ok(None);
    };
    let plan = oauth["subscriptionType"].as_str().unwrap_or("");
    if plan == "free" {
        return Ok(None);
    }
    if let Some(exp_ms) = i64_of(&oauth["expiresAt"]) {
        if exp_ms / 1000 < now_unix() {
            return Err("Claude token expired; run `claude` once to refresh".into());
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
        401 | 403 => return Err("Claude token rejected; run `claude` once to refresh".into()),
        s => return Err(format!("Anthropic returned HTTP {s}")),
    }

    claude_usage(&json, matches!(plan, "pro" | "max")).map(Some)
}

fn claude_usage(json: &Value, personal: bool) -> Result<Usage, String> {
    // A personal plan shows just its 5-hour and weekly windows.
    let windows: &[(&str, &str)] = if personal {
        &[("five_hour", "5h"), ("seven_day", "wk")]
    } else {
        &[
            ("five_hour", "5h"),
            ("seven_day", "week"),
            ("seven_day_opus", "opus wk"),
            ("seven_day_sonnet", "sonnet wk"),
        ]
    };
    let mut meters = Vec::new();
    for (key, label) in windows {
        let w = &json[key];
        if let Some(util) = f64_of(&w["utilization"]) {
            meters.push(Meter {
                label: Some((*label).into()),
                used: util,
                total: 100.0,
                unit: Unit::Percent,
            });
        }
    }
    if personal {
        if meters.is_empty() {
            return Err("no usage windows in response".into());
        }
        return Ok(Usage { meters, personal });
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
    Ok(Usage { meters, personal })
}

// ---------------------------------------------------------------------------
// Codex (Codex CLI ChatGPT token)
// ---------------------------------------------------------------------------

/// Like Claude Code, the Codex CLI only refreshes its ChatGPT token while it runs,
/// so an expired token is reported until `codex` is run again.
fn codex() -> Result<Option<Usage>, String> {
    let path = home()?.join(".codex").join("auth.json");
    // The Codex CLI has never been signed in on this machine.
    if !path.exists() {
        return Ok(None);
    }
    let auth_file = read_json_file(&path)?;
    let tokens = &auth_file["tokens"];
    // Signed in with an API key rather than a ChatGPT plan.
    let Some(token) = tokens["access_token"].as_str() else {
        return Ok(None);
    };
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

    codex_usage(&json)
}

fn codex_usage(json: &Value) -> Result<Option<Usage>, String> {
    let plan = json["plan_type"].as_str().unwrap_or("");
    if plan == "free" {
        return Ok(None);
    }
    // ChatGPT plans billed to the user rather than a workspace.
    let personal = matches!(plan, "plus" | "pro" | "go");

    let mut meters = Vec::new();

    let rl = &json["rate_limit"];
    let week = if personal { "wk" } else { "week" };
    for (key, fallback) in [("primary_window", "5h"), ("secondary_window", week)] {
        let w = &rl[key];
        if let Some(pct) = f64_of(&w["used_percent"]) {
            let label = i64_of(&w["limit_window_seconds"])
                .map(|secs| window_label(secs, week))
                .unwrap_or_else(|| fallback.into());
            meters.push(Meter {
                label: Some(label),
                used: pct,
                total: 100.0,
                unit: Unit::Percent,
            });
        }
    }

    // A personal plan shows just its windows.
    let lim = &json["spend_control"]["individual_limit"];
    if let (false, Some(used), Some(limit)) =
        (personal, f64_of(&lim["used"]), f64_of(&lim["limit"]))
    {
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
            return Ok(Some(Usage {
                meters: vec![Meter {
                    label: Some("unlimited".into()),
                    used: 0.0,
                    total: 0.0,
                    unit: Unit::Percent,
                }],
                personal: false,
            }));
        }
        return Err("no rate limit or spend data in response".into());
    }
    Ok(Some(Usage { meters, personal }))
}

/// "5h", "2d", or `week` for windows of six days or more.
fn window_label(secs: i64, week: &str) -> String {
    if secs >= 6 * 86_400 {
        week.into()
    } else if secs >= 86_400 {
        format!("{}d", secs / 86_400)
    } else {
        format!("{}h", (secs + 1799) / 3600)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcts(u: &Usage) -> Vec<(String, f64)> {
        u.meters
            .iter()
            .map(|m| (m.label.clone().unwrap_or_default(), m.percent()))
            .collect()
    }

    #[test]
    fn copilot_entitlement_takes_precedence_over_unlimited() {
        for unlimited in [false, true] {
            let json = serde_json::json!({
                "copilot_plan": "enterprise",
                "quota_snapshots": {
                    "premium_interactions": {
                        "unlimited": unlimited,
                        "entitlement": 150_000,
                        "credits_used": 0,
                        "remaining": 150_000
                    }
                }
            });
            let usage = copilot_usage(&json).unwrap().unwrap();
            assert!(!usage.personal);
            assert_eq!(usage.meters.len(), 1);
            assert_eq!(usage.meters[0].label, None);
            assert_eq!(usage.meters[0].unit, Unit::Dollars);
            assert_eq!(usage.meters[0].summary(), "$0 / $1,500");
        }
    }

    #[test]
    fn copilot_unlimited_without_entitlement() {
        let json = serde_json::json!({
            "quota_snapshots": {
                "premium_interactions": { "unlimited": true, "entitlement": 0 }
            }
        });
        let usage = copilot_usage(&json).unwrap().unwrap();
        assert_eq!(usage.meters[0].label.as_deref(), Some("unlimited"));
        assert_eq!(usage.meters[0].total, 0.0);
    }

    #[test]
    fn copilot_personal_and_free_plans() {
        let json = serde_json::json!({
            "copilot_plan": "individual_pro",
            "quota_snapshots": {
                "premium_interactions": { "entitlement": 1500, "remaining": 1125 }
            }
        });
        let usage = copilot_usage(&json).unwrap().unwrap();
        assert!(usage.personal);
        assert_eq!(pcts(&usage), [("mo".to_string(), 25.0)]);

        let free = serde_json::json!({ "copilot_plan": "free", "quota_snapshots": {} });
        assert!(copilot_usage(&free).unwrap().is_none());
    }

    #[test]
    fn claude_personal_shows_only_windows() {
        let json = serde_json::json!({
            "five_hour": { "utilization": 42.0 },
            "seven_day": { "utilization": 17.0 },
            "seven_day_opus": { "utilization": 5.0 },
            "extra_usage": { "used_credits": 500.0, "monthly_limit": 2000.0 }
        });
        let usage = claude_usage(&json, true).unwrap();
        assert!(usage.personal);
        assert_eq!(
            pcts(&usage),
            [("5h".to_string(), 42.0), ("wk".to_string(), 17.0)]
        );

        let business = claude_usage(&json, false).unwrap();
        assert!(!business.personal);
        assert_eq!(business.meters.len(), 4);
        assert_eq!(business.meters[3].unit, Unit::Dollars);
    }

    #[test]
    fn codex_plans() {
        let windows = serde_json::json!({
            "primary_window": { "used_percent": 30, "limit_window_seconds": 18_000 },
            "secondary_window": { "used_percent": 60, "limit_window_seconds": 604_800 }
        });
        let spend = serde_json::json!({ "individual_limit": { "used": "1250", "limit": "12500" } });

        let plus = serde_json::json!({ "plan_type": "plus", "rate_limit": windows, "spend_control": spend });
        let usage = codex_usage(&plus).unwrap().unwrap();
        assert!(usage.personal);
        assert_eq!(
            pcts(&usage),
            [("5h".to_string(), 30.0), ("wk".to_string(), 60.0)]
        );

        let business = serde_json::json!({ "plan_type": "business", "rate_limit": null, "spend_control": spend });
        let usage = codex_usage(&business).unwrap().unwrap();
        assert!(!usage.personal);
        assert_eq!(usage.meters[0].summary(), "$50 / $500");

        let free = serde_json::json!({ "plan_type": "free", "rate_limit": windows });
        assert!(codex_usage(&free).unwrap().is_none());
    }

    #[test]
    fn formatting() {
        assert_eq!(money(62_440.0 * COPILOT_USD_PER_CREDIT), "624.40");
        assert_eq!(money(12_500.0 * CODEX_USD_PER_CREDIT), "500");
        assert_eq!(money(518.94), "518.94");
        assert_eq!(money(1000.0), "1,000");
        assert_eq!(money(12_345.5), "12,345.50");
        assert_eq!(window_label(18_000, "week"), "5h");
        assert_eq!(window_label(604_800, "week"), "week");
    }
}
