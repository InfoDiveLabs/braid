//! Single-connection download loop.
//!
//! Invariants, which also hold for every chunk once phase 3 adds parallelism:
//! a body shorter or longer than its declared length is an error, a
//! non-identity `Content-Encoding` is refused, and nothing reaches the
//! destination path until the transfer has verified.

use crate::budget::BudgetChain;
use crate::error::{Error, Result};
use crate::integrity::{Algorithm, Digest, Hasher};
use crate::model::{ByteRange, Progress, SourceInfo};
use crate::source::{ByteSource, Fetch};
use crate::store::Storage;
use futures_util::StreamExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Called often; must stay cheap.
pub type ProgressFn = Box<dyn FnMut(Progress) + Send>;

pub struct DownloadOptions {
    /// Verify the finished file against this digest, in whichever algorithm
    /// the publisher used.
    pub expect: Option<Digest>,
    /// How often progress is reported.
    pub progress_interval: Duration,
    /// Fetch only this range, producing a file of exactly that length.
    ///
    /// The length check still applies, so a stream ending early inside the
    /// range is an error. Requires the origin to honour `Range`.
    pub range: Option<ByteRange>,
    /// Bandwidth limits. Applied here too, so a limit means the same thing
    /// whichever path a download takes.
    pub budget: BudgetChain,
}

impl Default for DownloadOptions {
    fn default() -> Self {
        // 10 Hz bounds UI-thread wakeups regardless of download count.
        Self {
            expect: None,
            progress_interval: Duration::from_millis(100),
            range: None,
            budget: BudgetChain::unlimited(),
        }
    }
}

#[derive(Debug)]
pub struct Outcome {
    pub path: PathBuf,
    pub bytes: u64,
    pub blake3: blake3::Hash,
    pub elapsed: Duration,
    pub info: SourceInfo,
}

impl Outcome {
    pub fn bytes_per_sec(&self) -> u64 {
        let secs = self.elapsed.as_secs_f64();
        if secs <= 0.0 { 0 } else { (self.bytes as f64 / secs) as u64 }
    }
}

/// Fetch `source` in one connection and write it through `storage`.
///
/// Any failure discards the partial file.
pub async fn download(
    source: &dyn ByteSource,
    storage: &dyn Storage,
    options: DownloadOptions,
    on_progress: Option<ProgressFn>,
) -> Result<Outcome> {
    match attempt(source, storage, options, on_progress).await {
        Ok(outcome) => Ok(outcome),
        Err(e) => {
            // Covers probe failures too: creating the storage already put an
            // empty `.part` on disk. Cleanup failure must not mask the
            // transfer error.
            if let Err(cleanup) = storage.discard().await {
                tracing::warn!(error = %cleanup, "could not remove the partial file");
            }
            Err(e)
        }
    }
}

async fn attempt(
    source: &dyn ByteSource,
    storage: &dyn Storage,
    options: DownloadOptions,
    mut on_progress: Option<ProgressFn>,
) -> Result<Outcome> {
    let info = source.probe().await?;

    if info.has_content_encoding() {
        return Err(Error::UnexpectedContentEncoding(
            info.content_encoding.clone().unwrap_or_default(),
        ));
    }

    if options.range.is_some() && !info.accept_ranges {
        return Err(Error::Transport(
            "a byte range was requested but the origin does not support ranges".into(),
        ));
    }

    let (bytes, hash, digest, elapsed) =
        stream_to_storage(source, storage, &info, &options, &mut on_progress).await?;

    if let Some(expected) = &options.expect {
        // `digest` is the expected algorithm's hash when it is not BLAKE3;
        // otherwise the content hash we computed anyway serves.
        let actual = digest.unwrap_or_else(|| Digest::from(hash));
        if *expected != actual {
            return Err(Error::IntegrityMismatch {
                expected: expected.to_hex(),
                actual: actual.to_hex(),
            });
        }
    }

    let path = storage.finalize().await?;
    Ok(Outcome { path, bytes, blake3: hash, elapsed, info })
}

async fn stream_to_storage(
    source: &dyn ByteSource,
    storage: &dyn Storage,
    info: &SourceInfo,
    options: &DownloadOptions,
    on_progress: &mut Option<ProgressFn>,
) -> Result<(u64, blake3::Hash, Option<Digest>, Duration)> {
    let expected_len = options.range.map(|r| r.len()).or(info.len);
    let mut stream = source.open(Fetch { range: options.range, if_range: None }).await?;

    let started = Instant::now();
    let mut last_report = started;
    // Relative to the output file: a ranged fetch still starts at 0.
    let mut offset = 0u64;
    let mut hasher = blake3::Hasher::new();
    // A second pass only when the publisher used something other than BLAKE3.
    // Hashing twice costs a pass over a buffer that is already in cache; the
    // alternative is refusing the checksums most mirrors actually publish.
    let mut expected_hasher = options
        .expect
        .as_ref()
        .filter(|digest| digest.algorithm() != Algorithm::Blake3)
        .map(|digest| Hasher::new(digest.algorithm()));

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if chunk.is_empty() {
            continue;
        }

        // Checked before writing so the excess never lands.
        if let Some(expected) = expected_len
            && offset + chunk.len() as u64 > expected
        {
            return Err(Error::OverlongBody { expected });
        }

        // Charged after arrival: the bytes are already in the socket buffer,
        // and pacing here is what slows the sender through TCP back-pressure.
        let mut owed = chunk.len();
        while owed > 0 {
            owed -= options.budget.acquire(owed).await;
        }

        hasher.update(&chunk);
        if let Some(extra) = expected_hasher.as_mut() {
            extra.update(&chunk);
        }
        let len = chunk.len() as u64;
        storage.write_at(offset, chunk).await?;
        offset += len;

        if let Some(report) = on_progress.as_mut() {
            let now = Instant::now();
            if now.duration_since(last_report) >= options.progress_interval {
                report(Progress {
                    downloaded: offset,
                    total: expected_len,
                    bytes_per_sec: rate(offset, started.elapsed()),
                    smoothed_bytes_per_sec: 0,
                });
                last_report = now;
            }
        }
    }

    // Without this, a dropped connection yields a silently truncated file.
    if let Some(expected) = expected_len
        && offset != expected
    {
        return Err(Error::ShortBody { expected, received: offset });
    }

    let elapsed = started.elapsed();
    if let Some(report) = on_progress.as_mut() {
        report(Progress {
            downloaded: offset,
            total: expected_len.or(Some(offset)),
            bytes_per_sec: rate(offset, elapsed),
            smoothed_bytes_per_sec: 0,
        });
    }

    Ok((offset, hasher.finalize(), expected_hasher.map(Hasher::finalize), elapsed))
}

fn rate(bytes: u64, elapsed: Duration) -> u64 {
    let secs = elapsed.as_secs_f64();
    if secs <= 0.0 { 0 } else { (bytes as f64 / secs) as u64 }
}
