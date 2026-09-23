//! Forcing an outbound TCP connection out of a chosen network interface.
//!
//! Binding the source address is not sufficient on any of the three platforms:
//! route selection follows the destination, so a socket carrying interface A's
//! address can still egress via B. Each OS has a dedicated socket option:
//!
//! | OS      | Option                        | Privileges |
//! |---------|-------------------------------|------------|
//! | Linux   | `SO_BINDTODEVICE`             | none (≥5.7)|
//! | macOS   | `IP_BOUND_IF` / `IPV6_BOUND_IF` | none     |
//! | Windows | `IP_UNICAST_IF` / `IPV6_UNICAST_IF` | none |
//!
//! The mechanism that took effect is reported as a [`BindMechanism`]: a silent
//! fallback to address-only binding is indistinguishable from success until
//! throughput fails to aggregate.

use crate::iface::Interface;
use socket2::Socket;
use std::net::{IpAddr, SocketAddr};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(any(target_os = "ios", target_os = "macos", target_os = "tvos", target_os = "watchos"))]
mod macos;
#[cfg(windows)]
mod windows;

/// How an outbound socket ended up pinned to an interface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindMechanism {
    /// Linux `SO_BINDTODEVICE`. Constrains egress regardless of routing table.
    BindToDevice,
    /// macOS `IP_BOUND_IF`. Constrains scoped route selection.
    BoundIf,
    /// Windows `IP_UNICAST_IF`. Overrides route selection.
    UnicastIf,
    /// Only the source address was bound. Route selection is **not** constrained,
    /// so this may silently egress via a different interface.
    LocalAddressOnly,
}

impl BindMechanism {
    /// Whether this mechanism actually constrains which interface packets leave by.
    pub fn is_authoritative(self) -> bool {
        !matches!(self, Self::LocalAddressOnly)
    }

    /// What this platform will attempt before any socket exists.
    ///
    /// The authoritative answer comes from [`bind_to_interface`], which reports
    /// what actually took effect: a kernel too old for the scoping option
    /// falls back silently, and only the live call can see that. This is for
    /// showing the user what to expect, not for deciding anything.
    pub fn platform_default() -> Self {
        #[cfg(target_os = "linux")]
        {
            Self::BindToDevice
        }
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        {
            Self::BoundIf
        }
        #[cfg(target_os = "windows")]
        {
            Self::UnicastIf
        }
        #[cfg(not(any(
            target_os = "linux",
            target_os = "macos",
            target_os = "ios",
            target_os = "windows"
        )))]
        {
            Self::LocalAddressOnly
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::BindToDevice => "SO_BINDTODEVICE",
            Self::BoundIf => "IP_BOUND_IF",
            Self::UnicastIf => "IP_UNICAST_IF",
            Self::LocalAddressOnly => "local-address-only",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BindError {
    #[error("interface {name:?} has no {family} address to bind to")]
    NoAddress { name: String, family: &'static str },
    #[error("interface {name:?} has an invalid index (0)")]
    InvalidIndex { name: String },
    #[error("binding to interface {name:?} via {mechanism} failed: {source}")]
    Syscall { name: String, mechanism: &'static str, source: std::io::Error },
}

/// Pin `socket` to `interface` for traffic to an address of `family`.
///
/// Applies the scoping option, which constrains routing, and binds the source
/// address, which makes the choice explicit rather than the kernel's.
pub fn bind_to_interface(
    socket: &Socket,
    interface: &Interface,
    family: Family,
) -> Result<BindMechanism, BindError> {
    let mechanism = bind_device(socket, interface, family)?;

    // Best effort; the scoping option above does the load-bearing work.
    if let Some(addr) = source_address(interface, family) {
        let _ = socket.bind(&SocketAddr::new(addr, 0).into());
    }

    Ok(mechanism)
}

/// Interface scoping is a per-family socket option, so the family must be
/// known before binding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    V4,
    V6,
}

impl Family {
    pub fn of(addr: &IpAddr) -> Self {
        match addr {
            IpAddr::V4(_) => Self::V4,
            IpAddr::V6(_) => Self::V6,
        }
    }
}

fn source_address(interface: &Interface, family: Family) -> Option<IpAddr> {
    match family {
        Family::V4 => interface.ipv4.first().copied().map(IpAddr::V4),
        Family::V6 => interface
            .ipv6
            .iter()
            // Link-local addresses need a scope id to be usable as a source.
            .find(|a| a.segments()[0] != 0xfe80)
            .or_else(|| interface.ipv6.first())
            .copied()
            .map(IpAddr::V6),
    }
}

#[cfg(target_os = "linux")]
use linux::bind_device;
#[cfg(any(target_os = "ios", target_os = "macos", target_os = "tvos", target_os = "watchos"))]
use macos::bind_device;
#[cfg(windows)]
use windows::bind_device;

#[cfg(not(any(
    target_os = "linux",
    target_os = "ios",
    target_os = "macos",
    target_os = "tvos",
    target_os = "watchos",
    windows
)))]
fn bind_device(
    _socket: &Socket,
    interface: &Interface,
    family: Family,
) -> Result<BindMechanism, BindError> {
    // No scoping option known here; `is_authoritative()` reports the truth.
    if source_address(interface, family).is_none() {
        return Err(BindError::NoAddress {
            name: interface.name.clone(),
            family: if family == Family::V4 { "IPv4" } else { "IPv6" },
        });
    }
    Ok(BindMechanism::LocalAddressOnly)
}
