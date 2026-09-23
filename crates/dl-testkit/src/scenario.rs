//! Ways an HTTP origin can misbehave.
//!
//! Named so the same catalogue drives integration tests, CI, and the in-app
//! scenario runner, and a protocol bug can be reproduced by name.

/// The payload seed. Fixed so digests are stable across runs.
pub const SEED: u64 = 0x5EED_1234_ABCD_0001;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scenario {
    /// A well-behaved origin: 200, correct `Content-Length`, full body.
    Ok200 { size: u64 },

    /// 404 with an HTML body, which must not be written to disk as content.
    NotFound,

    /// `hops` redirects before the real content.
    RedirectChain { hops: u8, size: u64 },

    /// Declares `declared` bytes but sends `actual`, then closes.
    TruncatedBody { declared: u64, actual: u64 },

    /// Dribbles the body out at roughly `bytes_per_sec`.
    SlowStream { size: u64, bytes_per_sec: u64 },

    /// Drops the connection abruptly after `reset_at` bytes.
    ResetAtOffset { size: u64, reset_at: u64 },

    /// Ignores `Accept-Encoding: identity` and labels the body `gzip`. The
    /// body is not actually gzip: a correct client refuses before reading it.
    ClaimsGzip { size: u64 },

    /// Advertises `Accept-Ranges: bytes` and then answers every range request
    /// with the whole body and a 200. The most common real-world range bug.
    AcceptRangesLies { size: u64 },

    /// Returns 206 with a correct `Content-Range`, but sends the bytes from
    /// offset zero instead of the requested ones. Only per-chunk hashing
    /// catches this; the lengths are all correct.
    WrongBytesForRange { size: u64 },

    /// Returns 206 for a span other than the one requested.
    ContentRangeMismatch { size: u64 },

    /// The ETag changes after `after_requests` requests, as it would if the
    /// file were replaced mid-download.
    EtagChangesMidDownload { size: u64, after_requests: u64 },

    /// Corrupts `len` bytes at `offset`, with every length reported correctly.
    CorruptBytesAt { size: u64, offset: u64, len: u64 },

    /// A signed link with a lifetime. Requests carrying a token older than
    /// `lifetime_secs`, and requests carrying no token at all, get 403.
    ExpiresAfterSeconds { size: u64, lifetime_secs: u64 },

    /// A signed link good for `after_bytes` and no more, so it dies part-way
    /// through a download rather than before it starts.
    ExpiresAfterBytes { size: u64, after_bytes: u64 },

    /// The original link has been withdrawn. Only a re-issued one works.
    Returns410Gone { size: u64 },

    /// After `after_bytes`, a login page: `200`, correct `Content-Length`,
    /// `text/html`. Every status and length check passes and the body is not
    /// the file. This is the expiry that silently corrupts downloads.
    HtmlErrorBodyWith200 { size: u64, after_bytes: u64 },

    /// The signature is bound to the address it was issued to, so one signed
    /// link 403s on every network path but one.
    SignedPerSourceIp { size: u64 },

    /// A mirror serving different bytes at the same length. Interchangeable by
    /// every check except the validator.
    MirrorDisagrees { size: u64 },

    /// Refuses the first `refusals` requests with `429` and a `Retry-After`,
    /// then serves normally.
    ///
    /// `retry_after_secs` of `None` omits the header, which is the case a
    /// client has to back off on its own for.
    RateLimited429 { size: u64, refusals: u64, retry_after_secs: Option<u64> },

    /// Refuses the first `refusals` requests with `503` and no `Retry-After`.
    /// An overloaded origin rather than a deliberate limit.
    Unavailable503 { size: u64, refusals: u64 },
}

impl Scenario {
    /// Stable name, used by `dl dev scenario` and in test output.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Ok200 { .. } => "ok200",
            Self::NotFound => "not-found",
            Self::RedirectChain { .. } => "redirect-chain",
            Self::TruncatedBody { .. } => "truncated-body",
            Self::SlowStream { .. } => "slow-stream",
            Self::RateLimited429 { .. } => "rate-limited-429",
            Self::Unavailable503 { .. } => "unavailable-503",
            Self::ResetAtOffset { .. } => "reset-at-offset",
            Self::ClaimsGzip { .. } => "claims-gzip",
            Self::AcceptRangesLies { .. } => "accept-ranges-lies",
            Self::WrongBytesForRange { .. } => "wrong-bytes-for-range",
            Self::ContentRangeMismatch { .. } => "content-range-mismatch",
            Self::EtagChangesMidDownload { .. } => "etag-changes-mid-download",
            Self::CorruptBytesAt { .. } => "corrupt-bytes-at",
            Self::ExpiresAfterSeconds { .. } => "expires-after-seconds",
            Self::ExpiresAfterBytes { .. } => "expires-after-bytes",
            Self::Returns410Gone { .. } => "returns-410-gone",
            Self::HtmlErrorBodyWith200 { .. } => "html-error-body-with-200",
            Self::SignedPerSourceIp { .. } => "signed-per-source-ip",
            Self::MirrorDisagrees { .. } => "mirror-disagrees",
        }
    }

    /// Whether this origin only works for a client that re-resolves its link.
    ///
    /// These are excluded from the two catalogue sweeps, which drive a client
    /// with no refresher: the sweeps assert "right bytes or no file", and
    /// against a signed origin a plain client can only ever produce no file,
    /// so including them would test nothing. `html-error-body-with-200` would
    /// be worse than useless there: a plain client writes the login page and
    /// reports success, which is the bug the scenario exists to demonstrate.
    /// They are exercised in full by the link-refresh tests instead.
    pub fn requires_refresh(&self) -> bool {
        matches!(
            self,
            Self::ExpiresAfterSeconds { .. }
                | Self::ExpiresAfterBytes { .. }
                | Self::Returns410Gone { .. }
                | Self::HtmlErrorBodyWith200 { .. }
                | Self::SignedPerSourceIp { .. }
        )
    }

    /// How long an issued link lives, where the scenario gives one a lifetime.
    /// Stated in the issued URL, so a client can refresh before it expires.
    pub fn link_lifetime(&self) -> Option<std::time::Duration> {
        match *self {
            Self::ExpiresAfterSeconds { lifetime_secs, .. } => {
                Some(std::time::Duration::from_secs(lifetime_secs))
            }
            _ => None,
        }
    }

    /// The payload this origin actually serves. A disagreeing mirror serves a
    /// different file of exactly the same length.
    pub fn payload_seed(&self) -> u64 {
        match self {
            Self::MirrorDisagrees { .. } => SEED ^ 0xA5A5_A5A5_A5A5_A5A5,
            _ => SEED,
        }
    }

    /// The resource's true length, where the scenario defines one.
    pub fn size(&self) -> Option<u64> {
        match *self {
            Self::NotFound | Self::TruncatedBody { .. } => None,
            Self::Ok200 { size }
            | Self::RedirectChain { size, .. }
            | Self::SlowStream { size, .. }
            | Self::RateLimited429 { size, .. }
            | Self::Unavailable503 { size, .. }
            | Self::ResetAtOffset { size, .. }
            | Self::ClaimsGzip { size }
            | Self::AcceptRangesLies { size }
            | Self::WrongBytesForRange { size }
            | Self::ContentRangeMismatch { size }
            | Self::EtagChangesMidDownload { size, .. }
            | Self::CorruptBytesAt { size, .. }
            | Self::ExpiresAfterSeconds { size, .. }
            | Self::ExpiresAfterBytes { size, .. }
            | Self::Returns410Gone { size }
            | Self::HtmlErrorBodyWith200 { size, .. }
            | Self::SignedPerSourceIp { size }
            | Self::MirrorDisagrees { size } => Some(size),
        }
    }

    /// Whether a single whole-body `GET` must fail.
    ///
    /// Separate from [`Scenario::fails_chunked`] because most range bugs are
    /// invisible to a client that never sends `Range`: the origin answers one
    /// unranged request perfectly well and only misbehaves when asked for a
    /// span. A single flag would have to lie about one path or the other.
    pub fn fails_whole_body(&self) -> bool {
        self.requires_refresh()
            || matches!(
                self,
                Self::NotFound
                    | Self::TruncatedBody { .. }
                    | Self::ResetAtOffset { .. }
                    | Self::ClaimsGzip { .. }
            )
    }

    /// Whether a parallel, ranged download must fail.
    pub fn fails_chunked(&self) -> bool {
        self.fails_whole_body()
            || matches!(
                self,
                Self::AcceptRangesLies { .. }
                    | Self::WrongBytesForRange { .. }
                    | Self::ContentRangeMismatch { .. }
                    | Self::EtagChangesMidDownload { .. }
                    | Self::CorruptBytesAt { .. }
            )
    }

    /// How many bytes a whole-body download should produce, if it can succeed.
    pub fn expected_len(&self) -> Option<u64> {
        if self.fails_whole_body() { None } else { self.size() }
    }

    /// The digest a correct whole-body download should compute.
    pub fn expected_digest(&self) -> Option<blake3::Hash> {
        self.expected_len().map(|len| crate::fixtures::digest(self.payload_seed(), len))
    }

    /// The digest of the resource's true content, whatever the origin does
    /// with it. This is what a chunked download must produce or refuse.
    pub fn true_digest(&self) -> Option<blake3::Hash> {
        self.size().map(|len| crate::fixtures::digest(self.payload_seed(), len))
    }

    /// Whether a correct whole-body client must fail rather than produce a file.
    pub fn must_fail(&self) -> bool {
        self.fails_whole_body()
    }

    /// Whether the origin needs per-request state, so it must not be shared
    /// between independent test runs.
    pub fn is_stateful(&self) -> bool {
        self.requires_refresh() || matches!(self, Self::EtagChangesMidDownload { .. })
    }

    /// Every scenario, with sizes small enough to keep tests fast.
    pub fn catalogue() -> Vec<Scenario> {
        vec![
            Self::Ok200 { size: 1 << 20 },
            Self::NotFound,
            Self::RedirectChain { hops: 3, size: 64 << 10 },
            Self::TruncatedBody { declared: 1 << 20, actual: 300 << 10 },
            Self::SlowStream { size: 32 << 10, bytes_per_sec: 256 << 10 },
            Self::ResetAtOffset { size: 1 << 20, reset_at: 400 << 10 },
            Self::ClaimsGzip { size: 64 << 10 },
            Self::AcceptRangesLies { size: 1 << 20 },
            Self::WrongBytesForRange { size: 1 << 20 },
            Self::ContentRangeMismatch { size: 1 << 20 },
            Self::EtagChangesMidDownload { size: 4 << 20, after_requests: 3 },
            Self::CorruptBytesAt { size: 1 << 20, offset: 700 << 10, len: 64 },
            Self::ExpiresAfterSeconds { size: 1 << 20, lifetime_secs: 2 },
            Self::ExpiresAfterBytes { size: 1 << 20, after_bytes: 256 << 10 },
            Self::Returns410Gone { size: 256 << 10 },
            Self::HtmlErrorBodyWith200 { size: 1 << 20, after_bytes: 256 << 10 },
            Self::SignedPerSourceIp { size: 512 << 10 },
            Self::MirrorDisagrees { size: 1 << 20 },
        ]
    }
}

impl std::fmt::Display for Scenario {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}
