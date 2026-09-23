//! Linux interface binding via `SO_BINDTODEVICE`.
//!
//! Unprivileged since kernel 5.7 for a socket's first bind, and we always bind
//! a fresh socket once. Older kernels return `EPERM`; we degrade to
//! address-only binding and report that rather than claiming success.
//!
//! Source-address binding alone is not enough: with two default routes a socket
//! carrying ISP B's address can egress via ISP A and be dropped by egress
//! filtering, which hangs rather than errors.

use super::{BindError, BindMechanism, Family};
use crate::iface::Interface;
use socket2::Socket;

pub(super) fn bind_device(
    socket: &Socket,
    interface: &Interface,
    _family: Family,
) -> Result<BindMechanism, BindError> {
    match socket.bind_device(Some(interface.name.as_bytes())) {
        Ok(()) => Ok(BindMechanism::BindToDevice),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            tracing::warn!(
                interface = %interface.name,
                "SO_BINDTODEVICE denied (kernel older than 5.7?); falling back to \
                 source-address binding, which does not constrain routing"
            );
            Ok(BindMechanism::LocalAddressOnly)
        }
        Err(source) => Err(BindError::Syscall {
            name: interface.name.clone(),
            mechanism: "SO_BINDTODEVICE",
            source,
        }),
    }
}
