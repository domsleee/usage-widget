//! Small time helpers so we don't need chrono for a couple of conversions.

use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// "5m ago" style age for a past timestamp.
pub fn ago(ts: i64) -> String {
    let diff = (now_unix() - ts).max(0);
    if diff < 60 {
        "just now".into()
    } else if diff < 3600 {
        format!("{}m ago", diff / 60)
    } else {
        format!("{}h {}m ago", diff / 3600, (diff % 3600) / 60)
    }
}
