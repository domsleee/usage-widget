//! Sub-percent estimate for Codex's weekly window.
//!
//! Codex reports `used_percent` in whole points. Each refresh this reads what the
//! Codex CLI's session logs (~/.codex/sessions) gained since the last one and
//! prices that token use with the published credit rate card. Every log reading
//! also carries the account's weekly percent, so `Tracker` learns what a point
//! costs and estimates the part of the next one. Usage the logs can't see (Codex
//! cloud, other devices) makes it lag, so it's a guess and the widget marks it.

use crate::estimate::Tracker;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// The first scan looks this far back: the current week and the one before.
const HISTORY: Duration = Duration::from_secs(15 * 86_400);
const WEEK_MINUTES: i64 = 7 * 24 * 60;

/// Credits per million tokens (input, cached input, output) from the Codex rate
/// card. Models not on it use GPT-5.6 Sol's rates.
fn rate_card(model: &str) -> (f64, f64, f64) {
    match model {
        "gpt-6-astra" => (250.0, 25.0, 1250.0),
        "gpt-5.6-terra" => (50.0, 5.0, 300.0),
        "gpt-5.6-luna" => (5.0, 0.5, 30.0),
        _ => (100.0, 10.0, 500.0),
    }
}

/// The estimated share of the next point already used (0.0 to 0.99), given the
/// server's whole `pct` for the weekly window that resets at `window`.
pub fn fraction(pct: f64, window: i64, plan: &str) -> Option<f64> {
    let mut state = State::load();
    let mut events = state.scan();
    events.sort_by(|a, b| a.ts.cmp(&b.ts));
    for e in &events {
        state.week.apply(e.pct, e.window, &e.plan, e.credits);
    }
    state.week.observe(pct, window, plan);
    let result = state.week.fraction(pct, window);
    state.save();
    result
}

/// Where the scan stopped in one session log, plus what it needs to turn the
/// log's running totals into per-response usage.
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Cursor {
    offset: u64,
    session: String,
    model: String,
    /// Running totals: input, cached input, output.
    totals: [u64; 3],
}

#[derive(Debug, PartialEq)]
struct Event {
    ts: String,
    credits: f64,
    pct: f64,
    window: i64,
    plan: String,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct State {
    files: HashMap<String, Cursor>,
    /// Highest running totals seen per session, so a session resumed into a new
    /// log file isn't counted twice.
    sessions: HashMap<String, [u64; 3]>,
    week: Tracker,
}

impl State {
    fn path() -> Option<PathBuf> {
        dirs::cache_dir().map(|d| d.join("usage-widget").join("codex-estimate.json"))
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

    /// New `token_count` events from recent session logs. Files are compared by
    /// size against where the last scan stopped, so a failed read is retried and a
    /// file that shrank (rewritten) is read again from the start.
    fn scan(&mut self) -> Vec<Event> {
        let Some(root) = dirs::home_dir().map(|h| h.join(".codex").join("sessions")) else {
            return Vec::new();
        };
        let horizon = SystemTime::now() - HISTORY;
        let mut events = Vec::new();
        for path in logs_under(&root) {
            let Ok(meta) = path.metadata() else {
                continue;
            };
            if meta.modified().map_or(true, |m| m < horizon) {
                continue;
            }
            let cursor = self
                .files
                .entry(path.to_string_lossy().into_owned())
                .or_default();
            if meta.len() < cursor.offset {
                *cursor = Cursor::default();
            }
            if meta.len() > cursor.offset {
                let sessions = &mut self.sessions;
                cursor.offset = read_lines(&path, cursor.offset, |line| {
                    parse_line(line, cursor, sessions, &mut events)
                });
            }
        }
        // Forget logs too old to matter so the state stays small.
        self.files.retain(|p, _| modified_since(p, horizon));
        let live: HashSet<&str> = self.files.values().map(|c| c.session.as_str()).collect();
        self.sessions.retain(|id, _| live.contains(id.as_str()));
        events
    }
}

pub(crate) fn modified_since(path: &str, horizon: SystemTime) -> bool {
    Path::new(path)
        .metadata()
        .and_then(|m| m.modified())
        .is_ok_and(|m| m >= horizon)
}

/// Every `.jsonl` anywhere under `root`.
pub(crate) fn logs_under(root: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![root.to_path_buf()];
    let mut logs = Vec::new();
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().is_some_and(|e| e == "jsonl") {
                logs.push(path);
            }
        }
    }
    logs
}

/// Feeds the complete lines after `offset` to `each`, leaving a half-written last
/// line for the next scan. Returns the new offset (unchanged if the read fails).
pub(crate) fn read_lines(path: &Path, offset: u64, mut each: impl FnMut(&str)) -> u64 {
    let Ok(mut file) = std::fs::File::open(path) else {
        return offset;
    };
    let mut bytes = Vec::new();
    if file.seek(SeekFrom::Start(offset)).is_err() || file.read_to_end(&mut bytes).is_err() {
        return offset;
    }
    let complete = bytes.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    for line in String::from_utf8_lossy(&bytes[..complete]).lines() {
        each(line);
    }
    offset + complete as u64
}

/// Raises each running total in `to` to at least the one in `from`.
fn raise(to: &mut [u64; 3], from: &[u64; 3]) {
    for (t, f) in to.iter_mut().zip(from) {
        *t = (*t).max(*f);
    }
}

fn parse_line(
    line: &str,
    cursor: &mut Cursor,
    sessions: &mut HashMap<String, [u64; 3]>,
    events: &mut Vec<Event>,
) {
    let kinds = ["\"session_meta\"", "\"turn_context\"", "\"token_count\""];
    if !kinds.iter().any(|k| line.contains(k)) {
        return;
    }
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return;
    };
    let payload = &v["payload"];
    match v["type"].as_str() {
        Some("session_meta") => {
            if let Some(id) = payload["id"].as_str() {
                cursor.session = id.to_string();
                // A resumed session carries its running totals on, maybe in a new
                // file: start from what was already counted.
                if let Some(seen) = sessions.get(id) {
                    raise(&mut cursor.totals, seen);
                }
            }
            return;
        }
        Some("turn_context") => {
            if let Some(model) = payload["model"].as_str() {
                cursor.model = model.to_string();
            }
            return;
        }
        _ => {}
    }
    let totals = &payload["info"]["total_token_usage"];
    if payload["type"] != "token_count" || !totals.is_object() {
        return;
    }
    let now = ["input_tokens", "cached_input_tokens", "output_tokens"]
        .map(|k| totals[k].as_u64().unwrap_or(0));
    let [input, cached, output] = [0, 1, 2].map(|i| now[i].saturating_sub(cursor.totals[i]));
    raise(&mut cursor.totals, &now);
    if !cursor.session.is_empty() {
        raise(
            sessions.entry(cursor.session.clone()).or_default(),
            &cursor.totals,
        );
    }

    let limits = &payload["rate_limits"];
    let Some(week) = [&limits["primary"], &limits["secondary"]]
        .into_iter()
        .find(|w| w["window_minutes"].as_i64() == Some(WEEK_MINUTES))
    else {
        return;
    };
    let (Some(pct), Some(window)) = (week["used_percent"].as_f64(), week["resets_at"].as_i64())
    else {
        return;
    };
    // `input_tokens` includes the cached ones.
    let (rate_in, rate_cached, rate_out) = rate_card(&cursor.model);
    let credits = (input.saturating_sub(cached) as f64 * rate_in
        + cached as f64 * rate_cached
        + output as f64 * rate_out)
        / 1e6;
    events.push(Event {
        ts: v["timestamp"].as_str().unwrap_or_default().to_string(),
        credits,
        pct,
        window,
        plan: limits["plan_type"].as_str().unwrap_or_default().to_string(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count_line(input: u64, cached: u64, output: u64) -> String {
        format!(
            r#"{{"timestamp":"t","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":{input},"cached_input_tokens":{cached},"output_tokens":{output}}}}},"rate_limits":{{"plan_type":"prolite","primary":{{"used_percent":72.0,"window_minutes":10080,"resets_at":99}}}}}}}}"#
        )
    }

    #[test]
    fn prices_log_lines() {
        let (mut cursor, mut sessions, mut events) =
            (Cursor::default(), HashMap::new(), Vec::new());
        let turn = r#"{"type":"turn_context","payload":{"model":"gpt-6-astra"}}"#;
        parse_line(turn, &mut cursor, &mut sessions, &mut events);
        parse_line(
            &count_line(1_000_000, 0, 0),
            &mut cursor,
            &mut sessions,
            &mut events,
        );
        // Running totals: the second response adds 1M cached input and 1M output.
        let second = count_line(2_000_000, 1_000_000, 1_000_000);
        parse_line(&second, &mut cursor, &mut sessions, &mut events);
        let credits: Vec<f64> = events.iter().map(|e| e.credits).collect();
        assert_eq!(credits, [250.0, 25.0 + 1250.0]);
        assert_eq!(events[1].plan, "prolite");
        assert_eq!((events[1].pct, events[1].window), (72.0, 99));
    }

    #[test]
    fn resumed_session_is_not_counted_twice() {
        let (mut sessions, mut events) = (HashMap::new(), Vec::new());
        let meta = r#"{"type":"session_meta","payload":{"id":"s1"}}"#;
        let mut first = Cursor::default();
        parse_line(meta, &mut first, &mut sessions, &mut events);
        parse_line(
            &count_line(1_000_000, 0, 0),
            &mut first,
            &mut sessions,
            &mut events,
        );
        // The resumed file repeats the old running total, then adds 1M more input.
        let mut resumed = Cursor::default();
        parse_line(meta, &mut resumed, &mut sessions, &mut events);
        parse_line(
            &count_line(1_000_000, 0, 0),
            &mut resumed,
            &mut sessions,
            &mut events,
        );
        parse_line(
            &count_line(2_000_000, 0, 0),
            &mut resumed,
            &mut sessions,
            &mut events,
        );
        let credits: Vec<f64> = events.iter().map(|e| e.credits).collect();
        assert_eq!(credits, [100.0, 0.0, 100.0]);
    }
}
