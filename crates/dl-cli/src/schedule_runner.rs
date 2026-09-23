//! Applies a [`Schedule`] to a live budget.

use dl_core::budget::Budget;
use dl_core::schedule::{LocalTime, Schedule, Weekday};
use std::sync::Arc;
use std::time::Duration;

/// The current local weekday and minute, from the system clock.
///
/// `localtime_r` is used rather than a date library: the only thing needed is
/// the local wall-clock position within the week, and the platform already
/// knows the timezone and any daylight-saving offset in force right now.
#[cfg(unix)]
pub fn local_now() -> LocalTime {
    // SAFETY: `tm` is fully initialised by localtime_r before it is read, and
    // the time_t is taken from the same call chain.
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);

        // `tm_wday` is 0 for Sunday; our Weekday is 0 for Monday.
        let weekday = Weekday::from_index(((tm.tm_wday + 6) % 7) as u8).unwrap_or(Weekday::Monday);
        LocalTime { weekday, minutes: (tm.tm_hour * 60 + tm.tm_min) as u16 }
    }
}

#[cfg(not(unix))]
pub fn local_now() -> LocalTime {
    // Falls back to a fixed point, which makes every window behave as though it
    // were midnight Monday. Windows support needs a real implementation.
    LocalTime { weekday: Weekday::Monday, minutes: 0 }
}

/// Keep `budget` in step with `schedule` until the returned task is dropped.
///
/// Sleeps until the next boundary rather than polling every minute, and caps
/// the wait so a clock change or a suspend/resume cannot leave a stale limit
/// in force indefinitely.
pub fn spawn(schedule: Schedule, budget: Arc<Budget>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let now = local_now();
            let rate = schedule.rate_at(now);
            if budget.rate() != rate {
                tracing::info!(rate, "applying a scheduled bandwidth limit");
                budget.set_rate(rate);
            }

            // Never sleep past half an hour, so a clock change or a resume from
            // sleep cannot leave a stale limit in force.
            let minutes = schedule.minutes_until_change(now).clamp(1, 30);
            tokio::time::sleep(Duration::from_secs(minutes as u64 * 60)).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dl_core::schedule::TimeWindow;

    #[test]
    fn the_local_clock_produces_a_sane_position_in_the_week() {
        let now = local_now();
        assert!(now.minutes < 1440);
        assert!(Weekday::ALL.contains(&now.weekday));
    }

    #[tokio::test(start_paused = true)]
    async fn the_scheduled_rate_is_applied_immediately_on_start() {
        let schedule = Schedule::with_default(Some(4096));
        let budget = Budget::with_rate(0);
        let handle = spawn(schedule, Arc::clone(&budget));

        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(budget.rate(), 4096);
        handle.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn a_schedule_with_no_limit_leaves_the_budget_unlimited() {
        let mut schedule = Schedule::unlimited();
        schedule.push(TimeWindow::every_day(0, 1440, None));
        let budget = Budget::with_rate(1000);
        let handle = spawn(schedule, Arc::clone(&budget));

        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(budget.is_unlimited());
        handle.abort();
    }
}
