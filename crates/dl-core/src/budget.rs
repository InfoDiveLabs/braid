//! Bandwidth limiting.
//!
//! Limits form a hierarchy: global, then per-interface, then per-download: //! and a read must satisfy every level it sits under. The two levels mean
//! different things: a global cap is what someone means by "don't use more than
//! 5 MB/s", while a per-interface cap is a ceiling on one metered link. A
//! saturated interface cap must therefore push work to other interfaces rather
//! than throttle the whole download.
//!
//! Grants are partial on purpose. A reader asking for 64 KiB when 8 KiB of
//! allowance exists is given 8 KiB, so a request larger than the burst size can
//! never deadlock waiting for an allowance that will not arrive.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Monotonic time, injectable so limiter tests are deterministic.
pub trait Clock: Send + Sync + 'static {
    fn now_nanos(&self) -> u64;
}

pub struct SystemClock {
    origin: Instant,
}

impl Default for SystemClock {
    fn default() -> Self {
        Self { origin: Instant::now() }
    }
}

impl Clock for SystemClock {
    fn now_nanos(&self) -> u64 {
        self.origin.elapsed().as_nanos() as u64
    }
}

/// A clock that only moves when a test moves it.
#[derive(Default)]
pub struct TestClock(AtomicU64);

impl TestClock {
    pub fn advance(&self, by: Duration) {
        self.0.fetch_add(by.as_nanos() as u64, Ordering::SeqCst);
    }
}

impl Clock for TestClock {
    fn now_nanos(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

const NANOS_PER_SEC: f64 = 1_000_000_000.0;

#[derive(Debug)]
struct BucketState {
    tokens: f64,
    last_nanos: u64,
}

impl BucketState {
    /// Add the allowance earned since the last refill, capped at the burst size.
    ///
    /// Capping is what stops an idle limiter banking hours of unused allowance
    /// and then releasing it all at once.
    fn refill(&mut self, now_nanos: u64, rate: u64, capacity: f64) {
        let elapsed = now_nanos.saturating_sub(self.last_nanos);
        self.last_nanos = now_nanos;
        if rate == 0 {
            return;
        }
        let earned = rate as f64 * (elapsed as f64 / NANOS_PER_SEC);
        self.tokens = (self.tokens + earned).min(capacity);
    }
}

/// One level of the limit hierarchy.
pub struct Budget {
    /// Bytes per second; zero means unlimited.
    rate: AtomicU64,
    /// Burst size in bytes.
    capacity: AtomicU64,
    state: Mutex<BucketState>,
    clock: Arc<dyn Clock>,
    notify: tokio::sync::Notify,
}

impl Budget {
    pub fn unlimited() -> Arc<Self> {
        Self::new(0, Arc::new(SystemClock::default()))
    }

    pub fn with_rate(bytes_per_sec: u64) -> Arc<Self> {
        Self::new(bytes_per_sec, Arc::new(SystemClock::default()))
    }

    pub fn new(bytes_per_sec: u64, clock: Arc<dyn Clock>) -> Arc<Self> {
        // One second of allowance: enough to absorb normal jitter, short enough
        // that a burst cannot meaningfully exceed the stated rate.
        let capacity = bytes_per_sec.max(1);
        let now = clock.now_nanos();
        Arc::new(Self {
            rate: AtomicU64::new(bytes_per_sec),
            capacity: AtomicU64::new(capacity),
            state: Mutex::new(BucketState { tokens: capacity as f64, last_nanos: now }),
            clock,
            notify: tokio::sync::Notify::new(),
        })
    }

    pub fn rate(&self) -> u64 {
        self.rate.load(Ordering::Relaxed)
    }

    pub fn is_unlimited(&self) -> bool {
        self.rate() == 0
    }

    /// Change the limit. Used by the scheduler when a time window opens.
    pub fn set_rate(&self, bytes_per_sec: u64) {
        self.rate.store(bytes_per_sec, Ordering::Relaxed);
        self.capacity.store(bytes_per_sec.max(1), Ordering::Relaxed);
        // Anything blocked on the old, lower rate should re-evaluate.
        self.notify.notify_waiters();
    }

    /// Whether this level currently has no allowance to give.
    pub fn is_saturated(&self) -> bool {
        if self.is_unlimited() {
            return false;
        }
        self.available() == 0
    }

    /// Allowance available right now, in bytes.
    pub fn available(&self) -> u64 {
        if self.is_unlimited() {
            return u64::MAX;
        }
        let mut state = self.state.lock().unwrap();
        state.refill(
            self.clock.now_nanos(),
            self.rate(),
            self.capacity.load(Ordering::Relaxed) as f64,
        );
        state.tokens.max(0.0) as u64
    }

    /// Take up to `want` bytes of allowance without waiting.
    fn try_take(&self, want: u64) -> u64 {
        if self.is_unlimited() {
            return want;
        }
        let mut state = self.state.lock().unwrap();
        state.refill(
            self.clock.now_nanos(),
            self.rate(),
            self.capacity.load(Ordering::Relaxed) as f64,
        );

        let granted = want.min(state.tokens.max(0.0) as u64);
        state.tokens -= granted as f64;
        granted
    }

    /// How long until at least one byte of allowance exists.
    fn wait_hint(&self) -> Duration {
        let rate = self.rate();
        if rate == 0 {
            return Duration::ZERO;
        }
        // At least a millisecond: sleeping for a single byte's worth of time
        // would spin.
        Duration::from_millis(1).max(Duration::from_secs_f64(1.0 / rate as f64))
    }
}

/// The chain of limits a single stream of bytes must satisfy.
#[derive(Clone, Default)]
pub struct BudgetChain {
    levels: Vec<Arc<Budget>>,
}

impl BudgetChain {
    pub fn new(levels: Vec<Arc<Budget>>) -> Self {
        Self { levels }
    }

    pub fn unlimited() -> Self {
        Self::default()
    }

    pub fn push(&mut self, budget: Arc<Budget>) {
        self.levels.push(budget);
    }

    pub fn is_unlimited(&self) -> bool {
        self.levels.iter().all(|b| b.is_unlimited())
    }

    /// Reserve up to `want` bytes, waiting until at least one byte is free.
    ///
    /// Every level is charged the same amount, so no level can be over-drawn by
    /// traffic that another level ultimately refused. Levels are taken in order
    ///: innermost first: so a download blocked on its own limit does not
    /// first consume global allowance that others could have used.
    pub async fn acquire(&self, want: usize) -> usize {
        if want == 0 || self.is_unlimited() {
            return want;
        }

        loop {
            let granted = self.try_acquire(want);
            if granted > 0 {
                return granted;
            }

            let hint = self
                .levels
                .iter()
                .filter(|b| !b.is_unlimited())
                .map(|b| b.wait_hint())
                .max()
                .unwrap_or(Duration::from_millis(1));
            tokio::time::sleep(hint).await;
        }
    }

    /// Charge every level the smallest amount any of them can afford.
    fn try_acquire(&self, want: usize) -> usize {
        let want = want as u64;

        // Smallest allowance across the chain, so the charge below can never
        // exceed what any single level had.
        let granted = self
            .levels
            .iter()
            .map(|b| if b.is_unlimited() { want } else { b.available().min(want) })
            .min()
            .unwrap_or(want);

        if granted == 0 {
            return 0;
        }
        for level in &self.levels {
            level.try_take(granted);
        }
        granted as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn budget(rate: u64) -> (Arc<Budget>, Arc<TestClock>) {
        let clock = Arc::new(TestClock::default());
        (Budget::new(rate, clock.clone()), clock)
    }

    #[test]
    fn an_unlimited_budget_grants_everything_immediately() {
        let (b, _) = budget(0);
        assert!(b.is_unlimited());
        assert_eq!(b.try_take(1 << 30), 1 << 30);
        assert!(!b.is_saturated());
    }

    #[test]
    fn allowance_is_capped_rather_than_banked_while_idle() {
        let (b, clock) = budget(1000);
        // An hour idle must not buy an hour's worth of burst.
        clock.advance(Duration::from_secs(3600));
        assert_eq!(b.available(), 1000, "idle time was banked into the burst");
    }

    #[test]
    fn a_request_larger_than_the_burst_is_granted_partially() {
        // Without partial grants this would wait forever for an allowance that
        // the burst size can never reach.
        let (b, _) = budget(1000);
        assert_eq!(b.try_take(1_000_000), 1000);
    }

    #[test]
    fn allowance_accrues_at_the_configured_rate() {
        let (b, clock) = budget(1000);
        b.try_take(1000);
        assert_eq!(b.available(), 0);

        clock.advance(Duration::from_millis(500));
        assert_eq!(b.available(), 500);
        clock.advance(Duration::from_millis(500));
        assert_eq!(b.available(), 1000);
    }

    #[test]
    fn raising_the_limit_takes_effect_immediately() {
        let (b, clock) = budget(1000);
        b.try_take(1000);
        b.set_rate(10_000);
        clock.advance(Duration::from_secs(1));
        assert_eq!(b.available(), 10_000);
    }

    #[test]
    fn the_chain_charges_every_level_the_same_amount() {
        let clock = Arc::new(TestClock::default());
        let global = Budget::new(10_000, clock.clone());
        let per_download = Budget::new(4_000, clock.clone());
        let chain = BudgetChain::new(vec![per_download.clone(), global.clone()]);

        // The tighter level decides, and the looser one is charged only what
        // was actually granted.
        assert_eq!(chain.try_acquire(8_000), 4_000);
        assert_eq!(per_download.available(), 0);
        assert_eq!(global.available(), 6_000);
    }

    #[test]
    fn one_saturated_level_blocks_the_chain() {
        let clock = Arc::new(TestClock::default());
        let global = Budget::new(10_000, clock.clone());
        let per_download = Budget::new(1_000, clock.clone());
        let chain = BudgetChain::new(vec![per_download, global]);

        assert_eq!(chain.try_acquire(5_000), 1_000);
        assert_eq!(chain.try_acquire(5_000), 0, "a drained level must stop the chain");
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(1500))]

        /// The invariant a rate limiter exists for: over any period, the total
        /// granted cannot exceed what the rate allows plus one burst.
        #[test]
        fn total_granted_never_exceeds_the_rate_over_time(
            rate in 1u64..10_000_000,
            steps in prop::collection::vec((1u64..50_000, 1u64..500_000), 1..60),
        ) {
            let (b, clock) = budget(rate);
            let mut granted_total: u64 = 0;
            let mut elapsed_nanos: u64 = 0;

            for (want, advance_micros) in steps {
                clock.advance(Duration::from_micros(advance_micros));
                elapsed_nanos += advance_micros * 1_000;
                granted_total += b.try_take(want);
            }

            let allowed =
                rate as f64 * (elapsed_nanos as f64 / NANOS_PER_SEC) + b.capacity.load(Ordering::Relaxed) as f64;
            prop_assert!(
                granted_total as f64 <= allowed + 1.0,
                "granted {granted_total} exceeds the permitted {allowed}"
            );
        }

        /// A chain can never grant more than its tightest level would alone.
        #[test]
        fn a_chain_never_grants_more_than_its_tightest_level(
            rates in prop::collection::vec(1u64..1_000_000, 1..4),
            wants in prop::collection::vec(1u64..200_000, 1..25),
        ) {
            let clock = Arc::new(TestClock::default());
            let levels: Vec<_> =
                rates.iter().map(|r| Budget::new(*r, clock.clone())).collect();
            let chain = BudgetChain::new(levels);
            let tightest = *rates.iter().min().unwrap();

            let mut total: u64 = 0;
            let mut elapsed_nanos: u64 = 0;
            for want in wants {
                clock.advance(Duration::from_millis(10));
                elapsed_nanos += 10_000_000;
                total += chain.try_acquire(want as usize) as u64;
            }

            let allowed = tightest as f64 * (elapsed_nanos as f64 / NANOS_PER_SEC)
                + tightest.max(1) as f64;
            prop_assert!(
                total as f64 <= allowed + 1.0,
                "chain granted {total}, tightest level permits {allowed}"
            );
        }

        /// Never hand out more than was asked for.
        #[test]
        fn a_grant_never_exceeds_the_request(rate in 1u64..1_000_000, want in 1u64..1_000_000) {
            let (b, _) = budget(rate);
            prop_assert!(b.try_take(want) <= want);
        }
    }
}
