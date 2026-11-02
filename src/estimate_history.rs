//! A log of each estimate's inputs and result: token counts and readings, never
//! prompt or response text. It lets the estimators be replayed and tuned offline.

use serde::Serialize;
use std::fs::OpenOptions;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

pub const ESTIMATOR_VERSION: &str = "cumulative-v2";

/// Daily files older than this are deleted when a new record is written.
const KEEP_DAYS: i64 = 30;

#[derive(Serialize)]
struct Record<'a, T> {
    schema_version: u32,
    estimator_version: &'static str,
    app_version: &'static str,
    provider: &'a str,
    /// When this batch became available to the estimator, not when usage occurred.
    received_at: &'a str,
    batch: &'a T,
}

/// Best effort: callers ignore a failure, so a full disk or a file another
/// process holds never stops the estimate itself.
pub fn append(provider: &str, received_at: &str, batch: &impl Serialize) -> Result<(), String> {
    let record = Record {
        schema_version: 1,
        estimator_version: ESTIMATOR_VERSION,
        app_version: env!("CARGO_PKG_VERSION"),
        provider,
        received_at,
        batch,
    };
    let write = || -> io::Result<()> {
        let root = crate::paths::data_local_dir()
            .ok_or_else(|| io::Error::other("local data directory unavailable"))?
            .join("usage-widget")
            .join("history");
        std::fs::create_dir_all(&root)?;
        let today = chrono::Utc::now().date_naive();
        prune(&root, today);
        let day = today.format("%Y-%m-%d");
        append_to(&root.join(format!("{provider}-{day}.jsonl")), &record)
    };
    write().map_err(|e| format!("estimate history: {e}"))
}

/// Deletes daily files (`<provider>-YYYY-MM-DD.jsonl`) older than `KEEP_DAYS`.
fn prune(root: &Path, today: chrono::NaiveDate) {
    let cutoff = today - chrono::Duration::days(KEEP_DAYS);
    for entry in std::fs::read_dir(root).into_iter().flatten().flatten() {
        let path = entry.path();
        let day = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.get(s.len().saturating_sub(10)..))
            .and_then(|d| chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").ok());
        if path.extension().is_some_and(|e| e == "jsonl") && day.is_some_and(|d| d < cutoff) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

fn append_to(path: &Path, record: &impl Serialize) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(record)?;
    bytes.push(b'\n');
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(path)?;
    // Never wait: another process holding the file would stall every refresh.
    file.try_lock().map_err(|e| match e {
        std::fs::TryLockError::WouldBlock => io::Error::other("history file is in use"),
        std::fs::TryLockError::Error(e) => e,
    })?;
    // A crash can leave a half-written last line. Keep it, but start this record
    // on a new line so the two don't run together; readers skip lines that don't parse.
    if file.metadata()?.len() > 0 {
        file.seek(SeekFrom::End(-1))?;
        let mut last = [0];
        file.read_exact(&mut last)?;
        if last[0] != b'\n' {
            file.write_all(b"\n")?;
        }
    }
    file.write_all(&bytes)?;
    file.sync_data()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn appends_across_reopens_and_preserves_interrupted_records() {
        let path = std::env::temp_dir().join(format!(
            "usage-widget-history-{}-{}.jsonl",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        append_to(&path, &json!({"batch": 1})).unwrap();
        append_to(&path, &json!({"batch": 2})).unwrap();
        let before = std::fs::read(&path).unwrap();
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"interrupted\":")
            .unwrap();
        append_to(&path, &json!({"batch": 3})).unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.as_bytes().starts_with(&before));
        let lines: Vec<_> = after.lines().collect();
        assert_eq!(lines.len(), 4);
        assert!(serde_json::from_str::<serde_json::Value>(lines[2]).is_err());
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(lines[3]).unwrap(),
            json!({"batch": 3})
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn prunes_old_daily_files_only() {
        let root = std::env::temp_dir().join(format!(
            "usage-widget-prune-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let today = chrono::NaiveDate::from_ymd_opt(2026, 11, 2).unwrap();
        for name in [
            "codex-2026-09-01.jsonl",
            "claude-2026-10-03.jsonl",
            "claude-2026-10-02.jsonl",
            "notes.jsonl",
        ] {
            std::fs::write(root.join(name), b"").unwrap();
        }
        prune(&root, today);
        let mut left: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, ["claude-2026-10-03.jsonl", "notes.jsonl"]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn write_failure_is_returned() {
        assert!(append_to(&std::env::temp_dir(), &serde_json::json!({})).is_err());
    }
}
