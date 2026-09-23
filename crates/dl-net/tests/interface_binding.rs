//! Does interface binding actually constrain routing, or does the kernel
//! silently ignore it?
//!
//! A `setsockopt` returning `Ok` proves nothing: the failure mode we care about
//! is an option that is accepted and then has no effect, which looks exactly
//! like success until bandwidth fails to aggregate. So every test here asserts
//! on *observed behaviour*: either the source address the peer actually saw, or
//! a connection that must fail because the binding forbids the route.
//!
//! These run with no privileges and no special hardware. The privileged
//! counterparts (synthetic `dummy`/`feth` interfaces) live behind the
//! `privileged-tests` feature.

use dl_net::{Family, InterfaceProvider, SystemInterfaces, bind_to_interface};
use socket2::{Domain, Protocol, Socket, Type};
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// A throwaway TCP server that reports the source address it observed.
/// This is the same trick the CI multi-interface job uses: the peer, not the
/// client, is the authority on which address the packets actually came from.
struct ObservingServer {
    addr: SocketAddr,
    handle: Option<std::thread::JoinHandle<Option<IpAddr>>>,
}

impl ObservingServer {
    fn bind(addr: SocketAddr) -> std::io::Result<Self> {
        let listener = TcpListener::bind(addr)?;
        let addr = listener.local_addr()?;
        let handle = std::thread::spawn(move || {
            let (mut stream, peer) = listener.accept().ok()?;
            let mut buf = [0u8; 16];
            let _ = stream.read(&mut buf);
            let _ = stream.write_all(b"ok");
            Some(peer.ip())
        });
        Ok(Self { addr, handle: Some(handle) })
    }

    fn observed_source(mut self) -> Option<IpAddr> {
        self.handle.take().and_then(|h| h.join().ok()).flatten()
    }
}

fn tcp_socket_v4() -> Socket {
    Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).expect("creating socket")
}

#[test]
fn loopback_binding_reaches_loopback() {
    let provider = SystemInterfaces;
    let Some(lo) = provider.interfaces().into_iter().find(|i| i.is_loopback && !i.ipv4.is_empty())
    else {
        eprintln!("skipping: no loopback interface with an IPv4 address");
        return;
    };

    let server = ObservingServer::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap();
    let target = server.addr;

    let socket = tcp_socket_v4();
    let mechanism = bind_to_interface(&socket, &lo, Family::V4).expect("binding to loopback");
    socket.connect_timeout(&target.into(), CONNECT_TIMEOUT).expect("connecting via loopback");

    let mut stream: std::net::TcpStream = socket.into();
    stream.write_all(b"hello").unwrap();
    drop(stream);

    assert_eq!(
        server.observed_source(),
        Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        "the peer should have seen the loopback source address"
    );
    println!("bound to {} via {}", lo.name, mechanism.as_str());
}

/// The load-bearing assertion: the binding must actually *constrain* routing.
///
/// A socket pinned to the loopback interface cannot reach the public internet.
/// An unbound socket can, so if the bound one also succeeds, the option was
/// accepted and ignored: precisely the silent failure this design must rule
/// out. Skipped when the machine is offline.
///
/// Note the direction. The mirror-image test (pin to a real NIC, try to reach
/// 127.0.0.1) does *not* work: macOS delivers to local addresses regardless of
/// `IP_BOUND_IF`, so it succeeds and proves nothing. Loopback destinations are
/// not a valid negative control.
#[test]
fn loopback_binding_cannot_reach_the_internet() {
    let provider = SystemInterfaces;
    let Some(lo) = provider.interfaces().into_iter().find(|i| i.is_loopback && !i.ipv4.is_empty())
    else {
        eprintln!("skipping: no loopback interface with an IPv4 address");
        return;
    };

    // A public anycast resolver on 443: routable from anywhere with internet,
    // and never a local address.
    let external = SocketAddr::from((Ipv4Addr::new(1, 1, 1, 1), 443));

    // Control: unbound, this must succeed, or the machine is offline.
    let control = tcp_socket_v4();
    if control.connect_timeout(&external.into(), CONNECT_TIMEOUT).is_err() {
        eprintln!("skipping: no internet connectivity for the control connection");
        return;
    }
    drop(control);

    let socket = tcp_socket_v4();
    let mechanism = bind_to_interface(&socket, &lo, Family::V4).expect("binding to loopback");
    let result = socket.connect_timeout(&external.into(), CONNECT_TIMEOUT);

    if !mechanism.is_authoritative() {
        eprintln!("skipping assertion: {} does not constrain routing", mechanism.as_str());
        return;
    }
    let err = result.expect_err(&format!(
        "a socket bound to {} via {} still reached the internet: the option was accepted \
         but had no effect on routing",
        lo.name,
        mechanism.as_str()
    ));
    println!(
        "{} is enforced: binding to {} blocked the route ({err})",
        mechanism.as_str(),
        lo.name
    );
}

/// Binding to a real NIC must produce that NIC's address as the observed source.
#[test]
fn binding_selects_the_expected_source_address() {
    let provider = SystemInterfaces;
    let Some(nic) = provider.usable().into_iter().find(|i| i.has_gateway && !i.ipv4.is_empty())
    else {
        eprintln!("skipping: no usable IPv4 interface with a gateway on this machine");
        return;
    };
    let expected = nic.ipv4[0];

    // Listen on the NIC's own address so the route stays within that interface.
    let Ok(server) = ObservingServer::bind(SocketAddr::from((expected, 0))) else {
        eprintln!("skipping: could not bind a listener to {expected}");
        return;
    };
    let target = server.addr;

    let socket = tcp_socket_v4();
    let mechanism = bind_to_interface(&socket, &nic, Family::V4).expect("binding to NIC");
    if socket.connect_timeout(&target.into(), CONNECT_TIMEOUT).is_err() {
        eprintln!("skipping: {} could not reach its own address", nic.name);
        return;
    }
    let mut stream: std::net::TcpStream = socket.into();
    let _ = stream.write_all(b"hello");
    drop(stream);

    assert_eq!(
        server.observed_source(),
        Some(IpAddr::V4(expected)),
        "peer observed a different source address than the interface we bound to"
    );
    println!("{} via {} egressed as {expected}", nic.name, mechanism.as_str());
}

/// On macOS, `IP_BOUND_IF` must be the mechanism in play. If this ever reports
/// `local-address-only`, multi-interface aggregation is silently broken.
#[cfg(target_os = "macos")]
#[test]
fn macos_uses_ip_bound_if() {
    let provider = SystemInterfaces;
    let Some(nic) = provider.usable().into_iter().find(|i| !i.ipv4.is_empty()) else {
        eprintln!("skipping: no usable IPv4 interface");
        return;
    };
    let socket = tcp_socket_v4();
    let mechanism = bind_to_interface(&socket, &nic, Family::V4).expect("binding to NIC");
    assert_eq!(mechanism, dl_net::BindMechanism::BoundIf);
    assert!(mechanism.is_authoritative());
}

/// Isolate `IP_BOUND_IF` from the source-address bind.
///
/// `bind_to_interface` does two things, and the test above cannot tell which one
/// blocked the route: binding a loopback *source address* and then connecting
/// outward would fail on its own with `EADDRNOTAVAIL`, with no help from the
/// socket option. So here the scoping option is applied alone, with no address
/// bind at all. If the connection still fails, only `IP_BOUND_IF` can be
/// responsible.
#[cfg(target_os = "macos")]
#[test]
fn ip_bound_if_constrains_routing_without_any_address_bind() {
    let provider = SystemInterfaces;
    let Some(lo) = provider.interfaces().into_iter().find(|i| i.is_loopback) else {
        eprintln!("skipping: no loopback interface");
        return;
    };
    let external = SocketAddr::from((Ipv4Addr::new(1, 1, 1, 1), 443));

    let control = tcp_socket_v4();
    if control.connect_timeout(&external.into(), CONNECT_TIMEOUT).is_err() {
        eprintln!("skipping: no internet connectivity for the control connection");
        return;
    }
    drop(control);

    // The socket option and nothing else.
    let socket = tcp_socket_v4();
    socket
        .bind_device_by_index_v4(Some(std::num::NonZeroU32::new(lo.index).unwrap()))
        .expect("IP_BOUND_IF should be accepted");

    let err = socket
        .connect_timeout(&external.into(), CONNECT_TIMEOUT)
        .expect_err("IP_BOUND_IF alone failed to constrain the route");

    // A routing refusal, not an address-assignment complaint: the latter would
    // mean a source bind was responsible after all.
    let kind = err.raw_os_error();
    assert_ne!(
        kind,
        Some(libc::EADDRNOTAVAIL),
        "got EADDRNOTAVAIL, which indicates a source-address problem rather than route scoping"
    );
    println!("IP_BOUND_IF alone blocked the route: {err} (errno {kind:?})");
}
