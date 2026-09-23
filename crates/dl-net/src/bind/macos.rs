//! macOS interface binding via `IP_BOUND_IF`. No privileges required.
//!
//! macOS uses scoped routing, so a source-address bind does not constrain the
//! outgoing interface. Verified enforced in `tests/interface_binding.rs`.
//!
//! `socket2::bind_device_by_index_v4/_v6` implements this; its rendered docs
//! list only Android and Linux, but the cfg covers Apple targets.
//!
//! A VPN with `includeAllNetworks` captures traffic regardless of this option.

use super::{BindError, BindMechanism, Family};
use crate::iface::Interface;
use socket2::Socket;

pub(super) fn bind_device(
    socket: &Socket,
    interface: &Interface,
    family: Family,
) -> Result<BindMechanism, BindError> {
    let index = std::num::NonZeroU32::new(interface.index)
        .ok_or_else(|| BindError::InvalidIndex { name: interface.name.clone() })?;

    let result = match family {
        Family::V4 => socket.bind_device_by_index_v4(Some(index)),
        Family::V6 => socket.bind_device_by_index_v6(Some(index)),
    };
    result.map_err(|source| BindError::Syscall {
        name: interface.name.clone(),
        mechanism: "IP_BOUND_IF",
        source,
    })?;

    Ok(BindMechanism::BoundIf)
}
