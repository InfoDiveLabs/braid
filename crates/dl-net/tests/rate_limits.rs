//! Rate limiting, end to end against an origin that actually refuses.
//!
//! The behaviour under test is not "the download eventually finishes": a
//! client that ignored every 429 and hammered until it got through would also
//! pass that. It is that we *wait*, that we wait as long as we were told, and
//! that being refused on one interface does not immediately move the same
//! request onto another one.

use dl_core::lane::LaneSet;
use dl_core::source::ByteSource;
use dl_core::{ResumeOptions, download_over_lanes};
use dl_net::{HttpConfig, HttpSource};
use dl_testkit::{Origin, Scenario, fixtures};
use std::time::Instant;

const SEED: u64 = dl_testkit::scenario::SEED;

/// Several lanes over loopback, standing in for several interfaces. They all
/// reach the same origin, which is the point: a rate limit is a property of the
/// origin, not of the path taken to it.
struct Lanes {
    sources: Vec<HttpSource>,
    labels: Vec<String>,
}

impl LaneSet for Lanes {
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

fn lanes(origin: &Origin, count: usize) -> Lanes {
    Lanes {
        sources: (0..count)
            .map(|_| HttpSource::with_config(&HttpConfig::default(), origin.url("f.bin")).unwrap())
            .collect(),
        labels: (0..count).map(|i| format!("lane{i}")).collect(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stated_retry_after_is_waited_out_rather_than_ignored() {
    let size = 2 << 20;
    // Two seconds, stated. A client that ignored it would finish in well under
    // that; one that honoured it cannot.
    let origin =
        Origin::spawn(Scenario::RateLimited429 { size, refusals: 1, retry_after_secs: Some(2) })
            .await
            .unwrap();

    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("f.bin");

    let began = Instant::now();
    let outcome = download_over_lanes(
        &lanes(&origin, 1),
        &dest,
        ResumeOptions { connections: 2, ..Default::default() },
        None,
    )
    .await
    .expect("the transfer should survive a rate limit");
    let elapsed = began.elapsed();

    assert_eq!(blake3::hash(&std::fs::read(&dest).unwrap()), fixtures::digest(SEED, size));
    assert_eq!(outcome.total, size);
    assert!(
        elapsed >= std::time::Duration::from_secs(2),
        "finished in {elapsed:?}, so the Retry-After was ignored"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_limit_without_a_retry_after_still_backs_off_and_recovers() {
    let size = 2 << 20;
    let origin =
        Origin::spawn(Scenario::RateLimited429 { size, refusals: 3, retry_after_secs: None })
            .await
            .unwrap();

    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("f.bin");

    download_over_lanes(
        &lanes(&origin, 1),
        &dest,
        ResumeOptions { connections: 2, ..Default::default() },
        None,
    )
    .await
    .expect("the transfer should recover once the origin relents");

    assert_eq!(blake3::hash(&std::fs::read(&dest).unwrap()), fixtures::digest(SEED, size));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_503_is_treated_as_a_limit_rather_than_a_dead_path() {
    let size = 1 << 20;
    let origin = Origin::spawn(Scenario::Unavailable503 { size, refusals: 2 }).await.unwrap();

    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("f.bin");

    download_over_lanes(
        &lanes(&origin, 1),
        &dest,
        ResumeOptions { connections: 1, ..Default::default() },
        None,
    )
    .await
    .expect("an overloaded origin is something to wait for, not to give up on");

    assert_eq!(blake3::hash(&std::fs::read(&dest).unwrap()), fixtures::digest(SEED, size));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_limit_does_not_spread_the_load_onto_every_other_interface() {
    // A 429 on one path must not be answered by immediately retrying the same
    // chunk on another, or a single rate limit becomes a rate limit on every
    // address we own. With a stated wait, a correct client waits.
    let size = 1 << 20;
    let origin =
        Origin::spawn(Scenario::RateLimited429 { size, refusals: 1, retry_after_secs: Some(1) })
            .await
            .unwrap();

    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("f.bin");

    download_over_lanes(
        &lanes(&origin, 4),
        &dest,
        ResumeOptions { connections: 4, ..Default::default() },
        None,
    )
    .await
    .expect("the transfer should complete");

    // One refusal, one probe, and the real work. The exact figure depends on
    // the chunk layout; what matters is that four lanes did not each burn a
    // request rediscovering the same limit.
    let requests = origin.requests();
    assert!(requests < 40, "{requests} requests for a 1 MB file suggests a retry storm");
    assert_eq!(blake3::hash(&std::fs::read(&dest).unwrap()), fixtures::digest(SEED, size));
}
