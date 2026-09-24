//! Three ways to find a phone, all ending at the same `/braid/hello`.
//!
//! None of them involve Android platform tools. A USB-tethered phone is the
//! other end of an ordinary IP link and is that link's gateway, so a desktop
//! reaches it by talking to the gateway: no `adb`, no SDK, and nothing that
//! puts a developer-mode toggle between a person and their own download.

use crate::control::{self, Hello};
use crate::iface::InterfaceProvider;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

/// The port the companion listens on.
///
/// Fixed, because a tether link carries no service discovery and nobody should
/// have to find out a port number to use their own phone.
pub const RELAY_PORT: u16 = 8710;

/// What the companion advertises and what this browses for.
pub const SERVICE_TYPE: &str = "_braid-relay._tcp.local.";

/// Something that answered, and what it said.
#[derive(Clone, Debug)]
pub struct Candidate {
    pub address: String,
    pub hello: Hello,
}

/// Addresses worth trying on a cable.
///
/// Every interface with a gateway offers that gateway. Most will be routers
/// and will refuse, which costs one request each; missing the right one costs
/// the whole USB feature, so the trade is heavily one-sided.
pub fn tether_candidates(provider: &dyn InterfaceProvider) -> Vec<String> {
    provider
        .interfaces()
        .into_iter()
        .filter(|interface| !interface.is_loopback)
        .flat_map(|interface| {
            // Both families. A phone tethering on an IPv6-only carrier creates
            // a link carrying no IPv4 at all, so an IPv4-only search finds
            // nothing on exactly the networks this feature is most useful on.
            let v4 = interface.gateway_ipv4.map(IpAddr::from);
            let v6 = interface.gateway_ipv6.map(IpAddr::from);
            [v4, v6].into_iter().flatten()
        })
        // The same rule as discovery: an address that needs a scope id is not
        // one we can build a URL from, and `netdev` reports an unspecified
        // `fe80::` for interfaces that have no gateway at all, which would
        // otherwise fill the candidate list with a dozen copies of nothing.
        .filter(|gateway| !is_link_local(gateway) && !is_unspecified(gateway))
        .map(|gateway| endpoint(gateway, RELAY_PORT))
        .collect()
}

fn is_unspecified(address: &IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => v4.is_unspecified(),
        IpAddr::V6(v6) => v6.is_unspecified(),
    }
}

/// An address and port as a URL authority.
///
/// Through `SocketAddr` rather than by formatting, because an IPv6 literal has
/// to be bracketed: `2409:db8::1:8710` is ambiguous and unusable, and
/// `[2409:db8::1]:8710` is what every URL parser and proxy setting expects.
fn endpoint(address: IpAddr, port: u16) -> String {
    SocketAddr::new(address, port).to_string()
}

/// Addresses that only mean something with a scope attached.
fn is_link_local(address: &IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => v4.is_link_local(),
        IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) == 0xfe80,
    }
}

/// Browse the local network for companions.
///
/// Returns only what answered `/braid/hello`, because an mDNS record can
/// outlive the thing that published it.
pub async fn browse(timeout: Duration) -> Vec<Candidate> {
    let Ok(daemon) = mdns_sd::ServiceDaemon::new() else { return Vec::new() };
    let Ok(receiver) = daemon.browse(SERVICE_TYPE) else { return Vec::new() };

    let mut addresses = Vec::new();
    let deadline = tokio::time::Instant::now() + timeout;
    while let Ok(Ok(event)) = tokio::time::timeout_at(deadline, receiver.recv_async()).await {
        if let mdns_sd::ServiceEvent::ServiceResolved(info) = event {
            for address in info.get_addresses() {
                // A link-local address would need its scope id to be usable,
                // and a scope id is only meaningful on the machine that
                // produced it. Skip them: a companion advertising a global or
                // site address is reachable, and one advertising only
                // link-local is a problem to report rather than to guess at.
                let address = address.to_ip_addr();
                if is_link_local(&address) {
                    tracing::debug!(%address, "ignoring a link-local relay address");
                    continue;
                }
                addresses.push(endpoint(address, info.get_port()));
            }
        }
    }
    let _ = daemon.shutdown();

    confirm(&addresses).await
}

/// Ask each address who it is, and keep the ones that answer.
///
/// Discovery produces addresses; only `/braid/hello` makes one a relay. A
/// tether gateway is usually a router, and a printer will happily accept a
/// connection on any port.
pub async fn confirm(addresses: &[String]) -> Vec<Candidate> {
    let mut found: Vec<Candidate> = Vec::new();
    for address in addresses {
        // One phone can be reachable at several addresses at once: over Wi-Fi
        // and down a cable, or on two of its own interfaces. The device id is
        // the identity, so the second sighting is dropped rather than shown as
        // a second phone.
        if let Ok(hello) = control::hello(address).await
            && !found.iter().any(|c| c.hello.device_id == hello.device_id)
        {
            found.push(Candidate { address: address.clone(), hello });
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iface::{FakeInterfaces, Interface, InterfaceKind};

    #[test]
    fn an_ipv6_relay_address_is_bracketed() {
        // `2409:db8::1:8710` is ambiguous and unusable as a URL authority, and
        // an IPv6-only network is exactly where a phone's mobile data is worth
        // borrowing, so getting this wrong breaks the feature where it matters
        // most.
        let address: IpAddr = "2409:40e4:2004:769a::9ea5".parse().unwrap();
        assert_eq!(endpoint(address, RELAY_PORT), "[2409:40e4:2004:769a::9ea5]:8710");
    }

    #[test]
    fn an_ipv4_relay_address_is_not_bracketed() {
        let address: IpAddr = "192.168.42.129".parse().unwrap();
        assert_eq!(endpoint(address, RELAY_PORT), "192.168.42.129:8710");
    }

    #[test]
    fn a_tether_link_with_only_ipv6_is_still_probed() {
        // A phone tethering on an IPv6-only carrier creates a link with no
        // IPv4 on it at all. Looking only at the IPv4 gateway finds nothing.
        let mut interface = interface("en5", None, false);
        interface.gateway_ipv6 = Some("fd00::1".parse().unwrap());
        let found = tether_candidates(&FakeInterfaces(vec![interface]));
        assert_eq!(found, vec!["[fd00::1]:8710".to_string()]);
    }

    #[test]
    fn a_dual_stack_tether_link_offers_both() {
        let mut interface = interface("en5", Some("192.168.42.129"), false);
        interface.gateway_ipv6 = Some("fd00::1".parse().unwrap());
        let found = tether_candidates(&FakeInterfaces(vec![interface]));
        assert_eq!(found, vec!["192.168.42.129:8710".to_string(), "[fd00::1]:8710".to_string()]);
    }

    #[test]
    fn a_link_local_gateway_is_not_a_candidate() {
        // Measured: on a hotspot, netdev reports the router's link-local as
        // the gateway, and reports an unspecified fe80:: for every interface
        // that has none. Probing either wastes a request and, worse, fills the
        // list with addresses no URL parser will accept.
        let mut router = interface("en0", None, false);
        router.gateway_ipv6 = Some("fe80::6057:c8ff:fe2e:2c64".parse().unwrap());
        let mut empty = interface("utun0", None, false);
        empty.gateway_ipv6 = Some("::".parse().unwrap());
        assert!(tether_candidates(&FakeInterfaces(vec![router, empty])).is_empty());
    }

    #[test]
    fn link_local_addresses_are_recognised() {
        // Advertised by mDNS constantly, and unusable without a scope id that
        // only means something on the machine that produced it.
        assert!(is_link_local(&"fe80::1".parse().unwrap()));
        assert!(is_link_local(&"169.254.1.1".parse().unwrap()));
        assert!(!is_link_local(&"2409:40e4:2004:769a::1".parse().unwrap()));
        assert!(!is_link_local(&"192.168.1.1".parse().unwrap()));
    }

    fn interface(name: &str, gateway: Option<&str>, loopback: bool) -> Interface {
        Interface {
            name: name.into(),
            index: 1,
            ipv4: vec!["192.168.42.100".parse().unwrap()],
            ipv6: vec![],
            is_up: true,
            is_loopback: loopback,
            has_gateway: gateway.is_some(),
            gateway_ipv4: gateway.map(|g| g.parse().unwrap()),
            gateway_ipv6: None,
            kind: InterfaceKind::default(),
            service_name: None,
        }
    }

    #[test]
    fn a_tether_link_offers_its_gateway() {
        // A phone sharing over USB is the other end of that link, so its
        // gateway address is where the companion listens.
        let provider = FakeInterfaces(vec![interface("en5", Some("192.168.42.129"), false)]);
        assert_eq!(tether_candidates(&provider), vec!["192.168.42.129:8710".to_string()]);
    }

    #[test]
    fn an_interface_with_no_gateway_offers_nothing() {
        let provider = FakeInterfaces(vec![interface("utun0", None, false)]);
        assert!(tether_candidates(&provider).is_empty());
    }

    #[test]
    fn loopback_is_never_a_phone() {
        let provider = FakeInterfaces(vec![interface("lo0", Some("127.0.0.1"), true)]);
        assert!(tether_candidates(&provider).is_empty());
    }

    #[tokio::test]
    async fn a_candidate_is_only_a_candidate_until_it_says_hello() {
        // Discovery finds addresses; this is what tells a phone from a router.
        let phone = dl_testkit::Relay::spawn_phone("Pixel", Vec::new()).await.unwrap();
        let confirmed = confirm(&[phone.addr().to_string(), "127.0.0.1:9".into()]).await;
        assert_eq!(confirmed.len(), 1);
        assert_eq!(confirmed[0].hello.name, "Pixel");
    }

    #[tokio::test]
    async fn one_phone_reachable_twice_is_still_one_phone() {
        // Over Wi-Fi and down a cable at the same time. Showing it twice would
        // invite someone to pair the same device against itself.
        let phone = dl_testkit::Relay::spawn_phone("Pixel", Vec::new()).await.unwrap();
        let address = phone.addr().to_string();
        let confirmed = confirm(&[address.clone(), address]).await;
        assert_eq!(confirmed.len(), 1);
    }
}
