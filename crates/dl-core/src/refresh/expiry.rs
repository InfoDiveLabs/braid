//! How long a link is good for.
//!
//! Signed URLs carry their lifetime in the query string rather than in a
//! header: `X-Amz-Expires` is a query parameter, not `X-Amz-Expires:`: so
//! both places are read. Caching headers are the fallback.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A signed URL's lifetime, taken from the query string.
///
/// Measured from now rather than from `X-Amz-Date`. The URL was minted moments
/// ago in the case that matters, and treating it as younger than it is would
/// refresh late; treating it as newly issued refreshes early, which is the
/// harmless direction.
pub fn lifetime_from_url(url: &str) -> Option<Duration> {
    let query = url.split_once('?')?.1;
    for pair in query.split('&') {
        let Some((key, value)) = pair.split_once('=') else { continue };
        if key.eq_ignore_ascii_case("X-Amz-Expires")
            && let Ok(seconds) = value.trim().parse::<u64>()
            && seconds > 0
        {
            return Some(Duration::from_secs(seconds));
        }
    }
    None
}

/// A lifetime from caching headers: `Cache-Control: max-age` first, because it
/// is relative and therefore immune to a skewed clock, then `Expires`.
pub fn lifetime_from_headers(headers: &[(String, String)]) -> Option<Duration> {
    let get = |name: &str| {
        headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    };

    if let Some(control) = get("cache-control")
        && let Some(seconds) = max_age(control)
    {
        return Some(Duration::from_secs(seconds));
    }

    let expires = get("expires")?;
    let at = http_date_to_unix(expires)?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs() as i64;
    let remaining = at - now;
    if remaining > 0 { Some(Duration::from_secs(remaining as u64)) } else { None }
}

/// The lifetime of a resolved source, preferring what the signer said.
pub fn lifetime_of(url: &str, headers: &[(String, String)]) -> Option<Duration> {
    lifetime_from_url(url).or_else(|| lifetime_from_headers(headers))
}

fn max_age(cache_control: &str) -> Option<u64> {
    for directive in cache_control.split(',') {
        let directive = directive.trim();
        if let Some(value) = directive.strip_prefix("max-age")
            && let Some(seconds) = value.trim().strip_prefix('=')
            && let Ok(seconds) = seconds.trim().parse::<u64>()
        {
            return Some(seconds);
        }
    }
    None
}

/// Parse an IMF-fixdate, `Sun, 06 Nov 1994 08:49:37 GMT`, to a Unix timestamp.
///
/// Only the preferred form of RFC 9110 is accepted. The obsolete forms appear
/// on origins old enough that their `Expires` values are not worth trusting.
fn http_date_to_unix(value: &str) -> Option<i64> {
    let value = value.trim();
    let rest = value.split_once(',')?.1.trim();
    let mut parts = rest.split_whitespace();

    let day: i64 = parts.next()?.parse().ok()?;
    let month = month_number(parts.next()?)?;
    let year: i64 = parts.next()?.parse().ok()?;
    let time = parts.next()?;
    if parts.next() != Some("GMT") {
        return None;
    }

    let mut clock = time.split(':');
    let hour: i64 = clock.next()?.parse().ok()?;
    let minute: i64 = clock.next()?.parse().ok()?;
    let second: i64 = clock.next()?.parse().ok()?;
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }

    Some(days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second)
}

fn month_number(name: &str) -> Option<i64> {
    const MONTHS: [&str; 12] =
        ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    MONTHS.iter().position(|m| *m == name).map(|i| i as i64 + 1)
}

/// Days since 1970-01-01, by Howard Hinnant's civil-from-days algorithm.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_signed_urls_lifetime_comes_from_its_query_string() {
        // X-Amz-Expires is a query parameter. Looking for it among the headers
        // finds nothing and refreshes only reactively, which is sixteen 403s.
        assert_eq!(
            lifetime_from_url("https://b.s3.test/k?X-Amz-Date=20240101T000000Z&X-Amz-Expires=900"),
            Some(Duration::from_secs(900))
        );
        assert_eq!(
            lifetime_from_url("https://b.s3.test/k?x-amz-expires=60"),
            Some(Duration::from_secs(60))
        );
        assert_eq!(lifetime_from_url("https://b.s3.test/k"), None);
        assert_eq!(lifetime_from_url("https://b.s3.test/k?X-Amz-Expires=0"), None);
    }

    #[test]
    fn max_age_is_preferred_over_an_absolute_expiry() {
        // A relative lifetime survives a skewed clock; an absolute one does not.
        let headers = vec![
            ("Cache-Control".to_string(), "private, max-age=120".to_string()),
            ("Expires".to_string(), "Sun, 06 Nov 1994 08:49:37 GMT".to_string()),
        ];
        assert_eq!(lifetime_from_headers(&headers), Some(Duration::from_secs(120)));
    }

    #[test]
    fn an_expiry_already_in_the_past_yields_no_lifetime() {
        let headers = vec![("Expires".to_string(), "Sun, 06 Nov 1994 08:49:37 GMT".to_string())];
        assert_eq!(lifetime_from_headers(&headers), None);
    }

    #[test]
    fn http_dates_convert_to_the_documented_timestamp() {
        // The example from RFC 9110.
        assert_eq!(http_date_to_unix("Sun, 06 Nov 1994 08:49:37 GMT"), Some(784_111_777));
        assert_eq!(http_date_to_unix("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        assert_eq!(http_date_to_unix("Sunday, 06-Nov-94 08:49:37 GMT"), None);
        assert_eq!(http_date_to_unix("nonsense"), None);
    }

    #[test]
    fn the_query_string_wins_over_caching_headers() {
        let headers = vec![("Cache-Control".to_string(), "max-age=5".to_string())];
        assert_eq!(
            lifetime_of("https://b.s3.test/k?X-Amz-Expires=900", &headers),
            Some(Duration::from_secs(900))
        );
    }
}
