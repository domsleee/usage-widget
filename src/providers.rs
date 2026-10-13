//! Fetches usage for each service using credentials already stored by the
//! corresponding CLI (gh, Claude Code, Codex). The only thing persisted is
//! Claude's last API response, rate-limit backoff and renewal date (see `ClaudeState`).

use crate::timeutil::{now_unix, parse_rfc3339};
use base64::Engine;
use serde::{Deserialize, Serialize};
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
            Provider::Claude => "https://claude.ai/settings/usage",
            Provider::Codex => "https://chatgpt.com/settings/usage?tab=overview",
        }
    }

    /// `Problem::NotAvailable` when the account has no paid plan with this
    /// service (not signed in on this machine, or on a free plan), so the widget
    /// leaves it out.
    pub fn fetch(self) -> Result<Usage, FetchError> {
        match self {
            Provider::Copilot => copilot(),
            Provider::Claude => claude(&config.claude),
            Provider::Codex => codex(config.codex.estimate),
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

#[derive(Serialize)]
pub struct Usage {
    /// Subscription name, e.g. "Max 5x" or "Enterprise".
    pub plan: Option<String>,
    /// When the plan renews or its quota resets.
    pub cycle: Option<Cycle>,
    /// Caveat about the data, e.g. that it is old because of rate limiting.
    pub note: Option<String>,
    /// Labels of meters whose values include a local estimate (see `estimate`).
    pub estimated: Vec<String>,
    pub meters: Vec<Meter>,
}

/// A billing-cycle boundary; `verb` is "renews", "resets" or "ends".
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Cycle {
    pub verb: String,
    pub at: i64,
    #[serde(default)]
    pub date_only: bool,
}

impl Cycle {
    pub fn countdown(&self, today: chrono::NaiveDate) -> String {
        if self.date_only {
            let date = chrono::DateTime::from_timestamp(self.at, 0)
                .unwrap()
                .with_timezone(&chrono::Local)
                .date_naive();
            let days = (date - today).num_days();
            if days == 0 {
                "today".into()
            } else if days > 0 {
                format!("{days}d")
            } else {
                "date passed".into()
            }
        } else {
            crate::timeutil::until_short(self.at)
        }
    }

    pub fn when(&self) -> String {
        if self.date_only {
            chrono::DateTime::from_timestamp(self.at, 0)
                .unwrap()
                .with_timezone(&chrono::Local)
                .format("%a %d %b %Y")
                .to_string()
        } else {
            crate::timeutil::local_when(self.at)
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Meter {
    /// Optional sub-label when a provider has more than one meter (e.g. "5h", "week").
    pub label: Option<String>,
    pub used: f64,
    pub total: f64,
    pub unit: Unit,
    /// When a usage window next resets, as unix seconds.
    pub resets_at: Option<i64>,
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

pub(crate) fn agent() -> ureq::Agent {
    use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
    TlsConfig::builder()
        .provider(TlsProvider::NativeTls)
        .root_certs(RootCerts::PlatformVerifier)
        .build()
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .tls_config(tls())
        .timeout_global(Some(Duration::from_secs(25)))
        .http_status_as_error(false)
        .user_agent(UA)
        .build()
        .into()
}

fn get_json(url: &str, headers: &[(&str, &str)]) -> Result<(u16, Value), FetchError> {
    get_json_retry(url, headers).map(|(status, json, _)| (status, json))
}

/// Like `get_json`, plus the `Retry-After` seconds when the server sends them.
fn get_json_retry(
    url: &str,
    headers: &[(&str, &str)],
) -> Result<(u16, Value, Option<i64>), FetchError> {
    let mut req = agent().get(url).header("Accept", "application/json");
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let mut resp = req
        .call()
        .map_err(|e| FetchError::new(Problem::Connection, format!("request failed: {e}")))?;
    let status = resp.status().as_u16();
    let retry_after = resp
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse().ok());
    let text = resp
        .body_mut()
        .read_to_string()
        .map_err(|e| FetchError::new(Problem::Connection, format!("read failed: {e}")))?;
    let json = serde_json::from_str::<Value>(&text).unwrap_or(Value::Null);
    Ok((status, json, retry_after))
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

/// A missing file means the CLI was never logged in here, so the service is
/// reported as not available, with `hint` saying how to set it up.
fn read_json_file(path: &PathBuf, hint: &str) -> Result<Value, FetchError> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            FetchError::new(
                Problem::NotAvailable,
                format!("{} not found; {hint}", path.display()),
            )
        } else {
            FetchError::new(
                Problem::Other,
                format!("cannot read {}: {e}", path.display()),
            )
        }
    })?;
    serde_json::from_str(&text).map_err(|e| {
        FetchError::new(
            Problem::Other,
            format!("bad JSON in {}: {e}", path.display()),
        )
    })
}

fn f64_of(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// A timestamp given as unix seconds or as RFC 3339 text.
fn time_of(v: &Value) -> Option<i64> {
    match v {
        Value::String(s) => parse_rfc3339(s),
        v => i64_of(v),
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
// Errors
// ---------------------------------------------------------------------------

/// What went wrong reading a service, shown in short on the widget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Problem {
    /// The service is not set up on this machine (no login or no subscription).
    NotAvailable,
    TokenExpired,
    TokenRejected,
    RateLimited,
    Connection,
    /// The service answered with an unexpected HTTP status.
    Service,
    /// The service answered, but without the usage data expected.
    BadResponse,
    Other,
}

impl Problem {
    pub fn label(self) -> &'static str {
        match self {
            Problem::NotAvailable => "Not Available",
            Problem::TokenExpired => "Token expired",
            Problem::TokenRejected => "Token rejected",
            Problem::RateLimited => "Rate limited",
            Problem::Connection => "Connection failed",
            Problem::Service => "Service error",
            Problem::BadResponse => "Unexpected response",
            Problem::Other => "Error",
        }
    }
}

/// A short `problem` for the widget, plus the full `detail` shown on hover.
#[derive(Clone, Debug)]
pub struct FetchError {
    pub problem: Problem,
    pub detail: String,
}

impl FetchError {
    fn new(problem: Problem, detail: impl Into<String>) -> Self {
        Self {
            problem,
            detail: detail.into(),
        }
    }

    /// An expired or rejected token, which running the owning CLI may refresh.
    fn needs_refresh(&self) -> bool {
        matches!(self.problem, Problem::TokenExpired | Problem::TokenRejected)
    }

    fn with_cause(self, why: &str) -> Self {
        FetchError::new(self.problem, format!("{} ({why})", self.detail))
    }
}

impl From<String> for FetchError {
    fn from(e: String) -> Self {
        FetchError::new(Problem::Other, e)
    }
}

// ---------------------------------------------------------------------------
// GitHub Copilot
// ---------------------------------------------------------------------------

fn github_token() -> Result<String, FetchError> {
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
    let out = cmd.output().map_err(|e| {
        FetchError::new(
            Problem::NotAvailable,
            format!("gh not found ({e}); set GITHUB_TOKEN or install GitHub CLI"),
        )
    })?;
    if !out.status.success() {
        return Err(FetchError::new(
            Problem::NotAvailable,
            "`gh auth token` failed; run `gh auth login`",
        ));
    }
    let tok = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if tok.is_empty() {
        return Err(FetchError::new(
            Problem::NotAvailable,
            "gh returned an empty token; run `gh auth login`",
        ));
    }
    Ok(tok)
}

fn copilot() -> Result<Usage, FetchError> {
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
        401 | 403 => {
            return Err(FetchError::new(
                Problem::TokenRejected,
                "GitHub token rejected; run `gh auth login`",
            ));
        }
        404 => {
            return Err(FetchError::new(
                Problem::NotAvailable,
                "no Copilot access on this GitHub account",
            ));
        }
        s => {
            return Err(FetchError::new(
                Problem::Service,
                format!("GitHub returned HTTP {s}"),
            ));
        }
    }

    copilot_usage(&json)
}

fn copilot_usage(json: &Value) -> Result<Usage, FetchError> {
    let plan = json["copilot_plan"].as_str().unwrap_or("");
    if plan == "free" {
        return Err(FetchError::new(
            Problem::NotAvailable,
            "Copilot Free has no paid plan to track",
        ));
    }
    let snap = &json["quota_snapshots"]["premium_interactions"];
    if snap.is_null() {
        return Err(FetchError::new(
            Problem::NotAvailable,
            "no Copilot premium request quota on this GitHub account",
        ));
    }

    let total = f64_of(&snap["entitlement"]).unwrap_or(0.0);
    if total <= 0.0 && snap["unlimited"].as_bool() == Some(true) {
        return Ok(Usage {
            meters: vec![Meter {
                label: Some("unlimited".into()),
                used: 0.0,
                total: 0.0,
                unit: Unit::Percent,
                resets_at: None,
            }],
            personal: false,
        });
    }

    let used = f64_of(&snap["credits_used"])
        .or_else(|| f64_of(&snap["remaining"]).map(|r| total - r))
        .unwrap_or(0.0);

    // Copilot Pro and Pro+ ("individual", "individual_pro") have no rolling
    // windows, only the monthly allowance.
    if plan.starts_with("individual") {
        return Ok(Usage {
            meters: vec![Meter {
                label: Some("mo".into()),
                used: if total > 0.0 {
                    used / total * 100.0
                } else {
                    0.0
                },
                total: 100.0,
                unit: Unit::Percent,
                resets_at: None,
            }],
            personal: true,
        });
    }

    Ok(Usage {
        meters: vec![Meter {
            label: None,
            used: used * COPILOT_USD_PER_CREDIT,
            total: total * COPILOT_USD_PER_CREDIT,
            unit: Unit::Dollars,
            resets_at: None,
        }],
        personal: false,
    })
}

fn copilot_plan(json: &Value) -> Option<String> {
    let sku = json["access_type_sku"].as_str().unwrap_or("");
    Some(
        match json["copilot_plan"].as_str().filter(|s| !s.is_empty())? {
            "individual" if sku.contains("pro_plus") => "Pro+".into(),
            "individual" if sku.contains("free") => "Free".into(),
            "individual" => "Pro".into(),
            other => title_case(other),
        },
    )
}

// ---------------------------------------------------------------------------
// Claude (Claude Code OAuth token)
// ---------------------------------------------------------------------------

/// Claude Code only refreshes its OAuth token while it runs, so on an idle machine
/// the token expires. The widget never refreshes it itself (that would rotate the
/// refresh token under Claude Code, or spend usage on a headless prompt), so it
/// asks for Claude Code to be run and picks the new token up on the next refresh.
fn claude() -> Result<Usage, FetchError> {
    let path = home()?.join(".claude").join(".credentials.json");
    let creds = read_json_file(&path, "log in with `claude`")?;
    let oauth = &creds["claudeAiOauth"];
    // Signed in with an API key rather than a Claude plan.
    let token = oauth["accessToken"].as_str().ok_or_else(|| {
        FetchError::new(
            Problem::NotAvailable,
            "no claudeAiOauth.accessToken; log in with `claude`",
        )
    })?;
    let plan = oauth["subscriptionType"].as_str().unwrap_or("");
    if plan == "free" {
        return Err(FetchError::new(
            Problem::NotAvailable,
            "Claude Free has no plan usage to track",
        ));
    }
    if let Some(exp_ms) = i64_of(&oauth["expiresAt"]) {
        if exp_ms / 1000 < now_unix() {
            return Err(FetchError::new(
                Problem::TokenExpired,
                "Claude token expired; run `claude` once to refresh",
            ));
        }
    }

    let auth = format!("Bearer {token}");
    let user_agent = claude_user_agent(version);
    let (status, json, retry_after) = get_json_retry(
        "https://api.anthropic.com/api/oauth/usage",
        &[
            ("Authorization", &auth),
            ("anthropic-beta", "oauth-2025-04-20"),
            ("User-Agent", &user_agent),
        ],
    )?;
    match status {
        200 => Ok(json),
        401 | 403 => Err(FetchError::new(
            Problem::TokenRejected,
            "Claude token rejected; run `claude` once to refresh",
        )
        .into()),
        429 => Err(ClaudeError::RateLimited(retry_after.unwrap_or(0))),
        s => Err(FetchError::new(Problem::Service, format!("Anthropic returned HTTP {s}")).into()),
    }
}

    claude_usage(&json, matches!(plan, "pro" | "max"))
}

fn claude_usage(json: &Value, personal: bool) -> Result<Usage, FetchError> {
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
                resets_at: time_of(&w["resets_at"]),
            });
        }
    }
    if personal {
        if meters.is_empty() {
            return Err(FetchError::new(
                Problem::BadResponse,
                "no usage windows in response",
            ));
        }
        return Ok(Usage { meters, personal });
    }

    // Per-model weekly caps (e.g. Fable) are only reported in `limits`.
    for l in json["limits"].as_array().into_iter().flatten() {
        if l["kind"].as_str() != Some("weekly_scoped") {
            continue;
        }
        let (Some(model), Some(pct)) = (
            l["scope"]["model"]["display_name"].as_str(),
            f64_of(&l["percent"]),
        ) else {
            continue;
        };
        let label = format!("{} wk", model.to_lowercase());
        if meters.iter().any(|m| m.label.as_deref() == Some(&label)) {
            continue;
        }
        meters.push(Meter {
            label: Some(label),
            used: pct,
            total: 100.0,
            unit: Unit::Percent,
            resets_at: l["resets_at"].as_str().and_then(parse_rfc3339),
        });
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
            resets_at: None,
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
                resets_at: None,
            });
        }
    }

    if meters.is_empty() {
        return Err(FetchError::new(
            Problem::BadResponse,
            "no usage windows in response",
        ));
    }
    Ok(Usage { meters, personal })
}

// ---------------------------------------------------------------------------
// Codex (Codex CLI ChatGPT token)
// ---------------------------------------------------------------------------

/// Like Claude Code, the Codex CLI only refreshes its ChatGPT token while it runs,
/// so an expired token is reported until `codex` is run again.
fn codex() -> Result<Usage, FetchError> {
    let path = home()?.join(".codex").join("auth.json");
    let auth_file = read_json_file(&path, "log in with `codex`")?;
    let tokens = &auth_file["tokens"];
    // Signed in with an API key rather than a ChatGPT plan.
    let token = tokens["access_token"].as_str().ok_or_else(|| {
        FetchError::new(
            Problem::NotAvailable,
            "no tokens.access_token; log in with `codex`",
        )
    })?;
    if let Some(exp) = jwt_exp(token) {
        if exp < now_unix() {
            return Err(FetchError::new(
                Problem::TokenExpired,
                "Codex token expired; run `codex` once to refresh",
            ));
        }
    }
    let account_id = tokens["account_id"].as_str().unwrap_or("");

    let auth = format!("Bearer {token}");
    let mut headers: Vec<(&str, &str)> = vec![("Authorization", &auth)];
    if !account_id.is_empty() {
        headers.push(("ChatGPT-Account-Id", account_id));
    }
    let (status, json) = get_json("https://chatgpt.com/backend-api/wham/usage", &headers)?;
    let observed_at = now_unix();
    match status {
        200 => {}
        401 | 403 => {
            return Err(FetchError::new(
                Problem::TokenRejected,
                "Codex token rejected; run `codex` once to refresh",
            ));
        }
        s => {
            return Err(FetchError::new(
                Problem::Service,
                format!("OpenAI returned HTTP {s}"),
            ));
        }
    }

    codex_usage(&json)
}

fn codex_usage(json: &Value) -> Result<Usage, FetchError> {
    let plan = json["plan_type"].as_str().unwrap_or("");
    if plan == "free" {
        return Err(FetchError::new(
            Problem::NotAvailable,
            "ChatGPT Free has no paid plan to track",
        ));
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
            let resets_at = time_of(&w["reset_at"])
                .or_else(|| i64_of(&w["reset_after_seconds"]).map(|s| now_unix() + s));
            meters.push(Meter {
                label: Some(label),
                used: pct,
                total: 100.0,
                unit: Unit::Percent,
                resets_at,
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
            resets_at: None,
        });
    }

    if meters.is_empty() {
        if json["credits"]["unlimited"].as_bool() == Some(true) {
            return Ok(Usage {
                meters: vec![Meter {
                    label: Some("unlimited".into()),
                    used: 0.0,
                    total: 0.0,
                    unit: Unit::Percent,
                    resets_at: None,
                }],
                personal: false,
            });
        }
        return Err(FetchError::new(
            Problem::BadResponse,
            "no rate limit or spend data in response",
        ));
    }
    Ok(Usage { meters, personal })
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

/// "max" -> "Max", "business_plus" -> "Business Plus".
fn title_case(s: &str) -> String {
    s.split(['_', ' '])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut chars = w.chars();
            chars
                .next()
                .map(|c| c.to_uppercase().chain(chars).collect::<String>())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn not_available<T>(r: Result<T, FetchError>) -> bool {
        matches!(r, Err(e) if e.problem == Problem::NotAvailable)
    }

    fn pcts(u: &Usage) -> Vec<(String, f64)> {
        u.meters
            .iter()
            .map(|m| (m.label.clone().unwrap_or_default(), m.percent()))
            .collect()
    }

    #[test]
    fn account_switch_resets_backoff_and_distrusts_older_statusline() {
        let mut state = ClaudeState::default();
        // The first account seen trusts the statusline file as it is.
        assert!(note_account(&mut state, Some("a"), 500));
        assert_eq!(state.account_since, 0);
        assert!(!note_account(&mut state, Some("a"), 600));
        assert!(!note_account(&mut state, None, 600));
        state.blocked_until = 5000;
        state.last_attempt = 900;
        // Switched to B, noticed at 1000: A's backoff no longer applies, and
        // statusline files written before 1000 may be A's.
        assert!(note_account(&mut state, Some("b"), 1000));
        assert_eq!(state.account_since, 1000);
        assert_eq!((state.blocked_until, state.last_attempt), (0, 0));
    }

    #[test]
    fn manual_renewal_uses_local_dates_and_rejects_expired_or_invalid_dates() {
        use chrono::{Local, NaiveDate, TimeZone};
        let today = NaiveDate::from_ymd_opt(2026, 9, 12).unwrap();
        let cycle = manual_claude_cycle("2026-10-12", today).unwrap();
        assert_eq!(cycle.verb, "renews");
        assert_eq!(
            Local.timestamp_opt(cycle.at, 0).unwrap().date_naive(),
            NaiveDate::from_ymd_opt(2026, 10, 12).unwrap()
        );
        assert!(manual_claude_cycle("2026-09-12", today).is_ok());
        assert_eq!(
            manual_claude_cycle("2026-09-12", today)
                .unwrap()
                .countdown(today),
            "today"
        );
        assert_eq!(
            manual_claude_cycle("2026-09-14", today)
                .unwrap()
                .countdown(today),
            "2d"
        );
        assert!(!cycle.when().contains("12am"));
        for date in ["2026-09-11", "2026-02-30", "2026-9-15", "invalid", ""] {
            assert!(manual_claude_cycle(date, today).is_err(), "{date}");
        }
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
            let usage = copilot_usage(&json).unwrap();
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
        let usage = copilot_usage(&json).unwrap();
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
        let usage = copilot_usage(&json).unwrap();
        assert!(usage.personal);
        assert_eq!(pcts(&usage), [("mo".to_string(), 25.0)]);

        let free = serde_json::json!({ "copilot_plan": "free", "quota_snapshots": {} });
        assert!(not_available(copilot_usage(&free)));
    }

    #[test]
    fn copilot_without_quota_is_not_available() {
        let json = serde_json::json!({ "quota_snapshots": {} });
        assert!(not_available(copilot_usage(&json)));
    }

    #[test]
    fn missing_credentials_file_is_not_available() {
        let path = std::env::temp_dir().join("usage-widget-test-missing.json");
        assert!(not_available(read_json_file(&path, "log in")));
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
        let usage = codex_usage(&plus).unwrap();
        assert!(usage.personal);
        assert_eq!(
            pcts(&usage),
            [("5h".to_string(), 30.0), ("wk".to_string(), 60.0)]
        );

        let business = serde_json::json!({ "plan_type": "business", "rate_limit": null, "spend_control": spend });
        let usage = codex_usage(&business).unwrap();
        assert!(!usage.personal);
        assert_eq!(usage.meters[0].summary(), "$50 / $500");

        let free = serde_json::json!({ "plan_type": "free", "rate_limit": windows });
        assert!(not_available(codex_usage(&free)));
    }

    #[test]
    fn formatting() {
        assert_eq!(whole_percent(71.37), None);
        assert_eq!(whole_percent(87.0), Some(87.0));
        assert_eq!(whole_percent(55.00000000000001), Some(55.0));
        assert_eq!(money(62_440.0 * COPILOT_USD_PER_CREDIT), "624.40");
        assert_eq!(money(12_500.0 * CODEX_USD_PER_CREDIT), "500");
        assert_eq!(money(518.94), "518.94");
        assert_eq!(money(1000.0), "1,000");
        assert_eq!(money(12_345.5), "12,345.50");
        assert_eq!(window_label(18_000, "week"), "5h");
        assert_eq!(window_label(604_800, "week"), "week");
    }

    fn labels(meters: &[Meter]) -> Vec<(&str, f64, Option<i64>)> {
        meters
            .iter()
            .map(|m| (m.label.as_deref().unwrap_or(""), m.used, m.resets_at))
            .collect()
    }

    #[test]
    fn claude_windows_and_scoped_caps() {
        let json = serde_json::json!({
            "five_hour": { "utilization": 0.0, "resets_at": "2026-09-11T13:40:00.838217+00:00" },
            "seven_day": { "utilization": 53.0, "resets_at": "2026-09-15T20:00:00.838238+00:00" },
            "seven_day_opus": null,
            "extra_usage": { "monthly_limit": null, "used_credits": null },
            "limits": [
                { "kind": "session", "percent": 0, "scope": null },
                { "kind": "weekly_all", "percent": 53, "scope": null },
                {
                    "kind": "weekly_scoped",
                    "percent": 100,
                    "resets_at": "2026-09-15T19:59:59.838420+00:00",
                    "scope": { "model": { "display_name": "Fable" }, "surface": null }
                }
            ],
            "spend": { "used": { "amount_minor": 0, "exponent": 2 }, "limit": null }
        });
        assert_eq!(
            labels(&claude_meters(&json)),
            [
                ("5h", 0.0, Some(1_789_134_000)),
                ("week", 53.0, Some(1_789_502_400)),
                ("fable wk", 100.0, Some(1_789_502_399)),
            ]
        );
    }

    #[test]
    fn codex_weekly_only() {
        let json = serde_json::json!({
            "rate_limit": {
                "primary_window": {
                    "used_percent": 65,
                    "limit_window_seconds": 604800,
                    "reset_after_seconds": 346322,
                    "reset_at": 1789462965
                },
                "secondary_window": null
            },
            "spend_control": { "individual_limit": null }
        });
        assert_eq!(
            labels(&codex_meters(&json)),
            [("week", 65.0, Some(1_789_462_965))]
        );
    }

    /// Extraction keeps whatever precision a service sends; rounding happens only
    /// when the widget formats the number.
    #[test]
    fn fractional_percents_survive_extraction() {
        use serde_json::json;
        let codex = codex_meters(&json!({
            "rate_limit": { "primary_window": { "used_percent": 71.37, "limit_window_seconds": 604800 } }
        }));
        assert_eq!(codex[0].used, 71.37);

        let mut claude = claude_meters(&json!({ "five_hour": { "utilization": 33.25 } }));
        assert_eq!(claude[0].used, 33.25);
        apply_statusline(
            &mut claude,
            &json!({ "five_hour": { "used_percentage": 12.34 } }),
        );
        assert_eq!(claude[0].used, 12.34);
    }

    #[test]
    fn statusline_overrides_windows() {
        use serde_json::json;
        let mut meters = claude_meters(&json!({
            "five_hour": { "utilization": 3.0, "resets_at": "2026-09-11T13:40:00Z" },
            "seven_day": { "utilization": 53.0, "resets_at": "2026-09-15T20:00:00Z" },
            "limits": [{
                "kind": "weekly_scoped",
                "percent": 100,
                "scope": { "model": { "display_name": "Fable" } }
            }]
        }));
        apply_statusline(
            &mut meters,
            &json!({
                "five_hour": { "used_percentage": 41.5, "resets_at": 1_789_134_000 },
                "seven_day": { "used_percentage": 60, "resets_at": 1_789_502_400_000_i64 }
            }),
        );
        assert_eq!(
            labels(&meters),
            [
                ("5h", 41.5, Some(1_789_134_000)),
                ("week", 60.0, Some(1_789_502_400)),
                ("fable wk", 100.0, None),
            ]
        );

        let mut only = Vec::new();
        apply_statusline(&mut only, &json!({ "seven_day": { "used_percentage": 7 } }));
        assert_eq!(labels(&only), [("week", 7.0, None)]);
    }

    #[test]
    fn claude_staleness_names_old_meters() {
        let now = now_unix();
        let labels = ["5h", "week", "fable wk"];
        let note = |live, snap, labels: &[&str]| claude_staleness(now, live, snap, labels);
        assert_eq!(note(Some(now - 5), Some(now - 60), &labels), None);
        // Fresh statusline, old full response: only the caps are old.
        assert_eq!(
            note(Some(now - 5), Some(now - 1800), &labels).as_deref(),
            Some("fable wk 30m old")
        );
        assert_eq!(note(Some(now - 5), Some(now - 1800), &["5h", "week"]), None);
        // No statusline, or an old one: everything is as old as the newest source.
        assert_eq!(
            note(None, Some(now - 1800), &labels).as_deref(),
            Some("30m old")
        );
        assert_eq!(
            note(Some(now - 1800), Some(now - 3600), &labels).as_deref(),
            Some("30m old")
        );
    }

    #[test]
    fn monthly_spend_hides_age_but_keeps_stale_errors() {
        let now = now_unix();
        for total in [0.0, 100.0] {
            let meters = claude_meters(&serde_json::json!({
                "extra_usage": { "used_credits": 10, "monthly_limit": total }
            }));
            assert!(spend_meter(&meters).is_some());
            let note =
                |at, blocked, problem| claude_note(now, None, Some(at), &meters, blocked, problem);
            assert_eq!(note(now - 1800, 0, None), None);
            assert_eq!(note(now - 60, now + 900, None), None);
            assert_eq!(note(now - 60, 0, Some("offline".into())), None);
            assert!(
                note(now - 1800, now + 900, None)
                    .unwrap()
                    .starts_with("rate limited")
            );
            assert_eq!(
                note(now - 1800, 0, Some("offline".into())).as_deref(),
                Some("offline")
            );
        }
        let mixed = claude_meters(&serde_json::json!({
            "five_hour": { "utilization": 20 },
            "extra_usage": { "used_credits": 10, "monthly_limit": 100 }
        }));
        assert!(spend_meter(&mixed).is_none());
        assert_eq!(
            claude_note(now, None, Some(now - 1800), &mixed, 0, None).as_deref(),
            Some("30m old")
        );
    }

    #[test]
    fn renewal_looked_up_for_another_organization_is_dropped() {
        let config = crate::config::Claude::default();
        let mut state = ClaudeState {
            renewal: Some((
                0,
                Cycle {
                    verb: "renews".into(),
                    at: 1,
                    date_only: true,
                },
            )),
            renewal_org: Some("a".into()),
            renewal_attempt: 5,
            ..Default::default()
        };
        let (cycle, _) = claude_renewal(&config, &mut state, 10, &[], Some("a"), None);
        assert!(cycle.is_none()); // browser_cookies is off
        assert!(state.renewal.is_some());
        claude_renewal(&config, &mut state, 10, &[], Some("b"), None);
        assert!(state.renewal.is_none());
        assert_eq!(state.renewal_attempt, 0);
    }

    #[test]
    fn renewal_looked_up_for_another_plan_is_dropped() {
        let config = crate::config::Claude::default();
        let cycle = Cycle {
            verb: "renews".into(),
            at: 1,
            date_only: false,
        };
        let mut state = ClaudeState {
            renewal: Some((0, cycle)),
            renewal_plan: Some("Max 5x".into()),
            renewal_attempt: 5,
            ..Default::default()
        };
        claude_renewal(&config, &mut state, 10, &[], None, Some("Max 5x"));
        assert!(state.renewal.is_some());
        claude_renewal(&config, &mut state, 10, &[], None, Some("Max 20x"));
        assert!(state.renewal.is_none());
        assert_eq!(state.renewal_attempt, 0);
    }

    #[test]
    fn monthly_spend_skips_manual_and_browser_renewal() {
        for total in [0.0, 100.0] {
            for date in ["invalid", "2099-01-01"] {
                let config = crate::config::Claude {
                    browser_cookies: true,
                    renewal_date: Some(date.into()),
                    ..Default::default()
                };
                let mut state = ClaudeState::default();
                state.renewal = Some((
                    0,
                    Cycle {
                        verb: "renews".into(),
                        at: 1,
                        date_only: true,
                    },
                ));
                let meters = claude_meters(&serde_json::json!({
                    "extra_usage": { "used_credits": 10, "monthly_limit": total }
                }));
                let (cycle, error) =
                    claude_renewal(&config, &mut state, now_unix(), &meters, None, None);
                assert!(cycle.is_none());
                assert!(error.is_none());
                assert_eq!(state.renewal_attempt, 0);
            }
        }
    }

    #[test]
    fn claude_call_gating() {
        let now = 1_000_000;
        let idle = ClaudeState::default();
        assert!(!claude_should_call(true, now, Some(now - 60), &idle));
        assert!(!claude_should_call(
            true,
            now,
            Some(now - CLAUDE_POLL_SECS + 1),
            &idle
        ));
        assert!(claude_should_call(
            true,
            now,
            Some(now - CLAUDE_POLL_SECS),
            &idle
        ));
        assert!(claude_should_call(true, now, None, &idle));
        assert!(!claude_should_call(false, now, None, &idle));
        let blocked = ClaudeState {
            blocked_until: now + 60,
            ..Default::default()
        };
        assert!(!claude_should_call(true, now, None, &blocked));
        let recent = ClaudeState {
            last_attempt: now - 60,
            ..Default::default()
        };
        assert!(!claude_should_call(true, now, None, &recent));
    }

    #[test]
    fn claude_user_agent_names_claude_code() {
        assert_eq!(claude_user_agent(Some("2.1.300")), "claude-code/2.1.300");
        let fallback = format!("claude-code/{CLAUDE_CODE_VERSION}");
        assert_eq!(claude_user_agent(None), fallback);
        assert_eq!(claude_user_agent(Some("2.1.x")), fallback);
        assert_eq!(claude_user_agent(Some("")), fallback);
    }

    #[test]
    fn plan_names() {
        use serde_json::json;
        let claude = |sub: &str, tier: &str| {
            claude_plan(&json!({ "subscriptionType": sub, "rateLimitTier": tier }))
        };
        assert_eq!(
            claude("max", "default_claude_max_5x").as_deref(),
            Some("Max 5x")
        );
        assert_eq!(
            claude("max", "default_claude_max_20x").as_deref(),
            Some("Max 20x")
        );
        assert_eq!(claude("pro", "default_claude_ai").as_deref(), Some("Pro"));
        // Claude Code's profile names the kind "claude_pro" / "claude_max".
        assert_eq!(
            plan_name("claude_pro", Some("default_claude_ai")).as_deref(),
            Some("Pro")
        );
        assert_eq!(
            plan_name("claude_max", Some("default_claude_max_20x")).as_deref(),
            Some("Max 20x")
        );
        assert_eq!(claude("", "").as_deref(), None);

        let copilot = |plan: &str, sku: &str| {
            copilot_plan(&json!({ "copilot_plan": plan, "access_type_sku": sku }))
        };
        assert_eq!(
            copilot("enterprise", "copilot_enterprise_seat_quota").as_deref(),
            Some("Enterprise")
        );
        assert_eq!(
            copilot("individual", "copilot_pro_plus_monthly").as_deref(),
            Some("Pro+")
        );
        assert_eq!(
            copilot("individual", "monthly_subscriber").as_deref(),
            Some("Pro")
        );

        let codex = |plan: &str| codex_plan(&json!({ "plan_type": plan }));
        assert_eq!(codex("prolite").as_deref(), Some("Pro Lite"));
        assert_eq!(codex("plus").as_deref(), Some("Plus"));
    }
}
