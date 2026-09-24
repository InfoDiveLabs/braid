//! The phones this computer has paired with, across a quit.
//!
//! Beside `settings.conf` and `transfers.conf`, in the same flat format and
//! for the same reason. Pairing is something a person did once by picking
//! their phone up and tapping accept; asking them again every launch is the
//! kind of friction that makes a feature go unused.

use dl_net::Relay;
use std::path::PathBuf;

/// A phone this computer may use, and which of its lanes are switched on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paired {
    pub relay: Relay,
    /// Stable across a rename and a new lease, so a phone that moved is
    /// recognised rather than offered for pairing again.
    pub device_id: String,
    /// Lane ids the person switched on. Anything absent is off, which is the
    /// right default for a lane that spends someone's money.
    pub enabled: Vec<String>,
}

fn path() -> Option<PathBuf> {
    crate::settings::Settings::path().map(|p| p.with_file_name("relays.conf"))
}

fn encode(paired: &[Paired]) -> String {
    let mut out = String::from("# Braid relays. Written by the app.\n");
    for entry in paired {
        out.push_str("\n[relay]\n");
        out.push_str(&format!("name = {}\n", entry.relay.name));
        out.push_str(&format!("address = {}\n", entry.relay.address));
        out.push_str(&format!("device_id = {}\n", entry.device_id));
        if let Some(key) = &entry.relay.key {
            out.push_str(&format!("key = {key}\n"));
        }
        if !entry.enabled.is_empty() {
            out.push_str(&format!("enabled = {}\n", entry.enabled.join(",")));
        }
    }
    out
}

/// One record as it is being read, before it is known to be complete.
#[derive(Default)]
struct Record {
    name: Option<String>,
    address: Option<String>,
    device_id: String,
    key: Option<String>,
    enabled: Vec<String>,
}

impl Record {
    /// A record missing a name or an address is dropped whole rather than
    /// guessed at: half a relay is a lane pointing nowhere.
    fn flush(self, out: &mut Vec<Paired>) {
        let (Some(name), Some(address)) = (self.name, self.address) else { return };
        out.push(Paired {
            relay: Relay::new(name, address, self.key),
            device_id: self.device_id,
            enabled: self.enabled,
        });
    }
}

fn decode(text: &str) -> Vec<Paired> {
    let mut out = Vec::new();
    let mut record = Record::default();

    for line in text.lines() {
        let line = line.trim();
        if line == "[relay]" {
            std::mem::take(&mut record).flush(&mut out);
            continue;
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        let (key, value) = (key.trim(), value.trim());
        match key {
            "name" => record.name = Some(value.to_string()),
            "address" => record.address = Some(value.to_string()),
            "device_id" => record.device_id = value.to_string(),
            "key" => record.key = Some(value.to_string()),
            "enabled" => {
                record.enabled = value
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect();
            }
            _ => {}
        }
    }
    record.flush(&mut out);
    out
}

pub fn load() -> Vec<Paired> {
    let Some(path) = path() else { return Vec::new() };
    let Ok(text) = std::fs::read_to_string(&path) else { return Vec::new() };
    decode(&text)
}

/// Written through a temporary file, so an interrupted save leaves the
/// previous list rather than half of this one.
pub fn save(paired: &[Paired]) {
    let Some(path) = path() else { return };
    if let Some(parent) = path.parent()
        && std::fs::create_dir_all(parent).is_err()
    {
        return;
    }
    let temp = path.with_extension("conf.tmp");
    if std::fs::write(&temp, encode(paired)).is_ok() {
        let _ = std::fs::rename(&temp, &path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paired() -> Paired {
        Paired {
            relay: Relay::new("Pixel", "192.168.1.42:8710", Some("secret".into())),
            device_id: "abc123".into(),
            enabled: vec!["cell".into()],
        }
    }

    #[test]
    fn a_paired_phone_survives_a_round_trip() {
        let back = decode(&encode(&[paired()]));
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].relay.name, "Pixel");
        assert_eq!(back[0].relay.address, "192.168.1.42:8710");
        assert_eq!(back[0].relay.key.as_deref(), Some("secret"));
        assert_eq!(back[0].device_id, "abc123");
        assert_eq!(back[0].enabled, vec!["cell".to_string()]);
    }

    #[test]
    fn a_phone_with_no_enabled_lanes_is_still_remembered() {
        // Pairing and using are different decisions. Forgetting a phone
        // because every lane is off would mean pairing it again to turn one on.
        let mut one = paired();
        one.enabled.clear();
        let back = decode(&encode(&[one]));
        assert_eq!(back.len(), 1);
        assert!(back[0].enabled.is_empty());
    }

    #[test]
    fn a_record_with_no_address_is_dropped_whole() {
        assert!(decode("[relay]\nname = Pixel\n").is_empty());
    }

    #[test]
    fn a_relay_with_no_key_is_kept_as_unpaired() {
        // Seen but not authorised is a real state, and it is the one the
        // settings page needs in order to offer a Pair button.
        let mut one = paired();
        one.relay.key = None;
        let back = decode(&encode(&[one]));
        assert_eq!(back.len(), 1);
        assert!(back[0].relay.key.is_none());
    }

    #[test]
    fn several_phones_keep_their_own_lanes() {
        // The list is per device; mixing the enabled ids would send traffic
        // over a radio someone switched off.
        let mut second = paired();
        second.relay.name = "Spare".into();
        second.relay.address = "192.168.1.43:8710".into();
        second.enabled = vec!["wifi".into(), "cell".into()];

        let back = decode(&encode(&[paired(), second]));
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].enabled, vec!["cell".to_string()]);
        assert_eq!(back[1].enabled, vec!["wifi".to_string(), "cell".to_string()]);
    }
}
