//! Network interface enumeration.
//!
//! The engine only sees [`InterfaceProvider`], so selection, assignment and
//! failover are testable against synthetic interfaces with no hardware.

use std::net::{Ipv4Addr, Ipv6Addr};

/// What kind of link an interface is, coarsely.
///
/// Coarse on purpose: the UI needs an icon and a word, not the forty-odd
/// distinctions the operating system draws. Derived from the OS's own report
/// and **never from the device name**: `en0` is Wi-Fi on a laptop and
/// Ethernet on a Mac mini, so a name-prefix table would be wrong on exactly
/// the machines that matter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum InterfaceKind {
    WiFi,
    Ethernet,
    Cellular,
    Vpn,
    Bridge,
    Loopback,
    #[default]
    Other,
}

impl InterfaceKind {
    /// What to call this kind when the system offers no name of its own.
    pub fn label(self) -> &'static str {
        match self {
            Self::WiFi => "Wi-Fi",
            Self::Ethernet => "Ethernet",
            Self::Cellular => "Cellular",
            Self::Vpn => "VPN",
            Self::Bridge => "Bridge",
            Self::Loopback => "Loopback",
            Self::Other => "Network",
        }
    }

    /// Icon key, matched in the UI's glyph set.
    pub fn icon(self) -> &'static str {
        match self {
            Self::WiFi => "wifi",
            Self::Ethernet => "ethernet",
            Self::Cellular => "cellular",
            Self::Vpn => "vpn",
            Self::Bridge => "bridge",
            Self::Loopback => "loopback",
            Self::Other => "network",
        }
    }
}

/// A network interface we may bind outbound connections to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Interface {
    /// The device id: `en0`, `eth0`, `utun3`.
    ///
    /// **This is the identity.** It keys `SO_BINDTODEVICE`/`IP_BOUND_IF`, the
    /// saved interface selection and per-interface limits, and the lane labels
    /// the engine matches against. The display name below is presentation on
    /// top of it and must never become the key: someone renaming an adapter
    /// in System Settings would otherwise silently lose their saved limits.
    pub name: String,
    /// Kernel interface index, required by `IP_BOUND_IF` and `IP_UNICAST_IF`.
    /// Zero is never valid.
    pub index: u32,
    pub ipv4: Vec<Ipv4Addr>,
    pub ipv6: Vec<Ipv6Addr>,
    pub is_up: bool,
    pub is_loopback: bool,
    /// A NIC with an address but no gateway cannot reach the internet, however
    /// "up" it claims to be.
    pub has_gateway: bool,
    /// The IPv6 gateway, when the system reports one.
    ///
    /// Kept beside the IPv4 rather than instead of it: a phone tethering on an
    /// IPv6-only carrier creates a link with no IPv4 on it at all, so looking
    /// only at `gateway_ipv4` finds nothing to probe.
    pub gateway_ipv6: Option<Ipv6Addr>,
    /// The gateway itself, when the system reports one.
    ///
    /// Kept rather than reduced to the flag above because a USB-tethered phone
    /// *is* the gateway of the link it creates, so this is the address the
    /// companion is listening on. Discovering it this way needs no Android
    /// platform tools: ordinary tethering already set the route up.
    pub gateway_ipv4: Option<Ipv4Addr>,
    pub kind: InterfaceKind,
    /// The system's own name for it: "Wi-Fi", "Thunderbolt 1": when it has
    /// one that says more than the device id does.
    pub service_name: Option<String>,
}

impl Interface {
    /// A bare interface, for tests and for providers with nothing more to say.
    pub fn named(name: impl Into<String>, index: u32) -> Self {
        Self {
            name: name.into(),
            index,
            ipv4: Vec::new(),
            ipv6: Vec::new(),
            is_up: true,
            is_loopback: false,
            has_gateway: false,
            gateway_ipv4: None,
            gateway_ipv6: None,
            kind: InterfaceKind::default(),
            service_name: None,
        }
    }

    /// What to call this interface: the system's name for it, or its kind.
    pub fn display_name(&self) -> String {
        match self.service_name.as_deref() {
            Some(service) if useful_service_name(service, &self.name) => service.trim().to_string(),
            _ => self.kind.label().to_string(),
        }
    }

    /// The one spelling used everywhere an interface is shown: `Wi-Fi (en0)`.
    ///
    /// One helper rather than four call sites formatting it their own way, so
    /// the sidebar, the settings table, the add sheet and the Inspector cannot
    /// drift apart.
    pub fn display_label(&self) -> String {
        let name = self.display_name();
        if name.eq_ignore_ascii_case(&self.name) { name } else { format!("{name} ({})", self.name) }
    }

    /// A structural filter, not a liveness check: a NIC can pass this and still
    /// sit behind a captive portal.
    pub fn is_usable(&self) -> bool {
        self.is_up && !self.is_loopback && self.has_routable_address()
    }

    /// Whether the interface has an address that can reach beyond the link.
    ///
    /// Link-local addresses do not count. VPN and tunnel devices routinely
    /// carry nothing but an `fe80::` address while still reporting themselves
    /// as up with a gateway, and binding a download to one of those produces a
    /// connection that simply times out.
    pub fn has_routable_address(&self) -> bool {
        self.ipv4.iter().any(|a| !a.is_link_local() && !a.is_unspecified())
            || self.ipv6.iter().any(is_routable_v6)
    }

    /// The first address usable as a source for outbound traffic.
    pub fn routable_ipv4(&self) -> Option<std::net::Ipv4Addr> {
        self.ipv4.iter().copied().find(|a| !a.is_link_local() && !a.is_unspecified())
    }
}

/// Whether the system's own name for an interface says anything the device id
/// does not.
///
/// macOS hands back "Ethernet Adapter (en3)" for a nameless adapter, which
/// would otherwise render as "Ethernet Adapter (en3) (en3)". Anything that
/// already contains the device id is the system padding out a blank rather
/// than a name someone chose.
fn useful_service_name(service: &str, device: &str) -> bool {
    let service = service.trim();
    !service.is_empty()
        && !service.eq_ignore_ascii_case(device)
        && !service.to_ascii_lowercase().contains(&device.to_ascii_lowercase())
}

/// `fe80::/10` is link-local; `::` and `::1` are not usable sources here.
fn is_routable_v6(addr: &Ipv6Addr) -> bool {
    let segments = addr.segments();
    let link_local = segments[0] & 0xffc0 == 0xfe80;
    !link_local && !addr.is_unspecified() && !addr.is_loopback()
}

pub trait InterfaceProvider: Send + Sync {
    fn interfaces(&self) -> Vec<Interface>;

    fn usable(&self) -> Vec<Interface> {
        self.interfaces().into_iter().filter(Interface::is_usable).collect()
    }

    fn by_name(&self, name: &str) -> Option<Interface> {
        self.interfaces().into_iter().find(|i| i.name == name)
    }
}

/// Enumerates the machine's real interfaces via `netdev`.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemInterfaces;

impl InterfaceProvider for SystemInterfaces {
    fn interfaces(&self) -> Vec<Interface> {
        netdev::get_interfaces()
            .into_iter()
            .map(|i| Interface {
                is_loopback: i.is_loopback(),
                is_up: i.is_up(),
                has_gateway: i.gateway.is_some(),
                gateway_ipv4: i.gateway.as_ref().and_then(|g| g.ipv4.first().copied()),
                gateway_ipv6: i.gateway.as_ref().and_then(|g| g.ipv6.first().copied()),
                kind: kind_of(i.if_type),
                service_name: i.friendly_name.clone(),
                name: i.name,
                index: i.index,
                ipv4: i.ipv4.into_iter().map(|n| n.addr()).collect(),
                ipv6: i.ipv6.into_iter().map(|n| n.addr()).collect(),
            })
            .collect()
    }
}

/// Map the operating system's own classification onto the coarse one above.
fn kind_of(reported: netdev::interface::types::InterfaceType) -> InterfaceKind {
    use netdev::interface::types::InterfaceType as T;
    match reported {
        T::Wireless80211 => InterfaceKind::WiFi,
        T::Ethernet
        | T::Ethernet3Megabit
        | T::FastEthernetT
        | T::FastEthernetFx
        | T::GigabitEthernet => InterfaceKind::Ethernet,
        // A USB tether presents as Ethernet, not as cellular; these are the
        // machine's own modem.
        T::Wwan | T::Wwanpp | T::Wwanpp2 | T::Wman => InterfaceKind::Cellular,
        T::Tunnel | T::Ppp => InterfaceKind::Vpn,
        T::Bridge => InterfaceKind::Bridge,
        T::Loopback => InterfaceKind::Loopback,
        _ => InterfaceKind::Other,
    }
}

/// A fixed list of interfaces, for tests.
#[derive(Clone, Debug, Default)]
pub struct FakeInterfaces(pub Vec<Interface>);

impl InterfaceProvider for FakeInterfaces {
    fn interfaces(&self) -> Vec<Interface> {
        self.0.clone()
    }
}

#[cfg(test)]
mod naming_tests {
    use super::*;

    fn iface(name: &str, kind: InterfaceKind, service: Option<&str>) -> Interface {
        Interface { kind, service_name: service.map(str::to_string), ..Interface::named(name, 1) }
    }

    #[test]
    fn the_systems_own_name_is_preferred_over_the_kind() {
        let wifi = iface("en0", InterfaceKind::WiFi, Some("Wi-Fi"));
        assert_eq!(wifi.display_label(), "Wi-Fi (en0)");

        let tb = iface("en1", InterfaceKind::Ethernet, Some("Thunderbolt 1"));
        assert_eq!(tb.display_label(), "Thunderbolt 1 (en1)");
    }

    #[test]
    fn a_padded_out_name_does_not_repeat_the_device_id() {
        // macOS names a nameless adapter "Ethernet Adapter (en3)", which would
        // otherwise render as "Ethernet Adapter (en3) (en3)".
        let padded = iface("en3", InterfaceKind::Ethernet, Some("Ethernet Adapter (en3)"));
        assert_eq!(padded.display_label(), "Ethernet (en3)");
    }

    #[test]
    fn a_name_that_merely_repeats_the_device_falls_back_to_the_kind() {
        // "Ethernet (eth0)" says more than a bare "eth0".
        let linux = iface("eth0", InterfaceKind::Ethernet, Some("eth0"));
        assert_eq!(linux.display_label(), "Ethernet (eth0)");
    }

    #[test]
    fn an_interface_with_no_system_name_is_named_by_its_kind() {
        assert_eq!(iface("utun3", InterfaceKind::Vpn, None).display_label(), "VPN (utun3)");
        assert_eq!(iface("lo0", InterfaceKind::Loopback, None).display_label(), "Loopback (lo0)");
        assert_eq!(
            iface("eth9", InterfaceKind::Other, Some("   ")).display_label(),
            "Network (eth9)"
        );
    }

    #[test]
    fn the_label_never_doubles_up_when_the_name_is_the_device() {
        let bare = iface("tun0", InterfaceKind::Other, None);
        // "Network (tun0)", not "tun0 (tun0)".
        assert!(!bare.display_label().contains("tun0 (tun0)"));
    }

    #[test]
    fn the_device_id_is_untouched_by_any_of_this() {
        // It keys the socket option and the saved settings. A display name
        // that overwrote it would lose someone's per-interface limits.
        let wifi = iface("en0", InterfaceKind::WiFi, Some("Wi-Fi"));
        assert_eq!(wifi.name, "en0");
    }

    #[test]
    fn every_kind_has_a_label_and_an_icon() {
        for kind in [
            InterfaceKind::WiFi,
            InterfaceKind::Ethernet,
            InterfaceKind::Cellular,
            InterfaceKind::Vpn,
            InterfaceKind::Bridge,
            InterfaceKind::Loopback,
            InterfaceKind::Other,
        ] {
            assert!(!kind.label().is_empty());
            assert!(!kind.icon().is_empty());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iface(name: &str, index: u32, up: bool, loopback: bool, addrs: &[&str]) -> Interface {
        Interface {
            name: name.into(),
            index,
            ipv4: addrs.iter().map(|a| a.parse().unwrap()).collect(),
            ipv6: vec![],
            is_up: up,
            is_loopback: loopback,
            has_gateway: !loopback,
            gateway_ipv4: (!loopback).then(|| "10.0.0.1".parse().unwrap()),
            gateway_ipv6: None,
            kind: InterfaceKind::default(),
            service_name: None,
        }
    }

    #[test]
    fn link_local_only_interfaces_are_not_usable() {
        // VPN and tunnel devices commonly look up and routable while carrying
        // nothing but fe80::, and binding to one just times out.
        let tunnel = Interface {
            name: "utun4".into(),
            index: 20,
            ipv4: vec![],
            ipv6: vec!["fe80::1".parse().unwrap()],
            is_up: true,
            is_loopback: false,
            has_gateway: true,
            gateway_ipv4: None,
            gateway_ipv6: None,
            kind: InterfaceKind::default(),
            service_name: None,
        };
        assert!(!tunnel.is_usable());
        assert!(!tunnel.has_routable_address());

        let apipa = iface("en7", 7, true, false, &["169.254.10.1"]);
        assert!(!apipa.is_usable(), "a self-assigned address cannot route");

        let real = Interface {
            name: "utun5".into(),
            index: 21,
            ipv4: vec![],
            ipv6: vec!["2001:db8::1".parse().unwrap()],
            is_up: true,
            is_loopback: false,
            has_gateway: true,
            gateway_ipv4: None,
            gateway_ipv6: None,
            kind: InterfaceKind::default(),
            service_name: None,
        };
        assert!(real.is_usable(), "a tunnel with a global address is usable");
    }

    #[test]
    fn routable_ipv4_skips_link_local() {
        let mixed = iface("en0", 1, true, false, &["169.254.3.3", "10.0.0.5"]);
        assert_eq!(mixed.routable_ipv4(), Some("10.0.0.5".parse().unwrap()));
    }

    #[test]
    fn usable_excludes_loopback_down_and_addressless() {
        let p = FakeInterfaces(vec![
            iface("en0", 1, true, false, &["10.0.0.2"]),
            iface("lo0", 2, true, true, &["127.0.0.1"]),
            iface("en1", 3, false, false, &["10.0.1.2"]),
            iface("en2", 4, true, false, &[]),
        ]);
        let names: Vec<_> = p.usable().into_iter().map(|i| i.name).collect();
        assert_eq!(names, ["en0"]);
    }

    #[test]
    fn by_name_finds_down_interfaces_too() {
        let p = FakeInterfaces(vec![iface("en1", 3, false, false, &["10.0.1.2"])]);
        assert!(p.by_name("en1").is_some());
        assert!(p.by_name("nope").is_none());
    }
}
