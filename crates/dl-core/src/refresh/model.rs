//! The types a link refresher works in.
//!
//! A resolved source is keyed by `(source, lane)` rather than by source alone:
//! some signers bind a signature to the requesting address, so one signed URL
//! can be valid on one interface and rejected on every other.

use std::sync::Arc;
use std::time::{Duration, Instant};

/// Which resolved source this is. Two lanes of the same source are separate
/// identities because a signature can be bound to the address that asked for
/// it: an S3 pre-signed URL issued over Ethernet 403s over Wi-Fi.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SourceKey {
    pub source_id: String,
    pub lane: usize,
}

impl SourceKey {
    pub fn new(source_id: impl Into<String>, lane: usize) -> Self {
        Self { source_id: source_id.into(), lane }
    }
}

impl std::fmt::Display for SourceKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}#{}", self.source_id, self.lane)
    }
}

/// Where the bytes are right now, and until when.
#[derive(Clone, Debug)]
pub struct ResolvedSource {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub expires_at: Option<Instant>,
    pub generation: u64,
    /// When this resolution was made.
    ///
    /// Not in the original design, and proactive refresh cannot work without
    /// it: refreshing at 80% of a lifetime needs both ends of that lifetime,
    /// and `expires_at` alone gives only one.
    pub issued_at: Instant,
}

impl ResolvedSource {
    /// A source with no known expiry. The generation is assigned by the
    /// [`crate::refresh::SourceHandle`] that stores it, so refreshers leave
    /// it at zero.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            headers: Vec::new(),
            expires_at: None,
            generation: 0,
            issued_at: Instant::now(),
        }
    }

    pub fn with_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.headers = headers;
        self
    }

    pub fn expiring_in(mut self, lifetime: Duration) -> Self {
        self.expires_at = Some(self.issued_at + lifetime);
        self
    }

    /// When this source should be replaced, at `fraction` of its lifetime.
    ///
    /// The point of refreshing early is that the common case becomes zero
    /// errors rather than one 403 per connection followed by recovery.
    pub fn refresh_at(&self, fraction: f64) -> Option<Instant> {
        let expires_at = self.expires_at?;
        let lifetime = expires_at.checked_duration_since(self.issued_at)?;
        Some(self.issued_at + lifetime.mul_f64(fraction.clamp(0.0, 1.0)))
    }

    pub fn is_due_for_refresh(&self, fraction: f64, now: Instant) -> bool {
        self.refresh_at(fraction).is_some_and(|due| now >= due)
    }
}

/// Why a resolution was asked for. Refreshers that mint credentials sometimes
/// need to know, and it makes the logs readable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefreshReason {
    /// The lifetime is nearly up; nothing has failed yet.
    Proactive,
    /// The origin rejected the link, or answered with something that is not
    /// the resource.
    Rejected,
    /// A 404, which may mean expiry and may mean the resource is gone.
    Ambiguous,
}

/// What a refresher is given when it is asked to resolve.
#[derive(Clone, Debug)]
pub struct RefreshCtx {
    pub key: SourceKey,
    /// The URL the download started with, before any refresh.
    pub original_url: String,
    /// What is believed to be the source right now.
    pub current: Option<Arc<ResolvedSource>>,
    pub reason: RefreshReason,
    /// How many times this source has already been refreshed.
    pub attempt: u32,
}

/// What came back, in the terms staleness detection needs.
#[derive(Clone, Debug, Default)]
pub struct ResponseSummary {
    pub status: u16,
    pub content_type: Option<String>,
    /// The content type this resource had when it was first probed.
    ///
    /// Without it an HTML body cannot be judged: a download of a web page is
    /// supposed to be `text/html`, and a login redirect looks identical.
    pub expected_content_type: Option<String>,
    pub content_length: Option<u64>,
    /// Whether the request carried a `Range` header.
    pub ranged: bool,
    pub headers: Vec<(String, String)>,
}

impl ResponseSummary {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Staleness {
    Fresh,
    /// The link is definitely no longer usable.
    Stale,
    /// It might be expiry and it might be the truth. Worth one refresh.
    Ambiguous,
}

/// Limits on how hard a dead link may be chased.
#[derive(Clone, Copy, Debug)]
pub struct RefreshPolicy {
    /// Total refreshes allowed across every lane of one download.
    pub max_refreshes: u32,
    pub base_backoff: Duration,
    pub max_backoff: Duration,
    /// Consecutive resolution failures before the source is given up on.
    pub failures_before_open: u32,
    /// Fraction of a link's lifetime after which it is replaced proactively.
    pub proactive_fraction: f64,
}

impl Default for RefreshPolicy {
    fn default() -> Self {
        Self {
            max_refreshes: 20,
            base_backoff: Duration::from_millis(250),
            max_backoff: Duration::from_secs(30),
            failures_before_open: 3,
            proactive_fraction: 0.8,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_link_is_replaced_before_it_expires_not_after() {
        let source = ResolvedSource::new("http://x.test/f").expiring_in(Duration::from_secs(100));
        let due = source.refresh_at(0.8).expect("a lifetime implies a refresh point");
        assert!(due < source.expires_at.unwrap(), "refreshing at expiry is already too late");
        assert!(!source.is_due_for_refresh(0.8, source.issued_at));
        assert!(source.is_due_for_refresh(0.8, source.issued_at + Duration::from_secs(81)));
    }

    #[test]
    fn a_source_with_no_known_lifetime_is_never_refreshed_proactively() {
        let source = ResolvedSource::new("http://x.test/f");
        assert_eq!(source.refresh_at(0.8), None);
        assert!(!source.is_due_for_refresh(0.8, Instant::now()));
    }
}
