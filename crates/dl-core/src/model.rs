//! Core types shared by the engine, the transports, and the UI.

use std::time::Duration;

/// What an origin reported about a resource during the initial probe.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SourceInfo {
    pub len: Option<u64>,
    pub accept_ranges: bool,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub content_type: Option<String>,
    /// The URL after redirects. This is what gets re-requested, not the input.
    pub final_url: String,
    /// Non-identity encoding makes `Range` arithmetic address compressed bytes
    /// while we write decompressed ones, so chunking must refuse it.
    pub content_encoding: Option<String>,
    /// Filename suggested by `Content-Disposition`, if any.
    pub suggested_filename: Option<String>,
}

impl SourceInfo {
    /// Whether this resource can safely be fetched with parallel ranges.
    pub fn supports_chunking(&self) -> bool {
        self.accept_ranges && self.len.is_some_and(|l| l > 0) && !self.has_content_encoding()
    }

    /// Why this mirror cannot be treated as the same resource as `reference`,
    /// or `None` when it can.
    ///
    /// Length is the obvious check. The validator is the load-bearing one:
    /// every chunk request carries one `If-Range` value, taken from whichever
    /// lane answered the probe, so a mirror with a different ETag would answer
    /// `200` to every chunk and read as a resource that changed mid-download.
    /// Agreement is not a nicety here, it is what makes the lanes
    /// interchangeable at all.
    pub fn disagrees_with(&self, reference: &SourceInfo) -> Option<String> {
        if self.len != reference.len {
            return Some(format!(
                "it reports {:?} bytes where the first source reports {:?}",
                self.len, reference.len
            ));
        }
        if self.etag != reference.etag {
            return Some(format!(
                "its validator is {:?} where the first source has {:?}",
                self.etag, reference.etag
            ));
        }
        None
    }

    /// True when the origin applied a content coding other than `identity`.
    pub fn has_content_encoding(&self) -> bool {
        self.content_encoding
            .as_deref()
            .is_some_and(|e| !e.trim().is_empty() && !e.eq_ignore_ascii_case("identity"))
    }
}

/// A half-open byte range, `[start, end)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ByteRange {
    pub start: u64,
    pub end: u64,
}

impl ByteRange {
    pub fn new(start: u64, end: u64) -> Self {
        debug_assert!(start <= end, "range start must not exceed end");
        Self { start, end }
    }

    pub fn len(&self) -> u64 {
        self.end - self.start
    }

    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }

    /// HTTP `Range` is inclusive at both ends, unlike this half-open type.
    pub fn to_header_value(self) -> String {
        debug_assert!(!self.is_empty(), "an empty range has no header form");
        format!("bytes={}-{}", self.start, self.end - 1)
    }
}

/// A snapshot of one download's progress. Plain data, cheap to clone.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Progress {
    pub downloaded: u64,
    pub total: Option<u64>,
    /// The instantaneous rate. This is the figure to *display*: people expect
    /// a throughput readout to react.
    pub bytes_per_sec: u64,
    /// The smoothed rate, for estimating time remaining.
    ///
    /// Separate from the one above because they answer different questions. On
    /// a multi-interface transfer the instantaneous rate swings hard as lanes
    /// are parked and unparked, and an ETA computed from it jumps between "4m"
    /// and a different "4m" every tick, which reads as broken.
    ///
    /// Zero until the first sample has been taken.
    pub smoothed_bytes_per_sec: u64,
}

impl Progress {
    /// Completed fraction in `[0, 1]`, when the total is known.
    pub fn fraction(&self) -> Option<f32> {
        match self.total {
            Some(total) if total > 0 => {
                Some((self.downloaded as f64 / total as f64).clamp(0.0, 1.0) as f32)
            }
            _ => None,
        }
    }

    /// Time remaining at the current rate.
    /// Time remaining, from the smoothed rate.
    ///
    /// `None` when nothing is moving. A stalled transfer must report no
    /// estimate rather than a growing one: dividing by a rate approaching zero
    /// produces "14h", then "3d", which is worse than an honest dash.
    pub fn eta(&self) -> Option<Duration> {
        let total = self.total?;
        let remaining = total.checked_sub(self.downloaded)?;
        let rate = self.rate_for_eta();
        if rate == 0 {
            return None;
        }
        Some(Duration::from_secs_f64(remaining as f64 / rate as f64))
    }

    /// The smoothed rate, falling back to the instantaneous one before the
    /// average has a sample: otherwise every transfer opens with no estimate
    /// at all for its first second.
    fn rate_for_eta(&self) -> u64 {
        if self.smoothed_bytes_per_sec > 0 {
            self.smoothed_bytes_per_sec
        } else {
            self.bytes_per_sec
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_header_is_inclusive_at_both_ends() {
        // [0, 1024) is bytes 0 through 1023 on the wire. An off-by-one here
        // would silently fetch one byte too many on every chunk.
        assert_eq!(ByteRange::new(0, 1024).to_header_value(), "bytes=0-1023");
        assert_eq!(ByteRange::new(1024, 2048).to_header_value(), "bytes=1024-2047");
        assert_eq!(ByteRange::new(5, 6).to_header_value(), "bytes=5-5");
    }

    #[test]
    fn range_length() {
        assert_eq!(ByteRange::new(10, 30).len(), 20);
        assert!(ByteRange::new(4, 4).is_empty());
    }

    #[test]
    fn content_encoding_blocks_chunking() {
        let base = SourceInfo { len: Some(1000), accept_ranges: true, ..Default::default() };
        assert!(base.supports_chunking());

        // Range arithmetic over compressed bytes would corrupt the output.
        let gzipped = SourceInfo { content_encoding: Some("gzip".into()), ..base.clone() };
        assert!(!gzipped.supports_chunking());

        // `identity` is not a real coding and must not block chunking.
        let identity = SourceInfo { content_encoding: Some("identity".into()), ..base.clone() };
        assert!(identity.supports_chunking());
        assert!(!identity.has_content_encoding());
    }

    #[test]
    fn chunking_needs_a_known_nonzero_length() {
        let no_len = SourceInfo { accept_ranges: true, ..Default::default() };
        assert!(!no_len.supports_chunking());

        let zero = SourceInfo { len: Some(0), accept_ranges: true, ..Default::default() };
        assert!(!zero.supports_chunking());
    }

    #[test]
    fn a_mirror_serving_something_else_is_not_interchangeable() {
        let reference = SourceInfo {
            len: Some(1000),
            etag: Some("\"v1\"".into()),
            accept_ranges: true,
            ..Default::default()
        };
        assert_eq!(reference.disagrees_with(&reference), None);

        let shorter = SourceInfo { len: Some(999), ..reference.clone() };
        assert!(shorter.disagrees_with(&reference).is_some());

        // Same length, different bytes. Splicing these together produces a
        // file that is the right size and is not either version.
        let other_content = SourceInfo { etag: Some("\"v2\"".into()), ..reference.clone() };
        assert!(other_content.disagrees_with(&reference).is_some());

        // One validator carries every chunk request, so a mirror with none
        // cannot answer them.
        let unvalidated = SourceInfo { etag: None, ..reference.clone() };
        assert!(unvalidated.disagrees_with(&reference).is_some());
    }

    #[test]
    fn progress_fraction_and_eta() {
        let p = Progress {
            downloaded: 50,
            total: Some(200),
            bytes_per_sec: 10,
            smoothed_bytes_per_sec: 0,
        };
        assert_eq!(p.fraction(), Some(0.25));
        assert_eq!(p.eta(), Some(Duration::from_secs(15)));

        // Unknown total, or a stalled transfer, must not fabricate an estimate.
        let unknown =
            Progress { downloaded: 50, total: None, bytes_per_sec: 10, smoothed_bytes_per_sec: 0 };
        assert_eq!(unknown.fraction(), None);
        assert_eq!(unknown.eta(), None);

        let stalled = Progress {
            downloaded: 50,
            total: Some(200),
            bytes_per_sec: 0,
            smoothed_bytes_per_sec: 0,
        };
        assert_eq!(stalled.eta(), None);
    }

    fn at(downloaded: u64, total: u64, rate: u64, smoothed: u64) -> Progress {
        Progress {
            downloaded,
            total: Some(total),
            bytes_per_sec: rate,
            smoothed_bytes_per_sec: smoothed,
        }
    }

    #[test]
    fn the_estimate_comes_from_the_smoothed_rate_not_the_instantaneous_one() {
        // A lane being parked halves the instantaneous rate for one tick. The
        // estimate must not double for that tick.
        let steady = at(0, 100, 2, 10);
        assert_eq!(steady.eta(), Some(Duration::from_secs(10)));
    }

    #[test]
    fn a_stalled_transfer_reports_no_estimate_rather_than_a_growing_one() {
        // Dividing by a rate approaching zero gives "14h", then "3d". A dash
        // is the honest answer.
        assert_eq!(at(50, 100, 0, 0).eta(), None);
        assert_eq!(Progress::default().eta(), None, "nothing known at all");
    }

    #[test]
    fn the_first_sample_is_used_before_the_average_has_one() {
        // Otherwise every transfer opens with no estimate at all.
        assert_eq!(at(0, 100, 10, 0).eta(), Some(Duration::from_secs(10)));
    }

    #[test]
    fn a_sawtooth_rate_produces_a_steadier_estimate_than_its_swings() {
        // The property that matters: the estimate tracks the average rather
        // than the last tick. Modelled with the engine's own factor.
        const ALPHA: f64 = 0.3;
        let mut smoothed: Option<f64> = None;
        let mut estimates = Vec::new();
        // 8 MB/s and 2 MB/s alternating: a mean of 5, swinging 4x.
        for tick in 0..40 {
            let sample = if tick % 2 == 0 { 8_000_000.0 } else { 2_000_000.0 };
            smoothed = Some(match smoothed {
                Some(prev) => prev * (1.0 - ALPHA) + sample * ALPHA,
                None => sample,
            });
            if tick > 10 {
                let p = at(0, 500_000_000, sample as u64, smoothed.unwrap() as u64);
                estimates.push(p.eta().unwrap().as_secs_f64());
            }
        }
        let low = estimates.iter().cloned().fold(f64::MAX, f64::min);
        let high = estimates.iter().cloned().fold(0.0f64, f64::max);
        assert!(
            high / low < 2.0,
            "the estimate still swings {:.1}x; the raw rate swings 4x",
            high / low
        );
    }

    #[test]
    fn a_finished_transfer_has_no_time_left() {
        assert_eq!(at(100, 100, 0, 5).eta(), Some(Duration::ZERO));
    }
}
