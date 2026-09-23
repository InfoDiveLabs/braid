//! BitTorrent transfers, over librqbit.
//!
//! The only crate in the workspace that knows what a piece or a peer is.
//! `dl-core` reaches it through [`dl_core::TorrentBackend`] and nothing else,
//! so a build without this crate links no BitTorrent code and the engine says
//! so rather than failing somewhere confusing.
//!
//! **What is deliberately absent**: no content index, no search, and no
//! bundled tracker list. A client finds the swarm the link names. DHT
//! bootstrap nodes ship with librqbit and are kept: they are the address book
//! for the distributed hash table and carry no content of their own.
//!
//! **Seeding is publication.** Joining a swarm announces this machine's
//! address to every peer and tracker in it. That is how the protocol works and
//! there is no setting here that changes it; see `README.md`.

use dl_core::budget::Budget;
use dl_core::error::{Error, Result};
use dl_core::model::Progress;
use dl_core::torrent::{
    TorrentBackend, TorrentFile, TorrentOutcome, TorrentPeer, TorrentProgress, TorrentRequest,
    TorrentSource, TorrentStatus,
};
use librqbit::{
    AddTorrent, AddTorrentOptions, ManagedTorrent, ManagedTorrentState, Session, SessionOptions,
    TorrentStats, TorrentStatsState,
};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// How often the swarm is asked what it has done.
///
/// Four a second: fast enough that the UI's own 10 Hz poll always has a fresh
/// figure to read, slow enough that taking librqbit's stats lock is not itself
/// the work. The rate limits are pushed on the same beat, so a schedule change
/// reaches the swarm within one tick.
const POLL: Duration = Duration::from_millis(250);

/// How the session is built. Everything here is a decision the app makes once.
#[derive(Clone, Debug)]
pub struct SessionConfig {
    /// Where torrents land when a request does not say. A request always says,
    /// so this is only librqbit's own fallback.
    pub default_folder: PathBuf,
    /// Bind every socket: peers, DHT, trackers: to this network device.
    ///
    /// The same interface pinning the HTTP path does, except that a torrent
    /// cannot be split across interfaces: a swarm sees one address. Binding is
    /// a routing choice, **not** a privacy control.
    pub bind_device: Option<String>,
    /// The port to accept incoming peers on. `None` lets the OS choose one.
    ///
    /// A listener is always opened: a client that only dials out can reach
    /// nobody who is behind a NAT of their own, which on a small swarm is
    /// most of it. Without a forwarded or mapped port the ephemeral one is
    /// still reachable from peers on the same network and from anyone the
    /// router's NAT traversal happens to let through.
    pub listen_port: Option<u16>,
    /// Turn off the distributed hash table. Magnet links need it.
    pub disable_dht: bool,
    /// Turn off tracker announces.
    pub disable_trackers: bool,
    /// Turn off local peer discovery multicast.
    pub disable_local_discovery: bool,
    /// Peers to try before any discovery happens.
    ///
    /// Empty in normal use. The in-process test uses it to reach a seeder on
    /// loopback with the DHT and trackers switched off, which is the only way
    /// to exercise a real swarm without touching the public network.
    pub initial_peers: Vec<SocketAddr>,
}

impl SessionConfig {
    pub fn new(default_folder: impl Into<PathBuf>) -> Self {
        Self {
            default_folder: default_folder.into(),
            bind_device: None,
            listen_port: None,
            disable_dht: false,
            disable_trackers: false,
            disable_local_discovery: false,
            initial_peers: Vec::new(),
        }
    }
}

/// The torrent backend the engine talks to.
///
/// One librqbit session, built on first use and kept for the life of the app.
/// It has to outlive any single transfer: a paused torrent stays registered
/// with the session, which is what makes resuming cost the pieces in flight
/// rather than the whole file.
pub struct LibrqbitBackend {
    config: SessionConfig,
    session: tokio::sync::OnceCell<Arc<Session>>,
    /// Which torrent id each source became, so a transfer can be discarded
    /// after its loop has ended. librqbit keeps a torrent registered until it
    /// is told otherwise, and a re-added link finds it and carries on.
    known: std::sync::Mutex<std::collections::HashMap<String, usize>>,
}

impl LibrqbitBackend {
    pub fn new(config: SessionConfig) -> Self {
        Self { config, session: tokio::sync::OnceCell::new(), known: Default::default() }
    }

    /// The session, building it if this is the first transfer.
    ///
    /// Lazy because building one opens sockets and starts the DHT, and an app
    /// that never opens a torrent should not be announcing itself to anything.
    pub async fn session(&self) -> Result<&Arc<Session>> {
        self.session
            .get_or_try_init(|| async { build_session(&self.config).await })
            .await
            .map_err(|e| Error::Torrent(format!("could not start the torrent session: {e:#}")))
    }

    /// Stop the session and everything in it.
    pub async fn shutdown(&self) {
        if let Some(session) = self.session.get() {
            session.stop().await;
        }
    }
}

async fn build_session(config: &SessionConfig) -> anyhow::Result<Arc<Session>> {
    let listen = librqbit::ListenerOptions {
        listen_addr: (std::net::Ipv6Addr::UNSPECIFIED, config.listen_port.unwrap_or(0)).into(),
        ..Default::default()
    };

    let options = SessionOptions {
        // Kept: the DHT is how a magnet link finds anyone at all.
        dht: (!config.disable_dht).then(librqbit::DhtSessionConfig::default),
        bind_device_name: config.bind_device.clone(),
        disable_trackers: config.disable_trackers,
        disable_local_service_discovery: config.disable_local_discovery,
        listen: Some(listen),
        // Empty, and it stays empty. Shipping a tracker list would make this a
        // directory of where to find things rather than a client.
        trackers: Default::default(),
        // The engine keeps the transfer list; a second one on disk here would
        // resurrect torrents the user removed.
        persistence: None,
        ..Default::default()
    };

    Session::new_with_opts(config.default_folder.clone(), options).await
}

#[async_trait::async_trait]
impl TorrentBackend for LibrqbitBackend {
    async fn discard(&self, source: &TorrentSource, delete_files: bool) -> Result<()> {
        let id = self.known.lock().ok().and_then(|mut known| known.remove(&source.key()));
        let (Some(id), Some(session)) = (id, self.session.get()) else {
            // Never started, or already gone. Nothing registered means nothing
            // to delete, and inventing a path to remove would be worse.
            return Ok(());
        };
        session
            .delete(id.into(), delete_files)
            .await
            .map_err(|error| Error::Torrent(error.to_string()))
    }

    async fn run(&self, request: TorrentRequest) -> Result<TorrentOutcome> {
        // Read before the session is built, not after: starting a session
        // opens a peer socket and joins the DHT, and doing that only to
        // discover the `.torrent` is not on disk announces this machine to
        // the network for a transfer that was never going to happen.
        let add = read_source(&request.source).await?;
        let session = self.session().await?;

        // Before the torrent is added, not after: the ceilings live on the
        // session, so setting them first means they are in force when the
        // first byte moves rather than a quarter of a second later.
        let mut rates = AppliedRates::default();
        rates.apply(
            session,
            &request.download_limit,
            &request.upload_limit,
            &request.other_traffic,
        );

        let handle = self.add(session, add, &request).await?;
        // Remembered so `discard` can reach this torrent after the loop below
        // has returned. A finished transfer has no loop to cancel.
        if let Ok(mut known) = self.known.lock() {
            known.insert(request.source.key(), handle.id());
        }

        // A resume finds the torrent already registered and paused: the
        // session outlives any one transfer precisely so this costs the pieces
        // that were in flight rather than the whole file.
        if handle.is_paused()
            && let Err(error) = session.unpause(&handle).await
        {
            return Err(Error::Torrent(format!("could not restart the torrent: {error:#}")));
        }

        let outcome = self.watch(session, &handle, &request, rates).await;

        // A cancellation is a pause, and a pause keeps everything: which is
        // why this looks at the error rather than only at `keep_partial`.
        // Deleting here would make pausing a torrent throw the download away.
        if matches!(&outcome, Err(error) if !request.keep_partial && !matches!(error, Error::Cancelled))
            && let Err(error) = session.delete(handle.id().into(), true).await
        {
            tracing::warn!(%error, "could not discard the partial torrent");
        }
        outcome
    }
}

impl LibrqbitBackend {
    async fn add(
        &self,
        session: &Arc<Session>,
        add: AddTorrent<'static>,
        request: &TorrentRequest,
    ) -> Result<Arc<ManagedTorrent>> {
        let options = AddTorrentOptions {
            // Required to resume: without it librqbit refuses to write over
            // the pieces it wrote last time, so every pause would cost the
            // whole file.
            overwrite: true,
            output_folder: Some(request.destination.to_string_lossy().into_owned()),
            initial_peers: (!self.config.initial_peers.is_empty())
                .then(|| self.config.initial_peers.clone()),
            ..Default::default()
        };

        // A metadata error, not a transfer error: whatever is wrong with the
        // link or the file will still be wrong on the next attempt, and
        // retrying it burns the whole budget on the same bytes.
        session
            .add_torrent(add, Some(options))
            .await
            .map_err(|e| Error::TorrentMetadata(format!("{e:#}")))?
            .into_handle()
            .ok_or_else(|| {
                Error::TorrentMetadata("the session listed the torrent instead of adding it".into())
            })
    }

    /// Poll the torrent until it finishes, fails, or is cancelled.
    ///
    /// Also the only place the rate limits are applied, which is why they are
    /// pushed every tick rather than once: the Bandwidth schedule moves the
    /// budget under a running transfer, and a limit that only took effect on
    /// the next torrent would be a limit that does nothing this evening.
    async fn watch(
        &self,
        session: &Arc<Session>,
        handle: &Arc<ManagedTorrent>,
        request: &TorrentRequest,
        mut rates: AppliedRates,
    ) -> Result<TorrentOutcome> {
        let mut ticker = tokio::time::interval(POLL);
        let mut meter = Meter::default();
        let mut seeding = false;
        // Carried across a tick where the torrent is not live, so pausing and
        // resuming does not read as a burst of traffic that never happened.
        let mut last_fetched = 0u64;

        loop {
            ticker.tick().await;

            rates.apply(
                session,
                &request.download_limit,
                &request.upload_limit,
                &request.other_traffic,
            );

            if request.cancel.is_cancelled() {
                if request.delete_files.load(Ordering::SeqCst) {
                    // librqbit removes exactly the files it wrote. The engine
                    // cannot: a single-file torrent lands under a folder whose
                    // name came from the link, and reconstructing the paths
                    // from that left the contents behind.
                    let id = handle.id();
                    if let Err(error) = session.delete(id.into(), true).await {
                        tracing::warn!(%error, "could not delete the torrent's files");
                    }
                    return Err(Error::Cancelled);
                }
                // Pause rather than delete: the pieces on disk stay valid and
                // the torrent stays registered, so resuming costs the pieces
                // that were in flight and nothing else.
                pause_quietly(session, handle).await;
                return Err(Error::Cancelled);
            }

            let stats = handle.stats();
            if let TorrentStatsState::Error = stats.state {
                let detail = stats.error.unwrap_or_else(|| "the torrent stopped".into());
                return Err(Error::Torrent(detail));
            }

            let name = handle.name();
            let files = file_list(handle, &stats);
            // Bytes pulled from peers, not bytes verified. `progress_bytes`
            // also climbs while a resumed torrent re-checks what is already on
            // disk, and metering that reported the disk read as download speed
            //: hundreds of MB/s that never touched the network.
            let fetched =
                stats.live.as_ref().map(|live| live.snapshot.fetched_bytes).unwrap_or(last_fetched);
            last_fetched = fetched;
            let sample = meter.sample(fetched, stats.uploaded_bytes);

            // What the torrent is doing when it is not simply downloading, so
            // a long re-check reads as work rather than as a stall.
            let phase = match stats.state {
                TorrentStatsState::Initializing { .. } => Some("Checking".to_string()),
                _ => None,
            };

            if stats.finished && !seeding {
                seeding = true;
                if !request.seed_after_complete {
                    pause_quietly(session, handle).await;
                    return Ok(TorrentOutcome {
                        total: stats.total_bytes,
                        uploaded: stats.uploaded_bytes,
                        name: name.unwrap_or_default(),
                        files,
                    });
                }
                tracing::info!(torrent = ?name, "download complete; seeding");
            }

            if let Some(on_progress) = &request.on_progress {
                on_progress(TorrentProgress {
                    progress: Progress {
                        downloaded: stats.progress_bytes,
                        total: Some(stats.total_bytes),
                        bytes_per_sec: sample.down,
                        smoothed_bytes_per_sec: 0,
                    },
                    status: TorrentStatus {
                        uploaded: stats.uploaded_bytes,
                        upload_bytes_per_sec: sample.up,
                        peers: live_peers(&stats),
                        files,
                        peer_list: peer_list(handle),
                        interface: self.config.bind_device.clone(),
                    },
                    name,
                    seeding,
                    phase,
                });
            }
        }
    }
}

/// Turn a source into something librqbit can be handed.
///
/// The `.torrent` on disk is read here rather than inside the session, so a
/// path that is wrong fails as an i/o error naming the file.
async fn read_source(source: &TorrentSource) -> Result<AddTorrent<'static>> {
    Ok(match source {
        TorrentSource::Magnet(uri) => AddTorrent::from_url(uri.clone()),
        TorrentSource::Url(url) => AddTorrent::from_url(url.clone()),
        TorrentSource::File(path) => {
            let bytes = tokio::fs::read(path).await.map_err(|e| Error::io(path.display(), e))?;
            AddTorrent::from_bytes(bytes)
        }
    })
}

/// A pause the caller cannot act on: the transfer is ending either way, and a
/// torrent that was already paused reports that as an error.
async fn pause_quietly(session: &Arc<Session>, handle: &Arc<ManagedTorrent>) {
    if let Err(error) = session.pause(handle).await {
        tracing::debug!(%error, "torrent was already stopped");
    }
}

/// Peers actually connected, not peers heard about.
///
/// The distinction matters in the row: a torrent that has *seen* four hundred
/// peers and connected to none is stalled, and reporting the larger number
/// would draw it as healthy.
fn live_peers(stats: &TorrentStats) -> u32 {
    stats.live.as_ref().map(|l| l.snapshot.peer_stats.live).unwrap_or(0)
}

/// The most active connected peers, for the Inspector.
///
/// Capped: a healthy swarm is hundreds of peers and the panel shows a window
/// onto it, not the whole list. Sorted by bytes fetched so the window is the
/// part worth looking at: the peers actually feeding this transfer: rather
/// than whichever ones the hash map happened to yield first.
fn peer_list(handle: &Arc<ManagedTorrent>) -> Vec<TorrentPeer> {
    const SHOWN: usize = 64;

    let snapshot = handle.with_state(|state| match state {
        ManagedTorrentState::Live(live) => {
            // `Default::default()` rather than naming `PeerStatsFilter`: the
            // type is only exported behind librqbit's `http-api` features,
            // which we deliberately do not enable, and inference reaches it
            // through the signature without the path being public.
            Some(live.per_peer_stats_snapshot(Default::default()))
        }
        // Paused, initializing or errored: there are no peers to report, and
        // the last list would be a lie about a connection that is gone.
        _ => None,
    });
    let Some(snapshot) = snapshot else { return Vec::new() };

    let mut peers: Vec<TorrentPeer> = snapshot
        .peers
        .into_iter()
        .map(|(address, stats)| TorrentPeer {
            address,
            client: stats.client_name.clone(),
            downloaded: stats.counters.fetched_bytes,
            uploaded: stats.counters.uploaded_bytes,
            state: stats.state.to_string(),
        })
        .collect();
    peers.sort_by(|a, b| b.downloaded.cmp(&a.downloaded).then_with(|| a.address.cmp(&b.address)));
    peers.truncate(SHOWN);
    peers
}

fn file_list(handle: &Arc<ManagedTorrent>, stats: &TorrentStats) -> Vec<TorrentFile> {
    handle
        .with_metadata(|metadata| {
            metadata
                .file_infos
                .iter()
                .enumerate()
                .map(|(index, info)| TorrentFile {
                    path: info
                        .relative_filename
                        .components()
                        .map(|c| c.as_os_str().to_string_lossy())
                        .collect::<Vec<_>>()
                        .join("/"),
                    len: info.len,
                    downloaded: stats.file_progress.get(index).copied().unwrap_or(0),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The rates last pushed into the session.
///
/// Compared before writing because librqbit rebuilds its token bucket on every
/// set, and rebuilding it four times a second would hand out a fresh burst
/// each time and quietly defeat the limit.
#[derive(Default, PartialEq, Eq)]
struct AppliedRates {
    down: Option<u64>,
    up: Option<u64>,
}

impl AppliedRates {
    fn apply(
        &mut self,
        session: &Arc<Session>,
        download: &Arc<Budget>,
        upload: &Arc<Budget>,
        other_traffic: &std::sync::atomic::AtomicU64,
    ) {
        let down = headroom(download.rate(), other_traffic.load(Ordering::Relaxed));
        let up = upload.rate();
        if self.down != Some(down) {
            session.ratelimits.set_download_bps(as_bps(down));
            self.down = Some(down);
        }
        if self.up != Some(up) {
            session.ratelimits.set_upload_bps(as_bps(up));
            self.up = Some(up);
        }
    }
}

/// What is left of a shared ceiling once the other transfers have taken their
/// share.
///
/// librqbit enforces its own token bucket, so handing it the whole limit means
/// the HTTP path and this one each honour it separately and together reach
/// twice it.
///
/// Never returns zero for a limit that was set: zero is the unlimited sentinel
/// everywhere in this codebase, so a saturated HTTP download would turn the
/// torrent limit off rather than down.
fn headroom(limit: u64, other: u64) -> u64 {
    const FLOOR: u64 = 16 << 10;
    if limit == 0 {
        return 0;
    }
    limit.saturating_sub(other).max(FLOOR)
}

/// A [`Budget`] rate as librqbit wants it: `None` for unlimited, and clamped
/// to what its counter can hold rather than wrapping to a tiny limit.
fn as_bps(bytes_per_sec: u64) -> Option<std::num::NonZeroU32> {
    std::num::NonZeroU32::new(bytes_per_sec.min(u32::MAX as u64) as u32)
}

/// Throughput, measured here rather than read from librqbit.
///
/// Its own estimator reports megabits and rounds through an `f64`, so a slow
/// transfer reads as zero. The engine's figure has to agree with the HTTP
/// path's, which is bytes since the last sample over the time since the last
/// sample.
#[derive(Default)]
struct Meter {
    last: Option<(Instant, u64, u64)>,
}

struct Sample {
    down: u64,
    up: u64,
}

impl Meter {
    fn sample(&mut self, downloaded: u64, uploaded: u64) -> Sample {
        let now = Instant::now();
        let Some((then, was_down, was_up)) = self.last.replace((now, downloaded, uploaded)) else {
            return Sample { down: 0, up: 0 };
        };
        let elapsed = now.duration_since(then).as_secs_f64();
        if elapsed <= 0.0 {
            return Sample { down: 0, up: 0 };
        }
        let rate = |now: u64, before: u64| (now.saturating_sub(before) as f64 / elapsed) as u64;
        Sample { down: rate(downloaded, was_down), up: rate(uploaded, was_up) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unlimited_budget_means_no_limiter_rather_than_a_limit_of_zero() {
        // Zero is "unlimited" throughout this codebase and `NonZeroU32::new`
        // agrees by accident; asserting it so a future refactor cannot turn
        // unlimited into a total stall.
        assert_eq!(as_bps(0), None);
        assert_eq!(as_bps(1024).map(|v| v.get()), Some(1024));
    }

    #[test]
    fn a_rate_larger_than_the_counter_clamps_instead_of_wrapping() {
        // A 5 GB/s ceiling truncated to u32 becomes about 1 GB/s, which is a
        // limit nobody asked for. Clamping is at least honest about the cap.
        assert_eq!(as_bps(u64::MAX).map(|v| v.get()), Some(u32::MAX));
    }

    #[test]
    fn throughput_is_zero_until_there_are_two_samples() {
        // One reading is a total, not a rate. Dividing it by the time since
        // process start would draw a spike on the first tick of every
        // transfer.
        let mut meter = Meter::default();
        let first = meter.sample(1_000, 10);
        assert_eq!((first.down, first.up), (0, 0));
    }

    #[test]
    fn a_counter_that_goes_backwards_reads_as_idle_rather_than_enormous() {
        // A re-check after a resume can lower `progress_bytes`. An unsigned
        // subtraction there would report several exabytes a second.
        let mut meter = Meter::default();
        meter.sample(1_000, 100);
        let second = meter.sample(500, 50);
        assert_eq!((second.down, second.up), (0, 0));
    }

    #[test]
    fn a_torrent_with_no_live_state_reports_no_peers() {
        // `stats.live` is None while the metadata is still resolving, which is
        // exactly when a magnet row is most tempting to fill with guesses.
        let stats = TorrentStats {
            state: TorrentStatsState::Paused,
            file_progress: Vec::new(),
            error: None,
            progress_bytes: 0,
            uploaded_bytes: 0,
            total_bytes: 0,
            finished: false,
            live: None,
        };
        assert_eq!(live_peers(&stats), 0);
    }
}

#[cfg(test)]
mod rate_tests {
    use super::headroom;

    #[test]
    fn an_unlimited_cap_stays_unlimited() {
        assert_eq!(headroom(0, 0), 0);
        assert_eq!(headroom(0, 50 << 20), 0);
    }

    #[test]
    fn the_torrent_takes_what_http_is_not_using() {
        assert_eq!(headroom(10 << 20, 4 << 20), 6 << 20);
        assert_eq!(headroom(10 << 20, 0), 10 << 20);
    }

    #[test]
    fn a_saturated_http_path_does_not_turn_the_torrent_limit_off() {
        // Zero is the unlimited sentinel, so subtracting down to it would
        // remove the cap it was meant to enforce.
        let left = headroom(10 << 20, 40 << 20);
        assert!(left > 0, "a limit must not decay into unlimited");
        assert!(left < 1 << 20, "and it must still be small: {left}");
    }
}
