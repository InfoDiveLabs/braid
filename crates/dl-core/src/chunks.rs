//! Live per-chunk state, for the Inspector's piece grid.
//!
//! Deliberately **not** part of [`crate::engine::DownloadSnapshot`]. The
//! snapshot is built ten times a second for every transfer in the list, and a
//! bitmap of a few thousand chunks in each one is precisely the wakeup storm
//! the bridge was designed to avoid. This is pulled instead, by id, only for
//! the transfer whose grid is on screen.
//!
//! Shared out through [`crate::ResumeOptions::on_chunks_ready`] the moment a
//! transfer starts, the same way the lane selector is, because the alternative
//! is learning what the chunks did only once the transfer has finished.

use crate::store::layout::{ChunkLayout, Completed};
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

/// A point-in-time view of the chunk map. Plain data, cheap to send anywhere.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChunkReport {
    pub chunk_count: u64,
    pub chunk_size: u64,
    /// One bit per chunk, little-endian within each byte. A few hundred bytes
    /// for a large file, against tens of thousands of model rows if this were
    /// expanded before it crossed into the UI.
    pub complete: Vec<u8>,
    /// Chunks a connection is fetching right now.
    pub inflight: Vec<u64>,
}

impl ChunkReport {
    pub fn is_complete(&self, index: u64) -> bool {
        let (byte, bit) = ((index / 8) as usize, index % 8);
        self.complete.get(byte).is_some_and(|b| b & (1 << bit) != 0)
    }

    pub fn completed_count(&self) -> u64 {
        (0..self.chunk_count).filter(|i| self.is_complete(*i)).count() as u64
    }
}

#[derive(Default)]
struct State {
    complete: Option<Completed>,
    inflight: BTreeSet<u64>,
}

/// The live chunk map of one transfer.
pub struct ChunkProgress {
    layout: ChunkLayout,
    state: Mutex<State>,
}

impl ChunkProgress {
    pub fn new(layout: ChunkLayout) -> Arc<Self> {
        Arc::new(Self { layout, state: Mutex::new(State::default()) })
    }

    /// Adopt what the journal already had, so a resumed transfer shows its
    /// existing chunks rather than filling in from empty.
    pub fn seed(&self, complete: Completed) {
        let mut state = self.state.lock().unwrap();
        state.complete = Some(complete);
    }

    pub fn started(&self, index: u64) {
        self.state.lock().unwrap().inflight.insert(index);
    }

    /// A chunk that was handed back without completing: a rate limit, a dead
    /// lane, a requeue. It stops being in flight and does not become complete.
    pub fn released(&self, index: u64) {
        self.state.lock().unwrap().inflight.remove(&index);
    }

    pub fn completed(&self, index: u64) {
        let mut state = self.state.lock().unwrap();
        state.inflight.remove(&index);
        if let Some(complete) = state.complete.as_mut() {
            complete.insert(index);
        }
    }

    /// A chunk that failed verification and has to be fetched again.
    pub fn forgotten(&self, index: u64) {
        let mut state = self.state.lock().unwrap();
        if let Some(complete) = state.complete.as_mut() {
            complete.remove(index);
        }
    }

    pub fn report(&self) -> ChunkReport {
        let state = self.state.lock().unwrap();
        ChunkReport {
            chunk_count: self.layout.chunk_count(),
            chunk_size: self.layout.chunk_size(),
            complete: state.complete.as_ref().map(|c| c.as_bits().to_vec()).unwrap_or_default(),
            inflight: state.inflight.iter().copied().collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ChunkLayout` enforces a floor on chunk size, so the tests use one
    /// rather than a convenient small number that would be silently raised
    /// and leave the chunk count nothing like what was asked for.
    const CHUNK: u64 = crate::store::layout::MIN_CHUNK_SIZE;

    fn progress(chunks: u64) -> Arc<ChunkProgress> {
        let layout = ChunkLayout::new(chunks * CHUNK, CHUNK);
        assert_eq!(layout.chunk_count(), chunks, "the fixture must lay out what it asked for");
        let p = ChunkProgress::new(layout);
        p.seed(Completed::new(chunks));
        p
    }

    #[test]
    fn a_fresh_transfer_reports_every_chunk_missing() {
        let report = progress(16).report();
        assert_eq!(report.chunk_count, 16);
        assert_eq!(report.completed_count(), 0);
        assert!(report.inflight.is_empty());
    }

    #[test]
    fn a_chunk_in_flight_is_not_reported_complete() {
        // The grid's whole point is that these are different states. Showing
        // an in-flight chunk as Have would claim bytes that are not there.
        let p = progress(8);
        p.started(3);
        let report = p.report();
        assert_eq!(report.inflight, vec![3]);
        assert!(!report.is_complete(3));
    }

    #[test]
    fn completing_a_chunk_takes_it_out_of_flight() {
        let p = progress(8);
        p.started(3);
        p.completed(3);
        let report = p.report();
        assert!(report.inflight.is_empty(), "still shown as downloading after it landed");
        assert!(report.is_complete(3));
        assert_eq!(report.completed_count(), 1);
    }

    #[test]
    fn a_released_chunk_goes_back_to_missing_rather_than_complete() {
        // A rate limit or a dead lane hands the chunk back. Treating that as
        // completion is how a grid comes to claim a file is whole when it is
        // not.
        let p = progress(8);
        p.started(5);
        p.released(5);
        let report = p.report();
        assert!(report.inflight.is_empty());
        assert!(!report.is_complete(5));
    }

    #[test]
    fn a_chunk_that_failed_verification_returns_to_missing() {
        // This is the one moment the grid earns its place: a cell going from
        // Have back to Missing is per-chunk hashing catching corruption.
        let p = progress(8);
        p.completed(2);
        assert!(p.report().is_complete(2));
        p.forgotten(2);
        assert!(!p.report().is_complete(2));
    }

    #[test]
    fn a_resumed_transfer_adopts_what_the_journal_already_had() {
        let mut existing = Completed::new(8);
        existing.insert(0);
        existing.insert(1);
        let p = ChunkProgress::new(ChunkLayout::new(8 * CHUNK, CHUNK));
        p.seed(existing);
        assert_eq!(p.report().completed_count(), 2);
    }

    #[test]
    fn the_bitmap_reads_back_the_chunks_that_were_set() {
        // The bits cross a process boundary as bytes and are expanded on the
        // other side, so the indexing has to agree at both ends.
        let p = progress(20);
        for index in [0, 7, 8, 19] {
            p.completed(index);
        }
        let report = p.report();
        for index in 0..20 {
            let want = [0, 7, 8, 19].contains(&index);
            assert_eq!(report.is_complete(index), want, "chunk {index}");
        }
        assert_eq!(report.completed_count(), 4);
    }
}
