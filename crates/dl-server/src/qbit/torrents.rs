//! `torrents/info`, `torrents/properties` and `torrents/files`: the read side
//! of the surface Sonarr and Radarr actually drive.
//!
//! Everything here reads through [`dl_core::engine::Engine`] alone. A
//! torrent's identity in this API is its info hash, so an entry with none is
//! not a torrent yet as far as any of this is concerned: see
//! [`torrent_hash`]. The driving half, `torrents/add` and everything that
//! acts on a hash a client already has, lands in the same file next.

use crate::qbit::state::{eta_seconds, qbit_state};
use crate::state::AppState;
use axum::Json;
use axum::Router;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use dl_core::engine::{DownloadId, DownloadSnapshot, Engine};
use dl_core::torrent::TorrentStatus;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v2/torrents/info", get(list_torrents))
        .route("/api/v2/torrents/properties", get(torrent_properties))
        .route("/api/v2/torrents/files", get(torrent_files))
}

/// A torrent's info hash, if it has one yet.
///
/// A magnet or a `.torrent` URL is queued the moment it is pasted, long
/// before the backend has anything to report; `Engine::snapshot` carries it
/// as any other transfer until then. Everything in this module treats "has a
/// torrent block with a hash" as the definition of "is a torrent Sonarr can
/// see", so an HTTP download and a torrent still waiting on its metadata are
/// both, correctly, absent from every endpoint here.
fn torrent_hash(snapshot: &DownloadSnapshot) -> Option<String> {
    snapshot.torrent.as_ref()?.info_hash.clone()
}

fn parse_hash_list(raw: &str) -> Vec<String> {
    raw.split('|').map(|s| s.trim().to_ascii_lowercase()).filter(|s| !s.is_empty()).collect()
}

fn find_by_hash(engine: &Engine, hash: &str) -> Option<DownloadSnapshot> {
    let wanted = hash.to_ascii_lowercase();
    engine.snapshot().into_iter().find(|s| torrent_hash(s).as_deref() == Some(wanted.as_str()))
}

fn now_unix() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// `added_on` and `completion_on`, read from labels and written the first
/// time each becomes true rather than guessed.
///
/// `added_on` is written the moment a transfer is created, so by the time
/// anything is listed it is already there. There is no equivalent moment for
/// completion: the engine reports progress as a stream of numbers with no
/// event for "just finished", so there is nothing to hook. What there is,
/// instead, is exactly the endpoint a client polls in order to find out: the
/// first time a listing observes a torrent complete with no `completion_on`
/// on record yet, that observation *is* the earliest honest moment to call it
/// done, and it is written once, here, so a later poll finds it already set
/// rather than sliding forward every time.
fn timestamps(
    engine: &Engine,
    id: DownloadId,
    labels: &BTreeMap<String, String>,
    complete: bool,
) -> (i64, i64) {
    let added_on = labels.get("added_on").and_then(|v| v.parse().ok()).unwrap_or(0);
    if let Some(recorded) = labels.get("completion_on").and_then(|v| v.parse().ok()) {
        return (added_on, recorded);
    }
    if !complete {
        // qBittorrent's own spelling of "not finished yet", so a client
        // sorting or filtering on this field does not read an unfinished
        // download as having completed at the Unix epoch.
        return (added_on, -1);
    }
    let now = now_unix();
    let mut updated = labels.clone();
    updated.insert("completion_on".to_string(), now.to_string());
    engine.set_labels(id, updated);
    (added_on, now)
}

/// The path a client hands to its importer: the file itself for a
/// single-file torrent, the folder for anything else. `destination` is
/// always the folder; only a torrent naming exactly one file resolves to
/// something more specific than that.
fn content_path(destination: &Path, torrent: &TorrentStatus) -> PathBuf {
    match torrent.files.as_slice() {
        [only] => destination.join(&only.path),
        _ => destination.to_path_buf(),
    }
}

fn torrent_entry_json(
    engine: &Engine,
    snapshot: &DownloadSnapshot,
    hash: &str,
    labels: &BTreeMap<String, String>,
    destination: &Path,
) -> Value {
    let progress = &snapshot.progress;
    let complete = progress.total.is_some_and(|total| progress.downloaded >= total);
    let (added_on, completion_on) = timestamps(engine, snapshot.id, labels, complete);
    // Filtered to entries that have one before this is ever called.
    let torrent = snapshot.torrent.as_ref().expect("torrent_hash already checked this");

    let ratio = if progress.downloaded == 0 {
        0.0
    } else {
        torrent.uploaded as f64 / progress.downloaded as f64
    };

    json!({
        "hash": hash,
        "name": snapshot.filename,
        "size": progress.total.unwrap_or(0),
        // qBittorrent's own scale is 0.0 to 1.0, not a percentage: sending 50
        // for half would have every client reading it show five thousand
        // percent complete.
        "progress": progress.fraction().unwrap_or(0.0) as f64,
        "dlspeed": progress.bytes_per_sec,
        "upspeed": torrent.upload_bytes_per_sec,
        "eta": eta_seconds(progress),
        "state": qbit_state(snapshot),
        "category": labels.get("category").cloned().unwrap_or_default(),
        "tags": labels.get("tags").cloned().unwrap_or_default(),
        "save_path": destination.display().to_string(),
        "content_path": content_path(destination, torrent).display().to_string(),
        "added_on": added_on,
        "completion_on": completion_on,
        "ratio": ratio,
        // librqbit keeps whether a peer holds the whole torrent behind a
        // `pub(crate)` type on `LivePeerState` and discards the tracker's own
        // complete/incomplete counts, so there is no honest number available
        // for either of these. Zero, always, rather than a guess: a field
        // that is permanently wrong is worse than one that is visibly a
        // placeholder, because every reader has to rediscover that it means
        // nothing.
        "num_seeds": 0,
        "num_leechs": 0,
    })
}

#[derive(Deserialize, Default)]
struct InfoQuery {
    category: Option<String>,
    hashes: Option<String>,
}

async fn list_torrents(
    State(state): State<AppState>,
    Query(query): Query<InfoQuery>,
) -> Json<Vec<Value>> {
    let wanted_hashes = query.hashes.as_deref().map(parse_hash_list);

    let list = state
        .engine
        .snapshot()
        .into_iter()
        .filter_map(|snapshot| {
            let hash = torrent_hash(&snapshot)?;
            if let Some(wanted) = &wanted_hashes
                && !wanted.contains(&hash)
            {
                return None;
            }
            let labels = state.engine.labels(snapshot.id);
            if let Some(category) = &query.category {
                let current = labels.get("category").map(String::as_str).unwrap_or("");
                if current != category {
                    return None;
                }
            }
            let destination = state.engine.destination(snapshot.id).unwrap_or_default();
            Some(torrent_entry_json(&state.engine, &snapshot, &hash, &labels, &destination))
        })
        .collect();
    Json(list)
}

#[derive(Deserialize)]
struct HashQuery {
    hash: String,
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, Json(json!({ "error": "torrent not found" }))).into_response()
}

async fn torrent_properties(
    State(state): State<AppState>,
    Query(query): Query<HashQuery>,
) -> Response {
    let Some(snapshot) = find_by_hash(&state.engine, &query.hash) else { return not_found() };
    let labels = state.engine.labels(snapshot.id);
    let destination = state.engine.destination(snapshot.id).unwrap_or_default();
    let torrent = snapshot.torrent.as_ref().expect("find_by_hash only matches a torrent");
    let complete =
        snapshot.progress.total.is_some_and(|total| snapshot.progress.downloaded >= total);
    let (added_on, completion_on) = timestamps(&state.engine, snapshot.id, &labels, complete);

    Json(json!({
        "save_path": destination.display().to_string(),
        "total_size": snapshot.progress.total.unwrap_or(0),
        "addition_date": added_on,
        "completion_date": completion_on,
        "up_total": torrent.uploaded,
        "upload_speed": torrent.upload_bytes_per_sec,
        "dl_speed": snapshot.progress.bytes_per_sec,
        "nb_connections": torrent.peers,
        // Neither is tracked: see the comment on `num_seeds` in
        // `torrent_entry_json` for why, and `comment` because librqbit's
        // metadata handling does not surface one at all.
        "seeds": 0,
        "comment": "",
    }))
    .into_response()
}

async fn torrent_files(State(state): State<AppState>, Query(query): Query<HashQuery>) -> Response {
    let Some(snapshot) = find_by_hash(&state.engine, &query.hash) else { return not_found() };
    let torrent = snapshot.torrent.as_ref().expect("find_by_hash only matches a torrent");
    let files: Vec<Value> = torrent
        .files
        .iter()
        .map(|file| {
            json!({
                "name": file.path,
                "size": file.len,
                "progress": if file.len == 0 { 1.0 } else { file.downloaded as f64 / file.len as f64 },
                // The engine downloads every file in a torrent together; there
                // is no per-file priority to report, so "normal" is the only
                // honest answer for all of them.
                "priority": 1,
            })
        })
        .collect();
    Json(files).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use dl_core::Progress;
    use dl_core::budget::Budget;
    use dl_core::engine::{DownloadSpec, Engine, EngineConfig, SourceFactory};
    use dl_core::error::{Error, Result as EngineResult};
    use dl_core::lane::LaneSet;
    use dl_core::torrent::{
        TorrentBackend, TorrentFile, TorrentOutcome, TorrentProgress, TorrentRequest, TorrentSource,
    };
    use http_body_util::BodyExt;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tower::ServiceExt;

    /// Never reached: nothing in this suite starts an HTTP transfer, only
    /// torrents, which never call into a `SourceFactory` at all.
    struct NoFactory;

    impl SourceFactory for NoFactory {
        fn lanes_for(&self, _spec: &DownloadSpec) -> EngineResult<Box<dyn LaneSet>> {
            Err(Error::NoRouteAvailable)
        }
    }

    /// A torrent backend whose every fact is supplied by the test that built
    /// it, standing in for whichever part of librqbit's own reporting that
    /// test cares about.
    ///
    /// `run` never returns on its own: a real torrent stays open until
    /// cancelled, and returning early here would run the engine's real
    /// finish path, which drops everything but `uploaded` and `files` from
    /// the torrent block (see `Engine::run_torrent`) and would silently
    /// erase the very hash these tests are asserting on.
    struct FakeTorrentBackend {
        /// `None` stands in for a torrent whose metadata has not arrived: no
        /// callback ever fires, so the transfer never gains a torrent block
        /// at all.
        progress: Option<TorrentProgress>,
        /// Where each source's files were told to land, learned from `run`.
        /// Unused by anything in this file yet, but kept alongside `run` so
        /// the driving half, landing next, does not have to change this
        /// backend's shape to add the delete test that needs it.
        destinations: Mutex<BTreeMap<String, PathBuf>>,
    }

    impl FakeTorrentBackend {
        fn new(progress: Option<TorrentProgress>) -> Arc<Self> {
            Arc::new(Self { progress, destinations: Mutex::new(BTreeMap::new()) })
        }
    }

    #[async_trait::async_trait]
    impl TorrentBackend for FakeTorrentBackend {
        async fn run(&self, request: TorrentRequest) -> EngineResult<TorrentOutcome> {
            self.destinations
                .lock()
                .unwrap()
                .insert(request.source.key(), request.destination.clone());

            if let (Some(template), Some(on_progress)) = (&self.progress, &request.on_progress) {
                let mut progress = template.clone();
                if progress.status.info_hash.is_none()
                    && let TorrentSource::Magnet(uri) = &request.source
                {
                    progress.status.info_hash = dl_core::torrent::info_hash_of(uri);
                }
                on_progress(progress);
            }

            while !request.cancel.is_cancelled() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            Err(Error::Cancelled)
        }

        async fn discard(&self, _source: &TorrentSource, _delete_files: bool) -> EngineResult<()> {
            Ok(())
        }
    }

    fn default_progress() -> TorrentProgress {
        TorrentProgress {
            progress: Progress {
                downloaded: 500,
                total: Some(1000),
                bytes_per_sec: 1_000,
                smoothed_bytes_per_sec: 900,
            },
            status: TorrentStatus {
                uploaded: 200,
                upload_bytes_per_sec: 50,
                peers: 3,
                files: vec![TorrentFile { path: "file.iso".into(), len: 1000, downloaded: 500 }],
                ..Default::default()
            },
            name: Some("Some Release".into()),
            seeding: false,
            phase: None,
        }
    }

    /// A fresh engine, config directory and download directory per test, so
    /// nothing here reads or writes the real filesystem `Config::default`
    /// would otherwise point at.
    struct TestApp {
        _dir: tempfile::TempDir,
        state: AppState,
    }

    impl TestApp {
        fn with_default_backend() -> Self {
            Self::with_backend(FakeTorrentBackend::new(Some(default_progress())))
        }

        fn with_backend(backend: Arc<FakeTorrentBackend>) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let config_dir = dir.path().join("config");
            let download_dir = dir.path().join("downloads");
            std::fs::create_dir_all(&config_dir).unwrap();
            std::fs::create_dir_all(&download_dir).unwrap();

            let engine =
                Engine::new(Arc::new(NoFactory), EngineConfig::default(), Budget::unlimited());
            engine.set_torrent_backend(backend);

            let config = crate::config::Config { config_dir, download_dir, ..Default::default() };
            Self { _dir: dir, state: AppState { engine, config: Arc::new(config) } }
        }

        fn router(&self) -> Router {
            routes().with_state(self.state.clone())
        }

        /// Add a magnet naming `hash` and wait for the fake backend's first
        /// progress report to land, so a request made right after this call
        /// sees a settled torrent rather than racing the spawned task that
        /// runs it.
        async fn add_magnet_and_wait(&self, hash: &str) -> DownloadId {
            let magnet = format!("magnet:?xt=urn:btih:{hash}&dn=Some+Release");
            let id = self
                .state
                .engine
                .add(DownloadSpec::new(magnet, self.state.config.download_dir.clone()));
            for _ in 0..200 {
                if self.state.engine.get(id).is_some_and(|s| s.torrent.is_some()) {
                    return id;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            panic!("the fake backend never reported a torrent status");
        }
    }

    /// A stable, valid-looking 40 character hex hash from a small seed, so a
    /// test needing a fresh identity does not have to hand-count characters
    /// in a literal.
    fn hash_n(n: u32) -> String {
        format!("{n:040x}")
    }

    fn get(
        router: &Router,
        uri: &str,
    ) -> impl std::future::Future<Output = axum::http::Response<axum::body::Body>> + 'static {
        let request =
            axum::http::Request::builder().uri(uri).body(axum::body::Body::empty()).unwrap();
        let router = router.clone();
        async move { router.oneshot(request).await.unwrap() }
    }

    async fn body_bytes(response: axum::http::Response<axum::body::Body>) -> Vec<u8> {
        response.into_body().collect().await.unwrap().to_bytes().to_vec()
    }

    async fn json_body(response: axum::http::Response<axum::body::Body>) -> Value {
        serde_json::from_slice(&body_bytes(response).await).unwrap()
    }

    async fn list_json(router: &Router, query: &str) -> Vec<Value> {
        let response = get(router, &format!("/api/v2/torrents/info{query}")).await;
        json_body(response).await.as_array().unwrap().clone()
    }

    #[tokio::test]
    async fn the_listing_carries_every_field_the_arr_applications_read() {
        let hash = "2c6b6858d61da9543d4231a71db4b1c9264b0685";
        let app = TestApp::with_default_backend();
        app.add_magnet_and_wait(hash).await;

        let list = list_json(&app.router(), "").await;
        let t = &list[0];
        for field in [
            "hash",
            "name",
            "size",
            "progress",
            "dlspeed",
            "upspeed",
            "eta",
            "state",
            "category",
            "save_path",
            "content_path",
            "completion_on",
            "added_on",
            "ratio",
            "num_seeds",
            "num_leechs",
            "tags",
        ] {
            assert!(!t[field].is_null(), "{field} was missing or null");
        }
        // Zero, and honestly so: see Task 1. librqbit does not expose whether
        // a peer holds the complete torrent, so these are the one pair of
        // fields here that are a placeholder rather than a measurement.
        assert_eq!(t["num_seeds"], 0);
        assert_eq!(t["hash"], hash);
    }

    #[tokio::test]
    async fn progress_is_a_fraction_and_not_a_percentage() {
        // qBittorrent reports 0.0 to 1.0. Sending 50 for half would have
        // every client show a download as five thousand percent complete.
        let hash = hash_n(1);
        let app = TestApp::with_default_backend();
        app.add_magnet_and_wait(&hash).await;

        let list = list_json(&app.router(), "").await;
        assert_eq!(list[0]["progress"], 0.5);
    }

    #[tokio::test]
    async fn http_downloads_are_absent_from_the_torrent_listing() {
        // The clients that read this are torrent clients. A file download
        // with an invented hash would be fed to a state machine we cannot
        // test against.
        let hash = hash_n(2);
        let app = TestApp::with_default_backend();
        app.add_magnet_and_wait(&hash).await;
        app.state.engine.add(DownloadSpec::new(
            "https://example.test/x.iso",
            app.state.config.download_dir.join("x.iso"),
        ));

        let list = list_json(&app.router(), "").await;
        assert_eq!(list.len(), 1, "the HTTP transfer leaked into the torrent listing");
    }

    #[tokio::test]
    async fn a_torrent_whose_hash_is_not_known_yet_is_not_listed() {
        // A `.torrent` URL has no hash until it has been fetched. Listing it
        // with an empty hash makes a client add it again on the next poll.
        let app = TestApp::with_backend(FakeTorrentBackend::new(None));
        app.state.engine.add(DownloadSpec::new(
            "https://example.test/mystery.torrent",
            app.state.config.download_dir.clone(),
        ));
        // The fake backend never calls back regardless of how long this
        // waits, so a short pause is only to let the queue actually start it.
        tokio::time::sleep(Duration::from_millis(20)).await;

        let list = list_json(&app.router(), "").await;
        assert!(list.is_empty());
    }

    #[tokio::test]
    async fn filtering_by_category_and_by_hash_both_work() {
        // Sonarr polls for its own category, and then for one hash at a
        // time.
        let hash_tv = hash_n(3);
        let hash_movies = hash_n(4);
        let app = TestApp::with_default_backend();
        let id_tv = app.add_magnet_and_wait(&hash_tv).await;
        let id_movies = app.add_magnet_and_wait(&hash_movies).await;
        app.state
            .engine
            .set_labels(id_tv, BTreeMap::from([("category".to_string(), "tv-sonarr".to_string())]));
        app.state.engine.set_labels(
            id_movies,
            BTreeMap::from([("category".to_string(), "movies".to_string())]),
        );

        let router = app.router();
        let tv = list_json(&router, "?category=tv-sonarr").await;
        assert_eq!(tv.len(), 1);

        let one = list_json(&router, &format!("?hashes={hash_tv}")).await;
        assert_eq!(one[0]["hash"], hash_tv);
    }

    #[tokio::test]
    async fn a_ratio_with_nothing_downloaded_is_zero_rather_than_a_division_by_zero() {
        let hash = hash_n(5);
        let progress = TorrentProgress {
            progress: Progress {
                downloaded: 0,
                total: Some(1000),
                bytes_per_sec: 0,
                smoothed_bytes_per_sec: 0,
            },
            status: TorrentStatus { uploaded: 500, ..Default::default() },
            name: Some("thing".into()),
            seeding: false,
            phase: None,
        };
        let app = TestApp::with_backend(FakeTorrentBackend::new(Some(progress)));
        app.add_magnet_and_wait(&hash).await;

        let list = list_json(&app.router(), "").await;
        assert_eq!(list[0]["ratio"], 0.0);
    }

    #[tokio::test]
    async fn content_path_points_at_what_was_written_not_at_the_folder() {
        // This is the path the client hands to its importer. For a
        // single-file torrent it is the file; for a multi-file one it is the
        // folder.
        let hash = hash_n(6);
        let app = TestApp::with_default_backend();
        app.add_magnet_and_wait(&hash).await;

        let list = list_json(&app.router(), "").await;
        assert!(list[0]["content_path"].as_str().unwrap().ends_with(".iso"));
    }

    #[tokio::test]
    async fn properties_and_files_answer_for_a_known_hash_and_404_for_an_unknown_one() {
        let hash = hash_n(7);
        let app = TestApp::with_default_backend();
        app.add_magnet_and_wait(&hash).await;
        let router = app.router();

        let props =
            json_body(get(&router, &format!("/api/v2/torrents/properties?hash={hash}")).await)
                .await;
        assert_eq!(props["total_size"], 1000);

        let files =
            json_body(get(&router, &format!("/api/v2/torrents/files?hash={hash}")).await).await;
        assert_eq!(files[0]["name"], "file.iso");

        let missing =
            get(&router, &format!("/api/v2/torrents/properties?hash={}", hash_n(999))).await;
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    }
}
