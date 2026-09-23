//! Parallel chunked downloads against an origin that mishandles ranges.
//!
//! Range bugs are the interesting class here because most of them produce
//! correctly-sized, correctly-labelled responses containing the wrong bytes.
//! A downloader that trusts `Content-Length` will assemble them into a file
//! that looks perfect and is not.

use dl_core::{ResumeOptions, download_resumable};
use dl_net::{HttpConfig, HttpSource};
use dl_testkit::{Origin, Scenario, fixtures, scenario::SEED};

struct Fixture {
    _origin: Origin,
    _dir: tempfile::TempDir,
    dest: std::path::PathBuf,
    source: HttpSource,
}

async fn fixture(scenario: Scenario) -> Fixture {
    let origin = Origin::spawn(scenario).await.expect("origin");
    let dir = tempfile::tempdir().expect("temp dir");
    let dest = dir.path().join("payload.bin");
    let source = HttpSource::with_config(&HttpConfig::default(), origin.url("payload.bin"))
        .expect("http source");
    Fixture { _origin: origin, _dir: dir, dest, source }
}

fn options(connections: usize) -> ResumeOptions {
    ResumeOptions { connections, chunk_size: Some(128 << 10), ..Default::default() }
}

fn assert_nothing_on_disk(dest: &std::path::Path) {
    for suffix in ["", ".part", ".dlmeta"] {
        let path = if suffix.is_empty() {
            dest.to_path_buf()
        } else {
            dest.with_file_name(format!("payload.bin{suffix}"))
        };
        assert!(!path.exists(), "a failed download left {}", path.display());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn parallel_chunks_assemble_in_the_right_order() {
    let size = 4 << 20;
    let f = fixture(Scenario::Ok200 { size }).await;

    let outcome = download_resumable(&f.source, &f.dest, options(8), None)
        .await
        .expect("parallel download should succeed");

    assert_eq!(outcome.total, size);
    assert_eq!(outcome.connections, 8);
    assert!(outcome.chunks > 8, "expected more chunks than connections, got {}", outcome.chunks);

    // Chunks complete out of order, so this is the real test of offset handling.
    let written = std::fs::read(&f.dest).unwrap();
    assert_eq!(written.len() as u64, size);
    assert_eq!(blake3::hash(&written), fixtures::digest(SEED, size));
}

#[tokio::test(flavor = "multi_thread")]
async fn every_connection_count_produces_identical_bytes() {
    let size = 2 << 20;
    let expected = fixtures::digest(SEED, size);

    for connections in [1, 2, 5, 16] {
        let f = fixture(Scenario::Ok200 { size }).await;
        download_resumable(&f.source, &f.dest, options(connections), None)
            .await
            .unwrap_or_else(|e| panic!("{connections} connections failed: {e}"));

        let written = std::fs::read(&f.dest).unwrap();
        assert_eq!(
            blake3::hash(&written),
            expected,
            "{connections} connections produced different bytes"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_origin_that_lies_about_range_support_is_caught() {
    // Advertises Accept-Ranges then answers 200 to every range request. The
    // probe proves range support with a real 206 rather than trusting the
    // header, so this is rejected before a single chunk is requested.
    let f = fixture(Scenario::AcceptRangesLies { size: 1 << 20 }).await;

    let err = download_resumable(&f.source, &f.dest, options(4), None)
        .await
        .expect_err("a lying origin must not produce a file");
    assert!(matches!(err, dl_core::Error::RangeNotHonoured { .. }), "{err:?}");
    assert!(!err.is_retryable(), "retrying gets the same wrong answer");
    assert_nothing_on_disk(&f.dest);

    // Range support is reported honestly, so the caller can fall back to a
    // single-stream download instead of guessing.
    let info = dl_core::ByteSource::probe(&f.source).await.unwrap();
    assert!(!info.accept_ranges);
    assert!(!info.supports_chunking());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_206_for_the_wrong_span_is_rejected() {
    let f = fixture(Scenario::ContentRangeMismatch { size: 1 << 20 }).await;

    let err = download_resumable(&f.source, &f.dest, options(4), None)
        .await
        .expect_err("a mismatched Content-Range must be rejected");
    assert!(
        matches!(err, dl_core::Error::RangeNotHonoured { ref detail } if detail.contains("asked for bytes")),
        "unexpected error: {err:?}"
    );
    assert_nothing_on_disk(&f.dest);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_right_length_of_the_wrong_bytes_is_still_caught() {
    // Every header is correct and every length matches. The only evidence that
    // anything is wrong is the content itself.
    let size = 1 << 20;
    let f = fixture(Scenario::WrongBytesForRange { size }).await;

    let outcome = download_resumable(
        &f.source,
        &f.dest,
        ResumeOptions { expect: Some(fixtures::digest(SEED, size).into()), ..options(4) },
        None,
    )
    .await;

    match outcome {
        Err(dl_core::Error::IntegrityMismatch { .. }) => {}
        Err(other) => panic!("unexpected error: {other:?}"),
        Ok(_) => panic!("wrong bytes were accepted as correct"),
    }
    assert_nothing_on_disk(&f.dest);
}

#[tokio::test(flavor = "multi_thread")]
async fn corrupted_bytes_are_caught_by_the_whole_file_digest() {
    let size = 1 << 20;
    let f = fixture(Scenario::CorruptBytesAt { size, offset: 700 << 10, len: 64 }).await;

    let err = download_resumable(
        &f.source,
        &f.dest,
        ResumeOptions { expect: Some(fixtures::digest(SEED, size).into()), ..options(4) },
        None,
    )
    .await
    .expect_err("64 corrupted bytes must not pass verification");

    assert!(matches!(err, dl_core::Error::IntegrityMismatch { .. }), "{err:?}");
    assert_nothing_on_disk(&f.dest);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_resource_that_changes_mid_download_is_detected() {
    // The ETag changes partway through. Continuing would splice bytes from two
    // versions of the file together, which no length check would notice.
    let f = fixture(Scenario::EtagChangesMidDownload { size: 4 << 20, after_requests: 3 }).await;

    let err = download_resumable(&f.source, &f.dest, options(4), None)
        .await
        .expect_err("a changed resource must abort the download");

    assert!(
        matches!(err, dl_core::Error::ResourceChanged { .. }),
        "expected ResourceChanged, got {err:?}"
    );
    assert!(!err.is_retryable(), "retrying cannot fix a changed resource");
    assert_nothing_on_disk(&f.dest);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_matching_digest_is_accepted_across_parallel_chunks() {
    let size = 2 << 20;
    let f = fixture(Scenario::Ok200 { size }).await;

    let outcome = download_resumable(
        &f.source,
        &f.dest,
        ResumeOptions { expect: Some(fixtures::digest(SEED, size).into()), ..options(6) },
        None,
    )
    .await
    .expect("digest should match");

    assert_eq!(outcome.verified, Some(fixtures::digest(SEED, size).into()));
    assert_eq!(outcome.path, f.dest);
}

#[tokio::test(flavor = "multi_thread")]
async fn progress_never_goes_backwards_with_chunks_completing_out_of_order() {
    let f = fixture(Scenario::SlowStream { size: 2 << 20, bytes_per_sec: 8 << 20 }).await;

    let reports = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = std::sync::Arc::clone(&reports);
    download_resumable(
        &f.source,
        &f.dest,
        ResumeOptions { progress_interval: std::time::Duration::from_millis(5), ..options(8) },
        Some(Box::new(move |p| sink.lock().unwrap().push(p))),
    )
    .await
    .expect("download should succeed");

    let reports = reports.lock().unwrap();
    assert!(!reports.is_empty(), "no progress was reported");
    for pair in reports.windows(2) {
        assert!(
            pair[1].downloaded >= pair[0].downloaded,
            "progress went backwards: {} then {}",
            pair[0].downloaded,
            pair[1].downloaded
        );
    }
    assert_eq!(reports.last().unwrap().fraction(), Some(1.0));
}

/// The journal exists so a dropped connection costs the bytes in flight, not
/// the ones already on disk. A retryable failure must therefore leave the
/// partial file and its journal intact.
#[tokio::test(flavor = "multi_thread")]
async fn a_transient_failure_keeps_the_partial_file_for_resume() {
    let size = 4 << 20;
    let f = fixture(Scenario::ResetAtOffset { size, reset_at: 3 << 20 }).await;

    let err = download_resumable(&f.source, &f.dest, options(2), None)
        .await
        .expect_err("a reset connection should fail this attempt");
    assert!(err.is_retryable(), "a dropped connection is worth retrying: {err:?}");

    assert!(!f.dest.exists(), "an unfinished download must not reach the destination");
    let part = f.dest.with_file_name("payload.bin.part");
    let meta = f.dest.with_file_name("payload.bin.dlmeta");
    assert!(part.exists(), "the partial file was discarded despite a retryable failure");
    assert!(meta.exists(), "the journal was discarded despite a retryable failure");
}

/// Sweep the whole catalogue through the chunked path, with verification on.
///
/// Per-scenario tests check specific error kinds; this checks the property that
/// has to hold for every one of them: either the bytes are exactly right, or
/// there is no file.
///
/// The expected digest is supplied deliberately. Two scenarios here: /// `wrong-bytes-for-range` and `corrupt-bytes-at`: return responses whose
/// status, length and `Content-Range` are all correct and whose bytes are not.
/// Nothing in the protocol reveals that, and per-chunk hashes cannot either,
/// since they hash whatever arrived. An end-to-end digest is the only defence,
/// which is why `--verify` matters for any source that is not trusted.
#[tokio::test(flavor = "multi_thread")]
async fn every_scenario_chunked_either_verifies_or_leaves_nothing() {
    for scenario in Scenario::catalogue() {
        if scenario.requires_refresh() {
            continue;
        }
        let f = fixture(scenario).await;
        let result = download_resumable(
            &f.source,
            &f.dest,
            ResumeOptions { expect: scenario.true_digest().map(Into::into), ..options(4) },
            None,
        )
        .await;

        if scenario.fails_chunked() {
            let err = result
                .err()
                .unwrap_or_else(|| panic!("{scenario} should have failed when chunked"));

            // A retryable failure deliberately keeps the partial file so the
            // next attempt can resume; only the destination must stay clear.
            assert!(!f.dest.exists(), "{scenario}: an unfinished download reached the destination");
            if !err.is_retryable() {
                assert_nothing_on_disk(&f.dest);
            }
            println!("{scenario} failed as required: {err}");
        } else {
            let outcome =
                result.unwrap_or_else(|e| panic!("{scenario} should have succeeded: {e}"));
            let written = std::fs::read(&f.dest).unwrap();
            assert_eq!(
                blake3::hash(&written),
                scenario.true_digest().unwrap(),
                "{scenario} produced the wrong bytes"
            );
            assert_eq!(outcome.total, written.len() as u64);
        }
    }
}
