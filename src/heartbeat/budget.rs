//! Wake budget: an hourly cap (rolling 60 minutes) and a daily spend cap.
//! Over budget, the caller records a deferred wake and leaves the events
//! queued for the next attempt.

use chrono::{DateTime, Duration, Local, TimeZone, Utc};
use serde::{Deserialize, Serialize};

use super::state::WakeRecord;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Budget {
    pub wakes_per_hour: u32,
    pub daily_usd: f64,
}

impl Budget {
    pub fn from_settings(settings: &crate::settings::Settings) -> Self {
        Self {
            wakes_per_hour: settings.heartbeat_wakes_per_hour,
            daily_usd: settings.heartbeat_daily_usd,
        }
    }
}

/// May a wake run now? `Err` carries the defer reason. Only wakes that ran
/// (not deferred ones) count toward the hourly cap; a cap of 0 means no
/// wakes at all.
pub fn check_wake(
    trail: &[WakeRecord],
    budget: &Budget,
    spent_today_usd: f64,
    now: DateTime<Utc>,
) -> Result<(), String> {
    let window_start = now - Duration::minutes(60);
    let recent = trail
        .iter()
        .filter(|w| !w.deferred && w.at > window_start && w.at <= now)
        .count();
    if recent as u32 >= budget.wakes_per_hour {
        return Err(format!(
            "hourly cap reached ({recent} of {} wakes in the last 60 minutes)",
            budget.wakes_per_hour
        ));
    }
    if spent_today_usd >= budget.daily_usd {
        return Err(format!(
            "daily budget reached (${spent_today_usd:.2} of ${:.2})",
            budget.daily_usd
        ));
    }
    Ok(())
}

/// Wake spend recorded at or after `since`.
pub fn spent_since(trail: &[WakeRecord], since: DateTime<Utc>) -> f64 {
    trail
        .iter()
        .filter(|w| w.at >= since)
        .filter_map(|w| w.cost_usd)
        .sum()
}

/// Local midnight of `now`'s day, in UTC — the start of "today" for
/// [`spent_since`].
pub fn local_day_start(now: DateTime<Utc>) -> DateTime<Utc> {
    let midnight = now.with_timezone(&Local).date_naive().and_hms_opt(0, 0, 0).unwrap_or_default();
    Local
        .from_local_datetime(&midnight)
        .earliest()
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or(now - Duration::hours(24))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 20, 15, 0, 0).unwrap()
    }

    fn wake(mins_ago: i64, deferred: bool, cost: Option<f64>) -> WakeRecord {
        let mut w = WakeRecord::fired(now() - Duration::minutes(mins_ago), "test", vec![], "");
        w.deferred = deferred;
        w.cost_usd = cost;
        w
    }

    const B: Budget = Budget { wakes_per_hour: 2, daily_usd: 5.0 };

    #[test]
    fn hourly_cap_uses_a_rolling_window() {
        assert!(check_wake(&[], &B, 0.0, now()).is_ok());
        let one = [wake(10, false, None)];
        assert!(check_wake(&one, &B, 0.0, now()).is_ok());
        let two = [wake(10, false, None), wake(50, false, None)];
        let err = check_wake(&two, &B, 0.0, now()).unwrap_err();
        assert!(err.contains("hourly cap"));
        // 61 minutes ago has rolled out of the window.
        let rolled = [wake(10, false, None), wake(61, false, None)];
        assert!(check_wake(&rolled, &B, 0.0, now()).is_ok());
        // Deferred attempts don't count.
        let deferred = [wake(10, false, None), wake(20, true, None)];
        assert!(check_wake(&deferred, &B, 0.0, now()).is_ok());
    }

    #[test]
    fn zero_cap_never_wakes() {
        let b = Budget { wakes_per_hour: 0, daily_usd: 5.0 };
        assert!(check_wake(&[], &b, 0.0, now()).is_err());
    }

    #[test]
    fn daily_spend_cap() {
        let err = check_wake(&[], &B, 5.0, now()).unwrap_err();
        assert!(err.contains("daily budget"));
        assert!(check_wake(&[], &B, 4.99, now()).is_ok());
    }

    #[test]
    fn spend_sums_since() {
        let trail = [wake(10, false, Some(0.5)), wake(30, false, Some(0.25)), wake(600, false, Some(9.0))];
        let since = now() - Duration::hours(1);
        assert!((spent_since(&trail, since) - 0.75).abs() < 1e-9);
        assert!(local_day_start(now()) <= now());
        assert!(now() - local_day_start(now()) <= Duration::hours(25));
    }
}
