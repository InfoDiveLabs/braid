//! `/api/v1/transfers`: Braid's own read and write surface over the engine.
//!
//! One shape for a torrent and an HTTP download alike, because the web UI
//! draws one row type and only needs the torrent-only fields once it knows
//! it is looking at one. See [`transfer_json`] for the mapping and
//! `torrent_json` for why an HTTP transfer's `torrent` field is `null`
//! rather than a block of zeroes.

use crate::auth::{
    Credentials, MIN_PASSWORD_LEN, PasswordChangeError, SESSION_COOKIE, Sessions, cookie_value,
};
use crate::state::AppState;
use axum::Json;
use axum::Router;
use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use dl_core::chunks::ChunkReport;
use dl_core::engine::{
    DownloadId, DownloadSnapshot, DownloadSpec, EngineConfig, State as TransferState,
};
use dl_core::integrity::{Algorithm, Digest};
use dl_core::lane::LaneReport;
use dl_core::store::Durability;
use dl_core::torrent::{TorrentFile, TorrentPeer, TorrentStatus, TransferKind, classify};
use dl_net::iface::InterfaceProvider as _;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/transfers", get(list_transfers).post(add_transfer))
        .route("/api/v1/transfers/{id}", get(get_transfer).delete(delete_transfer))
        .route("/api/v1/transfers/{id}/pieces", get(get_pieces))
        .route("/api/v1/transfers/{id}/pause", post(pause_transfer))
        .route("/api/v1/transfers/{id}/resume", post(resume_transfer))
        .route("/api/v1/settings", get(get_settings).post(update_settings))
        .route("/api/v1/password", post(change_password))
}

/// One transfer, as the web UI reads it.
///
/// A plain function rather than a `Serialize` struct: half the fields are
/// conditional on whether this is a torrent, and matching that here once is
/// clearer than teaching `serde` a shape that changes underneath it.
///
/// `category` is not on `DownloadSnapshot` itself: it lives in the engine's
/// label map (see `set_category`) and is looked up by the caller, which
/// already has the engine in hand. Threading it in here rather than a second
/// lookup inside this function keeps this a pure mapping, and lets every call
/// site decide once whether the lookup is worth its cost.
pub(super) fn transfer_json(snapshot: &DownloadSnapshot, category: Option<&str>) -> Value {
    json!({
        "id": snapshot.id.0,
        "filename": snapshot.filename,
        "host": snapshot.host,
        "state": snapshot.state.as_str(),
        "category": category,
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

/// The `category` label for one transfer, or `None` when it was never set.
///
/// A thin wrapper over `Engine::labels` so every call site spells the same
/// lookup the same way, rather than each one reaching into the label map's
/// `"category"` key by hand.
fn category_for(engine: &dl_core::engine::Engine, id: DownloadId) -> Option<String> {
    engine.labels(id).get("category").cloned()
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
        // For the detail panel's Connections tab, which draws this list the
        // same way the desktop Inspector draws its Peers tab. The backend
        // already caps how many come back, so there is no separate limit to
        // apply here.
        "peer_list": torrent.peer_list.iter().map(torrent_peer_json).collect::<Vec<_>>(),
    })
}

fn torrent_file_json(file: &TorrentFile) -> Value {
    json!({ "path": file.path, "len": file.len, "downloaded": file.downloaded })
}

fn torrent_peer_json(peer: &TorrentPeer) -> Value {
    json!({
        "address": peer.address,
        "client": peer.client,
        "downloaded": peer.downloaded,
        "uploaded": peer.uploaded,
        "state": peer.state,
    })
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

/// The other half of [`base64_encode`], for the one other place base64
/// crosses this file's boundary: a dropped `.torrent` file arrives as JSON,
/// which has no way to carry raw bytes directly. Rejects anything that is not
/// a clean multiple of four characters from the standard alphabet rather than
/// guessing at what a malformed upload meant.
fn base64_decode(text: &str) -> Option<Vec<u8>> {
    fn value(byte: u8) -> Option<u32> {
        match byte {
            b'A'..=b'Z' => Some((byte - b'A') as u32),
            b'a'..=b'z' => Some((byte - b'a') as u32 + 26),
            b'0'..=b'9' => Some((byte - b'0') as u32 + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }

    let text: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    if text.is_empty() {
        return Some(Vec::new());
    }
    if !text.len().is_multiple_of(4) {
        return None;
    }

    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    for chunk in text.as_bytes().chunks(4) {
        let pad = chunk.iter().filter(|&&b| b == b'=').count();
        if pad > 2 || chunk[..4 - pad].contains(&b'=') {
            return None;
        }
        let mut n: u32 = 0;
        for &byte in chunk {
            n = (n << 6) | if byte == b'=' { 0 } else { value(byte)? };
        }
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
    }
    Some(out)
}

async fn list_transfers(State(state): State<AppState>) -> Json<Value> {
    let snapshots = state.engine.snapshot();
    Json(json!({
        "transfers": snapshots
            .iter()
            .map(|s| transfer_json(s, category_for(&state.engine, s.id).as_deref()))
            .collect::<Vec<_>>(),
        // The header reads this rather than summing the rows itself, so the
        // toolbar figure and the one this same call already computed from the
        // lane selectors can never drift apart.
        "total_bytes_per_sec": state.engine.total_bytes_per_sec(),
    }))
}

async fn get_transfer(State(state): State<AppState>, Path(id): Path<u64>) -> Response {
    let id = DownloadId(id);
    match state.engine.get(id) {
        Some(snapshot) => {
            let mut body = transfer_json(&snapshot, category_for(&state.engine, id).as_deref());
            // Only the detail view pays for this, not the list or the event
            // stream: a digest is checked once per transfer, not ten times a
            // second, and `checksum_json` has to walk `error` to say whether
            // it passed.
            if let Some(digest) = state.engine.expect(id) {
                body["checksum"] = checksum_json(&digest, &snapshot);
            }
            Json(body).into_response()
        }
        None => not_found(),
    }
}

/// What a caller wants to know about a requested checksum: what was asked
/// for, and whether it held up.
///
/// The HTTP path is the one this project can do that a bare torrent client
/// cannot, so this is not a footnote: a client that finished with the wrong
/// bytes and never said so is worse than one that never checked.
fn checksum_json(digest: &Digest, snapshot: &DownloadSnapshot) -> Value {
    // The engine has nowhere else to put "verified": a transfer that reaches
    // `Complete` with an `expect` set has already passed the comparison in
    // `dl_core::resume`, since a mismatch there aborts the transfer instead of
    // finishing it. A mismatch instead lands in `Failed` with the message
    // `error::Error::IntegrityMismatch` formats, so that text is the only
    // signal available without teaching the engine a new field for one bit of
    // information the state and the error string already carry between them.
    let verified = match snapshot.state {
        TransferState::Complete => Some(true),
        // A failure unrelated to the checksum, such as the network dropping
        // partway through, is not a "no" here: the file never reached the
        // comparison at all, and saying it failed verification would blame
        // the wrong half of the transfer for what went wrong.
        TransferState::Failed => snapshot
            .error
            .as_deref()
            .filter(|error| error.contains("integrity check failed"))
            .map(|_| false),
        _ => None,
    };
    json!({
        "algorithm": digest.algorithm().as_str(),
        "hex": digest.to_hex(),
        "verified": verified,
    })
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
/// One shape for a magnet, a URL and a dropped `.torrent` file alike: the
/// caller has one box to paste or drop into and no reason to know which kind
/// of thing it holds. `url` covers the first two, because
/// `dl_core::torrent::classify` already tells them apart; a browser cannot
/// hand this server a path on its own disk for the third, so `torrent_data`
/// exists to carry the file's bytes instead. Exactly one of the two is
/// expected.
#[derive(Deserialize)]
struct NewTransfer {
    url: Option<String>,
    /// A dropped `.torrent` file's contents, base64 encoded. Staged to a file
    /// under the server's own config directory and then handed to
    /// `classify` exactly like a path the desktop app's "Open Torrent File"
    /// picks: one parser for a local `.torrent`, not two.
    torrent_data: Option<String>,
    /// The dropped file's own name, so the staged copy keeps something
    /// recognisable rather than a bare timestamp. Cosmetic only: nothing
    /// downstream keys anything on it.
    filename: Option<String>,
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
    /// Add the transfer already paused, for a caller that wants to queue up
    /// several things before letting any of them touch the network. There is
    /// no such notion in `DownloadSpec`; this is `Engine::add` immediately
    /// followed by `Engine::pause`, done here rather than as two requests so
    /// the pause can never be lost to a client that navigates away between
    /// them.
    #[serde(default)]
    start_paused: bool,
}

async fn add_transfer(State(state): State<AppState>, Json(body): Json<NewTransfer>) -> Response {
    let expect = match body.expect.as_deref().map(parse_expect) {
        Some(Err(message)) => return bad_request("expect", message),
        Some(Ok(digest)) => Some(digest),
        None => None,
    };

    let non_empty_url = body.url.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let url = match (non_empty_url, &body.torrent_data) {
        (Some(url), _) => url.to_string(),
        (None, Some(data)) => {
            let bytes = match base64_decode(data) {
                Some(bytes) => bytes,
                None => return bad_request("torrent_data", "not valid base64"),
            };
            let name = sanitize_torrent_filename(body.filename.as_deref().unwrap_or("upload"));
            let staged = staged_torrent_path(&state.config.config_dir, &name);
            if let Err(error) = std::fs::create_dir_all(staged.parent().unwrap())
                .and_then(|()| std::fs::write(&staged, &bytes))
            {
                tracing::warn!(%error, "could not stage an uploaded torrent file");
                return bad_request("torrent_data", "could not save the uploaded file");
            }
            staged.display().to_string()
        }
        (None, None) => return bad_request("url", "give a URL, a magnet, or a .torrent file"),
    };

    let kind = classify(&url);
    let destination = match &body.destination {
        Some(raw) => resolve_destination(&state.config.download_dir, raw),
        None => default_destination(&state.config.download_dir, &url, &kind),
    };

    let mut spec = DownloadSpec::new(url, destination);
    spec.connections = body.connections.unwrap_or(state.config.connections).max(1);
    spec.expect = expect;

    let id = state.engine.add(spec);
    if let Some(category) = &body.category {
        set_category(&state.engine, id, category.clone());
    }
    if body.start_paused {
        state.engine.pause(id);
    }

    let snapshot = state.engine.get(id).expect("just added, cannot have vanished already");
    (StatusCode::CREATED, Json(transfer_json(&snapshot, body.category.as_deref()))).into_response()
}

/// Where an uploaded `.torrent` file's bytes land, so the backend has
/// something on disk to read once the transfer actually starts: it opens
/// this path lazily, not at the moment this request returns.
///
/// A timestamp plus a counter rather than the transfer's own id: the id does
/// not exist until `Engine::add` returns, one line after this path is needed.
fn staged_torrent_path(config_dir: &std::path::Path, filename: &str) -> PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    config_dir.join("torrents").join(format!("{nanos}-{seq}-{filename}"))
}

/// A safe, `.torrent`-suffixed name for a staged upload: no directory
/// separators from whatever the browser reported, and the extension
/// `classify` looks for, since a name that lost it along the way would
/// otherwise stage a file `classify` reads straight past as an HTTP download.
fn sanitize_torrent_filename(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().filter(|s| !s.is_empty()).unwrap_or("upload");
    if base.to_ascii_lowercase().ends_with(".torrent") {
        base.to_string()
    } else {
        format!("{base}.torrent")
    }
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
    let snapshot = state.engine.get(id).expect("still here, we hold no lock across this");
    Json(transfer_json(&snapshot, category_for(&state.engine, id).as_deref())).into_response()
}

async fn resume_transfer(State(state): State<AppState>, Path(id): Path<u64>) -> Response {
    let id = DownloadId(id);
    if state.engine.get(id).is_none() {
        return not_found();
    }
    state.engine.resume(id);
    let snapshot = state.engine.get(id).expect("still here, we hold no lock across this");
    Json(transfer_json(&snapshot, category_for(&state.engine, id).as_deref())).into_response()
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

/// `GET /api/v1/settings`: what the settings screen loads before it can offer
/// anything to change.
///
/// `interface_limits` and `durability` come straight off the engine's live
/// `EngineConfig`, which is the truth for both: `Engine::set_config`'s own
/// doc comment is explicit that a change there reaches queued and future
/// transfers, not ones already running, so echoing anything else back would
/// have the settings screen disagree with what the engine will actually do
/// next. `download_dir` and `connections` have no such live copy: `Config` is
/// read once at start into an `Arc` every request shares, and giving it
/// interior mutability so one settings save could reach into that would mean
/// every other handler in this crate now has to reason about a config that
/// can change under it mid-request. Both are still real settings, just ones
/// that take effect for the process that starts after the one holding this
/// request, which is why `update_settings` persists them unconditionally
/// even though it cannot apply them here and now.
async fn get_settings(State(state): State<AppState>) -> Json<Value> {
    Json(settings_json(&state, &state.config.download_dir, state.config.connections))
}

fn settings_json(state: &AppState, download_dir: &std::path::Path, connections: usize) -> Value {
    let engine_config = state.engine.config();
    json!({
        "download_limit": rate_or_unlimited(state.engine.budget().rate()),
        "upload_limit": rate_or_unlimited(state.engine.upload_budget().rate()),
        "interface_limits": engine_config.interface_limits,
        "durability": engine_config.durability.as_str(),
        "download_dir": download_dir,
        "connections": connections,
        "interfaces": usable_interfaces(),
    })
}

/// `Budget::rate` uses zero for unlimited internally; the settings screen
/// draws "Unlimited" for `null` rather than for the number zero, the same
/// convention `parse_limit` already applies to the config file.
fn rate_or_unlimited(rate: u64) -> Value {
    if rate == 0 { Value::Null } else { json!(rate) }
}

/// The interfaces a per-interface limit could name, so the settings screen
/// can offer a real list rather than a free-text field a typo silently does
/// nothing in. Not cached: this is read once per settings-page load, not on
/// every tick of the event stream.
fn usable_interfaces() -> Value {
    dl_net::SystemInterfaces
        .usable()
        .into_iter()
        .map(|iface| json!({ "name": iface.name, "up": iface.is_up, "has_gateway": iface.has_gateway }))
        .collect()
}

/// What a caller sends to change a setting. Every field optional: a caller
/// changing one limit on the Bandwidth page has no reason to also resend the
/// download directory, and a `PATCH`-shaped `POST` that required the whole
/// object would make every settings page write out fields it never showed the
/// user.
#[derive(Deserialize, Default)]
struct SettingsPatch {
    /// Bytes per second; zero (or, from a form, an empty field turned into
    /// zero by the page) means unlimited. Absent means leave it alone.
    download_limit: Option<u64>,
    upload_limit: Option<u64>,
    /// Replaces the whole map when present, the same way saving the Network
    /// page on the desktop app writes every row at once rather than one
    /// interface at a time.
    interface_limits: Option<BTreeMap<String, u64>>,
    /// `"safe"`, `"balanced"` or `"fast"`.
    durability: Option<String>,
    download_dir: Option<String>,
    connections: Option<usize>,
}

/// `POST /api/v1/settings`.
///
/// Two independent things happen here, because the fields split cleanly along
/// exactly that line: a bandwidth limit and the per-interface ceilings are
/// applied to the running engine immediately, through `Budget::set_rate` and
/// `Engine::set_config`; `download_dir` and `connections` cannot be, for the
/// reason `get_settings` explains, and are written to disk only. Everything
/// supplied is written to disk regardless, so a value that could not be
/// applied live is not also lost at the next restart.
async fn update_settings(
    State(state): State<AppState>,
    Json(patch): Json<SettingsPatch>,
) -> Response {
    let durability = match patch.durability.as_deref().map(Durability::parse) {
        Some(None) => {
            return bad_request("durability", "expected one of: safe, balanced, fast");
        }
        Some(Some(d)) => Some(d),
        None => None,
    };

    if let Some(rate) = patch.download_limit {
        state.engine.budget().set_rate(rate);
    }
    if let Some(rate) = patch.upload_limit {
        state.engine.upload_budget().set_rate(rate);
    }

    if patch.interface_limits.is_some() || durability.is_some() {
        let mut engine_config: EngineConfig = (*state.engine.config()).clone();
        if let Some(limits) = &patch.interface_limits {
            engine_config.interface_limits = drop_zero_limits(limits);
        }
        if let Some(d) = durability {
            engine_config.durability = d;
        }
        state.engine.set_config(engine_config);
    }

    let download_dir = patch
        .download_dir
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| state.config.download_dir.clone());
    let connections = patch.connections.unwrap_or(state.config.connections).max(1);

    let mut persisted = (*state.config).clone();
    if let Some(rate) = patch.download_limit {
        persisted.download_limit = (rate != 0).then_some(rate);
    }
    if let Some(rate) = patch.upload_limit {
        persisted.upload_limit = (rate != 0).then_some(rate);
    }
    if let Some(limits) = &patch.interface_limits {
        persisted.interface_limits = drop_zero_limits(limits);
    }
    if let Some(d) = durability {
        persisted.durability = d;
    }
    persisted.download_dir = download_dir.clone();
    persisted.connections = connections;

    // A settings save that took live effect but failed to reach disk should
    // still say so to whoever is running this container, not fail the
    // request: the engine already has the new limits, and the next restart is
    // the only thing at risk.
    if let Err(error) = persisted.write(&state.config.config_dir) {
        tracing::warn!(%error, "could not persist settings to disk");
    }

    Json(settings_json(&state, &download_dir, connections)).into_response()
}

fn drop_zero_limits(limits: &BTreeMap<String, u64>) -> BTreeMap<String, u64> {
    limits
        .iter()
        .filter(|&(_, &rate)| rate != 0)
        .map(|(name, rate)| (name.clone(), *rate))
        .collect()
}

/// What the Settings page sends to change the password.
#[derive(Deserialize)]
struct PasswordChange {
    current_password: String,
    new_password: String,
}

/// `POST /api/v1/password`.
///
/// Reached only with a valid session already, courtesy of `auth::require_auth`
/// sitting in front of every route in this module; the current password is
/// still asked for here, on top of that, because a valid session is exactly
/// what a shared machine's browser has left open for whoever walks up to it
/// next. Asking again costs the legitimate caller one field.
async fn change_password(
    State(state): State<AppState>,
    Extension(sessions): Extension<Arc<Sessions>>,
    Extension(credentials): Extension<Arc<Credentials>>,
    headers: HeaderMap,
    Json(body): Json<PasswordChange>,
) -> Response {
    match credentials.change_password(
        &state.config.config_dir,
        &body.current_password,
        &body.new_password,
    ) {
        Ok(()) => {
            // The whole reason to change a password is a suspicion that a
            // session is loose somewhere else, so every other session goes
            // now. Not this one: logging someone out of the page they are
            // standing on, as a reward for changing their password, is
            // hostile rather than safe.
            if let Some(id) = cookie_value(&headers, SESSION_COOKIE) {
                sessions.revoke_all_except(id);
            }
            StatusCode::NO_CONTENT.into_response()
        }
        // Deliberately no detail beyond the status: telling a caller the
        // current password specifically was wrong is the same mistake as
        // telling a login attempt which half it got wrong.
        Err(PasswordChangeError::WrongCurrentPassword) => {
            (StatusCode::FORBIDDEN, "wrong current password").into_response()
        }
        Err(PasswordChangeError::TooShort) => {
            bad_request("new_password", format!("must be at least {MIN_PASSWORD_LEN} characters"))
        }
        Err(PasswordChangeError::Io(error)) => {
            tracing::warn!(%error, "could not save the new password");
            (StatusCode::INTERNAL_SERVER_ERROR, "could not save the new password").into_response()
        }
    }
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
        let json = transfer_json(&snapshot, None);
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
        let json = transfer_json(&snapshot_with(TransferState::Running, 500, None, 0), None);
        assert!(json["total"].is_null());
        assert!(json["percent"].is_null());
    }

    #[tokio::test]
    async fn an_http_transfer_has_no_torrent_block_at_all() {
        // Reporting zero peers and zero uploaded for something that has
        // neither would have the UI draw a seeding row for a file download.
        let json = transfer_json(&snapshot_with(TransferState::Running, 1, Some(2), 0), None);
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

    #[test]
    fn base64_decode_reverses_base64_encode_for_every_padding_length() {
        for sample in [
            &b""[..],
            b"f",
            b"fo",
            b"foo",
            b"foob",
            b"fooba",
            b"foobar",
            b"a .torrent file's bytes",
        ] {
            assert_eq!(base64_decode(&base64_encode(sample)).unwrap(), sample);
        }
    }

    #[test]
    fn base64_decode_refuses_what_is_not_base64_rather_than_guessing() {
        assert!(base64_decode("not valid base64!!").is_none());
        assert!(base64_decode("AB").is_none(), "not a multiple of four characters");
        assert!(base64_decode("A=AA").is_none(), "padding in the middle of a chunk");
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
    async fn a_category_set_on_add_comes_back_on_every_read_path() {
        // The category filter in the web UI reads this off the list endpoint,
        // not just the detail one: a category invisible there could never be
        // filtered on.
        let state = test_state();
        let router = routes().with_state(state.clone());
        let added = router
            .clone()
            .oneshot(post(
                "/api/v1/transfers",
                json!({"url": "https://example.test/a.iso", "category": "movies"}),
            ))
            .await
            .unwrap();
        assert_eq!(body_json(added).await["category"], "movies");

        let listed = body_json(router.oneshot(get("/api/v1/transfers")).await.unwrap()).await;
        assert_eq!(listed["transfers"][0]["category"], "movies");
    }

    #[tokio::test]
    async fn a_transfer_with_no_category_reports_it_as_null_not_an_empty_string() {
        let router = routes().with_state(test_state());
        let added = router
            .oneshot(post("/api/v1/transfers", json!({"url": "https://example.test/a.iso"})))
            .await
            .unwrap();
        assert!(body_json(added).await["category"].is_null());
    }

    #[tokio::test]
    async fn start_paused_lands_the_transfer_in_paused_rather_than_queued() {
        // Two requests (add, then pause) would leave a window where a second
        // browser tab's own list briefly shows the transfer running; asking
        // for both in the one request that creates it closes that window.
        let router = routes().with_state(test_state());
        let added = router
            .oneshot(post(
                "/api/v1/transfers",
                json!({"url": "https://example.test/a.iso", "start_paused": true}),
            ))
            .await
            .unwrap();
        assert_eq!(body_json(added).await["state"], "paused");
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
    async fn a_dropped_torrent_file_is_staged_and_added_by_the_same_endpoint() {
        // A browser can hand this server bytes but never a path on its own
        // disk, so a drop has to travel as `torrent_data` rather than `url`.
        // Once staged it is meant to look exactly like a `.torrent` the
        // desktop app opened from a local path: one parser, not two.
        let dir = tempfile::tempdir().unwrap();
        let config =
            crate::config::Config { config_dir: dir.path().to_path_buf(), ..Default::default() };
        let state = AppState { engine: test_state().engine, config: Arc::new(config) };

        let response = routes()
            .with_state(state.clone())
            .oneshot(post(
                "/api/v1/transfers",
                json!({
                    "torrent_data": base64_encode(b"pretend this is bencoded"),
                    "filename": "ubuntu.torrent",
                }),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let body = body_json(response).await;
        assert_eq!(body["state"], "queued");
        // Named from the staged file, the same way a local `.torrent` is:
        // proof this actually went down the torrent path and not the HTTP
        // one. The staged name carries a uniqueness prefix, so this checks
        // the meaningful suffix rather than an exact match.
        assert!(body["filename"].as_str().unwrap().ends_with("ubuntu.torrent"));

        let staged: Vec<_> = std::fs::read_dir(dir.path().join("torrents"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(staged.len(), 1);
        assert_eq!(std::fs::read(&staged[0]).unwrap(), b"pretend this is bencoded");
    }

    #[tokio::test]
    async fn neither_a_url_nor_a_torrent_file_is_a_bad_request_not_an_empty_transfer() {
        let response = routes()
            .with_state(test_state())
            .oneshot(post("/api/v1/transfers", json!({})))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn invalid_base64_in_a_dropped_file_is_refused_before_anything_is_staged() {
        let response = routes()
            .with_state(test_state())
            .oneshot(post("/api/v1/transfers", json!({"torrent_data": "not base64!!"})))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
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

    fn a_digest() -> Digest {
        Digest::parse(Algorithm::Sha256, &"ab".repeat(32)).unwrap()
    }

    #[test]
    fn a_completed_transfer_with_a_requested_digest_reports_verified() {
        // `dl_core::resume` aborts rather than finishes a transfer whose bytes
        // do not match, so reaching `Complete` at all is the proof.
        let snapshot = snapshot_with(TransferState::Complete, 1000, Some(1000), 0);
        assert_eq!(checksum_json(&a_digest(), &snapshot)["verified"], true);
    }

    #[test]
    fn a_transfer_still_running_reports_verified_as_unknown_not_false() {
        let snapshot = snapshot_with(TransferState::Running, 500, Some(1000), 100);
        assert!(checksum_json(&a_digest(), &snapshot)["verified"].is_null());
    }

    #[test]
    fn a_mismatch_is_the_only_failure_that_reports_verified_false() {
        let mut mismatched = snapshot_with(TransferState::Failed, 1000, Some(1000), 0);
        mismatched.error = Some("integrity check failed: expected ab, computed cd".into());
        assert_eq!(checksum_json(&a_digest(), &mismatched)["verified"], false);

        // A failure that never reached the comparison, such as the connection
        // dropping, must not be reported as a checksum failure: the checksum
        // was never actually wrong, there was simply nothing left to check.
        let mut unrelated = snapshot_with(TransferState::Failed, 200, Some(1000), 0);
        unrelated.error = Some("connection reset by peer".into());
        assert!(checksum_json(&a_digest(), &unrelated)["verified"].is_null());
    }

    #[tokio::test]
    async fn the_detail_view_carries_a_checksum_the_list_does_not() {
        // `transfer_json` alone drives both the list and the event stream, so
        // keeping the digest out of it keeps it out of both; only the
        // single-transfer endpoint pays to look it up.
        let state = test_state();
        let router = routes().with_state(state.clone());
        let added = router
            .clone()
            .oneshot(post(
                "/api/v1/transfers",
                json!({"url": "https://example.test/a.iso", "expect": format!("sha256:{}", "ab".repeat(32))}),
            ))
            .await
            .unwrap();
        let id = body_json(added).await["id"].as_u64().unwrap();

        let listed =
            body_json(router.clone().oneshot(get("/api/v1/transfers")).await.unwrap()).await;
        assert!(listed["transfers"][0].get("checksum").is_none());

        let detail =
            body_json(router.oneshot(get(&format!("/api/v1/transfers/{id}"))).await.unwrap()).await;
        assert_eq!(detail["checksum"]["algorithm"], "sha256");
        assert_eq!(detail["checksum"]["hex"], "ab".repeat(32));
    }

    #[tokio::test]
    async fn settings_reports_the_unlimited_defaults_a_fresh_engine_starts_with() {
        let response =
            routes().with_state(test_state()).oneshot(get("/api/v1/settings")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert!(body["download_limit"].is_null());
        assert!(body["upload_limit"].is_null());
        assert_eq!(body["durability"], "balanced");
        assert_eq!(body["interface_limits"], json!({}));
        assert!(body["interfaces"].is_array());
    }

    #[tokio::test]
    async fn a_bandwidth_limit_reaches_the_live_budget_immediately() {
        // This is the half of a settings change that has to work without a
        // restart: a transfer already running reads this same `Budget` on
        // every chunk.
        let state = test_state();
        let response = routes()
            .with_state(state.clone())
            .oneshot(post("/api/v1/settings", json!({"download_limit": 5_000_000})))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(state.engine.budget().rate(), 5_000_000);
        assert_eq!(body_json(response).await["download_limit"], 5_000_000);
    }

    #[tokio::test]
    async fn a_download_limit_of_zero_clears_back_to_unlimited() {
        let state = test_state();
        state.engine.budget().set_rate(5_000_000);
        let router = routes().with_state(state.clone());
        router.oneshot(post("/api/v1/settings", json!({"download_limit": 0}))).await.unwrap();
        assert!(state.engine.budget().is_unlimited());
    }

    #[tokio::test]
    async fn interface_limits_and_durability_reach_the_engines_live_config() {
        // `Engine::set_config` only reaches transfers that have not started
        // yet, unlike the bandwidth budget above; this still has to be true
        // for the next one the engine picks up.
        let state = test_state();
        let router = routes().with_state(state.clone());
        let response = router
            .oneshot(post(
                "/api/v1/settings",
                json!({"interface_limits": {"en0": 2_000_000}, "durability": "safe"}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let config = state.engine.config();
        assert_eq!(config.interface_limits.get("en0"), Some(&2_000_000));
        assert_eq!(config.durability, dl_core::store::Durability::Safe);
    }

    #[tokio::test]
    async fn an_interface_limit_of_zero_is_dropped_rather_than_kept_as_a_zero_rate() {
        let state = test_state();
        let router = routes().with_state(state.clone());
        router
            .oneshot(post(
                "/api/v1/settings",
                json!({"interface_limits": {"en0": 2_000_000, "en1": 0}}),
            ))
            .await
            .unwrap();
        assert_eq!(
            state.engine.config().interface_limits,
            BTreeMap::from([("en0".to_string(), 2_000_000)])
        );
    }

    #[tokio::test]
    async fn an_unrecognised_durability_is_refused_before_it_touches_anything() {
        let state = test_state();
        let response = routes()
            .with_state(state.clone())
            .oneshot(post("/api/v1/settings", json!({"durability": "ludicrous"})))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        // Nothing else in the same request should have taken effect either.
        assert_eq!(state.engine.config().durability, dl_core::store::Durability::default());
    }

    #[tokio::test]
    async fn a_settings_save_persists_the_download_directory_and_connection_count() {
        let dir = tempfile::tempdir().unwrap();
        let config =
            crate::config::Config { config_dir: dir.path().to_path_buf(), ..Default::default() };
        let state = AppState { engine: test_state().engine, config: Arc::new(config) };

        let new_dir = dir.path().join("movies");
        let response = routes()
            .with_state(state.clone())
            .oneshot(post(
                "/api/v1/settings",
                json!({"download_dir": new_dir.to_str().unwrap(), "connections": 6}),
            ))
            .await
            .unwrap();
        let body = body_json(response).await;
        assert_eq!(body["download_dir"], new_dir.to_str().unwrap());
        assert_eq!(body["connections"], 6);

        // And it must actually be on disk, for the process that starts next.
        let reloaded = crate::config::Config::read(dir.path(), |_| None);
        assert_eq!(reloaded.download_dir, new_dir);
        assert_eq!(reloaded.connections, 6);
    }

    fn delete(uri: &str) -> axum::http::Request<axum::body::Body> {
        axum::http::Request::builder()
            .method("DELETE")
            .uri(uri)
            .body(axum::body::Body::empty())
            .unwrap()
    }

    /// Everything a password-change test needs: a config directory the change
    /// can actually write into, a session already issued (standing in for the
    /// one a browser would already be holding), and the real password
    /// `Credentials::load_or_create` generated for it, the same way every
    /// other test in this crate that needs to log in does.
    struct PasswordFixture {
        _dir: tempfile::TempDir,
        router: axum::Router,
        sessions: Arc<Sessions>,
        credentials: Arc<Credentials>,
        session: String,
        password: String,
    }

    fn password_fixture() -> PasswordFixture {
        let dir = tempfile::tempdir().unwrap();
        let config =
            crate::config::Config { config_dir: dir.path().to_path_buf(), ..Default::default() };
        let state = AppState { engine: test_state().engine, config: Arc::new(config) };
        let sessions = Arc::new(Sessions::default());
        let (credentials, password) = Credentials::load_or_create(dir.path()).unwrap();
        let password = password.expect("freshly created credentials hand back their password");
        let credentials = Arc::new(credentials);
        let session = sessions.issue();

        let router = routes()
            .with_state(state)
            .layer(Extension(sessions.clone()))
            .layer(Extension(credentials.clone()));

        PasswordFixture { _dir: dir, router, sessions, credentials, session, password }
    }

    fn password_post(
        session: &str,
        current: &str,
        new: &str,
    ) -> axum::http::Request<axum::body::Body> {
        axum::http::Request::builder()
            .method("POST")
            .uri("/api/v1/password")
            .header("content-type", "application/json")
            .header("cookie", format!("SID={session}"))
            .body(axum::body::Body::from(
                json!({"current_password": current, "new_password": new}).to_string(),
            ))
            .unwrap()
    }

    #[tokio::test]
    async fn a_changed_password_works_and_the_old_one_stops_working() {
        let fixture = password_fixture();
        let new_password = "a-fresh-password-of-plenty-of-length";

        let response = fixture
            .router
            .oneshot(password_post(&fixture.session, &fixture.password, new_password))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        assert!(fixture.credentials.verify("admin", new_password));
        assert!(!fixture.credentials.verify("admin", &fixture.password));
    }

    #[tokio::test]
    async fn the_current_password_is_required_even_with_a_valid_session() {
        let fixture = password_fixture();

        let response = fixture
            .router
            .oneshot(password_post(
                &fixture.session,
                "definitely the wrong password",
                "a-fresh-enough-password",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        // A valid session got the request past the middleware this endpoint
        // will sit behind; it must not also get it past this check. Nothing
        // should have moved: the old password still works, and the session
        // that made the doomed attempt is exactly as valid as before it.
        assert!(fixture.credentials.verify("admin", &fixture.password));
        assert!(fixture.sessions.valid(&fixture.session));
    }

    #[tokio::test]
    async fn a_short_new_password_is_refused_and_says_the_limit() {
        let fixture = password_fixture();

        let response = fixture
            .router
            .oneshot(password_post(&fixture.session, &fixture.password, "too-short"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let body = body_json(response).await;
        let message = body["error"].as_str().unwrap_or_default();
        assert!(
            message.contains(&MIN_PASSWORD_LEN.to_string()),
            "the message should say the limit rather than just refuse: {message}"
        );
        assert!(
            fixture.credentials.verify("admin", &fixture.password),
            "nothing should have changed"
        );
    }

    #[tokio::test]
    async fn changing_the_password_keeps_this_session_and_ends_the_others() {
        let fixture = password_fixture();
        let other_session = fixture.sessions.issue();

        let response = fixture
            .router
            .oneshot(password_post(
                &fixture.session,
                &fixture.password,
                "a-fresh-password-of-plenty-of-length",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        assert!(
            fixture.sessions.valid(&fixture.session),
            "changing a password must not sign out the caller"
        );
        assert!(!fixture.sessions.valid(&other_session), "every other session must be ended");
    }
}
