//! Surviving links that expire.
//!
//! A pre-signed URL is a credential with a deadline, not an address. The
//! engine treats it as one: it is re-resolved when it dies, replaced before it
//! dies, and resolved once per `(source, lane)` because a signature can be
//! bound to the address that asked for it.

pub mod expiry;
pub mod handle;
pub mod json;
pub mod model;
pub mod refreshers;
pub mod source;
pub mod staleness;

use crate::error::Result;

pub use handle::{RefreshBudget, RefreshCoordinator, SourceHandle};
pub use model::{
    RefreshCtx, RefreshPolicy, RefreshReason, ResolvedSource, ResponseSummary, SourceKey, Staleness,
};
pub use refreshers::{ApiRefresher, CommandRefresher, HttpJson, StaticRefresher};
pub use source::{Attempt, RefreshingSource, ResolvedFetcher};

/// Turns whatever the user started with into a source that works right now.
#[async_trait::async_trait]
pub trait LinkRefresher: Send + Sync {
    /// Stable name, for logs and for reporting which hook is in use.
    fn id(&self) -> &str;

    async fn resolve(&self, ctx: &RefreshCtx) -> Result<ResolvedSource>;

    /// Whether a response means the link has stopped working.
    ///
    /// The default covers every origin seen so far. Override it only for a
    /// service whose expiry is invisible to it: one that answers 200 with a
    /// JSON error document, say.
    fn staleness(&self, response: &ResponseSummary) -> Staleness {
        staleness::classify(response)
    }
}
