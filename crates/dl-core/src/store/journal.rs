//! Crash-safe record of which chunks are durably on disk.
//!
//! Layout: two 4 KiB headers followed by an append-only log.
//!
//! ```text
//! 0x0000  header A
//! 0x1000  header B
//! 0x2000  log records
//! ```
//!
//! Headers alternate, and the one with the higher sequence number whose CRC
//! validates wins, so a torn header write can never destroy both. Each log
//! record carries its own CRC; replay stops at the first record that fails or
//! is short, which is the crash point.
//!
//! Each completion also records the chunk's BLAKE3. That makes corruption
//! localized: a mismatch identifies one chunk to re-fetch rather than
//! invalidating the whole file.
//!
//! The ordering rule the whole design rests on:
//!
//! > chunk data is made durable **before** the journal claims the chunk.
//!
//! Violating it means resume trusts bytes that were never written, which
//! produces a corrupt file that looks complete. [`Journal::record_chunk`]
//! cannot express the wrong order: it syncs the data file itself.
//!
//! [`Durability`] widens or narrows the window between flushes but never
//! touches that ordering. A completion held back from the log is a chunk that
//! has to be fetched again after a crash; it is never a chunk the log claims
//! and the data file does not have.

use super::durability::Durability;
use super::layout::{ChunkLayout, Completed};
use super::rawfile::RawFile;
use crate::error::Result;
use std::sync::Arc;
use std::time::Instant;

const MAGIC: &[u8; 8] = b"DLMETA\x00\x01";
const VERSION: u32 = 1;
const HEADER_SIZE: usize = 4096;
const HEADER_A: u64 = 0;
const HEADER_B: u64 = HEADER_SIZE as u64;
pub const LOG_START: u64 = 2 * HEADER_SIZE as u64;

const ETAG_MAX: usize = 256;
const MODIFIED_MAX: usize = 64;
const BITMAP_OFFSET: usize = 408;
const BITMAP_MAX: usize = HEADER_SIZE - BITMAP_OFFSET - 4;

const KIND_CHUNK_COMPLETE: u8 = 1;
const KIND_RESET: u8 = 2;
const RECORD_HEADER: usize = 9;
const COMPLETE_PAYLOAD: usize = 8 + 32;

/// Identity of the resource a journal belongs to.
///
/// Resume is only safe if the bytes on disk came from the same resource, so a
/// mismatch here discards the partial file rather than continuing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResourceId {
    pub total_len: u64,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

impl ResourceId {
    /// Whether a partial file recorded under `self` may be resumed for `other`.
    ///
    /// Resume splices newly fetched bytes onto bytes fetched earlier, so it
    /// needs proof the two came from identical content. A changed length always
    /// disqualifies. A matching strong ETag is proof. A weak ETag (`W/"..."`)
    /// only promises semantic equivalence and explicitly declines to promise
    /// identical bytes, so it is refused even when both sides match: costing a
    /// re-download rather than risking a spliced file.
    pub fn can_resume_as(&self, other: &ResourceId) -> bool {
        if self.total_len != other.total_len {
            return false;
        }
        match (&self.etag, &other.etag) {
            (Some(a), Some(b)) => is_strong(a) && is_strong(b) && a == b,
            // One side has an ETag and the other does not: the resource is not
            // being described consistently, so do not assume it is unchanged.
            (Some(_), None) | (None, Some(_)) => false,
            (None, None) => match (&self.last_modified, &other.last_modified) {
                (Some(a), Some(b)) => a == b,
                // Only the length matches. Accepted, with the final digest
                // check as the backstop.
                _ => true,
            },
        }
    }
}

fn is_strong(etag: &str) -> bool {
    !etag.trim_start().starts_with("W/")
}

#[derive(Clone, Debug)]
struct Header {
    seqno: u64,
    total_len: u64,
    chunk_size: u64,
    chunk_count: u64,
    committed_log_len: u64,
    etag: Option<String>,
    last_modified: Option<String>,
    bits: Vec<u8>,
}

impl Header {
    fn encode(&self) -> Vec<u8> {
        let mut buf = vec![0u8; HEADER_SIZE];
        buf[0..8].copy_from_slice(MAGIC);
        buf[8..12].copy_from_slice(&VERSION.to_le_bytes());
        buf[16..24].copy_from_slice(&self.seqno.to_le_bytes());
        buf[24..32].copy_from_slice(&self.total_len.to_le_bytes());
        buf[32..40].copy_from_slice(&self.chunk_size.to_le_bytes());
        buf[40..48].copy_from_slice(&self.chunk_count.to_le_bytes());
        buf[48..56].copy_from_slice(&self.committed_log_len.to_le_bytes());

        let etag = truncated(self.etag.as_deref(), ETAG_MAX);
        let modified = truncated(self.last_modified.as_deref(), MODIFIED_MAX);
        buf[56..58].copy_from_slice(&(etag.len() as u16).to_le_bytes());
        buf[58..60].copy_from_slice(&(modified.len() as u16).to_le_bytes());
        buf[64..64 + etag.len()].copy_from_slice(etag);
        buf[320..320 + modified.len()].copy_from_slice(modified);

        let bits = &self.bits[..self.bits.len().min(BITMAP_MAX)];
        buf[BITMAP_OFFSET..BITMAP_OFFSET + bits.len()].copy_from_slice(bits);

        let crc = crc32c::crc32c(&buf[..HEADER_SIZE - 4]);
        buf[HEADER_SIZE - 4..].copy_from_slice(&crc.to_le_bytes());
        buf
    }

    fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < HEADER_SIZE || &buf[0..8] != MAGIC {
            return None;
        }
        let stored = u32::from_le_bytes(buf[HEADER_SIZE - 4..HEADER_SIZE].try_into().ok()?);
        if crc32c::crc32c(&buf[..HEADER_SIZE - 4]) != stored {
            return None;
        }
        if u32::from_le_bytes(buf[8..12].try_into().ok()?) != VERSION {
            return None;
        }

        let etag_len = u16::from_le_bytes(buf[56..58].try_into().ok()?) as usize;
        let modified_len = u16::from_le_bytes(buf[58..60].try_into().ok()?) as usize;
        if etag_len > ETAG_MAX || modified_len > MODIFIED_MAX {
            return None;
        }

        Some(Self {
            seqno: u64::from_le_bytes(buf[16..24].try_into().ok()?),
            total_len: u64::from_le_bytes(buf[24..32].try_into().ok()?),
            chunk_size: u64::from_le_bytes(buf[32..40].try_into().ok()?),
            chunk_count: u64::from_le_bytes(buf[40..48].try_into().ok()?),
            committed_log_len: u64::from_le_bytes(buf[48..56].try_into().ok()?),
            etag: string_or_none(&buf[64..64 + etag_len]),
            last_modified: string_or_none(&buf[320..320 + modified_len]),
            bits: buf[BITMAP_OFFSET..BITMAP_OFFSET + BITMAP_MAX].to_vec(),
        })
    }
}

fn truncated(value: Option<&str>, max: usize) -> &[u8] {
    let bytes = value.unwrap_or("").as_bytes();
    &bytes[..bytes.len().min(max)]
}

fn string_or_none(bytes: &[u8]) -> Option<String> {
    if bytes.is_empty() {
        return None;
    }
    std::str::from_utf8(bytes).ok().map(str::to_owned)
}

/// What opening an existing journal produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Opened {
    /// No usable journal; starting from scratch.
    Fresh,
    /// A journal for this exact resource; completed chunks are reusable.
    Resumed,
    /// A journal existed but described different content, so it was discarded.
    Restarted,
}

pub struct Journal {
    file: Arc<dyn RawFile>,
    data: Arc<dyn RawFile>,
    layout: ChunkLayout,
    completed: Completed,
    /// Per-chunk BLAKE3, by index. Only meaningful for completed chunks.
    hashes: std::collections::BTreeMap<u64, [u8; 32]>,
    resource: ResourceId,
    seqno: u64,
    log_len: u64,
    /// Log bytes already folded into a header.
    committed_log_len: u64,
    opened_as: Opened,

    durability: Durability,
    /// Chunks written and hashed but not yet in the log. A crash loses these
    /// and they are fetched again; it never loses the guarantee that what the
    /// log does claim is on disk.
    pending: Vec<(u64, [u8; 32])>,
    pending_bytes: u64,
    last_flush: Instant,
}

impl Journal {
    /// Open or create a journal for `resource`, alongside the data file.
    ///
    /// A journal describing different content is discarded rather than merged.
    pub async fn open(
        file: Arc<dyn RawFile>,
        data: Arc<dyn RawFile>,
        resource: ResourceId,
        chunk_size: u64,
    ) -> Result<Self> {
        Self::open_with(file, data, resource, chunk_size, Durability::default()).await
    }

    pub async fn open_with(
        file: Arc<dyn RawFile>,
        data: Arc<dyn RawFile>,
        resource: ResourceId,
        chunk_size: u64,
        durability: Durability,
    ) -> Result<Self> {
        let layout = ChunkLayout::new(resource.total_len, chunk_size);
        let existing = Self::load_header(&file).await?;

        let mut hashes = std::collections::BTreeMap::new();
        let (completed, seqno, log_len, committed_log_len, opened_as) = match existing {
            Some(header) => {
                let recorded = ResourceId {
                    total_len: header.total_len,
                    etag: header.etag.clone(),
                    last_modified: header.last_modified.clone(),
                };
                if recorded.can_resume_as(&resource) && header.chunk_size == layout.chunk_size() {
                    let mut set = Completed::from_bits(&header.bits, header.chunk_count);
                    let replayed =
                        Self::replay(&file, header.committed_log_len, &mut set, &mut hashes)
                            .await?;
                    (set, header.seqno + 1, replayed, header.committed_log_len, Opened::Resumed)
                } else {
                    (
                        Completed::new(layout.chunk_count()),
                        header.seqno + 1,
                        0,
                        0,
                        Opened::Restarted,
                    )
                }
            }
            None => (Completed::new(layout.chunk_count()), 1, 0, 0, Opened::Fresh),
        };

        let mut journal = Self {
            file,
            data,
            layout,
            completed,
            hashes,
            resource,
            seqno,
            log_len,
            committed_log_len,
            opened_as,
            durability,
            pending: Vec::new(),
            pending_bytes: 0,
            last_flush: Instant::now(),
        };

        if opened_as != Opened::Resumed {
            journal.hashes.clear();
            journal.completed.clear();
            journal.log_len = 0;
            journal.committed_log_len = 0;
            journal.checkpoint().await?;
        }
        Ok(journal)
    }

    pub fn opened_as(&self) -> Opened {
        self.opened_as
    }

    pub fn layout(&self) -> &ChunkLayout {
        &self.layout
    }

    pub fn completed(&self) -> &Completed {
        &self.completed
    }

    pub fn bytes_done(&self) -> u64 {
        self.completed.bytes_done(&self.layout)
    }

    pub fn is_complete(&self) -> bool {
        self.completed.all_complete()
    }

    /// Chunks still to fetch.
    pub fn remaining(&self) -> Vec<u64> {
        self.completed.missing().collect()
    }

    /// Mark `index` durable, recording the hash of its bytes.
    ///
    /// Syncs the data file first, then appends and syncs the journal. The two
    /// steps are inseparable here so no caller can invert them: a journal entry
    /// that outran its data is what turns a crash into a corrupt file.
    pub async fn record_chunk(&mut self, index: u64, hash: blake3::Hash) -> Result<()> {
        let len = self.layout.range(index).map(|r| r.len()).unwrap_or(0);
        self.pending.push((index, *hash.as_bytes()));
        self.pending_bytes += len;

        // In memory immediately, so an in-process resume and the "is it done"
        // question both see the chunk. Only the on-disk claim is deferred.
        self.hashes.insert(index, *hash.as_bytes());
        self.completed.insert(index);

        if self.flush_is_due() {
            self.flush().await?;
        }
        Ok(())
    }

    fn flush_is_due(&self) -> bool {
        self.durability.flushes_every_chunk()
            || self.pending_bytes >= self.durability.byte_floor()
            || self.last_flush.elapsed() >= self.durability.time_floor()
    }

    /// Make every pending completion durable, data first.
    ///
    /// The single place the ordering rule is expressed, in every mode.
    pub async fn flush(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            self.last_flush = Instant::now();
            return Ok(());
        }
        self.data.sync_data().await?;

        for (index, hash) in std::mem::take(&mut self.pending) {
            let mut payload = Vec::with_capacity(COMPLETE_PAYLOAD);
            payload.extend_from_slice(&index.to_le_bytes());
            payload.extend_from_slice(&hash);
            self.append(KIND_CHUNK_COMPLETE, &payload).await?;
        }

        if self.durability.media_barrier() {
            self.file.sync_barrier().await?;
        } else {
            self.file.sync_data().await?;
        }

        self.pending_bytes = 0;
        self.last_flush = Instant::now();
        Ok(())
    }

    /// How many chunks are written but not yet claimed by the log: what a
    /// crash would cost right now.
    pub fn pending_chunks(&self) -> usize {
        self.pending.len()
    }

    /// The recorded hash for a completed chunk.
    pub fn chunk_hash(&self, index: u64) -> Option<blake3::Hash> {
        self.hashes.get(&index).copied().map(blake3::Hash::from)
    }

    /// Forget a chunk so it will be fetched again.
    pub async fn forget_chunk(&mut self, index: u64) -> Result<()> {
        self.completed.remove(index);
        self.hashes.remove(&index);
        // Written as a checkpoint rather than a log record: the log only ever
        // adds completions, so a removal has to go through the header.
        self.checkpoint().await
    }

    /// Discard all progress, recording the reset durably.
    pub async fn reset(&mut self) -> Result<()> {
        self.append(KIND_RESET, &[]).await?;
        self.completed.clear();
        self.hashes.clear();
        self.checkpoint().await
    }

    /// Fold the log into a header so replay stays bounded.
    ///
    /// Flushes first: the header carries the completed bitmap, so writing it
    /// with pending completions still in memory would claim chunks the data
    /// file has not been synced for: the one thing this design must not do.
    pub async fn checkpoint(&mut self) -> Result<()> {
        self.flush().await?;
        let header = Header {
            seqno: self.seqno,
            total_len: self.resource.total_len,
            chunk_size: self.layout.chunk_size(),
            chunk_count: self.layout.chunk_count(),
            committed_log_len: self.log_len,
            etag: self.resource.etag.clone(),
            last_modified: self.resource.last_modified.clone(),
            bits: self.completed.as_bits().to_vec(),
        };

        // Alternate slots so a torn write cannot destroy the previous header.
        let slot = if self.seqno % 2 == 1 { HEADER_A } else { HEADER_B };
        self.file.write_at(slot, bytes::Bytes::from(header.encode())).await?;
        self.file.sync_barrier().await?;

        self.committed_log_len = self.log_len;
        self.seqno += 1;
        Ok(())
    }

    async fn append(&mut self, kind: u8, payload: &[u8]) -> Result<()> {
        let mut record = Vec::with_capacity(RECORD_HEADER + payload.len());
        record.extend_from_slice(&0u32.to_le_bytes());
        record.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        record.push(kind);
        record.extend_from_slice(payload);
        let crc = crc32c::crc32c(&record[4..]);
        record[0..4].copy_from_slice(&crc.to_le_bytes());

        let at = LOG_START + self.log_len;
        self.log_len += record.len() as u64;
        self.file.write_at(at, bytes::Bytes::from(record)).await
    }

    async fn load_header(file: &Arc<dyn RawFile>) -> Result<Option<Header>> {
        let buf = file.read_at(0, 2 * HEADER_SIZE).await?;
        let a = buf.get(..HEADER_SIZE).and_then(Header::decode);
        let b = buf.get(HEADER_SIZE..2 * HEADER_SIZE).and_then(Header::decode);

        // The newer valid header wins; a failed CRC means that slot was torn.
        Ok(match (a, b) {
            (Some(a), Some(b)) => Some(if a.seqno >= b.seqno { a } else { b }),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        })
    }

    /// Replay log records after `from`, stopping at the first damaged one.
    async fn replay(
        file: &Arc<dyn RawFile>,
        from: u64,
        set: &mut Completed,
        hashes: &mut std::collections::BTreeMap<u64, [u8; 32]>,
    ) -> Result<u64> {
        let buf = file.read_at(LOG_START + from, 1 << 20).await?;
        let mut cursor = 0usize;

        while cursor + RECORD_HEADER <= buf.len() {
            let crc = u32::from_le_bytes(buf[cursor..cursor + 4].try_into().unwrap());
            let len = u32::from_le_bytes(buf[cursor + 4..cursor + 8].try_into().unwrap()) as usize;
            let end = cursor + RECORD_HEADER + len;
            if end > buf.len() {
                break;
            }
            if crc32c::crc32c(&buf[cursor + 4..end]) != crc {
                break;
            }

            let kind = buf[cursor + 8];
            let payload = &buf[cursor + RECORD_HEADER..end];
            match kind {
                KIND_CHUNK_COMPLETE if payload.len() == COMPLETE_PAYLOAD => {
                    let index = u64::from_le_bytes(payload[..8].try_into().unwrap());
                    let hash: [u8; 32] = payload[8..].try_into().unwrap();
                    set.insert(index);
                    hashes.insert(index, hash);
                }
                KIND_RESET => {
                    set.clear();
                    hashes.clear();
                }
                // An unknown kind means a newer writer produced this journal;
                // stopping is safer than guessing what it meant.
                _ => break,
            }
            cursor = end;
        }
        Ok(from + cursor as u64)
    }
}

impl std::fmt::Debug for Journal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Journal")
            .field("opened_as", &self.opened_as)
            .field("chunks", &self.layout.chunk_count())
            .field("completed", &self.completed.len())
            .field("seqno", &self.seqno)
            .finish()
    }
}

/// Validate a journal against a data file, without opening either for writing.
pub async fn describe(file: &Arc<dyn RawFile>) -> Result<Option<(u64, u64, u64)>> {
    let Some(header) = Journal::load_header(file).await? else {
        return Ok(None);
    };
    let mut set = Completed::from_bits(&header.bits, header.chunk_count);
    let mut hashes = std::collections::BTreeMap::new();
    Journal::replay(file, header.committed_log_len, &mut set, &mut hashes).await?;
    Ok(Some((header.total_len, header.chunk_count, set.len())))
}

impl Journal {
    /// Re-read from disk, as a fresh process would. Used by crash tests.
    pub async fn reopen(&self) -> Result<Self> {
        Journal::open(
            Arc::clone(&self.file),
            Arc::clone(&self.data),
            self.resource.clone(),
            self.layout.chunk_size(),
        )
        .await
    }
}

impl Journal {
    pub fn resource(&self) -> &ResourceId {
        &self.resource
    }
}
