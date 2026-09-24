//! Timed runs against the mock origin, to answer what the scheduler costs.
//!
//! Every question here is one that reasoning about the code cannot settle
//! honestly: whether two paths really add up, what the connection ramp costs a
//! transfer that is over before it finishes ramping, and whether the endgame
//! recovers the tail or merely moves it.
//!
//! Run with `cargo run -p dl-bench --release`. Release matters: a debug build
//! spends enough time in hashing and the journal to swamp what is measured.
//!
//! Loopback is the transport, so no number here is an internet number. What is
//! meaningful is one configuration against another under identical conditions,
//! which is what every line below is. The chunk size is pinned for the same
//! reason: it is otherwise chosen from the lane count, and a run with more
//! lanes would be carrying twice the journal writes as well.

use anyhow::Result;
use dl_core::error::Result as DlResult;
use dl_core::lane::LaneSet;
use dl_core::source::{ByteSource, ByteStream, Fetch};
use dl_core::{ResumeOptions, SourceInfo, download_over_lanes};
use dl_net::{HttpConfig, HttpSource};
use dl_testkit::{Origin, Scenario};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Bytes in the payload. Long enough that the ramp is a small part of the run
/// and the tail is visible; short enough that the whole sweep takes a minute.
const SIZE: u64 = 64 << 20;

/// Fixed across every run, so lane count changes nothing but the lanes.
const CHUNK: u64 = 1 << 20;

/// How many times each configuration is run. The median is reported: a single
/// timing on a laptop is mostly a measure of what else was running.
const REPEATS: usize = 3;

/// A lane with a ceiling, so one path can be made slower than another without
/// needing two networks.
///
/// Applied to the body as it streams rather than up front: a lane that stalls
/// and then bursts is not a slow lane and would not exercise the same
/// decisions.
struct Capped {
    inner: HttpSource,
    bytes_per_sec: u64,
}

#[async_trait::async_trait]
impl ByteSource for Capped {
    async fn probe(&self) -> DlResult<SourceInfo> {
        self.inner.probe().await
    }

    async fn open(&self, fetch: Fetch) -> DlResult<ByteStream> {
        use futures_util::StreamExt;
        let stream = self.inner.open(fetch).await?;
        let rate = self.bytes_per_sec.max(1);
        Ok(Box::pin(stream.then(move |part| async move {
            if let Ok(bytes) = &part {
                tokio::time::sleep(Duration::from_secs_f64(bytes.len() as f64 / rate as f64)).await;
            }
            part
        })))
    }
}

struct Lanes {
    sources: Vec<Arc<dyn ByteSource>>,
    labels: Vec<String>,
}

impl LaneSet for Lanes {
    fn len(&self) -> usize {
        self.sources.len()
    }
    fn source(&self, lane: usize) -> &dyn ByteSource {
        self.sources[lane].as_ref()
    }
    fn label(&self, lane: usize) -> &str {
        &self.labels[lane]
    }
}

#[derive(Clone)]
struct Run {
    elapsed: Duration,
    size: u64,
    lanes: Vec<(String, u64)>,
}

impl Run {
    fn rate(&self) -> f64 {
        self.size as f64 / self.elapsed.as_secs_f64()
    }
}

fn mb(bytes_per_sec: f64) -> String {
    format!("{:.1} MB/s", bytes_per_sec / 1e6)
}

/// One download, timed end to end.
async fn once(url: &str, paths: &[(&str, u64)], connections: usize) -> Result<Run> {
    let config = HttpConfig::default();
    let mut sources: Vec<Arc<dyn ByteSource>> = Vec::new();
    let mut labels = Vec::new();
    for (label, cap) in paths {
        let http = HttpSource::with_config(&config, url)?;
        sources.push(Arc::new(Capped { inner: http, bytes_per_sec: *cap }));
        labels.push((*label).to_string());
    }
    let lanes = Lanes { sources, labels };

    let dir = tempfile::tempdir()?;
    let dest = dir.path().join("payload.bin");
    let started = Instant::now();
    let outcome = download_over_lanes(
        &lanes,
        &dest,
        ResumeOptions { connections, chunk_size: Some(CHUNK), ..Default::default() },
        None,
    )
    .await?;
    Ok(Run {
        elapsed: started.elapsed(),
        size: outcome.total,
        lanes: outcome.lanes.iter().map(|l| (l.label.clone(), l.bytes)).collect(),
    })
}

async fn measure(url: &str, paths: &[(&str, u64)], connections: usize) -> Result<Run> {
    let mut runs = Vec::new();
    for _ in 0..REPEATS {
        runs.push(once(url, paths, connections).await?);
    }
    runs.sort_by_key(|run| run.elapsed);
    Ok(runs[REPEATS / 2].clone())
}

fn report(name: &str, run: &Run, detail: bool) {
    println!("  {name:<32} {:>8.2}s  {:>11}", run.elapsed.as_secs_f64(), mb(run.rate()));
    if !detail {
        return;
    }
    for (label, bytes) in &run.lanes {
        let share = bytes * 100 / run.size.max(1);
        println!("      {label:<28} {:>9.1} MB  {share:>3}%", *bytes as f64 / 1e6);
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let origin = Origin::spawn(Scenario::Ok200 { size: SIZE }).await?;
    let url = origin.url("payload.bin");
    println!(
        "payload {:.1} MB, chunk {:.1} MB, median of {REPEATS}, loopback\n",
        SIZE as f64 / 1e6,
        CHUNK as f64 / 1e6
    );

    // Per connection, so more connections genuinely buy more until something
    // else becomes the limit. That is the shape of a real path.
    const LANE: u64 = 12_000_000;

    println!("what the connection ceiling is worth on one lane");
    for connections in [1, 2, 4, 8, 16] {
        let run = measure(&url, &[("lane", LANE)], connections).await?;
        report(&format!("ceiling {connections}"), &run, false);
    }

    println!("\ndo paths add up");
    let mut alone = None;
    for count in [1usize, 2, 4] {
        let paths: Vec<(&str, u64)> =
            ["a", "b", "c", "d"].into_iter().take(count).map(|l| (l, LANE)).collect();
        let run = measure(&url, &paths, 4).await?;
        report(&format!("{count} lane(s) at {}", mb(LANE as f64)), &run, count > 1);
        let first = alone.get_or_insert(run.rate());
        println!("      {:.2}x one lane", run.rate() / *first);
    }

    println!("\nwhat a far slower path costs the finish");
    let fast = measure(&url, &[("fast", LANE)], 8).await?;
    report("fast alone", &fast, false);
    for (name, cap) in [("slow", 2_000_000u64), ("crawling", 400_000)] {
        let both = measure(&url, &[("fast", LANE), (name, cap)], 8).await?;
        report(&format!("fast + {name}"), &both, true);
        println!(
            "      {:+.2}s against fast alone, ideal {:+.2}s",
            both.elapsed.as_secs_f64() - fast.elapsed.as_secs_f64(),
            SIZE as f64 / (fast.rate() + cap as f64) - fast.elapsed.as_secs_f64()
        );
    }

    Ok(())
}
