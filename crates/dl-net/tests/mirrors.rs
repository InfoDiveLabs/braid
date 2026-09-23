//! Several URLs for one resource.
//!
//! Mirrors reuse the lane machinery rather than adding a second one, so the
//! interesting questions are whether work actually spreads and whether a
//! mirror serving something else is caught before its bytes are spliced in.

use dl_core::refresh::{RefreshPolicy, StaticRefresher};
use dl_core::{ResumeOptions, download_over_lanes};
use dl_net::{HttpConfig, RefreshingLanes};
use dl_testkit::{Origin, Scenario, fixtures, scenario::SEED};
use std::sync::Arc;

const NAME: &str = "payload.bin";

fn lanes(urls: Vec<String>) -> RefreshingLanes {
    RefreshingLanes::mirrors(
        &urls,
        &HttpConfig::default(),
        RefreshPolicy::default(),
        Arc::new(StaticRefresher),
    )
    .expect("building mirror lanes")
}

#[tokio::test(flavor = "multi_thread")]
async fn work_spreads_across_every_mirror() {
    let size = 4 << 20;
    let a = Origin::spawn(Scenario::Ok200 { size }).await.unwrap();
    let b = Origin::spawn(Scenario::Ok200 { size }).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join(NAME);

    let outcome = download_over_lanes(
        &lanes(vec![a.url(NAME), b.url(NAME)]),
        &dest,
        ResumeOptions { connections: 6, chunk_size: Some(128 << 10), ..Default::default() },
        None,
    )
    .await
    .expect("two mirrors of one file should download");

    assert_eq!(blake3::hash(&std::fs::read(&dest).unwrap()), fixtures::digest(SEED, size));
    assert_eq!(outcome.lanes.len(), 2);
    for report in &outcome.lanes {
        assert!(report.chunks > 0, "{} carried nothing: {:?}", report.label, outcome.lanes);
    }
    assert!(a.requests() > 0 && b.requests() > 0, "one mirror was never used");
}

/// Same length, different bytes. Splicing the two together produces a file of
/// exactly the right size that is neither version, and no later check notices.
#[tokio::test(flavor = "multi_thread")]
async fn a_mirror_serving_different_content_is_rejected_rather_than_spliced_in() {
    let size = 2 << 20;
    let real = Origin::spawn(Scenario::Ok200 { size }).await.unwrap();
    let impostor = Origin::spawn(Scenario::MirrorDisagrees { size }).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join(NAME);

    let outcome = download_over_lanes(
        &lanes(vec![real.url(NAME), impostor.url(NAME)]),
        &dest,
        ResumeOptions { connections: 4, chunk_size: Some(128 << 10), ..Default::default() },
        None,
    )
    .await
    .expect("one bad mirror must not fail the download");

    assert_eq!(blake3::hash(&std::fs::read(&dest).unwrap()), fixtures::digest(SEED, size));

    let rejected = &outcome.lanes[1];
    assert!(rejected.parked, "a mirror serving other bytes was kept in rotation");
    assert_eq!(rejected.bytes, 0, "the impostor contributed bytes to the file");
    assert_eq!(outcome.lanes[0].bytes, size);
}

/// The first URL is the one the user asked for; the mirrors are the extras.
#[tokio::test(flavor = "multi_thread")]
async fn a_disagreeing_first_mirror_still_downloads_what_was_asked_for() {
    let size = 1 << 20;
    let asked_for = Origin::spawn(Scenario::MirrorDisagrees { size }).await.unwrap();
    let other = Origin::spawn(Scenario::Ok200 { size }).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join(NAME);

    let outcome = download_over_lanes(
        &lanes(vec![asked_for.url(NAME), other.url(NAME)]),
        &dest,
        ResumeOptions { connections: 4, chunk_size: Some(128 << 10), ..Default::default() },
        None,
    )
    .await
    .unwrap();

    let expected = Scenario::MirrorDisagrees { size };
    assert_eq!(
        blake3::hash(&std::fs::read(&dest).unwrap()),
        fixtures::digest(expected.payload_seed(), size),
        "the mirror's content was downloaded instead of the requested url's"
    );
    assert!(outcome.lanes[1].parked, "the disagreeing mirror was used");
}
