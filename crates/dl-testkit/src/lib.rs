//! Mock origin, deterministic payloads, and fakes for the engine's traits.
//!
//! A normal crate rather than a dev-dependency so the `devtools` build can
//! embed the scenario runner and use the same catalogue the tests run against.

pub mod faults;
pub mod fixtures;
pub mod origin;
pub mod relay;
pub mod scenario;

pub use faults::{Fault, FaultyFile};
pub use origin::Origin;
pub use relay::Relay;
pub use scenario::{SEED, Scenario};
