//! Sub-percent estimate for Claude's 5-hour and weekly windows.
//!
//! Claude reports whole percents, and Claude Code's transcripts
//! (~/.claude/projects, subagents included) record each response's tokens but not
//! the percent. So each refresh this prices what the transcripts gained by
//! Anthropic's relative model prices, and `Tracker` learns what a point costs from
//! the widget's own whole-percent readings. Claude used outside Claude Code on
//! this machine (claude.ai, the desktop app, other devices) isn't seen.

use crate::codex_estimate::{logs_under, modified_since, read_lines};
use crate::estimate::Tracker;
use crate::timeutil::parse_rfc3339;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How far back transcripts are read, and response ids remembered.
const HISTORY: Duration = Duration::from_secs(8 * 86_400);

/// Relative price per million tokens: input, cache write, cache read, output.
/// Only the ratios matter; each window learns its own cost per point.
fn prices(model: &str) -> [f64; 4] {
    if model.contains("haiku") {
        [1.0, 1.25, 0.1, 5.0]
    } else if model.contains("sonnet") {
        [3.0, 3.75, 0.3, 15.0]
    } else {
        [5.0, 6.25, 0.5, 25.0] // Opus, Fable and newer
    }
}

/// For each `(label, whole percent, window reset time)` reading, taken at
/// `observed_at`, the estimated share of the next point already used.
pub fn fractions(readings: &[(&str, f64, i64)], observed_at: i64, plan: &str) -> Vec<Option<f64>> {
    let mut state = State::load();
    let mut events = state.scan();
    events.sort_by(|a, b| a.0.total_cmp(&b.0));
    // Use logged before the reading was taken led up to it; the rest came after.
    let split = events.partition_point(|e| e.0 <= observed_at as f64);
    let result = readings
        .iter()
        .map(|&(label, pct, window)| {
            let tracker = state.windows.entry(label.to_string()).or_default();
            events[..split].iter().for_each(|e| tracker.spend(e.1));
            tracker.observe(pct, window, plan);
            events[split..].iter().for_each(|e| tracker.spend(e.1));
            tracker.fraction(pct, window)
        })
        .collect();
    state.save();
    result
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct State {
    /// Where the last scan stopped in each transcript.
    files: HashMap<String, u64>,
    /// Response ids already counted, with their time, since each streamed content
    /// block is logged with the same usage.
    seen: HashMap<String, i64>,
    /// One tracker per window label ("5h", "week").
    windows: HashMap<String, Tracker>,
}

impl State {
    fn path() -> Option<PathBuf> {
        dirs::cache_dir().map(|d| d.join("usage-widget").join("claude-estimate.json"))
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

    /// New responses as `(unix time, priced cost)`, from transcripts that grew.
    fn scan(&mut self) -> Vec<(f64, f64)> {
        let Some(root) = dirs::home_dir().map(|h| h.join(".claude").join("projects")) else {
            return Vec::new();
        };
        let now = SystemTime::now();
        let horizon = now - HISTORY;
        let mut events = Vec::new();
        for path in logs_under(&root) {
            let Ok(meta) = path.metadata() else {
                continue;
            };
            if meta.modified().map_or(true, |m| m < horizon) {
                continue;
            }
            let offset = self
                .files
                .entry(path.to_string_lossy().into_owned())
                .or_default();
            if meta.len() < *offset {
                *offset = 0; // rewritten; `seen` stops it being counted twice
            }
            if meta.len() > *offset {
                let seen = &mut self.seen;
                *offset = read_lines(&path, *offset, |line| parse_line(line, seen, &mut events));
            }
        }
        self.files.retain(|p, _| modified_since(p, horizon));
        let cutoff = now
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64)
            - HISTORY.as_secs() as i64;
        self.seen.retain(|_, ts| *ts >= cutoff);
        events
    }
}

fn parse_line(line: &str, seen: &mut HashMap<String, i64>, events: &mut Vec<(f64, f64)>) {
    if !line.contains("\"usage\"") || !line.contains("\"assistant\"") {
        return;
    }
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return;
    };
    let message = &v["message"];
    let usage = &message["usage"];
    if v["type"] != "assistant" || !usage.is_object() {
        return;
    }
    let (Some(id), Some(ts)) = (
        message["id"].as_str(),
        v["timestamp"].as_str().and_then(parse_rfc3339),
    ) else {
        return;
    };
    if seen.insert(id.to_string(), ts).is_some() {
        return;
    }
    // Anthropic's `input_tokens` excludes the cached ones.
    let tokens = |k: &str| usage[k].as_u64().unwrap_or(0) as f64;
    let p = prices(message["model"].as_str().unwrap_or_default());
    let cost = (tokens("input_tokens") * p[0]
        + tokens("cache_creation_input_tokens") * p[1]
        + tokens("cache_read_input_tokens") * p[2]
        + tokens("output_tokens") * p[3])
        / 1e6;
    events.push((ts as f64, cost));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prices_each_response_once() {
        let line = |id: &str, model: &str| {
            format!(
                r#"{{"type":"assistant","timestamp":"2026-09-11T08:50:19Z","message":{{"id":"{id}","model":"{model}","usage":{{"input_tokens":1000000,"cache_creation_input_tokens":1000000,"cache_read_input_tokens":1000000,"output_tokens":1000000}}}}}}"#
            )
        };
        let (mut seen, mut events) = (HashMap::new(), Vec::new());
        parse_line(&line("a", "claude-opus-5"), &mut seen, &mut events);
        parse_line(&line("a", "claude-opus-5"), &mut seen, &mut events); // same response, next block
        parse_line(&line("b", "claude-haiku-4-5"), &mut seen, &mut events);
        let costs: Vec<f64> = events.iter().map(|e| e.1).collect();
        assert_eq!(costs, [5.0 + 6.25 + 0.5 + 25.0, 1.0 + 1.25 + 0.1 + 5.0]);
        // 2026-09-11T13:40:00Z is 1_789_134_000; 08:50:19 is 4h 49m 41s earlier.
        assert_eq!(events[0].0, 1_789_116_619.0);
    }
}
