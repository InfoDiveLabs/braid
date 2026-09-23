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

#[cfg(test)]
mod tests {
    use super::*;

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
