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
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Relay {
    /// Start one on an ephemeral loopback port.
    pub async fn spawn() -> std::io::Result<Self> {
        Self::spawn_with_limit(None).await
    }

    /// Start one that refuses to forward more than `cap` requests.
    ///
    /// A relay that stops answering is the normal case, not an exotic one: a
    /// phone goes out of signal, sleeps, or overheats. The engine has to park
    /// that lane and carry on rather than failing the transfer.
    pub async fn spawn_with_limit(cap: Option<u64>) -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let forwarded = Arc::new(AtomicU64::new(0));
        let (tx, mut rx) = tokio::sync::oneshot::channel();

        let counter = Arc::clone(&forwarded);
        tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    _ = &mut rx => return,
                    accepted = listener.accept() => accepted,
                };
                let Ok((client, _)) = accepted else { return };
                let counter = Arc::clone(&counter);
                tokio::spawn(async move {
                    let seen = counter.fetch_add(1, Ordering::SeqCst);
                    if cap.is_some_and(|cap| seen >= cap) {
                        return;
                    }
                    if let Err(error) = forward(client).await {
                        tracing::debug!(%error, "relay connection ended");
                    }
                });
            }
        });

        Ok(Self { addr, forwarded, shutdown: Some(tx) })
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

/// Read the request line and headers, open the upstream, and splice.
///
/// A forward proxy receives the absolute URI on the request line: /// `GET http://host/path HTTP/1.1`: and sends the origin-form on. Everything
/// else, headers and body alike, is copied through untouched: rewriting any of
/// it here would hide a bug in what the engine actually sent.
async fn forward(client: TcpStream) -> std::io::Result<()> {
    let mut reader = BufReader::new(client);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).await?;

    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();
    let version = parts.next().unwrap_or("HTTP/1.1").to_string();

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

    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 {
            break;
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        // Hop-by-hop, and meaningless to the origin.
        if !line.to_ascii_lowercase().starts_with("proxy-") {
            headers.push(line);
        }
    }

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
async fn tunnel(mut reader: BufReader<TcpStream>, target: &str) -> std::io::Result<()> {
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 {
            break;
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
    }

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

#[cfg(test)]
mod tests {
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
