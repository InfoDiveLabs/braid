//! How hard the store works to survive a power cut.
//!
//! The ordering rule never changes with the mode: chunk data is made durable
//! before the journal claims the chunk, in every mode. What the mode changes is
//! how *often* that pair happens and whether the journal flush waits for the
//! drive's own cache.
//!
//! So the cost of a crash is always "some completed chunks have to be fetched
//! again", never "the file is wrong". A looser mode buys speed by widening that
//! window, not by weakening the invariant.

use std::time::Duration;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Durability {
    /// Flush on every completed chunk. A crash costs at most the chunk in
    /// flight, and every completion pays for a media barrier.
    Safe,
    /// Flush on a time and size floor.
    #[default]
    Balanced,
    /// Flush rarely, and let the operating system decide when the journal
    /// reaches the platter.
    Fast,
}

impl Durability {
    /// Bytes that may complete before a flush is forced.
    pub fn byte_floor(self) -> u64 {
        match self {
            Self::Safe => 0,
            Self::Balanced => 64 << 20,
            Self::Fast => 512 << 20,
        }
    }

    /// Time that may pass before a flush is forced.
    pub fn time_floor(self) -> Duration {
        match self {
            Self::Safe => Duration::ZERO,
            Self::Balanced => Duration::from_secs(5),
            Self::Fast => Duration::from_secs(30),
        }
    }

    /// Whether the journal flush waits for the drive to empty its own volatile
    /// cache. On macOS this is the difference between `fsync` and
    /// `F_FULLFSYNC`, and `fsync` alone does not survive a power cut.
    pub fn media_barrier(self) -> bool {
        match self {
            Self::Safe | Self::Balanced => true,
            Self::Fast => false,
        }
    }

    /// Whether every completion flushes, which is what makes Safe safe.
    pub fn flushes_every_chunk(self) -> bool {
        self.byte_floor() == 0 && self.time_floor().is_zero()
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Safe => "safe",
            Self::Balanced => "balanced",
            Self::Fast => "fast",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "safe" => Some(Self::Safe),
            "balanced" => Some(Self::Balanced),
            "fast" => Some(Self::Fast),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_safe_flushes_on_every_chunk() {
        assert!(Durability::Safe.flushes_every_chunk());
        assert!(!Durability::Balanced.flushes_every_chunk());
        assert!(!Durability::Fast.flushes_every_chunk());
    }

    #[test]
    fn the_modes_are_ordered_from_safest_to_loosest() {
        // A mode that flushed less often than a stricter one would make the
        // labels meaningless.
        assert!(Durability::Safe.byte_floor() < Durability::Balanced.byte_floor());
        assert!(Durability::Balanced.byte_floor() < Durability::Fast.byte_floor());
        assert!(Durability::Safe.time_floor() < Durability::Balanced.time_floor());
        assert!(Durability::Balanced.time_floor() < Durability::Fast.time_floor());
    }

    #[test]
    fn only_fast_gives_up_the_media_barrier() {
        assert!(Durability::Safe.media_barrier());
        assert!(Durability::Balanced.media_barrier());
        assert!(!Durability::Fast.media_barrier());
    }

    #[test]
    fn modes_round_trip_through_their_names() {
        for mode in [Durability::Safe, Durability::Balanced, Durability::Fast] {
            assert_eq!(Durability::parse(mode.as_str()), Some(mode));
        }
        assert_eq!(Durability::parse("SAFE"), Some(Durability::Safe));
        assert_eq!(Durability::parse("whatever"), None);
    }
}
