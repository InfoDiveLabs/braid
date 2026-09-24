//! Formatting and parsing for human-facing input and output.

use anyhow::{Context, Result, bail};
use dl_core::integrity::{Algorithm, Digest};
use dl_core::schedule::{Schedule, TimeWindow};
use dl_core::{Progress, SourceInfo};
use std::time::Duration;

/// `512K`, `16M`, `2G`, `512KiB`, or a plain byte count.
///
/// A bare `K`/`M`/`G`/`T` is decimal, to agree with what [`bytes`] prints. The
/// `i` forms are accepted and are binary, because someone who writes `KiB`
/// means 1024 and would be surprised by anything else.
pub fn parse_size(input: &str) -> Result<u64> {
    let s = input.trim();
    if s.is_empty() {
        bail!("empty size");
    }
    let upper = s.to_ascii_uppercase();
    let body = upper.strip_suffix('B').unwrap_or(&upper);
    let (body, base) = match body.strip_suffix('I') {
        Some(rest) => (rest, 1024u64),
        None => (body, 1000u64),
    };
    let (digits, multiplier) = match body.chars().last() {
        Some('K') => (&body[..body.len() - 1], base),
        Some('M') => (&body[..body.len() - 1], base.pow(2)),
        Some('G') => (&body[..body.len() - 1], base.pow(3)),
        Some('T') => (&body[..body.len() - 1], base.pow(4)),
        _ => (body, 1),
    };
    let value: u64 =
        digits.trim().parse().with_context(|| format!("{input:?} is not a byte size"))?;
    value.checked_mul(multiplier).context("size overflows a u64")
}

/// Accepts `blake3:<hex>` or a bare 64-character hex digest.
/// Read `--verify`, in any of the shapes a checksum is published in:
/// `sha256:<hex>`, `blake3:<hex>`, `md5:<hex>`, or a bare digest.
///
/// A bare 64-character digest is read as BLAKE3, because that is what this
/// tool prints. SHA-256 is the same length, so anyone pasting one from a
/// release page has to say so: guessing would report a mismatch on a
/// perfectly good file and send the user looking for a corruption that is not
/// there.
pub fn parse_digest(input: &str) -> Result<Digest> {
    let input = input.trim();
    let (algorithm, hex) = match input.split_once(':') {
        Some((name, hex)) => (
            Algorithm::parse(name)
                .ok_or_else(|| anyhow::anyhow!("unknown digest algorithm {name:?}"))?,
            hex.trim(),
        ),
        None => (Algorithm::guess_from_hex(input).unwrap_or(Algorithm::Blake3), input),
    };
    Digest::parse(algorithm, hex).ok_or_else(|| {
        anyhow::anyhow!(
            "expected a {}-character {} digest, got {}",
            algorithm.hex_len(),
            algorithm.label(),
            hex.len()
        )
    })
}

pub fn bytes(n: u64) -> String {
    // Decimal units, matching the GUI. See `dl_gui::bridge::format_bytes`:
    // the two must agree, because the same transfer is reported by both.
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 { format!("{n} B") } else { format!("{value:.1} {}", UNITS[unit]) }
}

pub fn duration(d: Duration) -> String {
    let secs = d.as_secs();
    match secs {
        0 => format!("{}ms", d.subsec_millis()),
        1..=59 => format!("{secs}.{:01}s", d.subsec_millis() / 100),
        60..=3599 => format!("{}m {}s", secs / 60, secs % 60),
        _ => format!("{}h {}m", secs / 3600, (secs % 3600) / 60),
    }
}

pub fn describe_source(info: &SourceInfo, requested_url: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "  size      {}\n",
        info.len.map(bytes).unwrap_or_else(|| "unknown".into())
    ));
    out.push_str(&format!(
        "  ranges    {}\n",
        if info.accept_ranges {
            "supported (verified with a real ranged request)"
        } else {
            "not supported: parallel chunks are impossible"
        }
    ));
    if let Some(t) = &info.content_type {
        out.push_str(&format!("  type      {t}\n"));
    }
    if let Some(e) = &info.etag {
        out.push_str(&format!("  etag      {e}\n"));
    }
    if let Some(m) = &info.last_modified {
        out.push_str(&format!("  modified  {m}\n"));
    }
    if info.has_content_encoding() {
        out.push_str(&format!(
            "  encoding  {}: unsafe for ranged requests\n",
            info.content_encoding.as_deref().unwrap_or("?")
        ));
    }
    if info.final_url != requested_url {
        out.push_str(&format!("  resolved  {}\n", info.final_url));
    }
    out.pop();
    out
}

/// A single-line progress display on stderr, drawn only for a terminal.
pub struct ProgressBar {
    width: usize,
    enabled: bool,
}

impl Default for ProgressBar {
    fn default() -> Self {
        Self::new()
    }
}

impl ProgressBar {
    pub fn new() -> Self {
        use std::io::IsTerminal;
        // Redrawing with \r produces one line per frame when piped to a file.
        Self { width: 28, enabled: std::io::stderr().is_terminal() }
    }

    pub fn update(&mut self, p: Progress) {
        use std::io::Write;
        if !self.enabled {
            return;
        }
        let rate = format!("{}/s", bytes(p.bytes_per_sec));
        let line = match (p.fraction(), p.eta()) {
            (Some(f), eta) => {
                let filled = (f * self.width as f32).round() as usize;
                let bar: String = "█".repeat(filled) + &"░".repeat(self.width - filled);
                format!(
                    "  {bar} {:>5.1}%  {:>9}  {:>10}  {}",
                    f * 100.0,
                    bytes(p.downloaded),
                    rate,
                    eta.map(|e| format!("{} left", duration(e))).unwrap_or_default(),
                )
            }
            // Unknown total: show throughput rather than a fake percentage.
            (None, _) => format!("  {:>9} downloaded  {:>10}", bytes(p.downloaded), rate),
        };
        let _ = write!(std::io::stderr(), "\r{line}\x1b[K");
        let _ = std::io::stderr().flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_suffixes() {
        assert_eq!(parse_size("1024").unwrap(), 1024);
        assert_eq!(parse_size("1K").unwrap(), 1_000);
        assert_eq!(parse_size("16M").unwrap(), 16_000_000);
        assert_eq!(parse_size("2G").unwrap(), 2_000_000_000);
        assert_eq!(parse_size("1g").unwrap(), 1_000_000_000);
        assert_eq!(parse_size("5MB").unwrap(), 5_000_000);
        // Anyone who writes the binary suffix means the binary number.
        assert_eq!(parse_size("1KiB").unwrap(), 1024);
        assert_eq!(parse_size("2Mi").unwrap(), 2 << 20);
        assert!(parse_size("").is_err());
        assert!(parse_size("banana").is_err());
    }

    #[test]
    fn blake3_digests_with_and_without_a_prefix() {
        let hash = blake3::hash(b"hello");
        let hex = hash.to_hex().to_string();
        assert_eq!(parse_digest(&hex).unwrap(), Digest::from(hash));
        assert_eq!(parse_digest(&format!("blake3:{hex}")).unwrap(), Digest::from(hash));
        // A SHA-256 digest is also 64 hex chars, so length alone cannot
        // distinguish them; the prefix is what disambiguates.
        let sha = parse_digest(&format!("sha256:{hex}")).unwrap();
        assert_eq!(sha.algorithm(), Algorithm::Sha256);
        assert_ne!(sha, Digest::from(hash), "the algorithm is part of the identity");
        // MD5 is the one length that is unambiguous.
        let md5 = parse_digest(&"a".repeat(32)).unwrap();
        assert_eq!(md5.algorithm(), Algorithm::Md5);
        assert!(parse_digest("abc").is_err());
        assert!(parse_digest("sha999:abc").is_err());
    }

    #[test]
    fn byte_formatting() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(999), "999 B");
        assert_eq!(bytes(1_000), "1.0 KB");
        assert_eq!(bytes(1_500), "1.5 KB");
        assert_eq!(bytes(1_000_000_000), "1.0 GB");
    }

    #[test]
    fn duration_formatting() {
        assert_eq!(duration(Duration::from_millis(250)), "250ms");
        assert_eq!(duration(Duration::from_secs(90)), "1m 30s");
        assert_eq!(duration(Duration::from_secs(7200)), "2h 0m");
    }
}

/// Parse `--at HH:MM-HH:MM=RATE` window specifications.
///
/// `off` means unlimited during the window, which is the common case: a
/// daytime cap that lifts overnight.
pub fn parse_windows(specs: &[String], default_limit: Option<u64>) -> Result<Option<Schedule>> {
    if specs.is_empty() {
        return Ok(None);
    }
    let mut schedule = Schedule::with_default(default_limit);

    for spec in specs {
        let (range, rate) = spec
            .split_once('=')
            .with_context(|| format!("expected HH:MM-HH:MM=RATE, got {spec:?}"))?;
        let (start, end) = range
            .split_once('-')
            .with_context(|| format!("expected HH:MM-HH:MM, got {range:?}"))?;

        let limit = match rate.trim() {
            "off" | "none" | "unlimited" => None,
            value => Some(parse_size(value)?),
        };
        schedule.push(TimeWindow::every_day(parse_hhmm(start)?, parse_hhmm(end)?, limit));
    }
    Ok(Some(schedule))
}

fn parse_hhmm(value: &str) -> Result<u16> {
    let (hours, minutes) =
        value.trim().split_once(':').with_context(|| format!("expected HH:MM, got {value:?}"))?;
    let hours: u16 = hours.trim().parse().with_context(|| format!("bad hour in {value:?}"))?;
    let minutes: u16 =
        minutes.trim().parse().with_context(|| format!("bad minute in {value:?}"))?;
    if hours > 23 || minutes > 59 {
        bail!("{value:?} is not a valid time of day");
    }
    Ok(hours * 60 + minutes)
}

#[cfg(test)]
mod window_tests {
    use super::*;
    use dl_core::schedule::{LocalTime, Weekday};

    #[test]
    fn a_window_switches_the_limit_at_its_boundaries() {
        let schedule =
            parse_windows(&["01:00-06:00=off".to_string()], Some(1 << 20)).unwrap().unwrap();

        assert_eq!(schedule.limit_at(LocalTime::new(Weekday::Monday, 3, 0)), None);
        assert_eq!(schedule.limit_at(LocalTime::new(Weekday::Monday, 12, 0)), Some(1 << 20));
    }

    #[test]
    fn windows_accept_a_rate_as_well_as_off() {
        let schedule = parse_windows(&["09:00-17:00=500K".to_string()], None).unwrap().unwrap();
        assert_eq!(schedule.limit_at(LocalTime::new(Weekday::Monday, 10, 0)), Some(500_000));
        assert_eq!(schedule.limit_at(LocalTime::new(Weekday::Monday, 20, 0)), None);
    }

    #[test]
    fn malformed_windows_are_rejected() {
        for bad in ["09:00-17:00", "9-17=1M", "25:00-26:00=1M", "09:60-10:00=1M"] {
            assert!(parse_windows(&[bad.to_string()], None).is_err(), "{bad} was accepted");
        }
    }

    #[test]
    fn no_windows_means_no_schedule() {
        assert!(parse_windows(&[], Some(1000)).unwrap().is_none());
    }
}
