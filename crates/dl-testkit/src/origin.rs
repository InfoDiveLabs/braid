//! A mock HTTP origin that misbehaves on purpose.
//!
//! Hand-written handlers rather than a stubbing library: the interesting cases
//! are stateful mid-stream misbehaviour, which a request/response matcher
//! cannot express. Bodies stream from `fixtures`, so large scenarios cost no
//! memory or disk.

use crate::fixtures;
use crate::scenario::Scenario;
use axum::body::Body;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use dl_core::refresh::ResolvedSource;
use futures_util::stream;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const CHUNK: usize = 64 * 1024;

/// The header a signed link's token travels in.
///
/// Signed URLs in the wild pair a query signature with headers the issuer
/// hands back: a `Cookie`, a `Referer`, an `Authorization`. Requiring one
/// here means a refresher that resolves the right URL and drops its headers
/// fails loudly instead of appearing to work.
pub const TOKEN_HEADER: &str = "x-dl-token";

/// Stands in for the client address a signer binds a signature to.
///
/// Every loopback connection has the same source address, so real per-address
/// binding cannot be reproduced on one machine. Origins behind a CDN read the
/// client address out of this header, which makes it a faithful stand-in: the
/// lane's identity travels with its requests and differs per lane.
pub const CLIENT_HEADER: &str = "x-forwarded-for";

/// One issued link.
#[derive(Debug)]
struct Issued {
    at: Instant,
    bytes: u64,
    /// The address this signature was issued to, where the scenario binds one.
    owner: Option<String>,
}

#[derive(Debug, Default)]
struct Tokens {
    next: u64,
    live: HashMap<u64, Issued>,
}

#[derive(Clone)]
struct Shared {
    scenario: Scenario,
    addr: SocketAddr,
    requests: Arc<AtomicU64>,
    /// Requests refused because the link was not usable: the number a
    /// proactive refresh has to drive to zero.
    rejections: Arc<AtomicU64>,
    issues: Arc<AtomicU64>,
    tokens: Arc<Mutex<Tokens>>,
}

/// A running mock origin. Shuts down when dropped.
pub struct Origin {
    addr: SocketAddr,
    scenario: Scenario,
    shared: Shared,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl Origin {
    /// Start the origin on an ephemeral loopback port.
    pub async fn spawn(scenario: Scenario) -> anyhow::Result<Self> {
        // Bound before the router is built: issued links have to carry an
        // absolute URL, so the port has to be known first.
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
        let addr = listener.local_addr()?;

        let shared = Shared {
            scenario,
            addr,
            requests: Arc::new(AtomicU64::new(0)),
            rejections: Arc::new(AtomicU64::new(0)),
            issues: Arc::new(AtomicU64::new(0)),
            tokens: Arc::new(Mutex::new(Tokens::default())),
        };

        let app = axum::Router::new()
            .route("/file/{name}", get(serve))
            .route("/hop/{n}/{name}", get(hop))
            .route("/issue/{name}", get(issue))
            .with_state(shared.clone());

        let (tx, rx) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(async move {
            let server = axum::serve(listener, app).with_graceful_shutdown(async {
                rx.await.ok();
            });
            if let Err(e) = server.await {
                tracing::debug!(error = %e, "mock origin stopped");
            }
        });

        Ok(Self { addr, scenario, shared, shutdown: Some(tx), handle: Some(handle) })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn scenario(&self) -> Scenario {
        self.scenario
    }

    /// URL for a download named `name`. The name only affects the derived
    /// filename; the body depends on the scenario.
    ///
    /// For a scenario that signs its links this is the *unsigned* URL, which
    /// is what a user pastes in after the signature they were given expired.
    pub fn url(&self, name: &str) -> String {
        match self.scenario {
            Scenario::RedirectChain { hops, .. } => {
                format!("http://{}/hop/{hops}/{name}", self.addr)
            }
            _ => format!("http://{}/file/{name}", self.addr),
        }
    }

    /// Where a refresher goes to have a link re-issued.
    pub fn issue_url(&self, name: &str) -> String {
        format!("http://{}/issue/{name}", self.addr)
    }

    /// Mint a signed link directly, for a download that starts with one still
    /// valid. `owner` is the address the signature is bound to.
    pub fn sign(&self, name: &str, owner: Option<&str>) -> ResolvedSource {
        let (url, headers, _) = self.shared.mint(name, owner.map(str::to_string));
        ResolvedSource::new(url).with_headers(headers)
    }

    /// Requests the origin refused because the link was not usable.
    pub fn rejections(&self) -> u64 {
        self.shared.rejections.load(Ordering::SeqCst)
    }

    /// Links issued. One per genuine refresh, so a stampede is visible here.
    pub fn issues(&self) -> u64 {
        self.shared.issues.load(Ordering::SeqCst)
    }

    pub fn requests(&self) -> u64 {
        self.shared.requests.load(Ordering::SeqCst)
    }
}

impl Drop for Origin {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

impl Shared {
    /// Issue a link, returning its URL, the headers that must accompany it,
    /// and its lifetime in seconds.
    fn mint(
        &self,
        name: &str,
        owner: Option<String>,
    ) -> (String, Vec<(String, String)>, Option<u64>) {
        let mut tokens = self.tokens.lock().unwrap();
        tokens.next += 1;
        let token = tokens.next;
        tokens.live.insert(token, Issued { at: Instant::now(), bytes: 0, owner });
        drop(tokens);

        self.issues.fetch_add(1, Ordering::SeqCst);
        let lifetime = self.scenario.link_lifetime().map(|l| l.as_secs());
        let mut url = format!("http://{}/file/{name}?token={token}", self.addr);
        if let Some(seconds) = lifetime {
            // The lifetime lives in the query string, exactly where a real
            // pre-signed URL puts it.
            url.push_str(&format!("&X-Amz-Expires={seconds}"));
        }
        (url, vec![(TOKEN_HEADER.to_string(), token.to_string())], lifetime)
    }

    /// Why this request may not have the file, or `None` if it may.
    fn refuse(&self, token: Option<u64>, headers: &HeaderMap) -> Option<Response> {
        let scenario = self.scenario;
        let Some(token) = token else {
            return Some(self.reject(unsigned_response(scenario)));
        };

        // A refresher that resolved the right URL and dropped the headers the
        // issuer gave it gets caught here rather than halfway down the file.
        if header(headers, TOKEN_HEADER) != Some(token.to_string()) {
            return Some(
                self.reject(forbidden("the signature header is missing or does not match")),
            );
        }

        let tokens = self.tokens.lock().unwrap();
        let Some(issued) = tokens.live.get(&token) else {
            return Some(self.reject(forbidden("unknown signature")));
        };

        match scenario {
            Scenario::ExpiresAfterSeconds { lifetime_secs, .. }
                if issued.at.elapsed() > Duration::from_secs(lifetime_secs) =>
            {
                Some(self.reject(forbidden("the signature has expired")))
            }
            Scenario::ExpiresAfterBytes { after_bytes, .. } if issued.bytes >= after_bytes => {
                Some(self.reject(forbidden("the signature is spent")))
            }
            Scenario::HtmlErrorBodyWith200 { after_bytes, .. } if issued.bytes >= after_bytes => {
                Some(self.reject(login_page()))
            }
            Scenario::SignedPerSourceIp { .. }
                if issued.owner != header(headers, CLIENT_HEADER) =>
            {
                Some(self.reject(forbidden("this signature was issued to another address")))
            }
            _ => None,
        }
    }

    fn reject(&self, response: Response) -> Response {
        self.rejections.fetch_add(1, Ordering::SeqCst);
        response
    }

    fn record(&self, token: Option<u64>, bytes: u64) {
        let Some(token) = token else { return };
        if let Some(issued) = self.tokens.lock().unwrap().live.get_mut(&token) {
            issued.bytes += bytes;
        }
    }
}

/// What an unsigned request gets. Each scenario expresses its expiry through a
/// different status, and the client has to recognise all of them.
fn unsigned_response(scenario: Scenario) -> Response {
    match scenario {
        Scenario::Returns410Gone { .. } => (
            StatusCode::GONE,
            [(header::CONTENT_TYPE, "text/plain")],
            "this object has been withdrawn",
        )
            .into_response(),
        Scenario::HtmlErrorBodyWith200 { .. } => login_page(),
        _ => forbidden("this request is not signed"),
    }
}

/// The refusal a rate-limiting scenario owes this request, if any.
///
/// Counted by request rather than by wall clock so the test is deterministic:
/// "the first N requests are refused" is checkable, "requests in the first
/// second" is a race.
fn rate_limit(scenario: Scenario, seen: u64) -> Option<Response> {
    match scenario {
        Scenario::RateLimited429 { refusals, retry_after_secs, .. } if seen < refusals => {
            let mut response = (
                StatusCode::TOO_MANY_REQUESTS,
                [(header::CONTENT_TYPE, "text/plain")],
                "slow down",
            )
                .into_response();
            if let Some(secs) = retry_after_secs {
                response.headers_mut().insert(
                    header::RETRY_AFTER,
                    header::HeaderValue::from_str(&secs.to_string()).expect("digits are a header"),
                );
            }
            Some(response)
        }
        Scenario::Unavailable503 { refusals, .. } if seen < refusals => Some(
            (StatusCode::SERVICE_UNAVAILABLE, [(header::CONTENT_TYPE, "text/plain")], "try later")
                .into_response(),
        ),
        _ => None,
    }
}

fn forbidden(reason: &'static str) -> Response {
    (StatusCode::FORBIDDEN, [(header::CONTENT_TYPE, "text/plain")], reason).into_response()
}

/// A portal's sign-in page, served with `200` and an honest `Content-Length`.
/// Nothing but the media type says this is not the file.
fn login_page() -> Response {
    const PAGE: &str = "<html><body><h1>Sign in to continue</h1></body></html>";
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8".to_string()),
            (header::CONTENT_LENGTH, PAGE.len().to_string()),
        ],
        PAGE,
    )
        .into_response()
}

fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers.get(name).and_then(|v| v.to_str().ok()).map(str::to_owned)
}

/// Hand out a fresh signed link. A refresher's endpoint.
async fn issue(
    State(state): State<Shared>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    let owner = header(&headers, CLIENT_HEADER);
    let (url, issued_headers, lifetime) = state.mint(&name, owner);

    let headers_json = issued_headers
        .iter()
        .map(|(k, v)| format!("\"{k}\":\"{v}\""))
        .collect::<Vec<_>>()
        .join(",");
    let expires = match lifetime {
        Some(seconds) => format!(",\"expires_in\":{seconds}"),
        None => String::new(),
    };

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        format!("{{\"url\":\"{url}\",\"headers\":{{{headers_json}}}{expires}}}"),
    )
        .into_response()
}

/// Walks the redirect chain down to `/file/{name}`.
async fn hop(Path((n, name)): Path<(u8, String)>) -> Response {
    let location =
        if n <= 1 { format!("/file/{name}") } else { format!("/hop/{}/{}", n - 1, name) };
    (StatusCode::FOUND, [(header::LOCATION, location)]).into_response()
}

/// The length this scenario claims, which is not always the length it delivers.
fn declared_size(scenario: Scenario) -> Option<u64> {
    match scenario {
        Scenario::NotFound => None,
        Scenario::TruncatedBody { declared, .. } => Some(declared),
        other => other.size(),
    }
}

/// Parse `Range: bytes=<start>-<end>` into an inclusive pair, clamped to
/// `size`. Single-range form only, which is all a downloader sends.
fn parse_range(headers: &HeaderMap, size: u64) -> Option<(u64, u64)> {
    let raw = headers.get(header::RANGE)?.to_str().ok()?;
    let spec = raw.trim().strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None;
    }
    let (start, end) = spec.split_once('-')?;

    // A suffix range, `bytes=-500`, means the last 500 bytes.
    if start.is_empty() {
        let last: u64 = end.trim().parse().ok()?;
        let last = last.min(size);
        return Some((size.saturating_sub(last), size.saturating_sub(1)));
    }

    let start: u64 = start.trim().parse().ok()?;
    let end = match end.trim() {
        "" => size.saturating_sub(1),
        v => v.parse::<u64>().ok()?.min(size.saturating_sub(1)),
    };
    if start > end { None } else { Some((start, end)) }
}

/// Pull a query parameter out of a raw query string.
fn query_value(query: Option<&str>, key: &str) -> Option<String> {
    query?
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v.to_string())
}

async fn serve(
    State(state): State<Shared>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let scenario = state.scenario;
    let seen = state.requests.fetch_add(1, Ordering::SeqCst);
    if scenario == Scenario::NotFound {
        return (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            "<html><body><h1>404 Not Found</h1></body></html>",
        )
            .into_response();
    }

    // Signature checks come before the range branch on purpose. A scenario
    // that only misbehaved on the unranged path would let the chunked client
    // sail straight through it.
    let token = query_value(query.as_deref(), "token").and_then(|v| v.parse::<u64>().ok());
    if scenario.requires_refresh()
        && let Some(refusal) = state.refuse(token, &headers)
    {
        return refusal;
    }

    // Rate limits come before everything else a scenario does. An origin that
    // only refused the unranged path would let the chunked client straight
    // through, and the chunked client is the one that can hammer a limit with
    // eight connections at once.
    if let Some(refusal) = rate_limit(scenario, seen) {
        return refusal;
    }

    let size = declared_size(scenario).unwrap_or(0);
    let seed = scenario.payload_seed();

    // Ranges are honoured before the scenario's misbehaviour applies: the
    // client probes with `bytes=0-0`, and an origin that ignored Range would
    // read as range-incapable in every scenario.
    let etag = etag_for(scenario, seen);

    if let Some((start, end)) = parse_range(&headers, size) {
        // Per RFC 9110: when the If-Range validator no longer matches, the
        // origin ignores Range and sends the whole representation with 200.
        // That 200 is how a client learns the resource changed underneath it.
        let validator_stale = headers
            .get(header::IF_RANGE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.trim() != etag);

        if validator_stale || matches!(scenario, Scenario::AcceptRangesLies { .. }) {
            state.record(token, size);
            return body_response_with_etag(size, generator(seed, size, size, None), &etag);
        }

        let (mut start, mut end) = (start, end);
        if matches!(scenario, Scenario::ContentRangeMismatch { .. }) {
            // Answer with a 206 describing a span other than the one requested.
            let shift = 4096.min(size.saturating_sub(end + 1));
            start += shift;
            end += shift;
        }

        let len = end - start + 1;
        // Correct length, correct headers, wrong bytes: only a content check
        // can tell.
        let payload_offset =
            if matches!(scenario, Scenario::WrongBytesForRange { .. }) { 0 } else { start };
        let mut body = fixtures::range(seed, payload_offset, len as usize);

        if let Scenario::CorruptBytesAt { offset, len: bad, .. } = scenario {
            corrupt_overlap(&mut body, start, offset, bad);
        }

        state.record(token, len);

        // A connection dropped part-way through a chunk, which is what a
        // retryable failure looks like once downloads are chunked.
        let body = match scenario {
            Scenario::ResetAtOffset { reset_at, .. }
                if (start..start + len).contains(&reset_at) =>
            {
                truncated_stream(body, (reset_at - start) as usize)
            }
            // The declared length is a lie on every path, not only on a whole
            // body request: the origin simply does not have these bytes.
            Scenario::TruncatedBody { actual, .. } if start + len > actual => {
                truncated_stream(body, actual.saturating_sub(start) as usize)
            }
            // Throttling has to apply to ranged responses too, or a chunked
            // client races through a scenario meant to be slow.
            Scenario::SlowStream { bytes_per_sec, .. } => paced_stream(body, bytes_per_sec),
            _ => Body::from(body),
        };

        let mut response = (
            StatusCode::PARTIAL_CONTENT,
            [
                (header::CONTENT_TYPE, "application/octet-stream".to_string()),
                (header::CONTENT_LENGTH, len.to_string()),
                (header::CONTENT_RANGE, format!("bytes {start}-{end}/{size}")),
                (header::ACCEPT_RANGES, "bytes".to_string()),
                (header::ETAG, etag.clone()),
            ],
            body,
        )
            .into_response();

        if matches!(scenario, Scenario::ClaimsGzip { .. }) {
            response
                .headers_mut()
                .insert(header::CONTENT_ENCODING, header::HeaderValue::from_static("gzip"));
        }
        return response;
    }

    state.record(token, size);

    match scenario {
        Scenario::NotFound => unreachable!("handled above"),

        Scenario::TruncatedBody { declared, actual } => {
            body_response(declared, generator(seed, actual, actual, None))
        }

        Scenario::SlowStream { size, bytes_per_sec } => {
            body_response(size, generator(seed, size, size, Some(bytes_per_sec)))
        }

        // Erroring the stream aborts the response mid-body.
        Scenario::ResetAtOffset { size, reset_at } => {
            body_response(size, generator(seed, size, reset_at, None))
        }

        // Body is plain, not gzip: a correct client refuses on the header.
        Scenario::ClaimsGzip { size } => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "application/octet-stream".to_string()),
                (header::CONTENT_LENGTH, size.to_string()),
                (header::CONTENT_ENCODING, "gzip".to_string()),
            ],
            generator(seed, size, size, None),
        )
            .into_response(),

        _ => body_response_with_etag(size, generator(seed, size, size, None), &etag),
    }
}

/// Dribble a body out at roughly `bytes_per_sec`.
fn paced_stream(body: Vec<u8>, bytes_per_sec: u64) -> Body {
    let buffer = bytes::Bytes::from(body);
    Body::from_stream(stream::unfold(0usize, move |sent| {
        let buffer = buffer.clone();
        async move {
            if sent >= buffer.len() {
                return None;
            }
            let n = CHUNK.min(buffer.len() - sent);
            if let Some(micros) = (n as u64 * 1_000_000).checked_div(bytes_per_sec) {
                tokio::time::sleep(std::time::Duration::from_micros(micros)).await;
            }
            Some((Ok::<_, std::io::Error>(buffer.slice(sent..sent + n)), sent + n))
        }
    }))
}

/// Send the first `keep` bytes, then fail, so the response is aborted mid-body
/// rather than terminated cleanly.
fn truncated_stream(body: Vec<u8>, keep: usize) -> Body {
    let keep = keep.min(body.len());
    let prefix = bytes::Bytes::from(body).slice(..keep);
    Body::from_stream(stream::unfold(Some(prefix), |state| async move {
        match state {
            Some(chunk) => Some((Ok(chunk), None)),
            None => Some((
                Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "mock origin dropped the connection",
                )),
                None,
            )),
        }
    }))
}

/// The ETag a scenario reports for its `seen`-th request.
///
/// A disagreeing mirror has its own, which is the only difference a client can
/// detect: it serves a different file of exactly the same length.
fn etag_for(scenario: Scenario, seen: u64) -> String {
    match scenario {
        Scenario::EtagChangesMidDownload { after_requests, .. } if seen >= after_requests => {
            "\"v2\"".to_string()
        }
        Scenario::MirrorDisagrees { .. } => "\"other\"".to_string(),
        _ => "\"v1\"".to_string(),
    }
}

/// Flip bytes where `[bad_offset, bad_offset + bad_len)` overlaps the span
/// starting at `start`.
fn corrupt_overlap(body: &mut [u8], start: u64, bad_offset: u64, bad_len: u64) {
    let body_end = start + body.len() as u64;
    let from = bad_offset.max(start);
    let to = (bad_offset + bad_len).min(body_end);
    if from >= to {
        return;
    }
    for i in from..to {
        body[(i - start) as usize] ^= 0xFF;
    }
}

fn body_response(content_length: u64, body: Body) -> Response {
    body_response_with_etag(content_length, body, "\"v1\"")
}

fn body_response_with_etag(content_length: u64, body: Body, etag: &str) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (header::CONTENT_LENGTH, content_length.to_string()),
            (header::ACCEPT_RANGES, "bytes".to_string()),
            (header::ETAG, etag.to_string()),
        ],
        body,
    )
        .into_response()
}

/// Stream `deliver` bytes. If `deliver < total` the stream ends with an error
/// so the client sees a broken body rather than a clean end.
fn generator(seed: u64, total: u64, deliver: u64, bytes_per_sec: Option<u64>) -> Body {
    let state = (0u64, deliver, total, bytes_per_sec);
    Body::from_stream(stream::unfold(state, move |(sent, deliver, total, rate)| async move {
        if sent >= deliver {
            if deliver < total {
                let err = std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "mock origin dropped the connection",
                );
                return Some((Err(err), (sent, deliver, total, rate)));
            }
            return None;
        }

        let n = CHUNK.min((deliver - sent) as usize);
        if let Some(rate) = rate.filter(|r| *r > 0) {
            let micros = (n as u64 * 1_000_000) / rate;
            tokio::time::sleep(std::time::Duration::from_micros(micros)).await;
        }

        let chunk = fixtures::range(seed, sent, n);
        Some((Ok(bytes::Bytes::from(chunk)), (sent + n as u64, deliver, total, rate)))
    }))
}
