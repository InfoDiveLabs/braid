//! Every way a lane can reach the origin, in one list.
//!
//! The engine asks for a `LaneSet` and weighs whatever it is given. It does
//! not care whether a lane is bound to a local card or points at a phone, so
//! there is no reason for those to be different types: a transfer using
//! Ethernet, Wi-Fi and two phones at once is the entire point of this project,
//! and that is only expressible if they share one list.
//!
//! One flat `Vec` rather than a set of sets. Nesting them would mean an outer
//! lane index and an inner one, and mapping between two index spaces is the
//! mistake that already cost this project a bug in the inspector's tabs.

use crate::http::{HttpConfig, HttpSource, ProxyMode};
use crate::iface::InterfaceProvider;
use crate::multi::InterfaceLanes;
use crate::relay::Relay;
use dl_core::error::{Error, Result};
use dl_core::lane::LaneSet;
use dl_core::source::ByteSource;

/// How one lane reaches the origin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Path {
    /// Whatever the routing table decides.
    Default,
    /// Bound to a local interface by name.
    Interface(String),
    /// Through a relay, over one of the networks it offers.
    Relay { relay: Relay, network: String },
}

impl Path {
    /// What the sidebar shows.
    ///
    /// The relay case names the device and which of its networks this is: a
    /// phone contributing two lanes would otherwise show one label twice, and
    /// the whole reason to show throughput per lane is to tell them apart.
    pub fn label(&self, resolved: Option<&str>) -> String {
        match self {
            Self::Default => resolved.unwrap_or("default route").to_string(),
            Self::Interface(name) => resolved.unwrap_or(name).to_string(),
            Self::Relay { relay, network } => format!("{} ({network})", relay.name),
        }
    }
}

/// The lanes a transfer will use, one per path.
pub struct PathLanes {
    sources: Vec<HttpSource>,
    labels: Vec<String>,
}

impl PathLanes {
    /// Build one lane per path.
    ///
    /// Nothing is contacted here. A relay that is asleep, or an interface that
    /// just went down, becomes a lane that fails its first request and is
    /// parked, which costs one request. Probing up front would cost every
    /// transfer a round trip to every path before it could start, and a phone
    /// that woke a second late would lose a path it could have served.
    pub fn build(
        paths: &[Path],
        url: &str,
        config: &HttpConfig,
        provider: &dyn InterfaceProvider,
    ) -> Result<Self> {
        if paths.is_empty() {
            return Err(Error::Transport("no paths to the origin".into()));
        }

        let mut sources = Vec::with_capacity(paths.len());
        let mut labels = Vec::with_capacity(paths.len());

        for path in paths {
            match path {
                Path::Default => {
                    sources.push(HttpSource::with_config(config, url)?);
                    labels.push(path.label(None));
                }
                Path::Interface(name) => {
                    // Reuses the binding matrix rather than repeating it. That
                    // code is the least portable part of this project and the
                    // only part that cannot be checked on the machine it was
                    // written on, so it should exist exactly once.
                    let bound = InterfaceLanes::from_names(
                        provider,
                        std::slice::from_ref(name),
                        url,
                        config,
                    )?;
                    let lane = bound.into_lanes().pop().ok_or_else(|| {
                        Error::Transport(format!("no usable interface named {name:?}"))
                    })?;
                    // The binding actually achieved, not the one asked for: a
                    // silent fall back to a plain source-address bind has to be
                    // visible rather than assumed.
                    labels.push(path.label(Some(&lane.label())));
                    sources.push(lane.into_source());
                }
                Path::Relay { relay, network } => {
                    let config = HttpConfig {
                        proxy: ProxyMode::Manual(relay.proxy_url(network)),
                        ..config.clone()
                    };
                    sources.push(HttpSource::with_config(&config, url)?);
                    labels.push(path.label(None));
                }
            }
        }

        Ok(Self { sources, labels })
    }
}

impl LaneSet for PathLanes {
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
    use crate::iface::FakeInterfaces;

    fn relay() -> Relay {
        Relay::new("Pixel", "127.0.0.1:1", Some("key".into()))
    }

    fn build(paths: &[Path]) -> Result<PathLanes> {
        PathLanes::build(
            paths,
            "http://example.test/x",
            &HttpConfig::default(),
            &FakeInterfaces(Vec::new()),
        )
    }

    #[test]
    fn one_lane_per_path_in_the_order_given() {
        let paths = vec![
            Path::Relay { relay: relay(), network: "cell".into() },
            Path::Relay { relay: relay(), network: "wifi".into() },
        ];
        let lanes = build(&paths).unwrap();
        assert_eq!(lanes.len(), 2);
        assert_eq!(lanes.label(0), "Pixel (cell)");
        assert_eq!(lanes.label(1), "Pixel (wifi)");
    }

    #[test]
    fn a_phone_contributes_as_many_lanes_as_it_offers() {
        // The USB case: one cable, two of the phone's networks, two lanes the
        // scheduler weighs independently.
        let paths = vec![
            Path::Default,
            Path::Relay { relay: relay(), network: "cell".into() },
            Path::Relay { relay: relay(), network: "wifi".into() },
        ];
        assert_eq!(build(&paths).unwrap().len(), 3);
    }

    #[test]
    fn no_paths_is_an_error_rather_than_an_empty_lane_set() {
        // An empty LaneSet reads as "nothing to do" and would finish a
        // transfer that never started.
        assert!(build(&[]).is_err());
    }

    #[test]
    fn a_relay_that_is_down_still_produces_its_lane() {
        // Built, not probed. The selector parks a lane that fails.
        let paths = vec![Path::Relay { relay: relay(), network: "cell".into() }];
        assert!(build(&paths).is_ok());
    }

    #[test]
    fn an_interface_that_does_not_exist_is_an_error() {
        // A typo must not silently downgrade a transfer to fewer paths than
        // the person chose.
        assert!(build(&[Path::Interface("nope0".into())]).is_err());
    }

    #[tokio::test]
    async fn a_relay_lane_really_goes_through_the_relay() {
        // The assertion that matters. Without counting the hop this would only
        // prove that a client was built.
        let phone = dl_testkit::Relay::spawn().await.unwrap();
        let origin =
            dl_testkit::Origin::spawn(dl_testkit::Scenario::Ok200 { size: 4096 }).await.unwrap();

        let relay = Relay::new("Pixel", phone.addr().to_string(), None);
        let paths = vec![Path::Relay { relay, network: "cell".into() }];
        let lanes = PathLanes::build(
            &paths,
            &origin.url("payload.bin"),
            &HttpConfig::default(),
            &FakeInterfaces(Vec::new()),
        )
        .unwrap();

        lanes.source(0).probe().await.expect("the relay forwards");
        assert!(phone.forwarded() > 0, "the request did not go through the relay");
    }

    #[tokio::test]
    async fn a_paired_key_satisfies_the_phone() {
        // End to end for pairing: the key pairing handed back, carried as
        // proxy credentials, is what stops the 407.
        let phone =
            dl_testkit::Relay::spawn_paired_phone("Pixel", Vec::new(), "secret").await.unwrap();
        let origin =
            dl_testkit::Origin::spawn(dl_testkit::Scenario::Ok200 { size: 4096 }).await.unwrap();

        let relay = Relay::new("Pixel", phone.addr().to_string(), Some("secret".into()));
        let paths = vec![Path::Relay { relay, network: "cell".into() }];
        let lanes = PathLanes::build(
            &paths,
            &origin.url("payload.bin"),
            &HttpConfig::default(),
            &FakeInterfaces(Vec::new()),
        )
        .unwrap();

        lanes.source(0).probe().await.expect("a paired desktop is served");
    }

    #[tokio::test]
    async fn an_unpaired_desktop_is_refused_by_a_phone_that_wants_a_key() {
        // The other half of the same claim. If this ever passes, the phone is
        // an open proxy and anyone nearby can spend its data.
        let phone =
            dl_testkit::Relay::spawn_paired_phone("Pixel", Vec::new(), "secret").await.unwrap();
        let origin =
            dl_testkit::Origin::spawn(dl_testkit::Scenario::Ok200 { size: 4096 }).await.unwrap();

        let relay = Relay::new("Pixel", phone.addr().to_string(), None);
        let paths = vec![Path::Relay { relay, network: "cell".into() }];
        let lanes = PathLanes::build(
            &paths,
            &origin.url("payload.bin"),
            &HttpConfig::default(),
            &FakeInterfaces(Vec::new()),
        )
        .unwrap();

        assert!(lanes.source(0).probe().await.is_err(), "an unpaired desktop was served");
    }
}
