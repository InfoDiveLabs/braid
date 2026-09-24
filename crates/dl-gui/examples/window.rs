//! Screenshot fixture: the real window, driven by a real engine against a
//! local mock origin.
//!
//! Deliberately not sample data any more. A screenshot of hand-written rows
//! proves only that the renderer works; this exercises the engine, the bridge
//! and the UI together, so a break anywhere in that chain shows up in the
//! captured image.

use dl_core::budget::Budget;
use dl_core::engine::{DownloadSpec, Engine, EngineConfig, SourceFactory};
use dl_core::lane::LaneSet;
use dl_core::source::ByteSource;
use dl_gui::{MainWindow, bridge};
use dl_net::{HttpConfig, HttpSource};
use dl_testkit::{Origin, Scenario};
use slint::ComponentHandle as _;
use std::sync::Arc;

struct Factory;

/// Several lanes over loopback, standing in for several NICs.
///
/// The machine this runs on has one routable interface, so the capture would
/// otherwise show a single band and prove nothing about the part of the UI that
/// exists to display aggregation.
struct Lanes {
    sources: Vec<HttpSource>,
    labels: Vec<String>,
}

impl LaneSet for Lanes {
    fn len(&self) -> usize {
        self.sources.len()
    }
    fn source(&self, lane: usize) -> &dyn ByteSource {
        &self.sources[lane]
    }
    fn label(&self, lane: usize) -> &str {
        &self.labels[lane]
    }
}

const LANES: [&str; 3] = ["Wi-Fi", "Ethernet", "USB Tether"];

/// A swarm that reports but never finishes, so the capture catches a torrent
/// mid-flight. The same idea as the mock origin the HTTP rows use: it drives
/// the real rendering path without a real network.
struct FakeSwarm;

#[async_trait::async_trait]
impl dl_core::torrent::TorrentBackend for FakeSwarm {
    async fn run(
        &self,
        request: dl_core::torrent::TorrentRequest,
    ) -> dl_core::Result<dl_core::torrent::TorrentOutcome> {
        let total = 13_100_000_000u64;
        let files = vec![
            dl_core::torrent::TorrentFile {
                path: "debian-12.5.0-amd64-DVD-1.iso".into(),
                len: 12_600_000_000,
                downloaded: 8_190_000_000,
            },
            dl_core::torrent::TorrentFile {
                path: "MD5SUMS".into(),
                len: 400_000,
                downloaded: 400_000,
            },
            dl_core::torrent::TorrentFile {
                path: "README.txt".into(),
                len: 4_000,
                downloaded: 800,
            },
        ];
        let mut done = 8_100_000_000u64;
        loop {
            request.cancel.check()?;
            done = (done + 24_000_000).min(total - 1);
            if let Some(report) = &request.on_progress {
                report(dl_core::torrent::TorrentProgress {
                    progress: dl_core::Progress {
                        downloaded: done,
                        total: Some(total),
                        bytes_per_sec: 46 << 20,
                        smoothed_bytes_per_sec: 46 << 20,
                    },
                    status: dl_core::torrent::TorrentStatus {
                        uploaded: 3_900_000_000,
                        upload_bytes_per_sec: 12 << 20,
                        peers: 61,
                        files: files.clone(),
                        peer_list: Vec::new(),
                        interface: Some("Ethernet".into()),
                    },
                    name: Some("debian-12.5-DVD".into()),
                    seeding: false,
                    phase: None,
                });
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    }

    async fn discard(
        &self,
        _source: &dl_core::torrent::TorrentSource,
        _delete_files: bool,
    ) -> dl_core::Result<()> {
        Ok(())
    }
}

impl SourceFactory for Factory {
    fn lanes_for(&self, spec: &DownloadSpec) -> dl_core::Result<Box<dyn LaneSet>> {
        let sources = LANES
            .iter()
            .map(|_| HttpSource::with_config(&HttpConfig::default(), &spec.url))
            .collect::<dl_core::Result<Vec<_>>>()?;
        Ok(Box::new(Lanes { sources, labels: LANES.iter().map(|l| l.to_string()).collect() }))
    }
}

fn main() -> anyhow::Result<()> {
    let runtime = tokio::runtime::Runtime::new()?;
    let _guard = runtime.enter();

    let dir = tempfile::tempdir()?;
    let engine = Engine::new(
        Arc::new(Factory),
        EngineConfig { max_concurrent: 3, chunk_size: Some(256 << 10), ..Default::default() },
        Budget::unlimited(),
    );

    // Throttled so the capture catches transfers mid-flight rather than a list
    // of finished rows.
    let files: Vec<(&str, u64, u64)> = vec![
        ("ubuntu-24.04.2-desktop-amd64.iso", 48 << 20, 3 << 20),
        ("Blender-4.2-macOS-arm64.dmg", 32 << 20, 2 << 20),
        ("imagenet-subset.tar.zst", 64 << 20, 1 << 20),
    ];

    if !std::env::args().any(|a| a == "--empty") {
        engine.set_torrent_backend(Arc::new(FakeSwarm));
        let mut spec = DownloadSpec::new(
            "magnet:?xt=urn:btih:1f2e3d4c5b6a798877665544332211aabbccddee&dn=debian-12.5-DVD",
            dir.path(),
        );
        spec.connections = 1;
        engine.add(spec);
    }

    let mut origins = Vec::new();
    // `--empty` captures the state a first-run user actually sees.
    let empty = std::env::args().any(|a| a == "--empty");
    for (name, size, rate) in if empty { Vec::new() } else { files } {
        let origin =
            runtime.block_on(Origin::spawn(Scenario::SlowStream { size, bytes_per_sec: rate }))?;
        let mut spec = DownloadSpec::new(origin.url(name), dir.path().join(name));
        spec.connections = 4;
        engine.add(spec);
        origins.push(origin);
    }

    let ui = MainWindow::new()?;
    ui.set_destination(dir.path().to_string_lossy().to_string().into());
    if std::env::args().any(|a| a == "--light") {
        ui.set_dark(false);
    }
    bridge::spawn(
        &ui,
        engine,
        None,
        std::sync::Arc::new(std::sync::RwLock::new(
            LANES
                .iter()
                .zip(["wifi", "ethernet", "cellular"])
                .map(|(l, icon)| dl_gui::bridge::InterfaceInfo {
                    id: l.to_string(),
                    label: l.to_string(),
                    icon: icon.to_string(),
                })
                .collect(),
        )),
        std::sync::Arc::new(std::sync::RwLock::new(dl_gui::settings::Settings::new(
            std::env::temp_dir(),
        ))),
    );
    ui.run()?;

    drop(origins);
    Ok(())
}
