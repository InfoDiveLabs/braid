//! Engine errors.

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("the origin returned HTTP {status}")]
    Http { status: u16 },

    #[error("transport failure: {0}")]
    Transport(String),

    /// Body ended before `Content-Length` was satisfied.
    #[error("the body ended early: expected {expected} bytes, received {received}")]
    ShortBody { expected: u64, received: u64 },

    #[error("the origin sent more data than the declared {expected} bytes")]
    OverlongBody { expected: u64 },

    /// Byte ranges would address compressed bytes.
    #[error("the origin applied Content-Encoding: {0}, which is unsafe for ranged requests")]
    UnexpectedContentEncoding(String),

    #[error("integrity check failed: expected {expected}, computed {actual}")]
    IntegrityMismatch { expected: String, actual: String },

    /// Chunk `index` did not hash to what the journal recorded.
    #[error("chunk {index} failed its integrity check")]
    ChunkCorrupt { index: u64 },

    /// The resource changed while the download was in progress. Continuing
    /// would splice bytes from two different versions into one file.
    #[error("the resource changed on the server during the download ({detail})")]
    ResourceChanged { detail: String },

    /// The origin accepted a range request and answered with something else.
    /// Distinct from a transport failure: retrying gets the same wrong answer.
    #[error("the origin did not honour the byte range: {detail}")]
    RangeNotHonoured { detail: String },

    #[error("i/o error on {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("invalid url: {0}")]
    InvalidUrl(String),

    /// Every network path has been taken out of rotation.
    #[error("every network path failed; no route left to retry on")]
    NoRouteAvailable,

    /// The link expired and could not be replaced with a working one.
    #[error("the link could not be refreshed: {detail}")]
    RefreshExhausted { detail: String },

    /// A mirror is not serving the same resource as the others.
    #[error("a mirror disagrees about the resource: {detail}")]
    MirrorDisagrees { detail: String },

    /// The origin asked us to slow down, and said for how long if it was
    /// polite about it.
    ///
    /// Separate from [`Error::Http`] because the correct response is different:
    /// not "try another path", which is what makes a rate limit worse, but
    /// "wait, then try the same one".
    #[error("the origin is rate limiting us (HTTP {status})")]
    RateLimited {
        status: u16,
        /// From `Retry-After`. `None` means the origin did not say, and the
        /// caller should fall back to its own backoff.
        retry_after: Option<std::time::Duration>,
    },

    /// Something went wrong inside the torrent backend.
    ///
    /// One variant rather than a translation of every librqbit failure: the
    /// engine has no decision to make on the difference between a tracker
    /// timeout and a peer disconnect, and inventing the taxonomy here would
    /// mean `dl-core` describing a protocol it deliberately does not know.
    #[error("torrent transfer failed: {0}")]
    Torrent(String),

    /// The torrent itself could not be understood.
    ///
    /// Separate from [`Error::Torrent`] because the correct response differs:
    /// a swarm that went quiet is worth asking again, while a file that is not
    /// bencode will not become bencode. Retrying that one spent the whole
    /// retry budget re-reading the same bytes.
    #[error("the torrent could not be read: {0}")]
    TorrentMetadata(String),

    /// A torrent was handed to a build with no torrent backend registered.
    #[error(
        "torrent support is not compiled in, so {link} cannot be opened; \
         build with the `torrent` feature to enable it"
    )]
    TorrentUnsupported { link: String },

    #[error("download was cancelled")]
    Cancelled,
}

impl Error {
    pub fn io(path: impl std::fmt::Display, source: std::io::Error) -> Self {
        Self::Io { path: path.to_string(), source }
    }

    /// Whether retrying the same request could plausibly succeed.
    ///
    /// Conservative: anything indicating wrong *content* is not retryable.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Transport(_) | Self::ShortBody { .. } | Self::ChunkCorrupt { .. } => true,
            // A swarm that went quiet is worth asking again, and every piece
            // already on disk passed its own hash, so a retry costs nothing
            // but the pieces in flight.
            Self::Torrent(_) => true,
            // 407 is the proxy refusing, not the origin. It belongs with the
            // rest only because of what "retryable" means here: the chunk is
            // requeued onto a different lane and this one accumulates a
            // failure until it is parked. Without it, a relay that stops
            // recognising us mid-transfer, because someone pressed Forget on
            // their phone, fails the whole download instead of costing it one
            // path. The other lanes are unaffected and can finish the file.
            Self::Http { status } => matches!(status, 407 | 408 | 429 | 500..=599),
            Self::RateLimited { .. } => true,
            Self::OverlongBody { .. }
            | Self::ResourceChanged { .. }
            | Self::RangeNotHonoured { .. }
            | Self::UnexpectedContentEncoding(_)
            | Self::IntegrityMismatch { .. }
            | Self::Io { .. }
            | Self::InvalidUrl(_)
            | Self::NoRouteAvailable
            | Self::RefreshExhausted { .. }
            | Self::MirrorDisagrees { .. }
            | Self::TorrentUnsupported { .. }
            | Self::TorrentMetadata(_)
            | Self::Cancelled => false,
        }
    }
}

impl Error {
    /// Whether this failure means the bytes already on disk cannot be trusted.
    ///
    /// Distinct from [`Error::is_retryable`]. Two failures are non-retryable
    /// and still leave the partial file intact: a pause, which is a decision
    /// rather than a fault, and a link that could not be refreshed, which says
    /// nothing about the bytes already written.
    pub fn invalidates_partial_data(&self) -> bool {
        !self.is_retryable() && !matches!(self, Self::Cancelled | Self::RefreshExhausted { .. })
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryable_classification() {
        assert!(Error::Transport("reset".into()).is_retryable());
        assert!(Error::ShortBody { expected: 10, received: 4 }.is_retryable());
        assert!(Error::Http { status: 503 }.is_retryable());
        // A phone that stopped recognising us costs its own lane, never the
        // file: the chunk moves to another path and that lane is parked.
        assert!(Error::Http { status: 407 }.is_retryable());
        assert!(Error::Http { status: 429 }.is_retryable());

        // A 404 will still be a 404, and cancellation is a decision, not a fault.
        assert!(!Error::Http { status: 404 }.is_retryable());
        assert!(!Error::Cancelled.is_retryable());

        // A corrupt chunk is worth re-fetching; that is the point of hashing
        // per chunk rather than only at the end.
        assert!(Error::ChunkCorrupt { index: 3 }.is_retryable());

        // A link that could not be refreshed says nothing about the bytes
        // already on disk, so those are kept for the next attempt.
        assert!(!Error::RefreshExhausted { detail: "cap".into() }.is_retryable());
        assert!(!Error::RefreshExhausted { detail: "cap".into() }.invalidates_partial_data());

        // A pause must keep everything already written: it is the one failure
        // that is neither retryable nor a reason to throw work away.
        assert!(!Error::Cancelled.invalidates_partial_data());
        assert!(Error::ResourceChanged { detail: "etag".into() }.invalidates_partial_data());
        assert!(!Error::Transport("reset".into()).invalidates_partial_data());

        // Retrying cannot fix content that is wrong rather than missing.
        assert!(!Error::UnexpectedContentEncoding("gzip".into()).is_retryable());
        assert!(!Error::ResourceChanged { detail: "etag".into() }.is_retryable());
        // Retrying a server that ignores Range just gets the same wrong answer.
        assert!(!Error::RangeNotHonoured { detail: "200".into() }.is_retryable());
        assert!(
            !Error::IntegrityMismatch { expected: "a".into(), actual: "b".into() }.is_retryable()
        );
    }

    #[test]
    fn a_torrent_failure_keeps_the_pieces_already_on_disk() {
        // Every piece a torrent wrote passed its own hash before it landed, so
        // a swarm going quiet is never a reason to start again from zero.
        assert!(Error::Torrent("no peers".into()).is_retryable());
        assert!(!Error::Torrent("no peers".into()).invalidates_partial_data());
    }

    #[test]
    fn a_torrent_that_is_not_bencode_is_not_worth_reading_again() {
        // A truncated or mislabelled `.torrent` will not become valid bencode
        // on a second reading, so retrying only spends time.
        let e = Error::TorrentMetadata("invalid value".into());
        assert!(!e.is_retryable());
        assert!(Error::Torrent("peer reset".into()).is_retryable());
    }

    #[test]
    fn a_build_without_torrent_support_says_so_instead_of_retrying() {
        // Retrying would spend the whole retry budget rediscovering that the
        // code is not linked in, and the message must name the missing
        // feature rather than reading as a broken link.
        let e = Error::TorrentUnsupported { link: "magnet:?xt=urn:btih:ab".into() };
        assert!(!e.is_retryable());
        let text = e.to_string();
        assert!(text.contains("torrent"), "{text}");
        assert!(text.contains("feature"), "{text}");
    }
}
