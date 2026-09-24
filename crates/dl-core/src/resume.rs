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
use crate::regions::{Regions, Run};
use crate::source::{ByteSource, Fetch};
use crate::store::journal::{Opened, ResourceId};
use crate::store::layout::MIN_CHUNK_SIZE;
use crate::store::resumable::{DEFAULT_CHUNK_SIZE, ResumableFile};
use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
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

pub struct ResumeOptions {
    /// `None` picks a size from the resource length and connection count.
    pub chunk_size: Option<u64>,
    /// How hard the store works to survive a power cut mid-transfer.
    pub durability: crate::store::Durability,
    /// Where the partial file and its journal live while the transfer runs.
    pub staging: crate::store::Staging,
    /// The most connections one lane may open.
    ///
    /// A ceiling, not a target: each lane starts with one and earns more only
    /// by going measurably faster with them. A transfer over several lanes can
    /// therefore hold more sockets than this in total, which is the right way
    /// round: the number is there to be polite to one origin over one route,
    /// and each lane is a different route to it.
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
    // Sized for every lane's connections, not one lane's: with four paths open
    // the grid has to be fine enough for all of them to have something to walk,
    // and a run of chunks is also what a lane steals in halves.
    let chunk_size = options
        .chunk_size
        .unwrap_or_else(|| choose_chunk_size(total, connections.saturating_mul(lanes.len())));

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
    // Whatever window each lane had open is folded in now, so a transfer that
    // was over inside one still reports what its lanes carried.
    selector.close();

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

/// Give every lane its own stretch of the file and run connections on each.
///
/// The split, the stealing, and why it is not a shared queue any more are in
/// [`crate::regions`]. Here the job is to keep a set of connections attached to
/// each lane, to notice a lane that turns up after the transfer began, and to
/// hand finished chunks to the one task that writes them.
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
    let regions = Regions::new(file.remaining(), lanes.len());
    if regions.is_empty() {
        return Ok(0);
    }

    let roster = Roster::new(lanes);
    let transferred = AtomicU64::new(0);
    let inflight = AtomicUsize::new(0);
    let total = info.len.unwrap_or(0);
    let already_done = file.bytes_done();

    // Chunks come back out of order, so they are journalled by a single owner
    // rather than written from the worker tasks.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<(u64, bytes::Bytes)>(connections * 2);

    let crew = Crew {
        roster: &roster,
        selector,
        regions: &regions,
        chunks_seen,
        transferred: &transferred,
        inflight: &inflight,
        limits: Mutex::new(BTreeMap::new()),
        layout: *file.layout(),
        // Only completions are validated against the resource, so a strong
        // validator is carried on every chunk request.
        validator: info.etag.clone(),
        cancel: options.cancel.clone(),
        limit: options.limit.clone(),
        lane_limits: &options.lane_limits,
        sockets: RwLock::new(Vec::new()),
        holders: AtomicUsize::new(0),
        endgame: Endgame::default(),
    };

    // The sender lives with the supervisor and nowhere else, so the writer's
    // channel closes exactly when the last connection has finished. Holding a
    // spare anywhere outside it leaves the writer waiting on a chunk that is
    // never coming.
    let crew = &crew;
    let roster = &roster;
    let supervise = async move {
        let mut running: FuturesUnordered<BoxFuture<'_>> = FuturesUnordered::new();
        // Every lane starts with one connection and earns the rest. See
        // `Ramp`: opening eight on a phone sharing mobile data spends its
        // battery on connections that carry nothing.
        let mut ramps: Vec<Ramp> = Vec::new();
        for _ in 0..roster.len() {
            ramps.push(Ramp::new());
        }
        crew.resize(&mut running, &tx);
        tracing::debug!(lanes = roster.len(), "lanes opened");

        let mut look = tokio::time::interval(LANE_POLL);
        let mut tune = tokio::time::interval(RAMP_POLL);
        let mut results = Vec::new();
        while !running.is_empty() {
            tokio::select! {
                finished = running.next() => {
                    if let Some(outcome) = finished {
                        results.push(outcome);
                    }
                }
                // Separate from the search for new lanes, and more often:
                // finding a lane's connection count is the first few seconds
                // of a transfer, and waiting two seconds a step to do it is
                // most of a small download.
                _ = tune.tick() => {
                    for (lane, ramp) in ramps.iter_mut().enumerate() {
                        ramp.consider(selector.rate_of(lane), &crew.sockets(lane), connections);
                    }
                    crew.resize(&mut running, &tx);
                }
                _ = look.tick() => {
                    for joined in roster.take_on(&selector.live()) {
                        // The roster and the meter only ever grow by appending,
                        // so the index one hands out is the index the other
                        // will. Everything downstream reads lanes by number.
                        let lane = selector.join(joined.label.clone());
                        debug_assert_eq!(lane, joined.lane, "lane numbering came apart");
                        ramps.push(Ramp::new());
                        tracing::info!(
                            lane = %joined.label,
                            "a path that appeared mid-transfer is now carrying chunks"
                        );
                    }
                    crew.resize(&mut running, &tx);
                }
            }
        }
        results
    };

    let writer = async {
        // On a timer as well as on each chunk: a rate that only moves when a
        // chunk lands reports nothing at all while a slow lane is mid-chunk,
        // and stops dead the moment a transfer is paused.
        let mut ticker = tokio::time::interval(options.progress_interval);
        loop {
            tokio::select! {
                received = rx.recv() => {
                    let Some((index, body)) = received else { break };
                    file.write_chunk(index, body).await?;
                    // After the write, not after the fetch: a chunk is Have
                    // when it is on disk, and the grid must not show one the
                    // journal has never been told about.
                    chunks_seen.completed(index);
                }
                _ = ticker.tick() => {
                    if let Some(report) = on_progress.as_mut() {
                        report(Progress {
                            downloaded: already_done + transferred.load(Ordering::Relaxed),
                            total: Some(total),
                            // The lanes' own measurement, summed. The figure in
                            // the header and the figures in the sidebar are then
                            // the same reading rather than two clocks that drift
                            // apart and invite the user to spot the difference.
                            bytes_per_sec: selector.aggregate_throughput() as u64,
                            smoothed_bytes_per_sec: 0,
                        });
                    }
                }
            }
        }
        Ok::<(), Error>(())
    };

    let (results, written) = futures_util::future::join(supervise, writer).await;
    let _ = started;

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

/// How often the transfer looks for a lane that has appeared since it began.
///
/// Answering means reading the paired-phone list off disk, so this is slow
/// enough not to be a poll loop and quick enough that pairing a phone and
/// watching the sidebar feels like cause and effect.
const LANE_POLL: Duration = Duration::from_secs(2);

/// This lane's source, if it still has one.
fn source_of<'a>(roster: &'a Roster<'a>, lane: usize) -> Option<LaneRef<'a>> {
    roster.source(lane)
}

/// What a connection does after one stretch of the file.
enum Step {
    Continue,
    Stop,
}

/// How long one request should keep a connection busy.
///
/// The whole point of a run: a request per chunk leaves the connection idle
/// for a round trip between each one, which measured out at more than half the
/// bandwidth on a real link. Long enough to stop that mattering, short enough
/// that whoever holds the last stretch is not holding up the finish.
const RUN_TARGET: Duration = Duration::from_secs(8);

/// Chunks per request before a lane's speed is known.
const OPENING_RUN: usize = 4;

/// The most chunks one request may cover, whatever the lane is managing.
const MAX_RUN: usize = 512;

/// How often a chunk in flight looks at whether it has been called off.
///
/// Bounds how long a paused transfer keeps pulling bytes, and how long the
/// finish waits on a copy that has already been beaten.
const CANCEL_POLL: Duration = Duration::from_millis(25);

/// How long a worker with nothing to do waits before looking again.
///
/// It is only reached at the very end of a transfer, or while another lane is
/// waiting out a rate limit, so the cost is a handful of wakeups.
const IDLE_POLL: Duration = Duration::from_millis(50);

/// How much faster a lane has to be before it will fetch a second copy of a
/// chunk somebody else is already on.
const ENDGAME_GAIN: f64 = 1.5;

/// The most copies of one chunk that may be in flight at once.
const ENDGAME_COPIES: usize = 2;

/// Chunks in flight, and who is racing for them.
///
/// At the end of a transfer every lane but one has run out of work, and the
/// finish time belongs to whoever is still mid-chunk. On a phone that is ten
/// seconds for four megabytes while a gigabit card sits idle waiting for it,
/// and no amount of dividing the file up beforehand helps, because the chunk
/// was already in flight when the last of the work ran out.
///
/// So a lane with nothing left may fetch its own copy of a chunk a slower lane
/// is already carrying. The first copy to land is kept and the other is called
/// off where it stands. BitTorrent has called this the endgame for twenty
/// years, for this exact reason.
///
/// Only a measurably faster lane may join a race, and only two lanes per
/// chunk, so it cannot turn into every lane fetching everything. That
/// restraint matters more here than it does in a swarm: a duplicate over a
/// phone's mobile data is somebody's money, and the lane that would be
/// duplicating is by construction not the phone.
#[derive(Default)]
struct Endgame {
    racing: Mutex<BTreeMap<u64, Contest>>,
}

struct Contest {
    /// Lanes fetching this chunk right now.
    lanes: Vec<usize>,
    /// The best rate among them, which is what a newcomer has to beat.
    best: f64,
    /// Cancelled by whichever copy lands first.
    landed: Cancel,
}

impl Endgame {
    /// Note that a lane has started a chunk it owns, and hand back the token
    /// its fetch should run under.
    fn started(&self, chunk: u64, lane: usize, rate: f64) -> Cancel {
        let mut racing = self.racing.lock().unwrap();
        let contest = racing.entry(chunk).or_insert_with(|| Contest {
            lanes: Vec::new(),
            best: 0.0,
            landed: Cancel::new(),
        });
        contest.lanes.push(lane);
        contest.best = contest.best.max(rate);
        contest.landed.clone()
    }

    /// A chunk worth racing for a lane going at `rate`, if there is one.
    ///
    /// Joins the race as well as finding it, under one lock, so two idle lanes
    /// asking at the same moment cannot both decide to be the second copy.
    fn join(&self, lane: usize, rate: f64) -> Option<(u64, Cancel)> {
        let mut racing = self.racing.lock().unwrap();
        let chunk = racing
            .iter()
            .find(|(_, contest)| {
                contest.lanes.len() < ENDGAME_COPIES
                    && !contest.lanes.contains(&lane)
                    && !contest.landed.is_cancelled()
                    && rate > contest.best * ENDGAME_GAIN
            })
            .map(|(chunk, _)| *chunk)?;
        let contest = racing.get_mut(&chunk)?;
        contest.lanes.push(lane);
        contest.best = contest.best.max(rate);
        Some((chunk, contest.landed.clone()))
    }

    /// Claim the chunk for the copy that just finished it.
    ///
    /// `false` means another copy got there first and this one is to be thrown
    /// away. Decided under the lock so exactly one copy is ever written: two
    /// finishing together would otherwise both be counted, and the transfer
    /// would report more bytes downloaded than the file has.
    fn won(&self, chunk: u64) -> bool {
        let mut racing = self.racing.lock().unwrap();
        let Some(contest) = racing.get_mut(&chunk) else { return true };
        if contest.landed.is_cancelled() {
            return false;
        }
        contest.landed.cancel();
        true
    }

    /// This lane is done with the chunk, however it went.
    fn finished(&self, chunk: u64, lane: usize) {
        let mut racing = self.racing.lock().unwrap();
        let Some(contest) = racing.get_mut(&chunk) else { return };
        if let Some(at) = contest.lanes.iter().position(|holder| *holder == lane) {
            contest.lanes.remove(at);
        }
        if contest.lanes.is_empty() {
            racing.remove(&chunk);
        }
    }
}

/// How many connections one lane has open, and how many it ought to have.
///
/// Read by the lane's own connections so one can retire itself when the count
/// comes down, and written only by the supervisor.
#[derive(Debug, Default)]
struct Sockets {
    open: AtomicUsize,
    want: AtomicUsize,
}

/// Finding out how many connections a lane is actually worth.
///
/// Opening eight connections to every lane is the usual approach and it is
/// wrong in both directions. A single connection to a distant origin is often
/// held back by its own window rather than by the link, so one connection
/// leaves most of a fast interface unused; and a phone sharing mobile data is
/// frequently saturated by two, so the other six buy nothing and cost the
/// phone battery, the origin connections, and this project the argument that
/// it is being careful with someone else's data plan.
///
/// So the count is measured rather than assumed: double it, see whether the
/// lane got faster, and stop when it did not. Doubling rather than stepping,
/// because reaching eight one at a time would spend half a minute getting
/// there on a link that could have had it in two moves.
#[derive(Debug)]
struct Ramp {
    /// The fastest the lane has gone, and at how many connections.
    best: f64,
    best_at: usize,
    /// Whether the search has finished. A lane that later falls well short of
    /// its best starts again: the link changed, so the answer may have too.
    settled: bool,
    changed_at: Instant,
}

/// How much faster a lane has to get for another connection to be worth it.
///
/// Well above measurement noise, because the cost of being wrong in this
/// direction is a connection that stays open for the rest of the transfer.
const RAMP_GAIN: f64 = 1.15;

/// How often a lane's connection count is reconsidered.
const RAMP_POLL: Duration = Duration::from_secs(1);

/// How long a lane runs at a new connection count before it is judged.
///
/// Long enough for the lane's own average to have caught up with the change,
/// or the measurement describes the count before last. Just under [`RAMP_POLL`]
/// so that every tick is allowed to decide something: a step that took two
/// ticks would double the time a transfer spends finding its feet.
const RAMP_SETTLE: Duration = Duration::from_millis(900);

/// A drop this far below the best seen means the link itself changed, and the
/// count that was right for it probably has not stayed right.
const RAMP_RESET: f64 = 0.5;

impl Ramp {
    fn new() -> Self {
        Self { best: 0.0, best_at: 1, settled: false, changed_at: Instant::now() }
    }

    /// Decide this lane's connection count from what it is currently doing.
    fn consider(&mut self, rate: Option<f64>, sockets: &Sockets, ceiling: usize) {
        let want = sockets.want.load(Ordering::Relaxed).max(1);
        // Judging before the lane has settled at the count it was last given
        // measures the previous count, not this one.
        if self.changed_at.elapsed() < RAMP_SETTLE || sockets.open.load(Ordering::Relaxed) != want {
            return;
        }
        let Some(rate) = rate.filter(|r| *r > 0.0) else { return };

        if self.settled {
            if rate < self.best * RAMP_RESET {
                tracing::debug!(want, "a lane slowed down; looking for its connection count again");
                self.settled = false;
                self.best = rate;
                self.best_at = want;
                self.changed_at = Instant::now();
            }
            return;
        }

        if rate > self.best * RAMP_GAIN {
            self.best = rate;
            self.best_at = want;
            if want < ceiling {
                self.set(sockets, (want * 2).min(ceiling));
                return;
            }
        }
        // Either the extra connections bought nothing or there is no room for
        // more. Fall back to the count that was fastest and stop asking.
        self.settled = true;
        if want != self.best_at {
            self.set(sockets, self.best_at);
        }
    }

    fn set(&mut self, sockets: &Sockets, count: usize) {
        tracing::debug!(from = sockets.want.load(Ordering::Relaxed), to = count, "connections");
        sockets.want.store(count, Ordering::Relaxed);
        self.changed_at = Instant::now();
    }
}

type BoxFuture<'a> = std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>>;

/// A lane's source, whichever side of the transfer's start it arrived on.
enum LaneRef<'a> {
    Fixed(&'a dyn ByteSource),
    Added(Arc<dyn ByteSource>),
}

impl LaneRef<'_> {
    fn get(&self) -> &dyn ByteSource {
        match self {
            Self::Fixed(source) => *source,
            Self::Added(source) => source.as_ref(),
        }
    }
}

/// A lane that turned up after the transfer started, and the number it got.
struct NewLane {
    lane: usize,
    label: String,
}

/// The lanes a transfer is using, including any that appeared along the way.
///
/// Lanes are numbered once and never renumbered: the meter, the regions and
/// the journal all refer to them by index, and a set that reordered itself
/// would silently reassign somebody else's work.
struct Roster<'a> {
    fixed: &'a dyn LaneSet,
    added: Mutex<Vec<crate::lane::Joined>>,
}

impl<'a> Roster<'a> {
    fn new(fixed: &'a dyn LaneSet) -> Self {
        Self { fixed, added: Mutex::new(Vec::new()) }
    }

    fn len(&self) -> usize {
        self.fixed.len() + self.added.lock().unwrap().len()
    }

    fn source(&self, lane: usize) -> Option<LaneRef<'a>> {
        if lane < self.fixed.len() {
            return Some(LaneRef::Fixed(self.fixed.source(lane)));
        }
        let added = self.added.lock().unwrap();
        added.get(lane - self.fixed.len()).map(|l| LaneRef::Added(Arc::clone(&l.source)))
    }

    /// Adopt whatever the lane set offers, given which lanes are still alive.
    fn take_on(&self, live: &[bool]) -> Vec<NewLane> {
        let mut added = self.added.lock().unwrap();
        let fresh = self.fixed.joined(live);
        let mut out = Vec::with_capacity(fresh.len());
        for lane in fresh {
            out.push(NewLane { lane: self.fixed.len() + added.len(), label: lane.label.clone() });
            added.push(lane);
        }
        out
    }
}

/// Everything a connection needs, so the worker body is written once.
struct Crew<'a> {
    roster: &'a Roster<'a>,
    selector: &'a Arc<LaneSelector>,
    regions: &'a Regions,
    chunks_seen: &'a Arc<crate::chunks::ChunkProgress>,
    transferred: &'a AtomicU64,
    /// Chunks handed out and not yet finished, across every lane.
    ///
    /// A worker that has run out of work cannot simply stop: one of these may
    /// still come back as a hand-back, and by then there would be nobody left
    /// to fetch it.
    inflight: &'a AtomicUsize,
    /// How many times each lane has been rate limited, so the wait grows for
    /// one that keeps being refused rather than restarting at the floor.
    ///
    /// Per lane rather than per chunk, because the lane is what gets parked:
    /// an origin refusing us is refusing the path, not the offset.
    limits: Mutex<BTreeMap<usize, u32>>,
    layout: crate::store::layout::ChunkLayout,
    validator: Option<String>,
    cancel: Cancel,
    limit: Option<Arc<Budget>>,
    lane_limits: &'a [Option<Arc<Budget>>],
    /// How many connections each lane has, by lane index. Grows as lanes do.
    sockets: RwLock<Vec<Arc<Sockets>>>,
    /// Hands each connection an id of its own, so the regions can tell two
    /// connections on the same lane apart: they walk different stretches.
    holders: AtomicUsize,
    endgame: Endgame,
}

impl<'a> Crew<'a> {
    /// This lane's connection counts, creating them if the lane is new.
    fn sockets(&self, lane: usize) -> Arc<Sockets> {
        {
            let open = self.sockets.read().unwrap();
            if let Some(found) = open.get(lane) {
                return Arc::clone(found);
            }
        }
        let mut open = self.sockets.write().unwrap();
        while open.len() <= lane {
            // One to begin with. The ramp adds more if they help.
            let fresh = Sockets::default();
            fresh.want.store(1, Ordering::Relaxed);
            open.push(Arc::new(fresh));
        }
        Arc::clone(&open[lane])
    }

    /// Open whatever connections the ramps have asked for and not yet got.
    ///
    /// Taking one away is the connection's own job: it notices after its
    /// current chunk and stops. Cancelling it here would abandon bytes that
    /// have already been fetched.
    fn resize(
        &'a self,
        running: &mut FuturesUnordered<BoxFuture<'a>>,
        tx: &tokio::sync::mpsc::Sender<(u64, bytes::Bytes)>,
    ) {
        for lane in 0..self.roster.len() {
            let sockets = self.sockets(lane);
            while sockets.open.load(Ordering::Relaxed) < sockets.want.load(Ordering::Relaxed) {
                sockets.open.fetch_add(1, Ordering::Relaxed);
                let holder = self.holders.fetch_add(1, Ordering::Relaxed);
                running.push(Box::pin(self.work(lane, holder, tx.clone())));
            }
        }
    }

    /// One chain per lane: the lane's own ceiling, then the download-wide cap.
    /// Ordered innermost first so a lane blocked on its own ceiling does not
    /// first consume download allowance that other lanes could have used.
    fn budget(&self, lane: usize) -> BudgetChain {
        let mut chain = BudgetChain::default();
        if let Some(Some(limit)) = self.lane_limits.get(lane) {
            chain.push(Arc::clone(limit));
        }
        if let Some(limit) = &self.limit {
            chain.push(Arc::clone(limit));
        }
        chain
    }

    /// One connection, fetching this lane's chunks until there are none.
    async fn work(
        &self,
        lane: usize,
        holder: usize,
        tx: tokio::sync::mpsc::Sender<(u64, bytes::Bytes)>,
    ) -> Result<()> {
        let budget = self.budget(lane);
        let sockets = self.sockets(lane);
        // Every exit from here goes through `retire`, which is what keeps the
        // open count honest: a lane that undercounts its connections opens
        // more on the next tick and never stops.
        let retire = || {
            sockets.open.fetch_sub(1, Ordering::Relaxed);
        };
        // Stand down if the lane has more connections than it wants, claiming
        // the place in the same step. Checking and then decrementing would let
        // every connection on the lane see the same surplus and all leave.
        let step_down = || {
            let mut open = sockets.open.load(Ordering::Relaxed);
            loop {
                if open <= sockets.want.load(Ordering::Relaxed) {
                    return false;
                }
                match sockets.open.compare_exchange_weak(
                    open,
                    open - 1,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => return true,
                    Err(actual) => open = actual,
                }
            }
        };
        loop {
            if let Err(e) = self.cancel.check() {
                retire();
                return Err(e);
            }
            // The ramp decided this lane does not need as many connections as
            // it has. Between chunks is the only safe place to notice.
            if step_down() {
                return Ok(());
            }

            if !self.selector.claim(lane) {
                match self.selector.park_remaining(lane) {
                    // Waiting out a rate limit. The other lanes carry on.
                    Some(wait) => {
                        tokio::time::sleep(wait.clamp(IDLE_POLL, MAX_BACKOFF)).await;
                        continue;
                    }
                    // This lane is finished. Whatever it had left is handed
                    // back so another lane can walk it.
                    None => {
                        self.regions.retire(holder);
                        retire();
                        return Ok(());
                    }
                }
            }

            let rate = self.selector.rate_of(lane).unwrap_or(0.0);
            // Its own stretch of the file first; failing that, a copy of a
            // chunk a slower lane is holding up the finish with.
            let taken = match self.regions.begin(holder, lane, rate, self.run_limit(rate)) {
                Some(run) => Some((run, None)),
                None => self
                    .endgame
                    .join(lane, rate)
                    .map(|(index, stop)| (Run { first: index, end: index + 1 }, Some(stop))),
            };
            let Some((run, copy)) = taken else {
                if self.regions.is_empty() && self.inflight.load(Ordering::Acquire) == 0 {
                    retire();
                    return Ok(());
                }
                if self.selector.all_parked_permanently() {
                    retire();
                    return Err(Error::NoRouteAvailable);
                }
                tokio::time::sleep(IDLE_POLL).await;
                continue;
            };

            self.inflight.fetch_add(1, Ordering::AcqRel);
            let outcome = self.stream(lane, holder, rate, run, copy, &budget, &tx).await;
            self.inflight.fetch_sub(1, Ordering::AcqRel);
            match outcome {
                Ok(Step::Continue) => continue,
                Ok(Step::Stop) => {
                    retire();
                    return Ok(());
                }
                Err(e) => {
                    retire();
                    return Err(e);
                }
            }
        }
    }

    /// How many chunks one request should cover.
    ///
    /// Long enough that the round trip between requests stops mattering, short
    /// enough that a connection holding the last of them is not holding up the
    /// finish. Measured in time rather than bytes, so a phone on mobile data
    /// asks for a stretch it can actually get through.
    fn run_limit(&self, rate: f64) -> usize {
        if rate <= 0.0 {
            return OPENING_RUN;
        }
        let bytes = rate * RUN_TARGET.as_secs_f64();
        ((bytes / self.layout.chunk_size().max(1) as f64).ceil() as usize).clamp(1, MAX_RUN)
    }

    /// Stream one stretch of the file over a single request, cutting chunks
    /// out of it as the bytes pass.
    ///
    /// This is the difference between a download that holds a line open and
    /// one that keeps knocking. A request per chunk leaves the connection idle
    /// for a round trip between each, and on a real link that measured out at
    /// more than half the available bandwidth. Chunks are still the unit the
    /// journal and the per-chunk hash work in, because that is what makes
    /// corruption local and a crash cheap; they no longer each cost a request.
    ///
    /// `copy` is set when this is a second copy of a chunk another lane is
    /// already carrying, which happens only at the very end of a transfer.
    ///
    /// Exactly one chunk is this connection's at a time: the one being filled.
    /// The rest of the run is still in its claim, which is what lets a faster
    /// lane take the back of it mid-stream, and what means a failure hands
    /// back one chunk rather than a stretch nobody else was waiting on.
    #[allow(clippy::too_many_arguments)]
    async fn stream(
        &self,
        lane: usize,
        holder: usize,
        rate: f64,
        run: Run,
        copy: Option<Cancel>,
        budget: &BudgetChain,
        tx: &tokio::sync::mpsc::Sender<(u64, bytes::Bytes)>,
    ) -> Result<Step> {
        let owned = copy.is_none();
        let first = self.layout.range(run.first).expect("a pending index is in range");
        let last = self.layout.range(run.end - 1).expect("a pending index is in range");
        let span = crate::model::ByteRange::new(first.start, last.end);

        let mut index = run.first;
        let mut left = run.len();
        // A second copy leaves the grid alone: the chunk is already shown as
        // being fetched, by the lane that owns it.
        if owned {
            self.chunks_seen.started(index);
        }
        let mut stop = match &copy {
            Some(stop) => stop.clone(),
            None => self.endgame.started(index, lane, rate),
        };

        // Give the chunk in hand back, and only that one. Everything after it
        // in the run is still in the claim and will be offered again; handing
        // those back too would put them in two places at once.
        let drop_current = |index: u64| {
            if owned {
                self.chunks_seen.released(index);
                self.regions.give_back([index]);
            }
        };

        let mut body = match source_of(self.roster, lane) {
            Some(source) => {
                match source.get().open(Fetch::validated(span, self.validator.clone())).await {
                    Ok(body) => body,
                    Err(e) => {
                        drop_current(index);
                        self.endgame.finished(index, lane);
                        return self.blame(lane, e);
                    }
                }
            }
            None => {
                drop_current(index);
                self.endgame.finished(index, lane);
                return Err(Error::NoRouteAvailable);
            }
        };

        let mut want = self.layout.range(index).expect("in range").len();
        let mut buffer: Vec<u8> = Vec::with_capacity(want as usize);
        let mut had: u64 = 0;

        loop {
            // Checked as the body arrives, not only between chunks. Waiting
            // for a chunk to finish means a pause keeps pulling on every lane
            // at once, and on a borrowed mobile connection that is somebody's
            // money being spent after they asked it to stop.
            if self.cancel.is_cancelled() {
                drop_current(index);
                self.endgame.finished(index, lane);
                return Err(Error::Cancelled);
            }
            if stop.is_cancelled() {
                // Another lane finished the chunk this stream is filling.
                self.endgame.finished(index, lane);
                if owned {
                    self.chunks_seen.released(index);
                }
                return Ok(Step::Continue);
            }

            // On a clock as well as on arrival. Checking only when bytes land
            // ties how fast a stream can be called off to how fast it is
            // going, so the slowest lane, which is the one most likely to be
            // paused or overtaken, is the slowest to notice.
            let Ok(next) = tokio::time::timeout(CANCEL_POLL, body.next()).await else {
                continue;
            };
            let part = match next {
                Some(Ok(part)) => part,
                Some(Err(e)) => {
                    drop_current(index);
                    self.endgame.finished(index, lane);
                    return self.blame(lane, e);
                }
                None => break,
            };
            if had + buffer.len() as u64 + part.len() as u64 > span.len() {
                drop_current(index);
                self.endgame.finished(index, lane);
                return Err(Error::OverlongBody { expected: span.len() });
            }

            // Charged after the bytes arrive, not before: the socket has
            // already received them, and pacing here is what slows the sender
            // down through TCP back-pressure.
            let mut owed = part.len();
            while owed > 0 {
                owed -= budget.acquire(owed).await;
            }
            // Reported as they land rather than when a chunk ends, so a lane's
            // rate is current.
            self.selector.progressed(lane, part.len() as u64);
            buffer.extend_from_slice(&part);

            // Cut out every whole chunk the buffer now holds.
            while buffer.len() as u64 >= want {
                let rest = buffer.split_off(want as usize);
                let whole = bytes::Bytes::from(std::mem::replace(&mut buffer, rest));

                if !self.endgame.won(index) {
                    // A copy landed first. Nothing to write and nothing owed.
                    self.endgame.finished(index, lane);
                    if owned {
                        self.chunks_seen.released(index);
                    }
                    return Ok(Step::Continue);
                }
                self.selector.completed(lane, want);
                // A chunk through means the lane is being served again, so the
                // next refusal starts its wait at the floor.
                self.limits.lock().unwrap().remove(&lane);
                self.transferred.fetch_add(want, Ordering::Relaxed);
                self.endgame.finished(index, lane);
                had += want;
                left -= 1;
                if tx.send((index, whole)).await.is_err() {
                    return Ok(Step::Stop);
                }

                index += 1;
                if left == 0 {
                    return Ok(Step::Continue);
                }
                // The back of a claim can be taken while it is being streamed,
                // by a lane that has run out and is faster. Carrying on would
                // fetch what somebody else is now fetching.
                if owned && !self.regions.take_next(holder) {
                    return Ok(Step::Continue);
                }
                want = self.layout.range(index).expect("in range").len();
                if owned {
                    self.chunks_seen.started(index);
                }
                stop = self.endgame.started(index, lane, rate);
            }
        }

        // The body ended early. The part chunk in hand is discarded: nothing
        // is journalled until it is whole, so abandoning one costs bytes and
        // never correctness.
        drop_current(index);
        self.endgame.finished(index, lane);
        self.blame(lane, Error::ShortBody { expected: span.len(), received: had })
    }

    /// Decide what a failure means for the lane it happened on.
    fn blame(&self, lane: usize, error: Error) -> Result<Step> {
        match error {
            // The origin asking us to slow down is not a failing path. Moving
            // the work to another interface and trying again at once is what
            // turns one rate limit into a rate limit on every interface we own.
            Error::RateLimited { status, retry_after } => {
                let refusals = {
                    let mut limits = self.limits.lock().unwrap();
                    let count = limits.entry(lane).or_insert(0);
                    *count += 1;
                    *count - 1
                };
                let wait = backoff_for(retry_after, refusals);
                tracing::info!(
                    status,
                    wait_ms = wait.as_millis() as u64,
                    stated = retry_after.is_some(),
                    "rate limited; backing off"
                );
                self.selector.park_for(lane, wait);
                if self.selector.all_parked_permanently() {
                    return Err(Error::RateLimited { status, retry_after });
                }
                Ok(Step::Continue)
            }
            e if e.is_retryable() => {
                // A path that died mid-transfer should cost its stretch, not
                // the download: it has been handed back for a healthier lane.
                self.selector.failed(lane);
                if self.selector.all_parked() {
                    return Err(e);
                }
                Ok(Step::Continue)
            }
            // The origin misbehaved, which every lane would hit identically.
            // Not the lane's fault, so do not park it.
            e => {
                self.selector.released(lane);
                Err(e)
            }
        }
    }
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

    /// A lane whose speed is a function of how many connections it has, so a
    /// ramp can be driven without a network.
    fn ramp_to(speeds: &[f64], ceiling: usize) -> usize {
        let sockets = Sockets::default();
        sockets.want.store(1, Ordering::Relaxed);
        let mut ramp = Ramp::new();

        for _ in 0..12 {
            let want = sockets.want.load(Ordering::Relaxed);
            sockets.open.store(want, Ordering::Relaxed);
            // Stand in for the wait, so the ramp judges what it just asked for.
            ramp.changed_at = Instant::now() - RAMP_SETTLE * 2;
            let rate = speeds.get(want - 1).copied().unwrap_or_else(|| speeds[speeds.len() - 1]);
            ramp.consider(Some(rate), &sockets, ceiling);
        }
        sockets.want.load(Ordering::Relaxed)
    }

    #[test]
    fn a_much_faster_lane_takes_over_the_chunk_holding_up_the_finish() {
        // The tail this exists for: everything is handed out, one slow lane is
        // still mid-chunk, and the fast lane has nothing to do but wait.
        let endgame = Endgame::default();
        let slow = endgame.started(41, 0, 400e3);
        assert!(!slow.is_cancelled());

        let (chunk, fast) = endgame.join(1, 15e6).expect("the fast lane should join the race");
        assert_eq!(chunk, 41);

        assert!(endgame.won(41), "the first copy to finish keeps it");
        assert!(slow.is_cancelled(), "the slower copy was left running");
        assert!(fast.is_cancelled());
    }

    #[test]
    fn a_lane_no_faster_than_the_one_already_on_it_does_not_duplicate() {
        // Two copies over the same kind of link is bandwidth spent for a
        // coin toss, and on a phone it is spent out of somebody's data plan.
        let endgame = Endgame::default();
        endgame.started(7, 0, 10e6);
        assert!(endgame.join(1, 11e6).is_none());
        assert!(endgame.join(1, 20e6).is_some(), "a genuinely faster lane should join");
    }

    #[test]
    fn a_chunk_is_never_fetched_more_than_twice() {
        let endgame = Endgame::default();
        endgame.started(7, 0, 400e3);
        assert!(endgame.join(1, 15e6).is_some());
        assert!(endgame.join(2, 30e6).is_none(), "a third copy of one chunk");
    }

    #[test]
    fn a_lane_does_not_race_itself() {
        // One lane's connections all go idle together at the end; without
        // this, they would all pile onto the same chunk.
        let endgame = Endgame::default();
        endgame.started(7, 0, 400e3);
        assert!(endgame.join(0, 400e3).is_none());
    }

    #[test]
    fn exactly_one_copy_is_ever_written() {
        // Two copies finishing together must not both be counted, or the
        // transfer reports more bytes downloaded than the file contains.
        let endgame = Endgame::default();
        endgame.started(7, 0, 400e3);
        endgame.join(1, 15e6).expect("joined");
        assert!(endgame.won(7));
        assert!(!endgame.won(7), "the second copy was counted too");
    }

    #[test]
    fn nobody_joins_a_race_that_is_already_decided() {
        let endgame = Endgame::default();
        endgame.started(7, 0, 400e3);
        assert!(endgame.won(7));
        assert!(endgame.join(1, 15e6).is_none());
    }

    #[test]
    fn a_chunk_is_forgotten_once_every_copy_has_finished_with_it() {
        // The map is every chunk in flight, so a leak here is a leak per
        // chunk for the length of the transfer.
        let endgame = Endgame::default();
        endgame.started(7, 0, 400e3);
        endgame.join(1, 15e6).expect("joined");
        endgame.finished(7, 1);
        assert_eq!(endgame.racing.lock().unwrap().len(), 1, "the owner is still on it");
        endgame.finished(7, 0);
        assert!(endgame.racing.lock().unwrap().is_empty());
    }

    #[test]
    fn a_chunk_nobody_is_racing_is_simply_won() {
        // The ordinary case: one copy, no contest, and the same code path.
        let endgame = Endgame::default();
        let stop = endgame.started(3, 0, 1e6);
        assert!(endgame.won(3));
        assert!(stop.is_cancelled(), "winning should call off any copy that appears later");
    }

    #[test]
    fn a_lane_held_back_by_one_connection_gets_more() {
        // The whole point: a single connection to a distant origin is limited
        // by its own window, not by the link, and the only way to find out is
        // to open another and see.
        let speeds = [2e6, 4e6, 6e6, 8e6, 10e6, 12e6, 14e6, 16e6];
        assert_eq!(ramp_to(&speeds, 8), 8);
    }

    #[test]
    fn a_lane_that_stops_gaining_is_wound_back_to_where_it_did() {
        // Four connections is as much as this link will take; the eighth is
        // tried, found to buy nothing, and given up.
        let speeds = [2e6, 4e6, 8e6, 16e6, 16e6, 16e6, 16e6, 16e6];
        assert_eq!(ramp_to(&speeds, 8), 4);
    }

    #[test]
    fn a_lane_that_is_already_saturated_stays_on_one_connection() {
        // A phone sharing mobile data. Seven more connections would carry
        // nothing and spend its battery doing it.
        let speeds = [400e3, 405e3, 402e3, 398e3, 400e3, 400e3, 400e3, 400e3];
        assert_eq!(ramp_to(&speeds, 8), 1);
    }

    #[test]
    fn the_search_stops_at_the_count_that_was_fastest() {
        // Two helps, four does not: the answer is two, not the four it had to
        // try in order to find out.
        let speeds = [1e6, 2e6, 2.05e6, 2.0e6, 2.0e6, 2.0e6, 2.0e6, 2.0e6];
        assert_eq!(ramp_to(&speeds, 8), 2);
    }

    #[test]
    fn the_ceiling_is_respected() {
        let speeds = [1e6, 4e6, 16e6, 64e6, 256e6, 1e9, 4e9, 16e9];
        assert_eq!(ramp_to(&speeds, 4), 4);
        assert_eq!(ramp_to(&speeds, 1), 1);
    }

    #[test]
    fn a_lane_that_slows_right_down_is_measured_again() {
        // Switching from Wi-Fi to a tether changes the answer, and a count
        // settled on the old link has no reason to suit the new one.
        let sockets = Sockets::default();
        sockets.want.store(4, Ordering::Relaxed);
        sockets.open.store(4, Ordering::Relaxed);
        let mut ramp = Ramp::new();
        ramp.best = 10e6;
        ramp.best_at = 4;
        ramp.settled = true;
        ramp.changed_at = Instant::now() - RAMP_SETTLE * 2;

        ramp.consider(Some(9e6), &sockets, 8);
        assert!(ramp.settled, "a small dip is not a different link");

        ramp.consider(Some(1e6), &sockets, 8);
        assert!(!ramp.settled, "a lane that fell to a tenth was never looked at again");
    }

    #[test]
    fn a_count_is_not_judged_before_it_has_had_time_to_show() {
        // Reading the rate straight after a change measures the count before
        // last, and would talk the ramp into a number by accident.
        let sockets = Sockets::default();
        sockets.want.store(2, Ordering::Relaxed);
        sockets.open.store(2, Ordering::Relaxed);
        let mut ramp = Ramp::new();
        ramp.consider(Some(50e6), &sockets, 8);
        assert_eq!(sockets.want.load(Ordering::Relaxed), 2, "judged too early");
    }

    #[test]
    fn nothing_happens_while_the_connections_asked_for_are_still_opening() {
        let sockets = Sockets::default();
        sockets.want.store(4, Ordering::Relaxed);
        sockets.open.store(2, Ordering::Relaxed);
        let mut ramp = Ramp::new();
        ramp.changed_at = Instant::now() - RAMP_SETTLE * 2;
        ramp.consider(Some(50e6), &sockets, 8);
        assert_eq!(sockets.want.load(Ordering::Relaxed), 4);
        assert!(!ramp.settled);
    }

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
    fn a_run_covers_about_eight_seconds_of_whatever_the_lane_manages() {
        // Long enough that the round trip between requests stops mattering,
        // short enough that whoever holds the last one is not holding up the
        // finish. A phone and a gigabit card get very different stretches.
        let layout = crate::store::layout::ChunkLayout::new(1 << 30, 4 << 20);
        let chunks = |rate: f64| {
            let bytes = rate * RUN_TARGET.as_secs_f64();
            ((bytes / layout.chunk_size() as f64).ceil() as usize).clamp(1, MAX_RUN)
        };
        assert_eq!(chunks(400e3), 1, "a phone should ask for a stretch it can finish");
        assert_eq!(chunks(36e6), 69);
        assert_eq!(chunks(10e9), MAX_RUN, "a run is capped however fast the link is");
    }
}
