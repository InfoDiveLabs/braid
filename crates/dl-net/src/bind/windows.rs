//! Windows interface binding via `IP_UNICAST_IF`.
//!
//! **Unverified on real hardware.** Neither `socket2` nor `hyper-util` wraps
//! this option, so it is issued directly through `windows-sys`, written against
//! Microsoft's `IPPROTO_IP` documentation and matching shadowsocks-rust,
//! clash-rs and EasyTier. Treat Windows multi-interface aggregation as best
//! effort until confirmed.
//!
//! Source-address binding cannot work here: Vista and later default to the
//! strong host model, so binding address A while the route selects interface B
//! fails the send rather than rerouting.
//!
//! The byte-order asymmetry below is a Winsock wart, not a mistake: IPv4 takes
//! the index in network byte order, IPv6 in host order.

use super::{BindError, BindMechanism, Family};
use crate::iface::Interface;
use socket2::Socket;
use std::os::windows::io::AsRawSocket;
use windows_sys::Win32::Networking::WinSock::{
    IP_UNICAST_IF, IPPROTO_IP, IPPROTO_IPV6, IPV6_UNICAST_IF, SOCKET_ERROR, WSAGetLastError,
    setsockopt,
};

pub(super) fn bind_device(
    socket: &Socket,
    interface: &Interface,
    family: Family,
) -> Result<BindMechanism, BindError> {
    if interface.index == 0 {
        return Err(BindError::InvalidIndex { name: interface.name.clone() });
    }

    let (level, option, value) = match family {
        // IPv4 wants the index in network byte order.
        Family::V4 => (IPPROTO_IP, IP_UNICAST_IF, interface.index.to_be()),
        // IPv6 wants it in host byte order.
        Family::V6 => (IPPROTO_IPV6, IPV6_UNICAST_IF, interface.index),
    };

    let bytes = value.to_ne_bytes();
    // SAFETY: `bytes` is a live 4-byte buffer for the duration of the call, and
    // both options are documented as taking a DWORD.
    let rc = unsafe {
        setsockopt(socket.as_raw_socket() as _, level, option, bytes.as_ptr(), bytes.len() as i32)
    };

    if rc == SOCKET_ERROR {
        let code = unsafe { WSAGetLastError() };
        return Err(BindError::Syscall {
            name: interface.name.clone(),
            mechanism: "IP_UNICAST_IF",
            source: std::io::Error::from_raw_os_error(code),
        });
    }

    Ok(BindMechanism::UnicastIf)
}
