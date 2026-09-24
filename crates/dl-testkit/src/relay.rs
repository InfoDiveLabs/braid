//! A forward proxy, standing in for a phone running the companion app.
//!
//! The real relay is an Android device that binds its sockets to the cellular
//! network and forwards what it fetches. From this side of the wire the only
//! thing that matters is that it speaks an ordinary HTTP forward proxy, which
//! is the whole point of the design: the relay is transparent, so every
//! guarantee the engine already has: `Accept-Encoding: identity`, the 206
//! check, per-chunk hashing: keeps working through it unchanged.
//!
//! Speaks both forms a forward proxy has to: the absolute-URI form for plain
//! HTTP, and `CONNECT` for anything tunnelled. `CONNECT` is the one that
//! matters in the field: real downloads are HTTPS, and a relay that only did
//! the first would work against every test here and nothing at all in
//! practice. It is exercised below, and it is the contract the Android
//! companion has to meet.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

/// A running proxy. Dropping it stops accepting.
pub struct Relay {
    addr: SocketAddr,
    forwarded: Arc<AtomicU64>,
    /// How many requests were answered with a 407.
    ///
    /// Lets a test ask the question that matters from the desktop's side: did
    /// our client actually present its credentials, on this shape of request?
    challenged: Arc<AtomicU64>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Relay {
    /// Start one on an ephemeral loopback port.
    pub async fn spawn() -> std::io::Result<Self> {
        Self::start(None, None).await
    }

    /// How many requests this relay refused for want of a key.
    pub fn challenged(&self) -> u64 {
        self.challenged.load(Ordering::SeqCst)
    }

    /// The address it listens on, for a test that speaks to it directly.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Start one that also answers the control protocol, standing in for a
    /// phone running the companion.
    ///
    /// This is how discovery, pairing and multi-lane relays are tested without
    /// a phone, the same way a fake interface provider tests multi-NIC code on
    /// a machine with one card.
    pub async fn spawn_phone(name: &str, lanes: Vec<Lane>) -> std::io::Result<Self> {
        Self::start_on("127.0.0.1:0", None, Some(Identity::new(name, lanes, None))).await
    }

    /// The same, listening on IPv6 loopback.
    ///
    /// An IPv6-only network is not exotic: a phone on an IPv6-only carrier,
    /// sharing over a hotspot, gives a link with no IPv4 on it at all. Every
    /// address the desktop then handles is a v6 literal, which has to be
    /// bracketed to be a usable URL authority.
    pub async fn spawn_phone_on_ipv6(name: &str, lanes: Vec<Lane>) -> std::io::Result<Self> {
        Self::start_on("[::1]:0", None, Some(Identity::new(name, lanes, None))).await
    }

    /// A phone that has already been paired and will serve only that key.
    pub async fn spawn_paired_phone(
        name: &str,
        lanes: Vec<Lane>,
        key: &str,
    ) -> std::io::Result<Self> {
        Self::start(None, Some(Identity::new(name, lanes, Some(key.to_string())))).await
    }

    /// Start one that refuses to forward more than `cap` requests.
    ///
    /// A relay that stops answering is the normal case, not an exotic one: a
    /// phone goes out of signal, sleeps, or overheats. The engine has to park
    /// that lane and carry on rather than failing the transfer.
    pub async fn spawn_with_limit(cap: Option<u64>) -> std::io::Result<Self> {
        Self::start(cap, None).await
    }

    async fn start(cap: Option<u64>, identity: Option<Identity>) -> std::io::Result<Self> {
        Self::start_on("127.0.0.1:0", cap, identity).await
    }

    async fn start_on(
        bind: &str,
        cap: Option<u64>,
        identity: Option<Identity>,
    ) -> std::io::Result<Self> {
        let listener = TcpListener::bind(bind).await?;
        let addr = listener.local_addr()?;
        let forwarded = Arc::new(AtomicU64::new(0));
        let challenged = Arc::new(AtomicU64::new(0));
        let (tx, mut rx) = tokio::sync::oneshot::channel();

        let counter = Arc::clone(&forwarded);
        let challenges = Arc::clone(&challenged);
        tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    _ = &mut rx => return,
                    accepted = listener.accept() => accepted,
                };
                let Ok((client, _)) = accepted else { return };
                let counter = Arc::clone(&counter);
                let identity = identity.clone();
                let challenge = Challenge(Arc::clone(&challenges));
                tokio::spawn(async move {
                    let seen = counter.fetch_add(1, Ordering::SeqCst);
                    if cap.is_some_and(|cap| seen >= cap) {
                        return;
                    }
                    if let Err(error) = forward(client, identity, challenge).await {
                        tracing::debug!(%error, "relay connection ended");
                    }
                });
            }
        });

        Ok(Self { addr, forwarded, challenged, shutdown: Some(tx) })
    }

    /// As a proxy URL, which is what `HttpConfig` wants.
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Requests this relay has taken, so a test can assert work was actually
    /// spread rather than all landing on one lane.
    pub fn forwarded(&self) -> u64 {
        self.forwarded.load(Ordering::SeqCst)
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

/// What a stand-in phone answers with.
///
/// Its JSON is written by hand rather than with the types in `dl-net`, for two
/// reasons. It keeps this crate free of the network stack, and it means the
/// desktop's parser is tested against an encoder that is not its own: the
/// phone is a separate program in another language, and a field name that only
/// agrees with itself would pass every test and fail on the first real device.
#[derive(Clone)]
struct Challenge(Arc<AtomicU64>);

#[derive(Clone)]
struct Identity {
    name: String,
    device_id: String,
    lanes: Arc<Vec<Lane>>,
    /// When set, proxy traffic must present it. A phone that has never been
    /// paired serves nobody, which is the safe default for something that
    /// spends money.
    key: Option<String>,
}

impl Identity {
    fn new(name: &str, lanes: Vec<Lane>, key: Option<String>) -> Self {
        Self {
            name: name.to_string(),
            // Stable for the life of this relay, which is all a test needs.
            device_id: format!("testkit-{name}"),
            lanes: Arc::new(lanes),
            key,
        }
    }
}

/// One lane a stand-in phone claims to offer.
#[derive(Clone, Debug)]
pub struct Lane {
    pub id: String,
    /// "cellular" | "wifi" | "ethernet", or anything, to prove an unknown
    /// transport does not break the desktop.
    pub kind: String,
    pub label: String,
    pub egress: Option<String>,
    pub note: Option<String>,
}

impl Lane {
    pub fn new(id: &str, kind: &str, label: &str) -> Self {
        Self { id: id.into(), kind: kind.into(), label: label.into(), egress: None, note: None }
    }

    /// The public address this lane claims to leave from, which is how the
    /// desktop notices a lane that is the route it already has.
    pub fn leaving_from(mut self, egress: &str) -> Self {
        self.egress = Some(egress.into());
        self
    }

    pub fn saying(mut self, note: &str) -> Self {
        self.note = Some(note.into());
        self
    }

    fn to_json(&self) -> String {
        let mut out = format!(
            "{{\"id\":{},\"kind\":{},\"label\":{}",
            quote(&self.id),
            quote(&self.kind),
            quote(&self.label)
        );
        if let Some(egress) = &self.egress {
            out.push_str(&format!(",\"egress\":{}", quote(egress)));
        }
        if let Some(note) = &self.note {
            out.push_str(&format!(",\"note\":{}", quote(note)));
        }
        out.push('}');
        out
    }
}

/// A JSON string literal. Enough of the escaping rules for a device name,
/// which is the field a person gets to choose.
fn quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Read the request line and headers, open the upstream, and splice.
///
/// A forward proxy receives the absolute URI on the request line: /// `GET http://host/path HTTP/1.1`: and sends the origin-form on. Everything
/// else, headers and body alike, is copied through untouched: rewriting any of
/// it here would hide a bug in what the engine actually sent.
async fn forward(
    client: TcpStream,
    identity: Option<Identity>,
    challenge: Challenge,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(client);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).await?;

    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();
    let version = parts.next().unwrap_or("HTTP/1.1").to_string();

    // An origin-form target is not proxy traffic. A real relay answers its
    // control plane on these paths, so a stand-in phone has to as well.
    if target.starts_with('/') {
        return match identity {
            Some(identity) => control(reader, &target, &identity).await,
            None => respond(reader.into_inner(), 404, "not a relay").await,
        };
    }

    // Read the headers before branching, because both kinds of proxy request
    // have to be authorised and a `CONNECT` carries its credentials here too.
    let mut headers = Vec::new();
    let mut proxy_headers = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 {
            break;
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        // Hop-by-hop, and meaningless to the origin.
        if line.to_ascii_lowercase().starts_with("proxy-") {
            proxy_headers.push(line);
        } else {
            headers.push(line);
        }
    }

    // Proxy traffic from a desktop that has not paired is refused, whichever
    // shape it takes. Checking only the absolute-URI form would mean every
    // test of "an unpaired desktop is refused" passed over a path that real
    // traffic never takes, since a download from an HTTPS origin is a tunnel.
    if let Some(expected) = identity.as_ref().and_then(|i| i.key.as_deref())
        && !presents(&proxy_headers, expected)
    {
        challenge.0.fetch_add(1, Ordering::SeqCst);
        return respond(reader.into_inner(), 407, "pair with this phone first").await;
    }

    // `CONNECT host:port` asks for a raw tunnel, which is how every HTTPS
    // request through a proxy begins. Nothing inside it is ours to read: TLS
    // starts immediately after the 200 and the proxy is a pipe from there on.
    if method.eq_ignore_ascii_case("CONNECT") {
        return tunnel(reader, &target).await;
    }

    let Some((host, path)) = split_absolute(&target) else {
        let _ = reader.into_inner().write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n").await;
        return Ok(());
    };

    let mut upstream = TcpStream::connect(&host).await?;
    let mut head = format!("{method} {path} {version}\r\n");
    for header in headers {
        head.push_str(&header);
    }
    head.push_str("\r\n");
    upstream.write_all(head.as_bytes()).await?;
    upstream.flush().await?;

    // Whatever the client had buffered past the headers, then the two halves
    // in both directions until either side is done.
    let buffered = reader.buffer().to_vec();
    if !buffered.is_empty() {
        upstream.write_all(&buffered).await?;
    }
    let mut client = reader.into_inner();
    let (mut client_read, mut client_write) = client.split();
    let (mut up_read, mut up_write) = upstream.split();

    let to_origin = async {
        let _ = tokio::io::copy(&mut client_read, &mut up_write).await;
        let _ = up_write.shutdown().await;
    };
    let to_client = async {
        let _ = tokio::io::copy(&mut up_read, &mut client_write).await;
        let _ = client_write.shutdown().await;
    };
    tokio::join!(to_origin, to_client);
    Ok(())
}

/// Open a raw tunnel to `target` and splice both directions.
///
/// The reply must be sent before anything is forwarded, and the headers the
/// client sent with the `CONNECT` are consumed and discarded: they belong to
/// the proxy hop, not to whatever is tunnelled.
async fn tunnel(reader: BufReader<TcpStream>, target: &str) -> std::io::Result<()> {
    let upstream = match TcpStream::connect(target).await {
        Ok(upstream) => upstream,
        Err(error) => {
            let _ = reader.into_inner().write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await;
            return Err(error);
        }
    };

    let mut client = reader.into_inner();
    client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await?;
    client.flush().await?;

    let (mut client_read, mut client_write) = client.into_split();
    let (mut up_read, mut up_write) = upstream.into_split();
    let up = async {
        let _ = tokio::io::copy(&mut client_read, &mut up_write).await;
        let _ = up_write.shutdown().await;
    };
    let down = async {
        let _ = tokio::io::copy(&mut up_read, &mut client_write).await;
        let _ = client_write.shutdown().await;
    };
    tokio::join!(up, down);
    Ok(())
}

/// `http://host:port/path` → `("host:port", "/path")`.
fn split_absolute(target: &str) -> Option<(String, String)> {
    let rest = target.strip_prefix("http://")?;
    match rest.find('/') {
        Some(at) => Some((rest[..at].to_string(), rest[at..].to_string())),
        None => Some((rest.to_string(), "/".to_string())),
    }
}

/// The control plane, as far as a test needs one.
async fn control(
    reader: BufReader<TcpStream>,
    target: &str,
    identity: &Identity,
) -> std::io::Result<()> {
    let body = match target {
        "/braid/hello" => format!(
            "{{\"name\":{},\"device_id\":{},\"version\":1}}",
            quote(&identity.name),
            quote(&identity.device_id)
        ),
        "/braid/status" => {
            let lanes: Vec<String> = identity.lanes.iter().map(Lane::to_json).collect();
            format!("{{\"lanes\":[{}]}}", lanes.join(","))
        }
        "/braid/pair" => format!(
            // A real phone generates this after someone taps accept. A test
            // needs it predictable, not secret.
            "{{\"key\":{}}}",
            quote(&format!("key-for-{}", identity.device_id))
        ),
        _ => return respond(reader.into_inner(), 404, "no such control path").await,
    };

    let mut stream = reader.into_inner();
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await
}

/// A bare status line with a plain-text body.
async fn respond(mut stream: TcpStream, status: u16, message: &str) -> std::io::Result<()> {
    let reason = match status {
        404 => "Not Found",
        407 => "Proxy Authentication Required",
        _ => "OK",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{message}",
        message.len()
    );
    stream.write_all(response.as_bytes()).await
}

/// Whether the request carries the paired key.
///
/// Basic credentials are `user:password` in base64. The user part carries the
/// network the phone should use, so only the password is the secret.
fn presents(headers: &[String], expected: &str) -> bool {
    headers.iter().any(|line| {
        let Some((name, value)) = line.split_once(':') else { return false };
        if !name.trim().eq_ignore_ascii_case("proxy-authorization") {
            return false;
        }
        let Some(encoded) = value.trim().strip_prefix("Basic ") else { return false };
        let Ok(decoded) = base64_decode(encoded) else { return false };
        decoded.split_once(':').is_some_and(|(_, password)| password == expected)
    })
}

/// Enough base64 to read one header, so this crate gains no dependency for it.
fn base64_decode(input: &str) -> std::result::Result<String, ()> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut bits = 0u32;
    let mut count = 0;
    let mut out = Vec::new();
    for byte in input.bytes().filter(|b| *b != b'=') {
        let Some(index) = TABLE.iter().position(|c| *c == byte) else { return Err(()) };
        bits = (bits << 6) | index as u32;
        count += 6;
        if count >= 8 {
            count -= 8;
            out.push((bits >> count) as u8);
        }
    }
    String::from_utf8(out).map_err(|_| ())
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn a_phone_answers_hello_without_being_paired() {
        // How a desktop decides that something on the network is a relay at
        // all, before it has any credentials to offer.
        let relay = Relay::spawn_phone("Pixel", Vec::new()).await.unwrap();
        let body = request(&relay, "GET /braid/hello HTTP/1.1", "").await;
        assert!(body.contains("\"name\":\"Pixel\""), "got {body}");
        assert!(body.contains("\"version\":1"), "got {body}");
        assert!(body.contains("\"device_id\""), "a desktop needs something stable to key on");
    }

    #[tokio::test]
    async fn a_phone_reports_the_lanes_it_was_given() {
        let lanes = vec![
            Lane::new("cell", "cellular", "Mobile data")
                .leaving_from("203.0.113.7")
                .saying("2 GB left"),
            Lane::new("wifi", "wifi", "Home").leaving_from("198.51.100.4"),
        ];
        let relay = Relay::spawn_phone("Pixel", lanes).await.unwrap();
        let body = request(&relay, "GET /braid/status HTTP/1.1", "").await;
        assert!(body.contains("\"id\":\"cell\""), "got {body}");
        assert!(body.contains("\"egress\":\"203.0.113.7\""), "got {body}");
        assert!(body.contains("\"note\":\"2 GB left\""), "got {body}");
        assert!(body.contains("\"label\":\"Home\""), "got {body}");
    }

    #[tokio::test]
    async fn a_name_that_would_break_the_json_does_not() {
        // People name their phones. This is the case the flat key = value
        // format used elsewhere could not carry.
        let relay = Relay::spawn_phone("Suraj = \"phone\"", Vec::new()).await.unwrap();
        let body = request(&relay, "GET /braid/hello HTTP/1.1", "").await;
        assert!(body.contains(r#""name":"Suraj = \"phone\"""#), "got {body}");
    }

    #[tokio::test]
    async fn an_unpaired_desktop_cannot_open_a_tunnel_either() {
        // The path that matters. A download from an HTTPS origin is a CONNECT
        // tunnel, so a stand-in phone that authorised only the absolute-URI
        // form would let every desktop test of "unpaired is refused" pass over
        // a path real traffic never takes.
        let relay = Relay::spawn_paired_phone("Pixel", Vec::new(), "secret").await.unwrap();
        let raw = raw_request(&relay, "CONNECT example.test:443 HTTP/1.1", "").await;
        assert!(raw.starts_with("HTTP/1.1 407"), "expected a challenge, got {raw:?}");
    }

    #[tokio::test]
    async fn a_paired_desktop_may_open_a_tunnel() {
        // The other half: the key has to actually work on this path, or HTTPS
        // downloads through a phone are refused for everyone.
        let relay = Relay::spawn_paired_phone("Pixel", Vec::new(), "secret").await.unwrap();
        let credentials = base64_encode("cell:secret");
        let raw = raw_with_header(
            &relay,
            "CONNECT 127.0.0.1:9 HTTP/1.1",
            &format!("Proxy-Authorization: Basic {credentials}"),
        )
        .await;
        assert!(!raw.starts_with("HTTP/1.1 407"), "a paired desktop was challenged: {raw:?}");
    }

    #[tokio::test]
    async fn an_unpaired_desktop_cannot_use_the_proxy() {
        // Without this, anyone on the same Wi-Fi can spend the phone's data.
        let relay = Relay::spawn_paired_phone("Pixel", Vec::new(), "secret").await.unwrap();
        let raw = raw_request(&relay, "GET http://example.test/x HTTP/1.1", "").await;
        assert!(raw.starts_with("HTTP/1.1 407"), "expected a challenge, got {raw:?}");
    }

    #[tokio::test]
    async fn pairing_hands_back_a_key() {
        let relay = Relay::spawn_phone("Pixel", Vec::new()).await.unwrap();
        let body = request(&relay, "POST /braid/pair HTTP/1.1", r#"{"desktop":"Studio"}"#).await;
        assert!(body.contains("\"key\":\"key-for-testkit-Pixel\""), "got {body}");
    }

    #[tokio::test]
    async fn a_plain_relay_is_not_a_phone() {
        // `Relay::spawn` has no identity, so its control paths must not
        // pretend to be a companion.
        let relay = Relay::spawn().await.unwrap();
        let raw = raw_request(&relay, "GET /braid/hello HTTP/1.1", "").await;
        assert!(raw.starts_with("HTTP/1.1 404"), "got {raw:?}");
    }

    /// Send one request with an extra header and return the whole response.
    async fn raw_with_header(relay: &Relay, line: &str, header: &str) -> String {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let mut stream = tokio::net::TcpStream::connect(relay.addr()).await.unwrap();
        let request = format!("{line}\r\nHost: relay\r\n{header}\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut raw = String::new();
        let _ = stream.read_to_string(&mut raw).await;
        raw
    }

    /// Only used to build a test credential.
    fn base64_encode(input: &str) -> String {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let bytes = input.as_bytes();
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
            let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
            for i in 0..4 {
                if i <= chunk.len() {
                    out.push(TABLE[((n >> (18 - 6 * i)) & 0x3f) as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    /// Send one request and return the body.
    async fn request(relay: &Relay, line: &str, body: &str) -> String {
        let raw = raw_request(relay, line, body).await;
        raw.split_once("\r\n\r\n").expect("a response with a body").1.to_string()
    }

    /// Send one request and return the whole response, status line included.
    async fn raw_request(relay: &Relay, line: &str, body: &str) -> String {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let mut stream = tokio::net::TcpStream::connect(relay.addr()).await.unwrap();
        let request = format!(
            "{line}\r\nHost: relay\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut raw = String::new();
        stream.read_to_string(&mut raw).await.unwrap();
        raw
    }

    use super::*;

    #[test]
    fn an_absolute_uri_splits_into_host_and_path() {
        assert_eq!(
            split_absolute("http://127.0.0.1:8080/a/b.bin?x=1"),
            Some(("127.0.0.1:8080".into(), "/a/b.bin?x=1".into()))
        );
        assert_eq!(
            split_absolute("http://example.test"),
            Some(("example.test".into(), "/".into()))
        );
    }

    #[test]
    fn an_origin_form_target_is_refused() {
        // A proxy is given the absolute URI. Anything else means the client
        // did not know it was talking to one, and guessing a host would send
        // the bytes somewhere arbitrary.
        assert_eq!(split_absolute("/a/b.bin"), None);
        assert_eq!(split_absolute("https://example.test/x"), None, "TLS needs CONNECT");
    }
}
