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
    /// The public IPv4 address this lane leaves from, when the phone knows it.
    #[serde(default)]
    pub egress: Option<String>,
    /// The global IPv6 address this lane leaves from.
    ///
    /// Carried as well as the IPv4 because on an IPv6-only carrier the IPv4 is
    /// not a property of the route at all: NAT64 hands out a public IPv4 per
    /// source address, so two devices on one link get two different ones, and
    /// the same device gets a different one a few minutes later. The IPv6
    /// prefix is stable and is shared by everything on the link.
    #[serde(default)]
    pub egress6: Option<String>,
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

/// What this computer looks like from outside, in both families.
///
/// Either half may be absent: a network with no IPv6, or an echo service that
/// cannot be reached. Absent means no claim, never a guess.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HostEgress {
    pub v4: Option<String>,
    pub v6: Option<String>,
}

/// This computer's own public address, asked of a service that echoes it.
///
/// Used only to notice that a phone is offering the route we already have. A
/// failure is not an error: it means no lane is marked duplicate, which shows
/// someone one more lane than they need rather than hiding one they wanted.
///
/// The service is a parameter rather than a constant so that it is testable,
/// and so a person can point it at something they trust instead of whatever
/// this project chose.
pub async fn host_egress(v4_service: &str, v6_service: &str) -> HostEgress {
    HostEgress { v4: echo(v4_service).await, v6: echo(v6_service).await }
}

async fn echo(service: &str) -> Option<String> {
    let client = control_client(CONTROL_TIMEOUT).ok()?;
    let body = client.get(service).send().await.ok()?.text().await.ok()?;
    // Whatever comes back has to look like an address. A captive portal
    // answering with a login page must not become "our" egress and switch off
    // every lane as a duplicate.
    body.trim().parse::<std::net::IpAddr>().ok().map(|address| address.to_string())
}

/// Whether two IPv6 addresses sit on the same /64.
///
/// The prefix, not the address: both ends use privacy addresses whose low 64
/// bits rotate, while everything on one link shares the top 64. A /64 is
/// delegated by one provider, so a match is strong evidence of one link and
/// therefore one upstream. A mismatch proves nothing, which is why it only
/// ever adds a duplicate and never clears one.
fn same_prefix(ours: &str, theirs: &str) -> bool {
    let (Ok(ours), Ok(theirs)) =
        (ours.parse::<std::net::Ipv6Addr>(), theirs.parse::<std::net::Ipv6Addr>())
    else {
        return false;
    };
    ours.octets()[..8] == theirs.octets()[..8]
}

/// The offered lanes that leave from the same address this computer does.
///
/// Such a lane is the route we already have, wearing a second name. Throughput
/// will not reveal it: a duplicate lane splits the same pipe and looks like it
/// is working.
///
/// Reported rather than hidden, so the settings page can show it switched off
/// with a reason instead of quietly dropping something the phone said it had.
/// An unknown address on either side means no claim is made: guessing would
/// switch off what might be the only useful lane.
pub fn duplicates_of<'a>(status: &'a Status, host: &HostEgress) -> Vec<&'a OfferedLane> {
    status
        .lanes
        .iter()
        .filter(|lane| {
            // Either signal is enough. They fail in opposite conditions: the
            // IPv4 is useless behind NAT64, which hands one out per source
            // address, and the IPv6 is absent on networks that have none.
            let same_v4 = match (&host.v4, &lane.egress) {
                (Some(ours), Some(theirs)) => ours == theirs,
                _ => false,
            };
            let same_v6 = match (&host.v6, &lane.egress6) {
                (Some(ours), Some(theirs)) => same_prefix(ours, theirs),
                _ => false,
            };
            same_v4 || same_v6
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A lane offering both families, as a phone that knows where it is will.
    fn both(id: &str, egress: &str, egress6: &str) -> OfferedLane {
        OfferedLane {
            id: id.into(),
            kind: LaneKind::Unknown,
            label: id.into(),
            egress: Some(egress.into()),
            egress6: Some(egress6.into()),
            note: None,
        }
    }

    #[test]
    fn a_shared_link_is_caught_even_when_the_public_ipv4_differs() {
        // Measured on a Pixel 7 Pro and a Mac sharing one hotspot on an
        // IPv6-only carrier. Both are on the same link, verified by router and
        // prefix, yet NAT64 handed each a different public IPv4, because it
        // allocates per source address. Matching on IPv4 alone declared the
        // phone's Wi-Fi lane a separate path when it is exactly the desktop's
        // own route: the placebo lane this whole check exists to catch.
        let status =
            Status { lanes: vec![both("wifi", "152.59.168.122", "2409:40e4:2004:769a:1:2:3:4")] };
        let host = HostEgress {
            v4: Some("152.59.170.6".into()),
            v6: Some("2409:40e4:2004:769a:aaaa:bbbb:cccc:dddd".into()),
        };
        assert_eq!(duplicates_of(&status, &host).len(), 1, "the shared link was missed");
    }

    #[test]
    fn a_different_prefix_is_a_different_link() {
        let status =
            Status { lanes: vec![both("cell", "152.59.146.101", "2409:40e4:110a:6fcb:1:2:3:4")] };
        let host = HostEgress {
            v4: Some("152.59.170.6".into()),
            v6: Some("2409:40e4:2004:769a:aaaa:bbbb:cccc:dddd".into()),
        };
        assert!(duplicates_of(&status, &host).is_empty(), "two links were merged into one");
    }

    #[test]
    fn the_low_bits_are_ignored_because_they_rotate() {
        // Privacy addresses change on both ends while the link does not.
        assert!(same_prefix(
            "2409:40e4:2004:769a:0000:0000:0000:0001",
            "2409:40e4:2004:769a:ffff:ffff:ffff:fffe"
        ));
    }

    #[test]
    fn matching_ipv4_still_counts_on_a_network_with_no_ipv6() {
        // The original rule has to keep working: most networks are still
        // IPv4-only, and there the public address is a real identity.
        let status = Status { lanes: vec![offered("wifi", Some("203.0.113.7"))] };
        assert_eq!(duplicates_of(&status, &v4("203.0.113.7")).len(), 1);
    }

    #[test]
    fn a_phone_that_reports_no_ipv6_is_judged_on_ipv4_alone() {
        // An older companion, or one on a network with no v6. Absence must not
        // be read as a mismatch and clear a duplicate the v4 rule found.
        let status = Status { lanes: vec![offered("wifi", Some("203.0.113.7"))] };
        let host = HostEgress {
            v4: Some("203.0.113.7".into()),
            v6: Some("2409:40e4:2004:769a::1".into()),
        };
        assert_eq!(duplicates_of(&status, &host).len(), 1);
    }

    #[test]
    fn something_that_is_not_an_address_never_matches() {
        // A captive portal answering with HTML must not make every lane a
        // duplicate and switch the feature off.
        assert!(!same_prefix("not an address", "not an address"));
    }

    #[test]
    fn an_older_phone_still_parses() {
        // No egress6 field at all, which is what every phone sends today.
        let json = r#"{"lanes":[{"id":"w","kind":"wifi","label":"Home","egress":"203.0.113.7"}]}"#;
        let status: Status = serde_json::from_str(json).expect("the field is optional");
        assert_eq!(status.lanes[0].egress6, None);
    }

    #[tokio::test]
    async fn our_own_address_comes_from_whatever_service_is_named() {
        let origin =
            dl_testkit::Origin::spawn(dl_testkit::Scenario::Ok200 { size: 8 }).await.unwrap();
        // The origin serves bytes, not an address, which is the case that
        // matters: anything that is not an address must be refused rather
        // than believed.
        let found = host_egress(&origin.url("payload.bin"), &origin.url("payload.bin")).await;
        assert_eq!(found, HostEgress::default());
    }

    #[tokio::test]
    async fn a_service_that_is_not_there_costs_nothing() {
        // Offline, or the service is down. No lane is marked duplicate, which
        // is the safe direction.
        assert_eq!(
            host_egress("http://127.0.0.1:9/ip", "http://127.0.0.1:9/ip").await,
            HostEgress::default()
        );
    }

    fn offered(id: &str, egress: Option<&str>) -> OfferedLane {
        OfferedLane {
            id: id.into(),
            kind: LaneKind::Unknown,
            label: id.into(),
            egress: egress.map(str::to_string),
            egress6: None,
            note: None,
        }
    }

    fn v4(address: &str) -> HostEgress {
        HostEgress { v4: Some(address.into()), v6: None }
    }

    #[test]
    fn a_lane_leaving_from_our_own_address_is_the_route_we_already_have() {
        let status = Status {
            lanes: vec![
                offered("wifi", Some("203.0.113.7")),
                offered("cell", Some("198.51.100.4")),
            ],
        };
        let same = duplicates_of(&status, &v4("203.0.113.7"));
        assert_eq!(same.len(), 1);
        assert_eq!(same[0].id, "wifi");
    }

    #[test]
    fn nothing_is_a_duplicate_when_we_do_not_know_our_own_address() {
        // Guessing here would switch off a phone's only useful lane.
        let status = Status { lanes: vec![offered("wifi", Some("203.0.113.7"))] };
        assert!(duplicates_of(&status, &HostEgress::default()).is_empty());
    }

    #[test]
    fn a_lane_that_will_not_say_where_it_leaves_from_is_not_a_duplicate() {
        // Silence is not evidence.
        let status = Status { lanes: vec![offered("wifi", None)] };
        assert!(duplicates_of(&status, &v4("203.0.113.7")).is_empty());
    }

    #[test]
    fn a_phone_behind_the_same_router_on_every_lane_is_all_duplicates() {
        // The case that makes this worth doing: a phone on the desk, on the
        // same Wi-Fi, with mobile data off. Every lane it offers is the
        // connection we already have.
        let status = Status {
            lanes: vec![offered("wifi", Some("203.0.113.7")), offered("cell", Some("203.0.113.7"))],
        };
        assert_eq!(duplicates_of(&status, &v4("203.0.113.7")).len(), 2);
    }

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
                egress6: Some("2409:40e4:110a:6fcb::1".into()),
                note: Some("1.4 GB left this month".into()),
            }],
        };
        let back: Status = serde_json::from_str(&serde_json::to_string(&status).unwrap()).unwrap();
        assert_eq!(back.lanes.len(), 1);
        assert_eq!(back.lanes[0].id, "cell");
        assert_eq!(back.lanes[0].kind, LaneKind::Cellular);
        assert_eq!(back.lanes[0].egress6.as_deref(), Some("2409:40e4:110a:6fcb::1"));
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
