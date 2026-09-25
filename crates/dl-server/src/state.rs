use crate::config::Config;
use dl_core::Engine;
use std::sync::Arc;

/// What every handler needs. `Engine` is already `Arc`-backed internally, so
/// cloning this to hand a copy to each request costs an atomic increment, not
/// a copy of the download table.
#[derive(Clone)]
pub struct AppState {
    pub engine: Engine,
    pub config: Arc<Config>,
}
