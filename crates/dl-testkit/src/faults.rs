//! A [`RawFile`] that lies.
//!
//! The crash-safety design rests on ordering between the data file and the
//! journal, and every way it can break is silent. A disk that always works
//! cannot test it, so this wrapper reproduces the failures that matter:
//! `DropSync` models a drive whose write cache is volatile despite reporting
//! success, and `ReorderWrites` models one that does not preserve order.
//! Together they are what actually prove the ordering invariant.

use dl_core::error::{Error, Result};
use dl_core::store::RawFile;
use std::path::Path;
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fault {
    /// Fail any write touching `offset`, once.
    FailWriteAt { offset: u64, kind: std::io::ErrorKind },
    /// Write only the first `keep` bytes of the write covering `offset`: a torn
    /// write, which is what a power cut during a write actually produces.
    TornWriteAt { offset: u64, keep: usize },
    /// `sync_data` and `sync_barrier` silently do nothing. Models a drive that
    /// acknowledges a flush it has not performed.
    DropSync,
    /// Hold writes back and replay them out of order on the next sync.
    ReorderWrites,
    /// Fail every write past a cumulative byte budget.
    DiskFull { after_bytes: u64 },
}

#[derive(Default)]
struct State {
    faults: Vec<Fault>,
    written: u64,
    pending: Vec<(u64, bytes::Bytes)>,
    writes: u64,
    syncs: u64,
    dropped_syncs: u64,
}

/// Wraps a real [`RawFile`], applying a fault script.
pub struct FaultyFile {
    inner: Arc<dyn RawFile>,
    state: Arc<Mutex<State>>,
}

impl FaultyFile {
    pub fn new(inner: Arc<dyn RawFile>) -> Self {
        Self { inner, state: Arc::new(Mutex::new(State::default())) }
    }

    pub fn with_faults(inner: Arc<dyn RawFile>, faults: Vec<Fault>) -> Self {
        let this = Self::new(inner);
        this.state.lock().unwrap().faults = faults;
        this
    }

    pub fn set_faults(&self, faults: Vec<Fault>) {
        self.state.lock().unwrap().faults = faults;
    }

    pub fn clear_faults(&self) {
        let mut state = self.state.lock().unwrap();
        state.faults.clear();
    }

    pub fn write_count(&self) -> u64 {
        self.state.lock().unwrap().writes
    }

    pub fn sync_count(&self) -> u64 {
        self.state.lock().unwrap().syncs
    }

    /// How many syncs were swallowed by `DropSync`.
    pub fn dropped_syncs(&self) -> u64 {
        self.state.lock().unwrap().dropped_syncs
    }

    /// Discard writes still held by `ReorderWrites`, as a power cut would.
    pub fn lose_pending_writes(&self) -> usize {
        let mut state = self.state.lock().unwrap();
        std::mem::take(&mut state.pending).len()
    }

    fn take_pending(&self) -> Vec<(u64, bytes::Bytes)> {
        let mut state = self.state.lock().unwrap();
        let mut pending = std::mem::take(&mut state.pending);
        // Reverse rather than shuffle: deterministic, and still the worst case
        // for any code that assumes issue order is durability order.
        pending.reverse();
        pending
    }
}

enum Action {
    Pass,
    Fail(std::io::ErrorKind),
    Torn(usize),
    Defer,
}

#[async_trait::async_trait]
impl RawFile for FaultyFile {
    async fn write_at(&self, offset: u64, data: bytes::Bytes) -> Result<()> {
        let action = {
            let mut state = self.state.lock().unwrap();
            state.writes += 1;
            let end = offset + data.len() as u64;

            let mut action = Action::Pass;
            let mut consumed = None;
            for (i, fault) in state.faults.iter().enumerate() {
                match fault {
                    Fault::FailWriteAt { offset: at, kind } if (offset..end).contains(at) => {
                        action = Action::Fail(*kind);
                        consumed = Some(i);
                        break;
                    }
                    Fault::TornWriteAt { offset: at, keep } if (offset..end).contains(at) => {
                        action = Action::Torn(*keep);
                        consumed = Some(i);
                        break;
                    }
                    Fault::DiskFull { after_bytes } if state.written >= *after_bytes => {
                        action = Action::Fail(std::io::ErrorKind::StorageFull);
                        break;
                    }
                    Fault::ReorderWrites => {
                        action = Action::Defer;
                        break;
                    }
                    _ => {}
                }
            }
            if let Some(i) = consumed {
                state.faults.remove(i);
            }
            if matches!(action, Action::Pass | Action::Torn(_)) {
                state.written += data.len() as u64;
            }
            if let Action::Defer = action {
                state.pending.push((offset, data.clone()));
            }
            action
        };

        match action {
            Action::Pass => self.inner.write_at(offset, data).await,
            Action::Defer => Ok(()),
            Action::Fail(kind) => Err(Error::io(
                self.inner.path().display(),
                std::io::Error::new(kind, "injected fault"),
            )),
            Action::Torn(keep) => {
                let keep = keep.min(data.len());
                self.inner.write_at(offset, data.slice(..keep)).await
            }
        }
    }

    async fn read_at(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        self.inner.read_at(offset, len).await
    }

    async fn set_len(&self, len: u64) -> Result<()> {
        self.inner.set_len(len).await
    }

    async fn size(&self) -> Result<u64> {
        self.inner.size().await
    }

    async fn sync_data(&self) -> Result<()> {
        self.sync_barrier().await
    }

    async fn sync_barrier(&self) -> Result<()> {
        let dropped = {
            let mut state = self.state.lock().unwrap();
            state.syncs += 1;
            let dropped = state.faults.contains(&Fault::DropSync);
            if dropped {
                state.dropped_syncs += 1;
            }
            dropped
        };

        for (offset, data) in self.take_pending() {
            self.inner.write_at(offset, data).await?;
        }

        if dropped {
            // The caller is told the data is durable when it is not.
            return Ok(());
        }
        self.inner.sync_barrier().await
    }

    fn path(&self) -> &Path {
        self.inner.path()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dl_core::store::StdFile;

    async fn temp() -> (tempfile::TempDir, Arc<dyn RawFile>) {
        let dir = tempfile::tempdir().unwrap();
        let file = StdFile::open(dir.path().join("f.bin")).await.unwrap();
        (dir, Arc::new(file))
    }

    #[tokio::test]
    async fn a_torn_write_lands_partially() {
        let (_dir, inner) = temp().await;
        let faulty =
            FaultyFile::with_faults(inner.clone(), vec![Fault::TornWriteAt { offset: 0, keep: 3 }]);

        faulty.write_at(0, bytes::Bytes::from_static(b"abcdefgh")).await.unwrap();
        assert_eq!(inner.read_at(0, 8).await.unwrap(), b"abc");

        // The fault fires once, so the retry succeeds in full.
        faulty.write_at(0, bytes::Bytes::from_static(b"abcdefgh")).await.unwrap();
        assert_eq!(inner.read_at(0, 8).await.unwrap(), b"abcdefgh");
    }

    #[tokio::test]
    async fn dropped_syncs_are_reported_but_not_performed() {
        let (_dir, inner) = temp().await;
        let faulty = FaultyFile::with_faults(inner, vec![Fault::DropSync]);
        faulty.sync_barrier().await.unwrap();
        assert_eq!(faulty.dropped_syncs(), 1);
    }

    #[tokio::test]
    async fn reordered_writes_are_withheld_until_sync_and_can_be_lost() {
        let (_dir, inner) = temp().await;
        let faulty = FaultyFile::with_faults(inner.clone(), vec![Fault::ReorderWrites]);

        faulty.write_at(0, bytes::Bytes::from_static(b"aaaa")).await.unwrap();
        faulty.write_at(4, bytes::Bytes::from_static(b"bbbb")).await.unwrap();
        // Nothing is durable before the sync.
        assert!(inner.read_at(0, 8).await.unwrap().iter().all(|b| *b == 0 || *b == b'\0'));

        assert_eq!(faulty.lose_pending_writes(), 2);
        faulty.sync_barrier().await.unwrap();
        assert!(inner.read_at(0, 8).await.unwrap().iter().all(|b| *b != b'a'));
    }

    #[tokio::test]
    async fn disk_full_fails_writes_past_the_budget() {
        let (_dir, inner) = temp().await;
        let faulty = FaultyFile::with_faults(inner, vec![Fault::DiskFull { after_bytes: 4 }]);

        faulty.write_at(0, bytes::Bytes::from_static(b"aaaa")).await.unwrap();
        let err = faulty.write_at(4, bytes::Bytes::from_static(b"bbbb")).await.unwrap_err();
        assert!(matches!(err, Error::Io { .. }), "{err:?}");
    }
}
