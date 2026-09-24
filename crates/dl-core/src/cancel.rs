//! Cooperative cancellation.
//!
//! Pausing a download cancels its task rather than suspending it. The journal
//! already makes an interrupted transfer resumable, so a pause and a crash
//! recover through exactly the same path: there is no separate "paused" state
//! on disk that could disagree with what was actually written.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Default)]
pub struct Cancel(Arc<Reason>);

#[derive(Default)]
struct Reason {
    stopped: AtomicBool,
    /// Tokens whose cancellation also cancels this one.
    ///
    /// One chunk can be called off for two unrelated reasons: the transfer was
    /// paused, or another lane fetched it first. Both have to stop the same
    /// stream, and a fetch carries one token.
    with: Vec<Cancel>,
}

impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }

    /// A token cancelled when either this or `other` is.
    ///
    /// Cancelling the result leaves both of them alone, which is the point: a
    /// chunk abandoned because somebody else finished it must not look like a
    /// paused transfer.
    pub fn or(&self, other: &Cancel) -> Cancel {
        Cancel(Arc::new(Reason {
            stopped: AtomicBool::new(false),
            with: vec![self.clone(), other.clone()],
        }))
    }

    pub fn cancel(&self) {
        self.0.stopped.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.stopped.load(Ordering::SeqCst) || self.0.with.iter().any(Cancel::is_cancelled)
    }

    /// `Err(Cancelled)` once cancelled, for use with `?` in a transfer loop.
    pub fn check(&self) -> crate::error::Result<()> {
        if self.is_cancelled() {
            return Err(crate::error::Error::Cancelled);
        }
        Ok(())
    }
}

impl std::fmt::Debug for Cancel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Cancel").field(&self.is_cancelled()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelling_is_visible_to_every_clone() {
        let a = Cancel::new();
        let b = a.clone();
        assert!(a.check().is_ok());

        b.cancel();
        assert!(a.is_cancelled());
        assert!(matches!(a.check(), Err(crate::error::Error::Cancelled)));
    }

    #[test]
    fn either_reason_stops_a_combined_token() {
        let paused = Cancel::new();
        let overtaken = Cancel::new();
        let chunk = paused.or(&overtaken);
        assert!(chunk.check().is_ok());

        overtaken.cancel();
        assert!(chunk.is_cancelled());
        assert!(paused.check().is_ok(), "one reason leaked into the other");
    }

    #[test]
    fn a_combined_token_can_be_stopped_without_stopping_its_reasons() {
        // A chunk abandoned because another lane finished it must not read as
        // a paused transfer, which is not retryable and would end the download.
        let paused = Cancel::new();
        let overtaken = Cancel::new();
        let chunk = paused.or(&overtaken);
        chunk.cancel();
        assert!(chunk.is_cancelled());
        assert!(paused.check().is_ok());
        assert!(overtaken.check().is_ok());
    }

    #[test]
    fn cancellation_is_not_retryable() {
        // A pause is a decision, not a fault; retrying it would defeat it.
        assert!(!crate::error::Error::Cancelled.is_retryable());
    }
}
