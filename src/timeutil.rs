//! Time parsing, countdowns and local reset dates.

use chrono::{Local, TimeZone};
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

/// Time left until a future timestamp as dd:hh:mm.
pub fn until(ts: i64) -> String {
    countdown((ts - now_unix()).max(0))
}

/// A reset date in the computer's time zone, including its offset at that date.
pub fn local_datetime(ts: i64) -> Option<String> {
    Local.timestamp_opt(ts, 0).single().map(|date| {
        date.format("%a %-d %b %Y, %H:%M\nLocal time (UTC%:z)")
            .to_string()
    })
}

/// dd:hh:mm, or hh:mm once under a day.
fn countdown(secs: i64) -> String {
    let (days, hours, mins) = (secs / 86_400, secs % 86_400 / 3600, secs % 3600 / 60);
    if days > 0 {
        format!("{days:02}:{hours:02}:{mins:02}")
    } else {
        format!("{hours:02}:{mins:02}")
    }
}

/// Time to the next whole minute, so a countdown ticks over on time.
pub fn until_next_minute() -> std::time::Duration {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() % 60_000)
        .unwrap_or(0);
    std::time::Duration::from_millis(60_000 - millis as u64)
}

/// Parses an RFC 3339 timestamp ("2026-10-13T17:59:59.94+00:00" or "...Z") to
/// unix seconds. Fractional seconds are dropped.
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let rest = s.get(19..)?;
    let zone = rest.trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
    let offset = match zone {
        "Z" | "z" | "" => 0,
        z => {
            let sign = if z.starts_with('-') { -1 } else { 1 };
            let oh = z.get(1..3)?.parse::<i64>().ok()?;
            let om = z.get(4..6)?.parse::<i64>().ok()?;
            sign * (oh * 3600 + om * 60)
        }
    };
    Some(days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + sec - offset)
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn countdown_drops_days_under_a_day() {
        assert_eq!(countdown(86_400 + 2 * 3600 + 3 * 60 + 4), "01:02:03");
        assert_eq!(countdown(86_400), "01:00:00");
        assert_eq!(countdown(3 * 3600 + 29 * 60 + 59), "03:29");
        assert_eq!(countdown(42), "00:00");
    }

    #[test]
    fn parses_rfc3339() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339("2026-10-13T02:35:28.308Z"),
            Some(1_791_858_928)
        );
        assert_eq!(
            parse_rfc3339("2026-10-13T12:35:28.943648+10:00"),
            Some(1_791_858_928)
        );
        assert_eq!(parse_rfc3339("not a time"), None);
    }
}
