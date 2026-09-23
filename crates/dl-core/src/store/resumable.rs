//! A download that can be interrupted and picked up again.
//!
//! Ties the preallocated `.part` file to its `.dlmeta` journal and publishes
//! the result atomically. Nothing occupies the destination path until the whole
//! file is present and verified.

use super::journal::{Journal, Opened, ResourceId};
use super::layout::ChunkLayout;
use super::rawfile::{RawFile, StdFile};
use super::staging::Staging;
use crate::error::{Error, Result};
use crate::model::ByteRange;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Default chunk size. Large enough that the per-chunk `fsync` pair is not the
/// bottleneck, small enough that a crash costs little re-download.
pub const DEFAULT_CHUNK_SIZE: u64 = 4 * 1024 * 1024;

pub struct ResumableFile {
    destination: PathBuf,
    part: PathBuf,
    meta: PathBuf,
    data: Arc<dyn RawFile>,
    journal: Journal,
}

impl ResumableFile {
    pub async fn open(
        destination: impl AsRef<Path>,
        resource: ResourceId,
        chunk_size: u64,
    ) -> Result<Self> {
        Self::open_with(destination, resource, chunk_size, super::Durability::default()).await
    }

    pub async fn open_with(
        destination: impl AsRef<Path>,
        resource: ResourceId,
        chunk_size: u64,
        durability: super::Durability,
    ) -> Result<Self> {
        Self::open_staged(destination, resource, chunk_size, durability, &Staging::default()).await
    }

    pub async fn open_staged(
        destination: impl AsRef<Path>,
        resource: ResourceId,
        chunk_size: u64,
        durability: super::Durability,
        staging: &Staging,
    ) -> Result<Self> {
        let destination = destination.as_ref().to_path_buf();
        staging.prepare()?;
        let (part, meta) = staging.paths_for(&destination);

        let data = Arc::new(StdFile::open(&part).await?);
        let meta_file: Arc<dyn RawFile> = Arc::new(StdFile::open(&meta).await?);

        // Reserved up front so the download fails immediately on a full disk
        // rather than at 99%, and so the file is not built up from fragments.
        // On Apple platforms a short reservation is reported as success, which
        // `preallocate` checks for.
        data.preallocate(resource.total_len).await?;

        let data_dyn: Arc<dyn RawFile> = data.clone();
        let journal =
            Journal::open_with(meta_file, Arc::clone(&data_dyn), resource, chunk_size, durability)
                .await?;

        Ok(Self { destination, part, meta, data: data_dyn, journal })
    }

    pub fn opened_as(&self) -> Opened {
        self.journal.opened_as()
    }

    /// Which chunks the journal claims, for the Inspector's grid.
    pub fn completed(&self) -> &super::layout::Completed {
        self.journal.completed()
    }

    pub fn layout(&self) -> &ChunkLayout {
        self.journal.layout()
    }

    pub fn bytes_done(&self) -> u64 {
        self.journal.bytes_done()
    }

    pub fn is_complete(&self) -> bool {
        self.journal.is_complete()
    }

    pub fn remaining(&self) -> Vec<u64> {
        self.journal.remaining()
    }

    pub fn range(&self, index: u64) -> Option<ByteRange> {
        self.journal.layout().range(index)
    }

    pub fn part_path(&self) -> &Path {
        &self.part
    }

    pub fn destination(&self) -> &Path {
        &self.destination
    }

    /// Write one chunk and record it as durable, with its hash.
    ///
    /// `data` must be exactly the chunk's length: a short write here would
    /// leave a hole that the journal then claims is filled.
    pub async fn write_chunk(&mut self, index: u64, data: bytes::Bytes) -> Result<()> {
        let range = self
            .journal
            .layout()
            .range(index)
            .ok_or_else(|| Error::Transport(format!("chunk {index} is out of range")))?;

        if data.len() as u64 != range.len() {
            return Err(Error::ShortBody { expected: range.len(), received: data.len() as u64 });
        }

        let hash = blake3::hash(&data);
        self.data.write_at(range.start, data).await?;
        self.journal.record_chunk(index, hash).await
    }

    /// Re-read every completed chunk and compare it with its recorded hash.
    ///
    /// Returns the indices that no longer match. Bit rot, bad RAM and a lying
    /// drive all produce bytes that were written correctly and are wrong now,
    /// which no amount of write-path care can catch.
    pub async fn verify_chunks(&self) -> Result<Vec<u64>> {
        let mut bad = Vec::new();
        for index in 0..self.journal.layout().chunk_count() {
            if !self.journal.completed().contains(index) {
                continue;
            }
            let Some(expected) = self.journal.chunk_hash(index) else {
                continue;
            };
            if blake3::hash(&self.read_chunk(index).await?) != expected {
                bad.push(index);
            }
        }
        Ok(bad)
    }

    /// Mark a chunk as missing so it is fetched again.
    pub async fn forget_chunk(&mut self, index: u64) -> Result<()> {
        self.journal.forget_chunk(index).await
    }

    pub fn chunk_hash(&self, index: u64) -> Option<blake3::Hash> {
        self.journal.chunk_hash(index)
    }

    /// Read a chunk back, for verification.
    pub async fn read_chunk(&self, index: u64) -> Result<Vec<u8>> {
        let range = self
            .journal
            .layout()
            .range(index)
            .ok_or_else(|| Error::Transport(format!("chunk {index} is out of range")))?;
        self.data.read_at(range.start, range.len() as usize).await
    }

    pub async fn checkpoint(&mut self) -> Result<()> {
        self.journal.checkpoint().await
    }

    /// Make every completed chunk durable now.
    ///
    /// Called whenever a transfer stops for any reason. Dropping the file
    /// cannot do this: `Drop` cannot await: so a stop that forgets to call
    /// it costs the work the current durability mode was holding back.
    pub async fn flush(&mut self) -> Result<()> {
        self.journal.flush().await
    }

    /// Chunks written but not yet claimed by the journal: what an abrupt stop
    /// would cost right now.
    pub fn unflushed_chunks(&self) -> usize {
        self.journal.pending_chunks()
    }

    /// Publish the finished file and remove the journal.
    pub async fn finalize(&mut self) -> Result<PathBuf> {
        if !self.journal.is_complete() {
            return Err(Error::ShortBody {
                expected: self.journal.layout().total(),
                received: self.journal.bytes_done(),
            });
        }
        // Whatever the mode, a finished file is fully durable before it takes
        // its real name: the loose modes trade crash cost during the transfer,
        // not the integrity of the result.
        self.journal.flush().await?;
        self.data.sync_barrier().await?;

        let part = self.part.clone();
        let meta = self.meta.clone();
        let destination = self.destination.clone();
        let result = destination.clone();

        tokio::task::spawn_blocking(move || {
            // A rename when the partial is beside its destination, a copy when
            // a configured incomplete folder puts it on another filesystem.
            super::staging::publish(&part, &destination)?;
            // Without this the move itself can be lost on power failure.
            sync_parent_dir(&destination)?;
            match std::fs::remove_file(&meta) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                other => other,
            }
        })
        .await
        .expect("finalize task panicked")
        .map_err(|e| Error::io(self.destination.display(), e))?;

        Ok(result)
    }

    /// Remove the partial file and its journal.
    pub async fn discard(&mut self) -> Result<()> {
        let part = self.part.clone();
        let meta = self.meta.clone();
        tokio::task::spawn_blocking(move || {
            for path in [part, meta] {
                match std::fs::remove_file(&path) {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e),
                    Ok(()) => {}
                }
            }
            Ok(())
        })
        .await
        .expect("discard task panicked")
        .map_err(|e| Error::io(self.part.display(), e))
    }
}

#[cfg(unix)]
fn sync_parent_dir(path: &Path) -> std::io::Result<()> {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    std::fs::File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_dir(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::Durability;
    use super::super::staging::suffixed;
    use super::*;

    fn resource(total: u64) -> ResourceId {
        ResourceId { total_len: total, etag: Some("\"v1\"".into()), last_modified: None }
    }

    #[tokio::test]
    async fn a_resumed_file_only_asks_for_the_missing_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out.bin");
        let total = 64 * 1024 * 5;

        let mut file =
            ResumableFile::open_with(&dest, resource(total), 64 * 1024, Durability::Safe)
                .await
                .unwrap();
        assert_eq!(file.opened_as(), Opened::Fresh);
        assert_eq!(file.remaining().len(), 5);

        for index in [0, 2] {
            let len = file.range(index).unwrap().len() as usize;
            file.write_chunk(index, bytes::Bytes::from(vec![b'x'; len])).await.unwrap();
        }
        drop(file);

        let reopened =
            ResumableFile::open_with(&dest, resource(total), 64 * 1024, Durability::Safe)
                .await
                .unwrap();
        assert_eq!(reopened.opened_as(), Opened::Resumed);
        assert_eq!(reopened.remaining(), vec![1, 3, 4]);
        assert_eq!(reopened.bytes_done(), 64 * 1024 * 2);
    }

    #[tokio::test]
    async fn finalizing_an_incomplete_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out.bin");
        let mut file =
            ResumableFile::open(&dest, resource(64 * 1024 * 3), 64 * 1024).await.unwrap();

        let len = file.range(0).unwrap().len() as usize;
        file.write_chunk(0, bytes::Bytes::from(vec![1u8; len])).await.unwrap();

        assert!(matches!(file.finalize().await, Err(Error::ShortBody { .. })));
        assert!(!dest.exists(), "an incomplete file must never reach the destination");
    }

    #[tokio::test]
    async fn a_wrong_length_chunk_is_rejected_before_it_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let mut file =
            ResumableFile::open(dir.path().join("out.bin"), resource(64 * 1024 * 2), 64 * 1024)
                .await
                .unwrap();

        let err = file.write_chunk(0, bytes::Bytes::from_static(b"too short")).await.unwrap_err();
        assert!(matches!(err, Error::ShortBody { .. }), "{err:?}");
        assert!(!file.remaining().is_empty());
        assert_eq!(file.bytes_done(), 0, "a rejected chunk must not be journalled");
    }

    #[tokio::test]
    async fn finalize_publishes_and_removes_the_journal() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out.bin");
        let total = 64 * 1024 * 2;
        let mut file = ResumableFile::open(&dest, resource(total), 64 * 1024).await.unwrap();

        for index in 0..2u64 {
            let len = file.range(index).unwrap().len() as usize;
            file.write_chunk(index, bytes::Bytes::from(vec![index as u8; len])).await.unwrap();
        }

        assert_eq!(file.finalize().await.unwrap(), dest);
        assert_eq!(std::fs::metadata(&dest).unwrap().len(), total);
        assert!(!suffixed(&dest, ".part").exists());
        assert!(!suffixed(&dest, ".dlmeta").exists());
    }

    /// The point of hashing every chunk: corruption identifies one chunk to
    /// re-fetch instead of invalidating the whole file.
    #[tokio::test]
    async fn verification_localizes_corruption_to_a_single_chunk() {
        use std::io::{Seek, SeekFrom, Write};

        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out.bin");
        let chunk = 64 * 1024;
        let total = chunk * 6;

        let mut file = ResumableFile::open_with(&dest, resource(total), chunk, Durability::Safe)
            .await
            .unwrap();
        for index in 0..6u64 {
            let len = file.range(index).unwrap().len() as usize;
            file.write_chunk(index, bytes::Bytes::from(vec![index as u8 + 1; len])).await.unwrap();
        }
        assert!(file.verify_chunks().await.unwrap().is_empty(), "a clean file must verify");
        drop(file);

        // Rot a single byte inside chunk 3, exactly as bit rot or a lying drive
        // would: the write path did everything right and the bytes are wrong.
        // Seek and write rather than a positional write: `write_at` is Unix
        // only, and its Windows counterpart moves the cursor, so the portable
        // pair is the one that says what it does on both.
        let mut handle =
            std::fs::OpenOptions::new().write(true).open(dir.path().join("out.bin.part")).unwrap();
        handle.seek(SeekFrom::Start(chunk * 3 + 17)).unwrap();
        handle.write_all(&[0x00]).unwrap();
        handle.sync_all().unwrap();
        drop(handle);

        let mut reopened =
            ResumableFile::open_with(&dest, resource(total), chunk, Durability::Safe)
                .await
                .unwrap();
        assert_eq!(reopened.opened_as(), Opened::Resumed);

        let bad = reopened.verify_chunks().await.unwrap();
        assert_eq!(bad, vec![3], "verification should name exactly the damaged chunk");

        reopened.forget_chunk(3).await.unwrap();
        assert_eq!(reopened.remaining(), vec![3], "only the damaged chunk should need re-fetching");
        assert_eq!(reopened.bytes_done(), chunk * 5);

        // And the repair survives a restart.
        drop(reopened);
        let again = ResumableFile::open_with(&dest, resource(total), chunk, Durability::Safe)
            .await
            .unwrap();
        assert_eq!(again.remaining(), vec![3]);
    }

    #[tokio::test]
    async fn a_recorded_hash_is_available_for_every_completed_chunk() {
        let dir = tempfile::tempdir().unwrap();
        let chunk = 64 * 1024;
        let mut file = ResumableFile::open(dir.path().join("out.bin"), resource(chunk * 2), chunk)
            .await
            .unwrap();

        let payload = vec![7u8; chunk as usize];
        file.write_chunk(0, bytes::Bytes::from(payload.clone())).await.unwrap();

        assert_eq!(file.chunk_hash(0), Some(blake3::hash(&payload)));
        assert_eq!(file.chunk_hash(1), None, "an unfetched chunk has no hash");
    }

    #[tokio::test]
    async fn preallocation_sizes_the_part_file_up_front() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out.bin");
        let total = 64 * 1024 * 7 + 13;
        let file = ResumableFile::open(&dest, resource(total), 64 * 1024).await.unwrap();

        assert_eq!(std::fs::metadata(file.part_path()).unwrap().len(), total);
    }

    /// The looser modes hold completions back, and an abrupt stop loses them.
    ///
    /// That is the trade, and it has to be visible: the reopened journal must
    /// under-claim rather than claim chunks it cannot prove. `unflushed_chunks`
    /// is how a caller sees what is at risk.
    #[tokio::test]
    async fn an_abrupt_stop_loses_unflushed_chunks_but_never_lies_about_them() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out.bin");
        let chunk = 64 * 1024;
        let total = chunk * 4;

        let mut file =
            ResumableFile::open_with(&dest, resource(total), chunk, Durability::Balanced)
                .await
                .unwrap();
        for index in 0..2u64 {
            let len = file.range(index).unwrap().len() as usize;
            file.write_chunk(index, bytes::Bytes::from(vec![b'x'; len])).await.unwrap();
        }
        assert_eq!(file.unflushed_chunks(), 2, "Balanced should be holding both back");
        // Dropped without a flush: a crash, as far as the journal is concerned.
        drop(file);

        let reopened =
            ResumableFile::open_with(&dest, resource(total), chunk, Durability::Balanced)
                .await
                .unwrap();
        assert_eq!(reopened.bytes_done(), 0, "nothing was claimed, so nothing is trusted");
        assert_eq!(reopened.remaining().len(), 4);
    }

    /// A flush before the stop is what turns that loss back into a resume.
    #[tokio::test]
    async fn flushing_before_a_stop_preserves_the_work() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out.bin");
        let chunk = 64 * 1024;
        let total = chunk * 4;

        let mut file =
            ResumableFile::open_with(&dest, resource(total), chunk, Durability::Balanced)
                .await
                .unwrap();
        for index in 0..2u64 {
            let len = file.range(index).unwrap().len() as usize;
            file.write_chunk(index, bytes::Bytes::from(vec![b'x'; len])).await.unwrap();
        }
        file.flush().await.unwrap();
        assert_eq!(file.unflushed_chunks(), 0);
        drop(file);

        let reopened =
            ResumableFile::open_with(&dest, resource(total), chunk, Durability::Balanced)
                .await
                .unwrap();
        assert_eq!(reopened.bytes_done(), chunk * 2);
        assert_eq!(reopened.remaining(), vec![2, 3]);
    }
}
