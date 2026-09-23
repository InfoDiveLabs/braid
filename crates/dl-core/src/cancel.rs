//! Cooperative cancellation.
//!
//! Pausing a download cancels its task rather than suspending it. The journal
//! already makes an interrupted transfer resumable, so a pause and a crash
//! recover through exactly the same path: there is no separate "paused" state
//! on disk that could disagree with what was actually written.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
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
    fn cancellation_is_not_retryable() {
        // A pause is a decision, not a fault; retrying it would defeat it.
        assert!(!crate::error::Error::Cancelled.is_retryable());
    }
}
