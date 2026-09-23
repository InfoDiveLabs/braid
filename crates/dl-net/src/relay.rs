//! Lanes that reach the origin through a relay rather than through a local
//! interface.
//!
//! A relay is a machine on the network forwarding for us: in practice an
//! Android phone running the companion app, which binds its sockets to the
//! cellular network and so reaches the origin over mobile data while itself
//! sitting on the local Wi-Fi. Several phones are several independent paths to
//! the internet, which is the same thing multi-NIC gives, arrived at from the
//! other direction.
//!
//! **Nothing here binds a socket.** A relay lane is an ordinary HTTP client
//! pointed at a different *address*, so none of `SO_BINDTODEVICE`,
//! `IP_BOUND_IF` or `IP_UNICAST_IF` is involved. That is most of the appeal:
//! the binding matrix is the least portable part of this project and the only
//! piece that cannot be verified on the machine it was written on, and this
//! path needs none of it. It works the same on all three desktops and needs no
//! privileges.
//!
//! What a relay shares with an interface is everything that matters to the
//! engine: it is a [`ByteSource`], it has its own public address so per-source
//! signed URLs resolve per lane, and it can go away mid-transfer: a phone
//! loses signal, sleeps, or throttles itself when hot: which the lane selector
//! already handles by parking it and moving the work.

use crate::http::{HttpConfig, HttpSource, ProxyMode};
use dl_core::error::{Error, Result};
use dl_core::lane::LaneSet;
use dl_core::source::ByteSource;

/// One relay, as the user configured it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Relay {
    /// What to call it in the sidebar. The phone's name, usually.
    pub name: String,
    /// Where it listens, as `host:port` or a full proxy URL.
    pub address: String,
}

impl Relay {
    pub fn new(name: impl Into<String>, address: impl Into<String>) -> Self {
        Self { name: name.into(), address: address.into() }
    }
}

/// A [`LaneSet`] where every lane goes through a different relay.
pub struct RelayLanes {
    sources: Vec<HttpSource>,
    labels: Vec<String>,
}

impl RelayLanes {
    /// Build one lane per relay, all fetching the same URL.
    ///
    /// The relays are not contacted here: an unreachable one becomes a lane
    /// that fails its probe, which the selector parks. Checking them up front
    /// would mean a phone that woke a second late cost the whole transfer a
    /// path it could have used.
    pub fn new(relays: &[Relay], url: &str, config: &HttpConfig) -> Result<Self> {
        if relays.is_empty() {
            return Err(Error::Transport("no relays configured".into()));
        }
        let mut sources = Vec::with_capacity(relays.len());
        let mut labels = Vec::with_capacity(relays.len());
        for relay in relays {
            let config =
                HttpConfig { proxy: ProxyMode::Manual(relay.address.clone()), ..config.clone() };
            sources.push(HttpSource::with_config(&config, url)?);
            labels.push(relay.name.clone());
        }
        Ok(Self { sources, labels })
    }
}

impl LaneSet for RelayLanes {
    fn len(&self) -> usize {
        self.sources.len()
    }

    fn source(&self, lane: usize) -> &dyn ByteSource {
        &self.sources[lane]
    }

    fn label(&self, lane: usize) -> &str {
        &self.labels[lane]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_lane_per_relay_keeps_their_names() {
        let relays = [Relay::new("Pixel", "127.0.0.1:1"), Relay::new("Spare", "127.0.0.1:2")];
        let lanes = RelayLanes::new(&relays, "http://example.test/x", &HttpConfig::default())
            .expect("lanes build without contacting anything");
        assert_eq!(lanes.len(), 2);
        assert_eq!(lanes.label(0), "Pixel");
        assert_eq!(lanes.label(1), "Spare");
    }

    #[test]
    fn no_relays_is_an_error_rather_than_an_empty_lane_set() {
        // An empty `LaneSet` would read as "nothing to do" and finish a
        // transfer that never started.
        assert!(RelayLanes::new(&[], "http://example.test/x", &HttpConfig::default()).is_err());
    }

    #[test]
    fn a_relay_that_is_down_still_produces_its_lane() {
        // Built, not probed: a phone that woke a second late should not cost
        // the transfer a path it could have used. The selector parks lanes
        // that fail their probe.
        let relays = [Relay::new("Asleep", "127.0.0.1:9")];
        assert!(RelayLanes::new(&relays, "http://example.test/x", &HttpConfig::default()).is_ok());
    }
}
