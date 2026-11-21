use super::{FetchError, HEADLESS_TIMEOUT, Meter, Problem, Unit, Usage, no_window, wait_for_cli};
use crate::timeutil::parse_rfc3339;
use serde_json::Value;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Instant;

fn executable() -> Option<PathBuf> {
    let name = if cfg!(windows) { "agy.exe" } else { "agy" };
    let path = std::env::var_os("PATH").unwrap_or_default();
    let on_path = std::env::split_paths(&path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name));
    // A GUI launched at login may not inherit the user's shell PATH.
    let standard = if cfg!(windows) {
        crate::paths::data_local_dir().map(|dir| dir.join("agy/bin/agy.exe"))
    } else {
        std::env::home_dir().map(|dir| dir.join(".local/bin/agy"))
    };
    // Homebrew installations can also be outside launchd's PATH.
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
    cmd.args(["-p", "/usage", "--output-format", "json"])
        .env("AGY_CLI_DISABLE_AUTO_UPDATE", "true")
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    no_window(&mut cmd);
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Could not start agy: {e}"))?;
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout.take(1024 * 1024).read_to_end(&mut bytes);
        let _ = tx.send(result.map(|_| bytes));
    });
    let deadline = Instant::now() + HEADLESS_TIMEOUT;
    let status = wait_for_cli(&mut child, "agy /usage", deadline)?;
    if !status.success() {
        return Err(FetchError::new(
            Problem::Service,
            format!("agy /usage exited with {status}. Open agy and check /usage."),
        ));
    }
    let bytes = rx
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|_| "Timed out reading agy /usage".to_string())?
        .map_err(|e| format!("Could not read agy /usage: {e}"))?;
    let value = serde_json::from_slice(&bytes)
        .map_err(|_| FetchError::new(Problem::BadResponse, "agy /usage returned invalid JSON."))?;
    parse(&value)
}

fn meter(group: &str, window: &str, remaining: f64, reset: Option<&str>) -> Option<Meter> {
    if group.is_empty() || window.is_empty() || !(0.0..=1.0).contains(&remaining) {
        return None;
    }
    let window = window.strip_suffix(" Remaining").unwrap_or(window);
    Some(Meter {
        label: Some(format!("{group} {window}")),
        used: (1.0 - remaining) * 100.0,
        total: 100.0,
        unit: Unit::Percent,
        resets_at: reset.and_then(parse_rfc3339),
    })
}

fn parse(value: &Value) -> Result<Usage, FetchError> {
    if value["status"]
        .as_str()
        .is_some_and(|status| status != "SUCCESS")
    {
        return Err(FetchError::new(
            Problem::Service,
            "Antigravity could not read quota. Open agy and check /usage.",
        ));
    }
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
        // Earlier agy versions include tab-separated quota rows in the JSON response.
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
        assert!(parse(&json!({"status":"ERROR","response":"Gemini\tweekly\t100%"})).is_err());
    }
}
