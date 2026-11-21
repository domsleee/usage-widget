use super::{FetchError, HEADLESS_TIMEOUT, Meter, Problem, Unit, Usage, no_window, wait_for_cli};
use crate::timeutil::parse_rfc3339;
use serde_json::Value;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::Instant;

fn executable() -> Option<PathBuf> {
    let name = if cfg!(windows) { "agy.exe" } else { "agy" };
    let path = std::env::var_os("PATH").unwrap_or_default();
    let on_path = std::env::split_paths(&path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name));
    let standard = if cfg!(windows) {
        crate::paths::data_local_dir().map(|dir| dir.join("agy/bin/agy.exe"))
    } else {
        std::env::home_dir().map(|dir| dir.join(".local/bin/agy"))
    };
    let homebrew = ["/opt/homebrew/bin/agy", "/usr/local/bin/agy"];
    on_path
        .chain(standard)
        .chain(
            homebrew
                .iter()
                .filter(|_| cfg!(target_os = "macos"))
                .map(PathBuf::from),
        )
        .find(|path| path.is_file())
}

pub(super) fn fetch() -> Result<Usage, FetchError> {
    let agy = executable().ok_or_else(|| {
        FetchError::new(
            Problem::NotAvailable,
            "Install the Antigravity CLI and sign in with agy.",
        )
    })?;
    let mut cmd = Command::new(agy);
    // In print mode disableSlashCommands=true would send /usage to the model.
    cmd.args([
        "--settings",
        r#"{"disableSlashCommands":false}"#,
        "-p",
        "/usage",
        "--output-format",
        "json",
    ])
    .env("AGY_CLI_DISABLE_AUTO_UPDATE", "true")
    .current_dir(std::env::temp_dir())
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    no_window(&mut cmd);
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Could not start agy: {e}"))?;
    let stdout = read_stdout(child.stdout.take().unwrap());
    let stderr = capture(child.stderr.take().unwrap());
    let deadline = Instant::now() + HEADLESS_TIMEOUT;
    let bytes = match stdout.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(Stdout::Usage(value)) => match parse(&value) {
            Ok(usage) => {
                std::thread::spawn(move || wait_for_cli(&mut child, "agy /usage", deadline));
                return Ok(usage);
            }
            Err(_) => Some(serde_json::to_vec(&value).map_err(std::io::Error::other)),
        },
        Ok(Stdout::Done(bytes)) => Some(bytes),
        Err(_) => None,
    };
    let status = wait_for_cli(&mut child, "agy /usage", deadline)?;
    let bytes = bytes
        .ok_or_else(|| "Timed out reading agy /usage".to_string())?
        .map_err(|e| format!("Could not read agy /usage: {e}"))?;
    let diagnostic = stderr
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
    parse_output(&bytes, &diagnostic, status)
}

enum Stdout {
    Usage(Value),
    Done(std::io::Result<Vec<u8>>),
}

fn read_stdout(pipe: impl Read + Send + 'static) -> mpsc::Receiver<Stdout> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut pipe = pipe.take(1024 * 1024);
        let mut bytes = Vec::new();
        let mut chunk = [0; 8192];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    bytes.extend_from_slice(&chunk[..n]);
                    if let Some(value) = successful_reading(&bytes) {
                        let _ = tx.send(Stdout::Usage(value));
                        let _ = std::io::copy(&mut pipe, &mut std::io::sink());
                        return;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => {
                    let _ = tx.send(Stdout::Done(Err(e)));
                    return;
                }
            }
        }
        let _ = tx.send(Stdout::Done(Ok(bytes)));
    });
    rx
}

fn successful_reading(bytes: &[u8]) -> Option<Value> {
    if bytes.trim_ascii_end().last() != Some(&b'}') {
        return None;
    }
    serde_json::from_slice::<Value>(bytes)
        .ok()
        .filter(|v| v["status"] == "SUCCESS")
}

fn parse_output(bytes: &[u8], diagnostic: &[u8], status: ExitStatus) -> Result<Usage, FetchError> {
    let value = serde_json::from_slice::<Value>(bytes);
    if !status.success()
        || value
            .as_ref()
            .is_ok_and(|v| v["status"].as_str().is_some_and(|s| s != "SUCCESS"))
    {
        return Err(cli_error(value.as_ref().ok(), diagnostic, status));
    }
    let value = value
        .map_err(|_| FetchError::new(Problem::BadResponse, "agy /usage returned invalid JSON."))?;
    parse(&value)
}

fn capture(pipe: impl Read + Send + 'static) -> mpsc::Receiver<std::io::Result<Vec<u8>>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = pipe.take(1024 * 1024).read_to_end(&mut bytes);
        let _ = tx.send(result.map(|_| bytes));
    });
    rx
}

fn cli_error(value: Option<&Value>, stderr: &[u8], status: ExitStatus) -> FetchError {
    let stderr = String::from_utf8_lossy(stderr);
    let Some(detail) = value
        .and_then(|v| v["error"].as_str())
        .filter(|s| !s.trim().is_empty())
        .or_else(|| stderr.lines().rev().map(str::trim).find(|s| !s.is_empty()))
    else {
        return FetchError::new(
            Problem::Service,
            format!("agy /usage failed ({status}). Open agy and check /usage."),
        );
    };
    let lower = detail.to_ascii_lowercase();
    if [
        "authentication required",
        "not authenticated",
        "not logged in",
        "not signed in",
    ]
    .iter()
    .any(|message| lower.contains(message))
    {
        return FetchError::new(
            Problem::NotAvailable,
            "Sign in with the Antigravity CLI (agy) to read quota.",
        );
    }
    FetchError::new(
        Problem::Service,
        format!(
            "agy /usage: {}",
            detail.chars().take(300).collect::<String>()
        ),
    )
}

fn meter(group: &str, window: &str, remaining: f64, reset: Option<&str>) -> Option<Meter> {
    if group.is_empty() || window.is_empty() || !(0.0..=1.0).contains(&remaining) {
        return None;
    }
    let lower = window.to_ascii_lowercase();
    let window = if lower == "5h" || lower.contains("five hour") {
        "5h"
    } else if lower.contains("weekly") {
        "weekly"
    } else {
        window.strip_suffix(" Remaining").unwrap_or(window)
    };
    Some(Meter {
        label: Some(format!("{group} {window}")),
        used: (1.0 - remaining) * 100.0,
        total: 100.0,
        unit: Unit::Percent,
        resets_at: reset.and_then(parse_rfc3339),
    })
}

fn parse(value: &Value) -> Result<Usage, FetchError> {
    let mut meters = Vec::new();
    if let Some(groups) = value
        .pointer("/command/data/groups")
        .and_then(Value::as_array)
    {
        for group in groups {
            let name = group["name"].as_str().unwrap_or_default();
            if let Some(buckets) = group["buckets"].as_array() {
                meters.extend(buckets.iter().filter_map(|bucket| {
                    let window = bucket["window"]
                        .as_str()
                        .or_else(|| bucket["name"].as_str())?;
                    meter(
                        name,
                        window,
                        bucket["remaining_fraction"].as_f64()?,
                        bucket["reset_time"].as_str(),
                    )
                }));
            }
        }
    }
    if meters.is_empty()
        && let Some(response) = value["response"].as_str()
    {
        meters.extend(response.lines().filter_map(|line| {
            let mut columns = line.split('\t').map(str::trim);
            let group = columns.next()?;
            let window = columns.next()?;
            let percent: f64 = columns.next()?.strip_suffix('%')?.trim().parse().ok()?;
            meter(group, window, percent / 100.0, columns.next())
        }));
    }
    if meters.is_empty() {
        return Err(FetchError::new(
            Problem::BadResponse,
            "agy /usage returned no readable quota windows.",
        ));
    }
    Ok(Usage {
        personal: true,
        plan: None,
        cycle: None,
        note: None,
        estimated: vec![],
        api_spend: None,
        meters,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_model_windows_and_converts_remaining_to_used() {
        let value = json!({"status": "SUCCESS", "command": {"data": {"groups": [
            {"name":"Gemini", "buckets":[
                {"window":"5h", "remaining_fraction":0.75, "reset_time":"2026-11-21T00:00:00Z"},
                {"window":"weekly", "remaining_fraction":0}
            ]},
            {"name":"Claude", "buckets":[{"window":"weekly", "remaining_fraction":1}, {"window":"unknown"}]}
        ]}}});
        let usage = parse(&value).unwrap();
        assert_eq!(usage.meters.len(), 3);
        assert_eq!(usage.meters[0].label.as_deref(), Some("Gemini 5h"));
        assert_eq!(usage.meters[0].used, 25.0);
        assert_eq!(usage.meters[0].resets_at, Some(1795219200));
        assert_eq!(usage.meters[1].used, 100.0);
        assert_eq!(usage.meters[2].used, 0.0);
    }

    #[test]
    fn accepts_tabular_quota_and_rejects_missing_readings() {
        let usage =
            parse(&json!({"response":"Gemini\tWeekly Limit Remaining\t80%\trolling"})).unwrap();
        assert!((usage.meters[0].used - 20.0).abs() < 1e-10);
        assert_eq!(usage.meters[0].resets_at, None);
        assert!(parse(&json!({"response":"Sign in"})).is_err());
    }

    #[test]
    fn uses_the_reading_once_complete_json_reports_success() {
        let full = br#"{"status":"SUCCESS","response":"Gemini\tweekly\t80%"}"#;
        assert!(successful_reading(&full[..full.len() - 1]).is_none());
        assert!(successful_reading(b"{\"status\":\"SUCCESS\",\"a\":{}").is_none());
        assert!(successful_reading(br#"{"status":"ERROR"}"#).is_none());
        let mut line = full.to_vec();
        line.push(b'\n');
        assert_eq!(successful_reading(&line).unwrap()["status"], "SUCCESS");
    }

    #[test]
    fn reports_cli_errors_before_accepting_quota() {
        let status = ExitStatus::default();
        let failed =
            br#"{"status":"ERROR","error":"Not signed in","response":"Gemini\tweekly\t100%"}"#;
        let err = parse_output(failed, b"other diagnostic", status)
            .err()
            .unwrap();
        assert_eq!(err.problem, Problem::NotAvailable);
        let err = parse_output(
            br#"{"status":"ERROR"}"#,
            b"starting\nquota unavailable\n",
            status,
        )
        .err()
        .unwrap();
        assert_eq!(err.problem, Problem::Service);
        assert_eq!(err.detail, "agy /usage: quota unavailable");
    }
}
