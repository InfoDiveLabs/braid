//! HTTP transport.
//!
//! No compression feature is enabled on the client and every request sends
//! `Accept-Encoding: identity`. With a content coding in play, ranges address
//! compressed bytes while we write decompressed ones: silent corruption that
//! is invisible against servers which do not compress. `Cargo.toml` must never
//! gain reqwest's `gzip`, `brotli`, `zstd` or `deflate` features.

use dl_core::error::{Error, Result};
use dl_core::model::{ByteRange, SourceInfo};
use dl_core::refresh::{Attempt, ResponseSummary};
use dl_core::source::{ByteSource, ByteStream, Fetch};
use futures_util::TryStreamExt;
use reqwest::header::{
    ACCEPT_ENCODING, CONTENT_RANGE, HeaderMap, HeaderName, HeaderValue, IF_RANGE, RANGE,
};
use reqwest::{Client, RequestBuilder, Response, StatusCode};
use std::time::Duration;

/// Where a proxy comes from.
///
/// `System` is reqwest's own behaviour: it reads `HTTP_PROXY`/`HTTPS_PROXY`
/// and, on macOS and Windows, the system proxy settings. `None` turns that off
/// explicitly, which is not the same thing as not asking: a corporate machine
/// with a proxy configured needs a way to bypass it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ProxyMode {
    #[default]
    System,
    None,
    /// A single proxy for both schemes, as `host:port` or a full URL.
    Manual(String),
}

/// Shared client configuration.
#[derive(Clone, Debug)]
pub struct HttpConfig {
    pub user_agent: String,
    pub proxy: ProxyMode,
    /// Time allowed to establish a connection.
    pub connect_timeout: Duration,
    /// Idle timeout, not a total one: large downloads are legitimately slow.
    pub read_timeout: Duration,
    pub headers: HeaderMap,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            user_agent: concat!("downloader/", env!("CARGO_PKG_VERSION")).to_string(),
            proxy: ProxyMode::default(),
            connect_timeout: Duration::from_secs(15),
            read_timeout: Duration::from_secs(30),
            headers: HeaderMap::new(),
        }
    }
}

/// Shared client configuration, including the compression guards above.
///
/// Public so interface-bound clients are built the same way; a lane that
/// quietly enabled decompression would corrupt every chunk it fetched.
pub fn client_builder(config: &HttpConfig) -> reqwest::ClientBuilder {
    let builder = Client::builder()
        .user_agent(config.user_agent.clone())
        .connect_timeout(config.connect_timeout)
        .read_timeout(config.read_timeout)
        .default_headers(config.headers.clone())
        // Defence in depth against a compression feature arriving transitively.
        .no_gzip()
        .no_brotli()
        .no_zstd()
        .no_deflate();

    match &config.proxy {
        ProxyMode::System => builder,
        ProxyMode::None => builder.no_proxy(),
        // A proxy we cannot parse is reported by falling back to none rather
        // than to the system's: silently using a different route than the one
        // asked for is how traffic leaks past a proxy someone chose on purpose.
        ProxyMode::Manual(url) => match reqwest::Proxy::all(normalise_proxy(url)) {
            Ok(proxy) => builder.proxy(proxy),
            Err(error) => {
                tracing::warn!(%error, %url, "unusable proxy, going direct");
                builder.no_proxy()
            }
        },
    }
}

/// Accept `host:port` as well as a full URL, because that is what the settings
/// field asks for.
fn normalise_proxy(url: &str) -> String {
    let url = url.trim();
    if url.contains("://") { url.to_string() } else { format!("http://{url}") }
}

pub fn build_client(config: &HttpConfig) -> Result<Client> {
    client_builder(config).build().map_err(|e| Error::Transport(e.to_string()))
}

/// One HTTP resource.
pub struct HttpSource {
    client: Client,
    url: String,
}

impl HttpSource {
    pub fn new(client: Client, url: impl Into<String>) -> Self {
        Self { client, url: url.into() }
    }

    pub fn with_config(config: &HttpConfig, url: impl Into<String>) -> Result<Self> {
        Ok(Self::new(build_client(config)?, url))
    }

    pub fn url(&self) -> &str {
        &self.url
    }
}

#[async_trait::async_trait]
impl ByteSource for HttpSource {
    async fn probe(&self) -> Result<SourceInfo> {
        probe_url(&self.client, &self.url, &[]).await?.outcome
    }

    async fn open(&self, fetch: Fetch) -> Result<ByteStream> {
        open_url(&self.client, &self.url, &[], fetch, None).await?.outcome
    }
}

/// Ask an origin what it has, reporting the response even when it is refused.
///
/// A one-byte ranged GET rather than HEAD: many origins answer HEAD wrongly,
/// and a real 206 proves range support instead of claiming it.
pub async fn probe_url(
    client: &Client,
    url: &str,
    extra_headers: &[(String, String)],
) -> Result<Attempt<SourceInfo>> {
    let request = client
        .get(url)
        .header(ACCEPT_ENCODING, HeaderValue::from_static("identity"))
        .header(RANGE, HeaderValue::from_static("bytes=0-0"));
    let response = with_headers(request, extra_headers)?.send().await.map_err(transport_error)?;

    let status = response.status();
    let summary = summarise(&response, true, None);
    if !status.is_success() {
        return Ok(Attempt::refused(summary, status_error(&response, status)));
    }
    Ok(Attempt::ok(summary, source_info_from(&response, status)))
}

/// Open a body stream, reporting the response even when it is refused.
///
/// The refusal and the summary travel together because a refusal can be a
/// symptom of expiry rather than of the resource: a portal answers a ranged
/// request with a 200 and a login page, which is indistinguishable from "the
/// resource changed" until the media type is taken into account.
pub async fn open_url(
    client: &Client,
    url: &str,
    extra_headers: &[(String, String)],
    fetch: Fetch,
    expected_content_type: Option<&str>,
) -> Result<Attempt<ByteStream>> {
    let range = fetch.range.filter(|r| !r.is_empty());

    let mut request = client.get(url).header(ACCEPT_ENCODING, HeaderValue::from_static("identity"));

    if let Some(range) = range {
        request = request.header(RANGE, range.to_header_value());
        // Only a strong validator may gate a range request: a weak one does
        // not promise the bytes are unchanged, so it cannot authorise
        // splicing this range onto ranges fetched earlier.
        if let Some(validator) = fetch.if_range.as_deref().filter(|v| is_strong_validator(v))
            && let Ok(value) = HeaderValue::from_str(validator)
        {
            request = request.header(IF_RANGE, value);
        }
    }

    let response = with_headers(request, extra_headers)?.send().await.map_err(transport_error)?;
    let status = response.status();
    let summary = summarise(&response, range.is_some(), expected_content_type);

    if let Some(refusal) = refuse(&response, status, range, &fetch) {
        return Ok(Attempt::refused(summary, refusal));
    }
    Ok(Attempt::ok(summary, Box::pin(response.bytes_stream().map_err(transport_error))))
}

/// Why this response cannot be written at the requested offset, if it cannot.
/// Classify a non-success status.
///
/// 429 and 503 are singled out because they are the origin asking for a delay
/// rather than reporting a fault. Answering them the way a transport failure is
/// answered: move to another path and try again at once: is what turns one
/// rate limit into a rate limit on every interface.
fn status_error(response: &Response, status: StatusCode) -> Error {
    match status.as_u16() {
        code @ (429 | 503) => {
            Error::RateLimited { status: code, retry_after: retry_after(response.headers()) }
        }
        code => Error::Http { status: code },
    }
}

fn refuse(
    response: &Response,
    status: StatusCode,
    range: Option<ByteRange>,
    fetch: &Fetch,
) -> Option<Error> {
    if !status.is_success() {
        return Some(status_error(response, status));
    }

    // Some origins ignore `identity` and compress anyway.
    if let Some(encoding) = content_encoding(response) {
        return Some(Error::UnexpectedContentEncoding(encoding));
    }

    let range = range?;

    // A 200 to a ranged request is the authoritative signal that the whole
    // body is coming. With If-Range set, it means the validator no longer
    // matches; without it, the origin simply ignores Range. Either way,
    // writing this body at the range's offset would scatter it through the
    // file.
    if status != StatusCode::PARTIAL_CONTENT {
        return Some(if fetch.if_range.is_some() {
            Error::ResourceChanged {
                detail: format!("the origin answered {status} to a validated range request"),
            }
        } else {
            Error::RangeNotHonoured { detail: format!("replied {status} instead of 206") }
        });
    }

    // A 206 whose Content-Range describes a different span means the body is
    // not what was asked for.
    verify_content_range(response, range).err()
}

/// Attach headers an issuer handed back with the link.
///
/// An invalid name or value is an error rather than a skipped header: a
/// refresher that returns a malformed `Cookie` would otherwise look like it
/// worked and then 403 on every chunk.
fn with_headers(
    mut request: RequestBuilder,
    headers: &[(String, String)],
) -> Result<RequestBuilder> {
    for (name, value) in headers {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|e| Error::Transport(format!("invalid header name {name:?}: {e}")))?;
        let value = HeaderValue::from_str(value)
            .map_err(|e| Error::Transport(format!("invalid value for header {name}: {e}")))?;
        request = request.header(name, value);
    }
    Ok(request)
}

fn summarise(
    response: &Response,
    ranged: bool,
    expected_content_type: Option<&str>,
) -> ResponseSummary {
    ResponseSummary {
        status: response.status().as_u16(),
        content_type: response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned),
        expected_content_type: expected_content_type.map(str::to_owned),
        content_length: response.content_length(),
        ranged,
        headers: response
            .headers()
            .iter()
            .filter_map(|(k, v)| v.to_str().ok().map(|v| (k.as_str().to_owned(), v.to_owned())))
            .collect(),
    }
}

/// reqwest's top-level message is often just "error sending request"; the cause
/// chain carries what actually went wrong.
fn transport_error(e: reqwest::Error) -> Error {
    let mut message = e.to_string();
    let mut source = std::error::Error::source(&e);
    while let Some(cause) = source {
        let text = cause.to_string();
        if !message.contains(&text) {
            message.push_str(": ");
            message.push_str(&text);
        }
        source = cause.source();
    }
    Error::Transport(message)
}

/// A validator usable with `If-Range`. Weak ETags are excluded: they promise
/// semantic equivalence, not identical bytes.
fn is_strong_validator(value: &str) -> bool {
    let trimmed = value.trim_start();
    !trimmed.starts_with("W/") && !trimmed.is_empty()
}

/// Confirm the 206 describes the range that was requested.
///
/// An origin that returns a 206 for a different span, or one whose total has
/// changed since the probe, would otherwise have its bytes written at offsets
/// they do not belong to.
fn verify_content_range(response: &Response, requested: ByteRange) -> Result<()> {
    let Some(header) =
        response.headers().get(CONTENT_RANGE).and_then(|v| v.to_str().ok()).map(str::to_owned)
    else {
        return Err(Error::RangeNotHonoured { detail: "sent 206 without a Content-Range".into() });
    };

    let Some((start, end)) = parse_content_range_span(&header) else {
        return Err(Error::RangeNotHonoured {
            detail: format!("could not parse Content-Range: {header:?}"),
        });
    };

    if start != requested.start || end + 1 != requested.end {
        return Err(Error::RangeNotHonoured {
            detail: format!(
                "asked for bytes {}-{} but the origin sent {start}-{end}",
                requested.start,
                requested.end - 1
            ),
        });
    }
    Ok(())
}

/// `bytes 100-199/1000` -> `(100, 199)`.
fn parse_content_range_span(value: &str) -> Option<(u64, u64)> {
    let spec = value.trim().strip_prefix("bytes")?.trim();
    let span = spec.split('/').next()?.trim();
    let (start, end) = span.split_once('-')?;
    Some((start.trim().parse().ok()?, end.trim().parse().ok()?))
}

fn content_encoding(response: &Response) -> Option<String> {
    let value = response.headers().get(reqwest::header::CONTENT_ENCODING)?.to_str().ok()?.trim();
    if value.is_empty() || value.eq_ignore_ascii_case("identity") {
        return None;
    }
    Some(value.to_string())
}

fn source_info_from(response: &Response, status: StatusCode) -> SourceInfo {
    let headers = response.headers();
    let text = |name: reqwest::header::HeaderName| {
        headers.get(name).and_then(|v| v.to_str().ok()).map(str::to_owned)
    };

    // A real 206 is proof; `Accept-Ranges` is only a claim, and origins lie.
    let accept_ranges = status == StatusCode::PARTIAL_CONTENT;

    // On a 206, `Content-Length` is the range's length; the resource total is
    // in `Content-Range`.
    let len = if status == StatusCode::PARTIAL_CONTENT {
        text(reqwest::header::CONTENT_RANGE).as_deref().and_then(total_from_content_range)
    } else {
        response.content_length()
    };

    SourceInfo {
        len,
        accept_ranges,
        etag: text(reqwest::header::ETAG),
        last_modified: text(reqwest::header::LAST_MODIFIED),
        content_type: text(reqwest::header::CONTENT_TYPE),
        final_url: response.url().to_string(),
        content_encoding: content_encoding(response),
        suggested_filename: text(reqwest::header::CONTENT_DISPOSITION)
            .as_deref()
            .and_then(filename_from_content_disposition),
    }
}

/// `bytes 0-0/12345` -> `12345`. An unknown total (`*`) yields `None`.
fn total_from_content_range(value: &str) -> Option<u64> {
    value.rsplit('/').next()?.trim().parse().ok()
}

/// Pull `filename` out of `Content-Disposition`, preferring the RFC 5987
/// `filename*=` form. Path separators are stripped: the header is
/// attacker-controlled and must not steer a write out of the download
/// directory.
fn filename_from_content_disposition(value: &str) -> Option<String> {
    let mut plain = None;
    let mut extended = None;

    for part in value.split(';').map(str::trim) {
        if let Some(rest) = part.strip_prefix("filename*=") {
            let encoded = rest.rsplit("''").next().unwrap_or(rest);
            extended = percent_decode(encoded);
        } else if let Some(rest) = part.strip_prefix("filename=") {
            plain = Some(rest.trim_matches('"').to_string());
        }
    }

    let name = extended.or(plain)?;
    sanitize_filename(&name)
}

fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Reduce an untrusted name to a single safe path component.
pub fn sanitize_filename(name: &str) -> Option<String> {
    let base = name.rsplit(['/', '\\']).next()?.trim();
    if base.is_empty() || base == "." || base == ".." {
        return None;
    }
    let cleaned: String =
        base.chars().filter(|c| !c.is_control() && *c != ':' && *c != '\0').collect();
    let cleaned = cleaned.trim().to_string();
    if cleaned.is_empty() { None } else { Some(cleaned) }
}

/// Best-effort filename for a URL, used when the origin suggests none.
pub fn filename_from_url(url: &str) -> Option<String> {
    let without_scheme = url.split("://").last().unwrap_or(url);
    let path = without_scheme.split(['?', '#']).next()?;
    let last = path.rsplit('/').next()?;
    let decoded = percent_decode(last).unwrap_or_else(|| last.to_string());
    sanitize_filename(&decoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_span_out_of_content_range() {
        assert_eq!(parse_content_range_span("bytes 100-199/1000"), Some((100, 199)));
        assert_eq!(parse_content_range_span("bytes 0-0/*"), Some((0, 0)));
        assert_eq!(parse_content_range_span("nonsense"), None);
    }

    #[test]
    fn only_strong_validators_gate_a_range_request() {
        assert!(is_strong_validator("\"abc\""));
        assert!(!is_strong_validator("W/\"abc\""));
        assert!(!is_strong_validator(""));
    }

    #[test]
    fn parses_the_total_length_out_of_content_range() {
        assert_eq!(total_from_content_range("bytes 0-0/12345"), Some(12345));
        assert_eq!(total_from_content_range("bytes 500-999/1000"), Some(1000));
        // An unknown total must stay unknown rather than become a wrong number.
        assert_eq!(total_from_content_range("bytes 0-0/*"), None);
        assert_eq!(total_from_content_range("nonsense"), None);
    }

    #[test]
    fn content_disposition_filenames() {
        assert_eq!(
            filename_from_content_disposition(r#"attachment; filename="report.pdf""#),
            Some("report.pdf".into())
        );
        // The RFC 5987 form wins when both are present.
        assert_eq!(
            filename_from_content_disposition(
                r#"attachment; filename="fallback.bin"; filename*=UTF-8''caf%C3%A9.txt"#
            ),
            Some("café.txt".into())
        );
    }

    #[test]
    fn a_traversing_filename_is_reduced_to_its_last_component() {
        // Content-Disposition is attacker-controlled; it must not be able to
        // steer a write outside the download directory.
        assert_eq!(
            filename_from_content_disposition(r#"attachment; filename="../../.bashrc""#),
            Some(".bashrc".into())
        );
        assert_eq!(
            filename_from_content_disposition(r#"attachment; filename="/etc/passwd""#),
            Some("passwd".into())
        );
        assert_eq!(sanitize_filename("..").as_deref(), None);
        assert_eq!(sanitize_filename("").as_deref(), None);
        assert_eq!(sanitize_filename("a/b/../c").as_deref(), Some("c"));
    }

    #[test]
    fn filenames_from_urls() {
        assert_eq!(filename_from_url("http://x.test/a/1GB.bin"), Some("1GB.bin".into()));
        assert_eq!(
            filename_from_url("https://x.test/p/file.zip?token=abc#frag"),
            Some("file.zip".into())
        );
        assert_eq!(filename_from_url("https://x.test/caf%C3%A9.txt"), Some("café.txt".into()));
        // A bare host has no filename to offer.
        assert_eq!(filename_from_url("https://x.test/"), None);
    }
}

/// How long the origin asked us to wait, from `Retry-After`.
///
/// Two forms are legal (RFC 9110 §10.2.3) and both appear in the wild: a count
/// of seconds, and an HTTP-date. A CDN under load sends the date form often
/// enough that ignoring it means ignoring half the rate limits we are told
/// about.
///
/// Clamped to a day: a header saying "come back next year" is not something to
/// hold a download open for, and the caller's own cap is a better answer.
pub fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    const MAX: Duration = Duration::from_secs(24 * 60 * 60);
    let raw = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?.trim();

    if let Ok(seconds) = raw.parse::<u64>() {
        return Some(Duration::from_secs(seconds).min(MAX));
    }

    // An absolute date: the wait is however far in the future it is, which may
    // be no time at all if it has already passed or our clock disagrees.
    let target = http_date_to_unix(raw)?;
    let now =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as i64;
    Some(Duration::from_secs(target.saturating_sub(now).max(0) as u64).min(MAX))
}

/// Seconds since the epoch for an IMF-fixdate: `Sun, 06 Nov 1994 08:49:37 GMT`.
///
/// Hand-rolled rather than pulled in as a dependency: this is the only date
/// this project ever parses, the format is fixed-width by specification, and
/// the obsolete RFC 850 and asctime forms are not worth carrying a crate for.
/// Anything that is not an IMF-fixdate returns `None` and the caller backs off
/// on its own schedule.
fn http_date_to_unix(text: &str) -> Option<i64> {
    const MONTHS: [&str; 12] =
        ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    // "Sun, 06 Nov 1994 08:49:37 GMT"
    let rest = text.split_once(", ")?.1;
    let mut parts = rest.split(' ');
    let day: i64 = parts.next()?.parse().ok()?;
    let name = parts.next()?;
    let month = MONTHS.iter().position(|m| *m == name)? as i64 + 1;
    let year: i64 = parts.next()?.parse().ok()?;
    let mut clock = parts.next()?.split(':');
    let hour: i64 = clock.next()?.parse().ok()?;
    let minute: i64 = clock.next()?.parse().ok()?;
    let second: i64 = clock.next()?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 {
        return None;
    }

    // Days since the epoch, by the civil-from-days algorithm: no leap-year
    // table and correct across century boundaries.
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;

    Some(days * 86_400 + hour * 3_600 + minute * 60 + second)
}

#[cfg(test)]
mod retry_after_tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};

    fn headers(value: &str) -> HeaderMap {
        let mut map = HeaderMap::new();
        map.insert(RETRY_AFTER, HeaderValue::from_str(value).unwrap());
        map
    }

    #[test]
    fn the_seconds_form_is_read_directly() {
        assert_eq!(retry_after(&headers("120")), Some(Duration::from_secs(120)));
        assert_eq!(retry_after(&headers("  30  ")), Some(Duration::from_secs(30)));
        assert_eq!(retry_after(&headers("0")), Some(Duration::ZERO));
    }

    #[test]
    fn a_missing_or_unparseable_header_defers_to_the_caller() {
        assert_eq!(retry_after(&HeaderMap::new()), None);
        assert_eq!(retry_after(&headers("soon")), None);
        assert_eq!(retry_after(&headers("-5")), None);
    }

    #[test]
    fn the_epoch_is_the_reference_point_for_the_date_form() {
        assert_eq!(http_date_to_unix("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        // The example date from the HTTP specification itself.
        assert_eq!(http_date_to_unix("Sun, 06 Nov 1994 08:49:37 GMT"), Some(784_111_777));
        // A leap day, and a century that is not a leap year.
        assert_eq!(http_date_to_unix("Sat, 29 Feb 2020 00:00:00 GMT"), Some(1_582_934_400));
        assert_eq!(http_date_to_unix("Wed, 01 Mar 1900 00:00:00 GMT"), Some(-2_203_891_200));
    }

    #[test]
    fn a_date_in_the_past_means_no_wait_rather_than_a_negative_one() {
        // Clocks disagree, and a limit that has already expired is not a
        // reason to refuse to try.
        assert_eq!(retry_after(&headers("Thu, 01 Jan 1970 00:00:00 GMT")), Some(Duration::ZERO));
    }

    #[test]
    fn an_absurd_wait_is_clamped_rather_than_honoured() {
        assert_eq!(retry_after(&headers("999999999")), Some(Duration::from_secs(86_400)));
        assert_eq!(
            retry_after(&headers("Fri, 01 Jan 2100 00:00:00 GMT")),
            Some(Duration::from_secs(86_400))
        );
    }

    #[test]
    fn a_malformed_date_is_refused_rather_than_guessed_at() {
        assert_eq!(http_date_to_unix("Sun, 06 Xxx 1994 08:49:37 GMT"), None);
        assert_eq!(http_date_to_unix("06 Nov 1994 08:49:37 GMT"), None);
        assert_eq!(http_date_to_unix("Sun, 06 Nov 1994 25:49:37 GMT"), None);
        assert_eq!(http_date_to_unix(""), None);
    }
}
