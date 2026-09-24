//! Three ways to find a phone, all ending at the same `/braid/hello`.
//!
//! None of them involve Android platform tools. A USB-tethered phone is the
//! other end of an ordinary IP link and is that link's gateway, so a desktop
//! reaches it by talking to the gateway: no `adb`, no SDK, and nothing that
//! puts a developer-mode toggle between a person and their own download.

use crate::control::{self, Hello};
use crate::iface::InterfaceProvider;
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
        .filter_map(|interface| interface.gateway_ipv4)
        .map(|gateway| format!("{gateway}:{RELAY_PORT}"))
        .collect()
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
                addresses.push(format!("{address}:{}", info.get_port()));
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
