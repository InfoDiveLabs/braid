//! A real swarm, entirely inside this process.
//!
//! A seeder and a leecher over loopback with the DHT, trackers and local
//! discovery all switched off, so the test reaches nothing outside the machine
//! and cannot pass or fail on the state of the public network. The leecher is
//! driven through [`dl_core::Engine`] rather than through the backend
//! directly, because the dispatch in `run_one` is the part most likely to be
//! wrong and a backend test would not touch it.

use dl_core::budget::Budget;
use dl_core::engine::{DownloadId, DownloadSpec, Engine, EngineConfig, SourceFactory, State};
use dl_torrent::{LibrqbitBackend, SessionConfig};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Two pieces' worth and then some, so the transfer is not a single request
/// and the piece accounting is actually exercised.
const PIECE: u32 = 32 * 1024;
const CONTENT: usize = 7 * 32 * 1024 + 913;

/// No HTTP transfer is ever started here; the factory exists because the
/// engine takes one.
struct NoSources;

impl SourceFactory for NoSources {
    fn lanes_for(&self, _spec: &DownloadSpec) -> dl_core::Result<Box<dyn dl_core::LaneSet>> {
        unreachable!("this test only runs torrents")
    }
}

/// Bytes that do not compress and do not repeat, so a torrent that silently
/// wrote zeroes would still fail the hash check at the end.
fn payload(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut state = 0x243f_6a88_85a3_08d3u64;
    while out.len() < len {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        out.extend_from_slice(&state.to_le_bytes());
    }
    out.truncate(len);
    out
}

/// A session serving `folder`, and the address it accepts peers on.
async fn seeder(folder: &Path, torrent: bytes::Bytes) -> (Arc<librqbit::Session>, SocketAddr) {
    let session = librqbit::Session::new_with_opts(
        folder.to_owned(),
        librqbit::SessionOptions {
            dht: None,
            disable_trackers: true,
            disable_local_service_discovery: true,
            listen: Some(librqbit::ListenerOptions {
                // A concrete loopback address, not the unspecified one: the
                // leecher has to be able to dial back what `listen_addr`
                // reports, and `[::]` is not a destination.
                listen_addr: (Ipv4Addr::LOCALHOST, 0).into(),
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .await
    .expect("the seeder session starts");

    let handle = session
        .add_torrent(
            librqbit::AddTorrent::TorrentFileBytes(torrent),
            Some(librqbit::AddTorrentOptions {
                overwrite: true,
                output_folder: Some(folder.to_string_lossy().into_owned()),
                ..Default::default()
            }),
        )
        .await
        .expect("the seeder accepts the torrent")
        .into_handle()
        .expect("the seeder returns a handle");

    // It has the files already, but it still has to hash them before it will
    // serve anything. Announcing the address before that is done means the
    // leecher connects to a peer with no pieces.
    for _ in 0..600 {
        if handle.stats().finished {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(handle.stats().finished, "the seeder never finished checking its own files");

    let addr = session.listen_addr().expect("the seeder is listening");
    (session, addr)
}

/// An engine wired to a backend that can only reach `peers`.
fn leeching_engine(out: &Path, peers: Vec<SocketAddr>, config: EngineConfig) -> Engine {
    let engine = Engine::new(Arc::new(NoSources), config, Budget::unlimited());
    engine.set_torrent_backend(Arc::new(LibrqbitBackend::new(SessionConfig {
        disable_dht: true,
        disable_trackers: true,
        disable_local_discovery: true,
        initial_peers: peers,
        ..SessionConfig::new(out)
    })));
    engine
}

/// Wait for one of `states`, or say what it was stuck on.
async fn wait_for(engine: &Engine, id: DownloadId, states: &[State], secs: u64) -> State {
    for _ in 0..(secs * 20) {
        if let Some(snapshot) = engine.get(id) {
            if states.contains(&snapshot.state) {
                return snapshot.state;
            }
            if snapshot.state == State::Failed {
                panic!("the transfer failed: {:?}", snapshot.error);
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let snapshot = engine.get(id).expect("the record is still there");
    panic!("stuck in {:?} after {secs}s (error: {:?})", snapshot.state, snapshot.error);
}

/// Write the payload into its own folder and build a torrent for it.
async fn published(root: &Path) -> (PathBuf, Vec<u8>, bytes::Bytes) {
    let folder = root.join("published");
    std::fs::create_dir_all(&folder).expect("the seed folder is writable");
    let content = payload(CONTENT);
    let file = folder.join("payload.bin");
    std::fs::write(&file, &content).expect("the payload is written");

    let torrent = librqbit::create_torrent(
        &file,
        librqbit::CreateTorrentOptions { piece_length: Some(PIECE), ..Default::default() },
        &librqbit::spawn_utils::BlockingSpawner::new(1),
    )
    .await
    .expect("a torrent is created from the payload");

    (folder, content, torrent.as_bytes().expect("the torrent serializes"))
}

/// The whole point: bytes that arrive through a swarm are the bytes that went
/// in. A torrent that wrote pieces at the wrong offsets would still report
/// 100% and the right length, and only the hash catches it.
#[tokio::test(flavor = "multi_thread")]
async fn a_file_downloaded_from_a_loopback_swarm_hashes_to_the_original() {
    let root = tempfile::TempDir::with_prefix("braid-swarm").expect("a temp dir");
    let (seed_folder, content, torrent_bytes) = published(root.path()).await;

    let torrent_file = root.path().join("payload.torrent");
    std::fs::write(&torrent_file, &torrent_bytes).expect("the .torrent is written");

    let (_seeder, addr) = seeder(&seed_folder, torrent_bytes).await;

    let out = root.path().join("out");
    std::fs::create_dir_all(&out).expect("the output folder is writable");
    // Seeding off, so the transfer finishes rather than sitting in the swarm
    // for the rest of the test run.
    let engine = leeching_engine(
        &out,
        vec![addr],
        EngineConfig { seed_after_complete: false, ..Default::default() },
    );

    let id = engine.add(DownloadSpec::new(torrent_file.to_string_lossy().to_string(), &out));
    wait_for(&engine, id, &[State::Complete], 120).await;

    let landed = out.join("payload.bin");
    let got = std::fs::read(&landed).expect("the file landed where the torrent named it");
    assert_eq!(
        blake3::hash(&got),
        blake3::hash(&content),
        "the file came through the swarm with different bytes in it"
    );

    let snapshot = engine.get(id).expect("the record survives completion");
    assert_eq!(snapshot.progress.downloaded, content.len() as u64);
    let torrent = snapshot.torrent.expect("a torrent transfer reports torrent statistics");
    assert_eq!(torrent.files.len(), 1, "the file list must name what is inside");
    assert_eq!(torrent.files[0].path, "payload.bin");
    assert_eq!(torrent.files[0].len, content.len() as u64);
}

/// Seeding is a state the engine has to keep out of the queue, or one finished
/// torrent permanently occupies a concurrency slot.
#[tokio::test(flavor = "multi_thread")]
async fn a_finished_torrent_seeds_without_holding_a_download_slot() {
    let root = tempfile::TempDir::with_prefix("braid-seeding").expect("a temp dir");
    let (seed_folder, content, torrent_bytes) = published(root.path()).await;
    let torrent_file = root.path().join("payload.torrent");
    std::fs::write(&torrent_file, &torrent_bytes).expect("the .torrent is written");

    let (_seeder, addr) = seeder(&seed_folder, torrent_bytes).await;

    let out = root.path().join("out");
    std::fs::create_dir_all(&out).expect("the output folder is writable");
    let engine = leeching_engine(
        &out,
        vec![addr],
        EngineConfig { max_concurrent: 1, seed_after_complete: true, ..Default::default() },
    );

    let id = engine.add(DownloadSpec::new(torrent_file.to_string_lossy().to_string(), &out));
    wait_for(&engine, id, &[State::Seeding], 120).await;

    assert_eq!(
        blake3::hash(&std::fs::read(out.join("payload.bin")).expect("the file is on disk")),
        blake3::hash(&content)
    );
    // The seeding torrent is still running, but the single slot has to be free
    // for the next transfer or the queue stops moving.
    assert_eq!(engine.count_in(State::Running), 0);

    // And it must still be stoppable: seeding is not terminal.
    engine.pause(id);
    assert_eq!(wait_for(&engine, id, &[State::Paused], 30).await, State::Paused);
}

/// A paused torrent keeps its pieces.
///
/// The seeder is stopped before the resume, so a transfer that threw its data
/// away could never finish: there is nobody left to download from. That is
/// what makes this a test of resuming rather than of downloading twice.
#[tokio::test(flavor = "multi_thread")]
async fn resuming_a_paused_torrent_keeps_what_is_already_on_disk() {
    let root = tempfile::TempDir::with_prefix("braid-resume").expect("a temp dir");
    let (seed_folder, content, torrent_bytes) = published(root.path()).await;
    let torrent_file = root.path().join("payload.torrent");
    std::fs::write(&torrent_file, &torrent_bytes).expect("the .torrent is written");

    let (seeding_session, addr) = seeder(&seed_folder, torrent_bytes).await;

    let out = root.path().join("out");
    std::fs::create_dir_all(&out).expect("the output folder is writable");
    let engine = leeching_engine(
        &out,
        vec![addr],
        EngineConfig { seed_after_complete: true, ..Default::default() },
    );

    let id = engine.add(DownloadSpec::new(torrent_file.to_string_lossy().to_string(), &out));
    wait_for(&engine, id, &[State::Seeding], 120).await;

    engine.pause(id);
    wait_for(&engine, id, &[State::Paused], 30).await;

    // From here there is no swarm at all.
    seeding_session.stop().await;
    drop(seeding_session);

    engine.resume(id);
    wait_for(&engine, id, &[State::Seeding], 60).await;

    let snapshot = engine.get(id).expect("the record survives a resume");
    assert_eq!(
        snapshot.progress.downloaded,
        content.len() as u64,
        "a resume must not throw away the pieces already verified on disk"
    );
    assert_eq!(
        blake3::hash(&std::fs::read(out.join("payload.bin")).expect("the file is still there")),
        blake3::hash(&content)
    );
}

/// Cancelling with the delete flag raised must actually remove the file.
///
/// Asserted end to end rather than at the seam: the paths are librqbit's to
/// know, and a transfer that only *looked* deleted would resume from the files
/// still on disk when the same link was added again.
#[tokio::test(flavor = "multi_thread")]
async fn cancelling_with_delete_removes_the_file_from_disk() {
    let root = tempfile::TempDir::with_prefix("braid-delete").expect("a temp dir");
    let (seed_folder, content, torrent_bytes) = published(root.path()).await;
    let torrent_file = root.path().join("payload.torrent");
    std::fs::write(&torrent_file, &torrent_bytes).expect("the .torrent is written");

    let (seeding_session, addr) = seeder(&seed_folder, torrent_bytes).await;

    let out = root.path().join("out");
    std::fs::create_dir_all(&out).expect("the output folder is writable");
    let engine = leeching_engine(&out, vec![addr], EngineConfig::default());

    let id = engine.add(DownloadSpec::new(torrent_file.to_string_lossy().to_string(), &out));
    wait_for(&engine, id, &[State::Complete, State::Seeding], 120).await;

    let landed = out.join("payload.bin");
    assert!(landed.exists(), "nothing was downloaded to delete");
    assert_eq!(std::fs::read(&landed).unwrap().len(), content.len());

    engine.remove_with_files(id, true);

    // The backend does the deleting on the tick that stops it, so there is a
    // bounded wait rather than an immediate assertion.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while landed.exists() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!landed.exists(), "the file is still on disk after asking for it to be deleted");

    seeding_session.stop().await;
}

/// The opposite, so the delete is a choice rather than the only behaviour.
#[tokio::test(flavor = "multi_thread")]
async fn cancelling_without_delete_leaves_the_file_alone() {
    let root = tempfile::TempDir::with_prefix("braid-keep").expect("a temp dir");
    let (seed_folder, _content, torrent_bytes) = published(root.path()).await;
    let torrent_file = root.path().join("payload.torrent");
    std::fs::write(&torrent_file, &torrent_bytes).expect("the .torrent is written");

    let (seeding_session, addr) = seeder(&seed_folder, torrent_bytes).await;

    let out = root.path().join("out");
    std::fs::create_dir_all(&out).expect("the output folder is writable");
    let engine = leeching_engine(&out, vec![addr], EngineConfig::default());

    let id = engine.add(DownloadSpec::new(torrent_file.to_string_lossy().to_string(), &out));
    wait_for(&engine, id, &[State::Complete, State::Seeding], 120).await;

    engine.remove_with_files(id, false);
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(out.join("payload.bin").exists(), "the file was deleted without being asked");

    seeding_session.stop().await;
}

/// The case the user actually hit: delete while it is still downloading.
///
/// No peers, so it cannot finish and the file on disk is the empty shell
/// librqbit lays down. Cancelling with delete must take that with it: /// otherwise re-adding the same link finds the torrent still registered and
/// picks up where it left off, which is what "it resumed" meant.
#[tokio::test(flavor = "multi_thread")]
async fn cancelling_an_unfinished_torrent_with_delete_removes_what_is_on_disk() {
    let root = tempfile::TempDir::with_prefix("braid-delete-partial").expect("a temp dir");
    let (_seed_folder, _content, torrent_bytes) = published(root.path()).await;
    let torrent_file = root.path().join("payload.torrent");
    std::fs::write(&torrent_file, &torrent_bytes).expect("the .torrent is written");

    let out = root.path().join("out");
    std::fs::create_dir_all(&out).expect("the output folder is writable");
    // No peers at all: it will never complete.
    let engine = leeching_engine(&out, Vec::new(), EngineConfig::default());

    let id = engine.add(DownloadSpec::new(torrent_file.to_string_lossy().to_string(), &out));
    wait_for(&engine, id, &[State::Running], 60).await;

    let landed = out.join("payload.bin");
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !landed.exists() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(landed.exists(), "librqbit never laid the file down, so there is nothing to test");

    engine.remove_with_files(id, true);

    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while landed.exists() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!landed.exists(), "an unfinished torrent's file survived a delete");
}
