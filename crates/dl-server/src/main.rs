// Braid: a download manager that splits one file across every network path
// you have.
// Copyright (C) 2026 InfoDive Labs Pvt Ltd. <https://www.infodivelabs.com>
//
// This program is free software: you can redistribute it and/or modify it
// under the terms of the GNU General Public License version 3, as published
// by the Free Software Foundation.
//
// This program is distributed in the hope that it will be useful, but WITHOUT
// ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or
// FITNESS FOR A PARTICULAR PURPOSE. See the GNU General Public License for
// more details.
//
// You should have received a copy of the GNU General Public License along
// with this program. If not, see <https://www.gnu.org/licenses/>.

//! `braid-server`: the download engine behind an HTTP API, with no window to
//! own the main thread. What `dl-gui` wires up for a desktop event loop, this
//! wires up for a container: same engine, same network layer, same torrent
//! backend, answering requests instead of drawing a UI.

mod config;
mod state;

use anyhow::Result;
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use config::Config;
use dl_core::budget::Budget;
use dl_core::engine::{DownloadSpec, Engine, SourceFactory};
use dl_core::lane::LaneSet;
use dl_net::iface::InterfaceProvider as _;
use dl_net::path::{Path as NetPath, PathLanes};
use dl_net::{HttpConfig, SystemInterfaces};
use state::AppState;
use std::path::PathBuf;
use std::sync::Arc;

const DEFAULT_LOG: &str = "info";

/// Builds the network paths for each download.
///
/// Deliberately simpler than `dl-gui`'s equivalent: there is no settings
/// window here to pin a transfer to one interface or to pair a phone, so
/// every transfer gets the single path the OS would pick anyway. Interface
/// selection can be added once there is an API request that carries the
/// choice; inventing the plumbing before there is a caller for it would be
/// guessing at a shape nobody has asked for yet.
struct HeadlessFactory {
    /// What to call a transfer whose lane the OS chose rather than us. See
    /// `dl_core::EngineConfig::unattributed_lane` for why this is not the
    /// literal string "default".
    default_lane: String,
}

impl SourceFactory for HeadlessFactory {
    fn lanes_for(&self, spec: &DownloadSpec) -> dl_core::Result<Box<dyn LaneSet>> {
        let paths = vec![NetPath::Default(self.default_lane.clone())];
        let lanes = PathLanes::build(&paths, &spec.url, &HttpConfig::default(), &SystemInterfaces)?;
        Ok(Box::new(lanes))
    }
}

/// The interface the OS will route over when none was requested: the first
/// usable one with a gateway. Falls back to a neutral label rather than
/// guessing, so nothing here claims a NIC that is not carrying anything.
fn default_route_interface(interfaces: &[dl_net::Interface]) -> String {
    interfaces
        .iter()
        .find(|i| i.has_gateway)
        .map(|i| i.name.clone())
        .unwrap_or_else(|| "default".into())
}

/// The shape of a `/health` response. A struct rather than an inline
/// `serde_json::json!` call, so a field added or renamed here is a compile
/// error at every place that reads one back, including the API tests later
/// tasks will add against it.
#[derive(serde::Serialize)]
struct Health {
    status: &'static str,
    active_downloads: usize,
    port: u16,
}

/// `GET /health`. Reports that the process answers requests and that the
/// engine it holds is the same one still running, not just that a socket
/// somewhere accepted a connection: a process wedged on a poisoned lock
/// inside the engine would still accept the TCP connection but never reach
/// this far to answer it.
async fn health(State(state): State<AppState>) -> (StatusCode, Json<Health>) {
    let active = state.engine.snapshot().len();
    (
        StatusCode::OK,
        Json(Health { status: "ok", active_downloads: active, port: state.config.web_port }),
    )
}

/// Wait for whichever shutdown signal arrives first.
///
/// SIGTERM is what a container runtime sends on `docker stop`, and it is a
/// request, not a kill: the runtime gives the process a grace period before
/// following up with SIGKILL. Exiting immediately on it would cut off
/// whatever axum's graceful shutdown would otherwise have let finish, which
/// for this server means an in-flight request rather than a download: the
/// journal on disk is what makes a transfer resumable, not a clean exit. But
/// treating a deliberate stop as though the machine had lost power, when
/// there was time to leave things tidy, is still worse than not needing that
/// safety net at all.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install a SIGTERM handler");
        sigterm.recv().await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("received Ctrl-C"),
        _ = terminate => tracing::info!("received SIGTERM"),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| DEFAULT_LOG.into()),
        )
        .init();

    // Where the config file itself lives is one level above what `Config`
    // describes: the directory has to be known before there is a `Config` to
    // ask, so it is read from the real process environment here and nowhere
    // else in this binary.
    let config_dir = std::env::var("BRAID_CONFIG_DIR").map(PathBuf::from).unwrap_or_else(|_| {
        // Kept in sync with `Config::default().config_dir` by hand: the two
        // exist for different reasons (this decides where to look; that
        // records what was found) and collapsing them would make `Config`
        // depend on I/O to construct.
        PathBuf::from("/config")
    });
    let config = Arc::new(Config::read(&config_dir, |key| std::env::var(key).ok()));

    let usable = SystemInterfaces.usable();
    let default_lane = default_route_interface(&usable);
    tracing::debug!(%default_lane, "unpinned transfers will be attributed here");

    let engine_config = dl_core::EngineConfig {
        max_concurrent: config.max_concurrent.max(1),
        ..Default::default()
    };
    let budget = match config.download_limit {
        Some(rate) => Budget::with_rate(rate),
        None => Budget::unlimited(),
    };
    let engine = Engine::new(Arc::new(HeadlessFactory { default_lane }), engine_config, budget);
    if let Some(rate) = config.upload_limit {
        engine.upload_budget().set_rate(rate);
    }

    engine.set_torrent_backend(Arc::new(dl_torrent::LibrqbitBackend::new(
        dl_torrent::SessionConfig {
            listen_port: Some(config.torrent_port),
            ..dl_torrent::SessionConfig::new(config.download_dir.clone())
        },
    )));

    // `dl-gui` restores its transfer list here through `dl_core::persist`.
    // That module does not exist in this worktree yet (another task is
    // adding transfer persistence separately), so restoration is skipped
    // rather than reimplemented: inventing a second, incompatible on-disk
    // format for the same job would leave the project with two of them to
    // reconcile later instead of one to finish.

    let state = AppState { engine, config: config.clone() };
    let app = axum::Router::new().route("/health", get(health)).with_state(state);

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", config.web_port)).await?;
    tracing::info!(port = config.web_port, "listening");
    axum::serve(listener, app).with_graceful_shutdown(shutdown_signal()).await?;

    Ok(())
}
