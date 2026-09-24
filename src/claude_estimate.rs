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
pub fn fractions(
    readings: &[(&str, f64, i64)],
    observed_at: i64,
    plan: &str,
    source: &str,
) -> Result<Vec<Option<f64>>, String> {
    let mut state = State::load();
    let mut events = state.scan();
    let received_at = chrono::Utc::now().to_rfc3339();
    events.sort_by_key(|e| e.ts);
    let mut batch = Batch {
        before: state.windows.clone(),
        events,
        readings: readings
            .iter()
            .map(|&(label, pct, window)| (label.to_string(), pct, window))
            .collect(),
        source_at: observed_at,
        source: source.to_string(),
        plan: plan.to_string(),
        estimates: Vec::new(),
    };
    let (windows, result) = batch.replay();
    state.windows = windows;
    batch.estimates = readings
        .iter()
        .zip(&result)
        .map(|((_, pct, _), fraction)| fraction.map(|f| pct.round() + f))
        .collect();
    crate::estimate_history::append("claude", &received_at, &batch)?;
    state.save();
    Ok(result)
}

#[derive(Serialize, Deserialize)]
struct Batch {
    before: HashMap<String, Tracker>,
    events: Vec<Event>,
    readings: Vec<(String, f64, i64)>,
    source_at: i64,
    source: String,
    plan: String,
    /// Predicted percentages, in reading order; null means no local estimate.
    estimates: Vec<Option<f64>>,
}

impl Batch {
    fn replay(&self) -> (HashMap<String, Tracker>, Vec<Option<f64>>) {
        let mut windows = self.before.clone();
        // Preserve the original estimator's second-resolution timestamp split.
        let split = self.events.partition_point(|e| e.ts <= self.source_at);
        let result = self
            .readings
            .iter()
            .map(|(label, pct, window)| {
                crate::providers::whole_percent(*pct).and_then(|pct| {
                    let tracker = windows.entry(label.clone()).or_default();
                    self.events[..split]
                        .iter()
                        .for_each(|e| tracker.spend(e.cost));
                    tracker.observe(pct, *window, &self.plan);
                    self.events[split..]
                        .iter()
                        .for_each(|e| tracker.spend(e.cost));
                    tracker.fraction(pct, *window)
                })
            })
            .collect();
        (windows, result)
    }
}

#[derive(Serialize, Deserialize)]
struct Event {
    ts: i64,
    source_at: String,
    response_id: String,
    model: String,
    /// Input (excluding cache), cache creation, cache read, output.
    tokens: [u64; 4],
    cost: f64,
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
        crate::paths::cache_dir().map(|d| d.join("usage-widget").join("claude-estimate.json"))
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

    /// New responses from transcripts that grew.
    fn scan(&mut self) -> Vec<Event> {
        let Some(root) = std::env::home_dir().map(|h| h.join(".claude").join("projects")) else {
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

fn parse_line(line: &str, seen: &mut HashMap<String, i64>, events: &mut Vec<Event>) {
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
    let tokens = [
        "input_tokens",
        "cache_creation_input_tokens",
        "cache_read_input_tokens",
        "output_tokens",
    ]
    .map(|k| usage[k].as_u64().unwrap_or(0));
    let p = prices(message["model"].as_str().unwrap_or_default());
    let cost = (tokens[0] as f64 * p[0]
        + tokens[1] as f64 * p[1]
        + tokens[2] as f64 * p[2]
        + tokens[3] as f64 * p[3])
        / 1e6;
    events.push(Event {
        ts,
        source_at: v["timestamp"].as_str().unwrap_or_default().to_string(),
        response_id: id.to_string(),
        model: message["model"].as_str().unwrap_or_default().to_string(),
        tokens,
        cost,
    });
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
        let costs: Vec<f64> = events.iter().map(|e| e.cost).collect();
        assert_eq!(costs, [5.0 + 6.25 + 0.5 + 25.0, 1.0 + 1.25 + 0.1 + 5.0]);
        // 2026-09-11T13:40:00Z is 1_789_134_000; 08:50:19 is 4h 49m 41s earlier.
        assert_eq!(events[0].ts, 1_789_116_619);
        assert_eq!(events[0].source_at, "2026-09-11T08:50:19Z");
        assert_eq!(events[0].response_id, "a");
        assert_eq!(events[0].model, "claude-opus-5");
        assert_eq!(events[0].tokens, [1_000_000; 4]);
    }

    #[test]
    fn saved_batch_preserves_reading_boundary_and_replays_both_windows() {
        let window = 1_789_462_965;
        let mut tracker = Tracker::default();
        tracker.observe(10.0, window, "Max 5x");
        for pct in [11.0, 12.0, 13.0] {
            tracker.spend(8.0);
            tracker.observe(pct, window, "Max 5x");
        }
        let event = |ts, cost| Event {
            ts,
            source_at: format!("2026-09-12T08:50:{ts:02}.500Z"),
            response_id: format!("response-{ts}"),
            model: "claude-sonnet".into(),
            tokens: [100, 0, 0, 10],
            cost,
        };
        let mut batch = Batch {
            before: HashMap::from([("5h".into(), tracker.clone()), ("week".into(), tracker)]),
            events: vec![event(19, 8.0), event(21, 2.0)],
            readings: vec![("5h".into(), 14.0, window), ("week".into(), 13.0, window)],
            source_at: 20,
            source: "statusline".into(),
            plan: "Max 5x".into(),
            estimates: vec![],
        };
        let (_, original) = batch.replay();
        assert_eq!(original, [Some(0.25), Some(0.99)]);
        batch.estimates = batch
            .readings
            .iter()
            .zip(original)
            .map(|((_, pct, _), f)| f.map(|f| pct + f))
            .collect();
        let restored: Batch =
            serde_json::from_str(&serde_json::to_string(&batch).unwrap()).unwrap();
        let replayed: Vec<_> = restored
            .readings
            .iter()
            .zip(restored.replay().1)
            .map(|((_, pct, _), f)| f.map(|f| pct + f))
            .collect();
        assert_eq!(replayed, restored.estimates);
        batch.readings[0].1 = 14.25;
        assert_eq!(batch.replay().1[0], None);
    }
}
