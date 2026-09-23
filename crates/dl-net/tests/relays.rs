//! Transfers through relays, end to end against real proxies.
//!
//! The question these answer is whether the lane abstraction really is enough:
//! if a relay is just another `ByteSource`, then chunk assignment, parking a
//! failed path and per-lane accounting all have to work with no changes at all.
//! Anywhere they do not is a place the design leaks.

use dl_core::lane::LaneSet;
use dl_core::{ResumeOptions, download_over_lanes};
use dl_net::{HttpConfig, Relay, RelayLanes};
use dl_testkit::{Origin, Scenario, fixtures};
use std::time::Duration;

const SEED: u64 = dl_testkit::scenario::SEED;

/// Relays pointed at one origin, as several phones on one network would be.
async fn relays(count: usize) -> Vec<dl_testkit::Relay> {
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        out.push(dl_testkit::Relay::spawn().await.expect("a relay starts"));
    }
    out
}

fn lanes(running: &[dl_testkit::Relay], url: &str) -> RelayLanes {
    let configured: Vec<Relay> = running
        .iter()
        .enumerate()
        .map(|(i, relay)| Relay::new(format!("phone{i}"), relay.url()))
        .collect();
    RelayLanes::new(&configured, url, &HttpConfig::default()).expect("lanes build")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_fetched_through_relays_is_the_file() {
    let size = 4 << 20;
    let origin = Origin::spawn(Scenario::Ok200 { size }).await.unwrap();
    let running = relays(3).await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("through-relays.bin");

    let outcome = download_over_lanes(
        &lanes(&running, &origin.url("f.bin")),
        &dest,
        ResumeOptions { connections: 6, ..Default::default() },
        None,
    )
    .await
    .expect("the transfer completes through the relays");

    assert_eq!(outcome.total, size);
    assert_eq!(blake3::hash(&std::fs::read(&dest).unwrap()), fixtures::digest(SEED, size));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_work_is_spread_across_the_relays() {
    // The whole point: three phones should each carry some of it. One relay
    // doing everything would mean the lane selector never saw them as separate
    // paths.
    let size = 8 << 20;
    let origin = Origin::spawn(Scenario::Ok200 { size }).await.unwrap();
    let running = relays(3).await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("spread.bin");

    let outcome = download_over_lanes(
        &lanes(&running, &origin.url("f.bin")),
        &dest,
        ResumeOptions { connections: 6, ..Default::default() },
        None,
    )
    .await
    .expect("the transfer completes");

    let used = running.iter().filter(|r| r.forwarded() > 0).count();
    assert!(used >= 2, "only {used} of 3 relays were used at all");
    assert_eq!(outcome.lanes.len(), 3, "every relay should be reported as a lane");
    assert_eq!(blake3::hash(&std::fs::read(&dest).unwrap()), fixtures::digest(SEED, size));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_relay_that_goes_away_costs_its_chunks_and_not_the_transfer() {
    // A phone loses signal, sleeps, or throttles itself when it gets hot. The
    // transfer has to continue on the others, which is exactly what the lane
    // selector does for a failed NIC: and the point of this test is that it
    // needed no new code to do it for a relay.
    let size = 8 << 20;
    let origin = Origin::spawn(Scenario::Ok200 { size }).await.unwrap();

    let healthy = dl_testkit::Relay::spawn().await.unwrap();
    // Answers a couple of requests and then stops, mid-transfer.
    let flaky = dl_testkit::Relay::spawn_with_limit(Some(2)).await.unwrap();

    let configured = [Relay::new("healthy", healthy.url()), Relay::new("flaky", flaky.url())];
    let set = RelayLanes::new(&configured, &origin.url("f.bin"), &HttpConfig::default()).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("survivor.bin");

    let outcome = download_over_lanes(
        &set,
        &dest,
        ResumeOptions { connections: 4, ..Default::default() },
        None,
    )
    .await
    .expect("the healthy relay should carry the transfer to the end");

    assert_eq!(blake3::hash(&std::fs::read(&dest).unwrap()), fixtures::digest(SEED, size));
    let parked = outcome.lanes.iter().filter(|l| l.parked).count();
    assert!(parked <= 1, "the healthy relay must not have been parked too");
}

#[tokio::test(flavor = "multi_thread")]
async fn every_relay_being_unreachable_fails_rather_than_hanging() {
    // Nothing on the other end of any of them: better a clear error than a
    // transfer that sits at zero for ever.
    let origin = Origin::spawn(Scenario::Ok200 { size: 1 << 20 }).await.unwrap();
    let configured = [Relay::new("gone", "127.0.0.1:9"), Relay::new("also gone", "127.0.0.1:10")];
    let set = RelayLanes::new(&configured, &origin.url("f.bin"), &HttpConfig::default()).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("never.bin");
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        download_over_lanes(&set, &dest, ResumeOptions::default(), None),
    )
    .await
    .expect("it must not hang");

    assert!(result.is_err(), "a transfer with no working relay must fail");
    assert!(!dest.exists(), "nothing should be published");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_relay_is_transparent_to_the_range_checks() {
    // The reason the relay speaks plain HTTP rather than a protocol of its
    // own: everything the engine checks about a response: that a 206 really
    // is a 206, that the encoding is identity: has to survive the hop.
    let size = 4 << 20;
    let origin = Origin::spawn(Scenario::AcceptRangesLies { size }).await.unwrap();
    let running = relays(2).await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("lying.bin");

    let result = download_over_lanes(
        &lanes(&running, &origin.url("f.bin")),
        &dest,
        ResumeOptions { connections: 4, ..Default::default() },
        None,
    )
    .await;

    assert!(result.is_err(), "an origin that lies about ranges must still be caught");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_relay_lane_reports_itself_by_name() {
    let size = 2 << 20;
    let origin = Origin::spawn(Scenario::Ok200 { size }).await.unwrap();
    let running = relays(2).await;
    let set = lanes(&running, &origin.url("f.bin"));
    assert_eq!(set.label(0), "phone0");

    let dir = tempfile::tempdir().unwrap();
    let outcome = download_over_lanes(
        &set,
        &dir.path().join("named.bin"),
        ResumeOptions { connections: 2, ..Default::default() },
        None,
    )
    .await
    .unwrap();

    // The sidebar meters and the graph bands are keyed on these.
    let names: Vec<&str> = outcome.lanes.iter().map(|l| l.label.as_str()).collect();
    assert!(names.contains(&"phone0") && names.contains(&"phone1"), "{names:?}");
}
