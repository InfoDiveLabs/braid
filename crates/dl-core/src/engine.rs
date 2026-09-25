//! Many downloads at once, with a queue and a lifecycle.
//!
//! The engine owns the truth. Nothing here calls back into a UI: callers take
//! a [`Engine::snapshot`] whenever they want to draw. That keeps redraw cost
//! bounded by how often the UI asks rather than by how fast bytes arrive, which
//! is the difference between a progress display that works with eight downloads
//! and one that melts.

use crate::budget::Budget;
use crate::cancel::Cancel;
use crate::error::Result;
use crate::lane::{LaneReport, LaneSelector, LaneSet};
use crate::model::Progress;
use crate::resume::{ResumeOptions, download_over_lanes};
use crate::torrent::{
    TorrentBackend, TorrentProgress, TorrentRequest, TorrentSource, TorrentStatus, TransferKind,
};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DownloadId(pub u64);

impl std::fmt::Display for DownloadId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{}", self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Waiting for a slot.
    Queued,
    Running,
    /// Stopped by the user. Everything journalled is kept.
    Paused,
    /// Every piece is on disk and the transfer is giving rather than taking.
    /// Only a torrent reaches this, and only the user leaves it.
    Seeding,
    Complete,
    Failed,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Seeding => "seeding",
            Self::Complete => "done",
            Self::Failed => "error",
        }
    }

    /// Whether the engine is done with this transfer for good.
    ///
    /// Seeding is not: the files are complete but the transfer is still
    /// running, still uploading, and still stoppable. Calling it terminal
    /// would make pause a no-op on the one state where uploading is all that
    /// is happening.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Complete | Self::Failed)
    }
}

/// What to download, and how.
#[derive(Clone, Debug)]
pub struct DownloadSpec {
    pub url: String,
    /// Where the bytes land. A file path for an HTTP transfer; for a torrent
    /// it is the **folder** the torrent's own files are written under, because
    /// a torrent names its contents and we do not get to rename them.
    pub destination: PathBuf,
    pub connections: usize,
    /// Verify against this digest when the transfer finishes.
    pub expect: Option<crate::integrity::Digest>,
    pub interfaces: Vec<String>,
    /// Which of the engine's two paths this takes. Derived from the URL by
    /// [`crate::torrent::classify`] in [`DownloadSpec::new`] so no caller has
    /// to remember to set it, and overridable for the rare case that knows
    /// better.
    pub kind: TransferKind,
}

impl DownloadSpec {
    pub fn new(url: impl Into<String>, destination: impl Into<PathBuf>) -> Self {
        let url = url.into();
        let kind = crate::torrent::classify(&url);
        Self {
            url,
            destination: destination.into(),
            connections: 8,
            expect: None,
            interfaces: Vec::new(),
            kind,
        }
    }

    /// The torrent this spec names, if it names one.
    pub fn torrent_source(&self) -> Option<&TorrentSource> {
        match &self.kind {
            TransferKind::Torrent(source) => Some(source),
            _ => None,
        }
    }
}

/// A point-in-time view of one download. Plain data, cheap to clone.
#[derive(Clone, Debug)]
pub struct DownloadSnapshot {
    pub id: DownloadId,
    pub filename: String,
    pub host: String,
    pub state: State,
    pub progress: Progress,
    pub lanes: Vec<LaneReport>,
    /// Work the transfer is doing that is not moving bytes: verifying what is
    /// on disk, hashing the finished file, publishing it. `None` when it is
    /// simply downloading, which is the usual case.
    ///
    /// Shown rather than hidden because these take real time on a large file,
    /// and a progress bar sitting at 100% with nothing happening reads as a
    /// hang.
    pub phase: Option<String>,
    pub error: Option<String>,
    /// Peers, upload and the files inside the torrent. `None` for an HTTP
    /// transfer, which has none of those and must not report zero as though
    /// it had looked.
    pub torrent: Option<TorrentStatus>,
}

/// Builds the network paths for a URL. Keeps the engine free of HTTP.
pub trait SourceFactory: Send + Sync + 'static {
    fn lanes_for(&self, spec: &DownloadSpec) -> Result<Box<dyn LaneSet>>;
}

#[derive(Clone, Debug)]
pub struct EngineConfig {
    /// How many downloads may run at once. The rest wait in the queue.
    pub max_concurrent: usize,
    pub chunk_size: Option<u64>,
    pub durability: crate::store::Durability,
    /// Where partial files live while a transfer runs. See
    /// `dl_core::store::Staging`.
    pub staging: crate::store::Staging,
    /// Ceilings for individual interfaces, by the lane label the factory gives
    /// them. A lane with no entry here is limited only by the global budget.
    pub interface_limits: std::collections::BTreeMap<String, u64>,
    /// Re-read completed chunks on resume and re-fetch any that fail.
    pub verify_existing: bool,
    /// Hash every finished file, even with nothing to compare it against.
    pub verify_every: Option<crate::integrity::Algorithm>,
    pub keep_partial: bool,
    /// Attempts after the first before a transfer is called failed.
    pub retries: u32,
    /// What to call traffic the engine cannot attribute to an interface.
    ///
    /// A torrent's sockets belong to the backend, and an HTTP transfer with no
    /// interface pinned is routed by the OS. Both still have to appear in the
    /// sidebar meters and the throughput graph, and calling them "default"
    /// invents a NIC that sits beside the real ones carrying all the traffic.
    /// The caller knows the honest answer: the interface the default route
    /// uses: so it supplies it.
    pub unattributed_lane: String,
    /// Stay in the swarm once a torrent's last piece arrives.
    ///
    /// On, a finished torrent sits in [`State::Seeding`] until the user stops
    /// it, which is what a torrent client does and the only way the swarm gets
    /// anything back. Off, it finishes like a download. HTTP transfers are
    /// unaffected either way.
    pub seed_after_complete: bool,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            max_concurrent: 3,
            chunk_size: None,
            durability: Default::default(),
            staging: Default::default(),
            interface_limits: Default::default(),
            verify_existing: false,
            verify_every: None,
            keep_partial: true,
            retries: 0,
            unattributed_lane: "network".into(),
            seed_after_complete: true,
        }
    }
}

struct Record {
    spec: DownloadSpec,
    state: State,
    progress: Progress,
    /// Final lane statistics, kept once the download ends.
    lanes: Vec<LaneReport>,
    /// Live statistics while running. Read straight from the selector so
    /// per-interface throughput is visible during the transfer rather than
    /// only after it.
    selector: Option<Arc<LaneSelector>>,
    /// The smoothed transfer rate, for the time estimate. `None` until the
    /// first sample, so the average starts from a real figure rather than
    /// climbing out of zero and reporting hours for the first few seconds.
    smoothed_rate: Option<f64>,
    /// When the last rate sample arrived, so the average spans a fixed period
    /// of time rather than a fixed number of reports.
    rate_sampled_at: Option<Instant>,
    /// See `DownloadSnapshot::phase`.
    phase: Option<String>,
    /// Raised before cancelling to tell the torrent backend the files go too.
    torrent_delete: Arc<std::sync::atomic::AtomicBool>,
    /// The live chunk map, while the transfer is running. Pulled by id rather
    /// than folded into the snapshot: see `crate::chunks`.
    chunks: Option<Arc<crate::chunks::ChunkProgress>>,
    error: Option<String>,
    cancel: Cancel,
    /// What to call this transfer when the destination path cannot say.
    ///
    /// A torrent's destination is a folder, so the filename of the path is the
    /// folder's name and not the torrent's. This holds the magnet's `dn=`
    /// until the real metadata arrives, and the torrent's own name after.
    display_name: Option<String>,
    /// Peers, upload and file list, for a torrent. Stays `None` for HTTP.
    torrent: Option<TorrentStatus>,
    /// See `Restorable::labels`. Held here only so a transfer restored with
    /// some keeps them for the next time it is written back out.
    labels: BTreeMap<String, String>,
}

/// How long the rate average takes to follow a step change.
///
/// Throughput measured over a tenth of a second is genuinely spiky: a chunk
/// landing, a peer choking, a lane being parked: and showing that raw made the
/// readout unreadable and let a single burst set the graph's axis so that
/// everything after it drew as a flat line near zero.
///
/// The HTTP path already reports a plain mean over the last second, so for a
/// download this only takes the last of the edge off an already steady figure.
/// It earns its keep on the torrent backend, which reports raw instantaneous
/// rates: without it one spike sets the graph's axis and everything after it
/// draws as a flat line near zero.
const RATE_TAU: Duration = Duration::from_millis(600);

impl Record {
    /// Fold a progress report into the smoothed rate.
    ///
    /// Time-based rather than a fixed weight per sample: the HTTP path reports
    /// every 100ms and the torrent backend every 250ms, and a fixed weight
    /// would smooth them over different spans and make the two kinds of
    /// transfer behave differently on the same graph.
    fn smooth(&mut self, progress: Progress) -> Progress {
        self.smooth_at(progress, Instant::now())
    }

    /// Fold a report in, never letting the byte count go backwards.
    ///
    /// A run counts bytes as they are fetched, while a retry restarts from
    /// what the journal has. Anything fetched but not yet journalled when a
    /// lane failed is therefore counted once and then not counted, and the
    /// figure steps back. The bytes on disk never decrease, so neither should
    /// the number describing them: a progress bar that retreats reads as
    /// corruption, which is the one thing this project must never look like.
    fn advance(&mut self, mut progress: Progress) -> Progress {
        progress.downloaded = progress.downloaded.max(self.progress.downloaded);
        self.smooth(progress)
    }

    /// The same, with the clock supplied.
    ///
    /// Separated so the span can be exercised exactly. Sleeping for it instead
    /// measures the scheduler: on a loaded machine ten 50ms sleeps overshoot
    /// far more than two 250ms ones, and one filter then looks like two.
    fn smooth_at(&mut self, mut progress: Progress, now: Instant) -> Progress {
        let elapsed = self.rate_sampled_at.map(|last| now.duration_since(last));
        self.rate_sampled_at = Some(now);

        let sample = progress.bytes_per_sec as f64;
        let smoothed = match (self.smoothed_rate, elapsed) {
            (Some(previous), Some(elapsed)) => {
                let alpha = 1.0 - (-elapsed.as_secs_f64() / RATE_TAU.as_secs_f64()).exp();
                previous + (sample - previous) * alpha
            }
            // Seeded from the first real sample, not from zero, or every
            // transfer opens reporting nothing for a second and a half.
            _ => sample,
        };
        self.smoothed_rate = Some(smoothed);
        progress.smoothed_bytes_per_sec = smoothed as u64;
        progress
    }

    fn snapshot(&self, id: DownloadId) -> DownloadSnapshot {
        DownloadSnapshot {
            id,
            filename: self
                .display_name
                .clone()
                .unwrap_or_else(|| filename_of(&self.spec.destination)),
            host: host_of(&self.spec.url),
            state: self.state,
            progress: self.progress,
            lanes: match (&self.selector, self.state) {
                (Some(selector), State::Running) => selector.reports(),
                _ => self.lanes.clone(),
            },
            phase: self.phase.clone(),
            error: self.error.clone(),
            torrent: self.torrent.clone(),
        }
    }
}

/// Remove the files one transfer wrote. See [`Engine::remove_with_files`] for
/// the rules; this is the part that touches the disk.
fn delete_transfer_files(record: &Record, staging: &crate::store::Staging) -> Vec<PathBuf> {
    let destination = &record.spec.destination;
    let mut removed = Vec::new();

    let mut unlink = |path: PathBuf| {
        if path.is_file() && std::fs::remove_file(&path).is_ok() {
            removed.push(path);
        }
    };

    // Sidecars first: they are ours by construction, whichever path this is.
    // Asked of the staging rules rather than assembled here, so a configured
    // incomplete folder is not left holding the partial of a transfer the user
    // just removed.
    let (part, meta) = staging.paths_for(destination);
    unlink(part);
    unlink(meta);

    // A torrent's contents belong to the backend, which deleted them on the
    // way out; reconstructing the paths here as well would be a second, less
    // informed attempt at the same job.
    if record.torrent.is_some() {
        return removed;
    }

    match record.torrent.as_ref().filter(|t| !t.files.is_empty()) {
        // A torrent's destination is its output folder, and the torrent is the
        // authority on what is inside it.
        Some(torrent) => {
            for file in &torrent.files {
                // Checked before joining, not after. `destination.join("../x")`
                // is lexically `dest/../x`, and `starts_with` compares
                // components, so it answers "yes, inside dest" for a path that
                // plainly is not. A broken or malicious torrent naming
                // `../../x` would have had that file deleted.
                if !is_contained(&file.path) {
                    tracing::warn!(path = %file.path, "refusing to delete a path outside the torrent's folder");
                    continue;
                }
                unlink(destination.join(&file.path));
            }
            prune_empty_dirs(destination, &mut removed);
        }
        None => unlink(destination.clone()),
    }
    removed
}

/// Whether a torrent's relative path stays inside the folder it was given.
///
/// Rejects anything absolute, anything with a root or prefix, and any `..`
/// component. Nothing legitimate in a torrent needs them.
fn is_contained(relative: &str) -> bool {
    use std::path::Component;
    let path = std::path::Path::new(relative);
    !path.components().any(|component| {
        matches!(component, Component::ParentDir | Component::RootDir | Component::Prefix(_))
    })
}

/// Remove `root` and any directories under it that are now empty, deepest
/// first. Never touches a directory that still holds anything.
fn prune_empty_dirs(root: &std::path::Path, removed: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            prune_empty_dirs(&path, removed);
        }
    }
    if std::fs::read_dir(root).is_ok_and(|mut d| d.next().is_none())
        && std::fs::remove_dir(root).is_ok()
    {
        removed.push(root.to_path_buf());
    }
}

/// One budget per lane, for the lanes whose interface has a ceiling set.
///
/// Built fresh per transfer rather than shared: a per-interface cap is a
/// ceiling for that link, and two downloads over a metered tether should each
/// be held to it rather than splitting one allowance between them. The global
/// budget above is what bounds the app as a whole.
fn lane_limits(
    lanes: &dyn LaneSet,
    limits: &std::collections::BTreeMap<String, u64>,
) -> Vec<Option<Arc<Budget>>> {
    if limits.is_empty() {
        return Vec::new();
    }
    (0..lanes.len()).map(|i| limits.get(lanes.label(i)).map(|r| Budget::with_rate(*r))).collect()
}

fn filename_of(path: &std::path::Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "download".into())
}

fn host_of(url: &str) -> String {
    url.rsplit("://").next().and_then(|rest| rest.split('/').next()).unwrap_or(url).to_string()
}

struct Inner {
    /// Throughput of everything that is not a torrent, so the torrent backend
    /// can take the headroom under the shared limit rather than honouring it
    /// again on its own.
    http_rate: Arc<std::sync::atomic::AtomicU64>,
    records: Mutex<BTreeMap<DownloadId, Record>>,
    next_id: AtomicU64,
    /// Swapped rather than locked: every transfer reads it and the settings
    /// window writes it, and a reader must never wait on a preference change.
    /// A transfer already running keeps the config it started with, which is
    /// the only coherent answer: its chunk layout is on disk.
    config: arc_swap::ArcSwap<EngineConfig>,
    factory: Arc<dyn SourceFactory>,
    budget: Arc<Budget>,
    /// The upload ceiling. Only the torrent path has anything to apply it to,
    /// but it lives here so the Bandwidth page has one place to set it whether
    /// or not a torrent is running right now.
    upload_budget: Arc<Budget>,
    /// Empty in a build with no torrent support compiled in, which is the
    /// case the error message has to name. Set once at startup rather than
    /// swapped, because a backend owns a live session with sockets in it and
    /// replacing one mid-transfer would strand them.
    torrent: std::sync::OnceLock<Arc<dyn TorrentBackend>>,
}

/// A transfer as it goes to disk and comes back.
///
/// Carries what a row needs to read correctly before anything runs again: the
/// size and the bytes already on disk, and the name a torrent only learns from
/// its metadata.
#[derive(Debug, Clone)]
pub struct Restorable {
    pub spec: DownloadSpec,
    pub state: State,
    pub downloaded: u64,
    pub total: Option<u64>,
    pub name: Option<String>,
    /// Whatever a front end wants attached to a transfer that the engine has
    /// no business interpreting: a category, a pair of timestamps, anything
    /// else in that vein. Carried through unread, so a compatibility detail
    /// like a qBittorrent category never becomes something this crate has an
    /// opinion about.
    pub labels: BTreeMap<String, String>,
}

/// A set of downloads with a shared bandwidth budget and a concurrency limit.
#[derive(Clone)]
pub struct Engine {
    inner: Arc<Inner>,
}

impl Engine {
    pub fn new(factory: Arc<dyn SourceFactory>, config: EngineConfig, budget: Arc<Budget>) -> Self {
        Self {
            inner: Arc::new(Inner {
                http_rate: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                records: Mutex::new(BTreeMap::new()),
                next_id: AtomicU64::new(1),
                config: arc_swap::ArcSwap::from_pointee(config),
                factory,
                budget,
                upload_budget: Budget::unlimited(),
                torrent: std::sync::OnceLock::new(),
            }),
        }
    }

    /// Replace the configuration for transfers that have not started yet.
    ///
    /// Running transfers are left alone: their chunk size is recorded in the
    /// journal on disk, so changing it underneath them would invalidate a
    /// resume that is currently in progress.
    pub fn set_config(&self, config: EngineConfig) {
        self.inner.config.store(Arc::new(config));
        // A raised concurrency limit should start queued work now rather than
        // at the next completion.
        self.pump();
    }

    pub fn config(&self) -> Arc<EngineConfig> {
        self.inner.config.load_full()
    }

    /// The chunk map of one transfer, or `None` if it is not running.
    ///
    /// An explicit call rather than a field on the snapshot: the snapshot is
    /// rebuilt for every transfer ten times a second, and a bitmap in each
    /// one would cost the whole list to serve one open panel.
    pub fn chunks(&self, id: DownloadId) -> Option<crate::chunks::ChunkReport> {
        let records = self.inner.records.lock().unwrap();
        records.get(&id)?.chunks.as_ref().map(|c| c.report())
    }

    /// Where one transfer writes its bytes, or `None` if there is no such
    /// transfer.
    ///
    /// Nothing before this needed a destination back out of the engine: a
    /// caller supplied it once, to [`Self::add`], and never had to ask again.
    /// The qBittorrent-compatible API breaks that, because `save_path` and
    /// `content_path` are fields somebody else's client polls for by hash.
    pub fn destination(&self, id: DownloadId) -> Option<PathBuf> {
        let records = self.inner.records.lock().unwrap();
        records.get(&id).map(|r| r.spec.destination.clone())
    }

    /// The URL a transfer was asked for.
    ///
    /// For a magnet this is the only place the info hash exists until the
    /// backend has joined the swarm, and a client that adds a torrent asks for
    /// it by hash within the second. Without this the compatible API can only
    /// answer once the backend reports, so the client sees nothing, concludes
    /// its request was lost, and adds the same torrent again on every poll.
    pub fn url(&self, id: DownloadId) -> Option<String> {
        let records = self.inner.records.lock().unwrap();
        records.get(&id).map(|r| r.spec.url.clone())
    }

    /// The labels attached to one transfer, or empty if there are none.
    ///
    /// [`Self::set_labels`] is write-only by design: nothing that wrote a
    /// category needed to read it back, because the record it was attached to
    /// carried it forward on its own. Listing transfers by category, the way
    /// the compatible API's clients do, needs the other direction as well.
    pub fn labels(&self, id: DownloadId) -> BTreeMap<String, String> {
        let records = self.inner.records.lock().unwrap();
        records.get(&id).map(|r| r.labels.clone()).unwrap_or_default()
    }

    pub fn budget(&self) -> &Arc<Budget> {
        &self.inner.budget
    }

    /// The ceiling on what this app gives back to swarms.
    ///
    /// Separate from the download budget because they are separate directions
    /// on the wire, and because an upload cap that also throttled downloads
    /// would be a control that does something other than what it says.
    pub fn upload_budget(&self) -> &Arc<Budget> {
        &self.inner.upload_budget
    }

    /// Register the backend that runs torrents.
    ///
    /// Optional on purpose: with none registered, a magnet fails with an error
    /// naming the missing feature rather than being pushed down the HTTP path
    /// to die as an unparseable URL.
    pub fn set_torrent_backend(&self, backend: Arc<dyn TorrentBackend>) {
        let _ = self.inner.torrent.set(backend);
    }

    /// Whether this build can open a torrent at all.
    pub fn has_torrent_backend(&self) -> bool {
        self.inner.torrent.get().is_some()
    }

    /// Queue a download. It starts as soon as a slot is free.
    pub fn add(&self, spec: DownloadSpec) -> DownloadId {
        let id = DownloadId(self.inner.next_id.fetch_add(1, Ordering::SeqCst));
        // A torrent's destination is a folder, so its row needs a name from
        // somewhere else until the metadata lands.
        let display_name = spec.torrent_source().and_then(|s| s.provisional_name());
        {
            let mut records = self.inner.records.lock().unwrap();
            records.insert(
                id,
                Record {
                    spec,
                    state: State::Queued,
                    progress: Progress::default(),
                    lanes: Vec::new(),
                    selector: None,
                    smoothed_rate: None,
                    rate_sampled_at: None,
                    phase: None,
                    torrent_delete: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    chunks: None,
                    error: None,
                    cancel: Cancel::new(),
                    display_name,
                    torrent: None,
                    labels: BTreeMap::new(),
                },
            );
        }
        self.pump();
        id
    }

    /// Attach front-end metadata to a transfer that has already started.
    ///
    /// [`Self::add`] takes a spec and nothing else, because a spec is
    /// everything the engine needs in order to fetch bytes. A category or a
    /// timestamp is not that: it belongs to whoever is presenting the
    /// transfer, and the engine's only duty is to keep it and hand it back
    /// through [`Self::specs`] so it survives a restart with the rest of the
    /// record.
    ///
    /// Without this the only way in was [`Self::restore`], which exists to put
    /// history back at startup rather than to label something just added.
    /// Anything a front end learned after a transfer had begun was therefore
    /// lost at the next restart, and a category assigned by whoever asked for
    /// the download would not be there when they came back looking for it.
    pub fn set_labels(&self, id: DownloadId, labels: BTreeMap<String, String>) {
        let mut records = self.inner.records.lock().unwrap();
        if let Some(record) = records.get_mut(&id) {
            record.labels = labels;
        }
    }

    /// Stop a download, keeping everything already written.
    pub fn pause(&self, id: DownloadId) {
        let mut records = self.inner.records.lock().unwrap();
        if let Some(record) = records.get_mut(&id)
            && !record.state.is_terminal()
        {
            record.cancel.cancel();
            record.state = State::Paused;
        }
    }

    /// Put a paused or failed download back in the queue.
    pub fn resume(&self, id: DownloadId) {
        {
            let mut records = self.inner.records.lock().unwrap();
            let Some(record) = records.get_mut(&id) else { return };
            if record.state == State::Complete {
                return;
            }
            // A fresh token: the old one is cancelled forever.
            record.cancel = Cancel::new();
            record.error = None;
            record.state = State::Queued;
        }
        self.pump();
    }

    /// Cancel and forget a download. Files on disk are left alone.
    /// Forget a transfer, leaving whatever is on disk.
    pub fn remove(&self, id: DownloadId) {
        self.remove_with_files(id, false);
    }

    /// Forget a transfer and, if asked, delete what it wrote.
    ///
    /// Only paths this transfer is known to have created are touched:
    /// - the `.part` and `.dlmeta` sidecars, which nothing else can own;
    /// - the destination, but **only when it is a file**;
    /// - for a torrent, each file the torrent itself named, inside its own
    ///   output folder, and then that folder if it is left empty.
    ///
    /// A directory is never removed recursively. The destination of a torrent
    /// is a folder assembled from a name the *link* supplied, and a recursive
    /// delete driven by that is how a download manager empties someone's
    /// Downloads folder.
    ///
    /// Returns the paths actually removed.
    pub fn remove_with_files(&self, id: DownloadId, delete_files: bool) -> Vec<PathBuf> {
        let record = {
            let mut records = self.inner.records.lock().unwrap();
            let record = records.remove(&id);
            if let Some(record) = record.as_ref() {
                // Raised before the cancel, so the backend sees it on the tick
                // that stops it rather than one tick too late.
                record.torrent_delete.store(delete_files, std::sync::atomic::Ordering::SeqCst);
                record.cancel.cancel();
            }
            record
        };
        self.pump();

        let Some(record) = record else { return Vec::new() };

        // A torrent's files belong to the backend. Told directly rather than
        // only through the cancel flag, because a transfer that has already
        // finished has no loop left to notice the flag: and until it is told,
        // librqbit keeps the torrent registered and re-adding the same link
        // picks up from the files that were meant to be gone.
        if let (TransferKind::Torrent(source), Some(backend)) =
            (crate::torrent::classify(&record.spec.url), self.inner.torrent.get())
        {
            let backend = Arc::clone(backend);
            tokio::spawn(async move {
                if let Err(error) = backend.discard(&source, delete_files).await {
                    tracing::warn!(%error, "could not discard the torrent");
                }
            });
            return Vec::new();
        }

        if !delete_files {
            return Vec::new();
        }
        delete_transfer_files(&record, &self.inner.config.load().staging)
    }

    /// Every transfer as the arguments that created it, with enough of what it
    /// became to put an honest row back. What a caller needs to write the list
    /// to disk and rebuild it.
    pub fn specs(&self) -> Vec<Restorable> {
        let records = self.inner.records.lock().unwrap();
        records
            .values()
            .map(|r| Restorable {
                spec: r.spec.clone(),
                state: r.state,
                downloaded: r.progress.downloaded,
                total: r.progress.total,
                name: r.display_name.clone(),
                labels: r.labels.clone(),
            })
            .collect()
    }

    /// Put a transfer back in the state it was in, without starting it.
    ///
    /// Distinct from [`Engine::add`], which always queues. A list restored at
    /// launch has to come back as it was left: a paused transfer that resumed
    /// itself because the app restarted would be the opposite of a pause, and
    /// a finished one must not be fetched again.
    pub fn restore(&self, entry: Restorable) -> DownloadId {
        let Restorable { spec, state, downloaded, total, name, labels } = entry;
        let id = DownloadId(self.inner.next_id.fetch_add(1, Ordering::SeqCst));
        // A finished transfer with no size would render as "0 of 0 bytes", so
        // the figures come back with it rather than being rediscovered.
        let total = total.or_else(|| state.is_terminal().then_some(downloaded));
        {
            let mut records = self.inner.records.lock().unwrap();
            records.insert(
                id,
                Record {
                    spec,
                    state,
                    progress: Progress { downloaded, total, ..Default::default() },
                    lanes: Vec::new(),
                    selector: None,
                    smoothed_rate: None,
                    rate_sampled_at: None,
                    phase: None,
                    torrent_delete: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    chunks: None,
                    torrent: None,
                    display_name: name,
                    error: None,
                    cancel: Cancel::new(),
                    labels,
                },
            );
        }
        // Only a transfer that was running gets to start again; the journal on
        // disk means it picks up rather than restarts.
        if state == State::Queued || state == State::Running {
            self.pump();
        }
        id
    }

    pub fn snapshot(&self) -> Vec<DownloadSnapshot> {
        let records = self.inner.records.lock().unwrap();
        records.iter().map(|(id, record)| record.snapshot(*id)).collect()
    }

    pub fn get(&self, id: DownloadId) -> Option<DownloadSnapshot> {
        let records = self.inner.records.lock().unwrap();
        records.get(&id).map(|r| r.snapshot(id))
    }

    /// Combined throughput across everything running.
    /// Combined throughput across everything running, smoothed.
    ///
    /// The smoothed figure rather than the instantaneous one: this drives the
    /// toolbar readout and the throughput graph, and a raw sum of per-tick
    /// rates made both unreadable: one burst set the graph's axis and flattened
    /// the rest of the trace against it.
    pub fn total_bytes_per_sec(&self) -> u64 {
        let records = self.inner.records.lock().unwrap();
        records
            .values()
            .filter(|r| r.state == State::Running)
            .map(|r| r.progress.smoothed_bytes_per_sec)
            .sum()
    }

    pub fn count_in(&self, state: State) -> usize {
        let records = self.inner.records.lock().unwrap();
        records.values().filter(|r| r.state == state).count()
    }

    /// Start queued downloads until the concurrency limit is reached.
    fn pump(&self) {
        let to_start: Vec<DownloadId> = {
            let records = self.inner.records.lock().unwrap();
            // Seeding is deliberately not counted: it costs upload, not a
            // download slot, and letting it hold one would stall the queue
            // behind torrents that have already finished downloading.
            let running = records.values().filter(|r| r.state == State::Running).count();
            let free = self.inner.config.load().max_concurrent.saturating_sub(running);

            records
                .iter()
                .filter(|(_, r)| r.state == State::Queued)
                .take(free)
                .map(|(id, _)| *id)
                .collect()
        };

        for id in to_start {
            self.start(id);
        }
    }

    fn start(&self, id: DownloadId) {
        let (spec, cancel) = {
            let mut records = self.inner.records.lock().unwrap();
            let Some(record) = records.get_mut(&id) else { return };
            if record.state != State::Queued {
                return;
            }
            record.state = State::Running;
            (record.spec.clone(), record.cancel.clone())
        };

        let engine = self.clone();
        tokio::spawn(async move {
            let outcome = engine.run_with_retries(id, &spec, cancel.clone()).await;
            engine.finish(id, outcome, cancel);
            // A finished download frees a slot for whatever is queued.
            engine.pump();
        });
    }

    /// Run a transfer, retrying a failure the configured number of times.
    ///
    /// Only transport failures are retried. A cancellation is the user's
    /// decision and a failure that invalidates the partial data means the
    /// resource itself changed: repeating either just produces the same
    /// answer more slowly.
    ///
    /// Each attempt resumes from the journal, so a retry costs the bytes in
    /// flight rather than starting again.
    async fn run_with_retries(
        &self,
        id: DownloadId,
        spec: &DownloadSpec,
        cancel: Cancel,
    ) -> Result<()> {
        let attempts = self.inner.config.load().retries.saturating_add(1);
        let mut backoff = Duration::from_millis(500);
        for attempt in 1..=attempts {
            match self.run_one(id, spec, cancel.clone()).await {
                Ok(()) => return Ok(()),
                Err(error) => {
                    let last = attempt == attempts;
                    if last || cancel.is_cancelled() || error.invalidates_partial_data() {
                        return Err(error);
                    }
                    tracing::info!(
                        ?id,
                        attempt,
                        of = attempts,
                        %error,
                        "transfer failed, retrying"
                    );
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                }
            }
        }
        unreachable!("the loop returns on the last attempt")
    }

    async fn run_one(&self, id: DownloadId, spec: &DownloadSpec, cancel: Cancel) -> Result<()> {
        // Read once, at the start: a transfer keeps the settings it began
        // with rather than changing chunk size half way through.
        let config = self.inner.config.load_full();

        match &spec.kind {
            TransferKind::Torrent(source) => {
                return self.run_torrent(id, spec, source.clone(), &config, cancel).await;
            }
            // A magnet with no info hash names nothing to look up. Saying so
            // is the whole point of classifying it separately.
            TransferKind::IncompleteMagnet => {
                return Err(crate::error::Error::InvalidUrl(format!(
                    "{} has no xt=urn:btih: topic, so there is no torrent to find",
                    spec.url
                )));
            }
            TransferKind::Http => {}
        }

        let lanes = self.inner.factory.lanes_for(spec)?;

        let observer = {
            let engine = self.clone();
            Box::new(move |selector: Arc<LaneSelector>| {
                let mut records = engine.inner.records.lock().unwrap();
                if let Some(record) = records.get_mut(&id) {
                    record.selector = Some(selector);
                }
            })
        };

        let phase_observer = {
            let engine = self.clone();
            Box::new(move |phase: Option<&'static str>| {
                let mut records = engine.inner.records.lock().unwrap();
                if let Some(record) = records.get_mut(&id) {
                    record.phase = phase.map(str::to_string);
                }
            })
        };

        let chunk_observer = {
            let engine = self.clone();
            Box::new(move |chunks: Arc<crate::chunks::ChunkProgress>| {
                let mut records = engine.inner.records.lock().unwrap();
                if let Some(record) = records.get_mut(&id) {
                    record.chunks = Some(chunks);
                }
            })
        };

        let engine = self.clone();
        let on_progress = Box::new(move |progress: Progress| {
            let mut records = engine.inner.records.lock().unwrap();
            if let Some(record) = records.get_mut(&id) {
                record.progress = record.advance(progress);
                let http: u64 = records
                    .values()
                    .filter(|r| r.torrent.is_none() && r.state == State::Running)
                    .map(|r| r.progress.smoothed_bytes_per_sec)
                    .sum();
                engine.inner.http_rate.store(http, Ordering::Relaxed);
            }
        });

        let outcome = download_over_lanes(
            lanes.as_ref(),
            &spec.destination,
            ResumeOptions {
                chunk_size: config.chunk_size,
                durability: config.durability,
                staging: config.staging.clone(),
                connections: spec.connections,
                expect: spec.expect.clone(),
                verify_existing: config.verify_existing,
                always_hash: config.verify_every,
                keep_partial: config.keep_partial,
                limit: Some(Arc::clone(&self.inner.budget)),
                lane_limits: lane_limits(lanes.as_ref(), &config.interface_limits),
                cancel,
                on_lanes_ready: Some(observer),
                on_chunks_ready: Some(chunk_observer),
                on_phase: Some(phase_observer),
                ..Default::default()
            },
            Some(on_progress),
        )
        .await?;

        let mut records = self.inner.records.lock().unwrap();
        if let Some(record) = records.get_mut(&id) {
            record.phase = None;
            record.selector = None;
            record.lanes = outcome.lanes;
            record.progress = Progress {
                downloaded: outcome.total,
                total: Some(outcome.total),
                bytes_per_sec: 0,
                smoothed_bytes_per_sec: 0,
            };
        }
        Ok(())
    }

    /// Run a transfer over the torrent backend.
    ///
    /// Shorter than the HTTP path because almost nothing here is ours: the
    /// backend owns pieces, peers and the swarm, and the engine owns only the
    /// record it is updating and the token that stops it.
    async fn run_torrent(
        &self,
        id: DownloadId,
        spec: &DownloadSpec,
        source: TorrentSource,
        config: &EngineConfig,
        cancel: Cancel,
    ) -> Result<()> {
        let Some(backend) = self.inner.torrent.get() else {
            return Err(crate::error::Error::TorrentUnsupported { link: spec.url.clone() });
        };

        let delete_flag = {
            let records = self.inner.records.lock().unwrap();
            match records.get(&id) {
                Some(record) => Arc::clone(&record.torrent_delete),
                None => return Ok(()),
            }
        };
        let config_label = config.unattributed_lane.clone();
        let engine = self.clone();
        let on_progress = Box::new(move |report: TorrentProgress| {
            let mut records = engine.inner.records.lock().unwrap();
            let Some(record) = records.get_mut(&id) else { return };
            record.progress = record.advance(report.progress);
            let rate = record.progress.smoothed_bytes_per_sec;
            // A torrent has no lanes of ours: librqbit owns its sockets: but
            // the sidebar meters, the throughput graph and the combined total
            // are all built from lane reports. Without one, a torrent pulling
            // 12 MB/s leaves the graph flat and the header reading 0 B/s.
            //
            // One lane, labelled with whatever the session actually bound to,
            // or the engine's name for traffic it cannot attribute.
            let label = report.status.interface.clone().unwrap_or_else(|| config_label.clone());
            record.lanes = vec![LaneReport {
                lane: 0,
                label,
                bytes: record.progress.downloaded,
                chunks: 0,
                throughput: Some(rate as f64),
                parked: false,
            }];
            record.phase = report.phase;
            record.torrent = Some(report.status);
            if let Some(name) = report.name {
                record.display_name = Some(name);
            }
            // Only from Running: a pause that landed between two reports must
            // not be undone by the report that was already in flight.
            if report.seeding && record.state == State::Running {
                record.state = State::Seeding;
            }
        });

        let outcome = backend
            .run(TorrentRequest {
                source,
                destination: spec.destination.clone(),
                cancel,
                download_limit: Arc::clone(&self.inner.budget),
                other_traffic: Arc::clone(&self.inner.http_rate),
                upload_limit: Arc::clone(&self.inner.upload_budget),
                delete_files: delete_flag,
                keep_partial: config.keep_partial,
                seed_after_complete: config.seed_after_complete,
                on_progress: Some(on_progress),
            })
            .await?;

        let mut records = self.inner.records.lock().unwrap();
        if let Some(record) = records.get_mut(&id) {
            record.display_name = Some(outcome.name);
            record.progress = Progress {
                downloaded: outcome.total,
                total: Some(outcome.total),
                bytes_per_sec: 0,
                smoothed_bytes_per_sec: 0,
            };
            record.torrent = Some(TorrentStatus {
                uploaded: outcome.uploaded,
                files: outcome.files,
                ..Default::default()
            });
        }
        Ok(())
    }

    fn finish(&self, id: DownloadId, outcome: Result<()>, cancel: Cancel) {
        let mut records = self.inner.records.lock().unwrap();
        let Some(record) = records.get_mut(&id) else { return };

        // Freeze the live statistics before dropping the selector, so a paused
        // or failed row keeps showing which interfaces it was using.
        if let Some(selector) = record.selector.take() {
            let reports = selector.reports();
            if !reports.is_empty() {
                record.lanes = reports;
            }
        }

        match outcome {
            Ok(()) => {
                record.state = State::Complete;
                record.error = None;
            }
            Err(crate::error::Error::Cancelled) => {
                record.state = State::Paused;
            }
            Err(e) => {
                // A pause races the transfer: the task may fail on a cancelled
                // socket rather than at the cancellation check. Reporting that
                // as an error would show a red row for a deliberate pause.
                if cancel.is_cancelled() {
                    record.state = State::Paused;
                } else {
                    record.error = Some(e.to_string());
                    record.state = State::Failed;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_filename_is_taken_from_the_destination() {
        assert_eq!(filename_of(std::path::Path::new("/tmp/a/b.iso")), "b.iso");
    }

    #[test]
    fn a_host_is_extracted_without_a_url_parser() {
        assert_eq!(host_of("https://example.test/a/b.bin?x=1"), "example.test");
        assert_eq!(host_of("http://10.0.0.1:8080/f"), "10.0.0.1:8080");
    }

    #[test]
    fn states_report_terminality() {
        assert!(State::Complete.is_terminal());
        assert!(State::Failed.is_terminal());
        assert!(!State::Paused.is_terminal());
        assert!(!State::Queued.is_terminal());
        // Seeding is still a live transfer. Were it terminal, `pause` would
        // refuse it and there would be no way to stop uploading.
        assert!(!State::Seeding.is_terminal());
        assert_eq!(State::Seeding.as_str(), "seeding");
    }

    #[test]
    fn a_spec_classifies_its_own_url() {
        // Callers build specs in three places; deriving the path here is what
        // stops one of them forgetting and sending a magnet to the HTTP code.
        let http = DownloadSpec::new("https://example.test/a.iso", "/tmp/a.iso");
        assert_eq!(http.kind, TransferKind::Http);
        assert!(http.torrent_source().is_none());

        let magnet = DownloadSpec::new(
            "magnet:?xt=urn:btih:cab507494d02ebb1178b38f2e9d7be299c86b862&dn=Thing",
            "/tmp",
        );
        assert!(matches!(magnet.kind, TransferKind::Torrent(TorrentSource::Magnet(_))));
    }

    #[tokio::test]
    async fn a_magnet_with_no_backend_says_the_feature_is_missing() {
        // The failure mode this replaces: the magnet fell through to the HTTP
        // path and came back as "invalid url", sending people to check a link
        // that was perfectly fine.
        let engine = Engine::new(
            std::sync::Arc::new(NoSources),
            EngineConfig::default(),
            Budget::unlimited(),
        );
        assert!(!engine.has_torrent_backend());
        let id = engine.add(DownloadSpec::new(
            "magnet:?xt=urn:btih:cab507494d02ebb1178b38f2e9d7be299c86b862",
            "/tmp",
        ));

        let snapshot = settled(&engine, id).await;
        assert_eq!(snapshot.state, State::Failed);
        let error = snapshot.error.unwrap_or_default();
        assert!(error.contains("torrent support is not compiled in"), "{error}");
    }

    #[tokio::test]
    async fn an_incomplete_magnet_names_the_missing_topic_rather_than_the_url() {
        // A magnet with no info hash is not a broken URL; there is simply
        // nothing in it to look up, and the message has to say which part.
        let engine = Engine::new(
            std::sync::Arc::new(NoSources),
            EngineConfig::default(),
            Budget::unlimited(),
        );
        engine.set_torrent_backend(std::sync::Arc::new(NeverRuns));
        let id = engine.add(DownloadSpec::new("magnet:?dn=something", "/tmp"));

        let snapshot = settled(&engine, id).await;
        assert_eq!(snapshot.state, State::Failed);
        let error = snapshot.error.unwrap_or_default();
        assert!(error.contains("xt=urn:btih:"), "{error}");
    }

    #[test]
    fn a_torrent_row_is_named_before_its_metadata_arrives() {
        // A magnet resolves in minutes on a cold swarm. Until it does, the
        // destination is a folder, so without this the row is labelled with
        // the download directory's name.
        let engine = Engine::new(
            std::sync::Arc::new(NoSources),
            EngineConfig { max_concurrent: 0, ..Default::default() },
            Budget::unlimited(),
        );
        let id = engine.add(DownloadSpec::new(
            "magnet:?xt=urn:btih:cab507494d02ebb1178b38f2e9d7be299c86b862&dn=Ubuntu+24.04",
            "/tmp/Downloads",
        ));
        assert_eq!(engine.get(id).unwrap().filename, "Ubuntu 24.04");
    }

    #[test]
    fn an_http_snapshot_reports_no_torrent_statistics_at_all() {
        // Zero peers and zero uploaded would read as "we looked and found
        // none", which is a different claim from "this is not a torrent".
        let engine = Engine::new(
            std::sync::Arc::new(NoSources),
            EngineConfig { max_concurrent: 0, ..Default::default() },
            Budget::unlimited(),
        );
        let id = engine.add(DownloadSpec::new("https://example.test/a.iso", "/tmp/a.iso"));
        assert!(engine.get(id).unwrap().torrent.is_none());
    }

    /// Poll until the record leaves the queue and settles, or give up.
    async fn settled(engine: &Engine, id: DownloadId) -> DownloadSnapshot {
        for _ in 0..200 {
            if let Some(snapshot) = engine.get(id)
                && snapshot.state.is_terminal()
            {
                return snapshot;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the transfer never settled");
    }

    /// A factory no torrent test ever reaches: the HTTP path is the one thing
    /// these tests are not exercising.
    struct NoSources;

    impl SourceFactory for NoSources {
        fn lanes_for(&self, _spec: &DownloadSpec) -> Result<Box<dyn LaneSet>> {
            unreachable!("no HTTP transfer is started in these tests")
        }
    }

    /// A backend that is registered but never reached, so the engine's own
    /// rejection is what the test observes.
    struct NeverRuns;

    #[async_trait::async_trait]
    impl crate::torrent::TorrentBackend for NeverRuns {
        async fn discard(
            &self,
            _source: &crate::torrent::TorrentSource,
            _delete_files: bool,
        ) -> crate::Result<()> {
            Ok(())
        }

        async fn run(&self, _request: TorrentRequest) -> Result<crate::torrent::TorrentOutcome> {
            unreachable!("an incomplete magnet must be refused before the backend is asked")
        }
    }

    struct Labels(Vec<String>);

    impl LaneSet for Labels {
        fn len(&self) -> usize {
            self.0.len()
        }
        fn source(&self, _lane: usize) -> &dyn crate::source::ByteSource {
            unreachable!("lane_limits never opens a lane")
        }
        fn label(&self, lane: usize) -> &str {
            &self.0[lane]
        }
    }

    fn lanes(names: &[&str]) -> Labels {
        Labels(names.iter().map(|n| n.to_string()).collect())
    }

    #[test]
    fn no_interface_limits_means_no_per_lane_budgets() {
        // An empty vector is what `ResumeOptions` reads as "unlimited"; a
        // vector of `None` would work too but allocates per transfer.
        let limits = lane_limits(&lanes(&["en0", "en1"]), &BTreeMap::new());
        assert!(limits.is_empty());
    }

    #[test]
    fn a_limit_lands_on_the_lane_it_names_and_no_other() {
        let mut configured = BTreeMap::new();
        configured.insert("en1".to_string(), 5 << 20);

        let limits = lane_limits(&lanes(&["en0", "en1", "en2"]), &configured);
        assert_eq!(limits.len(), 3);
        assert!(limits[0].is_none(), "en0 was not limited");
        assert_eq!(limits[1].as_ref().map(|b| b.rate()), Some(5 << 20));
        assert!(limits[2].is_none(), "en2 was not limited");
    }

    #[test]
    fn a_limit_for_an_absent_interface_is_ignored_rather_than_misapplied() {
        // The tether is unplugged, so its ceiling must not land on whichever
        // lane happens to be at that index.
        let mut configured = BTreeMap::new();
        configured.insert("usb-tether".to_string(), 1 << 20);

        let limits = lane_limits(&lanes(&["en0"]), &configured);
        assert_eq!(limits.len(), 1);
        assert!(limits[0].is_none());
    }

    #[test]
    fn each_transfer_gets_its_own_budget_for_a_limited_interface() {
        // Two downloads over one metered link should each be held to the
        // ceiling rather than sharing one allowance between them.
        let mut configured = BTreeMap::new();
        configured.insert("en0".to_string(), 5 << 20);

        let first = lane_limits(&lanes(&["en0"]), &configured);
        let second = lane_limits(&lanes(&["en0"]), &configured);
        assert!(!Arc::ptr_eq(first[0].as_ref().unwrap(), second[0].as_ref().unwrap()));
    }

    fn record_at(rate: u64) -> Progress {
        Progress { downloaded: 0, total: Some(1 << 30), bytes_per_sec: rate, ..Default::default() }
    }

    /// As the engine does it: smooth, then store. The clock is handed in so a
    /// span is exact rather than however long a sleep really took.
    fn feed_at(record: &mut Record, rate: u64, now: Instant) -> u64 {
        let progress = record.smooth_at(record_at(rate), now);
        record.progress = progress;
        record.progress.smoothed_bytes_per_sec
    }

    fn blank_record() -> Record {
        Record {
            spec: DownloadSpec::new("http://x/y", "/tmp/y"),
            state: State::Running,
            progress: Progress::default(),
            lanes: Vec::new(),
            selector: None,
            smoothed_rate: None,
            rate_sampled_at: None,
            phase: None,
            torrent_delete: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            chunks: None,
            torrent: None,
            display_name: None,
            error: None,
            cancel: Cancel::new(),
            labels: BTreeMap::new(),
        }
    }

    #[test]
    fn the_first_sample_is_taken_as_it_stands() {
        // Seeding the average from zero would make every transfer open
        // reporting nothing for the first second and a half.
        let mut record = blank_record();
        assert_eq!(feed_at(&mut record, 8_000_000, Instant::now()), 8_000_000);
    }

    #[test]
    fn a_burst_moves_the_average_a_fraction_of_the_way_not_all_of_it() {
        // Without this, one spike sets the throughput graph's axis and every
        // sample after it draws as a flat line near zero.
        let mut record = blank_record();
        let start = Instant::now();
        feed_at(&mut record, 10_000_000, start);
        let moved = feed_at(&mut record, 300_000_000, start + Duration::from_millis(100));
        assert!(moved > 10_000_000, "the average must follow the sample at all");
        assert!(
            moved < 60_000_000,
            "a single 300 MB/s burst moved the average to {moved}; it should barely register"
        );
    }

    #[test]
    fn the_average_converges_on_a_steady_rate() {
        // Smoothing that never arrives is as wrong as no smoothing.
        let mut record = blank_record();
        let start = Instant::now();
        feed_at(&mut record, 0, start);
        let mut settled = 0;
        for tick in 1..=40 {
            settled = feed_at(&mut record, 20_000_000, start + Duration::from_millis(100 * tick));
        }
        // Four seconds is a shade under three time constants, so ~93% of the
        // way. Asserting any closer would be asserting the clock, not the
        // filter.
        assert!(
            settled > 18_000_000 && settled <= 20_000_000,
            "settled at {settled} rather than near 20 MB/s"
        );
    }

    #[test]
    fn the_smoothing_span_does_not_depend_on_how_often_reports_arrive() {
        // The HTTP path reports every 100ms and the torrent backend every
        // 250ms. A fixed weight per sample would smooth them over different
        // spans and make the two behave differently on the same graph.
        let mut fast = blank_record();
        let mut slow = blank_record();
        // The same half second either way, so any difference is the filter's
        // and not the clock's.
        let start = Instant::now();
        let mut a = 0.0;
        feed_at(&mut fast, 0, start);
        for tick in 1..=10 {
            a = feed_at(&mut fast, 20_000_000, start + Duration::from_millis(50 * tick)) as f64;
        }

        let mut b = 0.0;
        feed_at(&mut slow, 0, start);
        for tick in 1..=2 {
            b = feed_at(&mut slow, 20_000_000, start + Duration::from_millis(250 * tick)) as f64;
        }
        assert!((a - b).abs() / a.max(b) < 0.01, "after the same half second: fast {a}, slow {b}");
    }

    fn touch(path: &std::path::Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, b"x").unwrap();
    }

    fn record_for(destination: PathBuf) -> Record {
        let mut record = blank_record();
        record.spec = DownloadSpec::new("http://x/y", destination);
        record
    }

    #[test]
    fn deleting_an_http_transfer_takes_the_file_and_both_sidecars() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("a.iso");
        touch(&dest);
        touch(&dir.path().join("a.iso.part"));
        touch(&dir.path().join("a.iso.dlmeta"));
        let neighbour = dir.path().join("untouched.iso");
        touch(&neighbour);

        let removed = delete_transfer_files(&record_for(dest.clone()), &Default::default());
        assert_eq!(removed.len(), 3, "{removed:?}");
        assert!(!dest.exists());
        assert!(!dir.path().join("a.iso.part").exists());
        assert!(!dir.path().join("a.iso.dlmeta").exists());
        assert!(neighbour.exists(), "a file this transfer never wrote was deleted");
    }

    #[test]
    fn a_torrent_leaves_its_contents_to_the_backend() {
        // librqbit wrote those files and is the only thing that knows exactly
        // which they are. The engine reconstructing them from a folder name
        // the *link* supplied is what left a cancelled torrent's contents on
        // disk after the user asked for them to go.
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("Some Release");
        touch(&folder.join("a.bin"));

        let mut record = record_for(folder.clone());
        record.torrent = Some(TorrentStatus {
            files: vec![crate::torrent::TorrentFile {
                path: "a.bin".into(),
                len: 1,
                downloaded: 1,
            }],
            ..Default::default()
        });

        let removed = delete_transfer_files(&record, &Default::default());
        assert!(removed.is_empty(), "the engine must not race the backend: {removed:?}");
        assert!(folder.join("a.bin").exists());
    }

    #[tokio::test]
    async fn the_delete_flag_reaches_the_backend_before_the_cancel() {
        // Raised one tick late, the backend pauses and keeps everything.
        let engine = Engine::new(Arc::new(NoFactory), EngineConfig::default(), Budget::unlimited());
        let id = engine.add(DownloadSpec::new("magnet:?xt=urn:btih:abc", "/tmp/x"));
        let flag = {
            let records = engine.inner.records.lock().unwrap();
            Arc::clone(&records.get(&id).unwrap().torrent_delete)
        };
        assert!(!flag.load(std::sync::atomic::Ordering::SeqCst));
        engine.remove_with_files(id, true);
        assert!(flag.load(std::sync::atomic::Ordering::SeqCst), "the backend was never told");
    }

    #[test]
    fn a_destination_that_is_a_directory_is_never_removed_for_a_plain_download() {
        // Without a torrent file list there is nothing naming what is inside,
        // and a recursive delete driven by a name the link supplied is how a
        // download manager empties someone's Downloads folder.
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("Downloads");
        touch(&folder.join("everything-they-own.zip"));

        delete_transfer_files(&record_for(folder.clone()), &Default::default());
        assert!(folder.join("everything-they-own.zip").exists());
        assert!(folder.exists());
    }

    #[tokio::test]
    async fn removing_without_deleting_leaves_every_file_alone() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("a.iso");
        touch(&dest);
        touch(&dir.path().join("a.iso.part"));

        let engine = Engine::new(Arc::new(NoFactory), EngineConfig::default(), Budget::unlimited());
        let id = engine.add(DownloadSpec::new("http://x/a.iso", &dest));
        assert!(engine.remove_with_files(id, false).is_empty());
        assert!(dest.exists());
        assert!(dir.path().join("a.iso.part").exists());
    }

    #[tokio::test]
    async fn a_restored_transfer_keeps_the_state_it_was_left_in() {
        let engine = Engine::new(Arc::new(NoFactory), EngineConfig::default(), Budget::unlimited());
        let id = engine.restore(Restorable {
            spec: DownloadSpec::new("http://x/a.iso", "/tmp/a.iso"),
            state: State::Paused,
            downloaded: 1024,
            total: Some(4096),
            name: None,
            labels: BTreeMap::new(),
        });
        let row = engine.get(id).expect("restored");
        assert_eq!(row.state, State::Paused);
        assert_eq!(row.progress.downloaded, 1024);
        assert_eq!(row.progress.total, Some(4096));
    }

    #[tokio::test]
    async fn the_url_comes_back_out_so_a_magnet_can_be_identified_before_it_starts() {
        // A magnet carries its own info hash. Until the backend has joined the
        // swarm that URL is the only place it exists, and a client that just
        // added a torrent asks for it by hash immediately.
        let engine = Engine::new(Arc::new(NoFactory), EngineConfig::default(), Budget::unlimited());
        let magnet = "magnet:?xt=urn:btih:2c6b6858d61da9543d4231a71db4b1c9264b0685";
        let id = engine.add(DownloadSpec::new(magnet, "/tmp/x"));
        assert_eq!(engine.url(id).as_deref(), Some(magnet));
        assert_eq!(engine.url(DownloadId(9999)), None);
    }

    #[tokio::test]
    async fn a_label_set_after_a_transfer_starts_reaches_the_record_that_is_saved() {
        // A category is chosen by whoever asked for the download, which is
        // after `add` has returned. Before this the only way in was `restore`,
        // so a label learned later was dropped at the next restart: the very
        // moment a client comes back looking for it by category.
        let engine = Engine::new(Arc::new(NoFactory), EngineConfig::default(), Budget::unlimited());
        let id = engine.add(DownloadSpec::new("http://x/a.iso", "/tmp/a.iso"));
        engine.set_labels(id, BTreeMap::from([("category".to_string(), "tv-sonarr".to_string())]));

        let saved = engine.specs();
        let entry = saved.first().expect("the transfer should be in what gets written out");
        assert_eq!(entry.labels.get("category").map(String::as_str), Some("tv-sonarr"));
    }

    #[tokio::test]
    async fn labelling_a_transfer_that_is_gone_is_ignored_rather_than_a_panic() {
        // The caller races removal: a client can categorise something it has
        // just deleted, and that is not an error worth taking the server down
        // for.
        let engine = Engine::new(Arc::new(NoFactory), EngineConfig::default(), Budget::unlimited());
        engine.set_labels(DownloadId(9999), BTreeMap::from([("a".to_string(), "b".to_string())]));
        assert!(engine.specs().is_empty());
    }

    #[tokio::test]
    async fn a_label_written_can_be_read_back_by_id() {
        // `set_labels` predates this and is write-only: a category survives a
        // restart through `specs()`, which has no id to match one against.
        // Reading one transfer's labels back by id is what the compatible
        // API's listing needs in order to show the category it was given.
        let engine = Engine::new(Arc::new(NoFactory), EngineConfig::default(), Budget::unlimited());
        let id = engine.add(DownloadSpec::new("http://x/a.iso", "/tmp/a.iso"));
        engine.set_labels(id, BTreeMap::from([("category".to_string(), "tv-sonarr".to_string())]));
        assert_eq!(engine.labels(id).get("category").map(String::as_str), Some("tv-sonarr"));
    }

    #[test]
    fn labels_for_an_unknown_transfer_are_empty_rather_than_a_panic() {
        let engine = Engine::new(Arc::new(NoFactory), EngineConfig::default(), Budget::unlimited());
        assert!(engine.labels(DownloadId(9999)).is_empty());
    }

    #[tokio::test]
    async fn a_transfers_destination_can_be_read_back_by_id() {
        // `save_path` and `content_path` in the compatible API are answers to
        // a question nothing before it ever needed to ask: where a transfer,
        // already running, is writing its bytes.
        let engine = Engine::new(Arc::new(NoFactory), EngineConfig::default(), Budget::unlimited());
        let id = engine.add(DownloadSpec::new("http://x/a.iso", "/tmp/somewhere/a.iso"));
        assert_eq!(engine.destination(id), Some(PathBuf::from("/tmp/somewhere/a.iso")));
    }

    #[test]
    fn the_destination_of_an_unknown_transfer_is_none() {
        let engine = Engine::new(Arc::new(NoFactory), EngineConfig::default(), Budget::unlimited());
        assert_eq!(engine.destination(DownloadId(9999)), None);
    }

    #[tokio::test]
    async fn a_finished_transfer_comes_back_at_its_full_size() {
        // With no size it would read as "0 of 0 bytes", which is what a
        // restored row looked like before the figures travelled with it.
        let engine = Engine::new(Arc::new(NoFactory), EngineConfig::default(), Budget::unlimited());
        let id = engine.restore(Restorable {
            spec: DownloadSpec::new("http://x/a.iso", "/tmp/a.iso"),
            state: State::Complete,
            downloaded: 4096,
            total: None,
            name: None,
            labels: BTreeMap::new(),
        });
        let row = engine.get(id).expect("restored");
        assert_eq!(row.progress.fraction(), Some(1.0));
    }

    #[tokio::test]
    async fn what_is_written_is_what_comes_back() {
        let engine = Engine::new(Arc::new(NoFactory), EngineConfig::default(), Budget::unlimited());
        let mut spec = DownloadSpec::new("http://x/a.iso", "/tmp/a.iso");
        spec.connections = 6;
        engine.restore(Restorable {
            spec: spec.clone(),
            state: State::Paused,
            downloaded: 7,
            total: Some(9),
            name: Some("a.iso".into()),
            labels: BTreeMap::new(),
        });

        let written = engine.specs();
        assert_eq!(written.len(), 1);
        assert_eq!(written[0].spec.connections, 6);
        assert_eq!(written[0].state, State::Paused);
        assert_eq!((written[0].downloaded, written[0].total), (7, Some(9)));
        assert_eq!(written[0].name.as_deref(), Some("a.iso"));
    }

    #[tokio::test]
    async fn progress_never_steps_backwards() {
        // A retry restarts from the journal, so bytes fetched but not yet
        // journalled are counted and then uncounted. The file on disk never
        // shrinks, and a bar that retreats reads as corruption.
        let mut record = blank_record();
        record.progress = Progress { downloaded: 5_000_000, ..Default::default() };
        let after = record.advance(Progress { downloaded: 4_000_000, ..Default::default() });
        assert_eq!(after.downloaded, 5_000_000);
    }

    #[tokio::test]
    async fn progress_still_moves_forward() {
        let mut record = blank_record();
        record.progress = Progress { downloaded: 5_000_000, ..Default::default() };
        let after = record.advance(Progress { downloaded: 6_000_000, ..Default::default() });
        assert_eq!(after.downloaded, 6_000_000);
    }

    struct NoFactory;

    impl SourceFactory for NoFactory {
        fn lanes_for(&self, _spec: &DownloadSpec) -> crate::Result<Box<dyn LaneSet>> {
            Err(crate::error::Error::NoRouteAvailable)
        }
    }
}
