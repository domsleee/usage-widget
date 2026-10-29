//! Small time helpers.

use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, TimeZone, Timelike};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Parses an RFC 3339 timestamp to unix seconds.
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(s.trim())
        .ok()
        .map(|d| d.timestamp())
}

/// Remaining time until a timestamp: "3d 4h", "4h 11m", "12m" or "now".
pub fn until(ts: i64) -> String {
    countdown(ts - now_unix())
}

fn countdown(diff: i64) -> String {
    if diff <= 0 {
        return "now".into();
    }
    let d = diff / 86_400;
    let h = (diff % 86_400) / 3600;
    let m = (diff % 3600) / 60;
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m")
    }
}

/// Time since a past timestamp: "28m" or "2h 5m".
pub fn age(ts: i64) -> String {
    let diff = (now_unix() - ts).max(0);
    if diff < 3600 {
        format!("{}m", diff / 60)
    } else {
        format!("{}h {}m", diff / 3600, (diff % 3600) / 60)
    }
}

/// Local wall-clock time of a timestamp: "today 11:40pm", "Wednesday 6am".
pub fn local_when(ts: i64) -> String {
    // Round to the minute so a reset at 19:59:59 reads as 8pm, not 7:59pm.
    match Local
        .timestamp_opt((ts + 30).div_euclid(60) * 60, 0)
        .single()
    {
        Some(t) => when(&t, Local::now().date_naive()),
        None => String::new(),
    }
}

fn when<Tz: TimeZone>(t: &DateTime<Tz>, today: NaiveDate) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let day = match (t.date_naive() - today).num_days() {
        0 => "today".to_string(),
        1 => "tomorrow".to_string(),
        2..=6 => t.format("%A").to_string(),
        _ => t.format("%a %-d %b").to_string(),
    };
    let time = if t.minute() == 0 {
        t.format("%-I%P")
    } else {
        t.format("%-I:%M%P")
    };
    format!("{day} {time}")
}

/// Coarse remaining time for long spans like a billing cycle: "11d", "7h", "12m"
/// or "now". Rounds down, so it never promises more time than is left.
pub fn until_short(ts: i64) -> String {
    short(ts - now_unix())
}

fn short(secs: i64) -> String {
    match secs {
        ..=0 => "now".into(),
        86_400.. => format!("{}d", secs / 86_400),
        3600.. => format!("{}h", secs / 3600),
        _ => format!("{}m", (secs / 60).max(1)),
    }
}

/// "5m ago" style age for a past timestamp.
pub fn ago(ts: i64) -> String {
    if now_unix() - ts < 60 {
        "just now".into()
    } else {
        format!("{} ago", age(ts))
    }
}

/// Time left until a future timestamp as dd:hh:mm.
pub fn window_until(ts: i64) -> String {
    window_countdown((ts - now_unix()).max(0))
}

/// A reset date in the computer's time zone, as "Fri 16 Oct 2026, 2:30PM".
pub fn local_datetime(ts: i64) -> Option<String> {
    Local
        .timestamp_opt(ts, 0)
        .single()
        .map(|date| format_datetime(&date.naive_local()))
}

fn format_datetime(date: &NaiveDateTime) -> String {
    date.format("%a %-d %b %Y, %-I:%M%p").to_string()
}

/// Local reset date and time for a weekly window, for example "17 Oct 2:30pm".
pub fn reset_date_time(ts: i64) -> String {
    Local
        .timestamp_opt(ts, 0)
        .single()
        .map(|t| t.format("%-d %b %-I:%M%P").to_string())
        .unwrap_or_default()
}

/// dd:hh:mm, or hh:mm once under a day.
fn window_countdown(secs: i64) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;

    #[test]
    fn dates() {
        assert_eq!(parse_rfc3339("2026-10-01T00:00:01Z"), Some(1_790_812_801));
        assert_eq!(
            parse_rfc3339("2026-10-01T02:00:00.123+02:00"),
            Some(1_790_812_800)
        );
        assert_eq!(
            parse_rfc3339("2026-09-15T20:00:00.838238+00:00"),
            Some(1_789_502_400)
        );
        assert_eq!(parse_rfc3339("nope"), None);
    }

    #[test]
    fn window_countdown_drops_days_under_a_day() {
        assert_eq!(window_countdown(86_400 + 2 * 3600 + 3 * 60 + 4), "01:02:03");
        assert_eq!(window_countdown(86_400), "01:00:00");
        assert_eq!(window_countdown(3 * 3600 + 29 * 60 + 59), "03:29");
        assert_eq!(window_countdown(42), "00:00");
    }

    #[test]
    fn formats_reset_time_as_12_hour() {
        let at = |h, m| {
            chrono::NaiveDate::from_ymd_opt(2026, 10, 16)
                .unwrap()
                .and_hms_opt(h, m, 0)
                .unwrap()
        };
        assert_eq!(format_datetime(&at(21, 11)), "Fri 16 Oct 2026, 9:11PM");
        assert_eq!(format_datetime(&at(0, 5)), "Fri 16 Oct 2026, 12:05AM");
        assert_eq!(format_datetime(&at(12, 0)), "Fri 16 Oct 2026, 12:00PM");
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

    #[test]
    fn local_times() {
        let tz = FixedOffset::east_opt(10 * 3600).unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 9, 11).unwrap(); // a Friday
        let at = |s: &str| DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&tz);
        assert_eq!(when(&at("2026-09-11T13:40:00Z"), today), "today 11:40pm");
        assert_eq!(when(&at("2026-09-11T20:00:00Z"), today), "tomorrow 6am");
        assert_eq!(when(&at("2026-09-15T20:00:00Z"), today), "Wednesday 6am");
        assert_eq!(when(&at("2026-09-19T02:00:00Z"), today), "Sat 19 Sep 12pm");
    }

    #[test]
    fn short_countdowns() {
        let got: Vec<String> = [
            -5,
            0,
            30,
            59 * 60,
            3600,
            7 * 3600 + 59 * 60,
            86_400,
            11 * 86_400 + 22 * 3600,
        ]
        .into_iter()
        .map(short)
        .collect();
        assert_eq!(got, ["now", "now", "1m", "59m", "1h", "7h", "1d", "11d"]);
    }
}
