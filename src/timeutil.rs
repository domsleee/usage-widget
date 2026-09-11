//! Small time helpers so we don't need chrono for a handful of conversions.

use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Days since 1970-01-01 for a proleptic Gregorian civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn parse_num(s: &str) -> Option<i64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// Parses `YYYY-MM-DD` to a unix timestamp at midnight UTC.
pub fn parse_date(s: &str) -> Option<i64> {
    let mut parts = s.trim().splitn(3, '-');
    let y = parse_num(parts.next()?)?;
    let m = parse_num(parts.next()?)? as u32;
    let d = parse_num(parts.next()?)? as u32;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some(days_from_civil(y, m, d) * 86_400)
}

/// Parses a subset of RFC 3339: `YYYY-MM-DDTHH:MM:SS[.frac][Z|+HH:MM|-HH:MM]`.
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    let s = s.trim();
    let (date, rest) = s.split_once(['T', 't', ' '])?;
    let day_secs = parse_date(date)?;

    // Split off timezone suffix.
    let (time, offset) = if let Some(t) = rest.strip_suffix(['Z', 'z']) {
        (t, 0)
    } else if let Some(idx) = rest.rfind(['+', '-']) {
        let (t, tz) = rest.split_at(idx);
        let sign = if tz.starts_with('-') { -1 } else { 1 };
        let tz = &tz[1..];
        let (th, tm) = tz.split_once(':').unwrap_or((tz, "0"));
        let off = parse_num(th)? * 3600 + parse_num(tm)? * 60;
        (t, sign * off)
    } else {
        (rest, 0)
    };

    let time = time.split_once('.').map(|(t, _)| t).unwrap_or(time);
    let mut hms = time.splitn(3, ':');
    let h = parse_num(hms.next()?)?;
    let m = parse_num(hms.next()?)?;
    let sec = hms.next().and_then(parse_num).unwrap_or(0);
    Some(day_secs + h * 3600 + m * 60 + sec - offset)
}

/// Human friendly "in 3d 4h" / "in 12m" style remaining time.
pub fn until(ts: i64) -> String {
    let diff = ts - now_unix();
    if diff <= 0 {
        return "now".into();
    }
    let d = diff / 86_400;
    let h = (diff % 86_400) / 3600;
    let m = (diff % 3600) / 60;
    if d > 0 {
        format!("in {d}d {h}h")
    } else if h > 0 {
        format!("in {h}h {m}m")
    } else {
        format!("in {m}m")
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(parse_date("1970-01-01"), Some(0));
        assert_eq!(parse_date("2026-10-01"), Some(1_790_812_800));
        assert_eq!(parse_rfc3339("2026-10-01T00:00:01Z"), Some(1_790_812_801));
        assert_eq!(
            parse_rfc3339("2026-10-01T02:00:00.123+02:00"),
            Some(1_790_812_800)
        );
        assert_eq!(
            parse_rfc3339("2026-09-30T22:00:00-02:00"),
            Some(1_790_812_800)
        );
        assert_eq!(parse_date("nope"), None);
    }
}
