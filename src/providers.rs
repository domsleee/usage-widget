//! Fetches usage for each service using credentials already stored by the
//! corresponding CLI (gh, Claude Code, Codex). The only thing persisted is
//! Claude's last API response, rate-limit backoff and renewal date (see `ClaudeState`).

use crate::config::Config;
use crate::timeutil::{age, now_unix, parse_rfc3339, until};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

const UA: &str = "usage-widget/0.1 (+https://github.com/pepsi-enjoyer/usage-widget)";

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

    pub fn url(self) -> &'static str {
        match self {
            Provider::Copilot => "https://github.com/settings/copilot/features",
            Provider::Claude => "https://claude.ai/new#settings/usage",
            Provider::Codex => "https://chatgpt.com/#settings/Usage",
        }
    }

    pub fn fetch(self, config: &Config) -> Result<Usage, String> {
        match self {
            Provider::Copilot => copilot(),
            Provider::Claude => claude(&config.claude),
            Provider::Codex => codex(config.codex.estimate),
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

#[derive(Clone, Debug)]
pub struct Meter {
    /// Optional sub-label when a provider has more than one meter (e.g. "5h", "week").
    pub label: Option<String>,
    pub used: f64,
    pub total: f64,
    pub unit: Unit,
    /// Unix timestamp when a rolling window (5h, week) resets. Monthly spend leaves this unset.
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

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(25)))
        .http_status_as_error(false)
        .user_agent(UA)
        .build()
        .into()
}

fn get_json(url: &str, headers: &[(&str, &str)]) -> Result<(u16, Value), String> {
    get_json_retry(url, headers).map(|(status, json, _)| (status, json))
}

/// Like `get_json`, plus the `Retry-After` seconds when the server sends them.
fn get_json_retry(
    url: &str,
    headers: &[(&str, &str)],
) -> Result<(u16, Value, Option<i64>), String> {
    let mut req = agent().get(url).header("Accept", "application/json");
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let mut resp = req.call().map_err(|e| format!("request failed: {e}"))?;
    let status = resp.status().as_u16();
    let retry_after = resp
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse().ok());
    let text = resp
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("read failed: {e}"))?;
    let json = serde_json::from_str::<Value>(&text).unwrap_or(Value::Null);
    Ok((status, json, retry_after))
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
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
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

fn copilot() -> Result<Usage, String> {
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

    let plan = copilot_plan(&json);
    // Seats are billed to the org or enterprise; the monthly quota reset is the only date.
    let cycle = json["quota_reset_date_utc"]
        .as_str()
        .and_then(parse_rfc3339)
        .map(|at| Cycle {
            date_only: false,
            verb: "resets".into(),
            at,
        });

    if snap["unlimited"].as_bool() == Some(true) {
        return Ok(Usage {
            plan,
            cycle,
            note: None,
            estimated: Vec::new(),
            meters: vec![Meter {
                label: Some("unlimited".into()),
                used: 0.0,
                total: 0.0,
                unit: Unit::Percent,
                resets_at: None,
            }],
        });
    }

    let total = f64_of(&snap["entitlement"]).unwrap_or(0.0);
    let used = f64_of(&snap["credits_used"])
        .or_else(|| f64_of(&snap["remaining"]).map(|r| total - r))
        .unwrap_or(0.0);
    Ok(Usage {
        plan,
        cycle,
        note: None,
        estimated: Vec::new(),
        meters: vec![Meter {
            label: None,
            used: used * COPILOT_USD_PER_CREDIT,
            total: total * COPILOT_USD_PER_CREDIT,
            unit: Unit::Dollars,
            resets_at: None,
        }],
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

fn claude_credentials() -> Result<Value, String> {
    let path = home()?.join(".claude").join(".credentials.json");
    #[cfg(target_os = "macos")]
    if !path.exists() {
        return claude_keychain_credentials();
    }
    read_json_file(&path)
}

/// Claude Code on macOS keeps its credentials in the login keychain, not on disk.
#[cfg(target_os = "macos")]
fn claude_keychain_credentials() -> Result<Value, String> {
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
        return Err("no Claude Code credentials in keychain; log in with `claude`".into());
    }
    serde_json::from_slice(&out.stdout).map_err(|e| format!("bad JSON in keychain entry: {e}"))
}

/// Anthropic gives each OAuth token only a handful of usage calls before
/// answering 429 for up to an hour, and Claude Code spends the same budget.
/// Claude Code caches its own responses in ~/.claude.json, so read that and call
/// the API only when it is stale, and no more than once per this many seconds.
const CLAUDE_FRESH_SECS: i64 = 15 * 60;

#[derive(Clone, Serialize, Deserialize)]
struct Snapshot {
    at: i64,
    usage: Value,
    /// The Claude account it belongs to, so a login switch doesn't show old usage.
    #[serde(default)]
    account: Option<String>,
}

/// Our last API response, rate-limit backoff and renewal date, kept across restarts.
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct ClaudeState {
    last: Option<Snapshot>,
    last_attempt: i64,
    blocked_until: i64,
    /// The claude.ai renewal (`browser_cookies`) and when it was fetched.
    renewal: Option<(i64, Cycle)>,
    renewal_attempt: i64,
}

impl ClaudeState {
    fn path() -> Option<PathBuf> {
        dirs::cache_dir().map(|d| d.join("usage-widget").join("claude.json"))
    }

    fn load() -> Self {
        Self::path()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    fn save(&self) {
        let Some(path) = Self::path() else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(bytes) = serde_json::to_vec(self) {
            let _ = std::fs::write(path, bytes);
        }
    }
}

/// Claude Code hands the statusline command live `rate_limits` taken from each
/// response's headers; a statusline script can save that JSON here (see README).
fn claude_statusline() -> Option<(i64, Value)> {
    let path = home()
        .ok()?
        .join(".claude")
        .join("usage-widget-statusline.json");
    let modified = std::fs::metadata(&path).ok()?.modified().ok()?;
    let at = modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    let mut v = read_json_file(&path).ok()?;
    let rate_limits = v["rate_limits"].take();
    rate_limits.is_object().then_some((at, rate_limits))
}

/// Overwrites (or adds) the 5h and weekly meters from statusline `rate_limits`.
fn apply_statusline(meters: &mut Vec<Meter>, rate_limits: &Value) {
    for (i, (key, label)) in [("five_hour", "5h"), ("seven_day", "week")]
        .into_iter()
        .enumerate()
    {
        let w = &rate_limits[key];
        let Some(used) = f64_of(&w["used_percentage"]) else {
            continue;
        };
        // Seconds per the docs; tolerate milliseconds.
        let resets_at =
            i64_of(&w["resets_at"]).map(|t| if t > 100_000_000_000 { t / 1000 } else { t });
        match meters
            .iter_mut()
            .find(|m| m.label.as_deref() == Some(label))
        {
            Some(m) => {
                m.used = used;
                m.resets_at = resets_at.or(m.resets_at);
            }
            None => meters.insert(
                i.min(meters.len()),
                Meter {
                    label: Some(label.into()),
                    used,
                    total: 100.0,
                    unit: Unit::Percent,
                    resets_at,
                },
            ),
        }
    }
}

/// The "old" note for Claude. Live statusline figures cover only 5h and week, so
/// when they are fresh, name the meters that came from the older full response.
fn claude_staleness(
    now: i64,
    live_at: Option<i64>,
    snap_at: Option<i64>,
    labels: &[&str],
) -> Option<String> {
    match (live_at, snap_at) {
        (Some(live), Some(snap)) if now - live < CLAUDE_FRESH_SECS => {
            let others: Vec<&str> = labels
                .iter()
                .copied()
                .filter(|l| !matches!(*l, "5h" | "week"))
                .collect();
            (now - snap >= CLAUDE_FRESH_SECS && !others.is_empty())
                .then(|| format!("{} {} old", others.join(", "), age(snap)))
        }
        (live, snap) => {
            let at = live.into_iter().chain(snap).max()?;
            (now - at >= CLAUDE_FRESH_SECS).then(|| format!("{} old", age(at)))
        }
    }
}

fn claude_should_call(api: bool, now: i64, newest: Option<i64>, state: &ClaudeState) -> bool {
    let fresh = newest.is_some_and(|at| now - at < CLAUDE_FRESH_SECS);
    api && !fresh && now >= state.blocked_until && now - state.last_attempt >= CLAUDE_FRESH_SECS
}

fn claude(config: &crate::config::Claude) -> Result<Usage, String> {
    let api = config.api;
    let creds = claude_credentials();
    let oauth = creds.as_ref().map(|c| &c["claudeAiOauth"]);
    let (cache, account) = claude_code_state();
    let mut state = ClaudeState::load();
    let now = now_unix();
    // Our saved response is only good for the account it was fetched for.
    if account.is_some() && state.last.as_ref().is_some_and(|s| s.account != account) {
        state.last = None;
    }

    // The statusline refreshes only 5h and week, so it doesn't make the rest fresh.
    let statusline = claude_statusline();
    let newest = [&cache, &state.last]
        .into_iter()
        .flatten()
        .map(|s| s.at)
        .max();
    let mut problem = None;
    if claude_should_call(api, now, newest, &state) {
        state.last_attempt = now;
        match claude_api(oauth) {
            Ok(usage) => {
                state.last = Some(Snapshot {
                    at: now,
                    usage,
                    account: account.clone(),
                })
            }
            Err(ClaudeError::RateLimited(retry_after)) => {
                state.blocked_until = now + retry_after.max(CLAUDE_FRESH_SECS);
            }
            Err(ClaudeError::Other(e)) => problem = Some(e),
        }
        state.save();
    }

    let manual_cycle = config
        .renewal_date
        .as_deref()
        .map(|date| manual_claude_cycle(date, chrono::Local::now().date_naive()));
    let mut renewal_problem = manual_cycle
        .as_ref()
        .and_then(|c| c.as_ref().err())
        .cloned();
    if config.browser_cookies && !matches!(&manual_cycle, Some(Ok(_))) {
        // Fetch daily and again once the saved date has passed; after a failure retry
        // at most hourly, so a denied keychain prompt doesn't return every refresh.
        let due = state
            .renewal
            .as_ref()
            .is_none_or(|(fetched, c)| now - fetched >= 86_400 || now >= c.at);
        if due && now - state.renewal_attempt >= 3600 {
            state.renewal_attempt = now;
            match crate::claude_web::cycle() {
                Ok(c) => state.renewal = Some((now, c)),
                Err(e) => {
                    renewal_problem = Some(renewal_problem.map_or_else(
                        || e.clone(),
                        |manual| format!("{manual}; browser lookup: {e}"),
                    ))
                }
            }
            state.save();
        }
    }

    let rate_limited = now < state.blocked_until;
    let snap = [cache, state.last]
        .into_iter()
        .flatten()
        .max_by_key(|s| s.at);
    let mut meters = snap
        .as_ref()
        .map(|s| claude_meters(&s.usage))
        .unwrap_or_default();
    let snap_at = snap.as_ref().map(|s| s.at);
    // Live 5h/weekly numbers from the statusline beat an older full response.
    let mut live_at = None;
    if let Some((at, rate_limits)) = statusline
        && snap_at.is_none_or(|d| at > d)
    {
        apply_statusline(&mut meters, &rate_limits);
        live_at = Some(at);
    }
    // When the 5h and week readings were taken.
    let Some(data_at) = live_at.or(snap_at).filter(|_| !meters.is_empty()) else {
        return Err(if rate_limited {
            format!(
                "Claude rate limited; retrying in {}",
                until(state.blocked_until)
            )
        } else {
            problem.unwrap_or_else(|| "no Claude usage cached yet; open Claude Code".into())
        });
    };

    // Claude reports whole percents; estimate the part of the next one from local use.
    let mut estimated = Vec::new();
    if config.estimate {
        let plan = oauth.ok().and_then(claude_plan).unwrap_or_default();
        // Rounded: statusline figures carry float noise (55.00000000000001).
        let readings: Vec<(String, f64, i64)> = meters
            .iter()
            .filter(|m| {
                m.unit == Unit::Percent && matches!(m.label.as_deref(), Some("5h" | "week"))
            })
            .filter_map(|m| Some((m.label.clone()?, whole_percent(m.used)?, m.resets_at?)))
            .collect();
        let refs: Vec<(&str, f64, i64)> = readings
            .iter()
            .map(|(label, pct, window)| (label.as_str(), *pct, *window))
            .collect();
        let fractions = crate::claude_estimate::fractions(&refs, data_at, &plan);
        for ((label, pct, _), fraction) in readings.iter().zip(fractions) {
            if let Some(fraction) = fraction
                && let Some(m) = meters.iter_mut().find(|m| m.label.as_ref() == Some(label))
            {
                m.used = pct + fraction;
                estimated.push(label.clone());
            }
        }
    }

    let labels: Vec<&str> = meters.iter().filter_map(|m| m.label.as_deref()).collect();
    let note = claude_staleness(now, live_at, snap_at, &labels).map(|mut note| {
        if rate_limited {
            note += &format!(" · rate limited · retry {}", until(state.blocked_until));
        } else if let Some(p) = problem {
            note += &format!(" · {p}");
        }
        note
    });
    let cycle = match manual_cycle {
        Some(Ok(cycle)) => Some(cycle),
        _ => state
            .renewal
            .filter(|_| config.browser_cookies)
            .map(|(_, c)| c),
    };
    // Say why the renewal date is missing, but only when there is none to show.
    let note = match renewal_problem.filter(|_| cycle.is_none()) {
        Some(e) => Some(note.map_or(format!("renewal: {e}"), |n| format!("{n} · renewal: {e}"))),
        None => note,
    };
    Ok(Usage {
        plan: oauth.ok().and_then(claude_plan),
        cycle,
        note,
        estimated,
        meters,
    })
}

/// A manually supplied next billing date, without guessing the billing cadence.
pub(crate) fn manual_claude_cycle(date: &str, today: chrono::NaiveDate) -> Result<Cycle, String> {
    use chrono::{Local, NaiveDate, TimeZone};

    let parsed = NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .ok()
        .filter(|d| d.format("%Y-%m-%d").to_string() == date)
        .ok_or("set claude.renewal_date to a valid YYYY-MM-DD date")?;
    if parsed < today {
        return Err("the renewal date has passed; use the refresh icon beside Claude to look it up again, or update claude.renewal_date".into());
    }
    let at = Local
        .from_local_datetime(&parsed.and_hms_opt(0, 0, 0).unwrap())
        .earliest()
        .ok_or("claude.renewal_date has no local midnight; choose another date")?
        .timestamp();
    Ok(Cycle {
        date_only: true,
        verb: "renews".into(),
        at,
    })
}

/// From ~/.claude.json: Claude Code's latest /api/oauth/usage response, if it
/// belongs to the logged-in account, and that account's id.
fn claude_code_state() -> (Option<Snapshot>, Option<String>) {
    let Some(v) = home()
        .ok()
        .and_then(|h| read_json_file(&h.join(".claude.json")).ok())
    else {
        return (None, None);
    };
    let account = v["oauthAccount"]["accountUuid"].as_str().map(String::from);
    let c = &v["cachedUsageUtilization"];
    // Ignore a cache left behind by another account.
    let cache = (account.is_none() || c["accountUuid"].as_str() == account.as_deref())
        .then(|| {
            Some(Snapshot {
                at: i64_of(&c["fetchedAtMs"])? / 1000,
                usage: c["utilization"].clone(),
                account: account.clone(),
            })
        })
        .flatten();
    (cache, account)
}

enum ClaudeError {
    /// Seconds from `Retry-After`, 0 when absent.
    RateLimited(i64),
    Other(String),
}

fn claude_api(oauth: Result<&Value, &String>) -> Result<Value, ClaudeError> {
    use ClaudeError::Other;
    let oauth = oauth.map_err(|e| Other(e.clone()))?;
    let token = oauth["accessToken"]
        .as_str()
        .ok_or_else(|| Other("no claudeAiOauth.accessToken; log in with `claude`".into()))?;
    if i64_of(&oauth["expiresAt"]).is_some_and(|ms| ms / 1000 < now_unix()) {
        return Err(Other(
            "Claude token expired; run `claude` once to refresh".into(),
        ));
    }

    let auth = format!("Bearer {token}");
    let (status, json, retry_after) = get_json_retry(
        "https://api.anthropic.com/api/oauth/usage",
        &[
            ("Authorization", &auth),
            ("anthropic-beta", "oauth-2025-04-20"),
        ],
    )
    .map_err(Other)?;
    match status {
        200 => Ok(json),
        401 | 403 => Err(Other(
            "Claude token rejected; run `claude` once to refresh".into(),
        )),
        429 => Err(ClaudeError::RateLimited(retry_after.unwrap_or(0))),
        s => Err(Other(format!("Anthropic returned HTTP {s}"))),
    }
}

fn claude_plan(oauth: &Value) -> Option<String> {
    let name = title_case(
        oauth["subscriptionType"]
            .as_str()
            .filter(|s| !s.is_empty())?,
    );
    // rateLimitTier looks like "default_claude_max_5x"; keep the "5x".
    let tier = oauth["rateLimitTier"]
        .as_str()
        .and_then(|t| t.rsplit('_').next())
        .filter(|t| {
            t.strip_suffix('x')
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        });
    Some(match tier {
        Some(t) => format!("{name} {t}"),
        None => name,
    })
}

fn claude_meters(json: &Value) -> Vec<Meter> {
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
                resets_at: w["resets_at"].as_str().and_then(parse_rfc3339),
            });
        }
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
    meters
}

// ---------------------------------------------------------------------------
// Codex (Codex CLI ChatGPT token)
// ---------------------------------------------------------------------------

fn codex(estimate: bool) -> Result<Usage, String> {
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

    let plan = codex_plan(&json);
    let cycle = codex_cycle(&headers, account_id);
    let mut meters = codex_meters(&json);
    if meters.is_empty() {
        if json["credits"]["unlimited"].as_bool() == Some(true) {
            return Ok(Usage {
                plan,
                cycle,
                note: None,
                estimated: Vec::new(),
                meters: vec![Meter {
                    label: Some("unlimited".into()),
                    used: 0.0,
                    total: 0.0,
                    unit: Unit::Percent,
                    resets_at: None,
                }],
            });
        }
        return Err("no rate limit or spend data in response".into());
    }

    // Codex reports whole percents; estimate the part of the next one from local use.
    let mut estimated = Vec::new();
    if estimate {
        let plan_type = json["plan_type"].as_str().unwrap_or_default();
        if let Some(week) = meters
            .iter_mut()
            .find(|m| m.label.as_deref() == Some("week") && m.unit == Unit::Percent)
            && let Some(window) = week.resets_at
            && let Some(reported) = whole_percent(week.used)
            && let Some(fraction) = crate::codex_estimate::fraction(reported, window, plan_type)
        {
            week.used = reported + fraction.clamp(0.0, 0.99);
            estimated.extend(week.label.clone());
        }
    }
    Ok(Usage {
        plan,
        cycle,
        note: None,
        estimated,
        meters,
    })
}

/// When the ChatGPT plan renews, or ends once cancelled, from the subscription record.
fn codex_cycle(headers: &[(&str, &str)], account_id: &str) -> Option<Cycle> {
    if account_id.is_empty() {
        return None;
    }
    let url = format!("https://chatgpt.com/backend-api/subscriptions?account_id={account_id}");
    let (200, sub) = get_json(&url, headers).ok()? else {
        return None;
    };
    let at = sub["active_until"].as_str().and_then(parse_rfc3339)?;
    let verb = if sub["will_renew"].as_bool() == Some(false) {
        "ends"
    } else {
        "renews"
    };
    Some(Cycle {
        date_only: false,
        verb: verb.into(),
        at,
    })
}

// Fractional service readings are already more precise than this estimator.
fn whole_percent(value: f64) -> Option<f64> {
    (value.is_finite() && (value - value.round()).abs() < 1e-9).then(|| value.round())
}

fn codex_plan(json: &Value) -> Option<String> {
    Some(
        match json["plan_type"].as_str().filter(|s| !s.is_empty())? {
            "prolite" => "Pro Lite".into(),
            other => title_case(other),
        },
    )
}

fn codex_meters(json: &Value) -> Vec<Meter> {
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
                resets_at: i64_of(&w["reset_at"])
                    .or_else(|| i64_of(&w["reset_after_seconds"]).map(|s| now_unix() + s)),
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
            resets_at: None,
        });
    }
    meters
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
    fn formatting() {
        assert_eq!(whole_percent(71.37), None);
        assert_eq!(whole_percent(87.0), Some(87.0));
        assert_eq!(whole_percent(55.00000000000001), Some(55.0));
        assert_eq!(money(62_440.0 * COPILOT_USD_PER_CREDIT), "624.40");
        assert_eq!(money(12_500.0 * CODEX_USD_PER_CREDIT), "500");
        assert_eq!(money(518.94), "518.94");
        assert_eq!(money(1000.0), "1,000");
        assert_eq!(money(12_345.5), "12,345.50");
        assert_eq!(window_label(18_000), "5h");
        assert_eq!(window_label(604_800), "week");
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
    fn claude_call_gating() {
        let now = 1_000_000;
        let idle = ClaudeState::default();
        assert!(!claude_should_call(true, now, Some(now - 60), &idle));
        assert!(claude_should_call(
            true,
            now,
            Some(now - CLAUDE_FRESH_SECS),
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
