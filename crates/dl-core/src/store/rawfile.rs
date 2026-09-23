//! The lowest I/O layer, behind a trait so faults can be injected beneath both
//! the data file and the journal.
//!
//! Crash safety depends on ordering between two files, so a fault harness that
//! could only reach one of them would not exercise the property that matters.

use crate::error::{Error, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[async_trait::async_trait]
pub trait RawFile: Send + Sync {
    async fn write_at(&self, offset: u64, data: bytes::Bytes) -> Result<()>;
    async fn read_at(&self, offset: u64, len: usize) -> Result<Vec<u8>>;
    async fn set_len(&self, len: u64) -> Result<()>;

    /// Current file length on disk. Named `size` rather than `len` because a
    /// preallocated file is full-length while holding no content, so an
    /// `is_empty` counterpart would be meaningless.
    async fn size(&self) -> Result<u64>;

    /// Flush file data. On macOS this is `fsync`, which does **not** flush the
    /// drive's write cache; use [`RawFile::sync_barrier`] where ordering must
    /// survive power loss.
    async fn sync_data(&self) -> Result<()>;

    /// The strongest durability the platform offers: `F_FULLFSYNC` on macOS,
    /// `fsync` elsewhere. Much slower, so reserved for the journal.
    async fn sync_barrier(&self) -> Result<()>;

    fn path(&self) -> &Path;
}

pub struct StdFile {
    path: PathBuf,
    file: Arc<std::fs::File>,
    /// Windows' `seek_write` moves the file cursor, unlike Unix `write_at`, so
    /// concurrent positional writes on one handle race.
    lock: tokio::sync::Mutex<()>,
}

impl StdFile {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            tokio::fs::create_dir_all(parent).await.map_err(|e| Error::io(parent.display(), e))?;
        }
        let open_path = path.clone();
        let file = tokio::task::spawn_blocking(move || {
            std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&open_path)
        })
        .await
        .expect("open task panicked")
        .map_err(|e| Error::io(path.display(), e))?;

        Ok(Self { path, file: Arc::new(file), lock: tokio::sync::Mutex::new(()) })
    }

    /// Reserve `len` bytes up front.
    ///
    /// On Apple platforms `F_PREALLOCATE` can report success after allocating
    /// only part of the request, so the result is verified rather than trusted
    /// (rustix #1682). A short allocation is treated as failure.
    pub async fn preallocate(&self, len: u64) -> Result<()> {
        use fs4::FileExt;
        let file = Arc::clone(&self.file);
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            file.allocate(len)?;
            let allocated = file.allocated_size()?;
            if allocated < len {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::StorageFull,
                    format!("preallocation was short: asked {len}, reserved {allocated}"),
                ));
            }
            file.set_len(len)
        })
        .await
        .expect("preallocate task panicked")
        .map_err(|e| Error::io(path.display(), e))
    }
}

#[async_trait::async_trait]
impl RawFile for StdFile {
    async fn write_at(&self, offset: u64, data: bytes::Bytes) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        let _guard = self.lock.lock().await;
        let file = Arc::clone(&self.file);
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || write_all_at(&file, offset, &data))
            .await
            .expect("write task panicked")
            .map_err(|e| Error::io(path.display(), e))
    }

    async fn read_at(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        let _guard = self.lock.lock().await;
        let file = Arc::clone(&self.file);
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || read_at_exact(&file, offset, len))
            .await
            .expect("read task panicked")
            .map_err(|e| Error::io(path.display(), e))
    }

    async fn set_len(&self, len: u64) -> Result<()> {
        let file = Arc::clone(&self.file);
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || file.set_len(len))
            .await
            .expect("set_len task panicked")
            .map_err(|e| Error::io(path.display(), e))
    }

    async fn size(&self) -> Result<u64> {
        let file = Arc::clone(&self.file);
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || file.metadata().map(|m| m.len()))
            .await
            .expect("len task panicked")
            .map_err(|e| Error::io(path.display(), e))
    }

    async fn sync_data(&self) -> Result<()> {
        let file = Arc::clone(&self.file);
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || file.sync_data())
            .await
            .expect("sync task panicked")
            .map_err(|e| Error::io(path.display(), e))
    }

    async fn sync_barrier(&self) -> Result<()> {
        let file = Arc::clone(&self.file);
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || full_barrier(&file))
            .await
            .expect("barrier task panicked")
            .map_err(|e| Error::io(path.display(), e))
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

/// macOS `fsync` returns before the drive's volatile cache is flushed;
/// `F_FULLFSYNC` is the only call that waits for the media.
#[cfg(target_os = "macos")]
fn full_barrier(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    // SAFETY: a live fd from an owned File, and F_FULLFSYNC takes no argument.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) } == -1 {
        // Not supported on every filesystem; fsync is the honest fallback.
        return file.sync_data();
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn full_barrier(file: &std::fs::File) -> std::io::Result<()> {
    file.sync_all()
}

/// Loops until the whole buffer is written: a short write would leave a hole.
#[cfg(unix)]
fn write_all_at(file: &std::fs::File, mut offset: u64, mut buf: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    while !buf.is_empty() {
        match file.write_at(buf, offset) {
            Ok(0) => return Err(zero_write()),
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
            Ok(0) => return Err(zero_write()),
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

fn zero_write() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::WriteZero, "failed to write the whole buffer")
}

/// Reads up to `len` bytes; a short read at end-of-file is not an error.
#[cfg(unix)]
fn read_at_exact(file: &std::fs::File, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
    use std::os::unix::fs::FileExt;
    let mut buf = vec![0u8; len];
    let mut filled = 0;
    while filled < len {
        match file.read_at(&mut buf[filled..], offset + filled as u64) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    buf.truncate(filled);
    Ok(buf)
}

#[cfg(windows)]
fn read_at_exact(file: &std::fs::File, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
    use std::os::windows::fs::FileExt;
    let mut buf = vec![0u8; len];
    let mut filled = 0;
    while filled < len {
        match file.seek_read(&mut buf[filled..], offset + filled as u64) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    buf.truncate(filled);
    Ok(buf)
}
