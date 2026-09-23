//! Bandwidth limits applied to real transfers.
//!
//! The unit tests prove the token bucket's arithmetic; these prove the limit
//! actually reaches the wire, which is a different claim. Rates are chosen so
//! the expected duration is several seconds: a limiter that is off by a factor
//! of two has to show up, while ordinary scheduling jitter does not.

use dl_core::budget::Budget;
use dl_core::{ResumeOptions, download_resumable};
use dl_net::{HttpConfig, HttpSource};
use dl_testkit::{Origin, Scenario, fixtures, scenario::SEED};
use std::sync::Arc;
use std::time::Instant;

async fn fixture(size: u64) -> (Origin, tempfile::TempDir, std::path::PathBuf, HttpSource) {
    let origin = Origin::spawn(Scenario::Ok200 { size }).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("payload.bin");
    let source =
        HttpSource::with_config(&HttpConfig::default(), origin.url("payload.bin")).unwrap();
    (origin, dir, dest, source)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_download_limit_is_enforced_on_the_wire() {
    let size = 4 << 20;
    let rate = 1 << 20;
    let (_origin, _dir, dest, source) = fixture(size).await;

    let started = Instant::now();
    let outcome = download_resumable(
        &source,
        &dest,
        ResumeOptions {
            connections: 4,
            chunk_size: Some(256 << 10),
            limit: Some(Budget::with_rate(rate)),
            ..Default::default()
        },
        None,
    )
    .await
    .expect("a limited download should still succeed");
    let elapsed = started.elapsed();

    assert_eq!(blake3::hash(&std::fs::read(&dest).unwrap()), fixtures::digest(SEED, size));
    assert_eq!(outcome.total, size);

    // 4 MiB at 1 MiB/s is about four seconds, less one burst of allowance.
    let measured = size as f64 / elapsed.as_secs_f64();
    assert!(
        elapsed.as_secs_f64() > 2.5,
        "finished in {elapsed:?} at {measured:.0} B/s, which exceeds the {rate} B/s limit"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_limit_holds_no_matter_how_many_connections_are_used() {
    // The cap is on the download, so opening more connections must not buy more
    // bandwidth. Getting this wrong would make the limit per-connection.
    let size = 3 << 20;
    let rate = 1 << 20;

    let mut timings = Vec::new();
    for connections in [1, 8] {
        let (_origin, _dir, dest, source) = fixture(size).await;
        let started = Instant::now();
        download_resumable(
            &source,
            &dest,
            ResumeOptions {
                connections,
                chunk_size: Some(128 << 10),
                limit: Some(Budget::with_rate(rate)),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
        timings.push((connections, started.elapsed().as_secs_f64()));
    }

    // What a cap promises is a ceiling, so a ceiling is what is asserted. The
    // bucket starts full with one second of allowance, so no run may deliver
    // more than `rate * (1 + elapsed)` bytes however many connections it
    // opened. A per-connection limit fails this outright: eight connections
    // would move the file in a fraction of a second and be over the line.
    //
    // Deliberately not "both runs take about the same time". Below the cap the
    // wall clock belongs to the platform, not to the budget: one connection
    // pays for every chunk request and every stall in series where eight
    // overlap them, so the single-connection run can finish well under its own
    // ceiling. Slower than asked for is something a ceiling permits, and
    // asserting otherwise measures the host rather than the limiter.
    for (connections, elapsed) in &timings {
        let allowed = rate as f64 * (1.0 + elapsed);
        assert!(
            size as f64 <= allowed * 1.05,
            "{connections} connections moved {size} bytes in {elapsed:.2}s, \
             past the {allowed:.0} the cap allows: {timings:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unlimited_budget_does_not_slow_the_transfer() {
    let size = 4 << 20;
    let (_origin, _dir, dest, source) = fixture(size).await;

    let started = Instant::now();
    download_resumable(
        &source,
        &dest,
        ResumeOptions {
            connections: 4,
            chunk_size: Some(256 << 10),
            limit: Some(Budget::unlimited()),
            ..Default::default()
        },
        None,
    )
    .await
    .unwrap();

    // Against a loopback origin this is far below a second; the assertion only
    // needs to catch a limiter that throttles when it was told not to.
    assert!(started.elapsed().as_secs_f64() < 2.0, "an unlimited budget throttled the transfer");
    assert_eq!(blake3::hash(&std::fs::read(&dest).unwrap()), fixtures::digest(SEED, size));
}

#[tokio::test(flavor = "multi_thread")]
async fn raising_the_limit_mid_download_takes_effect() {
    // This is how a time window opening is applied: the budget is mutated
    // while transfers are in flight rather than restarting them.
    let size = 6 << 20;
    let (_origin, _dir, dest, source) = fixture(size).await;
    let budget = Budget::with_rate(512 << 10);

    let raiser = {
        let budget = Arc::clone(&budget);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(600)).await;
            budget.set_rate(64 << 20);
        })
    };

    let started = Instant::now();
    download_resumable(
        &source,
        &dest,
        ResumeOptions {
            connections: 4,
            chunk_size: Some(256 << 10),
            limit: Some(Arc::clone(&budget)),
            ..Default::default()
        },
        None,
    )
    .await
    .unwrap();
    raiser.await.unwrap();

    // At the original rate this would take twelve seconds.
    assert!(
        started.elapsed().as_secs_f64() < 8.0,
        "raising the limit did not take effect: took {:?}",
        started.elapsed()
    );
    assert_eq!(blake3::hash(&std::fs::read(&dest).unwrap()), fixtures::digest(SEED, size));
}
