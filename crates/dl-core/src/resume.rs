//! Chunked, resumable, parallel downloading.
//!
//! Chunk boundaries are fixed once, because journal indices have to stay
//! stable across a crash. Tail latency is handled by making the grid fine
//! rather than by re-splitting a slow chunk mid-flight: workers pull from a
//! shared queue, so a slow connection simply claims fewer chunks, and the
//! worst case at the end is one outstanding chunk rather than one outstanding
//! half-file.

use crate::budget::{Budget, BudgetChain};
use crate::cancel::Cancel;
use crate::error::{Error, Result};
use crate::lane::{LaneReport, LaneSelector, LaneSet, SingleLane};
use crate::model::{Progress, SourceInfo};
use crate::source::{ByteSource, Fetch};
use crate::store::journal::{Opened, ResourceId};
use crate::store::layout::MIN_CHUNK_SIZE;
use crate::store::resumable::{DEFAULT_CHUNK_SIZE, ResumableFile};
use futures_util::StreamExt;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Enough chunks per connection that a slow one cannot hold up the finish.
const CHUNKS_PER_CONNECTION: u64 = 8;

/// Where the backoff starts when the origin rate limits us without saying for
/// how long.
const BASE_BACKOFF: Duration = Duration::from_millis(500);

/// How many times a probe waits out a rate limit before giving the lane up.
///
/// Small on purpose: this is before any bytes have moved, and an origin that
/// refuses three times in a row is not one to hold a transfer open for.
const PROBE_LIMIT_RETRIES: u32 = 3;

/// The longest a single wait may be.
///
/// Also caps a stated `Retry-After`: an origin asking for ten minutes is
/// telling us something useful, but holding every worker idle that long turns
/// a slow download into one that looks hung. Waiting the cap and asking again
/// costs one request and keeps the UI honest.
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// How long to wait before retrying a rate-limited chunk.
///
/// A stated `Retry-After` wins outright: the origin knows its own window, and
/// guessing shorter is how a client gets banned rather than throttled. Without
/// one, back off exponentially from [`BASE_BACKOFF`].
fn backoff_for(stated: Option<Duration>, attempts: u32) -> Duration {
    if let Some(stated) = stated {
        return stated.min(MAX_BACKOFF);
    }
    let doublings = attempts.min(8);
    (BASE_BACKOFF * 2u32.saturating_pow(doublings)).min(MAX_BACKOFF)
}

fn attempts_for(index: u64, seen: &Mutex<BTreeMap<u64, u32>>) -> u32 {
    seen.lock().unwrap().get(&index).copied().unwrap_or(0)
}

fn note_attempt(index: u64, seen: &Mutex<BTreeMap<u64, u32>>) {
    *seen.lock().unwrap().entry(index).or_insert(0) += 1;
}

pub struct ResumeOptions {
    /// `None` picks a size from the resource length and connection count.
    pub chunk_size: Option<u64>,
    /// How hard the store works to survive a power cut mid-transfer.
    pub durability: crate::store::Durability,
    /// Where the partial file and its journal live while the transfer runs.
    pub staging: crate::store::Staging,
    pub connections: usize,
    pub expect: Option<crate::integrity::Digest>,
    pub progress_interval: Duration,
    /// Re-read completed chunks on resume and re-fetch any that fail their hash.
    pub verify_existing: bool,
    /// Hash the finished file even when nothing was given to compare it
    /// against, so the digest can be shown and recorded.
    pub always_hash: Option<crate::integrity::Algorithm>,
    /// Keep the `.part` and its journal when a transfer fails, so it can be
    /// resumed later. Off deletes both: which is what someone who does not
    /// want half a file on disk means.
    ///
    /// Never keeps data a failure proved untrustworthy: a changed resource or
    /// a digest mismatch discards regardless.
    pub keep_partial: bool,
    /// Cap on this download's throughput, shared across all its lanes.
    pub limit: Option<Arc<Budget>>,
    /// Cap on each lane, by lane index. A ceiling for metered links.
    pub lane_limits: Vec<Option<Arc<Budget>>>,
    /// Stops the download between chunks. Anything already journalled stays.
    pub cancel: Cancel,
    /// Handed the lane selector once the download starts.
    ///
    /// Lane statistics are otherwise only returned at the end, which is too
    /// late for a UI that wants to show per-interface throughput while the
    /// transfer is running. Sharing the selector avoids copying reports on
    /// every progress tick.
    // `Sync` as well as `Send`: the options are borrowed across awaits, and a
    // borrow is only `Send` when the referent is `Sync`.
    pub on_lanes_ready: Option<Box<dyn FnOnce(Arc<LaneSelector>) + Send + Sync>>,
    /// Handed the live chunk map once the layout is known, for the same
    /// reason as `on_lanes_ready`: without it the only account of what the
    /// chunks did arrives after the transfer has ended.
    pub on_chunks_ready: Option<Box<dyn FnOnce(Arc<crate::chunks::ChunkProgress>) + Send + Sync>>,
    /// Called when the transfer starts or stops doing something other than
    /// moving bytes: checking what is on disk, hashing the finished file,
    /// publishing it. `None` means it is downloading normally.
    ///
    /// On a large file these take real time, and a bar sitting at 100% with
    /// nothing reported reads as a hang.
    pub on_phase: Option<Box<dyn Fn(Option<&'static str>) + Send + Sync>>,
}

impl Default for ResumeOptions {
    fn default() -> Self {
        Self {
            chunk_size: None,
            durability: crate::store::Durability::default(),
            staging: Default::default(),
            connections: 8,
            expect: None,
            progress_interval: Duration::from_millis(100),
            verify_existing: false,
            always_hash: None,
            keep_partial: true,
            limit: None,
            lane_limits: Vec::new(),
            cancel: Cancel::new(),
            on_lanes_ready: None,
            on_chunks_ready: None,
            on_phase: None,
        }
    }
}

#[derive(Debug)]
pub struct ResumeOutcome {
    pub path: PathBuf,
    pub total: u64,
    /// Bytes actually transferred, excluding anything reused from a previous run.
    pub transferred: u64,
    pub resumed_from: u64,
    pub opened_as: Opened,
    pub connections: usize,
    pub chunk_size: u64,
    pub chunks: u64,
    /// Chunks that failed verification on resume and were re-fetched.
    pub repaired: Vec<u64>,
    /// Per-lane throughput and share of the work.
    pub lanes: Vec<LaneReport>,
    /// The digest that was verified, in whichever algorithm was asked for.
    /// `None` when nothing was asked for: the file is not hashed end to end
    /// unless someone wants the answer.
    pub verified: Option<crate::integrity::Digest>,
    pub elapsed: Duration,
}

pub fn resource_id(info: &SourceInfo) -> ResourceId {
    ResourceId {
        total_len: info.len.unwrap_or(0),
        etag: info.etag.clone(),
        last_modified: info.last_modified.clone(),
    }
}

/// Pick a chunk size giving every connection plenty of chunks to pull.
///
/// Too few chunks and the slowest connection decides the finish time; too many
/// and the per-chunk `fsync` pair dominates.
pub fn choose_chunk_size(total: u64, connections: usize) -> u64 {
    let target_chunks = (connections as u64).max(1) * CHUNKS_PER_CONNECTION;
    let ideal = total.div_ceil(target_chunks.max(1));
    ideal.clamp(MIN_CHUNK_SIZE, DEFAULT_CHUNK_SIZE)
}

/// Download `source` to `destination`, continuing any previous attempt.
pub async fn download_resumable(
    source: &dyn ByteSource,
    destination: impl AsRef<Path>,
    options: ResumeOptions,
    on_progress: Option<crate::download::ProgressFn>,
) -> Result<ResumeOutcome> {
    download_over_lanes(&SingleLane::new(source), destination, options, on_progress).await
}

/// Download over several independent paths at once, spreading chunks across
/// them in proportion to measured throughput.
pub async fn download_over_lanes(
    lanes: &dyn LaneSet,
    destination: impl AsRef<Path>,
    mut options: ResumeOptions,
    mut on_progress: Option<crate::download::ProgressFn>,
) -> Result<ResumeOutcome> {
    if lanes.is_empty() {
        return Err(Error::Transport("no usable network path is available".into()));
    }
    let selector = Arc::new(LaneSelector::from_lanes(lanes));
    for (lane, limit) in options.lane_limits.iter().enumerate() {
        selector.set_cap(lane, limit.as_ref().map(|b| b.rate()));
    }
    if let Some(observer) = options.on_lanes_ready.take() {
        observer(Arc::clone(&selector));
    }

    // Probing is also the liveness check. A lane can be up, addressed and
    // routable and still reach nothing: a captive portal, a tunnel with no
    // route, an interface whose link just dropped: so any lane that cannot
    // answer a probe is parked here rather than being handed chunks that will
    // time out one by one.
    let info = probe_all_lanes(lanes, &selector).await?;
    if info.has_content_encoding() {
        return Err(Error::UnexpectedContentEncoding(
            info.content_encoding.clone().unwrap_or_default(),
        ));
    }
    // The probe proves range support with a real 206, so an origin that merely
    // advertises `Accept-Ranges` and then ignores Range is rejected here rather
    // than after scattering whole-body responses across chunk offsets.
    if !info.supports_chunking() {
        return Err(Error::RangeNotHonoured {
            detail: "the origin does not honour byte ranges, so it cannot be downloaded in chunks"
                .into(),
        });
    }

    let total = info.len.unwrap_or(0);
    let connections = options.connections.max(1);
    let chunk_size = options.chunk_size.unwrap_or_else(|| choose_chunk_size(total, connections));

    let mut file = ResumableFile::open_staged(
        destination,
        resource_id(&info),
        chunk_size,
        options.durability,
        &options.staging,
    )
    .await?;

    let mut repaired = Vec::new();
    let phase = |name: Option<&'static str>| {
        if let Some(report) = options.on_phase.as_ref() {
            report(name);
        }
    };

    if options.verify_existing && file.opened_as() == Opened::Resumed {
        phase(Some("Checking"));
        repaired = file.verify_chunks().await?;
        for index in &repaired {
            file.forget_chunk(*index).await?;
        }
        phase(None);
    }

    // After `verify_existing` has had its say, so a resumed transfer with a
    // damaged chunk shows it missing rather than briefly claiming it.
    let chunks_seen = crate::chunks::ChunkProgress::new(*file.layout());
    chunks_seen.seed(file.completed().clone());
    if let Some(observer) = options.on_chunks_ready.take() {
        observer(Arc::clone(&chunks_seen));
    }

    let resumed_from = file.bytes_done();
    let opened_as = file.opened_as();
    let chunks = file.layout().chunk_count();
    let started = Instant::now();

    let transferred = match fetch_all(
        lanes,
        &selector,
        &mut file,
        &info,
        &options,
        connections,
        started,
        &mut on_progress,
        &chunks_seen,
    )
    .await
    {
        Ok(transferred) => transferred,
        Err(e) => return Err(abandon(&mut file, e, options.keep_partial).await),
    };

    let digest = match (&options.expect, options.always_hash) {
        (Some(expected), _) => {
            phase(Some("Verifying"));
            let actual = hash_file(&file, expected.algorithm()).await?;
            if actual != *expected {
                let mismatch = Error::IntegrityMismatch {
                    expected: expected.to_hex(),
                    actual: actual.to_hex(),
                };
                return Err(abandon(&mut file, mismatch, options.keep_partial).await);
            }
            Some(actual)
        }
        // Nothing to compare against, but the digest is still worth having:
        // it is what a user pastes into an issue when a file looks wrong.
        (None, Some(algorithm)) => {
            phase(Some("Verifying"));
            Some(hash_file(&file, algorithm).await?)
        }
        (None, None) => None,
    };

    let elapsed = started.elapsed();
    if let Some(report) = on_progress.as_mut() {
        report(Progress {
            downloaded: total,
            total: Some(total),
            bytes_per_sec: rate(transferred, elapsed),
            smoothed_bytes_per_sec: 0,
        });
    }

    // Flushing the journal, fsyncing the data and publishing the file. Short
    // on a small transfer and distinctly not on a large one.
    phase(Some("Finishing"));
    let path = file.finalize().await?;
    phase(None);
    Ok(ResumeOutcome {
        path,
        total,
        transferred,
        resumed_from,
        opened_as,
        connections,
        chunk_size,
        chunks,
        repaired,
        lanes: selector.reports(),
        verified: digest,
        elapsed,
    })
}

/// Probe every lane, parking the ones that cannot answer or do not agree.
///
/// Every lane is probed rather than just the first, because lanes are not
/// always the same URL: mirrors arrive as lanes too, and one serving different
/// content has to be caught here. Splicing chunks from two versions produces a
/// file of exactly the right length that is neither of them, and no later
/// check would notice.
///
/// The first lane that answers is the reference, since that is the URL the
/// download was actually asked for; the mirrors are the extras.
async fn probe_all_lanes(
    lanes: &dyn LaneSet,
    selector: &LaneSelector,
) -> Result<crate::model::SourceInfo> {
    let mut reference: Option<crate::model::SourceInfo> = None;
    let mut last_error = None;

    for lane in 0..lanes.len() {
        match probe_with_backoff(lanes.source(lane), lanes.label(lane)).await {
            Ok(info) => match &reference {
                None => reference = Some(info),
                Some(reference) => {
                    if let Some(detail) = info.disagrees_with(reference) {
                        tracing::warn!(
                            lane = %lanes.label(lane),
                            detail,
                            "parking a mirror that is not serving the same resource"
                        );
                        selector.park(lane);
                        last_error = Some(Error::MirrorDisagrees { detail });
                    }
                }
            },
            Err(e) => {
                tracing::warn!(
                    lane = %lanes.label(lane),
                    error = %e,
                    "parking a network path that could not be probed"
                );
                // One failed probe is enough evidence; handing it chunks would
                // just repeat the timeout.
                selector.park(lane);
                last_error = Some(e);
            }
        }
    }

    reference.ok_or_else(|| last_error.unwrap_or(Error::NoRouteAvailable))
}

/// Probe a lane, waiting out a rate limit rather than reporting the path dead.
///
/// A 429 on the very first request is common: it is how an origin greets a
/// client that was already busy a moment ago: and parking the lane for it
/// would throw an interface away before the transfer had started. Every lane
/// getting that treatment means the download fails without a byte being tried.
async fn probe_with_backoff(
    source: &dyn ByteSource,
    label: &str,
) -> Result<crate::model::SourceInfo> {
    for attempt in 0..PROBE_LIMIT_RETRIES {
        match source.probe().await {
            Err(Error::RateLimited { status, retry_after }) => {
                let wait = backoff_for(retry_after, attempt);
                tracing::info!(
                    lane = %label,
                    status,
                    wait_ms = wait.as_millis() as u64,
                    "rate limited while probing; waiting"
                );
                tokio::time::sleep(wait).await;
            }
            other => return other,
        }
    }
    // Out of patience: report the limit itself rather than a generic failure,
    // so the caller can say why nothing started.
    source.probe().await
}

/// Decide what becomes of the partial file when a transfer stops badly.
///
/// Kept only when the user asked to keep partials *and* the failure leaves the
/// contents trustworthy. This is the whole value of a journal: a dropped
/// connection, or a pause, should cost the bytes in flight rather than the
/// gigabytes already on disk.
///
/// A changed resource, an origin that ignores ranges, or a failed digest are
/// different: those mean the bytes already written are suspect, and keeping
/// them would let a later resume assemble a file from two different things.
/// Those discard whatever the preference says.
async fn abandon(file: &mut ResumableFile, error: Error, keep_partial: bool) -> Error {
    if keep_partial && !error.invalidates_partial_data() {
        // Keeping the partial file is only worth anything if the journal knows
        // what is in it. Under a loose durability mode the last few completed
        // chunks are still in memory at this point, and without this a pause or
        // a dropped connection would throw away work that is already on disk.
        if let Err(flush) = file.flush().await {
            tracing::warn!(error = %flush, "could not record completed chunks before stopping");
        }
        return error;
    }
    if let Err(cleanup) = file.discard().await {
        tracing::warn!(error = %cleanup, "could not remove the partial file");
    }
    error
}

/// Run `connections` workers pulling chunks from a shared queue.
///
/// Work stealing rather than a static split: a connection that is slow, or one
/// bound to a slower interface once phase 6 lands, simply claims fewer chunks.
#[allow(clippy::too_many_arguments)]
async fn fetch_all(
    lanes: &dyn LaneSet,
    selector: &Arc<LaneSelector>,
    file: &mut ResumableFile,
    info: &SourceInfo,
    options: &ResumeOptions,
    connections: usize,
    started: Instant,
    on_progress: &mut Option<crate::download::ProgressFn>,
    chunks_seen: &Arc<crate::chunks::ChunkProgress>,
) -> Result<u64> {
    let queue = Arc::new(tokio::sync::Mutex::new(file.remaining()));
    if queue.lock().await.is_empty() {
        return Ok(0);
    }

    // Only completions are validated against the resource, so a strong
    // validator is carried on every chunk request.
    let validator = info.etag.clone();
    let transferred = Arc::new(AtomicU64::new(0));
    let total = info.len.unwrap_or(0);
    let already_done = file.bytes_done();

    // Chunks come back out of order, so they are journalled by a single owner
    // rather than written from the worker tasks.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<(u64, bytes::Bytes)>(connections * 2);

    let layout = *file.layout();

    // One chain per lane: the lane's own ceiling, then the download-wide cap.
    // Ordered innermost first so a lane blocked on its own ceiling does not
    // first consume download allowance that other lanes could have used.
    let budgets: Arc<Vec<BudgetChain>> = Arc::new(
        (0..lanes.len())
            .map(|lane| {
                let mut chain = BudgetChain::default();
                if let Some(Some(limit)) = options.lane_limits.get(lane) {
                    chain.push(Arc::clone(limit));
                }
                if let Some(limit) = &options.limit {
                    chain.push(Arc::clone(limit));
                }
                chain
            })
            .collect(),
    );
    // Built eagerly so every worker owns its sender before the original is
    // dropped; the writer loop ends when the last one goes away.
    // How many times each chunk has been rate limited, so the backoff grows
    // for a chunk that keeps being refused rather than restarting at the floor
    // every time a worker picks it up.
    let backoffs: Arc<Mutex<BTreeMap<u64, u32>>> = Arc::new(Mutex::new(BTreeMap::new()));

    let mut worker_futures = Vec::with_capacity(connections);
    for _ in 0..connections {
        let queue = Arc::clone(&queue);
        let backoffs = Arc::clone(&backoffs);
        let chunks_seen = Arc::clone(chunks_seen);
        let transferred = Arc::clone(&transferred);
        let selector = Arc::clone(selector);
        let budgets = Arc::clone(&budgets);
        let tx = tx.clone();
        let validator = validator.clone();
        let cancel = options.cancel.clone();
        worker_futures.push(async move {
            loop {
                // Checked between chunks, so a pause never interrupts a chunk
                // part-way and leaves it unjournalled.
                cancel.check()?;

                let Some(index) = queue.lock().await.pop() else {
                    return Ok::<(), Error>(());
                };
                let range = layout.range(index).expect("queued index is in range");
                chunks_seen.started(index);

                let Some(lane) = selector.acquire() else {
                    // Every path has failed out of rotation. Put the chunk back
                    // so the error, not a silently short file, is the outcome.
                    chunks_seen.released(index);
                    queue.lock().await.push(index);
                    return Err(Error::NoRouteAvailable);
                };

                let began = Instant::now();
                let body =
                    match fetch_chunk(lanes.source(lane), range, validator.clone(), &budgets[lane])
                        .await
                    {
                        Ok(body) => {
                            selector.completed(lane, range.len(), began.elapsed());
                            body
                        }
                        // The origin asking us to slow down is not a failing
                        // path. Moving the chunk to another interface and
                        // trying again at once is what turns one rate limit
                        // into a rate limit on every interface we own.
                        Err(Error::RateLimited { status, retry_after }) => {
                            let wait = backoff_for(retry_after, attempts_for(index, &backoffs));
                            tracing::info!(
                                status,
                                chunk = index,
                                wait_ms = wait.as_millis() as u64,
                                stated = retry_after.is_some(),
                                "rate limited; backing off"
                            );
                            selector.park_for(lane, wait);
                            chunks_seen.released(index);
                            queue.lock().await.push(index);
                            note_attempt(index, &backoffs);

                            if selector.all_parked_permanently() {
                                return Err(Error::RateLimited { status, retry_after });
                            }
                            // Sleep only if there is nowhere else to go; with a
                            // free lane the work continues there while this one
                            // waits out its period.
                            if let Some(pause) = selector.time_until_unpark()
                                && selector.all_parked()
                            {
                                tokio::time::sleep(pause.min(MAX_BACKOFF)).await;
                            }
                            continue;
                        }
                        Err(e) if e.is_retryable() => {
                            // A path that died mid-transfer should cost this chunk,
                            // not the download: requeue it for a healthier lane.
                            selector.failed(lane);
                            chunks_seen.released(index);
                            if !selector.all_parked() {
                                queue.lock().await.push(index);
                                continue;
                            }
                            return Err(e);
                        }
                        Err(e) => {
                            // The origin misbehaved, which every lane would hit
                            // identically. Not the lane's fault, so do not park it.
                            selector.released(lane);
                            chunks_seen.released(index);
                            return Err(e);
                        }
                    };
                transferred.fetch_add(range.len(), Ordering::Relaxed);

                if tx.send((index, body)).await.is_err() {
                    return Ok(());
                }
            }
        });
    }
    drop(tx);
    let workers = futures_util::future::join_all(worker_futures);

    let mut last_report = started;
    let writer = async {
        while let Some((index, body)) = rx.recv().await {
            file.write_chunk(index, body).await?;
            // After the write, not after the fetch: a chunk is Have when it is
            // on disk, and the grid must not show one the journal has never
            // been told about.
            chunks_seen.completed(index);

            if let Some(report) = on_progress.as_mut() {
                let now = Instant::now();
                if now.duration_since(last_report) >= options.progress_interval {
                    let done = transferred.load(Ordering::Relaxed);
                    report(Progress {
                        downloaded: already_done + done,
                        total: Some(total),
                        bytes_per_sec: rate(done, started.elapsed()),
                        smoothed_bytes_per_sec: 0,
                    });
                    last_report = now;
                }
            }
        }
        Ok::<(), Error>(())
    };

    let (results, written) = futures_util::future::join(workers, writer).await;

    // A worker error is the cause; a writer error is often just the closed
    // channel that followed it, so worker failures are reported first. Among
    // those, `NoRouteAvailable` is a consequence of whatever failed earlier, so
    // it is only reported when nothing more specific is available.
    let mut fallback = None;
    for result in results {
        match result {
            Ok(()) => {}
            Err(Error::NoRouteAvailable) => fallback = Some(Error::NoRouteAvailable),
            Err(e) => return Err(e),
        }
    }
    if let Some(e) = fallback {
        return Err(e);
    }
    written?;

    Ok(transferred.load(Ordering::Relaxed))
}

async fn fetch_chunk(
    source: &dyn ByteSource,
    range: crate::model::ByteRange,
    validator: Option<String>,
    budget: &BudgetChain,
) -> Result<bytes::Bytes> {
    let mut stream = source.open(Fetch::validated(range, validator)).await?;
    let mut buffer = Vec::with_capacity(range.len() as usize);

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if buffer.len() as u64 + chunk.len() as u64 > range.len() {
            return Err(Error::OverlongBody { expected: range.len() });
        }

        // Charged after the bytes arrive, not before: the socket has already
        // received them, and pacing here is what slows the sender down through
        // TCP back-pressure. Charging in advance would only add latency.
        let mut owed = chunk.len();
        while owed > 0 {
            owed -= budget.acquire(owed).await;
        }

        buffer.extend_from_slice(&chunk);
    }

    // Checked before the write so a short chunk is never journalled.
    if buffer.len() as u64 != range.len() {
        return Err(Error::ShortBody { expected: range.len(), received: buffer.len() as u64 });
    }
    Ok(bytes::Bytes::from(buffer))
}

/// Hash the assembled file by reading it back.
///
/// Chunks complete out of order and a resumed run never sees the bytes it
/// inherited, so streaming the hash during transfer is not possible.
/// Hash the assembled file, chunk by chunk so nothing larger than one chunk is
/// ever resident.
async fn hash_file(
    file: &ResumableFile,
    algorithm: crate::integrity::Algorithm,
) -> Result<crate::integrity::Digest> {
    let mut hasher = crate::integrity::Hasher::new(algorithm);
    for index in 0..file.layout().chunk_count() {
        hasher.update(&file.read_chunk(index).await?);
    }
    Ok(hasher.finalize())
}

fn rate(bytes: u64, elapsed: Duration) -> u64 {
    let secs = elapsed.as_secs_f64();
    if secs <= 0.0 { 0 } else { (bytes as f64 / secs) as u64 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_size_scales_with_connection_count() {
        // More connections means smaller chunks, so every worker has queue to
        // pull from and the tail stays short.
        let few = choose_chunk_size(1 << 30, 2);
        let many = choose_chunk_size(1 << 30, 32);
        assert!(many <= few, "more connections should not produce larger chunks");

        for connections in [1, 4, 16, 64] {
            let size = choose_chunk_size(1 << 30, connections);
            assert!(size >= MIN_CHUNK_SIZE);
            assert!(size <= DEFAULT_CHUNK_SIZE);
        }
    }

    #[test]
    fn a_small_file_still_gets_a_workable_chunk_size() {
        let size = choose_chunk_size(1000, 16);
        assert_eq!(size, MIN_CHUNK_SIZE, "tiny files should not be split into slivers");
    }

    #[test]
    fn a_stated_retry_after_wins_over_our_own_guess() {
        // The origin knows its own window. Guessing shorter is how a client
        // gets banned rather than throttled.
        let stated = Duration::from_secs(7);
        assert_eq!(backoff_for(Some(stated), 0), stated);
        assert_eq!(backoff_for(Some(stated), 5), stated, "attempts must not shorten it");
    }

    #[test]
    fn an_unstated_wait_grows_with_each_refusal() {
        let first = backoff_for(None, 0);
        let second = backoff_for(None, 1);
        let third = backoff_for(None, 2);
        assert_eq!(first, BASE_BACKOFF);
        assert!(second > first && third > second, "{first:?} {second:?} {third:?}");
    }

    #[test]
    fn no_wait_exceeds_the_cap_however_it_was_arrived_at() {
        // A wait long enough to look like a hang is worse than one more
        // request, both for the user and for anyone reading the UI.
        assert_eq!(backoff_for(Some(Duration::from_secs(3600)), 0), MAX_BACKOFF);
        assert_eq!(backoff_for(None, 30), MAX_BACKOFF);
        assert_eq!(backoff_for(None, u32::MAX), MAX_BACKOFF, "must not overflow");
    }

    #[test]
    fn a_zero_retry_after_is_honoured_as_zero() {
        // "Retry-After: 0" is a legal answer meaning go ahead now.
        assert_eq!(backoff_for(Some(Duration::ZERO), 0), Duration::ZERO);
    }

    #[test]
    fn attempts_are_counted_per_chunk_not_globally() {
        // Otherwise one unlucky chunk pushes every other chunk's first retry
        // to the cap.
        let seen = Mutex::new(BTreeMap::new());
        note_attempt(3, &seen);
        note_attempt(3, &seen);
        note_attempt(9, &seen);
        assert_eq!(attempts_for(3, &seen), 2);
        assert_eq!(attempts_for(9, &seen), 1);
        assert_eq!(attempts_for(4, &seen), 0);
    }
}
