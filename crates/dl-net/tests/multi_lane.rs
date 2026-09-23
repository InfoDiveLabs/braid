//! Downloads spread across several lanes.
//!
//! These use loopback as the "interface" for every lane, which exercises the
//! whole path: per-lane clients, lane selection, throughput measurement,
//! failover: without needing two NICs. What it deliberately does *not* prove
//! is that traffic leaves by different physical interfaces; that needs
//! synthetic interfaces and root, and lives behind the `privileged-tests`
//! feature.

use dl_core::lane::{LaneSelector, LaneSet};
use dl_core::source::ByteSource;
use dl_core::{ResumeOptions, download_over_lanes};
use dl_net::{HttpConfig, HttpSource};
use dl_testkit::{Origin, Scenario, fixtures, scenario::SEED};

/// Several independent clients pointed at the same URL.
struct TestLanes {
    sources: Vec<HttpSource>,
    labels: Vec<String>,
}

impl TestLanes {
    fn new(url: &str, count: usize) -> Self {
        let sources = (0..count)
            .map(|_| HttpSource::with_config(&HttpConfig::default(), url).unwrap())
            .collect();
        let labels = (0..count).map(|i| format!("lane{i}")).collect();
        Self { sources, labels }
    }
}

impl LaneSet for TestLanes {
    fn len(&self) -> usize {
        self.sources.len()
    }
    fn source(&self, lane: usize) -> &dyn ByteSource {
        &self.sources[lane]
    }
    fn label(&self, lane: usize) -> &str {
        &self.labels[lane]
    }
}

/// A lane that always fails, to test failover without unplugging anything.
struct DeadLane;

#[async_trait::async_trait]
impl ByteSource for DeadLane {
    async fn probe(&self) -> dl_core::Result<dl_core::SourceInfo> {
        Err(dl_core::Error::Transport("this lane is down".into()))
    }
    async fn open(&self, _fetch: dl_core::Fetch) -> dl_core::Result<dl_core::ByteStream> {
        Err(dl_core::Error::Transport("this lane is down".into()))
    }
}

struct MixedLanes {
    live: HttpSource,
    dead: DeadLane,
    labels: Vec<String>,
}

impl LaneSet for MixedLanes {
    fn len(&self) -> usize {
        2
    }
    fn source(&self, lane: usize) -> &dyn ByteSource {
        if lane == 0 { &self.live } else { &self.dead }
    }
    fn label(&self, lane: usize) -> &str {
        &self.labels[lane]
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn work_is_spread_across_every_lane() {
    let size = 8 << 20;
    let origin = Origin::spawn(Scenario::Ok200 { size }).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("payload.bin");
    let lanes = TestLanes::new(&origin.url("payload.bin"), 3);

    let outcome = download_over_lanes(
        &lanes,
        &dest,
        ResumeOptions { connections: 6, chunk_size: Some(256 << 10), ..Default::default() },
        None,
    )
    .await
    .expect("multi-lane download should succeed");

    assert_eq!(blake3::hash(&std::fs::read(&dest).unwrap()), fixtures::digest(SEED, size));

    // Every lane should have carried some of the file; a lane that never gets
    // used contributes no bandwidth, which is the whole point of having it.
    assert_eq!(outcome.lanes.len(), 3);
    for report in &outcome.lanes {
        assert!(report.chunks > 0, "{} carried nothing: {:?}", report.label, outcome.lanes);
    }
    let carried: u64 = outcome.lanes.iter().map(|l| l.bytes).sum();
    assert_eq!(carried, size, "lane byte counts should account for the whole file");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dead_lane_is_parked_and_the_download_still_finishes() {
    let size = 4 << 20;
    let origin = Origin::spawn(Scenario::Ok200 { size }).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("payload.bin");

    let lanes = MixedLanes {
        live: HttpSource::with_config(&HttpConfig::default(), origin.url("payload.bin")).unwrap(),
        dead: DeadLane,
        labels: vec!["live".into(), "dead".into()],
    };

    let outcome = download_over_lanes(
        &lanes,
        &dest,
        ResumeOptions { connections: 4, chunk_size: Some(256 << 10), ..Default::default() },
        None,
    )
    .await
    .expect("losing one interface must not fail the download");

    assert_eq!(blake3::hash(&std::fs::read(&dest).unwrap()), fixtures::digest(SEED, size));

    let dead = outcome.lanes.iter().find(|l| l.label == "dead").unwrap();
    let live = outcome.lanes.iter().find(|l| l.label == "live").unwrap();
    assert!(dead.parked, "the failing lane should have been taken out of rotation");
    assert_eq!(dead.bytes, 0);
    assert_eq!(live.bytes, size, "the surviving lane should have carried everything");
}

/// Found in real use: `--all-interfaces` picked up VPN tunnel devices, and the
/// probe was pinned to lane 0. A tunnel that routes nowhere therefore timed out
/// and killed the whole download, even though a working interface was present.
#[tokio::test(flavor = "multi_thread")]
async fn a_dead_first_lane_does_not_stop_the_probe() {
    let size = 2 << 20;
    let origin = Origin::spawn(Scenario::Ok200 { size }).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("payload.bin");

    struct DeadFirst {
        dead: DeadLane,
        live: HttpSource,
        labels: Vec<String>,
    }
    impl LaneSet for DeadFirst {
        fn len(&self) -> usize {
            2
        }
        fn source(&self, lane: usize) -> &dyn ByteSource {
            if lane == 0 { &self.dead } else { &self.live }
        }
        fn label(&self, lane: usize) -> &str {
            &self.labels[lane]
        }
    }

    let lanes = DeadFirst {
        dead: DeadLane,
        live: HttpSource::with_config(&HttpConfig::default(), origin.url("payload.bin")).unwrap(),
        labels: vec!["tunnel".into(), "en0".into()],
    };

    let outcome = download_over_lanes(
        &lanes,
        &dest,
        ResumeOptions { connections: 4, chunk_size: Some(128 << 10), ..Default::default() },
        None,
    )
    .await
    .expect("a dead first lane must not stop the download");

    assert_eq!(blake3::hash(&std::fs::read(&dest).unwrap()), fixtures::digest(SEED, size));
    let tunnel = outcome.lanes.iter().find(|l| l.label == "tunnel").unwrap();
    assert!(tunnel.parked, "the unreachable lane should have been parked at probe time");
    assert_eq!(tunnel.bytes, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_single_lane_still_works_through_the_lane_path() {
    let size = 2 << 20;
    let origin = Origin::spawn(Scenario::Ok200 { size }).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("payload.bin");
    let lanes = TestLanes::new(&origin.url("payload.bin"), 1);

    let outcome = download_over_lanes(
        &lanes,
        &dest,
        ResumeOptions { connections: 4, chunk_size: Some(128 << 10), ..Default::default() },
        None,
    )
    .await
    .expect("one lane should behave exactly like the single-source path");

    assert_eq!(blake3::hash(&std::fs::read(&dest).unwrap()), fixtures::digest(SEED, size));
    assert_eq!(outcome.lanes.len(), 1);
    assert_eq!(outcome.lanes[0].bytes, size);
}

#[tokio::test(flavor = "multi_thread")]
async fn throughput_is_measured_per_lane() {
    let size = 4 << 20;
    let origin = Origin::spawn(Scenario::Ok200 { size }).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("payload.bin");
    let lanes = TestLanes::new(&origin.url("payload.bin"), 2);

    let outcome = download_over_lanes(
        &lanes,
        &dest,
        ResumeOptions { connections: 4, chunk_size: Some(256 << 10), ..Default::default() },
        None,
    )
    .await
    .unwrap();

    for report in &outcome.lanes {
        let measured = report.throughput.unwrap_or(0.0);
        assert!(measured > 0.0, "{} reported no throughput", report.label);
    }
}

#[tokio::test]
async fn an_empty_lane_set_is_refused() {
    struct NoLanes;
    impl LaneSet for NoLanes {
        fn len(&self) -> usize {
            0
        }
        fn source(&self, _lane: usize) -> &dyn ByteSource {
            unreachable!("there are no lanes")
        }
        fn label(&self, _lane: usize) -> &str {
            unreachable!("there are no lanes")
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let err =
        download_over_lanes(&NoLanes, dir.path().join("x.bin"), ResumeOptions::default(), None)
            .await
            .expect_err("downloading with no network path must fail");
    assert!(err.to_string().contains("no usable network path"), "{err}");
}

#[test]
fn a_selector_with_no_lanes_reports_rather_than_blocking() {
    let selector = LaneSelector::new(vec![]);
    assert!(selector.is_empty());
    assert_eq!(selector.acquire(), None);
    assert!(!selector.all_parked(), "an empty set is not the same as a failed one");
}
