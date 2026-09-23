//! End-to-end downloads against a deliberately misbehaving origin.
//!
//! Every scenario asserts one of two things: a scenario that can succeed
//! produces a file whose BLAKE3 matches exactly, and one that cannot produces
//! no file at all: nothing at the destination and no `.part` left behind.
//! An error that still leaves a truncated file has corrupted the user's data.

use dl_core::{DownloadOptions, FileStorage, download};
use dl_net::{HttpConfig, HttpSource};
use dl_testkit::{Origin, Scenario};

struct Fixture {
    _origin: Origin,
    _dir: tempfile::TempDir,
    dest: std::path::PathBuf,
    source: HttpSource,
}

async fn fixture(scenario: Scenario) -> Fixture {
    let origin = Origin::spawn(scenario).await.expect("starting the mock origin");
    let dir = tempfile::tempdir().expect("temp dir");
    let dest = dir.path().join("payload.bin");
    let source = HttpSource::with_config(&HttpConfig::default(), origin.url("payload.bin"))
        .expect("building the http source");
    Fixture { _origin: origin, _dir: dir, dest, source }
}

/// Neither the destination nor a leftover `.part` may exist.
fn assert_nothing_on_disk(dest: &std::path::Path) {
    assert!(!dest.exists(), "a failed download left a file at {}", dest.display());
    let part = dest.with_extension("bin.part");
    assert!(!part.exists(), "a failed download left {} behind", part.display());
}

async fn run(scenario: Scenario) -> (Fixture, dl_core::Result<dl_core::Outcome>) {
    let f = fixture(scenario).await;
    let storage = FileStorage::create(&f.dest).await.expect("creating storage");
    let result = download(&f.source, &storage, DownloadOptions::default(), None).await;
    (f, result)
}

#[tokio::test]
async fn every_scenario_either_verifies_or_leaves_nothing() {
    for scenario in Scenario::catalogue() {
        // A signed origin can only ever refuse this client, so there is
        // nothing here for it to prove. `link_refresh.rs` drives these.
        if scenario.requires_refresh() {
            continue;
        }
        let (f, result) = run(scenario).await;

        match scenario.expected_digest() {
            Some(expected) => {
                let outcome = result.unwrap_or_else(|e| {
                    panic!("{scenario} should have succeeded, but failed: {e}")
                });
                assert_eq!(
                    outcome.blake3, expected,
                    "{scenario} produced a file with the wrong contents"
                );
                assert_eq!(outcome.bytes, scenario.expected_len().unwrap(), "{scenario} length");
                assert_eq!(
                    std::fs::metadata(&f.dest).unwrap().len(),
                    outcome.bytes,
                    "{scenario}: the file on disk does not match the reported byte count"
                );
            }
            None => {
                let err = result.err().unwrap_or_else(|| {
                    panic!("{scenario} should have failed, but reported success")
                });
                println!("{scenario} failed as required: {err}");
                assert_nothing_on_disk(&f.dest);
            }
        }
    }
}

#[tokio::test]
async fn a_short_body_is_an_error_not_a_truncated_file() {
    let scenario = Scenario::TruncatedBody { declared: 1 << 20, actual: 300 << 10 };
    let (f, result) = run(scenario).await;

    let err = result.expect_err("a body shorter than Content-Length must fail");
    // Either the client detects the short body itself or hyper reports the
    // broken stream first. What must never happen is success.
    assert!(
        matches!(err, dl_core::Error::ShortBody { .. } | dl_core::Error::Transport(_)),
        "unexpected error kind: {err:?}"
    );
    assert!(err.is_retryable(), "a truncated transfer is worth retrying");
    assert_nothing_on_disk(&f.dest);
}

#[tokio::test]
async fn a_compressed_response_is_refused_before_any_bytes_are_written() {
    // The guard against the worst silent-corruption bug in the design: with a
    // content coding applied, ranges address compressed bytes while we write
    // decompressed ones.
    let (f, result) = run(Scenario::ClaimsGzip { size: 64 << 10 }).await;

    let err = result.expect_err("a gzip-labelled body must be refused");
    assert!(
        matches!(err, dl_core::Error::UnexpectedContentEncoding(ref e) if e == "gzip"),
        "unexpected error kind: {err:?}"
    );
    assert!(!err.is_retryable(), "retrying cannot fix a content coding");
    assert_nothing_on_disk(&f.dest);
}

#[tokio::test]
async fn a_404_does_not_write_the_error_page_to_disk() {
    let (f, result) = run(Scenario::NotFound).await;

    match result.expect_err("404 must fail") {
        dl_core::Error::Http { status } => assert_eq!(status, 404),
        other => panic!("unexpected error kind: {other:?}"),
    }
    assert_nothing_on_disk(&f.dest);
}

#[tokio::test]
async fn redirects_are_followed_and_the_final_url_is_recorded() {
    let scenario = Scenario::RedirectChain { hops: 3, size: 64 << 10 };
    let (_f, result) = run(scenario).await;

    let outcome = result.expect("a redirect chain should resolve");
    assert_eq!(outcome.blake3, scenario.expected_digest().unwrap());
    // Later phases re-request the resolved URL, not the one the user typed, so
    // losing this would break link refresh and resume.
    assert!(
        outcome.info.final_url.contains("/file/"),
        "final_url should be the resolved target, got {}",
        outcome.info.final_url
    );
}

#[tokio::test]
async fn an_integrity_mismatch_discards_the_file() {
    let f = fixture(Scenario::Ok200 { size: 256 << 10 }).await;
    let storage = FileStorage::create(&f.dest).await.unwrap();

    let options = DownloadOptions {
        expect: Some(blake3::hash(b"a digest this payload certainly does not have").into()),
        ..Default::default()
    };
    let err = download(&f.source, &storage, options, None).await.expect_err("digest must mismatch");

    assert!(matches!(err, dl_core::Error::IntegrityMismatch { .. }), "got {err:?}");
    // The bytes transferred fine; they just are not what was asked for. Keeping
    // them would defeat the point of asking.
    assert_nothing_on_disk(&f.dest);
}

#[tokio::test]
async fn a_matching_digest_is_accepted() {
    let scenario = Scenario::Ok200 { size: 256 << 10 };
    let f = fixture(scenario).await;
    let storage = FileStorage::create(&f.dest).await.unwrap();

    let options = DownloadOptions {
        expect: scenario.expected_digest().map(Into::into),
        ..Default::default()
    };
    let outcome = download(&f.source, &storage, options, None).await.expect("digest should match");
    assert_eq!(outcome.path, f.dest);
}

#[tokio::test]
async fn progress_is_reported_and_ends_at_the_total() {
    let scenario = Scenario::SlowStream { size: 512 << 10, bytes_per_sec: 4 << 20 };
    let f = fixture(scenario).await;
    let storage = FileStorage::create(&f.dest).await.unwrap();

    let reports = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = std::sync::Arc::clone(&reports);
    let outcome = download(
        &f.source,
        &storage,
        DownloadOptions {
            progress_interval: std::time::Duration::from_millis(10),
            ..Default::default()
        },
        Some(Box::new(move |p| sink.lock().unwrap().push(p))),
    )
    .await
    .expect("slow stream should complete");

    let reports = reports.lock().unwrap();
    assert!(!reports.is_empty(), "progress was never reported");

    // Monotonic: a progress bar that goes backwards is a bug users notice.
    for pair in reports.windows(2) {
        assert!(
            pair[1].downloaded >= pair[0].downloaded,
            "progress went backwards: {} then {}",
            pair[0].downloaded,
            pair[1].downloaded
        );
    }

    let last = reports.last().unwrap();
    assert_eq!(last.downloaded, outcome.bytes);
    assert_eq!(last.fraction(), Some(1.0), "the final report must read as complete");
}

#[tokio::test]
async fn probing_reports_range_support_and_length() {
    let f = fixture(Scenario::Ok200 { size: 1 << 20 }).await;
    let info = dl_core::ByteSource::probe(&f.source).await.expect("probe");

    // Proven by a real 206 to a one-byte request, not by trusting Accept-Ranges.
    assert!(info.accept_ranges);
    assert_eq!(info.len, Some(1 << 20));
    assert!(info.supports_chunking(), "phase 3 will need this to be true here");
    assert!(!info.has_content_encoding());
}
