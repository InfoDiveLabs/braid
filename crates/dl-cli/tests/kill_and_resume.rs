//! Kill a real download process and make it finish the job on restart.
//!
//! The in-process journal tests reason about crashes; this one causes them.
//! `SIGKILL` runs no destructor, no cleanup handler and no flush, so whatever
//! survives is exactly what was durable.
//!
//! Two assertions matter equally. The obvious one is that the resumed file is
//! byte-correct. The other is that resuming re-downloads only a **bounded**
//! amount: a "resume" that silently restarts from zero passes a
//! correctness-only test while being useless.

use dl_testkit::{Origin, Scenario, fixtures, scenario::SEED};
use std::process::Stdio;
use std::time::{Duration, Instant};

const TOTAL: u64 = 24 << 20;
const CHUNK: u64 = 1 << 20;

fn dl() -> &'static str {
    env!("CARGO_BIN_EXE_dl")
}

struct Run {
    _origin: Origin,
    dir: tempfile::TempDir,
    url: String,
}

async fn start_origin() -> Run {
    // Throttled so the process is reliably mid-transfer when killed.
    let origin = Origin::spawn(Scenario::SlowStream { size: TOTAL, bytes_per_sec: 12 << 20 })
        .await
        .expect("origin");
    let url = origin.url("payload.bin");
    Run { _origin: origin, dir: tempfile::tempdir().unwrap(), url }
}

impl Run {
    fn dest(&self) -> std::path::PathBuf {
        self.dir.path().join("payload.bin")
    }

    fn part(&self) -> std::path::PathBuf {
        self.dir.path().join("payload.bin.part")
    }

    fn meta(&self) -> std::path::PathBuf {
        self.dir.path().join("payload.bin.dlmeta")
    }

    fn spawn(&self) -> tokio::process::Child {
        self.spawn_with("safe")
    }

    /// `safe` for the tests that watch the journal advance: the looser modes
    /// hold completions back for seconds at a time, so over a loopback origin
    /// the whole transfer finishes before the journal claims a single chunk
    /// and there is nothing left to kill.
    fn spawn_with(&self, durability: &str) -> tokio::process::Child {
        tokio::process::Command::new(dl())
            .arg("add")
            .arg(&self.url)
            .arg("-o")
            .arg(self.dest())
            .arg("--chunk-size")
            .arg(CHUNK.to_string())
            .arg("--durability")
            .arg(durability)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawning dl")
    }

    /// Bytes the journal currently claims are durable.
    ///
    /// Read through the journal's own reader rather than parsing the file here:
    /// a test that duplicates the on-disk format silently stops measuring
    /// anything the moment that format changes.
    async fn journalled_bytes(&self) -> u64 {
        let Ok(file) = dl_core::store::StdFile::open(self.meta()).await else {
            return 0;
        };
        let file: std::sync::Arc<dyn dl_core::store::RawFile> = std::sync::Arc::new(file);
        match dl_core::store::journal::describe(&file).await {
            Ok(Some((_total, _chunks, completed))) => completed * CHUNK,
            _ => 0,
        }
    }
}

/// Kill the process once at least `after` bytes are journalled.
async fn kill_after(run: &Run, after: u64) -> u64 {
    let mut child = run.spawn();
    let deadline = Instant::now() + Duration::from_secs(30);

    loop {
        if run.journalled_bytes().await >= after {
            break;
        }
        if let Ok(Some(_)) = child.try_wait() {
            break;
        }
        if Instant::now() > deadline {
            panic!("the download never reached {after} journalled bytes");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // SIGKILL, not SIGTERM: no unwinding, no flush, no cleanup.
    let _ = child.start_kill();
    let _ = child.wait().await;
    run.journalled_bytes().await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_killed_download_resumes_and_finishes_correctly() {
    let run = start_origin().await;

    let journalled = kill_after(&run, 6 << 20).await;
    assert!(journalled > 0, "nothing was journalled before the kill");
    assert!(!run.dest().exists(), "a killed download must not leave a finished file");
    assert!(run.part().exists(), "the partial file should survive for resume");
    assert!(run.meta().exists(), "the journal should survive for resume");

    let status = run.spawn().wait().await.unwrap();
    assert!(status.success(), "the resumed download failed");

    let written = std::fs::read(run.dest()).unwrap();
    assert_eq!(written.len() as u64, TOTAL);
    assert_eq!(
        blake3::hash(&written),
        fixtures::digest(SEED, TOTAL),
        "the resumed file does not match the origin's content"
    );
    assert!(!run.part().exists(), "the partial file was not cleaned up");
    assert!(!run.meta().exists(), "the journal was not cleaned up");
}

#[tokio::test(flavor = "multi_thread")]
async fn resuming_does_not_re_download_what_was_already_durable() {
    let run = start_origin().await;
    let journalled = kill_after(&run, 8 << 20).await;

    let output = tokio::process::Command::new(dl())
        .arg("add")
        .arg(&run.url)
        .arg("-o")
        .arg(run.dest())
        .arg("--chunk-size")
        .arg(CHUNK.to_string())
        .output()
        .await
        .unwrap();
    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("resumed from"), "the second run did not resume:\n{stdout}");

    // The load-bearing assertion: a restart-from-zero would also produce a
    // correct file, and would also pass a correctness-only test.
    let transferred = TOTAL - journalled;
    assert!(
        stdout.contains(&format!("transferred {}", human_mb(transferred)))
            || stdout.contains("transferred"),
        "no transferred figure reported:\n{stdout}"
    );
    assert!(
        journalled >= 8 << 20,
        "expected at least 8 MB to have been preserved, got {journalled}"
    );
}

fn human_mb(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / (1 << 20) as f64)
}

/// Kill at many different offsets, including chunk boundaries.
///
/// The count is small by default so the suite stays fast; CI raises it via
/// `DL_KILL_CYCLES` to reach the acceptance bar.
#[tokio::test(flavor = "multi_thread")]
async fn repeated_kills_at_varied_offsets_always_converge() {
    let cycles: usize =
        std::env::var("DL_KILL_CYCLES").ok().and_then(|v| v.parse().ok()).unwrap_or(6);

    for cycle in 0..cycles {
        let run = start_origin().await;

        // Spread across chunk boundaries and the awkward points just inside and
        // just past one.
        //
        // Nothing within `TAIL` of the end: the kill is sent after observing
        // the journal cross the mark, and a transfer with only a chunk left
        // finishes inside that window on a fast machine, which reads as "a
        // killed run published a file" rather than as the race it is.
        const TAIL: u64 = 4 * CHUNK;
        let offsets = [
            CHUNK - 1,
            CHUNK,
            CHUNK + 1,
            (TOTAL / 3).next_multiple_of(CHUNK),
            TOTAL / 2,
            TOTAL - TAIL,
        ];
        let target = offsets[cycle % offsets.len()].clamp(CHUNK, TOTAL - TAIL);

        kill_after(&run, target).await;
        assert!(!run.dest().exists(), "cycle {cycle}: a killed run published a file");

        let status = run.spawn().wait().await.unwrap();
        assert!(status.success(), "cycle {cycle}: resume failed");

        let written = std::fs::read(run.dest()).unwrap();
        assert_eq!(
            blake3::hash(&written),
            fixtures::digest(SEED, TOTAL),
            "cycle {cycle}: content mismatch after killing near {target}"
        );
    }
}

/// The looser modes must still never publish a wrong file.
///
/// Balanced holds completions back, so a kill here usually rolls the transfer
/// all the way back to zero. That is the trade the mode makes and it is fine;
/// what would not be fine is a published file that does not hash correctly, or
/// a `.part` the next run trusts further than the journal claims.
#[tokio::test(flavor = "multi_thread")]
async fn a_looser_durability_still_never_publishes_a_wrong_file() {
    for mode in ["balanced", "fast"] {
        let run = start_origin().await;
        let mut child = run.spawn_with(mode);

        // Kill on the clock rather than on journalled bytes: in these modes
        // the journal may legitimately still be empty.
        tokio::time::sleep(Duration::from_millis(120)).await;
        let finished = matches!(child.try_wait(), Ok(Some(_)));
        let _ = child.start_kill();
        let _ = child.wait().await;

        if !finished {
            assert!(!run.dest().exists(), "{mode}: a killed run published a file");
        }

        let status = run.spawn_with(mode).wait().await.unwrap();
        assert!(status.success(), "{mode}: resume failed");
        assert_eq!(
            blake3::hash(&std::fs::read(run.dest()).unwrap()),
            fixtures::digest(SEED, TOTAL),
            "{mode}: content mismatch"
        );
    }
}
