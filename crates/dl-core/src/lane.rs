//! Independent paths to the same bytes, and what each of them is doing.
//!
//! A lane is usually one network interface, but the engine never learns that.
//! It sees a set of sources that fetch the same resource at different speeds
//! and fail independently, which is also the shape of multiple mirrors: so
//! phase 7 reuses this rather than adding a parallel mechanism.
//!
//! Which lane carries which chunk is decided in [`crate::regions`], by giving
//! each lane its own run of the file. This module keeps the account: how fast
//! each lane is going, how much it has carried, and whether it is still fit to
//! be handed work.

use crate::source::ByteSource;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Weight given to each new measurement. Low enough to ride out one slow
/// chunk, high enough to notice an interface that has actually degraded.
const EWMA_ALPHA: f64 = 0.3;

/// Consecutive failures before a lane is taken out of rotation.
const FAILURES_BEFORE_PARKED: u32 = 3;

/// How often a lane's rate is recalculated.
///
/// Short enough that the sidebar reads as live and a path that just degraded
/// is noticed; long enough that a fast lane is not doing arithmetic on every
/// socket read.
pub const SAMPLE_WINDOW: Duration = Duration::from_millis(250);

/// A lane that appeared after the transfer began.
pub struct Joined {
    pub label: String,
    pub source: Arc<dyn ByteSource>,
}

pub trait LaneSet: Send + Sync {
    fn len(&self) -> usize;
    fn source(&self, lane: usize) -> &dyn ByteSource;
    fn label(&self, lane: usize) -> &str;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Lanes that should be carrying bytes and are not.
    ///
    /// `live` says, for each lane this set has handed over so far, whether it
    /// is still in rotation. That is what makes a path that went away and came
    /// back different from one that is simply working: a phone whose owner
    /// switched sharing off has a parked lane, and turning sharing back on has
    /// to produce a new one rather than being ignored because the path is
    /// familiar.
    ///
    /// This is also how a phone paired mid-download starts carrying bytes
    /// without the transfer being restarted. The default is none, so a set
    /// whose membership is fixed needs no code.
    ///
    /// Whatever is returned must serve the same resource as the lanes already
    /// in use: these are not probed against the reference the way the original
    /// lanes were, because by this point the file is half written and a
    /// disagreement has nothing useful to say.
    fn joined(&self, live: &[bool]) -> Vec<Joined> {
        let _ = live;
        Vec::new()
    }
}

/// A single source, so callers that do not care about lanes stay simple.
pub struct SingleLane<'a> {
    source: &'a dyn ByteSource,
    label: String,
}

impl<'a> SingleLane<'a> {
    pub fn new(source: &'a dyn ByteSource) -> Self {
        Self { source, label: "default".to_string() }
    }
}

impl LaneSet for SingleLane<'_> {
    fn len(&self) -> usize {
        1
    }
    fn source(&self, _lane: usize) -> &dyn ByteSource {
        self.source
    }
    fn label(&self, _lane: usize) -> &str {
        &self.label
    }
}

/// Fold a rate observation into a lane's smoothed estimate.
fn fold(entry: &mut LaneState, sample: f64) {
    entry.throughput = Some(match entry.throughput {
        Some(previous) => previous * (1.0 - EWMA_ALPHA) + sample * EWMA_ALPHA,
        None => sample,
    });
}

/// Close the current measurement window if it has run its course.
///
/// Every byte a lane carries is counted here and nowhere else, so a lane's rate
/// is bytes over wall-clock however many connections it has open. Folding a
/// single chunk's rate separately, as this used to on completion, described one
/// connection rather than the lane: with eight connections open the sidebar
/// read a fraction of what the interface was really doing, and the per-lane
/// figures no longer added up to the transfer's.
///
/// Also called when a lane is merely read, so a window that closes with no
/// bytes in it folds a zero. Without that a lane that stops carrying anything
/// keeps reporting whatever it last managed, which is how a paused transfer
/// left speeds sitting on the screen.
fn sample(entry: &mut LaneState, now: Instant) {
    let Some(opened) = entry.live_at else { return };
    let elapsed = now.duration_since(opened);
    if elapsed < SAMPLE_WINDOW {
        return;
    }
    fold(entry, entry.live_bytes as f64 / elapsed.as_secs_f64());
    entry.live_bytes = 0;
    entry.live_at = Some(now);
}

#[derive(Clone, Debug, Default)]
struct LaneState {
    label: String,
    /// Smoothed throughput in bytes per second. `None` until first measured.
    throughput: Option<f64>,
    bytes: u64,
    /// Bytes seen since the open window began, and when it began.
    live_bytes: u64,
    live_at: Option<Instant>,
    chunks: u64,
    consecutive_failures: u32,
    parked: bool,
    /// Set when the lane is parked for a stated period rather than for good.
    ///
    /// A rate limit is temporary by definition, so a lane that hit one must
    /// come back: parking it permanently means one 429 costs an interface for
    /// the rest of the transfer.
    parked_until: Option<Instant>,
}

/// What a lane has done so far, for the UI and for `dl interfaces`.
#[derive(Clone, Debug, PartialEq)]
pub struct LaneReport {
    pub lane: usize,
    pub label: String,
    pub bytes: u64,
    pub chunks: u64,
    pub throughput: Option<f64>,
    pub parked: bool,
}

/// The running account of every lane in a transfer.
pub struct LaneSelector {
    state: Mutex<Vec<LaneState>>,
}

impl LaneSelector {
    pub fn new(labels: Vec<String>) -> Self {
        let state =
            labels.into_iter().map(|label| LaneState { label, ..Default::default() }).collect();
        Self { state: Mutex::new(state) }
    }

    pub fn from_lanes(lanes: &dyn LaneSet) -> Self {
        Self::new((0..lanes.len()).map(|i| lanes.label(i).to_string()).collect())
    }

    pub fn len(&self) -> usize {
        self.state.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Take on a lane that appeared after the transfer started.
    ///
    /// Returns its index, which callers rely on matching the index the same
    /// lane has in the lane set: both grow only here and only by appending.
    pub fn join(&self, label: String) -> usize {
        let mut state = self.state.lock().unwrap();
        state.push(LaneState { label, ..Default::default() });
        state.len() - 1
    }

    /// Return any timed parks whose period has elapsed to service.
    fn expire_parks(state: &mut [LaneState], now: Instant) {
        for entry in state.iter_mut() {
            if entry.parked_until.is_some_and(|until| now >= until) {
                entry.parked_until = None;
                entry.parked = false;
                // The wait was the punishment; starting the lane back at full
                // strikes would park it again on the next single failure.
                entry.consecutive_failures = 0;
            }
        }
    }

    /// Whether this lane may be handed a chunk right now.
    ///
    /// The only reason to say no is that the lane is out of rotation: a
    /// failure streak, a rate limit still running, or a probe it could not
    /// answer.
    pub fn claim(&self, lane: usize) -> bool {
        let mut state = self.state.lock().unwrap();
        // Checked here rather than on a timer: this is where the answer
        // matters, so a lane cannot be left parked past its period by nobody
        // having asked.
        Self::expire_parks(&mut state, Instant::now());
        state.get(lane).is_some_and(|entry| !entry.parked)
    }

    /// Take a lane out of rotation for a stated period.
    ///
    /// Used for a rate limit, where the origin has told us how long to wait.
    /// The lane is not failing: it is being asked to be patient: so this does
    /// not count a strike against it.
    pub fn park_for(&self, lane: usize, period: Duration) {
        let mut state = self.state.lock().unwrap();
        if let Some(entry) = state.get_mut(lane) {
            entry.parked = true;
            let until = Instant::now() + period;
            // Never shorten an existing wait: two limits arriving together
            // should leave the longer one standing.
            entry.parked_until = Some(entry.parked_until.map_or(until, |prev| prev.max(until)));
        }
    }

    /// How long until the soonest timed park expires, or `None` if no lane is
    /// waiting on one.
    ///
    /// Lets a caller sleep exactly as long as it must rather than polling, and
    /// distinguishes "everything is rate limited, wait" from "everything has
    /// failed, give up".
    pub fn time_until_unpark(&self) -> Option<Duration> {
        let state = self.state.lock().unwrap();
        let now = Instant::now();
        state
            .iter()
            .filter(|s| s.parked)
            .filter_map(|s| s.parked_until)
            .map(|until| until.saturating_duration_since(now))
            .min()
    }

    /// How much longer a parked lane has to wait.
    ///
    /// `None` means it is not coming back, which is a caller's cue to stop
    /// offering it work. A lane that is not parked at all answers zero, so a
    /// worker that lost a race here simply goes round again.
    pub fn park_remaining(&self, lane: usize) -> Option<Duration> {
        let mut state = self.state.lock().unwrap();
        let now = Instant::now();
        Self::expire_parks(&mut state, now);
        let entry = state.get(lane)?;
        if !entry.parked {
            return Some(Duration::ZERO);
        }
        entry.parked_until.map(|until| until.saturating_duration_since(now))
    }

    /// Report a completed chunk.
    ///
    /// The rate is not touched here: those bytes were counted by
    /// [`Self::progressed`] as they arrived, and the window they belong to may
    /// still be open for the lane's other connections.
    pub fn completed(&self, lane: usize, bytes: u64) {
        let mut state = self.state.lock().unwrap();
        let Some(entry) = state.get_mut(lane) else { return };
        entry.bytes += bytes;
        entry.chunks += 1;
        entry.consecutive_failures = 0;
    }

    /// Bytes have arrived on a lane, mid-chunk.
    ///
    /// Called as the body streams rather than when it ends, which is what makes
    /// a rate current: otherwise a slow path reports a figure minutes old.
    pub fn progressed(&self, lane: usize, bytes: u64) {
        self.progressed_at(lane, bytes, Instant::now());
    }

    /// The same, with the clock supplied, so a test can measure a window
    /// without sleeping through one.
    pub fn progressed_at(&self, lane: usize, bytes: u64, now: Instant) {
        let mut state = self.state.lock().unwrap();
        let Some(entry) = state.get_mut(lane) else { return };
        entry.live_bytes += bytes;
        entry.live_at.get_or_insert(now);
        sample(entry, now);
    }

    /// Close any window that has run its course, whether or not bytes arrived.
    fn settle(state: &mut [LaneState], now: Instant) {
        for entry in state.iter_mut() {
            sample(entry, now);
        }
    }

    /// Release a lane after a failure that was not the lane's fault.
    ///
    /// An origin that ignores `Range`, or a resource that changed, fails
    /// identically on every path. Counting those against the lane would park
    /// each interface in turn and then report "all paths failed" instead of
    /// the actual cause.
    pub fn released(&self, _lane: usize) {}

    /// Report a failed chunk. Repeated failures park the lane so work migrates
    /// to the others instead of retrying a NIC that has gone away.
    pub fn failed(&self, lane: usize) {
        let mut state = self.state.lock().unwrap();
        let Some(entry) = state.get_mut(lane) else { return };

        entry.consecutive_failures += 1;
        if entry.consecutive_failures >= FAILURES_BEFORE_PARKED {
            entry.parked = true;
        }
    }

    /// Take a lane out of rotation outright.
    ///
    /// Used when a lane has already proven unusable: failing a probe, for
    /// instance: so there is no reason to spend chunks discovering it again.
    pub fn park(&self, lane: usize) {
        let mut state = self.state.lock().unwrap();
        if let Some(entry) = state.get_mut(lane) {
            entry.parked = true;
        }
    }

    /// Whether every lane has been taken out of rotation.
    pub fn all_parked(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        Self::expire_parks(&mut state, Instant::now());
        !state.is_empty() && state.iter().all(|s| s.parked)
    }

    /// Whether every lane is out of rotation and none of them is coming back.
    ///
    /// The distinction that matters to a caller: all-parked-with-a-deadline is
    /// something to wait out, all-parked-for-good is something to fail on.
    pub fn all_parked_permanently(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        Self::expire_parks(&mut state, Instant::now());
        !state.is_empty() && state.iter().all(|s| s.parked && s.parked_until.is_none())
    }

    /// One lane's current rate, in bytes per second.
    ///
    /// `None` until a window has closed on it, which is what tells a caller
    /// the difference between a lane doing nothing and a lane not yet measured.
    pub fn rate_of(&self, lane: usize) -> Option<f64> {
        let now = Instant::now();
        let mut state = self.state.lock().unwrap();
        Self::settle(&mut state, now);
        state.get(lane).and_then(|entry| entry.throughput)
    }

    /// Close every open window, however short it was.
    ///
    /// Called when a transfer ends. A download that finishes inside a single
    /// sample window would otherwise report no rate at all, because no window
    /// ever ran its course and nothing was ever folded: which is exactly the
    /// case for a small file on a fast link.
    pub fn close(&self) {
        let now = Instant::now();
        let mut state = self.state.lock().unwrap();
        for entry in state.iter_mut() {
            let Some(opened) = entry.live_at.take() else { continue };
            let elapsed = now.duration_since(opened);
            if entry.live_bytes > 0 && elapsed > Duration::ZERO {
                fold(entry, entry.live_bytes as f64 / elapsed.as_secs_f64());
            }
            entry.live_bytes = 0;
        }
    }

    pub fn reports(&self) -> Vec<LaneReport> {
        self.reports_at(Instant::now())
    }

    pub fn reports_at(&self, now: Instant) -> Vec<LaneReport> {
        let mut state = self.state.lock().unwrap();
        Self::settle(&mut state, now);
        state
            .iter()
            .enumerate()
            .map(|(lane, s)| LaneReport {
                lane,
                label: s.label.clone(),
                bytes: s.bytes,
                chunks: s.chunks,
                throughput: s.throughput,
                parked: s.parked,
            })
            .collect()
    }

    /// Which lanes are still in rotation, by index.
    ///
    /// A lane waiting out a rate limit counts as live: it is coming back, and
    /// replacing it would open a second connection to an origin that has just
    /// asked for fewer.
    pub fn live(&self) -> Vec<bool> {
        let mut state = self.state.lock().unwrap();
        Self::expire_parks(&mut state, Instant::now());
        state.iter().map(|s| !s.parked || s.parked_until.is_some()).collect()
    }

    /// Combined throughput across every lane, which is the number that should
    /// exceed any single interface for aggregation to be worth anything.
    ///
    /// This is also the transfer's rate. Reporting it from here rather than
    /// measuring the download separately is what makes the sidebar's figures
    /// add up to the one in the header: there is one measurement, not two.
    pub fn aggregate_throughput(&self) -> f64 {
        self.aggregate_throughput_at(Instant::now())
    }

    pub fn aggregate_throughput_at(&self, now: Instant) -> f64 {
        let mut state = self.state.lock().unwrap();
        Self::settle(&mut state, now);
        state.iter().filter(|s| !s.parked).filter_map(|s| s.throughput).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selector(n: usize) -> LaneSelector {
        LaneSelector::new((0..n).map(|i| format!("lane{i}")).collect())
    }

    /// Drive a lane as though `bytes` arrived over `span`, ending at `at`.
    fn carried(s: &LaneSelector, lane: usize, bytes: u64, span: Duration, at: Instant) {
        s.progressed_at(lane, bytes, at - span);
        s.progressed_at(lane, 0, at);
    }

    fn rate_of(s: &LaneSelector, lane: usize, at: Instant) -> f64 {
        s.reports_at(at)[lane].throughput.unwrap_or(0.0)
    }

    /// The bug the sidebar showed: four lanes reading well under what the
    /// transfer as a whole was doing, because each lane's rate described one
    /// connection rather than the interface.
    #[test]
    fn a_lanes_rate_is_what_the_interface_carried_not_what_one_chunk_did() {
        let s = selector(1);
        let now = Instant::now();
        // Four connections, each carrying a megabyte over the same second.
        for _ in 0..4 {
            s.progressed_at(0, 1_000_000, now - Duration::from_secs(1));
        }
        s.progressed_at(0, 0, now);
        s.completed(0, 1_000_000);

        let measured = rate_of(&s, 0, now);
        assert!(
            (measured - 4_000_000.0).abs() < 1.0,
            "the lane carried 4 MB in a second and reported {measured}"
        );
    }

    #[test]
    fn the_lanes_add_up_to_the_transfer() {
        // The header and the sidebar are the same measurement read twice, so
        // no arithmetic can put them out of step.
        let s = selector(3);
        let now = Instant::now();
        for (lane, bytes) in [(0, 12_000_000u64), (1, 500_000), (2, 400_000)] {
            carried(&s, lane, bytes, Duration::from_secs(1), now);
        }
        let reported: f64 = s.reports_at(now).iter().filter_map(|r| r.throughput).sum();
        assert!((reported - s.aggregate_throughput_at(now)).abs() < 1.0);
        assert!((reported - 12_900_000.0).abs() < 1.0, "got {reported}");
    }

    #[test]
    fn a_lane_that_stops_carrying_anything_falls_to_nothing() {
        // A paused transfer used to leave its last speeds on the screen,
        // because nothing folded a zero when the bytes stopped.
        let s = selector(1);
        let start = Instant::now();
        carried(&s, 0, 10_000_000, Duration::from_secs(1), start);
        assert!(rate_of(&s, 0, start) > 9_000_000.0);

        let mut at = start;
        for _ in 0..16 {
            at += SAMPLE_WINDOW;
            s.reports_at(at);
        }
        assert!(rate_of(&s, 0, at) < 100_000.0, "still reporting {}", rate_of(&s, 0, at));
    }

    #[test]
    fn a_transfer_shorter_than_one_window_still_reports_what_it_did() {
        // A small file on a fast link is over before a window closes. Without
        // this the lane reports nothing, which reads as an interface that did
        // no work when it did all of it.
        let s = selector(1);
        s.progressed_at(0, 4_000_000, Instant::now());
        assert_eq!(s.reports()[0].throughput, None, "the window is still open");
        s.close();
        assert!(s.reports()[0].throughput.unwrap() > 0.0);
    }

    #[test]
    fn closing_twice_does_not_count_the_same_bytes_again() {
        let s = selector(1);
        s.progressed_at(0, 4_000_000, Instant::now());
        s.close();
        let once = s.reports()[0].throughput.unwrap();
        s.close();
        assert_eq!(s.reports()[0].throughput.unwrap(), once);
    }

    #[test]
    fn a_window_that_has_not_run_its_course_is_left_open() {
        // Sampling on every read would divide a few bytes by a few
        // microseconds and report gigabytes a second.
        let s = selector(1);
        let now = Instant::now();
        s.progressed_at(0, 1_000, now);
        assert_eq!(s.reports_at(now + Duration::from_millis(10))[0].throughput, None);
    }

    #[test]
    fn a_lane_that_joins_mid_transfer_is_accounted_for_separately() {
        let s = selector(1);
        assert_eq!(s.join("Pixel 7 Pro (cell)".to_string()), 1);
        assert_eq!(s.len(), 2);

        let now = Instant::now();
        carried(&s, 1, 400_000, Duration::from_secs(1), now);
        let reports = s.reports_at(now);
        assert_eq!(reports[1].label, "Pixel 7 Pro (cell)");
        assert_eq!(reports[0].throughput, None, "the existing lane was disturbed");
        assert!(reports[1].throughput.unwrap() > 0.0);
    }

    #[test]
    fn a_repeatedly_failing_lane_is_parked_and_work_migrates() {
        let s = selector(2);
        for _ in 0..FAILURES_BEFORE_PARKED {
            s.failed(0);
        }
        assert!(!s.claim(0), "work was offered to a dead lane");
        assert!(s.claim(1));
        assert!(s.reports()[0].parked);
        assert!(!s.all_parked());
    }

    /// A fault in the origin fails on every lane at once; parking them for it
    /// would destroy the download and hide the real error.
    #[test]
    fn a_failure_that_is_not_the_lanes_fault_does_not_park_it() {
        let s = selector(1);
        for _ in 0..FAILURES_BEFORE_PARKED * 3 {
            assert!(s.claim(0));
            s.released(0);
        }
        assert!(!s.reports()[0].parked);
        assert!(!s.all_parked());
    }

    #[test]
    fn one_success_clears_a_partial_failure_streak() {
        let s = selector(1);
        s.failed(0);
        s.completed(0, 1_000_000);

        // A flaky chunk should not accumulate toward parking a working lane.
        s.failed(0);
        s.failed(0);
        assert!(!s.reports()[0].parked, "a lane was parked by non-consecutive failures");
    }

    #[test]
    fn a_parked_lane_is_reported_as_needing_replacing_and_a_waiting_one_is_not() {
        // A phone whose owner switched sharing off should be replaced when it
        // comes back. A lane waiting out a rate limit should not: it is coming
        // back by itself, and a second lane would just ask the origin again.
        let s = selector(3);
        s.park(0);
        s.park_for(1, Duration::from_secs(30));
        assert_eq!(s.live(), vec![false, true, true]);
    }

    #[test]
    fn parking_a_lane_directly_takes_it_out_of_rotation() {
        let s = selector(2);
        s.park(1);
        assert!(!s.claim(1));
        assert!(s.claim(0));
        assert!(s.reports()[1].parked);
    }

    #[test]
    fn losing_every_lane_is_reported_rather_than_hanging() {
        let s = selector(2);
        for lane in 0..2 {
            for _ in 0..FAILURES_BEFORE_PARKED {
                s.failed(lane);
            }
        }
        assert!(s.all_parked());
        assert!(s.all_parked_permanently());
    }

    #[test]
    fn aggregate_throughput_counts_live_lanes_only() {
        let s = selector(3);
        let now = Instant::now();
        for lane in 0..3 {
            carried(&s, lane, 1_000_000, Duration::from_secs(1), now);
        }
        assert!((s.aggregate_throughput_at(now) - 3_000_000.0).abs() < 1.0);

        for _ in 0..FAILURES_BEFORE_PARKED {
            s.failed(2);
        }
        let live = s.aggregate_throughput_at(now);
        assert!((live - 2_000_000.0).abs() < 1.0, "parked lane still counted: {live}");
    }

    #[test]
    fn reports_track_bytes_and_chunks_per_lane() {
        let s = selector(2);
        s.completed(0, 500);
        s.completed(0, 500);
        s.completed(1, 200);

        let reports = s.reports();
        assert_eq!((reports[0].bytes, reports[0].chunks), (1000, 2));
        assert_eq!((reports[1].bytes, reports[1].chunks), (200, 1));
        assert_eq!(reports[0].label, "lane0");
    }

    #[test]
    fn a_rate_limited_lane_comes_back_when_its_period_is_up() {
        // Parking permanently would mean one 429 costs an interface for the
        // rest of the transfer.
        let s = LaneSelector::new(vec!["en0".into()]);
        s.park_for(0, Duration::from_millis(40));
        assert!(!s.claim(0), "the lane should be out of rotation while it waits");
        assert!(s.all_parked());
        assert!(!s.all_parked_permanently(), "it is waiting, not dead");

        std::thread::sleep(Duration::from_millis(60));
        assert!(s.claim(0), "the lane should be back after its period");
    }

    #[test]
    fn a_waiting_lane_says_how_long_and_a_dead_one_says_never() {
        let s = LaneSelector::new(vec!["en0".into(), "en1".into()]);
        s.park_for(0, Duration::from_secs(10));
        s.park(1);
        assert!(s.park_remaining(0).expect("en0 is coming back") > Duration::from_secs(5));
        assert_eq!(s.park_remaining(1), None, "a dead lane must not be waited on");
        assert_eq!(s.park_remaining(9), None, "an index that does not exist");
    }

    #[test]
    fn a_timed_park_is_told_apart_from_a_failed_one() {
        let s = LaneSelector::new(vec!["en0".into(), "en1".into()]);
        s.park(0);
        s.park_for(1, Duration::from_secs(30));
        assert!(s.all_parked());
        assert!(!s.all_parked_permanently(), "en1 is coming back");

        let s = LaneSelector::new(vec!["en0".into()]);
        s.park(0);
        assert!(s.all_parked_permanently(), "nothing here is coming back");
        assert!(s.time_until_unpark().is_none());
    }

    #[test]
    fn the_longer_of_two_waits_stands() {
        // Two limits arriving together must not let the shorter one release
        // the lane early.
        let s = LaneSelector::new(vec!["en0".into()]);
        s.park_for(0, Duration::from_secs(30));
        s.park_for(0, Duration::from_millis(1));
        let remaining = s.time_until_unpark().expect("still parked");
        assert!(remaining > Duration::from_secs(20), "the long wait was shortened: {remaining:?}");
    }

    #[test]
    fn waiting_out_a_limit_clears_the_strikes_against_a_lane() {
        // The wait was the punishment. Coming back one failure from being
        // parked for good would make a rate limit compound into a park.
        let s = LaneSelector::new(vec!["en0".into()]);
        for _ in 0..FAILURES_BEFORE_PARKED - 1 {
            s.failed(0);
        }
        s.park_for(0, Duration::from_millis(30));
        std::thread::sleep(Duration::from_millis(50));

        assert!(s.claim(0));
        s.failed(0);
        assert!(!s.reports()[0].parked, "one failure after a wait should not park the lane");
    }

    #[test]
    fn a_timed_park_does_not_count_as_a_failure() {
        // A rate limit is the origin being busy, not the interface being bad.
        let s = LaneSelector::new(vec!["en0".into()]);
        s.park_for(0, Duration::from_millis(10));
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(s.reports()[0].chunks, 0);
        assert!(s.claim(0));
    }
}
