//! Learns what one whole percent of a usage window costs in locally priced token
//! use, and estimates how far into the next percent the window is. The Codex and
//! Claude estimators feed it their priced token use and the services'
//! whole-percent readings.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Points a window must have gained before its own cost per point is trusted.
const MIN_POINTS: f64 = 3.0;
/// Points a finished window must have gained to be remembered for the next one.
const MIN_POINTS_FINISHED: f64 = 5.0;

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Tracker {
    /// The window being tracked, by its reset time.
    window: i64,
    plan: String,
    start_pct: f64,
    last_pct: f64,
    /// Credits of local use since calibration started...
    credits: f64,
    /// ...and at the moment the percentage last went up.
    tick_credits: f64,
    /// Cost of a point in finished windows, by plan.
    rates: HashMap<String, f64>,
}

/// Whether two reset times name the same window. Reset times wobble by a second
/// between readings; windows are at least five hours apart.
fn same_window(a: i64, b: i64) -> bool {
    (a - b).abs() < 3600
}

impl Tracker {
    /// A reading: `credits` of local use, after which the service said `pct`.
    pub fn apply(&mut self, pct: f64, window: i64, plan: &str, credits: f64) {
        if !same_window(window, self.window) {
            if window < self.window {
                return; // a late reading from an earlier window
            }
            let gained = self.last_pct - self.start_pct;
            if self.window != 0 && gained >= MIN_POINTS_FINISHED && self.tick_credits > 0.0 {
                self.rates
                    .insert(self.plan.clone(), self.tick_credits / gained);
            }
            self.window = window;
            self.recalibrate(pct, plan);
            return;
        }
        if !plan.is_empty() && plan != self.plan {
            self.change_plan(pct, plan);
            return;
        }
        self.credits += credits;
        if pct < self.last_pct {
            // Logged late, from before the latest tick: it paid for points already reached.
            self.tick_credits += credits;
        } else if pct > self.last_pct {
            self.last_pct = pct;
            self.tick_credits = self.credits;
        }
    }

    /// Local use that arrives without a reading of its own.
    pub fn spend(&mut self, credits: f64) {
        if self.window != 0 {
            self.credits += credits;
        }
    }

    /// The service's own reading, which also moves with use the logs never see. A
    /// drop of two or more points means the allowance grew (a plan change); a
    /// one-point lag behind the logs is just the service catching up.
    pub fn observe(&mut self, pct: f64, window: i64, plan: &str) {
        if same_window(window, self.window) && pct + 1.0 < self.last_pct {
            if plan.is_empty() || plan == self.plan {
                self.recalibrate(pct, plan);
            } else {
                self.change_plan(pct, plan);
            }
        } else {
            self.apply(pct, window, plan, 0.0);
        }
    }

    /// A different allowance mid-window. The window's use so far now reads `pct` of
    /// the new plan, which prices the new plan's point; the window's own count
    /// then starts afresh.
    fn change_plan(&mut self, pct: f64, plan: &str) {
        let gained = self.last_pct - self.start_pct;
        if gained >= MIN_POINTS && self.tick_credits > 0.0 && pct >= MIN_POINTS {
            let per_point = self.tick_credits / gained;
            let used = per_point * self.last_pct + self.credits - self.tick_credits;
            self.rates.insert(plan.to_string(), used / pct);
        }
        self.recalibrate(pct, plan);
    }

    /// Starts learning what a point costs afresh from `pct`.
    fn recalibrate(&mut self, pct: f64, plan: &str) {
        self.plan = plan.to_string();
        self.start_pct = pct;
        self.last_pct = pct;
        self.credits = 0.0;
        self.tick_credits = 0.0;
    }

    /// The estimated share of the next point already used, 0.0 to 0.99. None until
    /// there is a cost per point to go on, and at the limit.
    pub fn fraction(&self, pct: f64, window: i64) -> Option<f64> {
        if pct >= 100.0 || !same_window(window, self.window) || pct != self.last_pct {
            return None;
        }
        let gained = self.last_pct - self.start_pct;
        let per_point = if gained >= MIN_POINTS && self.tick_credits > 0.0 {
            self.tick_credits / gained
        } else {
            *self.rates.get(&self.plan)?
        };
        Some(((self.credits - self.tick_credits) / per_point).clamp(0.0, 0.99))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WEEK: i64 = 1_789_462_965;

    #[test]
    fn estimates_between_ticks() {
        const NEXT_WEEK: i64 = WEEK + 604_800;
        let mut t = Tracker::default();
        t.apply(10.0, WEEK, "plus", 0.0);
        assert_eq!(t.fraction(10.0, WEEK), None); // no cost per point yet
        for pct in [11.0, 12.0, 13.0] {
            t.apply(pct, WEEK, "plus", 20.0); // each point costs 20 credits
        }
        // The reset time wobbles by a second; that's still this window.
        t.apply(13.0, WEEK + 1, "plus", 5.0);
        assert_eq!(t.fraction(13.0, WEEK - 1), Some(0.25));
        t.apply(13.0, WEEK, "plus", 100.0);
        assert_eq!(t.fraction(13.0, WEEK), Some(0.99)); // capped below the next point
        t.observe(14.0, WEEK, "plus"); // tick from use elsewhere restarts the count
        assert_eq!(t.fraction(14.0, WEEK), Some(0.0));

        // A new window falls back to the finished window's cost per point.
        for pct in [15.0, 16.0] {
            t.apply(pct, WEEK, "plus", 20.0);
        }
        t.apply(0.0, NEXT_WEEK, "plus", 0.0);
        t.apply(0.0, NEXT_WEEK, "plus", 5.0);
        let per_point = t.rates["plus"];
        assert!((t.fraction(0.0, NEXT_WEEK).unwrap() - 5.0 / per_point).abs() < 1e-9);
    }

    #[test]
    fn late_readings_plan_changes_and_the_limit() {
        let mut t = Tracker::default();
        for pct in [10.0, 11.0, 12.0, 13.0] {
            t.apply(pct, WEEK, "prolite", 20.0);
        }
        // Logged late from before the 13% tick: it doesn't count toward the next point.
        t.apply(12.0, WEEK, "prolite", 10.0);
        assert_eq!(t.fraction(13.0, WEEK), Some(0.0));
        // A straggler from last week changes nothing.
        t.apply(90.0, WEEK - 604_800, "prolite", 50.0);
        assert_eq!(t.fraction(13.0, WEEK), Some(0.0));

        // A plan change starts learning afresh.
        t.apply(13.0, WEEK, "pro", 5.0);
        assert_eq!(
            (t.plan.as_str(), t.start_pct, t.credits),
            ("pro", 13.0, 0.0)
        );
        // So does the service's figure dropping (a bigger allowance)...
        for pct in [14.0, 15.0] {
            t.apply(pct, WEEK, "pro", 10.0);
        }
        t.observe(6.0, WEEK, "pro");
        assert_eq!((t.start_pct, t.last_pct), (6.0, 6.0));
        // ...but not the service lagging a point behind the logs.
        t.apply(7.0, WEEK, "pro", 10.0);
        t.observe(6.0, WEEK, "pro");
        assert_eq!(t.last_pct, 7.0);

        t.observe(100.0, WEEK, "pro");
        assert_eq!(t.fraction(100.0, WEEK), None); // nothing past the limit
    }

    #[test]
    fn plan_change_carries_the_cost_of_a_point_over() {
        let mut t = Tracker::default();
        t.observe(10.0, WEEK, "Pro");
        for pct in [11.0, 12.0, 13.0, 14.0] {
            t.spend(10.0);
            t.observe(pct, WEEK, "Pro");
        }
        t.spend(5.0);
        // 145 credits' worth of use reads 4% of the bigger plan: 36.25 a point,
        // replacing whatever a past week left.
        t.rates.insert("Max 5x".into(), 1.0);
        t.observe(4.0, WEEK, "Max 5x");
        assert_eq!(t.rates["Max 5x"], 36.25);
        t.spend(9.0625);
        assert_eq!(t.fraction(4.0, WEEK), Some(0.25));

        // Too few points on the new plan to say.
        let mut t = Tracker::default();
        t.observe(10.0, WEEK, "Pro");
        t.spend(30.0);
        t.observe(40.0, WEEK, "Pro");
        t.observe(2.0, WEEK, "Max 5x");
        assert_eq!(t.rates.get("Max 5x"), None);
    }

    #[test]
    fn spend_between_readings() {
        let mut t = Tracker::default();
        t.spend(50.0); // before any reading: nothing to attach it to
        t.observe(40.0, WEEK, "Max 5x");
        for pct in [41.0, 42.0, 43.0] {
            t.spend(8.0);
            t.observe(pct, WEEK, "Max 5x");
        }
        t.spend(2.0);
        assert_eq!(t.fraction(43.0, WEEK), Some(0.25));
    }
}
