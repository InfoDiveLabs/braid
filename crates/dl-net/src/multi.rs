//! One HTTP client per network interface.
//!
//! Each client's connections leave by a chosen interface, so a download can use
//! Wi-Fi, Ethernet and a tether at once. Which mechanism actually took effect
//! is recorded per lane: a silent fall back to source-address binding looks
//! exactly like success until the combined throughput fails to exceed the
//! fastest single interface.

use crate::http::{HttpConfig, HttpSource};
use crate::iface::{Interface, InterfaceProvider};
use dl_core::error::{Error, Result};
use dl_core::lane::LaneSet;
use dl_core::source::ByteSource;

/// How outbound connections on a lane are pinned to its interface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Binding {
    /// The platform's interface-scoping socket option is in effect.
    Scoped,
    /// Only the source address is bound, which does not constrain routing.
    AddressOnly,
    /// Nothing is pinned; this lane uses the default route.
    Default,
}

impl Binding {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Scoped => "scoped",
            Self::AddressOnly => "address-only",
            Self::Default => "default-route",
        }
    }

    /// Whether this lane's traffic is genuinely constrained to its interface.
    pub fn is_authoritative(self) -> bool {
        matches!(self, Self::Scoped)
    }
}

pub struct InterfaceLane {
    pub interface: Interface,
    pub binding: Binding,
    source: HttpSource,
}

impl InterfaceLane {
    pub fn label(&self) -> String {
        format!("{} ({})", self.interface.name, self.binding.as_str())
    }
}

/// A set of interface-bound lanes, all fetching the same URL.
pub struct InterfaceLanes {
    lanes: Vec<InterfaceLane>,
    labels: Vec<String>,
}

impl InterfaceLanes {
    /// Build one lane per interface. Interfaces that cannot produce a working
    /// client are skipped rather than failing the whole download.
    pub fn new(interfaces: &[Interface], url: &str, config: &HttpConfig) -> Result<Self> {
        let mut lanes = Vec::new();
        for interface in interfaces {
            match build_lane(interface, url, config) {
                Ok(lane) => lanes.push(lane),
                Err(e) => {
                    tracing::warn!(
                        interface = %interface.name,
                        error = %e,
                        "skipping an interface that could not be bound"
                    );
                }
            }
        }

        if lanes.is_empty() {
            return Err(Error::Transport("none of the selected interfaces could be used".into()));
        }
        let labels = lanes.iter().map(InterfaceLane::label).collect();
        Ok(Self { lanes, labels })
    }

    /// Build lanes from every usable interface the system reports.
    pub fn from_system(
        provider: &dyn InterfaceProvider,
        url: &str,
        config: &HttpConfig,
    ) -> Result<Self> {
        let usable: Vec<Interface> = provider
            .usable()
            .into_iter()
            .filter(|i| i.has_gateway && i.has_routable_address())
            .collect();
        Self::new(&usable, url, config)
    }

    /// Select interfaces by name, erroring on any that does not exist so a
    /// typo cannot silently downgrade a download to one interface.
    pub fn from_names(
        provider: &dyn InterfaceProvider,
        names: &[String],
        url: &str,
        config: &HttpConfig,
    ) -> Result<Self> {
        let mut chosen = Vec::new();
        for name in names {
            let found = provider
                .by_name(name)
                .ok_or_else(|| Error::Transport(format!("no interface named {name:?}")))?;
            chosen.push(found);
        }
        Self::new(&chosen, url, config)
    }

    pub fn lanes(&self) -> &[InterfaceLane] {
        &self.lanes
    }

    /// Whether every lane is genuinely pinned to its own interface.
    ///
    /// When false, the lanes may all be leaving by the same route, and any
    /// apparent aggregation is just extra connections.
    pub fn all_scoped(&self) -> bool {
        self.lanes.iter().all(|l| l.binding.is_authoritative())
    }
}

impl std::fmt::Debug for InterfaceLanes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InterfaceLanes").field("lanes", &self.labels).finish()
    }
}

impl LaneSet for InterfaceLanes {
    fn len(&self) -> usize {
        self.lanes.len()
    }

    fn source(&self, lane: usize) -> &dyn ByteSource {
        &self.lanes[lane].source
    }

    fn label(&self, lane: usize) -> &str {
        &self.labels[lane]
    }
}

fn build_lane(interface: &Interface, url: &str, config: &HttpConfig) -> Result<InterfaceLane> {
    let (client, binding) = bound_client(interface, config)?;
    Ok(InterfaceLane {
        interface: interface.clone(),
        binding,
        source: HttpSource::new(client, url),
    })
}

/// Build a client whose connections leave by `interface`.
///
/// `reqwest::ClientBuilder::interface` issues `SO_BINDTODEVICE` on Linux and
/// `IP_BOUND_IF` on Apple platforms, which genuinely constrain routing. It does
/// not exist on Windows, where only the source address can be bound through
/// reqwest: and a source-address bind does not constrain routing on any of
/// these platforms. That difference is reported rather than hidden.
pub(crate) fn bound_client(
    interface: &Interface,
    config: &HttpConfig,
) -> Result<(reqwest::Client, Binding)> {
    let mut builder = crate::http::client_builder(config);

    if let Some(address) = interface.routable_ipv4() {
        builder = builder.local_address(std::net::IpAddr::V4(address));
    }

    #[cfg(any(
        target_os = "android",
        target_os = "fuchsia",
        target_os = "illumos",
        target_os = "ios",
        target_os = "linux",
        target_os = "macos",
        target_os = "solaris",
        target_os = "tvos",
        target_os = "visionos",
        target_os = "watchos",
    ))]
    {
        let client = builder
            .interface(&interface.name)
            .build()
            .map_err(|e| Error::Transport(e.to_string()))?;
        Ok((client, Binding::Scoped))
    }

    #[cfg(not(any(
        target_os = "android",
        target_os = "fuchsia",
        target_os = "illumos",
        target_os = "ios",
        target_os = "linux",
        target_os = "macos",
        target_os = "solaris",
        target_os = "tvos",
        target_os = "visionos",
        target_os = "watchos",
    )))]
    {
        let binding =
            if interface.ipv4.is_empty() { Binding::Default } else { Binding::AddressOnly };
        let client = builder.build().map_err(|e| Error::Transport(e.to_string()))?;
        Ok((client, binding))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iface::FakeInterfaces;
    use std::net::Ipv4Addr;

    fn iface(name: &str, index: u32, addr: &str, gateway: bool) -> Interface {
        Interface {
            name: name.into(),
            index,
            ipv4: vec![addr.parse::<Ipv4Addr>().unwrap()],
            ipv6: vec![],
            is_up: true,
            is_loopback: false,
            has_gateway: gateway,
            kind: Default::default(),
            service_name: None,
        }
    }

    #[test]
    fn a_lane_is_built_for_each_named_interface() {
        let provider = FakeInterfaces(vec![
            iface("en0", 1, "10.0.0.2", true),
            iface("en5", 2, "10.0.1.2", true),
        ]);
        let lanes = InterfaceLanes::from_names(
            &provider,
            &["en0".into(), "en5".into()],
            "http://example.test/f",
            &HttpConfig::default(),
        )
        .unwrap();

        assert_eq!(lanes.len(), 2);
        assert!(lanes.label(0).starts_with("en0"));
        assert!(lanes.label(1).starts_with("en5"));
    }

    #[test]
    fn a_misspelled_interface_is_an_error_rather_than_a_silent_downgrade() {
        // Quietly dropping it would leave the user believing they were
        // aggregating across interfaces when they were not.
        let provider = FakeInterfaces(vec![iface("en0", 1, "10.0.0.2", true)]);
        let err = InterfaceLanes::from_names(
            &provider,
            &["en0".into(), "typo0".into()],
            "http://example.test/f",
            &HttpConfig::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("typo0"), "{err}");
    }

    #[test]
    fn interfaces_without_a_gateway_are_not_used_automatically() {
        // An address without a route cannot reach the internet, however "up"
        // the interface claims to be.
        let provider = FakeInterfaces(vec![
            iface("en0", 1, "10.0.0.2", true),
            iface("en9", 2, "169.254.1.1", false),
        ]);
        let lanes =
            InterfaceLanes::from_system(&provider, "http://example.test/f", &HttpConfig::default())
                .unwrap();
        assert_eq!(lanes.len(), 1);
        assert!(lanes.label(0).starts_with("en0"));
    }

    #[test]
    fn having_no_usable_interface_is_an_error() {
        let provider = FakeInterfaces(vec![]);
        assert!(
            InterfaceLanes::from_system(&provider, "http://x.test/f", &HttpConfig::default())
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_lanes_report_scoped_binding() {
        let provider = FakeInterfaces(vec![iface("lo0", 1, "127.0.0.1", true)]);
        let lanes = InterfaceLanes::from_names(
            &provider,
            &["lo0".into()],
            "http://example.test/f",
            &HttpConfig::default(),
        )
        .unwrap();
        assert_eq!(lanes.lanes()[0].binding, Binding::Scoped);
        assert!(lanes.all_scoped());
    }
}
