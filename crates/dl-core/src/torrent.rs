//! What the engine knows about BitTorrent, which is deliberately very little.
//!
//! A torrent is not a [`ByteSource`](crate::source::ByteSource) that can be
//! range-requested: there is no origin, no `If-Range` validator and no chunk
//! the caller gets to choose. Forcing it through
//! [`download_over_lanes`](crate::resume::download_over_lanes) would mean
//! teaching that path to ignore everything it is built on. So the engine holds
//! a second path instead, reached through [`TorrentBackend`], and decides which
//! one a transfer takes from its URL.
//!
//! Nothing here speaks the protocol. The implementation lives in `dl-torrent`
//! behind a cargo feature, so a build without it links no BitTorrent code at
//! all and says so plainly when asked to open a magnet link.

use crate::budget::Budget;
use crate::cancel::Cancel;
use crate::error::Result;
use crate::model::Progress;
use std::path::PathBuf;
use std::sync::Arc;

/// Which of the engine's two transfer paths an input takes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransferKind {
    /// Fetched with byte ranges from an origin.
    Http,
    /// Fetched from a swarm through the torrent backend.
    Torrent(TorrentSource),
    /// A `magnet:` URI with no `xt=urn:btih:` topic.
    ///
    /// Named rather than folded into [`TransferKind::Http`]: such a link has
    /// no info hash, so there is nothing to look up, and reporting it as an
    /// unparseable URL would send someone looking for a typo in the host name
    /// of a link that has no host.
    IncompleteMagnet,
}

/// Where the torrent's metadata comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TorrentSource {
    /// A `magnet:` URI. The metadata is fetched from the swarm.
    Magnet(String),
    /// An HTTP(S) URL that serves a `.torrent` file.
    Url(String),
    /// A `.torrent` file already on disk.
    File(PathBuf),
}

impl TorrentSource {
    /// A stable key for this source, so a backend can find a torrent it
    /// started once the transfer that started it has gone.
    pub fn key(&self) -> String {
        match self {
            Self::Magnet(uri) => uri.clone(),
            Self::Url(url) => url.clone(),
            Self::File(path) => path.display().to_string(),
        }
    }

    /// A name to show before any metadata has arrived.
    ///
    /// A magnet link takes minutes to resolve on a cold swarm, and a row
    /// labelled with a forty-character hex hash for that whole time is a row
    /// nobody can find again. `dn=` is the publisher's own display name, which
    /// is the best guess available and is replaced the moment the real
    /// metadata lands.
    pub fn provisional_name(&self) -> Option<String> {
        match self {
            Self::Magnet(uri) => magnet_display_name(uri),
            Self::Url(url) => {
                let path = url.split(['?', '#']).next().unwrap_or(url);
                path.rsplit('/').next().filter(|s| !s.is_empty()).map(str::to_string)
            }
            Self::File(path) => path.file_name().map(|n| n.to_string_lossy().into_owned()),
        }
    }
}

/// One file inside a torrent, and how much of it is on disk.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TorrentFile {
    /// Relative to the torrent's own folder, with `/` separators.
    pub path: String,
    pub len: u64,
    pub downloaded: u64,
}

/// What a torrent has that an HTTP download does not.
///
/// Carried as one `Option` on the snapshot rather than as four loose fields,
/// so an HTTP transfer cannot report "0 peers" as though it had looked.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TorrentStatus {
    pub uploaded: u64,
    pub upload_bytes_per_sec: u64,
    /// Peers actually connected, not peers seen.
    pub peers: u32,
    pub files: Vec<TorrentFile>,
    /// The connected peers themselves, for the Inspector.
    ///
    /// Capped by the backend: a busy swarm is hundreds of peers and the panel
    /// shows a window onto it, not the whole list.
    pub peer_list: Vec<TorrentPeer>,
    /// The interface the session's sockets are bound to, when it is bound to
    /// one. `None` means the OS is routing, which is the usual case.
    pub interface: Option<String>,
}

/// One connected peer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TorrentPeer {
    pub address: String,
    /// What the peer says it is running, when it says.
    pub client: Option<String>,
    pub downloaded: u64,
    pub uploaded: u64,
    /// librqbit's own word for the connection state.
    pub state: String,
}

/// A progress report from the backend, pushed at whatever rate it samples.
#[derive(Clone, Debug, Default)]
pub struct TorrentProgress {
    pub progress: Progress,
    pub status: TorrentStatus,
    /// The torrent's own name, once the metadata has arrived.
    pub name: Option<String>,
    /// Every piece is on disk and the transfer is now giving rather than
    /// taking. Not terminal: the user ends it, or a setting does.
    pub seeding: bool,
    /// What the transfer is doing when it is not simply moving bytes: /// "Checking" while a resume re-validates what is on disk. `None` while it
    /// is downloading normally.
    pub phase: Option<String>,
}

pub type TorrentProgressFn = Box<dyn Fn(TorrentProgress) + Send + Sync>;

/// One transfer, as the backend is asked to run it.
pub struct TorrentRequest {
    /// Raised before cancelling when the user asked for the files to go with
    /// the transfer.
    ///
    /// The backend does the deleting, not the engine: librqbit created those
    /// files and is the only thing that knows exactly which they are. Guessing
    /// them from the output folder meant a single-file torrent whose folder
    /// name came from the link left its contents behind.
    pub delete_files: Arc<std::sync::atomic::AtomicBool>,
    pub source: TorrentSource,
    /// The **folder** the torrent's files are written under, not a file path.
    /// A torrent names its own contents and a single-file torrent is still a
    /// name the publisher chose, so the destination cannot be one we picked.
    /// The folder the torrent's files are written into, start to finish.
    ///
    /// Torrents do not use the incomplete folder that HTTP transfers can:
    /// a torrent that keeps seeding is still serving the files it downloaded,
    /// so they cannot be moved out from under it once the last piece lands.
    /// One folder throughout is the only behaviour that is the same whether a
    /// torrent seeds or not.
    pub destination: PathBuf,
    pub cancel: Cancel,
    /// The app-wide download ceiling. Zero means unlimited, as everywhere.
    pub download_limit: Arc<Budget>,
    /// Bytes per second currently being used by transfers that are not this
    /// backend's.
    ///
    /// librqbit enforces its own token bucket and the engine cannot hand it
    /// bytes from ours, so the two would otherwise each honour the same limit
    /// separately and together reach twice it. The backend takes the headroom
    /// that is left instead.
    pub other_traffic: Arc<std::sync::atomic::AtomicU64>,
    /// The upload ceiling, which only this path has anything to apply to.
    pub upload_limit: Arc<Budget>,
    /// Keep what is on disk when the transfer fails or is cancelled.
    pub keep_partial: bool,
    /// Stay in the swarm after the last piece arrives, until cancelled.
    ///
    /// False makes a torrent behave like a download and finish. True is what
    /// a torrent client normally does, and is the only way the swarm gets
    /// anything back.
    pub seed_after_complete: bool,
    pub on_progress: Option<TorrentProgressFn>,
}

/// What the transfer produced.
#[derive(Clone, Debug, Default)]
pub struct TorrentOutcome {
    pub total: u64,
    pub uploaded: u64,
    pub name: String,
    pub files: Vec<TorrentFile>,
}

/// The engine's whole view of BitTorrent.
///
/// One method, because the engine has nothing useful to say about pieces,
/// peers or trackers: it starts a transfer, watches the progress reports, and
/// stops it through the same [`Cancel`] every other transfer uses.
#[async_trait::async_trait]
pub trait TorrentBackend: Send + Sync + 'static {
    async fn run(&self, request: TorrentRequest) -> Result<TorrentOutcome>;

    /// Forget a torrent the backend is still holding, and delete its files if
    /// asked.
    ///
    /// Needed because a transfer that has already finished has no loop left to
    /// notice a cancellation. With seeding off, a completed torrent is parked
    /// and still registered; without this, removing it deleted nothing and
    /// adding the same link again resumed it from the files that were supposed
    /// to be gone.
    async fn discard(&self, source: &TorrentSource, delete_files: bool) -> Result<()>;
}

/// Decide which path an input takes, from the input alone.
///
/// Pure and total: no filesystem access and no network, because this runs on
/// every keystroke in the add sheet. A local path that does not exist is still
/// classified by its extension: the failure to open it belongs to the
/// backend, which can say which file it could not read.
pub fn classify(input: &str) -> TransferKind {
    let input = input.trim();

    if starts_with_scheme(input, "magnet:") {
        return match has_btih_topic(input) {
            true => TransferKind::Torrent(TorrentSource::Magnet(input.to_string())),
            false => TransferKind::IncompleteMagnet,
        };
    }

    if starts_with_scheme(input, "http://") || starts_with_scheme(input, "https://") {
        return match path_ends_in_torrent(input) {
            true => TransferKind::Torrent(TorrentSource::Url(input.to_string())),
            false => TransferKind::Http,
        };
    }

    if starts_with_scheme(input, "file://") {
        let path = &input["file://".len()..];
        return match path_ends_in_torrent(path) {
            true => TransferKind::Torrent(TorrentSource::File(PathBuf::from(path))),
            false => TransferKind::Http,
        };
    }

    // No scheme at all: a path. Only a `.torrent` is ours to claim; anything
    // else keeps the behaviour it had before torrents existed.
    if !input.contains("://") && path_ends_in_torrent(input) {
        return TransferKind::Torrent(TorrentSource::File(PathBuf::from(input)));
    }

    TransferKind::Http
}

/// The `dn` (display name) parameter of a magnet URI, percent-decoded.
pub fn magnet_display_name(uri: &str) -> Option<String> {
    let query = uri.split_once('?')?.1;
    let raw = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("dn=").or_else(|| pair.strip_prefix("dn.1=")))?;
    let decoded = percent_decode(raw);
    let trimmed = decoded.trim();
    match trimmed.is_empty() {
        true => None,
        false => Some(trimmed.to_string()),
    }
}

/// Schemes are case-insensitive, and people paste `MAGNET:?xt=...` out of
/// mail clients that upper-case them.
fn starts_with_scheme(input: &str, scheme: &str) -> bool {
    input.len() >= scheme.len() && input[..scheme.len()].eq_ignore_ascii_case(scheme)
}

/// Whether the magnet names a BitTorrent info hash.
///
/// `xt=urn:btih:` is the only topic this engine can act on. A magnet carrying
/// only `xs=` or `as=` web seeds, or a `urn:ed2k:` topic, describes something
/// we cannot join.
fn has_btih_topic(uri: &str) -> bool {
    let Some((_, query)) = uri.split_once('?') else { return false };
    query.split('&').any(|pair| {
        let Some((key, value)) = pair.split_once('=') else { return false };
        // `xt.1=` and `xt.2=` are how a multi-topic magnet is written.
        let is_topic =
            key.eq_ignore_ascii_case("xt") || key.len() > 3 && key[..3].eq_ignore_ascii_case("xt.");
        is_topic
            && value.len() > "urn:btih:".len()
            && value[.."urn:btih:".len()].eq_ignore_ascii_case("urn:btih:")
    })
}

/// Whether the **path** ends in `.torrent`, ignoring query and fragment.
///
/// A query string is not a filename: `…/get?file=x.torrent` is an endpoint
/// that may well serve HTML, and treating it as a torrent would hand the
/// backend a page of markup to parse as bencode.
fn path_ends_in_torrent(input: &str) -> bool {
    let path = input.split(['?', '#']).next().unwrap_or(input);
    // The last segment, not the whole path: a bare `/.torrent` is a dotfile
    // named after the extension, not a torrent called nothing.
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    name.len() > ".torrent".len()
        && name[name.len() - ".torrent".len()..].eq_ignore_ascii_case(".torrent")
}

/// Percent-decoding, plus `+` for space as `dn=` is written in practice.
///
/// Deliberately not a URL crate: this decodes one display name for one label,
/// and an invalid escape leaves the characters alone rather than failing: /// a mangled name is better than no name.
fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => match u8::from_str_radix(&raw[i + 1..i + 3], 16) {
                Ok(byte) => {
                    out.push(byte);
                    i += 3;
                }
                Err(_) => {
                    out.push(b'%');
                    i += 1;
                }
            },
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "cab507494d02ebb1178b38f2e9d7be299c86b862";

    fn magnet(query: &str) -> TransferKind {
        classify(&format!("magnet:?{query}"))
    }

    #[test]
    fn a_magnet_with_an_info_hash_is_a_torrent() {
        // The one form the backend can actually act on.
        let uri = format!("magnet:?xt=urn:btih:{HASH}&dn=ubuntu.iso");
        assert_eq!(
            classify(&uri),
            TransferKind::Torrent(TorrentSource::Magnet(uri.clone())),
            "a btih magnet must reach the torrent path"
        );
    }

    #[test]
    fn a_magnet_with_no_info_hash_is_not_a_magnet_we_can_open() {
        // Without `xt=urn:btih:` there is no swarm to join. Classifying this
        // as HTTP would report "invalid url" and send someone hunting for a
        // typo in a link that has no host to get wrong.
        assert_eq!(magnet("dn=something"), TransferKind::IncompleteMagnet);
        assert_eq!(magnet("xt=urn:ed2k:abc"), TransferKind::IncompleteMagnet);
        assert_eq!(classify("magnet:"), TransferKind::IncompleteMagnet);
        // A web seed alone is not a topic.
        assert_eq!(magnet("xs=https://example.test/a.torrent"), TransferKind::IncompleteMagnet);
    }

    #[test]
    fn a_multi_topic_magnet_counts_if_any_topic_is_bittorrent() {
        // `xt.1` / `xt.2` is how a magnet carrying more than one hash is
        // written; scanning only a bare `xt=` would reject those.
        assert!(matches!(
            magnet(&format!("xt.1=urn:ed2k:abc&xt.2=urn:btih:{HASH}")),
            TransferKind::Torrent(_)
        ));
    }

    #[test]
    fn the_magnet_scheme_is_matched_case_insensitively() {
        // Mail clients upper-case schemes, and a pasted `MAGNET:` that fell
        // through to the HTTP path failed with an unparseable-url error.
        let uri = format!("MAGNET:?XT=urn:btih:{HASH}");
        assert!(matches!(classify(&uri), TransferKind::Torrent(TorrentSource::Magnet(_))));
        let mixed = format!("magnet:?xt=URN:BTIH:{HASH}");
        assert!(matches!(classify(&mixed), TransferKind::Torrent(_)));
    }

    #[test]
    fn a_dot_torrent_url_is_a_torrent_but_one_in_a_query_string_is_not() {
        assert_eq!(
            classify("https://example.test/files/x.torrent"),
            TransferKind::Torrent(TorrentSource::Url(
                "https://example.test/files/x.torrent".into()
            ))
        );
        // `?file=x.torrent` is an endpoint that may well serve HTML. Handing
        // that to the backend means parsing a web page as bencode.
        assert_eq!(classify("https://example.test/get?file=x.torrent"), TransferKind::Http);
        assert_eq!(classify("https://example.test/a.torrent?token=1"), {
            TransferKind::Torrent(TorrentSource::Url(
                "https://example.test/a.torrent?token=1".into(),
            ))
        });
        assert_eq!(classify("https://example.test/a.iso"), TransferKind::Http);
        // The extension alone, with no name in front of it, is not a filename.
        assert_eq!(classify("https://example.test/.torrent"), TransferKind::Http);
    }

    #[test]
    fn a_local_dot_torrent_path_is_a_torrent() {
        assert_eq!(
            classify("./downloads/ubuntu.torrent"),
            TransferKind::Torrent(TorrentSource::File("./downloads/ubuntu.torrent".into()))
        );
        assert_eq!(
            classify("/Users/x/Ubuntu.TORRENT"),
            TransferKind::Torrent(TorrentSource::File("/Users/x/Ubuntu.TORRENT".into()))
        );
        assert_eq!(
            classify("file:///tmp/a.torrent"),
            TransferKind::Torrent(TorrentSource::File("/tmp/a.torrent".into()))
        );
        // A plain path with no torrent extension keeps whatever behaviour it
        // would otherwise have.
        assert_eq!(classify("/tmp/a.iso"), TransferKind::Http);
    }

    #[test]
    fn surrounding_whitespace_does_not_change_the_answer() {
        // Pasting from a web page brings a trailing newline with it.
        let uri = format!("  magnet:?xt=urn:btih:{HASH}\n");
        assert!(matches!(classify(&uri), TransferKind::Torrent(_)));
    }

    #[test]
    fn a_magnets_display_name_is_decoded_for_the_row_label() {
        // Before the metadata arrives this is the only name there is, and a
        // row labelled with a hex hash is a row nobody finds again.
        let uri = format!("magnet:?xt=urn:btih:{HASH}&dn=Ubuntu%2024.04%20%28amd64%29");
        assert_eq!(magnet_display_name(&uri).as_deref(), Some("Ubuntu 24.04 (amd64)"));

        let plus = format!("magnet:?xt=urn:btih:{HASH}&dn=two+words");
        assert_eq!(magnet_display_name(&plus).as_deref(), Some("two words"));

        // No `dn` at all, and a `dn` that decodes to nothing, both mean "no
        // name" rather than an empty row label.
        assert_eq!(magnet_display_name(&format!("magnet:?xt=urn:btih:{HASH}")), None);
        assert_eq!(magnet_display_name(&format!("magnet:?xt=urn:btih:{HASH}&dn=%20")), None);
    }

    #[test]
    fn a_truncated_escape_leaves_the_characters_alone() {
        // A mangled name beats no name, and beats a panic on a slice that is
        // not on a character boundary.
        assert_eq!(percent_decode("a%"), "a%");
        assert_eq!(percent_decode("a%2"), "a%2");
        assert_eq!(percent_decode("a%zz"), "a%zz");
    }

    #[test]
    fn provisional_names_come_from_the_source_itself() {
        let uri = format!("magnet:?xt=urn:btih:{HASH}&dn=Thing");
        assert_eq!(TorrentSource::Magnet(uri).provisional_name().as_deref(), Some("Thing"));
        assert_eq!(
            TorrentSource::Url("https://example.test/a/b.torrent?x=1".into())
                .provisional_name()
                .as_deref(),
            Some("b.torrent")
        );
        assert_eq!(
            TorrentSource::File("/tmp/c.torrent".into()).provisional_name().as_deref(),
            Some("c.torrent")
        );
    }
}
