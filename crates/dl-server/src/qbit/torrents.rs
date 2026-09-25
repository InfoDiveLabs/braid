//! `torrents/*` and `sync/maindata`: the surface Sonarr and Radarr actually
//! drive.
//!
//! Everything here reads and writes through [`dl_core::engine::Engine`]
//! alone. A torrent's identity in this API is its info hash, so an entry with
//! none is not a torrent yet as far as any of this is concerned: see
//! [`torrent_hash`].

use crate::qbit::state::{eta_seconds, qbit_state};
use crate::state::AppState;
use axum::Form;
use axum::Json;
use axum::Router;
use axum::body::to_bytes;
use axum::extract::{FromRequest, Multipart, Query, Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use dl_core::engine::{DownloadId, DownloadSnapshot, DownloadSpec, Engine};
use dl_core::torrent::TorrentStatus;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v2/torrents/info", get(list_torrents))
        .route("/api/v2/torrents/properties", get(torrent_properties))
        .route("/api/v2/torrents/files", get(torrent_files))
        .route("/api/v2/torrents/add", post(add_torrents))
        .route("/api/v2/torrents/delete", post(delete_torrents))
        .route("/api/v2/torrents/pause", post(pause_torrents))
        .route("/api/v2/torrents/resume", post(resume_torrents))
        .route("/api/v2/torrents/setCategory", post(set_category))
        .route("/api/v2/torrents/categories", get(list_categories))
        .route("/api/v2/torrents/createCategory", post(create_category))
        .route("/api/v2/torrents/removeCategories", post(remove_categories))
        .route("/api/v2/torrents/tags", get(list_tags))
        .route("/api/v2/torrents/createTags", post(create_tags))
        .route("/api/v2/torrents/deleteTags", post(delete_tags))
        .route("/api/v2/torrents/addTags", post(add_tags))
        .route("/api/v2/torrents/removeTags", post(remove_tags))
        .route("/api/v2/torrents/setShareLimits", post(set_share_limits))
        .route("/api/v2/torrents/topPrio", post(top_priority))
        .route("/api/v2/torrents/setForceStart", post(set_force_start))
        .route("/api/v2/sync/maindata", get(sync_maindata))
}

// ---------------------------------------------------------------------------
// Reading: info, properties, files.
// ---------------------------------------------------------------------------

/// A torrent's info hash, if it has one yet.
///
/// The backend's answer when it has one, and the magnet's own when it does
/// not. Both matter, and for different reasons.
///
/// A magnet carries its info hash in the URI, so the hash is knowable the
/// instant one is pasted. Waiting for the backend to join a swarm and report
/// back looks harmless and is not: a client adds a torrent and asks for it by
/// hash within the second, and a listing that leaves it out until the swarm
/// answers tells that client its request was lost. It adds the same torrent
/// again, and again on every poll, because nothing it can see says otherwise.
///
/// A `.torrent` URL genuinely has no hash until the file behind it has been
/// fetched and parsed, so those really are absent until the backend reports,
/// and so is every HTTP download. That is correct: this endpoint exists for
/// torrent clients, and a file download with an invented hash would be fed to
/// a state machine we do not control.
fn torrent_hash(engine: &Engine, snapshot: &DownloadSnapshot) -> Option<String> {
    if let Some(hash) = snapshot.torrent.as_ref().and_then(|t| t.info_hash.clone()) {
        return Some(hash);
    }
    dl_core::torrent::info_hash_of(&engine.url(snapshot.id)?)
}

fn parse_hash_list(raw: &str) -> Vec<String> {
    raw.split('|').map(|s| s.trim().to_ascii_lowercase()).filter(|s| !s.is_empty()).collect()
}

/// Every id an action's `hashes` field names, understanding the literal `all`
/// the way qBittorrent's clients use it: not every transfer, only every
/// torrent, since this whole module has nothing to say about an HTTP
/// download.
fn resolve_ids(engine: &Engine, hashes: &str) -> Vec<DownloadId> {
    let snapshots = engine.snapshot();
    if hashes.trim().eq_ignore_ascii_case("all") {
        return snapshots
            .iter()
            .filter(|s| torrent_hash(engine, s).is_some())
            .map(|s| s.id)
            .collect();
    }
    let wanted = parse_hash_list(hashes);
    snapshots
        .iter()
        .filter_map(|s| torrent_hash(engine, s).map(|hash| (hash, s.id)))
        .filter(|(hash, _)| wanted.contains(hash))
        .map(|(_, id)| id)
        .collect()
}

fn find_by_hash(engine: &Engine, hash: &str) -> Option<DownloadSnapshot> {
    let wanted = hash.to_ascii_lowercase();
    engine
        .snapshot()
        .into_iter()
        .find(|s| torrent_hash(engine, s).as_deref() == Some(wanted.as_str()))
}

fn now_unix() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// `added_on` and `completion_on`, read from labels and written the first
/// time each becomes true rather than guessed.
///
/// `added_on` is written by [`label_new_torrent`] at the moment a transfer is
/// created, so by the time anything is listed it is already there. There is
/// no equivalent moment for completion: the engine reports progress as a
/// stream of numbers with no event for "just finished", so there is nothing
/// to hook. What there is, instead, is exactly the endpoint a client polls in
/// order to find out: the first time a listing observes a torrent complete
/// with no `completion_on` on record yet, that observation *is* the earliest
/// honest moment to call it done, and it is written once, here, so a later
/// poll finds it already set rather than sliding forward every time.
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
/// Where the client should look for what was downloaded.
///
/// A single file torrent points at the file, a multi file one at the folder.
/// `None` means the backend has not reported yet, which is the same answer as
/// a multi file torrent for this purpose: the folder is the honest thing to
/// name, and it is where the contents will appear either way.
fn content_path(destination: &Path, torrent: Option<&TorrentStatus>) -> PathBuf {
    match torrent.map(|t| t.files.as_slice()) {
        Some([only]) => destination.join(&only.path),
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
    // A magnet is listed from the moment it is added, before the backend has
    // joined a swarm and has anything to report: see `torrent_hash` for why
    // that matters. So everything below is absent rather than zero until then,
    // and absent has to read as "not yet" rather than as a measurement. This
    // was an `expect` while the listing only ever saw torrents the backend had
    // already reported on, and it panicked the first time one was listed
    // before that.
    let torrent = snapshot.torrent.as_ref();
    let uploaded = torrent.map(|t| t.uploaded).unwrap_or(0);
    let upload_rate = torrent.map(|t| t.upload_bytes_per_sec).unwrap_or(0);

    let ratio =
        if progress.downloaded == 0 { 0.0 } else { uploaded as f64 / progress.downloaded as f64 };

    json!({
        "hash": hash,
        "name": snapshot.filename,
        "size": progress.total.unwrap_or(0),
        // qBittorrent's own scale is 0.0 to 1.0, not a percentage: sending 50
        // for half would have every client reading it show five thousand
        // percent complete.
        "progress": progress.fraction().unwrap_or(0.0) as f64,
        "dlspeed": progress.bytes_per_sec,
        "upspeed": upload_rate,
        "eta": eta_seconds(progress),
        "state": qbit_state(snapshot),
        "category": labels.get("category").cloned().unwrap_or_default(),
        "tags": labels.get("tags").cloned().unwrap_or_default(),
        // The directory the category points at, not the folder this torrent
        // writes into. A client reads `save_path` to learn where downloads in
        // general land and `content_path` to find this one, and collapsing the
        // two is exactly what made Sonarr refuse to import: if the content is
        // reported as being at the base directory, nothing distinguishes it
        // from everything else in there.
        "save_path": labels
            .get("save_path")
            .cloned()
            .unwrap_or_else(|| {
                destination.parent().unwrap_or(destination).display().to_string()
            }),
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
    /// A single tag to filter by, which is how qBittorrent spells it: one
    /// tag, not a list, because a client asks for the one it is looking after.
    tag: Option<String>,
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
            let hash = torrent_hash(&state.engine, &snapshot)?;
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
            if let Some(wanted) = &query.tag
                && !parse_tag_list(labels.get("tags").map(String::as_str).unwrap_or(""))
                    .contains(wanted)
            {
                return None;
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
    // A magnet's hash matches before the swarm is joined, so there may be no
    // `TorrentStatus` yet. Zero means "not reported", as in `torrent_entry_json`.
    let torrent = snapshot.torrent.as_ref();
    let complete =
        snapshot.progress.total.is_some_and(|total| snapshot.progress.downloaded >= total);
    let (added_on, completion_on) = timestamps(&state.engine, snapshot.id, &labels, complete);

    Json(json!({
        "save_path": destination.display().to_string(),
        "total_size": snapshot.progress.total.unwrap_or(0),
        "addition_date": added_on,
        "completion_date": completion_on,
        "up_total": torrent.map(|t| t.uploaded).unwrap_or(0),
        "upload_speed": torrent.map(|t| t.upload_bytes_per_sec).unwrap_or(0),
        "dl_speed": snapshot.progress.bytes_per_sec,
        "nb_connections": torrent.map(|t| t.peers).unwrap_or(0),
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
    // No `TorrentStatus` yet means no file list yet: see `torrent_properties`.
    let files: Vec<Value> = snapshot
        .torrent
        .as_ref()
        .map(|t| t.files.as_slice())
        .unwrap_or(&[])
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

// ---------------------------------------------------------------------------
// Categories: `/config/categories.conf`, name to save path.
// ---------------------------------------------------------------------------

fn categories_path(config_dir: &Path) -> PathBuf {
    config_dir.join("categories.conf")
}

/// `name = save path`, one per line: the same flat format `config.rs` and
/// `dl_core::persist` already use, so an operator who has opened one of those
/// files by hand finds nothing unfamiliar in this one.
fn load_categories(config_dir: &Path) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    let Ok(text) = std::fs::read_to_string(categories_path(config_dir)) else { return map };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((name, path)) = line.split_once('=') {
            map.insert(name.trim().to_string(), path.trim().to_string());
        }
    }
    map
}

fn save_categories(config_dir: &Path, categories: &BTreeMap<String, String>) {
    let mut out = String::from("# Braid categories, written by the compatible API.\n");
    for (name, path) in categories {
        out.push_str(&format!("{name} = {path}\n"));
    }
    if std::fs::create_dir_all(config_dir).is_ok() {
        let _ = std::fs::write(categories_path(config_dir), out);
    }
}

async fn list_categories(State(state): State<AppState>) -> Json<Value> {
    let categories = load_categories(&state.config.config_dir);
    let object: Map<String, Value> = categories
        .into_iter()
        .map(|(name, save_path)| {
            let entry = json!({ "name": name, "savePath": save_path });
            (name, entry)
        })
        .collect();
    Json(Value::Object(object))
}

#[derive(Deserialize)]
struct CreateCategoryForm {
    category: String,
    #[serde(rename = "savePath", default)]
    save_path: String,
}

/// Tags, which are a flat list rather than a map.
///
/// Stored in the same one-line-per-entry shape as the categories beside them,
/// with nothing after the separator: a tag carries no save path, only a name.
/// It would be neater as a plain list and it is not worth a second file format
/// in a directory people read by hand.
fn tags_path(config_dir: &Path) -> PathBuf {
    config_dir.join("tags.conf")
}

fn load_tags(config_dir: &Path) -> BTreeSet<String> {
    let Ok(text) = std::fs::read_to_string(tags_path(config_dir)) else {
        return BTreeSet::new();
    };
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| line.split('=').next().unwrap_or(line).trim().to_string())
        .filter(|name| !name.is_empty())
        .collect()
}

fn save_tags(config_dir: &Path, tags: &BTreeSet<String>) {
    let mut out = String::from("# Braid tags. Written by the app.\n");
    for tag in tags {
        out.push_str(tag);
        out.push_str(" =\n");
    }
    let _ = std::fs::create_dir_all(config_dir);
    let _ = std::fs::write(tags_path(config_dir), out);
}

/// Read a `tags=a,b,c` field, which is how every one of these endpoints takes
/// them, into the set the rest of this module works in.
fn parse_tag_list(raw: &str) -> BTreeSet<String> {
    raw.split(',').map(str::trim).filter(|t| !t.is_empty()).map(str::to_string).collect()
}

/// The tags on one transfer, from the single comma separated label they live
/// in. Stored that way rather than as a label each because `torrents/info`
/// reports them as one string and the engine's label map is not the place to
/// invent a schema.
fn tags_of(engine: &Engine, id: DownloadId) -> BTreeSet<String> {
    parse_tag_list(engine.labels(id).get("tags").map(String::as_str).unwrap_or(""))
}

fn write_tags(engine: &Engine, id: DownloadId, tags: &BTreeSet<String>) {
    let mut labels = engine.labels(id);
    if tags.is_empty() {
        labels.remove("tags");
    } else {
        labels.insert("tags".to_string(), tags.iter().cloned().collect::<Vec<_>>().join(","));
    }
    engine.set_labels(id, labels);
}

/// Every tag this server knows: the ones created explicitly, and the ones that
/// only ever arrived on a torrent.
///
/// Both, because a client that tagged something at `add` time never called
/// `createTags`, and a tag that exists on a download but not in this list
/// would be one the user can see and cannot filter by.
async fn list_tags(State(state): State<AppState>) -> Json<Vec<String>> {
    let mut tags = load_tags(&state.config.config_dir);
    for snapshot in state.engine.snapshot() {
        tags.extend(tags_of(&state.engine, snapshot.id));
    }
    Json(tags.into_iter().collect())
}

#[derive(Deserialize)]
struct TagsForm {
    tags: String,
}

async fn create_tags(State(state): State<AppState>, Form(form): Form<TagsForm>) -> StatusCode {
    let mut tags = load_tags(&state.config.config_dir);
    tags.extend(parse_tag_list(&form.tags));
    save_tags(&state.config.config_dir, &tags);
    StatusCode::OK
}

/// Delete tags, and take them off everything carrying them.
///
/// Leaving them on the transfers would mean a tag that is gone from the list
/// and still shown on a row, which is the sort of inconsistency that has a
/// client asking to filter by something this server has just said does not
/// exist.
async fn delete_tags(State(state): State<AppState>, Form(form): Form<TagsForm>) -> StatusCode {
    let going = parse_tag_list(&form.tags);
    let mut tags = load_tags(&state.config.config_dir);
    tags.retain(|tag| !going.contains(tag));
    save_tags(&state.config.config_dir, &tags);

    for snapshot in state.engine.snapshot() {
        let current = tags_of(&state.engine, snapshot.id);
        if current.iter().any(|tag| going.contains(tag)) {
            let kept: BTreeSet<String> =
                current.into_iter().filter(|tag| !going.contains(tag)).collect();
            write_tags(&state.engine, snapshot.id, &kept);
        }
    }
    StatusCode::OK
}

#[derive(Deserialize)]
struct TagEditForm {
    hashes: String,
    tags: String,
}

async fn add_tags(State(state): State<AppState>, Form(form): Form<TagEditForm>) -> StatusCode {
    let adding = parse_tag_list(&form.tags);
    for id in resolve_ids(&state.engine, &form.hashes) {
        let mut current = tags_of(&state.engine, id);
        current.extend(adding.iter().cloned());
        write_tags(&state.engine, id, &current);
    }
    // Recorded as well as applied, so a tag that only ever arrived this way
    // still shows up in `tags` for a client that wants to filter by it.
    let mut known = load_tags(&state.config.config_dir);
    known.extend(adding);
    save_tags(&state.config.config_dir, &known);
    StatusCode::OK
}

async fn remove_tags(State(state): State<AppState>, Form(form): Form<TagEditForm>) -> StatusCode {
    let going = parse_tag_list(&form.tags);
    for id in resolve_ids(&state.engine, &form.hashes) {
        let kept: BTreeSet<String> =
            tags_of(&state.engine, id).into_iter().filter(|tag| !going.contains(tag)).collect();
        write_tags(&state.engine, id, &kept);
    }
    StatusCode::OK
}

async fn create_category(
    State(state): State<AppState>,
    Form(form): Form<CreateCategoryForm>,
) -> StatusCode {
    let mut categories = load_categories(&state.config.config_dir);
    categories.insert(form.category, form.save_path);
    save_categories(&state.config.config_dir, &categories);
    StatusCode::OK
}

#[derive(Deserialize)]
struct RemoveCategoriesForm {
    /// qBittorrent joins more than one name with `\n`, the same as the
    /// category list a client shows for editing, rather than the `|` every
    /// hash list in this module uses.
    categories: String,
}

async fn remove_categories(
    State(state): State<AppState>,
    Form(form): Form<RemoveCategoriesForm>,
) -> StatusCode {
    let mut categories = load_categories(&state.config.config_dir);
    for name in form.categories.split('\n').map(str::trim).filter(|s| !s.is_empty()) {
        categories.remove(name);
    }
    save_categories(&state.config.config_dir, &categories);
    StatusCode::OK
}

#[derive(Deserialize)]
struct SetCategoryForm {
    hashes: String,
    category: String,
}

async fn set_category(
    State(state): State<AppState>,
    Form(form): Form<SetCategoryForm>,
) -> StatusCode {
    for id in resolve_ids(&state.engine, &form.hashes) {
        let mut labels = state.engine.labels(id);
        labels.insert("category".to_string(), form.category.clone());
        state.engine.set_labels(id, labels);
    }
    StatusCode::OK
}

// ---------------------------------------------------------------------------
// Driving: add, delete, pause, resume.
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
struct AddForm {
    /// Newline-separated, matching how qBittorrent's own clients send more
    /// than one link in a single call.
    urls: Option<String>,
    category: Option<String>,
    savepath: Option<String>,
    paused: Option<String>,
    tags: Option<String>,
}

fn resolve_path(download_dir: &Path, raw: &str) -> PathBuf {
    let path = PathBuf::from(raw);
    if path.is_absolute() { path } else { download_dir.join(path) }
}

/// Where a newly added torrent's files land, in order of what was actually
/// asked for: an explicit `savepath` first, then the destination recorded
/// against its category, and only then the plain download directory.
///
/// Sonarr and Radarr both call `createCategory` with no `savePath` at all:
/// see `harness/README.md`. Real qBittorrent's own rule for that case is to
/// use `<default save path>/<category name>`, and a category recorded here
/// with an empty path has to follow the same rule rather than being taken
/// literally: `PathBuf::from("")` resolves to the server's working
/// directory, which is not writable, and every torrent either of them adds
/// failed with a permission error the first time this was tried against a
/// real client instead of a guess at what one would send.
/// Where a category, or an explicit save path, says a download belongs.
///
/// This is the directory qBittorrent calls `save_path`, shared by every
/// torrent under that category. It is not where the files themselves land:
/// see `destination_for`.
fn save_path_for(state: &AppState, savepath: Option<&str>, category: Option<&str>) -> PathBuf {
    if let Some(raw) = savepath.filter(|s| !s.is_empty()) {
        return resolve_path(&state.config.download_dir, raw);
    }
    if let Some(category) = category.filter(|c| !c.is_empty()) {
        let categories = load_categories(&state.config.config_dir);
        if let Some(path) = categories.get(category) {
            if !path.is_empty() {
                return resolve_path(&state.config.download_dir, path);
            }
            return state.config.download_dir.join(category);
        }
    }
    state.config.download_dir.clone()
}

/// Where this particular torrent's files are written: a folder of its own,
/// inside the save path.
///
/// A torrent that carries no top level folder of its own writes its files
/// straight into whatever directory it is given. Two such torrents in one
/// category then share a directory, and nothing says which files belong to
/// which download. Sonarr refuses to import that, and it is right to: it
/// cannot tell either. It reports "Path matches client base download
/// directory" and the download sits finished and unusable for ever.
///
/// So every torrent gets its own folder. qBittorrent offers this as a content
/// layout setting and we simply always do it, because the alternative needs
/// the file list to decide, and a magnet does not have one until the swarm
/// answers, which is long after the destination has to be fixed.
fn destination_for(
    state: &AppState,
    savepath: Option<&str>,
    category: Option<&str>,
    url: &str,
) -> PathBuf {
    save_path_for(state, savepath, category).join(folder_name_for(url))
}

/// A directory name for one torrent, from whatever the source will tell us.
///
/// The magnet's display name if it has one, its info hash if it does not, and
/// the file's own stem for an uploaded `.torrent`. Something is always
/// available, and a folder named after a hash is ugly rather than wrong.
///
/// Sanitised, and not as a formality: the display name arrives inside a URI
/// that anybody can write, so `dn=../../etc` would otherwise place a download
/// outside the directory it was meant to land in.
fn folder_name_for(url: &str) -> String {
    let raw = dl_core::torrent::magnet_display_name(url)
        .or_else(|| dl_core::torrent::info_hash_of(url))
        .or_else(|| Path::new(url).file_stem().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_default();
    sanitise_folder(&raw)
}

/// Reduce a name to something safe to append to a path.
///
/// Separators and parent references are removed rather than escaped, because
/// there is no version of either that belongs in a single directory name.
fn sanitise_folder(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' => '-',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.').trim();
    if trimmed.is_empty() { "torrent".to_string() } else { trimmed.chars().take(120).collect() }
}

fn label_new_torrent(state: &AppState, id: DownloadId, form: &AddForm, save_path: &Path) {
    let mut labels = BTreeMap::new();
    labels.insert("added_on".to_string(), now_unix().to_string());
    // Kept because the engine only knows the folder this torrent writes into,
    // and qBittorrent's `save_path` is the directory above it, shared with
    // every other torrent in the same category. Deriving it from the parent
    // would be right until somebody sets a category whose path happens to
    // nest, so it is recorded rather than inferred.
    labels.insert("save_path".to_string(), save_path.display().to_string());
    if let Some(category) = form.category.as_deref().filter(|c| !c.is_empty()) {
        labels.insert("category".to_string(), category.to_string());
    }
    if let Some(tags) = form.tags.as_deref().filter(|t| !t.is_empty()) {
        labels.insert("tags".to_string(), tags.to_string());
    }
    state.engine.set_labels(id, labels);
    // `paused=true` asks for the torrent to be added but not started, which
    // Sonarr uses when it wants to inspect or reorder a batch before letting
    // any of it run. Pausing right after `add` rather than never starting it
    // in the first place keeps this one code path in front of the engine's
    // own queue instead of teaching it a second way to create a transfer.
    if form.paused.as_deref() == Some("true") {
        state.engine.pause(id);
    }
}

fn add_one(state: &AppState, url: String, form: &AddForm) {
    // librqbit keys its session by info hash, so a second add for a hash
    // already open attaches to the existing session and reports complete at
    // the *first* destination, leaving this one empty. Leave it alone, as
    // qBittorrent does.
    if let Some(hash) = dl_core::torrent::info_hash_of(&url)
        && find_by_hash(&state.engine, &hash).is_some()
    {
        return;
    }
    let save_path = save_path_for(state, form.savepath.as_deref(), form.category.as_deref());
    let destination =
        destination_for(state, form.savepath.as_deref(), form.category.as_deref(), &url);
    let id = state.engine.add(DownloadSpec::new(url, destination));
    label_new_torrent(state, id, form, &save_path);
}

fn add_urls_from(state: &AppState, urls: &str, form: &AddForm) -> usize {
    let mut added = 0;
    for url in urls.lines().map(str::trim).filter(|s| !s.is_empty()) {
        add_one(state, url.to_string(), form);
        added += 1;
    }
    added
}

/// Write an uploaded `.torrent` under the config directory and hand back the
/// path `classify` will read as a local file.
///
/// Named with a timestamp rather than the hash, because the hash is not known
/// until the backend has parsed the very file this function is about to
/// write: there is nothing else here safe to key it on.
fn save_uploaded_torrent(
    config_dir: &Path,
    original_name: &str,
    bytes: &[u8],
) -> std::io::Result<PathBuf> {
    let dir = config_dir.join("torrents");
    std::fs::create_dir_all(&dir)?;
    let safe_name = original_name.rsplit(['/', '\\']).next().filter(|s| !s.is_empty());
    let unique = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    let path = dir.join(format!("{unique}-{}", safe_name.unwrap_or("upload.torrent")));
    std::fs::write(&path, bytes)?;
    Ok(path)
}

async fn add_from_multipart(state: AppState, mut multipart: Multipart) -> Response {
    let mut form = AddForm::default();
    let mut uploads: Vec<(String, Vec<u8>)> = Vec::new();

    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(_) => return (StatusCode::BAD_REQUEST, "malformed upload").into_response(),
        };
        // Every branch below has to consume `field` to read it, so its name
        // is read out first.
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "torrents" => {
                let filename = field.file_name().unwrap_or("upload.torrent").to_string();
                match field.bytes().await {
                    Ok(bytes) => uploads.push((filename, bytes.to_vec())),
                    Err(_) => {
                        return (StatusCode::BAD_REQUEST, "could not read the uploaded file")
                            .into_response();
                    }
                }
            }
            "urls" => form.urls = field.text().await.ok(),
            "category" => form.category = field.text().await.ok(),
            "savepath" => form.savepath = field.text().await.ok(),
            "paused" => form.paused = field.text().await.ok(),
            "tags" => form.tags = field.text().await.ok(),
            _ => {
                // An unread field, kept for forward compatibility with a
                // client sending something this server does not act on yet:
                // see `set_share_limits` and `top_priority` for the same
                // idea applied to whole endpoints.
                let _ = field.bytes().await;
            }
        }
    }

    // Handled only once every field has been read, since a multipart body
    // carries no promise about which order its parts arrive in, and a
    // `category` or `savepath` field could easily follow the file it is
    // meant to apply to.
    let mut added = 0;
    if let Some(urls) = form.urls.as_deref() {
        added += add_urls_from(&state, urls, &form);
    }
    for (filename, bytes) in uploads {
        match save_uploaded_torrent(&state.config.config_dir, &filename, &bytes) {
            Ok(path) => {
                add_one(&state, path.to_string_lossy().into_owned(), &form);
                added += 1;
            }
            Err(_) => {
                return (StatusCode::INTERNAL_SERVER_ERROR, "could not store the upload")
                    .into_response();
            }
        }
    }

    if added > 0 {
        (StatusCode::OK, "Ok.").into_response()
    } else {
        (StatusCode::BAD_REQUEST, "nothing to add").into_response()
    }
}

/// `torrents/add` cannot commit to one body shape the way every other
/// handler here does: a plain link arrives form-encoded, but Prowlarr uploads
/// the file itself, as `multipart/form-data`, when a tracker needs a cookie
/// it will not hand over for a URL Braid could fetch on its own. Axum's own
/// `Form` and `Multipart` extractors each commit to a content type before
/// looking at the body, so the choice is made by hand from the header both of
/// them would otherwise have checked internally.
async fn add_torrents(State(state): State<AppState>, request: Request) -> Response {
    let is_multipart = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("multipart/"));

    if is_multipart {
        return match Multipart::from_request(request, &state).await {
            Ok(multipart) => add_from_multipart(state, multipart).await,
            Err(_) => (StatusCode::BAD_REQUEST, "could not read the upload").into_response(),
        };
    }

    let bytes = match to_bytes(request.into_body(), usize::MAX).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return (StatusCode::BAD_REQUEST, "could not read the request body").into_response();
        }
    };
    let form: AddForm = serde_urlencoded::from_bytes(&bytes).unwrap_or_default();
    let Some(urls) = form.urls.as_deref().filter(|s| !s.trim().is_empty()) else {
        return (StatusCode::BAD_REQUEST, "no urls given").into_response();
    };
    if add_urls_from(&state, urls, &form) > 0 {
        (StatusCode::OK, "Ok.").into_response()
    } else {
        (StatusCode::BAD_REQUEST, "nothing to add").into_response()
    }
}

#[derive(Deserialize)]
struct HashesForm {
    hashes: String,
}

async fn pause_torrents(State(state): State<AppState>, Form(form): Form<HashesForm>) -> StatusCode {
    for id in resolve_ids(&state.engine, &form.hashes) {
        state.engine.pause(id);
    }
    StatusCode::OK
}

async fn resume_torrents(
    State(state): State<AppState>,
    Form(form): Form<HashesForm>,
) -> StatusCode {
    for id in resolve_ids(&state.engine, &form.hashes) {
        state.engine.resume(id);
    }
    StatusCode::OK
}

#[derive(Deserialize)]
struct DeleteForm {
    hashes: String,
    #[serde(default, rename = "deleteFiles")]
    delete_files: bool,
}

async fn delete_torrents(
    State(state): State<AppState>,
    Form(form): Form<DeleteForm>,
) -> StatusCode {
    // A hash naming nothing is not an error: a client racing its own removal
    // against a stale poll must not see a failure it will only retry.
    for id in resolve_ids(&state.engine, &form.hashes) {
        state.engine.remove_with_files(id, form.delete_files);
    }
    StatusCode::OK
}

#[derive(Deserialize)]
struct ShareLimitsForm {
    hashes: String,
    #[serde(rename = "ratioLimit")]
    ratio_limit: Option<String>,
    #[serde(rename = "seedingTimeLimit")]
    seeding_time_limit: Option<String>,
    #[serde(rename = "inactiveSeedingTimeLimit")]
    inactive_seeding_time_limit: Option<String>,
}

/// Accepted and written to the transfer's labels, so a client that reads its
/// own request back sees what it asked for, but nothing here enforces a
/// ratio or a seeding time: there is no scheduler in this build watching for
/// either to tear a torrent down. Recording the request rather than
/// rejecting it is what lets Sonarr and Radarr set a limit defensively on
/// every add without logging an error for a feature this server does not
/// have yet.
async fn set_share_limits(
    State(state): State<AppState>,
    Form(form): Form<ShareLimitsForm>,
) -> StatusCode {
    for id in resolve_ids(&state.engine, &form.hashes) {
        let mut labels = state.engine.labels(id);
        if let Some(v) = &form.ratio_limit {
            labels.insert("ratio_limit".to_string(), v.clone());
        }
        if let Some(v) = &form.seeding_time_limit {
            labels.insert("seeding_time_limit".to_string(), v.clone());
        }
        if let Some(v) = &form.inactive_seeding_time_limit {
            labels.insert("inactive_seeding_time_limit".to_string(), v.clone());
        }
        state.engine.set_labels(id, labels);
    }
    StatusCode::OK
}

/// Accepted and acknowledged, changing nothing: the engine has a concurrency
/// limit, not a queue a transfer can be moved within, so there is no ordering
/// here for "top" to mean anything about. Refusing the request would make a
/// client log an error for a button its own interface still offers; doing
/// nothing and saying so here is the more honest of the two ways to fail.
async fn top_priority(State(state): State<AppState>, Form(form): Form<HashesForm>) -> StatusCode {
    let _ = resolve_ids(&state.engine, &form.hashes);
    StatusCode::OK
}

#[derive(Deserialize)]
struct ForceStartForm {
    hashes: String,
    value: String,
}

/// `value=true` resumes, which is what force-starting a queued torrent means
/// here. `value=false` is accepted and ignored: there is no per-torrent forced
/// flag to revert, only [`EngineConfig::max_concurrent`].
async fn set_force_start(
    State(state): State<AppState>,
    Form(form): Form<ForceStartForm>,
) -> StatusCode {
    if form.value == "true" {
        for id in resolve_ids(&state.engine, &form.hashes) {
            state.engine.resume(id);
        }
    }
    StatusCode::OK
}

// ---------------------------------------------------------------------------
// sync/maindata
// ---------------------------------------------------------------------------

/// Ticks on every call. There is exactly one of these per process, which is
/// the right scope: `rid` only has to convince a client it moved, not
/// identify which client asked.
static NEXT_RID: AtomicU64 = AtomicU64::new(1);

/// A full snapshot, every time, never a delta.
///
/// qBittorrent's real sync protocol tracks per-torrent removals between calls
/// and sends only what changed since the caller's `rid`. Reproducing that
/// correctly is a large surface for a payload this small: everything Braid
/// manages fits in one response with room to spare, so a delta would not make
/// this meaningfully cheaper, only somewhere subtle for it to be wrong.
/// `rid` still increments on every call, because a client checking that it
/// moved is the one part of the protocol worth honouring without the rest of
/// it.
async fn sync_maindata(State(state): State<AppState>) -> Json<Value> {
    let rid = NEXT_RID.fetch_add(1, Ordering::Relaxed);
    let snapshots = state.engine.snapshot();

    let mut torrents = Map::new();
    for snapshot in &snapshots {
        let Some(hash) = torrent_hash(&state.engine, snapshot) else { continue };
        let labels = state.engine.labels(snapshot.id);
        let destination = state.engine.destination(snapshot.id).unwrap_or_default();
        let entry = torrent_entry_json(&state.engine, snapshot, &hash, &labels, &destination);
        torrents.insert(hash, entry);
    }

    let categories: Map<String, Value> = load_categories(&state.config.config_dir)
        .into_iter()
        .map(|(name, save_path)| {
            let entry = json!({ "name": name, "savePath": save_path });
            (name, entry)
        })
        .collect();

    let dl_speed: u64 = snapshots.iter().map(|s| s.progress.bytes_per_sec).sum();
    let up_speed: u64 =
        snapshots.iter().filter_map(|s| s.torrent.as_ref()).map(|t| t.upload_bytes_per_sec).sum();

    Json(json!({
        "rid": rid,
        "full_update": true,
        "torrents": Value::Object(torrents),
        "categories": Value::Object(categories),
        "tags": [],
        "server_state": {
            "dl_info_speed": dl_speed,
            "up_info_speed": up_speed,
            "connection_status": "connected",
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dl_core::Progress;
    use dl_core::budget::Budget;
    use dl_core::engine::{Engine, EngineConfig, SourceFactory};
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
        /// Where each source's files were told to land, learned from `run`
        /// and consulted by `discard`, exactly as a real backend has to
        /// track it: `discard` is never told a destination.
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
            // Stands in for "already on disk", so a delete test can tell
            // whether `discard` actually removed anything.
            let _ = std::fs::create_dir_all(&request.destination);
            let _ = std::fs::write(request.destination.join("downloaded.iso"), b"data");

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

        async fn discard(&self, source: &TorrentSource, delete_files: bool) -> EngineResult<()> {
            if !delete_files {
                return Ok(());
            }
            if let Some(destination) = self.destinations.lock().unwrap().get(&source.key()) {
                let _ = std::fs::remove_file(destination.join("downloaded.iso"));
            }
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

    fn urlencode(s: &str) -> String {
        let mut out = String::new();
        for byte in s.bytes() {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(byte as char)
                }
                b' ' => out.push('+'),
                _ => out.push_str(&format!("%{byte:02X}")),
            }
        }
        out
    }

    fn form_body(pairs: &[(&str, &str)]) -> String {
        pairs
            .iter()
            .map(|(k, v)| format!("{}={}", urlencode(k), urlencode(v)))
            .collect::<Vec<_>>()
            .join("&")
    }

    async fn post_form(
        router: &Router,
        uri: &str,
        pairs: &[(&str, &str)],
    ) -> axum::http::Response<axum::body::Body> {
        let request = axum::http::Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(axum::body::Body::from(form_body(pairs)))
            .unwrap();
        router.clone().oneshot(request).await.unwrap()
    }

    fn multipart_body(boundary: &str, field: &str, filename: &str, bytes: &[u8]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!(
                "Content-Disposition: form-data; name=\"{field}\"; filename=\"{filename}\"\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(b"Content-Type: application/x-bittorrent\r\n\r\n");
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        body
    }

    async fn post_multipart(
        router: &Router,
        uri: &str,
        field: &str,
        filename: &str,
        bytes: &[u8],
    ) -> axum::http::Response<axum::body::Body> {
        const BOUNDARY: &str = "braid-test-boundary";
        let request = axum::http::Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", format!("multipart/form-data; boundary={BOUNDARY}"))
            .body(axum::body::Body::from(multipart_body(BOUNDARY, field, filename, bytes)))
            .unwrap();
        router.clone().oneshot(request).await.unwrap()
    }

    async fn body_bytes(response: axum::http::Response<axum::body::Body>) -> Vec<u8> {
        response.into_body().collect().await.unwrap().to_bytes().to_vec()
    }

    async fn body_text(response: axum::http::Response<axum::body::Body>) -> String {
        String::from_utf8(body_bytes(response).await).unwrap()
    }

    async fn json_body(response: axum::http::Response<axum::body::Body>) -> Value {
        serde_json::from_slice(&body_bytes(response).await).unwrap()
    }

    async fn list_json(router: &Router, query: &str) -> Vec<Value> {
        let response = get(router, &format!("/api/v2/torrents/info{query}")).await;
        json_body(response).await.as_array().unwrap().clone()
    }

    // -- Task 9: listing ----------------------------------------------------

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

    #[tokio::test]
    async fn properties_and_files_do_not_panic_before_the_torrent_is_known() {
        let hash = hash_n(13);
        let app = TestApp::with_backend(FakeTorrentBackend::new(None));
        let magnet = format!("magnet:?xt=urn:btih:{hash}&dn=Big+Buck+Bunny");
        app.state.engine.add(DownloadSpec::new(magnet, app.state.config.download_dir.clone()));
        let router = app.router();

        let props = get(&router, &format!("/api/v2/torrents/properties?hash={hash}")).await;
        assert_eq!(props.status(), StatusCode::OK);
        let props = json_body(props).await;
        assert_eq!(props["nb_connections"], 0);
        assert_eq!(props["up_total"], 0);
        assert_eq!(props["upload_speed"], 0);

        let files = get(&router, &format!("/api/v2/torrents/files?hash={hash}")).await;
        assert_eq!(files.status(), StatusCode::OK);
        assert!(json_body(files).await.as_array().unwrap().is_empty());
    }

    // -- Task 10: driving -----------------------------------------------

    #[tokio::test]
    async fn adding_by_magnet_takes_the_category_and_the_save_path() {
        let hash = hash_n(8);
        let app = TestApp::with_default_backend();
        let router = app.router();
        let magnet = format!("magnet:?xt=urn:btih:{hash}&dn=Show");

        let response = post_form(
            &router,
            "/api/v2/torrents/add",
            &[
                ("urls", &magnet),
                ("category", "tv-sonarr"),
                ("savepath", "/downloads/tv"),
                ("paused", "false"),
            ],
        )
        .await;
        assert_eq!(body_text(response).await, "Ok.");

        for _ in 0..200 {
            if find_by_hash(&app.state.engine, &hash).is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        let list = list_json(&router, "").await;
        assert_eq!(list[0]["category"], "tv-sonarr");
        assert_eq!(list[0]["save_path"], "/downloads/tv");
    }

    #[tokio::test]
    async fn re_adding_a_known_hash_under_a_different_category_does_not_create_a_second_torrent() {
        let hash = hash_n(20);
        let app = TestApp::with_default_backend();
        let router = app.router();
        let magnet = format!("magnet:?xt=urn:btih:{hash}&dn=Show");

        post_form(&router, "/api/v2/torrents/add", &[("urls", &magnet), ("category", "tv-sonarr")])
            .await;
        for _ in 0..200 {
            if find_by_hash(&app.state.engine, &hash).is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(app.state.engine.snapshot().len(), 1);

        post_form(
            &router,
            "/api/v2/torrents/add",
            &[("urls", &magnet), ("category", "movies-radarr")],
        )
        .await;
        // Nothing to poll for: a duplicate is either created immediately or
        // never at all.
        tokio::time::sleep(Duration::from_millis(20)).await;

        assert_eq!(
            app.state.engine.snapshot().len(),
            1,
            "adding a hash already open must not start a second, unreachable download"
        );
        let list = list_json(&router, "").await;
        assert_eq!(list[0]["category"], "tv-sonarr", "the original add must be left alone");
    }

    #[tokio::test]
    async fn adding_an_uploaded_torrent_file_works_as_well_as_a_link() {
        // Prowlarr uploads the file rather than handing over a URL when the
        // tracker needs a cookie it is not going to share.
        let app = TestApp::with_default_backend();
        let response = post_multipart(
            &app.router(),
            "/api/v2/torrents/add",
            "torrents",
            "x.torrent",
            b"fake torrent bytes",
        )
        .await;
        assert_eq!(body_text(response).await, "Ok.");
    }

    #[tokio::test]
    async fn several_hashes_in_one_call_are_all_acted_on() {
        // These endpoints take `hashes=a|b|c`, and handling only the first is
        // a bug that looks like flakiness.
        let hash_a = hash_n(9);
        let hash_b = hash_n(10);
        let app = TestApp::with_default_backend();
        app.add_magnet_and_wait(&hash_a).await;
        app.add_magnet_and_wait(&hash_b).await;
        let router = app.router();

        post_form(&router, "/api/v2/torrents/pause", &[("hashes", &format!("{hash_a}|{hash_b}"))])
            .await;

        let list = list_json(&router, "").await;
        assert!(list.iter().all(|t| t["state"].as_str().unwrap().starts_with("paused")));
    }

    #[tokio::test]
    async fn the_word_all_means_every_torrent() {
        let hash_a = hash_n(11);
        let hash_b = hash_n(12);
        let app = TestApp::with_default_backend();
        app.add_magnet_and_wait(&hash_a).await;
        app.add_magnet_and_wait(&hash_b).await;
        let router = app.router();

        post_form(&router, "/api/v2/torrents/pause", &[("hashes", "all")]).await;

        let list = list_json(&router, "").await;
        assert_eq!(
            list.iter().filter(|t| t["state"].as_str().unwrap().starts_with("paused")).count(),
            2
        );
    }

    #[tokio::test]
    async fn delete_only_removes_files_when_asked_to() {
        // `deleteFiles=false` is sent when the client has already imported
        // the data. Deleting anyway destroys the import.
        let hash = hash_n(13);
        let app = TestApp::with_default_backend();
        let id = app.add_magnet_and_wait(&hash).await;
        let destination = app.state.engine.destination(id).unwrap();
        let marker = destination.join("downloaded.iso");
        assert!(marker.exists(), "the fake backend should have written it");

        post_form(
            &app.router(),
            "/api/v2/torrents/delete",
            &[("hashes", &hash), ("deleteFiles", "false")],
        )
        .await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert!(marker.exists(), "the files were deleted anyway");
    }

    #[tokio::test]
    async fn an_unknown_hash_is_ignored_rather_than_an_error() {
        // A client deleting something already gone must not see a failure it
        // will retry forever.
        let app = TestApp::with_default_backend();
        let response = post_form(
            &app.router(),
            "/api/v2/torrents/delete",
            &[("hashes", &"0".repeat(40)), ("deleteFiles", "false")],
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn tags_are_created_listed_added_removed_and_deleted() {
        // Sonarr and Radarr both let somebody tag what they send to a client,
        // and a tag configured there means these endpoints get called. Without
        // them the client gets a 404 for something it has every reason to
        // expect, having been told this server is qBittorrent.
        let hash = hash_n(20);
        let app = TestApp::with_default_backend();
        let router = app.router();
        app.add_magnet_and_wait(&hash).await;

        post_form(&router, "/api/v2/torrents/createTags", &[("tags", "tv,archive")]).await;
        let listed: Vec<String> =
            serde_json::from_value(json_body(get(&router, "/api/v2/torrents/tags").await).await)
                .unwrap();
        assert!(listed.contains(&"tv".to_string()) && listed.contains(&"archive".to_string()));

        post_form(&router, "/api/v2/torrents/addTags", &[("hashes", &hash), ("tags", "tv")]).await;
        let list = list_json(&router, "").await;
        assert_eq!(list[0]["tags"], "tv");

        let tagged = list_json(&router, "?tag=tv").await;
        assert_eq!(tagged.len(), 1, "filtering by tag should find it");
        assert!(list_json(&router, "?tag=nothing").await.is_empty());

        post_form(&router, "/api/v2/torrents/removeTags", &[("hashes", &hash), ("tags", "tv")])
            .await;
        assert_eq!(list_json(&router, "").await[0]["tags"], "");
    }

    #[tokio::test]
    async fn deleting_a_tag_takes_it_off_everything_carrying_it() {
        // A tag gone from the list and still shown on a row has a client
        // asking to filter by something this server has just said is not there.
        let hash = hash_n(21);
        let app = TestApp::with_default_backend();
        let router = app.router();
        app.add_magnet_and_wait(&hash).await;

        post_form(&router, "/api/v2/torrents/addTags", &[("hashes", &hash), ("tags", "gone,kept")])
            .await;
        post_form(&router, "/api/v2/torrents/deleteTags", &[("tags", "gone")]).await;

        assert_eq!(list_json(&router, "").await[0]["tags"], "kept");
        let listed: Vec<String> =
            serde_json::from_value(json_body(get(&router, "/api/v2/torrents/tags").await).await)
                .unwrap();
        assert!(!listed.contains(&"gone".to_string()));
    }

    #[tokio::test]
    async fn a_tag_that_only_ever_arrived_on_a_torrent_is_still_listed() {
        // A client that tagged at add time never called createTags. Leaving
        // that tag out of the list would show it on a row and refuse to filter
        // by it.
        let hash = hash_n(22);
        let app = TestApp::with_default_backend();
        let router = app.router();
        let magnet = format!("magnet:?xt=urn:btih:{hash}&dn=Show");
        post_form(&router, "/api/v2/torrents/add", &[("urls", &magnet), ("tags", "from-add")])
            .await;
        for _ in 0..200 {
            if find_by_hash(&app.state.engine, &hash).is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        let listed: Vec<String> =
            serde_json::from_value(json_body(get(&router, "/api/v2/torrents/tags").await).await)
                .unwrap();
        assert!(listed.contains(&"from-add".to_string()));
    }

    #[tokio::test]
    async fn categories_are_created_listed_and_removed() {
        let app = TestApp::with_default_backend();
        let router = app.router();

        post_form(
            &router,
            "/api/v2/torrents/createCategory",
            &[("category", "tv-sonarr"), ("savePath", "/downloads/tv")],
        )
        .await;
        let categories = json_body(get(&router, "/api/v2/torrents/categories").await).await;
        assert_eq!(categories["tv-sonarr"]["savePath"], "/downloads/tv");

        post_form(&router, "/api/v2/torrents/removeCategories", &[("categories", "tv-sonarr")])
            .await;
        let after = json_body(get(&router, "/api/v2/torrents/categories").await).await;
        assert!(after.get("tv-sonarr").is_none());
    }

    #[tokio::test]
    async fn a_category_created_with_no_save_path_still_lands_somewhere_writable() {
        // Sonarr and Radarr both call createCategory with no savePath at all:
        // see harness/README.md. `destination_for` used to take that recorded
        // empty string literally, which resolves to the server's own working
        // directory rather than anywhere under the download directory, and
        // the first real add through either client failed with a permission
        // error rather than landing in the download directory the way real
        // qBittorrent's own "no save path set" category does.
        let hash = hash_n(14);
        let app = TestApp::with_default_backend();
        let router = app.router();
        let magnet = format!("magnet:?xt=urn:btih:{hash}&dn=Show");

        post_form(&router, "/api/v2/torrents/createCategory", &[("category", "tv-sonarr")]).await;
        post_form(&router, "/api/v2/torrents/add", &[("urls", &magnet), ("category", "tv-sonarr")])
            .await;

        for _ in 0..200 {
            if find_by_hash(&app.state.engine, &hash).is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let list = list_json(&router, "").await;
        assert_eq!(
            list[0]["save_path"],
            app.state.config.download_dir.join("tv-sonarr").display().to_string()
        );
    }
}
