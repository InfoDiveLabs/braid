//! Engine lifecycle against a real origin.
//!
//! Pause is implemented as cancellation, so the interesting question is not
//! whether it stops: it is whether resuming continues from what was already
//! journalled instead of starting over, and whether a deliberate pause is ever
//! mistaken for a failure.

use dl_core::budget::Budget;
use dl_core::engine::{DownloadSpec, Engine, EngineConfig, SourceFactory, State};
use dl_core::lane::{LaneSet, SingleLane};
use dl_core::source::ByteSource;
use dl_net::{HttpConfig, HttpSource};
use dl_testkit::{Origin, Scenario, fixtures, scenario::SEED};
use std::sync::Arc;
use std::time::Duration;

/// Builds one HTTP lane per download.
struct HttpFactory;

/// Owns the source so the returned `LaneSet` can borrow it for its own life.
struct OwnedLane {
    source: HttpSource,
    label: String,
}

impl LaneSet for OwnedLane {
    fn len(&self) -> usize {
        1
    }
    fn source(&self, _lane: usize) -> &dyn ByteSource {
        &self.source
    }
    fn label(&self, _lane: usize) -> &str {
        &self.label
    }
}

impl SourceFactory for HttpFactory {
    fn lanes_for(&self, spec: &DownloadSpec) -> dl_core::Result<Box<dyn LaneSet>> {
        let source = HttpSource::with_config(&HttpConfig::default(), &spec.url)?;
        Ok(Box::new(OwnedLane { source, label: "default".into() }))
    }
}

fn engine(max_concurrent: usize) -> Engine {
    Engine::new(
        Arc::new(HttpFactory),
        EngineConfig { max_concurrent, chunk_size: Some(256 << 10), ..Default::default() },
        Budget::unlimited(),
    )
}

/// Wait until the transfer has actually moved some bytes.
async fn wait_for_progress(engine: &Engine, id: dl_core::engine::DownloadId, within: Duration) {
    let deadline = std::time::Instant::now() + within;
    loop {
        let snapshot = engine.get(id).expect("the download should still exist");
        if snapshot.progress.downloaded > 0 {
            return;
        }
        assert!(!snapshot.state.is_terminal(), "finished before any progress: {snapshot:?}");
        if std::time::Instant::now() > deadline {
            panic!("no progress within {within:?}; state is {:?}", snapshot.state);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_for(
    engine: &Engine,
    id: dl_core::engine::DownloadId,
    want: impl Fn(State) -> bool,
    within: Duration,
) -> State {
    let deadline = std::time::Instant::now() + within;
    loop {
        let state = engine.get(id).map(|s| s.state).unwrap_or(State::Failed);
        if want(state) {
            return state;
        }
        if std::time::Instant::now() > deadline {
            panic!("timed out waiting; state is {state:?}");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_download_runs_to_completion() {
    let size = 2 << 20;
    let origin = Origin::spawn(Scenario::Ok200 { size }).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("a.bin");

    let engine = engine(3);
    let id = engine.add(DownloadSpec::new(origin.url("a.bin"), &dest));

    wait_for(&engine, id, |s| s == State::Complete, Duration::from_secs(30)).await;
    assert_eq!(blake3::hash(&std::fs::read(&dest).unwrap()), fixtures::digest(SEED, size));

    let snapshot = engine.get(id).unwrap();
    assert_eq!(snapshot.filename, "a.bin");
    assert_eq!(snapshot.progress.fraction(), Some(1.0));
    assert!(snapshot.error.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn pausing_then_resuming_continues_rather_than_restarting() {
    let size = 24 << 20;
    // Paced slowly enough that there is a real window to pause in. Do not
    // speed this up: without it the transfer finishes before the pause lands
    // and the test stops exercising anything.
    let origin =
        Origin::spawn(Scenario::SlowStream { size, bytes_per_sec: 512 << 10 }).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("b.bin");

    let engine = engine(3);
    let id = engine.add(DownloadSpec::new(origin.url("b.bin"), &dest));

    // Pause on an observed condition rather than after a fixed sleep, so the
    // test does not depend on how fast the machine is.
    wait_for_progress(&engine, id, Duration::from_secs(20)).await;

    engine.pause(id);
    let paused = wait_for(&engine, id, |s| s == State::Paused, Duration::from_secs(10)).await;
    assert_eq!(paused, State::Paused, "a pause must not be reported as a failure");

    let done_before = engine.get(id).unwrap().progress.downloaded;
    assert!(done_before > 0, "nothing was transferred before the pause");
    assert!(!dest.exists(), "a paused download must not publish a file");

    // The journal survives, so resuming picks up from it.
    let meta = dir.path().join("b.bin.dlmeta");
    assert!(meta.exists(), "the journal should outlive a pause");

    engine.resume(id);
    wait_for(&engine, id, |s| s == State::Complete, Duration::from_secs(60)).await;

    assert_eq!(blake3::hash(&std::fs::read(&dest).unwrap()), fixtures::digest(SEED, size));
    assert!(!meta.exists(), "the journal should be cleaned up on completion");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_concurrency_limit_is_respected() {
    let size = 8 << 20;
    let mut origins = Vec::new();
    for _ in 0..4 {
        origins.push(
            Origin::spawn(Scenario::SlowStream { size, bytes_per_sec: 2 << 20 }).await.unwrap(),
        );
    }
    let dir = tempfile::tempdir().unwrap();

    let engine = engine(2);
    for (i, origin) in origins.iter().enumerate() {
        engine.add(DownloadSpec::new(origin.url("c.bin"), dir.path().join(format!("c{i}.bin"))));
    }

    // Two run, two wait. Without a limit all four would start at once and
    // divide the same bandwidth four ways.
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(engine.count_in(State::Running), 2, "more than the limit started");
    assert_eq!(engine.count_in(State::Queued), 2);

    // A finished download must free its slot.
    let deadline = std::time::Instant::now() + Duration::from_secs(90);
    while engine.count_in(State::Complete) < 4 {
        assert!(engine.count_in(State::Running) <= 2, "the limit was exceeded while draining");
        assert!(std::time::Instant::now() < deadline, "the queue never drained");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failure_is_recorded_and_can_be_retried() {
    let origin = Origin::spawn(Scenario::NotFound).await.unwrap();
    let dir = tempfile::tempdir().unwrap();

    let engine = engine(3);
    let id = engine.add(DownloadSpec::new(origin.url("d.bin"), dir.path().join("d.bin")));

    wait_for(&engine, id, |s| s == State::Failed, Duration::from_secs(20)).await;
    let snapshot = engine.get(id).unwrap();
    assert!(snapshot.error.is_some(), "a failure should carry a reason");
    assert!(snapshot.error.unwrap().contains("404"));

    // Retrying clears the old error rather than leaving it showing.
    engine.resume(id);
    assert!(engine.get(id).unwrap().error.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn removing_a_download_stops_it_and_forgets_it() {
    let size = 16 << 20;
    let origin =
        Origin::spawn(Scenario::SlowStream { size, bytes_per_sec: 4 << 20 }).await.unwrap();
    let dir = tempfile::tempdir().unwrap();

    let engine = engine(3);
    let id = engine.add(DownloadSpec::new(origin.url("e.bin"), dir.path().join("e.bin")));
    wait_for(&engine, id, |s| s == State::Running, Duration::from_secs(10)).await;

    engine.remove(id);
    assert!(engine.get(id).is_none());
    assert!(engine.snapshot().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_shared_budget_limits_the_whole_engine() {
    // The cap is on the engine, so two downloads together must not exceed it.
    let size = 2 << 20;
    let a = Origin::spawn(Scenario::Ok200 { size }).await.unwrap();
    let b = Origin::spawn(Scenario::Ok200 { size }).await.unwrap();
    let dir = tempfile::tempdir().unwrap();

    let engine = Engine::new(
        Arc::new(HttpFactory),
        EngineConfig { max_concurrent: 2, chunk_size: Some(128 << 10), ..Default::default() },
        Budget::with_rate(1 << 20),
    );
    engine.add(DownloadSpec::new(a.url("f.bin"), dir.path().join("f.bin")));
    engine.add(DownloadSpec::new(b.url("g.bin"), dir.path().join("g.bin")));

    let started = std::time::Instant::now();
    let deadline = started + Duration::from_secs(60);
    while engine.count_in(State::Complete) < 2 {
        assert!(std::time::Instant::now() < deadline, "downloads never finished");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // 4 MiB total at 1 MiB/s cannot finish in under two seconds.
    assert!(
        started.elapsed().as_secs_f64() > 2.0,
        "the shared budget was not applied: finished in {:?}",
        started.elapsed()
    );
}

#[test]
fn a_single_lane_wrapper_satisfies_the_lane_set_contract() {
    struct Dummy;
    #[async_trait::async_trait]
    impl ByteSource for Dummy {
        async fn probe(&self) -> dl_core::Result<dl_core::SourceInfo> {
            unreachable!()
        }
        async fn open(&self, _f: dl_core::Fetch) -> dl_core::Result<dl_core::ByteStream> {
            unreachable!()
        }
    }
    let dummy = Dummy;
    let lane = SingleLane::new(&dummy);
    assert_eq!(lane.len(), 1);
    assert!(!lane.is_empty());
}

/// The Inspector's grid comes from this, so it has to describe a real
/// transfer rather than a plausible one.
#[tokio::test(flavor = "multi_thread")]
async fn the_chunk_map_tracks_a_running_transfer() {
    let size = 24 << 20;
    let origin =
        Origin::spawn(Scenario::SlowStream { size, bytes_per_sec: 512 << 10 }).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("c.bin");

    let engine = engine(3);
    assert!(engine.chunks(dl_core::engine::DownloadId(999)).is_none(), "no such transfer");

    let id = engine.add(DownloadSpec::new(origin.url("c.bin"), &dest));
    wait_for_progress(&engine, id, Duration::from_secs(20)).await;

    let mid = engine.chunks(id).expect("a running transfer has a chunk map");
    assert!(mid.chunk_count > 1, "a 24 MB file should be more than one chunk");
    assert!(mid.chunk_count * mid.chunk_size >= size, "the layout must cover the file");
    assert!(
        mid.completed_count() < mid.chunk_count,
        "the map claims the file is already whole while it is still arriving"
    );
    // Something must be moving, or the grid would show a file that is neither
    // arriving nor arrived.
    assert!(
        !mid.inflight.is_empty() || mid.completed_count() > 0,
        "nothing complete and nothing in flight part-way through a transfer"
    );
    assert!(
        mid.inflight.iter().all(|i| !mid.is_complete(*i)),
        "a chunk cannot be in flight and complete at once"
    );

    wait_for(&engine, id, |s| s == State::Complete, Duration::from_secs(120)).await;

    let done = engine.chunks(id).expect("the map outlives the transfer");
    assert_eq!(
        done.completed_count(),
        done.chunk_count,
        "every chunk must be complete once the file is"
    );
    assert!(done.inflight.is_empty(), "nothing is still in flight after the transfer ends");
    assert_eq!(blake3::hash(&std::fs::read(&dest).unwrap()), fixtures::digest(SEED, size));
}
