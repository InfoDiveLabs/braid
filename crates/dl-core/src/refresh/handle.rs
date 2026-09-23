//! Single-flight resolution.
//!
//! Sixteen chunks discover a dead link within microseconds of each other. The
//! generation counter is what turns that into one refresh: a task records the
//! generation it was using, and a refresh that finds the generation already
//! advanced returns the new source instead of resolving again.

use crate::error::{Error, Result};
use crate::refresh::LinkRefresher;
use crate::refresh::expiry::lifetime_of;
use crate::refresh::model::{RefreshCtx, RefreshPolicy, RefreshReason, ResolvedSource, SourceKey};
use arc_swap::ArcSwap;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A cap on refreshes shared by every lane of one download.
///
/// Per-handle counting would let a four-lane download chase a permanently dead
/// link four times as long, which is the opposite of what a cap is for.
#[derive(Debug)]
pub struct RefreshBudget {
    used: AtomicU32,
    max: u32,
}

impl RefreshBudget {
    pub fn new(max: u32) -> Arc<Self> {
        Arc::new(Self { used: AtomicU32::new(0), max })
    }

    pub fn used(&self) -> u32 {
        self.used.load(Ordering::SeqCst)
    }

    pub fn max(&self) -> u32 {
        self.max
    }

    fn take(&self) -> bool {
        self.used
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |used| {
                (used < self.max).then_some(used + 1)
            })
            .is_ok()
    }
}

/// State that only the refresh gate may touch.
#[derive(Debug, Default)]
struct Gate {
    consecutive_failures: u32,
    /// Once open, this source is given up on. A resolver that has failed
    /// repeatedly is not going to start working within one download.
    open: bool,
}

/// One resolved source, per `(source, lane)`.
pub struct SourceHandle {
    key: SourceKey,
    original_url: String,
    /// Read on the hot path by every chunk, so it must not take a lock.
    current: ArcSwap<ResolvedSource>,
    /// Serialises refreshes and guards the failure state they produce.
    gate: tokio::sync::Mutex<Gate>,
    refresher: Arc<dyn LinkRefresher>,
    policy: RefreshPolicy,
    budget: Arc<RefreshBudget>,
    resolves: AtomicU64,
}

impl SourceHandle {
    pub fn new(
        key: SourceKey,
        original_url: impl Into<String>,
        refresher: Arc<dyn LinkRefresher>,
        policy: RefreshPolicy,
        budget: Arc<RefreshBudget>,
    ) -> Arc<Self> {
        let original_url = original_url.into();
        let initial = ResolvedSource::new(original_url.clone());
        Self::starting_from(key, original_url, initial, refresher, policy, budget)
    }

    /// Start from a source that has already been resolved once: a signed link
    /// the user was handed, with the headers that go with it.
    pub fn starting_from(
        key: SourceKey,
        original_url: impl Into<String>,
        initial: ResolvedSource,
        refresher: Arc<dyn LinkRefresher>,
        policy: RefreshPolicy,
        budget: Arc<RefreshBudget>,
    ) -> Arc<Self> {
        let mut initial = initial;
        initial.generation = 1;
        derive_expiry(&mut initial);

        Arc::new(Self {
            key,
            original_url: original_url.into(),
            current: ArcSwap::from_pointee(initial),
            gate: tokio::sync::Mutex::new(Gate::default()),
            refresher,
            policy,
            budget,
            resolves: AtomicU64::new(0),
        })
    }

    pub fn key(&self) -> &SourceKey {
        &self.key
    }

    pub fn refresher(&self) -> &dyn LinkRefresher {
        self.refresher.as_ref()
    }

    pub fn policy(&self) -> &RefreshPolicy {
        &self.policy
    }

    /// How many times this source was actually resolved. The number tests
    /// assert on, and the one that reveals a refresh stampede.
    pub fn resolves(&self) -> u64 {
        self.resolves.load(Ordering::SeqCst)
    }

    pub fn current(&self) -> Arc<ResolvedSource> {
        self.current.load_full()
    }

    /// The source to use for the next request, refreshed first if its lifetime
    /// is nearly up.
    ///
    /// A proactive refresh that fails is not fatal: the link it would have
    /// replaced may still work, and the reactive path deals with it if not.
    pub async fn current_or_proactive(&self) -> Arc<ResolvedSource> {
        let current = self.current.load_full();
        if !current.is_due_for_refresh(self.policy.proactive_fraction, Instant::now()) {
            return current;
        }
        match self.get_fresh(current.generation, RefreshReason::Proactive).await {
            Ok(fresh) => fresh,
            Err(e) => {
                tracing::warn!(key = %self.key, error = %e, "a proactive refresh failed");
                current
            }
        }
    }

    /// Resolve past `seen_generation`, doing the work exactly once however
    /// many callers arrive together.
    pub async fn get_fresh(
        &self,
        seen_generation: u64,
        reason: RefreshReason,
    ) -> Result<Arc<ResolvedSource>> {
        let mut gate = self.gate.lock().await;

        // Re-checked under the lock: whoever held it first has already done
        // the work, and resolving again would issue a second credential and
        // invalidate the one the other fifteen chunks are about to use.
        let current = self.current.load_full();
        if current.generation > seen_generation {
            return Ok(current);
        }

        if gate.open {
            return Err(Error::RefreshExhausted {
                detail: format!("{} has failed to resolve repeatedly", self.key),
            });
        }
        if !self.budget.take() {
            return Err(Error::RefreshExhausted {
                detail: format!(
                    "reached the limit of {} refreshes for this download",
                    self.budget.max()
                ),
            });
        }

        // Backoff belongs before the attempt, not after a success: it is the
        // failed attempts that must be spaced out.
        if gate.consecutive_failures > 0 {
            tokio::time::sleep(backoff(&self.policy, gate.consecutive_failures)).await;
        }

        let ctx = RefreshCtx {
            key: self.key.clone(),
            original_url: self.original_url.clone(),
            current: Some(Arc::clone(&current)),
            reason,
            attempt: self.budget.used(),
        };

        self.resolves.fetch_add(1, Ordering::SeqCst);
        match self.refresher.resolve(&ctx).await {
            Ok(mut resolved) => {
                gate.consecutive_failures = 0;
                resolved.generation = current.generation + 1;
                derive_expiry(&mut resolved);
                let resolved = Arc::new(resolved);
                self.current.store(Arc::clone(&resolved));
                tracing::debug!(
                    key = %self.key,
                    generation = resolved.generation,
                    refresher = self.refresher.id(),
                    "resolved a fresh source"
                );
                Ok(resolved)
            }
            Err(e) => {
                gate.consecutive_failures += 1;
                if gate.consecutive_failures >= self.policy.failures_before_open {
                    gate.open = true;
                }
                Err(e)
            }
        }
    }
}

/// Fill in an expiry the refresher did not state, from the URL and headers.
fn derive_expiry(source: &mut ResolvedSource) {
    if source.expires_at.is_none()
        && let Some(lifetime) = lifetime_of(&source.url, &source.headers)
    {
        source.expires_at = Some(source.issued_at + lifetime);
    }
}

/// Full jitter over an exponential curve.
///
/// Without the jitter, sixteen lanes that failed together retry together and
/// keep colliding for as long as the backoff lasts.
fn backoff(policy: &RefreshPolicy, failures: u32) -> Duration {
    let exponent = failures.saturating_sub(1).min(16);
    let grown = policy.base_backoff.saturating_mul(1u32 << exponent);
    grown.min(policy.max_backoff).mul_f64(0.5 + 0.5 * unit_random())
}

fn unit_random() -> f64 {
    static STATE: AtomicU64 = AtomicU64::new(0);
    let mut state = STATE.load(Ordering::Relaxed);
    if state == 0 {
        state = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9_7F4A_7C15)
            | 1;
    }
    state ^= state << 13;
    state ^= state >> 7;
    state ^= state << 17;
    STATE.store(state, Ordering::Relaxed);
    (state >> 11) as f64 / (1u64 << 53) as f64
}

/// Hands out one [`SourceHandle`] per `(source, lane)` and holds the shared
/// refresh budget.
pub struct RefreshCoordinator {
    refresher: Arc<dyn LinkRefresher>,
    policy: RefreshPolicy,
    budget: Arc<RefreshBudget>,
    handles: Mutex<BTreeMap<SourceKey, Arc<SourceHandle>>>,
}

impl RefreshCoordinator {
    pub fn new(refresher: Arc<dyn LinkRefresher>, policy: RefreshPolicy) -> Arc<Self> {
        Arc::new(Self {
            refresher,
            budget: RefreshBudget::new(policy.max_refreshes),
            policy,
            handles: Mutex::new(BTreeMap::new()),
        })
    }

    /// The handle for one lane's view of one source, created on first use.
    pub fn handle(&self, key: SourceKey, url: &str) -> Arc<SourceHandle> {
        self.handle_with(key, ResolvedSource::new(url), None)
    }

    /// As [`RefreshCoordinator::handle`], with a starting source and
    /// optionally a refresher of this lane's own.
    ///
    /// The refresher varies per lane because resolution can be bound to the
    /// address that asks: a signature re-issued over the wrong interface is
    /// rejected exactly like the one it replaced, so the refresh request has
    /// to leave by the same path as the chunks it is for.
    pub fn handle_with(
        &self,
        key: SourceKey,
        initial: ResolvedSource,
        refresher: Option<Arc<dyn LinkRefresher>>,
    ) -> Arc<SourceHandle> {
        let mut handles = self.handles.lock().unwrap();
        Arc::clone(handles.entry(key.clone()).or_insert_with(|| {
            let original_url = initial.url.clone();
            SourceHandle::starting_from(
                key,
                original_url,
                initial,
                refresher.unwrap_or_else(|| Arc::clone(&self.refresher)),
                self.policy,
                Arc::clone(&self.budget),
            )
        }))
    }

    pub fn budget(&self) -> &Arc<RefreshBudget> {
        &self.budget
    }

    /// Total resolutions across every lane.
    pub fn resolves(&self) -> u64 {
        self.handles.lock().unwrap().values().map(|h| h.resolves()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::refresh::StaticRefresher;

    /// A refresher that counts, and can be told to fail.
    struct Counting {
        calls: AtomicU64,
        fail: bool,
    }

    #[async_trait::async_trait]
    impl LinkRefresher for Counting {
        fn id(&self) -> &str {
            "counting"
        }
        async fn resolve(&self, ctx: &RefreshCtx) -> Result<ResolvedSource> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(Error::Transport("the resolver is down".into()));
            }
            Ok(ResolvedSource::new(format!("{}?token={n}", ctx.original_url)))
        }
    }

    fn handle_with(refresher: Arc<dyn LinkRefresher>, policy: RefreshPolicy) -> Arc<SourceHandle> {
        SourceHandle::new(
            SourceKey::new("s", 0),
            "http://x.test/f",
            refresher,
            policy,
            RefreshBudget::new(policy.max_refreshes),
        )
    }

    /// The load-bearing property: a link that dies under sixteen chunks at
    /// once costs one resolution, not sixteen.
    #[tokio::test(flavor = "multi_thread")]
    async fn sixteen_chunks_that_expire_together_refresh_exactly_once() {
        let refresher = Arc::new(Counting { calls: AtomicU64::new(0), fail: false });
        let handle =
            handle_with(Arc::clone(&refresher) as Arc<dyn LinkRefresher>, RefreshPolicy::default());
        let seen = handle.current().generation;

        let mut tasks = Vec::new();
        for _ in 0..16 {
            let handle = Arc::clone(&handle);
            tasks.push(tokio::spawn(async move {
                handle.get_fresh(seen, RefreshReason::Rejected).await
            }));
        }

        let mut generations = Vec::new();
        for task in tasks {
            generations
                .push(task.await.unwrap().expect("every chunk should get a source").generation);
        }

        assert_eq!(refresher.calls.load(Ordering::SeqCst), 1, "the refresh stampeded");
        assert_eq!(handle.resolves(), 1);
        assert!(generations.iter().all(|g| *g == seen + 1), "chunks disagreed: {generations:?}");
    }

    #[tokio::test]
    async fn a_second_expiry_refreshes_again_rather_than_latching() {
        // The reason this is a generation counter and not a one-shot cell:
        // links expire more than once during a long download.
        let refresher = Arc::new(Counting { calls: AtomicU64::new(0), fail: false });
        let handle = handle_with(refresher, RefreshPolicy::default());

        let first = handle.get_fresh(1, RefreshReason::Rejected).await.unwrap();
        let second = handle.get_fresh(first.generation, RefreshReason::Rejected).await.unwrap();
        assert_eq!((first.generation, second.generation), (2, 3));
        assert_ne!(first.url, second.url);
    }

    #[tokio::test]
    async fn a_caller_holding_a_stale_generation_is_given_what_already_exists() {
        let refresher = Arc::new(Counting { calls: AtomicU64::new(0), fail: false });
        let handle =
            handle_with(Arc::clone(&refresher) as Arc<dyn LinkRefresher>, RefreshPolicy::default());

        handle.get_fresh(1, RefreshReason::Rejected).await.unwrap();
        let late = handle.get_fresh(1, RefreshReason::Rejected).await.unwrap();
        assert_eq!(late.generation, 2);
        assert_eq!(refresher.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_permanently_dead_link_stops_instead_of_spinning() {
        let policy = RefreshPolicy {
            max_refreshes: 50,
            base_backoff: Duration::from_millis(1),
            ..Default::default()
        };
        let refresher = Arc::new(Counting { calls: AtomicU64::new(0), fail: true });
        let handle = handle_with(Arc::clone(&refresher) as Arc<dyn LinkRefresher>, policy);

        let mut generation = handle.current().generation;
        let mut attempts = 0;
        loop {
            attempts += 1;
            match handle.get_fresh(generation, RefreshReason::Rejected).await {
                Ok(fresh) => generation = fresh.generation,
                Err(Error::RefreshExhausted { .. }) => break,
                Err(_) => {}
            }
            assert!(attempts < 50, "the handle never gave up");
        }
        assert_eq!(
            refresher.calls.load(Ordering::SeqCst),
            policy.failures_before_open as u64,
            "a dead resolver should not be called past the breaker"
        );
    }

    #[tokio::test]
    async fn the_refresh_cap_bounds_a_link_that_resolves_but_never_works() {
        // A refresher can succeed every time and still produce a URL that
        // fails, which is what a StaticRefresher against a dead link does.
        let policy = RefreshPolicy { max_refreshes: 4, ..Default::default() };
        let budget = RefreshBudget::new(policy.max_refreshes);
        let handle = SourceHandle::new(
            SourceKey::new("s", 0),
            "http://x.test/f",
            Arc::new(StaticRefresher),
            policy,
            Arc::clone(&budget),
        );

        let mut generation = handle.current().generation;
        for _ in 0..policy.max_refreshes {
            generation =
                handle.get_fresh(generation, RefreshReason::Rejected).await.unwrap().generation;
        }
        let err = handle
            .get_fresh(generation, RefreshReason::Rejected)
            .await
            .expect_err("the cap must eventually stop the download");
        assert!(matches!(err, Error::RefreshExhausted { .. }), "{err:?}");
        assert_eq!(budget.used(), policy.max_refreshes);
    }

    #[tokio::test]
    async fn the_budget_is_shared_across_lanes() {
        // Otherwise a four-lane download chases a dead link four times as long.
        let policy = RefreshPolicy { max_refreshes: 3, ..Default::default() };
        let coordinator = RefreshCoordinator::new(Arc::new(StaticRefresher), policy);
        let a = coordinator.handle(SourceKey::new("s", 0), "http://x.test/f");
        let b = coordinator.handle(SourceKey::new("s", 1), "http://x.test/f");

        a.get_fresh(1, RefreshReason::Rejected).await.unwrap();
        b.get_fresh(1, RefreshReason::Rejected).await.unwrap();
        a.get_fresh(2, RefreshReason::Rejected).await.unwrap();
        assert!(b.get_fresh(2, RefreshReason::Rejected).await.is_err());
        assert_eq!(coordinator.resolves(), 3);
    }

    #[tokio::test]
    async fn each_lane_resolves_its_own_source() {
        // A signature bound to the requesting address is valid on one lane and
        // rejected on every other, so lanes cannot share one resolution.
        let refresher = Arc::new(Counting { calls: AtomicU64::new(0), fail: false });
        let coordinator = RefreshCoordinator::new(refresher, RefreshPolicy::default());
        let a = coordinator.handle(SourceKey::new("s", 0), "http://x.test/f");
        let b = coordinator.handle(SourceKey::new("s", 1), "http://x.test/f");

        let a_url = a.get_fresh(1, RefreshReason::Rejected).await.unwrap().url.clone();
        let b_url = b.get_fresh(1, RefreshReason::Rejected).await.unwrap().url.clone();
        assert_ne!(a_url, b_url, "two lanes were handed the same signed url");
    }

    #[tokio::test]
    async fn a_link_with_a_known_lifetime_is_replaced_before_it_expires() {
        let refresher = Arc::new(Counting { calls: AtomicU64::new(0), fail: false });
        let handle = SourceHandle::new(
            SourceKey::new("s", 0),
            "http://x.test/f?X-Amz-Expires=1",
            Arc::clone(&refresher) as Arc<dyn LinkRefresher>,
            RefreshPolicy::default(),
            RefreshBudget::new(20),
        );
        assert!(handle.current().expires_at.is_some(), "the lifetime in the url was not read");

        assert_eq!(handle.current_or_proactive().await.generation, 1, "refreshed far too early");
        tokio::time::sleep(Duration::from_millis(850)).await;
        assert_eq!(handle.current_or_proactive().await.generation, 2);
        assert_eq!(refresher.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn backoff_grows_and_is_bounded() {
        let policy = RefreshPolicy {
            base_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(2),
            ..Default::default()
        };
        for failures in 1..12 {
            let delay = backoff(&policy, failures);
            assert!(delay <= policy.max_backoff, "{failures} failures gave {delay:?}");
            assert!(delay >= policy.base_backoff / 2, "{failures} failures gave {delay:?}");
        }
        // Jitter must actually vary, or lanes that failed together keep
        // colliding on every retry.
        let delays: std::collections::BTreeSet<_> =
            (0..8).map(|_| backoff(&policy, 4).as_nanos()).collect();
        assert!(delays.len() > 1, "the backoff is not jittered");
    }
}
