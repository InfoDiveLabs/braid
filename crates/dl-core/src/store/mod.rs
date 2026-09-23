//! Where bytes land.
//!
//! Behind a trait so phase 2 can inject a writer that drops `fsync`, reorders
//! writes, or fails at a chosen offset. Phase 1 writes to `<name>.part` and
//! renames on success; the journal and writer actor arrive in phase 2.

pub mod durability;
pub mod journal;
pub mod layout;
pub mod rawfile;
pub mod resumable;
pub mod staging;

pub use durability::Durability;
pub use journal::{Journal, Opened, ResourceId};
pub use layout::{ChunkLayout, Completed};
pub use rawfile::{RawFile, StdFile};
pub use resumable::{DEFAULT_CHUNK_SIZE, ResumableFile};
pub use staging::Staging;

use crate::error::{Error, Result};
use bytes::Bytes;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[async_trait::async_trait]
pub trait Storage: Send + Sync {
    /// Write `data` at `offset`. Offsets may arrive out of order.
    async fn write_at(&self, offset: u64, data: Bytes) -> Result<()>;

    /// Make everything written so far durable.
    async fn sync_data(&self) -> Result<()>;

    /// Atomically publish the finished file and return its path.
    async fn finalize(&self) -> Result<PathBuf>;

    /// Throw away partial state.
    async fn discard(&self) -> Result<()>;
}

/// Writes to a `.part` file next to the destination, renaming on success.
///
/// Nothing ever occupies the destination path until the download completes.
pub struct FileStorage {
    destination: PathBuf,
    part: PathBuf,
    file: Arc<std::fs::File>,
    /// Windows' `seek_write` moves the file cursor, unlike Unix `write_at`, so
    /// concurrent positional writes on one handle race. Phase 2 replaces this
    /// with a single writer task.
    write_lock: tokio::sync::Mutex<()>,
}

impl FileStorage {
    pub async fn create(destination: impl AsRef<Path>) -> Result<Self> {
        let destination = destination.as_ref().to_path_buf();
        let part = part_path(&destination);

        if let Some(parent) = destination.parent().filter(|p| !p.as_os_str().is_empty()) {
            tokio::fs::create_dir_all(parent).await.map_err(|e| Error::io(parent.display(), e))?;
        }

        let path = part.clone();
        let file = tokio::task::spawn_blocking(move || {
            std::fs::OpenOptions::new().create(true).truncate(true).write(true).open(&path)
        })
        .await
        .expect("file open task panicked")
        .map_err(|e| Error::io(part.display(), e))?;

        Ok(Self {
            destination,
            part,
            file: Arc::new(file),
            write_lock: tokio::sync::Mutex::new(()),
        })
    }

    pub fn part_path(&self) -> &Path {
        &self.part
    }

    pub fn destination(&self) -> &Path {
        &self.destination
    }
}

/// `foo.iso` -> `foo.iso.part`
fn part_path(destination: &Path) -> PathBuf {
    let mut name = destination.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    destination.with_file_name(name)
}

#[async_trait::async_trait]
impl Storage for FileStorage {
    async fn write_at(&self, offset: u64, data: Bytes) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        let _guard = self.write_lock.lock().await;
        let file = Arc::clone(&self.file);
        let path = self.part.clone();

        tokio::task::spawn_blocking(move || write_all_at(&file, offset, &data))
            .await
            .expect("write task panicked")
            .map_err(|e| Error::io(path.display(), e))
    }

    async fn sync_data(&self) -> Result<()> {
        let file = Arc::clone(&self.file);
        let path = self.part.clone();
        tokio::task::spawn_blocking(move || file.sync_data())
            .await
            .expect("sync task panicked")
            .map_err(|e| Error::io(path.display(), e))
    }

    async fn finalize(&self) -> Result<PathBuf> {
        self.sync_data().await?;

        let part = self.part.clone();
        let destination = self.destination.clone();
        let result = destination.clone();

        tokio::task::spawn_blocking(move || {
            std::fs::rename(&part, &destination)?;
            // The rename itself can be lost on power failure without this.
            sync_parent_dir(&destination)
        })
        .await
        .expect("finalize task panicked")
        .map_err(|e| Error::io(self.destination.display(), e))?;

        Ok(result)
    }

    async fn discard(&self) -> Result<()> {
        let part = self.part.clone();
        tokio::task::spawn_blocking(move || match std::fs::remove_file(&part) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            other => other,
        })
        .await
        .expect("discard task panicked")
        .map_err(|e| Error::io(self.part.display(), e))
    }
}

/// Loops until the whole buffer is written: a short write would leave a hole.
#[cfg(unix)]
fn write_all_at(file: &std::fs::File, mut offset: u64, mut buf: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    while !buf.is_empty() {
        match file.write_at(buf, offset) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "failed to write the whole buffer",
                ));
            }
            Ok(n) => {
                buf = &buf[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(windows)]
fn write_all_at(file: &std::fs::File, mut offset: u64, mut buf: &[u8]) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match file.seek_write(buf, offset) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "failed to write the whole buffer",
                ));
            }
            Ok(n) => {
                buf = &buf[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn sync_parent_dir(path: &Path) -> std::io::Result<()> {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    std::fs::File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_dir(_path: &Path) -> std::io::Result<()> {
    // NTFS metadata ordering covers this; there is no directory handle to sync.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn writes_land_at_their_offsets_regardless_of_order() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out.bin");
        let store = FileStorage::create(&dest).await.unwrap();

        // Deliberately out of order: chunked downloads complete in any order.
        store.write_at(5, Bytes::from_static(b"world")).await.unwrap();
        store.write_at(0, Bytes::from_static(b"hello")).await.unwrap();

        let path = store.finalize().await.unwrap();
        assert_eq!(path, dest);
        assert_eq!(std::fs::read(&dest).unwrap(), b"helloworld");
    }

    #[tokio::test]
    async fn partial_output_never_occupies_the_destination_path() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out.bin");
        let store = FileStorage::create(&dest).await.unwrap();
        store.write_at(0, Bytes::from_static(b"partial")).await.unwrap();

        // Until finalize, nothing may exist at the destination: another process
        // must never see an incomplete download as a finished file.
        assert!(!dest.exists());
        assert!(store.part_path().exists());

        store.discard().await.unwrap();
        assert!(!store.part_path().exists());
        assert!(!dest.exists());
    }

    #[tokio::test]
    async fn discard_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStorage::create(dir.path().join("out.bin")).await.unwrap();
        store.discard().await.unwrap();
        store.discard().await.unwrap();
    }

    #[tokio::test]
    async fn creates_missing_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("a/b/c/out.bin");
        let store = FileStorage::create(&dest).await.unwrap();
        store.write_at(0, Bytes::from_static(b"x")).await.unwrap();
        assert_eq!(store.finalize().await.unwrap(), dest);
    }

    #[test]
    fn part_path_appends_rather_than_replacing_the_extension() {
        // Replacing it would collide between `a.iso` and `a.zip`.
        assert_eq!(part_path(Path::new("/tmp/a.iso")), PathBuf::from("/tmp/a.iso.part"));
        assert_eq!(part_path(Path::new("/tmp/noext")), PathBuf::from("/tmp/noext.part"));
    }
}
