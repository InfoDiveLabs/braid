//! Independent paths to the same bytes, and how work is spread across them.
//!
//! A lane is usually one network interface, but the engine never learns that.
//! It sees a set of sources that fetch the same resource at different speeds
//! and fail independently, which is also the shape of multiple mirrors: so
//! phase 7 reuses this rather than adding a parallel mechanism.

use crate::source::ByteSource;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Weight given to each new measurement. Low enough to ride out one slow
/// chunk, high enough to notice an interface that has actually degraded.
const EWMA_ALPHA: f64 = 0.3;

/// Consecutive failures before a lane is taken out of rotation.
const FAILURES_BEFORE_PARKED: u32 = 3;

pub trait LaneSet: Send + Sync {
    fn len(&self) -> usize;
    fn source(&self, lane: usize) -> &dyn ByteSource;
    fn label(&self, lane: usize) -> &str;

    fn is_empty(&self) -> bool {
        self.len() == 0
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

/// How often a lane's rate is recalculated while a chunk is in flight.
///
/// Short enough that the sidebar reads as live and the assignment reacts to a
/// path that just degraded; long enough that a fast lane is not sampling on
/// every read.
const SAMPLE_WINDOW: Duration = Duration::from_millis(250);

/// Fold a rate observation into a lane's smoothed estimate.
fn fold(entry: &mut LaneState, sample: f64) {
    entry.throughput = Some(match entry.throughput {
        Some(previous) => previous * (1.0 - EWMA_ALPHA) + sample * EWMA_ALPHA,
        None => sample,
    });
}

#[derive(Clone, Debug, Default)]
struct LaneState {
    /// Smoothed throughput in bytes per second. `None` until first measured.
    throughput: Option<f64>,
    /// Configured ceiling in bytes per second, if this lane is capped.
    cap: Option<f64>,
    inflight: u32,
    bytes: u64,
    /// Bytes seen since the last rate sample, and when that sample was taken.
    ///
    /// Without these a lane's rate was only recalculated when a chunk
    /// finished, so a slow path reported a figure minutes old and the
    /// assignment kept weighting it on what it used to be worth.
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

impl LaneState {
    /// Higher is better. Dividing by in-flight work stops the fastest lane
    /// being handed every chunk while it is already saturated.
    ///
    /// A configured cap bounds the estimate and, before any measurement
    /// exists, stands in for one. Otherwise a lane throttled to 100 KB/s would
    /// be handed the same share as an unthrottled one until enough chunks had
    /// crawled through it to teach the average otherwise.
    fn score(&self) -> f64 {
        let estimate = match (self.throughput, self.cap) {
            (Some(measured), Some(cap)) => measured.min(cap),
            (Some(measured), None) => measured,
            (None, Some(cap)) => cap,
            (None, None) => 0.0,
        };
        estimate / (self.inflight as f64 + 1.0)
    }
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

/// Chooses which lane should carry the next chunk.
pub struct LaneSelector {
    labels: Vec<String>,
    state: Mutex<Vec<LaneState>>,
}

impl LaneSelector {
    pub fn new(labels: Vec<String>) -> Self {
        let state = vec![LaneState::default(); labels.len()];
        Self { labels, state: Mutex::new(state) }
    }

    pub fn from_lanes(lanes: &dyn LaneSet) -> Self {
        Self::new((0..lanes.len()).map(|i| lanes.label(i).to_string()).collect())
    }

    pub fn len(&self) -> usize {
        self.labels.len()
    }

    pub fn is_empty(&self) -> bool {
        self.labels.is_empty()
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

    /// Take a lane out of rotation for a stated period.
    ///
    /// Used for a rate limit, where the origin has told us how long to wait.
    /// The lane is not failing: it is being asked to be patient: so this does
    /// not count a strike against it.
    pub fn park_for(&self, lane: usize, period: Duration) {
        let mut state = self.state.lock().unwrap();
        if let Some(entry) = state.get_mut(lane) {
            entry.inflight = entry.inflight.saturating_sub(1);
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

    /// Claim a lane for one chunk, or `None` if every lane is parked.
    ///
    /// Unmeasured lanes are claimed first: a lane with no measurement has a
    /// score of zero and would never be chosen on throughput alone, so the
    /// fastest interface found first would keep all the work and the others
    /// would never be tried.
    pub fn acquire(&self) -> Option<usize> {
        let mut state = self.state.lock().unwrap();
        // Checked here rather than on a timer: this is the only place the
        // answer matters, so a lane cannot be left parked past its period by
        // nobody having asked.
        Self::expire_parks(&mut state, Instant::now());

        let cold = state
            .iter()
            .enumerate()
            .filter(|(_, s)| !s.parked && s.throughput.is_none() && s.inflight == 0)
            .map(|(i, _)| i)
            .next();

        let chosen = cold.or_else(|| {
            state
                .iter()
                .enumerate()
                .filter(|(_, s)| !s.parked)
                .max_by(|(_, a), (_, b)| {
                    a.score().partial_cmp(&b.score()).unwrap_or(std::cmp::Ordering::Equal)
                })
                .map(|(i, _)| i)
        })?;

        state[chosen].inflight += 1;
        Some(chosen)
    }

    /// Report a completed chunk and update the lane's measured throughput.
    pub fn completed(&self, lane: usize, bytes: u64, elapsed: Duration) {
        let mut state = self.state.lock().unwrap();
        let Some(entry) = state.get_mut(lane) else { return };

        entry.inflight = entry.inflight.saturating_sub(1);
        entry.bytes += bytes;
        entry.chunks += 1;
        entry.consecutive_failures = 0;
        entry.live_bytes = 0;
        entry.live_at = None;

        let secs = elapsed.as_secs_f64();
        if secs > 0.0 {
            fold(entry, bytes as f64 / secs);
        }
    }

    /// Bytes have arrived on a lane, mid-chunk.
    ///
    /// Called as the body streams rather than when it ends, which is what
    /// makes a rate current. The window keeps the cost to one sample every
    /// [`SAMPLE_WINDOW`] however small the reads are, and the same figure
    /// feeds the assignment, so work moves off a path that has just slowed
    /// down instead of after its chunk eventually lands.
    pub fn progressed(&self, lane: usize, bytes: u64) {
        let mut state = self.state.lock().unwrap();
        let Some(entry) = state.get_mut(lane) else { return };
        entry.live_bytes += bytes;

        let started = *entry.live_at.get_or_insert_with(Instant::now);
        let elapsed = started.elapsed();
        if elapsed < SAMPLE_WINDOW {
            return;
        }

        let secs = elapsed.as_secs_f64();
        if secs > 0.0 {
            fold(entry, entry.live_bytes as f64 / secs);
        }
        entry.live_bytes = 0;
        entry.live_at = Some(Instant::now());
    }

    /// Release a lane after a failure that was not the lane's fault.
    ///
    /// An origin that ignores `Range`, or a resource that changed, fails
    /// identically on every path. Counting those against the lane would park
    /// each interface in turn and then report "all paths failed" instead of
    /// the actual cause.
    pub fn released(&self, lane: usize) {
        let mut state = self.state.lock().unwrap();
        if let Some(entry) = state.get_mut(lane) {
            entry.inflight = entry.inflight.saturating_sub(1);
        }
    }

    /// Report a failed chunk. Repeated failures park the lane so work migrates
    /// to the others instead of retrying a NIC that has gone away.
    pub fn failed(&self, lane: usize) {
        let mut state = self.state.lock().unwrap();
        let Some(entry) = state.get_mut(lane) else { return };

        entry.inflight = entry.inflight.saturating_sub(1);
        entry.consecutive_failures += 1;
        if entry.consecutive_failures >= FAILURES_BEFORE_PARKED {
            entry.parked = true;
        }
    }

    /// Tell the selector a lane is capped, so assignment reflects the ceiling
    /// from the first chunk rather than after learning it the slow way.
    pub fn set_cap(&self, lane: usize, bytes_per_sec: Option<u64>) {
        let mut state = self.state.lock().unwrap();
        if let Some(entry) = state.get_mut(lane) {
            entry.cap = bytes_per_sec.filter(|r| *r > 0).map(|r| r as f64);
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

    pub fn reports(&self) -> Vec<LaneReport> {
        let state = self.state.lock().unwrap();
        state
            .iter()
            .enumerate()
            .map(|(lane, s)| LaneReport {
                lane,
                label: self.labels[lane].clone(),
                bytes: s.bytes,
                chunks: s.chunks,
                throughput: s.throughput,
                parked: s.parked,
            })
            .collect()
    }

    /// Combined throughput across every lane, which is the number that should
    /// exceed any single interface for aggregation to be worth anything.
    pub fn aggregate_throughput(&self) -> f64 {
        let state = self.state.lock().unwrap();
        state.iter().filter(|s| !s.parked).filter_map(|s| s.throughput).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selector(n: usize) -> LaneSelector {
        LaneSelector::new((0..n).map(|i| format!("lane{i}")).collect())
    }

    /// Without this, the first lane measured would score highest and keep every
    /// chunk, leaving the other interfaces idle forever.
    #[test]
    fn every_lane_is_tried_before_throughput_decides() {
        let s = selector(3);
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..3 {
            let lane = s.acquire().expect("a lane should be available");
            seen.insert(lane);
            s.completed(lane, 1 << 20, Duration::from_millis(100));
        }
        assert_eq!(seen.len(), 3, "some lane was never tried: {seen:?}");
    }

    #[test]
    fn a_faster_lane_receives_more_work() {
        let s = selector(2);
        // Lane 0 is ten times faster than lane 1.
        s.acquire();
        s.completed(0, 10 << 20, Duration::from_millis(100));
        s.acquire();
        s.completed(1, 1 << 20, Duration::from_millis(100));

        let mut counts = [0usize; 2];
        for _ in 0..40 {
            let lane = s.acquire().unwrap();
            counts[lane] += 1;
            let bytes = if lane == 0 { 10 << 20 } else { 1 << 20 };
            s.completed(lane, bytes, Duration::from_millis(100));
        }
        assert!(counts[0] > counts[1], "the faster lane should get more chunks, got {counts:?}");
    }

    #[test]
    fn in_flight_work_stops_one_lane_hoarding_every_chunk() {
        let s = selector(2);
        s.acquire();
        s.completed(0, 10 << 20, Duration::from_millis(100));
        s.acquire();
        s.completed(1, 5 << 20, Duration::from_millis(100));

        // Claim without completing: the fast lane's score falls as its queue
        // grows, so the slower lane eventually gets chosen.
        let mut lanes = Vec::new();
        for _ in 0..6 {
            lanes.push(s.acquire().unwrap());
        }
        assert!(lanes.contains(&1), "a saturated fast lane should yield: {lanes:?}");
    }

    #[test]
    fn a_repeatedly_failing_lane_is_parked_and_work_migrates() {
        let s = selector(2);
        for _ in 0..FAILURES_BEFORE_PARKED {
            s.acquire();
            s.failed(0);
        }
        // Every subsequent acquisition must avoid the dead lane.
        for _ in 0..10 {
            let lane = s.acquire().expect("the healthy lane is still available");
            assert_ne!(lane, 0, "work was sent to a parked lane");
            s.completed(lane, 1 << 20, Duration::from_millis(50));
        }
        assert!(s.reports()[0].parked);
        assert!(!s.all_parked());
    }

    /// A fault in the origin fails on every lane at once; parking them for it
    /// would destroy the download and hide the real error.
    #[test]
    fn a_failure_that_is_not_the_lanes_fault_does_not_park_it() {
        let s = selector(1);
        for _ in 0..FAILURES_BEFORE_PARKED * 3 {
            let lane = s.acquire().expect("the lane must stay available");
            s.released(lane);
        }
        assert!(!s.reports()[0].parked);
        assert!(!s.all_parked());
    }

    #[test]
    fn released_lanes_do_not_leak_in_flight_slots() {
        let s = selector(2);
        for _ in 0..5 {
            let lane = s.acquire().unwrap();
            s.released(lane);
        }
        // With in-flight counts leaking, scores would decay to zero and the
        // selector would spread work on stale information.
        s.acquire();
        s.completed(0, 1 << 20, Duration::from_millis(10));
        assert!(s.reports()[0].throughput.unwrap() > 0.0);
    }

    #[test]
    fn one_success_clears_a_partial_failure_streak() {
        let s = selector(1);
        s.acquire();
        s.failed(0);
        s.acquire();
        s.completed(0, 1 << 20, Duration::from_millis(10));

        // A flaky chunk should not accumulate toward parking a working lane.
        s.acquire();
        s.failed(0);
        s.acquire();
        s.failed(0);
        assert!(!s.reports()[0].parked, "a lane was parked by non-consecutive failures");
    }

    /// The plan's requirement: a saturated interface cap must move work to the
    /// other interfaces rather than throttle the whole download.
    #[test]
    fn a_capped_lane_receives_less_work_than_an_uncapped_one() {
        let s = selector(2);
        s.set_cap(0, Some(100 << 10));

        let mut counts = [0usize; 2];
        for _ in 0..60 {
            let lane = s.acquire().unwrap();
            counts[lane] += 1;
            // Both lanes are physically equal; only the cap differs.
            s.completed(lane, 8 << 20, Duration::from_millis(100));
        }
        assert!(counts[1] > counts[0] * 3, "the capped lane should carry far less: {counts:?}");
        assert!(counts[0] > 0, "a capped lane is still useful and must not be abandoned");
    }

    #[test]
    fn a_cap_is_used_as_the_estimate_before_anything_is_measured() {
        let s = selector(2);
        s.set_cap(0, Some(1 << 10));
        s.set_cap(1, Some(10 << 20));

        // Cold start: both lanes are tried once, then the faster ceiling wins.
        s.acquire();
        s.acquire();
        let mut counts = [0usize; 2];
        for _ in 0..10 {
            counts[s.acquire().unwrap()] += 1;
        }
        assert!(counts[1] >= counts[0], "the higher ceiling should be preferred: {counts:?}");
    }

    #[test]
    fn clearing_a_cap_restores_the_measured_estimate() {
        let s = selector(1);
        s.set_cap(0, Some(1000));
        s.acquire();
        s.completed(0, 10 << 20, Duration::from_secs(1));
        s.set_cap(0, None);
        assert!(s.reports()[0].throughput.unwrap() > 1000.0);
    }

    #[test]
    fn parking_a_lane_directly_takes_it_out_of_rotation() {
        let s = selector(2);
        s.park(1);
        for _ in 0..8 {
            let lane = s.acquire().expect("the other lane remains");
            assert_ne!(lane, 1, "a parked lane was handed work");
            s.completed(lane, 1 << 20, Duration::from_millis(10));
        }
        assert!(s.reports()[1].parked);
    }

    #[test]
    fn losing_every_lane_is_reported_rather_than_hanging() {
        let s = selector(2);
        for lane in 0..2 {
            for _ in 0..FAILURES_BEFORE_PARKED {
                s.acquire();
                s.failed(lane);
            }
        }
        assert!(s.all_parked());
        assert_eq!(s.acquire(), None, "a caller must be able to tell that no lane is left");
    }

    #[test]
    fn aggregate_throughput_sums_live_lanes_only() {
        let s = selector(3);
        for lane in 0..3 {
            s.acquire();
            s.completed(lane, 1 << 20, Duration::from_secs(1));
        }
        let all = s.aggregate_throughput();
        assert!((all - 3.0 * (1 << 20) as f64).abs() < 1.0, "got {all}");

        for _ in 0..FAILURES_BEFORE_PARKED {
            s.acquire();
            s.failed(2);
        }
        let live = s.aggregate_throughput();
        assert!((live - 2.0 * (1 << 20) as f64).abs() < 1.0, "parked lane still counted: {live}");
    }

    #[test]
    fn reports_track_bytes_and_chunks_per_lane() {
        let s = selector(2);
        s.acquire();
        s.completed(0, 500, Duration::from_millis(10));
        s.acquire();
        s.completed(0, 500, Duration::from_millis(10));
        s.acquire();
        s.completed(1, 200, Duration::from_millis(10));

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
        assert!(s.acquire().is_none(), "the lane should be out of rotation while it waits");
        assert!(s.all_parked());
        assert!(!s.all_parked_permanently(), "it is waiting, not dead");

        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(s.acquire(), Some(0), "the lane should be back after its period");
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
            s.acquire();
            s.failed(0);
        }
        s.park_for(0, Duration::from_millis(30));
        std::thread::sleep(Duration::from_millis(50));

        assert_eq!(s.acquire(), Some(0));
        s.failed(0);
        assert!(!s.reports()[0].parked, "one failure after a wait should not park the lane");
    }

    #[test]
    fn a_timed_park_does_not_count_as_a_failure() {
        // A rate limit is the origin being busy, not the interface being bad.
        let s = LaneSelector::new(vec!["en0".into()]);
        s.acquire();
        s.park_for(0, Duration::from_millis(10));
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(s.reports()[0].chunks, 0);
        assert_eq!(s.acquire(), Some(0));
    }
}
