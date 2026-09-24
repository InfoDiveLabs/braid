//! Pairing by showing a code, rather than by hunting for the phone.
//!
//! Discovery is the fragile half of this feature. Multicast is dropped by
//! plenty of networks and gated per application by macOS, and when it fails it
//! fails silently: the query goes out, nothing answers, and a phone sitting
//! two feet away looks like no phone at all.
//!
//! So this inverts the direction. The desktop shows a code and opens a door
//! for two minutes; the phone reads the code with the camera it already has
//! and announces itself. No multicast, no permission, nothing typed, and it
//! works on a network with no IPv4 on it.
//!
//! The token is a bearer secret for as long as the code is on screen: whoever
//! can read the screen can pair, which is the same trust as handing someone
//! the phone to tap Allow on.

use dl_core::error::{Error, Result};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

/// How long a code stays good for.
///
/// Long enough to pick a phone up and point it, short enough that a screen
/// left unlocked in a café is not an open invitation for the rest of the day.
pub const OFFER_LIFETIME: Duration = Duration::from_secs(120);

/// What the phone sends once someone has confirmed on it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Registration {
    pub device_id: String,
    pub name: String,
    /// Where the desktop should reach it, already bracketed if IPv6.
    ///
    /// Filled in by the phone because only the phone can be right: it uses the
    /// local address of the socket it just reached us on, which is by
    /// construction an address that works over an interface that works.
    pub address: String,
    pub key: String,
}

/// A code on screen and the door it opens.
pub struct Offer {
    /// What the QR encodes.
    pub uri: String,
    listener: TcpListener,
    token: String,
}

impl Offer {
    /// Open the door and build the code.
    ///
    /// `host` is this computer as the phone will see it, which the caller
    /// chooses because only it knows which interface faces the phone. The
    /// phone refuses a code pointing anywhere but its own link, so a wrong
    /// choice here is a refusal there rather than a silent mispairing.
    pub async fn open(desktop_name: &str, host: &str) -> Result<Self> {
        // All interfaces: the phone may arrive over Wi-Fi, over a cable, or
        // over a hotspot, and which one is not knowable in advance.
        let listener = TcpListener::bind("[::]:0")
            .await
            .or(TcpListener::bind("0.0.0.0:0").await)
            .map_err(|e| Error::Transport(format!("opening a pairing port: {e}")))?;
        let port = listener
            .local_addr()
            .map_err(|e| Error::Transport(format!("reading the pairing port: {e}")))?
            .port();

        let token = token();
        let uri = format!(
            "braid://pair?v=1&h={}&p={port}&t={token}&n={}",
            encode(host),
            encode(desktop_name)
        );
        Ok(Self { uri, listener, token })
    }

    /// Wait for a phone to announce itself.
    ///
    /// Returns `None` when the code expires. Exactly one successful
    /// registration is accepted: a token is single use, so a code photographed
    /// over someone's shoulder is worth nothing once it has been used.
    pub async fn accept(self) -> Option<Registration> {
        let deadline = tokio::time::Instant::now() + OFFER_LIFETIME;
        loop {
            let accepted =
                tokio::time::timeout_at(deadline, self.listener.accept()).await.ok()?.ok()?;
            match read_registration(accepted.0, &self.token).await {
                Ok(Some(registration)) => return Some(registration),
                Ok(None) => continue,
                Err(error) => {
                    tracing::debug!(%error, "a pairing attempt ended badly");
                    continue;
                }
            }
        }
    }
}

/// 32 bytes of system randomness as lowercase hex.
fn token() -> String {
    let mut bytes = [0u8; 32];
    // A predictable token would let anyone who can reach the port pair
    // without ever seeing the screen.
    getrandom::fill(&mut bytes).expect("the system has randomness");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Percent-encode a query value.
///
/// An IPv6 host arrives bracketed and a desktop is named by a person, so both
/// routinely contain characters that would otherwise end the value early.
fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Read one request and, if it is a valid registration, return it.
async fn read_registration(
    stream: tokio::net::TcpStream,
    token: &str,
) -> Result<Option<Registration>> {
    let peer = stream.peer_addr().ok();
    let mut reader = BufReader::new(stream);

    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .await
        .map_err(|e| Error::Transport(format!("reading a pairing request: {e}")))?;

    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();

    let mut length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await.unwrap_or(0) == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().unwrap_or(0);
        }
    }

    if !method.eq_ignore_ascii_case("POST") || path != "/braid/register" {
        respond(reader.into_inner(), 404, "no such path").await;
        return Ok(None);
    }

    let mut body = vec![0u8; length.min(8 * 1024)];
    reader
        .read_exact(&mut body)
        .await
        .map_err(|e| Error::Transport(format!("reading a pairing body: {e}")))?;
    let body = String::from_utf8_lossy(&body).to_string();

    let Some(registration) = parse(&body) else {
        respond(reader.into_inner(), 400, "not a registration").await;
        return Ok(None);
    };

    // Compared in full. A token is the whole secret, so a near miss is a miss.
    if field(&body, "token").as_deref() != Some(token) {
        tracing::warn!(?peer, "a pairing attempt presented the wrong token");
        respond(reader.into_inner(), 403, "that code has expired").await;
        return Ok(None);
    }

    respond(reader.into_inner(), 200, r#"{"ok":true}"#).await;
    Ok(Some(registration))
}

async fn respond(mut stream: tokio::net::TcpStream, status: u16, body: &str) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        _ => "Not Found",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
}

/// The five fields, read without pulling a JSON parser into this crate's
/// public surface. The phone is a separate program, so this must cope with
/// whitespace and field order it did not choose.
fn parse(body: &str) -> Option<Registration> {
    let registration = Registration {
        device_id: field(body, "device_id")?,
        name: field(body, "name")?,
        address: field(body, "address")?,
        key: field(body, "key")?,
    };
    // A phone that names itself and nothing else is not usable, and storing it
    // would put a row in the sidebar that can never serve.
    if registration.address.is_empty() || registration.key.is_empty() {
        return None;
    }
    Some(registration)
}

/// One string field out of a flat JSON object.
fn field(body: &str, name: &str) -> Option<String> {
    let key = format!("\"{name}\"");
    let start = body.find(&key)? + key.len();
    let rest = body.get(start..)?;
    let colon = rest.find(':')? + 1;
    let rest = rest.get(colon..)?;
    let open = rest.find('"')? + 1;
    let rest = rest.get(open..)?;

    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                other => out.push(other),
            },
            other => out.push(other),
        }
    }
    None
}

/// Which of this computer's addresses to put in the code.
///
/// The phone refuses a code that does not point at its own link, so this has
/// to be an address on the network the phone is on. The interface carrying the
/// default route is the best guess available without asking the phone first,
/// and a wrong guess is a refusal with an explanation rather than a silent
/// mispairing.
pub fn host_for_offer(provider: &dyn crate::iface::InterfaceProvider) -> Option<String> {
    let usable = provider.usable();
    let best = usable.iter().filter(|i| i.has_gateway && !i.is_loopback).max_by_key(|i| {
        i.ipv4.iter().filter(|a| reachable_v4(a)).count()
            + i.ipv6.iter().filter(|a| reachable_v6(a)).count()
    })?;

    // IPv4 when there is a real one: it is shorter on screen and every
    // network that has it routes it. Otherwise the global IPv6, which is the
    // only answer on a carrier with no IPv4 at all, and those are exactly the
    // networks where borrowing a phone's connection is most wanted.
    if let Some(v4) = best.ipv4.iter().find(|a| reachable_v4(a)) {
        return Some(v4.to_string());
    }
    best.ipv6.iter().find(|a| reachable_v6(a)).map(|v6| format!("[{v6}]"))
}

/// Whether another machine on this link could actually open a socket to it.
///
/// `192.0.0.0/29` is the trap worth naming. On an IPv6-only carrier with
/// 464XLAT the only IPv4 address a machine has is its CLAT address out of that
/// range, which exists so local software can keep speaking IPv4 to a
/// translator. It is not reachable by anything else, and worse, a phone on the
/// same kind of network has one too, so a naive "is it in my subnet" check on
/// the far end says yes and the connection then fails for reasons nobody can
/// see. Measured on this machine, which offered 192.0.0.2 before this existed.
fn reachable_v4(address: &std::net::Ipv4Addr) -> bool {
    let o = address.octets();
    let clat = o[0] == 192 && o[1] == 0 && o[2] == 0 && o[3] < 8;
    !clat && !address.is_link_local() && !address.is_loopback() && !address.is_unspecified()
}

fn reachable_v6(address: &std::net::Ipv6Addr) -> bool {
    let link_local = (address.segments()[0] & 0xffc0) == 0xfe80;
    !link_local && !address.is_loopback() && !address.is_unspecified()
}

#[cfg(test)]
mod host_tests {
    use super::*;

    #[test]
    fn a_clat_address_is_never_offered() {
        // 192.0.0.0/29 exists for talking to a local translator and reaches
        // nothing else. Offering it produces a code the phone accepts and then
        // cannot connect to, which is the worst of both.
        assert!(!reachable_v4(&"192.0.0.2".parse().unwrap()));
        assert!(!reachable_v4(&"192.0.0.4".parse().unwrap()));
        assert!(reachable_v4(&"192.0.1.2".parse().unwrap()), "only /29 is special");
        assert!(reachable_v4(&"192.168.1.10".parse().unwrap()));
    }

    #[test]
    fn link_local_and_loopback_are_never_offered() {
        assert!(!reachable_v4(&"169.254.1.1".parse().unwrap()));
        assert!(!reachable_v4(&"127.0.0.1".parse().unwrap()));
        assert!(!reachable_v6(&"fe80::1".parse().unwrap()));
        assert!(!reachable_v6(&"::1".parse().unwrap()));
        assert!(reachable_v6(&"2409:40e4:2004:769a::1".parse().unwrap()));
    }
}
