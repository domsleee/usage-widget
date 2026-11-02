//! Fetches usage for each service using credentials already stored by the
//! corresponding CLI (gh, Claude Code, Codex). Nothing is persisted here.

use crate::timeutil::{now_unix, parse_rfc3339};
use base64::Engine;
use serde_json::Value;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const UA: &str = "usage-widget/1.0 (+https://github.com/pepsi-enjoyer/usage-widget)";

/// Copilot premium-request credits: 50,000 credits = $500.
const COPILOT_USD_PER_CREDIT: f64 = 500.0 / 50_000.0;
/// Codex workspace spend credits: 12,500 credits = $500.
const CODEX_USD_PER_CREDIT: f64 = 500.0 / 12_500.0;
/// Cheapest Codex model, for the headless run that refreshes its token.
const CODEX_REFRESH_MODEL: &str = "gpt-6-luna";
/// How long a headless CLI run may take before it is given up on.
const HEADLESS_TIMEOUT: Duration = Duration::from_secs(90);

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

    /// Whether the token comes from a CLI that only refreshes it while it runs,
    /// which `refresh_token` can make it do. `gh auth token` does not expire.
    pub fn refreshes_token(self) -> bool {
        matches!(self, Provider::Claude | Provider::Codex)
    }

    /// Runs a one-line headless prompt on the CLI's cheapest model, so it
    /// refreshes its token and writes it back for the next fetch to pick up.
    /// This spends a little usage, so it is only done when asked for.
    pub fn refresh_token(self) -> Result<(), String> {
        match self {
            Provider::Copilot => Err("the gh token does not expire".into()),
            Provider::Claude => run_headless(
                "claude",
                &[
                    "-p",
                    "--model",
                    "haiku",
                    "--effort",
                    "low",
                    "--max-turns",
                    "1",
                    "--no-session-persistence",
                    "--strict-mcp-config",
                    "--tools",
                    "",
                    "--disable-slash-commands",
                    "--setting-sources",
                    "",
                    "--system-prompt",
                    "Reply with ok.",
                    "ok",
                ],
            ),
            Provider::Codex => {
                // The user config is skipped so a costly default model or effort
                // and MCP servers are not used for it.
                let run = |model: &[&str]| {
                    let mut args = vec![
                        "exec",
                        "--skip-git-repo-check",
                        "--ephemeral",
                        "--ignore-user-config",
                        "--ignore-rules",
                        "--sandbox",
                        "read-only",
                        "-c",
                        "model_reasoning_effort=\"low\"",
                    ];
                    args.extend(model);
                    args.push("Reply with ok.");
                    run_headless("codex", &args)
                };
                // Once that model is retired, the default one still refreshes.
                run(&["-m", CODEX_REFRESH_MODEL]).or_else(|_| run(&[]))
            }
        }
    }

    /// `Problem::NotAvailable` when the account has no paid plan with this
    /// service (not signed in on this machine, or on a free plan), so the widget
    /// leaves it out.
    pub fn fetch(self) -> Result<Usage, FetchError> {
        match self {
            Provider::Copilot => copilot(),
            Provider::Claude => claude(),
            Provider::Codex => codex(),
        }
    }
}

/// What a service reported for the signed-in account.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Usage {
    pub meters: Vec<Meter>,
    /// A personal subscription (Claude Pro/Max, ChatGPT Plus/Pro, Copilot Pro).
    /// Its meters are percent windows, shown on one line with no dollar amounts.
    pub personal: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Unit {
    /// Amount in whole currency units.
    Dollars,
    /// Only a percentage is known.
    Percent,
}

#[derive(Clone, Debug, serde::Serialize)]
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

fn get_json(url: &str, headers: &[(&str, &str)]) -> Result<(u16, Value), FetchError> {
    let mut req = agent().get(url).header("Accept", "application/json");
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let mut resp = req
        .call()
        .map_err(|e| FetchError::new(Problem::Connection, format!("request failed: {e}")))?;
    let status = resp.status().as_u16();
    let text = resp
        .body_mut()
        .read_to_string()
        .map_err(|e| FetchError::new(Problem::Connection, format!("read failed: {e}")))?;
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

/// Runs a CLI with no window or input, and fails with the last line it wrote to
/// stderr if it does not succeed within `HEADLESS_TIMEOUT`.
fn run_headless(program: &str, args: &[&str]) -> Result<(), String> {
    let spawn = |program: &str| {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .current_dir(std::env::temp_dir())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        no_window(&mut cmd);
        cmd.spawn()
    };
    // npm installs the CLIs as .cmd shims, which Command does not look for.
    let mut child = spawn(program)
        .or_else(|_| spawn(&format!("{program}.cmd")))
        .map_err(|e| format!("could not run {program}: {e}"))?;
    // Read on another thread so a chatty CLI cannot fill the pipe and stall.
    let mut stderr = child.stderr.take();
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(e) = stderr.as_mut() {
            let _ = e.read_to_string(&mut text);
        }
        text
    });

    let deadline = Instant::now() + HEADLESS_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(250));
            }
            // The reader is left behind: a process the shim started may still
            // hold the pipe open.
            Ok(None) => {
                let _ = child.kill();
                return Err(format!("{program} timed out"));
            }
            Err(e) => return Err(format!("{program} did not finish: {e}")),
        }
    };
    if status.success() {
        return Ok(());
    }
    let text = reader.join().unwrap_or_default();
    Err(match text.lines().rev().find(|l| !l.trim().is_empty()) {
        Some(line) => format!("{program} failed: {}", line.trim()),
        None => format!("{program} failed ({status})"),
    })
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

// ---------------------------------------------------------------------------
// Claude (Claude Code OAuth token)
// ---------------------------------------------------------------------------

/// Claude Code on macOS keeps its credentials in the login keychain, not on
/// disk, under the item its own tooling writes.
#[cfg(target_os = "macos")]
fn claude_keychain_credentials() -> Result<Value, FetchError> {
    let out = Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            "Claude Code-credentials",
            "-w",
        ])
        .output()
        .map_err(|e| format!("cannot run `security` ({e})"))?;
    if !out.status.success() {
        return Err(FetchError::new(
            Problem::NotAvailable,
            "no Claude Code credentials in keychain; log in with `claude`",
        ));
    }
    serde_json::from_slice(&out.stdout)
        .map_err(|e| FetchError::from(format!("bad JSON in keychain entry: {e}")))
}

/// Claude Code only refreshes its OAuth token while it runs, so on an idle machine
/// the token expires. The widget never refreshes it itself, since that would
/// rotate the refresh token under Claude Code. It asks for Claude Code to be run,
/// by hand or with `Provider::refresh_token`, and picks the new token up on the
/// next refresh.
fn claude() -> Result<Usage, FetchError> {
    let path = home()?.join(".claude").join(".credentials.json");
    let creds = match read_json_file(&path, "log in with `claude`") {
        Ok(creds) => creds,
        Err(e) => {
            // Claude Code on macOS keeps its credentials in the login keychain,
            // not on disk.
            #[cfg(target_os = "macos")]
            if let Ok(creds) = claude_keychain_credentials() {
                creds
            } else {
                return Err(e);
            }
            #[cfg(not(target_os = "macos"))]
            return Err(e);
        }
    };
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
                "Claude token expired; right-click > Refresh token, or run `claude`",
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
            return Err(FetchError::new(
                Problem::TokenRejected,
                "Claude token rejected; right-click > Refresh token, or run `claude`",
            ));
        }
        s => {
            return Err(FetchError::new(
                Problem::Service,
                format!("Anthropic returned HTTP {s}"),
            ));
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
/// so an expired token is reported until `codex` is run again, by hand or with
/// `Provider::refresh_token`.
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
                "Codex token expired; right-click > Refresh token, or run `codex`",
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
    match status {
        200 => {}
        401 | 403 => {
            return Err(FetchError::new(
                Problem::TokenRejected,
                "Codex token rejected; right-click > Refresh token, or run `codex`",
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
        assert_eq!(money(62_440.0 * COPILOT_USD_PER_CREDIT), "624.40");
        assert_eq!(money(12_500.0 * CODEX_USD_PER_CREDIT), "500");
        assert_eq!(money(518.94), "518.94");
        assert_eq!(money(1000.0), "1,000");
        assert_eq!(money(12_345.5), "12,345.50");
        assert_eq!(window_label(18_000, "week"), "5h");
        assert_eq!(window_label(604_800, "week"), "week");
    }
}
