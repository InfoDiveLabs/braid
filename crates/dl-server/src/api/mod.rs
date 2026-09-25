//! Braid's own API, `/api/v1`: torrents and HTTP downloads through one
//! surface, because the web UI has no reason to know which kind of transfer
//! it is looking at until it wants the torrent-only fields.
//!
//! A qBittorrent-compatible API is a separate, later concern and has nothing
//! to do with this module: that surface exists for other tools to talk to,
//! this one is what the UI itself speaks.
//!
//! Nothing in the binary calls [`routes`] yet: `main.rs` is a file three
//! agents touch at once, so wiring this in is left for afterwards rather than
//! risked as a merge conflict. Until then this whole module is unreachable
//! from `main`, and every item in it would otherwise be flagged as dead code
//! for that reason alone rather than for actually being unused.
#![allow(dead_code)]

mod events;
mod v1;

use crate::state::AppState;
use axum::Router;

/// Every route this crate answers, mounted as one router the binary attaches
/// to its own. Kept as a single entry point so the binary's own route table
/// stays a one-line merge rather than a list that has to be kept in step with
/// whatever this module adds next.
pub fn routes() -> Router<AppState> {
    Router::new().merge(v1::routes()).merge(events::routes())
}
