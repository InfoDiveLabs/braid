//! Splitting a resource into chunks, and tracking which are done.
//!
//! Pure logic with no I/O, so the invariants: chunks tile the file exactly,
//! never overlap, and a completion set always describes a real byte count: are
//! property-testable directly.

use crate::model::ByteRange;

/// Above this, the completion bitmap no longer fits in a journal header.
pub const MAX_CHUNKS: u64 = 29_000;

/// The smallest chunk worth issuing a separate request for.
pub const MIN_CHUNK_SIZE: u64 = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChunkLayout {
    total: u64,
    chunk_size: u64,
}

impl ChunkLayout {
    /// Split `total` bytes into chunks of at most `chunk_size`.
    ///
    /// `chunk_size` is raised if it would produce more chunks than the header
    /// bitmap can hold, so a very large file cannot silently overflow it.
    pub fn new(total: u64, chunk_size: u64) -> Self {
        let chunk_size = chunk_size.max(MIN_CHUNK_SIZE);
        let chunk_size = if total == 0 {
            chunk_size
        } else {
            let needed = total.div_ceil(MAX_CHUNKS);
            chunk_size.max(needed)
        };
        Self { total, chunk_size }
    }

    pub fn total(&self) -> u64 {
        self.total
    }

    pub fn chunk_size(&self) -> u64 {
        self.chunk_size
    }

    pub fn chunk_count(&self) -> u64 {
        self.total.div_ceil(self.chunk_size)
    }

    /// The byte range of chunk `index`. The last chunk is short unless the
    /// total divides evenly.
    pub fn range(&self, index: u64) -> Option<ByteRange> {
        if index >= self.chunk_count() {
            return None;
        }
        let start = index * self.chunk_size;
        Some(ByteRange::new(start, (start + self.chunk_size).min(self.total)))
    }
}

/// Which chunks are durably written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Completed {
    bits: Vec<u8>,
    count: u64,
}

impl Completed {
    pub fn new(chunk_count: u64) -> Self {
        Self { bits: vec![0; chunk_count.div_ceil(8) as usize], count: chunk_count }
    }

    pub fn from_bits(bits: &[u8], chunk_count: u64) -> Self {
        let mut set = Self::new(chunk_count);
        let n = set.bits.len().min(bits.len());
        set.bits[..n].copy_from_slice(&bits[..n]);
        // Bits past the chunk count would otherwise make `all_complete` lie.
        set.clear_trailing();
        set
    }

    pub fn as_bits(&self) -> &[u8] {
        &self.bits
    }

    pub fn chunk_count(&self) -> u64 {
        self.count
    }

    pub fn contains(&self, index: u64) -> bool {
        if index >= self.count {
            return false;
        }
        self.bits[(index / 8) as usize] & (1 << (index % 8)) != 0
    }

    pub fn insert(&mut self, index: u64) {
        if index < self.count {
            self.bits[(index / 8) as usize] |= 1 << (index % 8);
        }
    }

    pub fn remove(&mut self, index: u64) {
        if index < self.count {
            self.bits[(index / 8) as usize] &= !(1 << (index % 8));
        }
    }

    pub fn clear(&mut self) {
        self.bits.iter_mut().for_each(|b| *b = 0);
    }

    pub fn len(&self) -> u64 {
        self.bits.iter().map(|b| b.count_ones() as u64).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn all_complete(&self) -> bool {
        self.len() == self.count
    }

    pub fn missing(&self) -> impl Iterator<Item = u64> + '_ {
        (0..self.count).filter(|i| !self.contains(*i))
    }

    /// Bytes accounted for by completed chunks.
    pub fn bytes_done(&self, layout: &ChunkLayout) -> u64 {
        (0..self.count)
            .filter(|i| self.contains(*i))
            .filter_map(|i| layout.range(i))
            .map(|r| r.len())
            .sum()
    }

    fn clear_trailing(&mut self) {
        let extra = self.bits.len() as u64 * 8 - self.count;
        if extra > 0
            && let Some(last) = self.bits.last_mut()
        {
            *last &= 0xFFu8 >> extra;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn chunk_size_is_raised_so_the_bitmap_always_fits() {
        // 10 TB at 64 KB chunks would be 160 million chunks.
        let layout = ChunkLayout::new(10 * (1 << 40), MIN_CHUNK_SIZE);
        assert!(layout.chunk_count() <= MAX_CHUNKS, "got {}", layout.chunk_count());
    }

    #[test]
    fn a_short_final_chunk_is_not_padded() {
        let layout = ChunkLayout::new(MIN_CHUNK_SIZE * 2 + 5, MIN_CHUNK_SIZE);
        assert_eq!(layout.chunk_count(), 3);
        assert_eq!(layout.range(2).unwrap().len(), 5);
        assert_eq!(layout.range(3), None);
    }

    #[test]
    fn an_empty_resource_has_no_chunks() {
        let layout = ChunkLayout::new(0, MIN_CHUNK_SIZE);
        assert_eq!(layout.chunk_count(), 0);
        assert_eq!(layout.range(0), None);
    }

    #[test]
    fn bits_beyond_the_chunk_count_are_discarded() {
        // A journal written for a longer file must not make a shorter one look
        // finished.
        let set = Completed::from_bits(&[0xFF], 3);
        assert_eq!(set.len(), 3);
        assert!(set.all_complete());
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2000))]

        #[test]
        fn chunks_tile_the_file_exactly(total in 0u64..1_000_000_000, size in 1u64..8_000_000) {
            let layout = ChunkLayout::new(total, size);
            let mut cursor = 0u64;
            for i in 0..layout.chunk_count() {
                let range = layout.range(i).expect("index below chunk_count must exist");
                prop_assert_eq!(range.start, cursor, "chunk {} does not abut the previous", i);
                prop_assert!(!range.is_empty(), "chunk {} is empty", i);
                cursor = range.end;
            }
            prop_assert_eq!(cursor, total, "chunks do not cover the whole file");
        }

        #[test]
        fn completion_tracks_membership_and_byte_count(
            total in 1u64..500_000_000,
            size in 1u64..4_000_000,
            picks in prop::collection::vec(0u64..200, 0..80),
        ) {
            let layout = ChunkLayout::new(total, size);
            let count = layout.chunk_count();
            let mut set = Completed::new(count);

            let mut expected: std::collections::BTreeSet<u64> = Default::default();
            for pick in picks {
                let index = pick % count.max(1);
                if index < count {
                    set.insert(index);
                    expected.insert(index);
                }
            }

            prop_assert_eq!(set.len(), expected.len() as u64);
            for index in 0..count {
                prop_assert_eq!(set.contains(index), expected.contains(&index));
            }
            let expected_bytes: u64 =
                expected.iter().filter_map(|i| layout.range(*i)).map(|r| r.len()).sum();
            prop_assert_eq!(set.bytes_done(&layout), expected_bytes);
            prop_assert_eq!(set.missing().count() as u64, count - expected.len() as u64);
        }

        #[test]
        fn a_round_trip_through_bits_preserves_the_set(
            count in 1u64..2000,
            picks in prop::collection::vec(0u64..2000, 0..200),
        ) {
            let mut set = Completed::new(count);
            for pick in picks {
                set.insert(pick % count);
            }
            let restored = Completed::from_bits(set.as_bits(), count);
            prop_assert_eq!(set, restored);
        }
    }
}
