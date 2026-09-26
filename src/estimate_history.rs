//! A log of each estimate's inputs and result: token counts and readings, never
//! prompt or response text. It lets the estimators be replayed and tuned offline.

use serde::Serialize;
use std::fs::OpenOptions;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

pub const ESTIMATOR_VERSION: &str = "cumulative-v1";

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

/// Call this before saving how far the logs have been read, so that if it fails
/// the same inputs are read again next time.
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
        let day = chrono::Utc::now().format("%Y-%m-%d");
        append_to(&root.join(format!("{provider}-{day}.jsonl")), &record)
    };
    write().map_err(|e| format!("estimate history: {e}"))
}

fn append_to(path: &Path, record: &impl Serialize) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(record)?;
    bytes.push(b'\n');
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(path)?;
    file.lock()?;
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
    fn write_failure_is_returned() {
        assert!(append_to(&std::env::temp_dir(), &serde_json::json!({})).is_err());
    }
}
