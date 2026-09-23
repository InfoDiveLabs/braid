//! Downloads whose links expire underneath them.
//!
//! The property every test here defends is the same one: a link is a
//! credential with a deadline, and the file that comes out must be exactly the
//! file regardless of how many times that credential had to be replaced.

use dl_core::error::Error;
use dl_core::refresh::{ApiRefresher, LinkRefresher, RefreshPolicy, StaticRefresher};
use dl_core::{ResumeOptions, download_over_lanes};
use dl_net::{HttpConfig, LaneSpec, RefreshingLanes, ReqwestJson};
use dl_testkit::origin::CLIENT_HEADER;
use dl_testkit::{Origin, Scenario, fixtures};
use reqwest::Client;
use std::sync::Arc;

const NAME: &str = "payload.bin";

/// Resolves through the origin's own issuing endpoint, over this lane's
/// client: which is how a signature bound to the requesting address gets
/// re-issued to the address that will use it.
fn issuing_refresher(origin: &Origin, client: &Client) -> Arc<dyn LinkRefresher> {
    Arc::new(
        ApiRefresher::get(
            Arc::new(ReqwestJson::new(client.clone())),
            origin.issue_url(NAME),
            "url",
        )
        .with_headers_path("headers")
        .with_expires_path("expires_in"),
    )
}

fn options(connections: usize, chunk_size: u64) -> ResumeOptions {
    ResumeOptions { connections, chunk_size: Some(chunk_size), ..Default::default() }
}

fn assert_exact(dest: &std::path::Path, scenario: Scenario) {
    let written = std::fs::read(dest).expect("the download should have produced a file");
    assert_eq!(written.len() as u64, scenario.size().unwrap(), "wrong length for {scenario}");
    assert_eq!(
        blake3::hash(&written),
        fixtures::digest(scenario.payload_seed(), scenario.size().unwrap()),
        "{scenario} produced the wrong bytes"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_link_that_expires_mid_download_recovers_and_the_file_is_byte_exact() {
    let scenario = Scenario::ExpiresAfterBytes { size: 1 << 20, after_bytes: 128 << 10 };
    let origin = Origin::spawn(scenario).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join(NAME);

    let lanes = RefreshingLanes::build(
        vec![LaneSpec::new("origin", origin.url(NAME))],
        &HttpConfig::default(),
        RefreshPolicy::default(),
        Arc::new(StaticRefresher),
        &|_, client| Some(issuing_refresher(&origin, client)),
    )
    .unwrap();

    let outcome = download_over_lanes(&lanes, &dest, options(4, 64 << 10), None)
        .await
        .expect("an expiring link should not end the download");

    assert_exact(&dest, scenario);
    assert!(origin.rejections() > 0, "the link never actually expired");
    assert_eq!(
        outcome.transferred, outcome.total,
        "a refresh made the download re-fetch bytes it already had"
    );
}

/// A refresh replaces the credential, not the progress.
#[tokio::test(flavor = "multi_thread")]
async fn a_refresh_never_re_fetches_a_chunk_that_is_already_on_disk() {
    let scenario = Scenario::ExpiresAfterBytes { size: 1 << 20, after_bytes: 128 << 10 };
    let origin = Origin::spawn(scenario).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join(NAME);

    // A first attempt with nothing to refresh to, so it dies part-way and
    // leaves a journal behind.
    let stranded = RefreshingLanes::build(
        vec![LaneSpec {
            initial: origin.sign(NAME, None),
            ..LaneSpec::new("origin", origin.url(NAME))
        }],
        &HttpConfig::default(),
        RefreshPolicy { max_refreshes: 3, ..Default::default() },
        Arc::new(StaticRefresher),
        &|_, _| None,
    )
    .unwrap();
    let err = download_over_lanes(&stranded, &dest, options(4, 64 << 10), None)
        .await
        .expect_err("a link with no way to be refreshed must fail");
    assert!(matches!(err, Error::RefreshExhausted { .. }), "{err:?}");
    assert!(!dest.exists(), "an unfinished download reached the destination");
    assert!(
        dest.with_file_name("payload.bin.dlmeta").exists(),
        "an expired link discarded the chunks that were already safe"
    );

    let lanes = RefreshingLanes::build(
        vec![LaneSpec::new("origin", origin.url(NAME))],
        &HttpConfig::default(),
        RefreshPolicy::default(),
        Arc::new(StaticRefresher),
        &|_, client| Some(issuing_refresher(&origin, client)),
    )
    .unwrap();
    let outcome = download_over_lanes(&lanes, &dest, options(4, 64 << 10), None).await.unwrap();

    assert!(outcome.resumed_from > 0, "nothing was kept from the stranded attempt");
    assert_eq!(
        outcome.transferred,
        outcome.total - outcome.resumed_from,
        "chunks that were already durable were fetched again"
    );
    assert_exact(&dest, scenario);
}

/// The expiry that every status and length check passes.
#[tokio::test(flavor = "multi_thread")]
async fn a_login_page_served_with_200_never_reaches_the_file() {
    let scenario = Scenario::HtmlErrorBodyWith200 { size: 1 << 20, after_bytes: 128 << 10 };
    let origin = Origin::spawn(scenario).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join(NAME);

    // Started from a link that still works, because that is the only way the
    // engine can learn what the resource is. A download whose *first* response
    // is already a login page is indistinguishable from a download of a web
    // page, and the heuristic deliberately does not guess.
    let lanes = RefreshingLanes::build(
        vec![LaneSpec {
            initial: origin.sign(NAME, None),
            ..LaneSpec::new("origin", origin.url(NAME))
        }],
        &HttpConfig::default(),
        RefreshPolicy::default(),
        Arc::new(StaticRefresher),
        &|_, client| Some(issuing_refresher(&origin, client)),
    )
    .unwrap();

    download_over_lanes(&lanes, &dest, options(4, 64 << 10), None)
        .await
        .expect("a portal redirect is an expiry, not a fatal error");

    assert!(origin.rejections() > 0, "the login page was never served");
    let written = std::fs::read(&dest).unwrap();
    assert!(!written.windows(6).any(|w| w == b"<html>"), "a login page was written into the file");
    assert_exact(&dest, scenario);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_permanently_dead_link_fails_at_the_cap_instead_of_spinning() {
    let scenario = Scenario::Returns410Gone { size: 256 << 10 };
    let origin = Origin::spawn(scenario).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join(NAME);

    let policy = RefreshPolicy { max_refreshes: 5, ..Default::default() };
    let lanes = RefreshingLanes::build(
        vec![LaneSpec::new("origin", origin.url(NAME))],
        &HttpConfig::default(),
        policy,
        Arc::new(StaticRefresher),
        &|_, _| None,
    )
    .unwrap();

    let err = download_over_lanes(&lanes, &dest, options(4, 64 << 10), None)
        .await
        .expect_err("a withdrawn object must eventually fail");

    assert!(matches!(err, Error::RefreshExhausted { .. }), "{err:?}");
    assert!(err.to_string().contains("410"), "the origin's answer was lost: {err}");
    // One request per allowed refresh, plus the one that discovered the
    // problem. Anything more is a spin.
    assert_eq!(origin.requests(), policy.max_refreshes as u64 + 1);
    assert!(!dest.exists(), "a failed download reached the destination");
}

/// Refreshing early is what makes the common case zero errors rather than one
/// 403 per connection followed by recovery.
#[tokio::test(flavor = "multi_thread")]
async fn a_link_with_a_stated_lifetime_is_replaced_before_anything_is_refused() {
    let scenario = Scenario::ExpiresAfterSeconds { size: 2 << 20, lifetime_secs: 1 };
    let origin = Origin::spawn(scenario).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join(NAME);

    let lanes = RefreshingLanes::build(
        vec![LaneSpec {
            initial: origin.sign(NAME, None),
            ..LaneSpec::new("origin", origin.url(NAME))
        }],
        &HttpConfig::default(),
        RefreshPolicy::default(),
        Arc::new(StaticRefresher),
        &|_, client| Some(issuing_refresher(&origin, client)),
    )
    .unwrap();

    // Slow enough that the link expires several times during the transfer.
    let outcome = download_over_lanes(
        &lanes,
        &dest,
        ResumeOptions {
            limit: Some(dl_core::budget::Budget::with_rate(512 << 10)),
            ..options(4, 64 << 10)
        },
        None,
    )
    .await
    .expect("a link refreshed in time should never fail");

    assert!(outcome.elapsed.as_secs_f64() > 1.0, "the link never had time to expire");
    assert!(origin.issues() > 1, "the link was never renewed: {} issued", origin.issues());
    assert_eq!(origin.rejections(), 0, "a proactive refresh still let requests be refused");
    assert_exact(&dest, scenario);
}

/// A signature bound to the requesting address is a different credential on
/// every path, so each lane has to hold its own.
#[tokio::test(flavor = "multi_thread")]
async fn each_lane_resolves_its_own_address_bound_signature() {
    let scenario = Scenario::SignedPerSourceIp { size: 1 << 20 };
    let origin = Origin::spawn(scenario).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join(NAME);

    let specs: Vec<LaneSpec> = (0..4)
        .map(|i| {
            LaneSpec::new(format!("lane{i}"), origin.url(NAME))
                .with_headers(vec![(CLIENT_HEADER.to_string(), format!("203.0.113.{i}"))])
        })
        .collect();

    let lanes = RefreshingLanes::build(
        specs,
        &HttpConfig::default(),
        RefreshPolicy::default(),
        Arc::new(StaticRefresher),
        &|_, client| Some(issuing_refresher(&origin, client)),
    )
    .unwrap();

    let outcome = download_over_lanes(&lanes, &dest, options(16, 64 << 10), None)
        .await
        .expect("every lane should be able to resolve a signature of its own");

    assert_exact(&dest, scenario);
    assert_eq!(lanes.resolves(), 4, "lanes shared a signature they cannot share");
    assert_eq!(origin.issues(), 4);
    for report in &outcome.lanes {
        assert!(report.chunks > 0, "{} never worked: {:?}", report.label, outcome.lanes);
    }
}

/// Sixteen connections discover the same dead link within microseconds. One
/// refresh has to serve all of them; sixteen would invalidate each other.
#[tokio::test(flavor = "multi_thread")]
async fn a_burst_of_expired_chunks_costs_far_fewer_refreshes_than_chunks() {
    // The signature is spent by the probe, so every worker's first chunk
    // request is refused at once.
    let scenario = Scenario::ExpiresAfterBytes { size: 1 << 20, after_bytes: 1 };
    let origin = Origin::spawn(scenario).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join(NAME);

    let lanes = RefreshingLanes::build(
        vec![LaneSpec {
            initial: origin.sign(NAME, None),
            ..LaneSpec::new("origin", origin.url(NAME))
        }],
        &HttpConfig::default(),
        RefreshPolicy::default(),
        Arc::new(StaticRefresher),
        &|_, client| Some(issuing_refresher(&origin, client)),
    )
    .unwrap();

    download_over_lanes(&lanes, &dest, options(16, 64 << 10), None).await.unwrap();

    assert_exact(&dest, scenario);
    assert!(origin.rejections() >= 8, "the burst never happened: {}", origin.rejections());
    assert!(
        lanes.resolves() < origin.rejections(),
        "every refusal caused its own refresh: {} refreshes for {} refusals",
        lanes.resolves(),
        origin.rejections()
    );
}
