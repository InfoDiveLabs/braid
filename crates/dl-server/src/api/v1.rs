//! `/api/v1/transfers`: Braid's own read and write surface over the engine.
//!
//! One shape for a torrent and an HTTP download alike, because the web UI
//! draws one row type and only needs the torrent-only fields once it knows
//! it is looking at one. See [`transfer_json`] for the mapping and
//! `torrent_json` for why an HTTP transfer's `torrent` field is `null`
//! rather than a block of zeroes.

use crate::state::AppState;
use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use dl_core::chunks::ChunkReport;
use dl_core::engine::{DownloadId, DownloadSnapshot, DownloadSpec};
use dl_core::integrity::{Algorithm, Digest};
use dl_core::lane::LaneReport;
use dl_core::torrent::{TorrentFile, TorrentStatus, TransferKind, classify};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::PathBuf;

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/transfers", get(list_transfers).post(add_transfer))
        .route("/api/v1/transfers/{id}", get(get_transfer).delete(delete_transfer))
        .route("/api/v1/transfers/{id}/pieces", get(get_pieces))
        .route("/api/v1/transfers/{id}/pause", post(pause_transfer))
        .route("/api/v1/transfers/{id}/resume", post(resume_transfer))
}

/// One transfer, as the web UI reads it.
///
/// A plain function rather than a `Serialize` struct: half the fields are
/// conditional on whether this is a torrent, and matching that here once is
/// clearer than teaching `serde` a shape that changes underneath it.
pub(super) fn transfer_json(snapshot: &DownloadSnapshot) -> Value {
    json!({
        "id": snapshot.id.0,
        "filename": snapshot.filename,
        "host": snapshot.host,
        "state": snapshot.state.as_str(),
        "downloaded": snapshot.progress.downloaded,
        "total": snapshot.progress.total,
        // Expressed 0..100 rather than a bare fraction: a progress bar's
        // width and a percentage label both want the number directly, and
        // `null` here is the difference between "not started" and "the
        // server never told us how big this is" that `total` already draws.
        "percent": snapshot.progress.fraction().map(|f| f as f64 * 100.0),
        "bytes_per_sec": snapshot.progress.bytes_per_sec,
        "smoothed_bytes_per_sec": snapshot.progress.smoothed_bytes_per_sec,
        "phase": snapshot.phase,
        "error": snapshot.error,
        "lanes": snapshot.lanes.iter().map(lane_json).collect::<Vec<_>>(),
        // `None` for HTTP, not a block of zeroes: see `TorrentStatus`'s own
        // doc comment. Zero peers and zero uploaded would read as "we looked
        // and found none", which is a different claim from "this download
        // has no swarm to look in".
        "torrent": snapshot.torrent.as_ref().map(torrent_json),
    })
}

fn lane_json(lane: &LaneReport) -> Value {
    json!({
        "label": lane.label,
        "bytes": lane.bytes,
        "chunks": lane.chunks,
        "bytes_per_sec": lane.throughput,
        "parked": lane.parked,
    })
}

fn torrent_json(torrent: &TorrentStatus) -> Value {
    json!({
        "info_hash": torrent.info_hash,
        "uploaded": torrent.uploaded,
        "upload_bytes_per_sec": torrent.upload_bytes_per_sec,
        "peers": torrent.peers,
        "files": torrent.files.iter().map(torrent_file_json).collect::<Vec<_>>(),
    })
}

fn torrent_file_json(file: &TorrentFile) -> Value {
    json!({ "path": file.path, "len": file.len, "downloaded": file.downloaded })
}

/// The chunk bitmap of one transfer, base64 encoded.
///
/// A large torrent is hundreds of thousands of pieces; `ChunkReport` already
/// carries that as one bit per chunk rather than one row, for exactly the
/// reason given on the field itself in `dl_core::chunks`. Expanding it into a
/// JSON array here would throw that away at the last step and hand the
/// browser the very structure the bitmap exists to avoid. `None` means the
/// transfer exists but is not running right now, and reports nothing rather
/// than a bitmap of all zeroes, which would read as "nothing has arrived yet"
/// for a transfer that may in fact be finished.
fn pieces_json(report: Option<&ChunkReport>) -> Value {
    match report {
        Some(report) => json!({
            "chunk_count": report.chunk_count,
            "chunk_size": report.chunk_size,
            "complete": base64_encode(&report.complete),
            "inflight": report.inflight,
        }),
        None => json!({
            "chunk_count": null,
            "chunk_size": null,
            "complete": null,
            "inflight": [],
        }),
    }
}

/// Standard base64 (RFC 4648), with padding.
///
/// Not pulled in as a dependency: this is the one place a chunk bitmap ever
/// crosses into JSON, the same reasoning `dl_core::torrent::base32_decode`
/// gives for hand-rolling its own encoding rather than adding a crate for one
/// call site.
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(n >> 18 & 0x3f) as usize] as char);
        out.push(ALPHABET[(n >> 12 & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 { ALPHABET[(n >> 6 & 0x3f) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHABET[(n & 0x3f) as usize] as char } else { '=' });
    }
    out
}

async fn list_transfers(State(state): State<AppState>) -> Json<Value> {
    let snapshots = state.engine.snapshot();
    Json(json!({
        "transfers": snapshots.iter().map(transfer_json).collect::<Vec<_>>(),
        // The header reads this rather than summing the rows itself, so the
        // toolbar figure and the one this same call already computed from the
        // lane selectors can never drift apart.
        "total_bytes_per_sec": state.engine.total_bytes_per_sec(),
    }))
}

async fn get_transfer(State(state): State<AppState>, Path(id): Path<u64>) -> Response {
    match state.engine.get(DownloadId(id)) {
        Some(snapshot) => Json(transfer_json(&snapshot)).into_response(),
        None => not_found(),
    }
}

async fn get_pieces(State(state): State<AppState>, Path(id): Path<u64>) -> Response {
    if state.engine.get(DownloadId(id)).is_none() {
        return not_found();
    }
    Json(pieces_json(state.engine.chunks(DownloadId(id)).as_ref())).into_response()
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, Json(json!({ "error": "no such transfer" }))).into_response()
}

fn bad_request(field: &str, message: impl std::fmt::Display) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "field": field, "error": message.to_string() })))
        .into_response()
}

/// What a caller sends to start a transfer.
///
/// One shape for a magnet and a URL alike: the caller has one box to paste
/// into and no reason to know which kind of link it holds, and
/// `dl_core::torrent::classify` already answers that question for whichever
/// one arrives.
#[derive(Deserialize)]
struct NewTransfer {
    url: String,
    /// A path, absolute or relative to the server's download directory.
    /// Left to the server to pick when absent.
    destination: Option<String>,
    connections: Option<usize>,
    /// A pasted digest: `sha256:<hex>`, or bare hex when the algorithm is
    /// unambiguous. See [`parse_expect`].
    expect: Option<String>,
    /// A qBittorrent idea, kept out of `DownloadSpec` on purpose: see
    /// `set_category`.
    category: Option<String>,
}

async fn add_transfer(State(state): State<AppState>, Json(body): Json<NewTransfer>) -> Response {
    let expect = match body.expect.as_deref().map(parse_expect) {
        Some(Err(message)) => return bad_request("expect", message),
        Some(Ok(digest)) => Some(digest),
        None => None,
    };

    let kind = classify(&body.url);
    let destination = match &body.destination {
        Some(raw) => resolve_destination(&state.config.download_dir, raw),
        None => default_destination(&state.config.download_dir, &body.url, &kind),
    };

    let mut spec = DownloadSpec::new(body.url, destination);
    spec.connections = body.connections.unwrap_or(state.config.connections).max(1);
    spec.expect = expect;

    let id = state.engine.add(spec);
    if let Some(category) = body.category {
        set_category(&state.engine, id, category);
    }

    let snapshot = state.engine.get(id).expect("just added, cannot have vanished already");
    (StatusCode::CREATED, Json(transfer_json(&snapshot))).into_response()
}

/// Read a pasted digest the way a person actually writes one down: an
/// `algorithm:` prefix, or bare hex when the length alone says which
/// algorithm it must be. Mirrors `dl-cli`'s own `human::parse_digest`, which
/// this crate cannot call directly (a server has no business depending on the
/// CLI binary to read one string), but the two front ends must still agree on
/// what a person is allowed to paste.
fn parse_expect(text: &str) -> Result<Digest, String> {
    let text = text.trim();
    let (algorithm, hex) = match text.split_once(':') {
        Some((name, hex)) => (
            Algorithm::parse(name).ok_or_else(|| format!("unknown digest algorithm '{name}'"))?,
            hex.trim(),
        ),
        None => (Algorithm::guess_from_hex(text).unwrap_or_default(), text),
    };
    Digest::parse(algorithm, hex).ok_or_else(|| {
        format!("expected a {}-character {} digest", algorithm.hex_len(), algorithm.label())
    })
}

fn resolve_destination(download_dir: &std::path::Path, raw: &str) -> PathBuf {
    let path = PathBuf::from(raw);
    if path.is_absolute() { path } else { download_dir.join(path) }
}

/// Where a transfer lands when the caller did not say.
///
/// A torrent's destination is the folder its own files are written under, so
/// the download directory itself is already the right answer; an HTTP
/// transfer needs a filename, which the URL is the only source for.
fn default_destination(download_dir: &std::path::Path, url: &str, kind: &TransferKind) -> PathBuf {
    match kind {
        TransferKind::Torrent(_) | TransferKind::IncompleteMagnet => download_dir.to_path_buf(),
        TransferKind::Http => download_dir.join(filename_from_url(url)),
    }
}

fn filename_from_url(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    path.rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or("download").to_string()
}

async fn pause_transfer(State(state): State<AppState>, Path(id): Path<u64>) -> Response {
    let id = DownloadId(id);
    if state.engine.get(id).is_none() {
        return not_found();
    }
    state.engine.pause(id);
    Json(transfer_json(&state.engine.get(id).expect("still here, we hold no lock across this")))
        .into_response()
}

async fn resume_transfer(State(state): State<AppState>, Path(id): Path<u64>) -> Response {
    let id = DownloadId(id);
    if state.engine.get(id).is_none() {
        return not_found();
    }
    state.engine.resume(id);
    Json(transfer_json(&state.engine.get(id).expect("still here, we hold no lock across this")))
        .into_response()
}

#[derive(Deserialize, Default)]
struct DeleteQuery {
    /// Defaults to `false`. Deleting somebody's data on the default path of a
    /// `DELETE` is the kind of thing that gets a tool uninstalled, so a
    /// caller has to ask for it by name rather than by omission.
    #[serde(default)]
    delete_files: bool,
}

async fn delete_transfer(
    State(state): State<AppState>,
    Path(id): Path<u64>,
    Query(query): Query<DeleteQuery>,
) -> Response {
    let id = DownloadId(id);
    if state.engine.get(id).is_none() {
        return not_found();
    }
    // The labels go with the record when it does: nothing to clear.
    state.engine.remove_with_files(id, query.delete_files);
    StatusCode::NO_CONTENT.into_response()
}

/// Record which category a transfer was added under.
///
/// A category is a qBittorrent idea and `DownloadSpec` must never learn it
/// exists, which is why it is not a field on the spec. It lives in the
/// engine's label map instead, which the engine stores and hands back without
/// ever reading: see `Engine::set_labels`.
///
/// It has to persist, and not merely for tidiness. A client polls for its own
/// category to find the downloads it is waiting on, so a category lost at
/// restart means that after any restart the client asks for its work, is told
/// there is none, and quietly gives up on everything in flight.
fn set_category(engine: &dl_core::engine::Engine, id: DownloadId, category: String) {
    engine.set_labels(id, std::collections::BTreeMap::from([("category".to_string(), category)]));
}

#[cfg(test)]
mod tests {
    use super::*;
    use dl_core::budget::Budget;
    use dl_core::engine::{Engine, EngineConfig, SourceFactory, State as TransferState};
    use dl_core::lane::LaneSet;
    use http_body_util::BodyExt;
    use std::sync::Arc;
    use tower::ServiceExt;

    fn snapshot_with(
        state: TransferState,
        downloaded: u64,
        total: Option<u64>,
        bytes_per_sec: u64,
    ) -> DownloadSnapshot {
        DownloadSnapshot {
            id: DownloadId(7),
            filename: "some-file.iso".into(),
            host: "example.test".into(),
            state,
            progress: dl_core::model::Progress {
                downloaded,
                total,
                bytes_per_sec,
                smoothed_bytes_per_sec: bytes_per_sec / 2,
            },
            lanes: vec![LaneReport {
                lane: 0,
                label: "en0".into(),
                bytes: downloaded,
                chunks: 3,
                throughput: Some(bytes_per_sec as f64),
                parked: false,
            }],
            phase: None,
            error: None,
            torrent: None,
        }
    }

    #[tokio::test]
    async fn a_transfer_is_reported_with_both_rates_and_its_lanes() {
        // The UI needs the instantaneous rate to display and the smoothed one
        // for an estimate, and they are different numbers for a reason.
        // Collapsing them here would make the web UI's estimate jump the way
        // the desktop's used to.
        let snapshot = snapshot_with(TransferState::Running, 500, Some(1000), 20_000_000);
        let json = transfer_json(&snapshot);
        assert_eq!(json["state"], "running");
        assert_eq!(json["downloaded"], 500);
        assert_eq!(json["total"], 1000);
        assert_eq!(json["bytes_per_sec"], 20_000_000);
        assert!(json["smoothed_bytes_per_sec"].is_number());
        assert_eq!(json["lanes"][0]["label"], "en0");
    }

    #[tokio::test]
    async fn a_transfer_of_unknown_length_reports_null_rather_than_zero() {
        // A server that sent no Content-Length has not told us the file is
        // empty.
        let json = transfer_json(&snapshot_with(TransferState::Running, 500, None, 0));
        assert!(json["total"].is_null());
        assert!(json["percent"].is_null());
    }

    #[tokio::test]
    async fn an_http_transfer_has_no_torrent_block_at_all() {
        // Reporting zero peers and zero uploaded for something that has
        // neither would have the UI draw a seeding row for a file download.
        let json = transfer_json(&snapshot_with(TransferState::Running, 1, Some(2), 0));
        assert!(json["torrent"].is_null());
    }

    #[test]
    fn a_piece_bitmap_is_base64_not_an_array_of_thousands_of_rows() {
        let report = ChunkReport {
            chunk_count: 20,
            chunk_size: 1 << 20,
            complete: vec![0b0000_0101, 0b1111_1111],
            inflight: vec![2, 9],
        };
        let json = pieces_json(Some(&report));
        assert_eq!(json["chunk_count"], 20);
        assert_eq!(json["chunk_size"], 1 << 20);
        assert_eq!(json["complete"], base64_encode(&report.complete));
        assert_eq!(json["inflight"], json!([2, 9]));
        // A grid of twenty rows would already be longer than this string; the
        // whole point of the bitmap is that it does not grow with the piece
        // count the way an array would.
        assert!(json["complete"].as_str().unwrap().len() < 16);
    }

    #[test]
    fn a_transfer_with_no_live_chunk_map_reports_nothing_rather_than_zeroes() {
        // Queued, paused and finished transfers all have no live map. Zeroes
        // here would read as "nothing has arrived yet" for a transfer that
        // may in fact be complete.
        let json = pieces_json(None);
        assert!(json["chunk_count"].is_null());
        assert!(json["complete"].is_null());
    }

    #[test]
    fn base64_matches_known_vectors() {
        // RFC 4648's own test vectors, so a transposed shift or a missing pad
        // byte shows up immediately rather than only on a real bitmap.
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    /// A factory whose lanes are never opened: every test here keeps
    /// `max_concurrent` at zero, so nothing ever leaves the queue to ask for
    /// one.
    struct NoSources;

    impl SourceFactory for NoSources {
        fn lanes_for(&self, _spec: &DownloadSpec) -> dl_core::Result<Box<dyn LaneSet>> {
            unreachable!("nothing in this suite starts a transfer")
        }
    }

    fn test_state() -> AppState {
        let engine = Engine::new(
            Arc::new(NoSources),
            // Zero concurrency: a transfer added in these tests must stay
            // queued, never spawn the task that would call `NoSources`, and
            // never touch the network these tests have no business reaching.
            EngineConfig { max_concurrent: 0, ..Default::default() },
            Budget::unlimited(),
        );
        AppState { engine, config: Arc::new(crate::config::Config::default()) }
    }

    fn app() -> axum::Router {
        routes().with_state(test_state())
    }

    fn post(uri: &str, body: Value) -> axum::http::Request<axum::body::Body> {
        axum::http::Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body.to_string()))
            .unwrap()
    }

    fn get(uri: &str) -> axum::http::Request<axum::body::Body> {
        axum::http::Request::builder().uri(uri).body(axum::body::Body::empty()).unwrap()
    }

    async fn body_json(response: axum::http::Response<axum::body::Body>) -> Value {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn listing_reaches_every_transfer_the_engine_knows_about() {
        let router = app();
        let response = router
            .clone()
            .oneshot(post("/api/v1/transfers", json!({"url": "https://example.test/a.iso"})))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);

        let listed = body_json(router.oneshot(get("/api/v1/transfers")).await.unwrap()).await;
        assert_eq!(listed["transfers"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_transfer_that_does_not_exist_is_a_404_not_an_empty_row() {
        let response = app().oneshot(get("/api/v1/transfers/999")).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn adding_a_magnet_and_a_url_both_work_through_one_endpoint() {
        // The UI has one box. Making the caller say which kind it is would
        // push a decision onto a user who pasted a link, when `classify`
        // already knows.
        let state = test_state();
        let router = routes().with_state(state.clone());

        let hash = "cab507494d02ebb1178b38f2e9d7be299c86b862";
        let magnet = format!("magnet:?xt=urn:btih:{hash}&dn=Some+Release");
        let magnet_response = router
            .clone()
            .oneshot(post("/api/v1/transfers", json!({"url": magnet})))
            .await
            .unwrap();
        assert_eq!(magnet_response.status(), StatusCode::CREATED);
        let magnet_json = body_json(magnet_response).await;
        assert_eq!(magnet_json["state"], "queued");
        // The magnet's own `dn=` names the row before any metadata exists;
        // an HTTP transfer has no such thing, so seeing it here is also proof
        // the request actually reached the torrent path.
        assert_eq!(magnet_json["filename"], "Some Release");

        let url_response = router
            .oneshot(post("/api/v1/transfers", json!({"url": "https://example.test/x.iso"})))
            .await
            .unwrap();
        assert_eq!(url_response.status(), StatusCode::CREATED);
        assert_eq!(body_json(url_response).await["state"], "queued");

        assert_eq!(state.engine.snapshot().len(), 2, "one endpoint, both requests landed");
    }

    #[tokio::test]
    async fn a_bad_digest_is_refused_before_anything_is_started() {
        // Starting a six gigabyte download and failing it at the end over a
        // typo in the checksum field is a bad way to find out.
        let state = test_state();
        let response = routes()
            .with_state(state.clone())
            .oneshot(post(
                "/api/v1/transfers",
                json!({"url": "https://example.test/a.iso", "expect": "sha256:not-hex"}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = body_json(response).await;
        assert_eq!(body["field"], "expect");
        assert!(state.engine.snapshot().is_empty(), "nothing should have been queued");
    }

    #[tokio::test]
    async fn removing_a_transfer_keeps_the_files_unless_asked() {
        // Deleting somebody's data on the default path of a DELETE is the
        // kind of thing that gets a tool uninstalled.
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("a.iso");
        std::fs::write(&dest, b"already on disk").unwrap();

        let state = test_state();
        let router = routes().with_state(state.clone());
        let added = router
            .clone()
            .oneshot(post(
                "/api/v1/transfers",
                json!({"url": "https://example.test/a.iso", "destination": dest.to_str().unwrap()}),
            ))
            .await
            .unwrap();
        let id = body_json(added).await["id"].as_u64().unwrap();

        let deleted =
            router.clone().oneshot(delete(&format!("/api/v1/transfers/{id}"))).await.unwrap();
        assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
        assert!(dest.exists(), "a bare DELETE must not have touched the file");
        assert!(state.engine.get(DownloadId(id)).is_none(), "the row itself is gone");
    }

    #[tokio::test]
    async fn asking_for_delete_files_actually_removes_them() {
        // The other half of the story above: the flag has to do something,
        // or the guarantee it is checked against is untested.
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("a.iso");
        std::fs::write(&dest, b"already on disk").unwrap();

        let router = routes().with_state(test_state());
        let added = router
            .clone()
            .oneshot(post(
                "/api/v1/transfers",
                json!({"url": "https://example.test/a.iso", "destination": dest.to_str().unwrap()}),
            ))
            .await
            .unwrap();
        let id = body_json(added).await["id"].as_u64().unwrap();

        router.oneshot(delete(&format!("/api/v1/transfers/{id}?delete_files=true"))).await.unwrap();
        assert!(!dest.exists());
    }

    #[tokio::test]
    async fn pausing_and_resuming_an_unknown_transfer_is_a_404() {
        let router = routes().with_state(test_state());
        let paused =
            router.clone().oneshot(post("/api/v1/transfers/1/pause", json!({}))).await.unwrap();
        assert_eq!(paused.status(), StatusCode::NOT_FOUND);
        let resumed = router.oneshot(post("/api/v1/transfers/1/resume", json!({}))).await.unwrap();
        assert_eq!(resumed.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_paused_transfer_reports_as_paused_not_as_stalled() {
        let state = test_state();
        let router = routes().with_state(state.clone());
        let added = router
            .clone()
            .oneshot(post("/api/v1/transfers", json!({"url": "https://example.test/a.iso"})))
            .await
            .unwrap();
        let id = body_json(added).await["id"].as_u64().unwrap();

        let paused = router
            .oneshot(post(&format!("/api/v1/transfers/{id}/pause"), json!({})))
            .await
            .unwrap();
        assert_eq!(paused.status(), StatusCode::OK);
        assert_eq!(body_json(paused).await["state"], "paused");
    }

    fn delete(uri: &str) -> axum::http::Request<axum::body::Body> {
        axum::http::Request::builder()
            .method("DELETE")
            .uri(uri)
            .body(axum::body::Body::empty())
            .unwrap()
    }
}
