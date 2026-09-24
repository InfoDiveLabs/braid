//! Sockets, network interfaces, and HTTP transport.
//!
//! This is the only crate that knows interfaces exist. `dl-core` reaches it
//! through traits so the engine is testable with no real sockets.

pub mod bind;
pub mod control;
pub mod http;
pub mod iface;
pub mod multi;
pub mod path;
pub mod refreshing;
pub mod relay;

pub use bind::{BindError, BindMechanism, Family, bind_to_interface};
pub use http::{HttpConfig, HttpSource, ProxyMode, build_client, filename_from_url, retry_after};
pub use iface::{FakeInterfaces, Interface, InterfaceKind, InterfaceProvider, SystemInterfaces};
pub use multi::{Binding, InterfaceLanes};
pub use refreshing::{
    HttpFetcher, LaneSpec, RefresherFor, RefreshingLanes, ReqwestJson, lane_specs,
};
pub use relay::Relay;
