//! The small protocol a relay speaks beside its proxy traffic.
//!
//! A forward proxy receives absolute-URI request lines and nothing else, so
//! origin-form paths on the same port are unambiguous and can carry a control
//! plane: one port, one service record, one hole in a firewall.
//!
//! JSON rather than the flat `key = value` format the config files use. A
//! phone is named by a person and that name can contain an `=`, a quote or a
//! newline, and the phone is a separate program in another language that will
//! gain fields before this one learns about them.

use serde::{Deserialize, Serialize};

/// What this build speaks. A phone reporting a higher number may offer things
/// this desktop will ignore, which is allowed; a lower number means fields we
/// rely on may be missing.
pub const PROTOCOL_VERSION: u32 = 1;

/// The answer to `/braid/hello`. Unauthenticated: it is how a desktop decides
/// whether something on the network is a relay at all.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// What to call it. The phone's own name, usually.
    pub name: String,
    /// Stable across a rename and across an address change, so a phone that
    /// moved to a new lease is recognised as one already paired.
    pub device_id: String,
    pub version: u32,
}

/// How a lane reaches the internet, as the phone describes it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LaneKind {
    Cellular,
    Wifi,
    Ethernet,
    /// A transport this build does not know. Still usable: the kind only
    /// chooses an icon.
    #[serde(other)]
    #[default]
    Unknown,
}

/// One path the phone is willing to serve.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfferedLane {
    /// Opaque to us, and sent back on every request so the phone knows which
    /// of its networks to use.
    pub id: String,
    #[serde(default)]
    pub kind: LaneKind,
    /// Shown in the sidebar.
    pub label: String,
    /// The public address this lane leaves from, when the phone knows it.
    /// Used to notice that a lane and the desktop share an upstream.
    #[serde(default)]
    pub egress: Option<String>,
    /// Whatever the phone wants to say about itself: an allowance left, a
    /// warning that it is hot. Displayed verbatim, never parsed.
    #[serde(default)]
    pub note: Option<String>,
}

/// The answer to `/braid/status`: everything this phone is offering now.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    #[serde(default)]
    pub lanes: Vec<OfferedLane>,
}

/// Sent to `/braid/pair`. The name is shown on the phone so a person can see
/// what they are about to authorise.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairRequest {
    pub desktop: String,
}

/// What comes back when someone taps accept.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairOutcome {
    /// Sent as proxy credentials from then on.
    pub key: String,
}

use dl_core::error::{Error, Result};
use std::time::Duration;

/// Long enough for a phone whose radio is asleep to answer, short enough that
/// a device which has left the network does not hold up a settings page.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(4);

/// A person has to pick the phone up and look at it.
const PAIR_TIMEOUT: Duration = Duration::from_secs(60);

/// Ask what is listening there.
///
/// Unauthenticated on purpose: this is the call that decides whether an
/// address is a relay at all, and a desktop has to be able to make it before
/// it has any credentials.
pub async fn hello(address: &str) -> Result<Hello> {
    get(address, "/braid/hello", None).await
}

/// Ask what it is offering right now.
///
/// The key is optional because a phone may choose to describe itself to a
/// desktop it has not paired with. If it does not, its refusal arrives as a
/// transport error and the caller shows the relay as present but silent
/// rather than gone.
pub async fn status(address: &str, key: Option<&str>) -> Result<Status> {
    get(address, "/braid/status", key).await
}

/// Ask to be allowed to use this phone.
///
/// A real phone shows the name and waits for someone to tap accept, so this
/// call may sit for as long as a person takes to look at their screen.
pub async fn pair(address: &str, desktop: &str) -> Result<PairOutcome> {
    let client = control_client(PAIR_TIMEOUT)?;
    let response = client
        .post(format!("http://{address}/braid/pair"))
        .header("Content-Type", "application/json")
        .body(
            serde_json::to_string(&PairRequest { desktop: desktop.to_string() })
                .expect("a string field serialises"),
        )
        .send()
        .await
        .map_err(|e| Error::Transport(format!("reaching {address}: {e}")))?;

    if !response.status().is_success() {
        return Err(Error::Transport(format!("{address} refused to pair: {}", response.status())));
    }
    let body =
        response.text().await.map_err(|e| Error::Transport(format!("reading {address}: {e}")))?;
    serde_json::from_str(&body)
        .map_err(|e| Error::Transport(format!("{address} sent no usable key: {e}")))
}

/// A client for talking *to* a relay rather than through one.
///
/// `no_proxy` matters: a desktop with a system proxy configured would
/// otherwise ask that proxy to fetch the phone, which is both wrong and
/// usually unreachable.
fn control_client(timeout: Duration) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(timeout)
        .no_proxy()
        .build()
        .map_err(|e| Error::Transport(format!("building the control client: {e}")))
}

async fn get<T: for<'de> Deserialize<'de>>(
    address: &str,
    path: &str,
    key: Option<&str>,
) -> Result<T> {
    let client = control_client(CONTROL_TIMEOUT)?;
    let mut request = client.get(format!("http://{address}{path}"));
    if let Some(key) = key {
        request = request.header("X-Braid-Key", key);
    }

    let response =
        request.send().await.map_err(|e| Error::Transport(format!("reaching {address}: {e}")))?;

    if !response.status().is_success() {
        return Err(Error::Transport(format!("{address} answered {}", response.status())));
    }

    let body =
        response.text().await.map_err(|e| Error::Transport(format!("reading {address}: {e}")))?;
    serde_json::from_str(&body)
        .map_err(|e| Error::Transport(format!("{address} is not a relay we understand: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn hello_reads_a_phone_that_is_there() {
        let relay = dl_testkit::Relay::spawn_phone("Pixel", Vec::new()).await.unwrap();
        let found = hello(&relay.addr().to_string()).await.expect("a relay answers");
        assert_eq!(found.name, "Pixel");
        assert_eq!(found.version, PROTOCOL_VERSION);
    }

    #[tokio::test]
    async fn hello_fails_on_something_that_is_not_a_relay() {
        // A port with nothing behind it. Discovery turns up addresses that are
        // routers and printers, and this is what tells them apart.
        assert!(hello("127.0.0.1:9").await.is_err());
    }

    #[tokio::test]
    async fn hello_fails_on_a_proxy_that_is_not_a_companion() {
        // A plain forward proxy answers 404 here. Treating it as a relay would
        // put a lane in the sidebar that can never serve.
        let relay = dl_testkit::Relay::spawn().await.unwrap();
        assert!(hello(&relay.addr().to_string()).await.is_err());
    }

    #[tokio::test]
    async fn status_reads_the_offered_lanes() {
        let lanes = vec![
            dl_testkit::Lane::new("cell", "cellular", "Mobile data")
                .leaving_from("203.0.113.7")
                .saying("2 GB left"),
            dl_testkit::Lane::new("wifi", "wifi", "Home"),
        ];
        let relay = dl_testkit::Relay::spawn_phone("Pixel", lanes).await.unwrap();
        let status = status(&relay.addr().to_string(), None).await.unwrap();
        assert_eq!(status.lanes.len(), 2);
        assert_eq!(status.lanes[0].kind, LaneKind::Cellular);
        assert_eq!(status.lanes[0].egress.as_deref(), Some("203.0.113.7"));
        assert_eq!(status.lanes[0].note.as_deref(), Some("2 GB left"));
        // A lane that says nothing about itself is still a lane.
        assert_eq!(status.lanes[1].note, None);
    }

    #[tokio::test]
    async fn pairing_returns_a_key_we_can_keep() {
        let relay = dl_testkit::Relay::spawn_phone("Pixel", Vec::new()).await.unwrap();
        let outcome = pair(&relay.addr().to_string(), "Studio").await.unwrap();
        assert!(!outcome.key.is_empty());
    }

    #[test]
    fn a_status_survives_the_wire() {
        let status = Status {
            lanes: vec![OfferedLane {
                id: "cell".into(),
                kind: LaneKind::Cellular,
                label: "Jio 4G".into(),
                egress: Some("203.0.113.7".into()),
                note: Some("1.4 GB left this month".into()),
            }],
        };
        let back: Status = serde_json::from_str(&serde_json::to_string(&status).unwrap()).unwrap();
        assert_eq!(back.lanes.len(), 1);
        assert_eq!(back.lanes[0].id, "cell");
        assert_eq!(back.lanes[0].kind, LaneKind::Cellular);
        assert_eq!(back.lanes[0].note.as_deref(), Some("1.4 GB left this month"));
    }

    #[test]
    fn a_field_we_do_not_know_is_not_an_error() {
        // The phone is released separately and will gain fields. Refusing to
        // parse a newer phone would strand a desktop that is working fine.
        let json = r#"{"lanes":[{"id":"w","kind":"wifi","label":"Home","weather":"sunny"}]}"#;
        let status: Status = serde_json::from_str(json).expect("unknown fields are ignored");
        assert_eq!(status.lanes[0].label, "Home");
    }

    #[test]
    fn an_unknown_transport_kind_is_not_an_error() {
        // Same reason, one level down: a phone on a transport this build has
        // never heard of still has a usable lane.
        let json = r#"{"lanes":[{"id":"x","kind":"satellite","label":"Dish"}]}"#;
        let status: Status = serde_json::from_str(json).unwrap();
        assert_eq!(status.lanes[0].kind, LaneKind::Unknown);
    }

    #[test]
    fn a_device_name_with_awkward_characters_round_trips() {
        // Phones are named by people. JSON is used here rather than the flat
        // key = value format the config files use precisely because a name
        // can contain =, a newline, or a quote.
        let hello = Hello {
            name: "Suraj = \"phone\"\nspare".into(),
            device_id: "abc".into(),
            version: PROTOCOL_VERSION,
        };
        let back: Hello = serde_json::from_str(&serde_json::to_string(&hello).unwrap()).unwrap();
        assert_eq!(back.name, hello.name);
    }
}
