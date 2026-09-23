//! Time-of-day bandwidth rules.
//!
//! Windows are expressed in **local wall-clock time**, because that is what a
//! person means by "full speed after midnight". Working in wall-clock minutes
//! also gives the right answer across a daylight-saving change for free: on a
//! spring-forward day a 01:00-03:00 window simply contains fewer real minutes,
//! which is exactly what was asked for.
//!
//! No date library is involved. The caller supplies the current weekday and
//! minute, which keeps every rule here directly testable and leaves timezone
//! conversion at the edges where the platform already provides it.

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Weekday {
    Monday,
    Tuesday,
    Wednesday,
    Thursday,
    Friday,
    Saturday,
    Sunday,
}

impl Weekday {
    pub const ALL: [Weekday; 7] = [
        Self::Monday,
        Self::Tuesday,
        Self::Wednesday,
        Self::Thursday,
        Self::Friday,
        Self::Saturday,
        Self::Sunday,
    ];

    /// `0` is Monday, matching ISO-8601 ordering.
    pub fn from_index(index: u8) -> Option<Self> {
        Self::ALL.get(index as usize % 7).copied()
    }

    pub fn index(self) -> u8 {
        Self::ALL.iter().position(|d| *d == self).unwrap_or(0) as u8
    }

    fn next(self) -> Self {
        Self::from_index((self.index() + 1) % 7).unwrap_or(Self::Monday)
    }
}

/// A point in the local week.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalTime {
    pub weekday: Weekday,
    /// Minutes since local midnight, `0..1440`.
    pub minutes: u16,
}

impl LocalTime {
    pub fn new(weekday: Weekday, hour: u8, minute: u8) -> Self {
        Self { weekday, minutes: (hour as u16 * 60 + minute as u16).min(1439) }
    }
}

/// A bandwidth rule that applies during part of the week.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimeWindow {
    pub days: Vec<Weekday>,
    /// Minutes since midnight, inclusive.
    pub start: u16,
    /// Minutes since midnight, exclusive. May be less than `start`, meaning the
    /// window runs past midnight into the next day.
    pub end: u16,
    /// Bytes per second while this window applies; `None` means unlimited.
    pub limit: Option<u64>,
}

impl TimeWindow {
    pub fn new(days: Vec<Weekday>, start: u16, end: u16, limit: Option<u64>) -> Self {
        Self { days, start, end, limit }
    }

    pub fn every_day(start: u16, end: u16, limit: Option<u64>) -> Self {
        Self::new(Weekday::ALL.to_vec(), start, end, limit)
    }

    /// Whether the window wraps past midnight, such as 23:00-02:00.
    pub fn wraps_midnight(&self) -> bool {
        self.end <= self.start
    }

    pub fn contains(&self, at: LocalTime) -> bool {
        if self.wraps_midnight() {
            // The tail after midnight belongs to the *previous* day's window,
            // so "Friday 23:00-02:00" still applies at 01:00 on Saturday.
            if at.minutes >= self.start {
                return self.days.contains(&at.weekday);
            }
            if at.minutes < self.end {
                let started_on = previous_day(at.weekday);
                return self.days.contains(&started_on);
            }
            false
        } else {
            self.days.contains(&at.weekday) && at.minutes >= self.start && at.minutes < self.end
        }
    }
}

fn previous_day(day: Weekday) -> Weekday {
    Weekday::from_index((day.index() + 6) % 7).unwrap_or(Weekday::Monday)
}

/// The set of windows, plus what applies outside all of them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Schedule {
    pub windows: Vec<TimeWindow>,
    /// Applies when no window matches. `None` means unlimited.
    pub default_limit: Option<u64>,
}

impl Schedule {
    pub fn unlimited() -> Self {
        Self::default()
    }

    pub fn with_default(limit: Option<u64>) -> Self {
        Self { windows: Vec::new(), default_limit: limit }
    }

    pub fn push(&mut self, window: TimeWindow) -> &mut Self {
        self.windows.push(window);
        self
    }

    /// The limit in force at `at`, in bytes per second. `None` is unlimited.
    ///
    /// The last matching window wins, so a general rule can be written first
    /// and a specific exception after it.
    pub fn limit_at(&self, at: LocalTime) -> Option<u64> {
        self.windows
            .iter()
            .rev()
            .find(|w| w.contains(at))
            .map(|w| w.limit)
            .unwrap_or(self.default_limit)
    }

    /// The rate to hand a [`crate::budget::Budget`], where zero is unlimited.
    pub fn rate_at(&self, at: LocalTime) -> u64 {
        self.limit_at(at).unwrap_or(0)
    }

    /// Minutes until the limit could next change.
    ///
    /// Lets a scheduler sleep until the next boundary rather than waking every
    /// minute to discover nothing has changed.
    pub fn minutes_until_change(&self, at: LocalTime) -> u16 {
        let current = self.limit_at(at);
        for ahead in 1..=(7 * 24 * 60u32) {
            let probe = advance(at, ahead);
            if self.limit_at(probe) != current {
                return ahead.min(u16::MAX as u32) as u16;
            }
        }
        u16::MAX
    }
}

fn advance(at: LocalTime, minutes: u32) -> LocalTime {
    let total = at.minutes as u32 + minutes;
    let days = total / 1440;
    let mut weekday = at.weekday;
    for _ in 0..days {
        weekday = weekday.next();
    }
    LocalTime { weekday, minutes: (total % 1440) as u16 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Weekday::*;

    const MB: u64 = 1 << 20;

    #[test]
    fn a_plain_window_applies_only_inside_its_hours() {
        let window = TimeWindow::every_day(9 * 60, 17 * 60, Some(MB));
        assert!(!window.contains(LocalTime::new(Monday, 8, 59)));
        assert!(window.contains(LocalTime::new(Monday, 9, 0)));
        assert!(window.contains(LocalTime::new(Monday, 16, 59)));
        // The end is exclusive, so two adjacent windows cannot both match.
        assert!(!window.contains(LocalTime::new(Monday, 17, 0)));
    }

    #[test]
    fn a_window_past_midnight_belongs_to_the_day_it_started() {
        // "Saturday 23:00-02:00" must still apply at 01:00 on Sunday, not
        // require the user to also list Sunday.
        let window = TimeWindow::new(vec![Saturday], 23 * 60, 2 * 60, None);
        assert!(window.contains(LocalTime::new(Saturday, 23, 30)));
        assert!(window.contains(LocalTime::new(Sunday, 1, 0)));
        assert!(!window.contains(LocalTime::new(Sunday, 2, 0)));
        assert!(!window.contains(LocalTime::new(Sunday, 23, 30)));
        assert!(!window.contains(LocalTime::new(Friday, 23, 30)));
    }

    #[test]
    fn days_are_respected() {
        let weekdays =
            TimeWindow::new(vec![Monday, Tuesday, Wednesday, Thursday, Friday], 0, 1440, Some(MB));
        assert!(weekdays.contains(LocalTime::new(Wednesday, 12, 0)));
        assert!(!weekdays.contains(LocalTime::new(Saturday, 12, 0)));
    }

    #[test]
    fn the_default_applies_outside_every_window() {
        let mut schedule = Schedule::with_default(Some(2 * MB));
        schedule.push(TimeWindow::every_day(0, 6 * 60, None));

        assert_eq!(schedule.limit_at(LocalTime::new(Monday, 3, 0)), None, "night is unlimited");
        assert_eq!(schedule.limit_at(LocalTime::new(Monday, 12, 0)), Some(2 * MB));
    }

    #[test]
    fn a_later_window_overrides_an_earlier_one() {
        // So a broad rule can be written first and an exception after it.
        let mut schedule = Schedule::unlimited();
        schedule.push(TimeWindow::every_day(9 * 60, 17 * 60, Some(MB)));
        schedule.push(TimeWindow::new(vec![Friday], 9 * 60, 17 * 60, Some(4 * MB)));

        assert_eq!(schedule.limit_at(LocalTime::new(Monday, 10, 0)), Some(MB));
        assert_eq!(schedule.limit_at(LocalTime::new(Friday, 10, 0)), Some(4 * MB));
    }

    #[test]
    fn unlimited_is_expressed_as_a_zero_rate() {
        let schedule = Schedule::unlimited();
        assert_eq!(schedule.rate_at(LocalTime::new(Monday, 0, 0)), 0);

        let capped = Schedule::with_default(Some(500));
        assert_eq!(capped.rate_at(LocalTime::new(Monday, 0, 0)), 500);
    }

    #[test]
    fn the_next_change_is_the_window_boundary() {
        let mut schedule = Schedule::with_default(Some(2 * MB));
        schedule.push(TimeWindow::every_day(0, 6 * 60, None));

        // At 05:30 the limit changes at 06:00, thirty minutes later.
        assert_eq!(schedule.minutes_until_change(LocalTime::new(Monday, 5, 30)), 30);
        // At 07:00 the next change is midnight, seventeen hours away.
        assert_eq!(schedule.minutes_until_change(LocalTime::new(Monday, 7, 0)), 17 * 60);
    }

    #[test]
    fn a_schedule_that_never_changes_reports_no_boundary() {
        let schedule = Schedule::with_default(Some(MB));
        assert_eq!(schedule.minutes_until_change(LocalTime::new(Monday, 12, 0)), u16::MAX);
    }

    /// A spring-forward day simply has fewer wall-clock minutes inside the
    /// window, which is the behaviour someone scheduling in local time wants.
    /// Nothing here needs to know a transition happened.
    #[test]
    fn wall_clock_windows_need_no_daylight_saving_handling() {
        let window = TimeWindow::every_day(60, 180, None);
        // 02:00 does not exist on a spring-forward morning; the clock jumps
        // 01:59 -> 03:00 and the window ends early, as intended.
        assert!(window.contains(LocalTime::new(Sunday, 1, 59)));
        assert!(!window.contains(LocalTime::new(Sunday, 3, 0)));
    }

    #[test]
    fn advancing_rolls_over_days_and_weeks() {
        assert_eq!(
            advance(LocalTime::new(Sunday, 23, 30), 60),
            LocalTime::new(Monday, 0, 30),
            "the week should wrap from Sunday to Monday"
        );
        assert_eq!(advance(LocalTime::new(Monday, 0, 0), 1440 * 7), LocalTime::new(Monday, 0, 0));
    }
}
