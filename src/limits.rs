//! Works out which of a personal plan's two windows, 5-hour or weekly, will run
//! out first in the current session.
//!
//! The percentages alone cannot say: they are shares of differently sized
//! allowances. What is needed is how much of the week one 5-hour point costs,
//! which neither service publishes, so it is learnt from how both windows rise
//! between polls.

use crate::providers::Meter;
use serde::{Deserialize, Serialize};

/// Reset times further apart than this between polls mean the window rolled over.
const RESET_TOLERANCE_SECS: i64 = 600;
/// 5-hour points to see before the learnt ratio is trusted over the fallback.
const MIN_FIVE_HOUR_POINTS: f64 = 20.0;
/// Points kept in the running sums, so the ratio follows a plan change rather
/// than averaging over all history.
const MAX_FIVE_HOUR_POINTS: f64 = 300.0;

/// Summed increases of both windows over polls where neither reset.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Ratio {
    five_hour: f64,
    week: f64,
    #[serde(skip)]
    last: Option<Sample>,
}

#[derive(Clone, Copy, Debug)]
struct Sample {
    five_hour: f64,
    week: f64,
    five_hour_reset: Option<i64>,
    week_reset: Option<i64>,
}

impl Ratio {
    /// Adds the change since the previous poll, unless a window reset in between.
    pub fn observe(&mut self, five_hour: &Meter, week: &Meter) {
        let now = Sample {
            five_hour: five_hour.percent(),
            week: week.percent(),
            five_hour_reset: five_hour.resets_at,
            week_reset: week.resets_at,
        };
        let Some(prev) = self.last.replace(now) else {
            return;
        };
        // A window shown as full may have risen further than it shows, so a
        // poll that ends or starts at 100% says nothing reliable about the ratio.
        if [prev.five_hour, prev.week, now.five_hour, now.week]
            .iter()
            .any(|&p| p >= 100.0)
        {
            return;
        }
        // A 5-hour window with no reset time had not started yet; it may just have.
        let same_five_hour = prev.five_hour_reset.is_none()
            || same_window(prev.five_hour_reset, now.five_hour_reset);
        let same_week = same_window(prev.week_reset, now.week_reset);
        let d5 = now.five_hour - prev.five_hour;
        let dw = now.week - prev.week;
        if !(same_five_hour && same_week && d5 >= 0.0 && dw >= 0.0) {
            return;
        }
        self.five_hour += d5;
        self.week += dw;
        if self.five_hour > MAX_FIVE_HOUR_POINTS {
            let scale = MAX_FIVE_HOUR_POINTS / self.five_hour;
            self.five_hour *= scale;
            self.week *= scale;
        }
    }

    /// Weekly points used per 5-hour point, once enough use has been seen.
    pub fn weekly_per_five_hour(&self) -> Option<f64> {
        (self.five_hour >= MIN_FIVE_HOUR_POINTS).then(|| self.week / self.five_hour)
    }
}

fn spent(m: &Meter) -> bool {
    m.percent() >= 100.0
}

fn same_window(a: Option<i64>, b: Option<i64>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => (a - b).abs() <= RESET_TOLERANCE_SECS,
        (None, None) => true,
        _ => false,
    }
}

/// The window that runs out first this session: 0 for the 5-hour window, 1 for
/// the weekly one. Until the ratio is learnt the fuller window is picked, which
/// treats a 5-hour point and a weekly point as costing the same.
pub fn binding(five_hour: &Meter, week: &Meter, weekly_per_five_hour: Option<f64>) -> usize {
    // A spent week blocks use until it resets, whatever the 5-hour window shows,
    // unless that window is spent too and is the one to reset later.
    if spent(week) {
        let five_hour_later = spent(five_hour)
            && matches!((five_hour.resets_at, week.resets_at), (Some(f), Some(w)) if f > w);
        return usize::from(!five_hour_later);
    }
    // A week that resets before the 5-hour window does cannot end this session.
    if let (Some(f), Some(w)) = (five_hour.resets_at, week.resets_at)
        && w <= f
    {
        return 0;
    }
    let k = weekly_per_five_hour.unwrap_or(1.0);
    let five_hour_left = 100.0 - five_hour.percent();
    let week_left = 100.0 - week.percent();
    // Using up the rest of the 5-hour window would cost five_hour_left * k of the week.
    usize::from(week_left < five_hour_left * k)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::Unit;

    fn window(used: f64, resets_at: Option<i64>) -> Meter {
        Meter {
            label: None,
            used,
            total: 100.0,
            unit: Unit::Percent,
            resets_at,
        }
    }

    #[test]
    fn fallback_picks_the_fuller_window() {
        assert_eq!(binding(&window(50.0, None), &window(2.0, None), None), 0);
        assert_eq!(binding(&window(2.0, None), &window(99.0, None), None), 1);
    }

    #[test]
    fn learnt_ratio_decides() {
        // A full 5-hour window costs 10% of the week: 2% of the week left runs
        // out at 20% of the 5-hour window, before the 5-hour limit.
        assert_eq!(
            binding(&window(0.0, None), &window(98.0, None), Some(0.1)),
            1
        );
        // A full 5-hour window costs 1% of the week: 2% left outlasts it.
        assert_eq!(
            binding(&window(0.0, None), &window(98.0, None), Some(0.01)),
            0
        );
    }

    #[test]
    fn week_resetting_first_cannot_bind() {
        let five = window(0.0, Some(10_000));
        let week = window(99.0, Some(5_000));
        assert_eq!(binding(&five, &week, Some(1.0)), 0);
    }

    #[test]
    fn spent_week_binds() {
        let week = window(100.0, Some(600_000));
        // No ratio learnt yet, as on first launch.
        assert_eq!(binding(&window(0.0, Some(18_000)), &week, None), 1);
        // A learnt ratio near zero cannot make the empty 5-hour window bind.
        assert_eq!(binding(&window(0.0, Some(18_000)), &week, Some(0.0)), 1);
        // The 5-hour window has not started, or has just reset.
        assert_eq!(binding(&window(0.0, None), &week, Some(0.1)), 1);
        assert_eq!(binding(&window(0.0, Some(36_000)), &week, Some(0.1)), 1);
    }

    #[test]
    fn spent_week_binds_even_when_it_resets_first() {
        // Two hours of the week left, and an unstarted 5-hour window that would
        // reset after it: nothing can be used until the week resets.
        let five = window(0.0, Some(18_000));
        let week = window(100.0, Some(7_200));
        assert_eq!(binding(&five, &week, None), 1);
    }

    #[test]
    fn both_spent_follows_the_later_reset() {
        let five = window(100.0, Some(18_000));
        assert_eq!(binding(&five, &window(100.0, Some(600_000)), None), 1);
        assert_eq!(binding(&five, &window(100.0, Some(7_200)), None), 0);
    }

    #[test]
    fn selection_resumes_after_the_weekly_reset() {
        let five = window(40.0, Some(18_000));
        assert_eq!(binding(&five, &window(100.0, Some(600_000)), None), 1);
        assert_eq!(binding(&five, &window(0.0, Some(1_200_000)), None), 0);
    }

    fn assert_ratio(r: &Ratio, k: f64) {
        let got = r.weekly_per_five_hour().expect("ratio learnt");
        assert!((got - k).abs() < 1e-9, "{got} != {k}");
    }

    #[test]
    fn learns_ratio_and_skips_resets() {
        let mut r = Ratio::default();
        let (f, w) = (Some(18_000), Some(600_000));
        r.observe(&window(0.0, f), &window(50.0, w));
        r.observe(&window(10.0, f), &window(51.0, w));
        assert_eq!(r.weekly_per_five_hour(), None);
        r.observe(&window(30.0, f), &window(53.0, w));
        assert_ratio(&r, 0.1);
        // The 5-hour window rolls over: that drop is not usage.
        r.observe(&window(5.0, Some(36_000)), &window(54.0, w));
        assert_ratio(&r, 0.1);
        r.observe(&window(15.0, Some(36_000)), &window(55.0, w));
        assert_ratio(&r, 0.1);
    }

    #[test]
    fn skips_polls_from_a_full_window() {
        let mut r = Ratio::default();
        let (f, w) = (Some(18_000), Some(600_000));
        r.observe(&window(0.0, f), &window(70.0, w));
        r.observe(&window(20.0, f), &window(72.0, w));
        assert_ratio(&r, 0.1);
        // The week fills up, then stays at 100% while extra usage keeps the
        // 5-hour window rising: neither poll shows the real weekly cost.
        r.observe(&window(40.0, f), &window(100.0, w));
        r.observe(&window(90.0, f), &window(100.0, w));
        assert_ratio(&r, 0.1);
    }
}
