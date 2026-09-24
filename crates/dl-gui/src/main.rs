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

use anyhow::Result;
use dl_core::budget::Budget;
use dl_core::engine::{DownloadSpec, Engine, SourceFactory};
use dl_core::lane::LaneSet;
use dl_gui::{
    InterfaceSetting, MainWindow, RelayLaneRow, RelayRow, SettingsWindow, Tray, bridge, platform,
    relays, settings, transfers,
};
use dl_net::{HttpConfig, HttpSource, SystemInterfaces};
use slint::{ComponentHandle as _, Model as _};
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// Builds the network paths for each download.
///
/// Lives here rather than in `dl-core` so the engine stays free of HTTP and of
/// any notion of a network interface.
struct HttpFactory {
    /// Read fresh on every transfer, so changing the selection in Settings
    /// affects the next one without restarting anything.
    settings: settings::Shared,
    /// What to call a transfer that was not pinned to an interface.
    ///
    /// The OS picks the default route for us, so the honest label is the name
    /// of the interface it will pick. Calling it "default" invented a NIC that
    /// appeared in the sidebar beside the real ones, carrying all the traffic
    /// while the interface actually doing the work sat at zero.
    default_lane: String,
}

/// Which paths a transfer should use.
///
/// Pure, so the rule can be exercised without a network, an engine or a phone.
/// The rule: every selected interface, then every enabled lane of every paired
/// relay, and if that comes to nothing, the default route, because a transfer
/// with no lanes is a transfer that silently does not happen.
fn paths_for(
    interfaces: &[String],
    relays: &[relays::Paired],
    default_lane: &str,
) -> Vec<dl_net::path::Path> {
    use dl_net::path::Path;

    let mut paths: Vec<Path> =
        interfaces.iter().map(|name| Path::Interface(name.clone())).collect();

    for paired in relays {
        // A relay with no key has been seen, not authorised. Using it would
        // earn a 407 for every chunk before the lane was finally parked.
        if paired.relay.key.is_none() {
            continue;
        }
        for network in &paired.enabled {
            paths.push(Path::Relay { relay: paired.relay.clone(), network: network.clone() });
        }
    }

    if paths.is_empty() {
        // The label the sidebar will show for it, which is the interface the
        // OS will actually pick rather than an invented "default" NIC.
        tracing::debug!(%default_lane, "no path chosen; letting the OS route");
        paths.push(Path::Default);
    }
    paths
}

impl SourceFactory for HttpFactory {
    fn lanes_for(&self, spec: &DownloadSpec) -> dl_core::Result<Box<dyn LaneSet>> {
        // The spec wins if it named interfaces; otherwise the app-wide
        // selection applies. Read here rather than cached, so a change in
        // Settings reaches the next transfer without a restart.
        let (allowed, config) = {
            let current = self.settings.read().ok();
            let allowed = match (&current, spec.interfaces.is_empty()) {
                (Some(current), true) => current.interfaces.clone(),
                _ => spec.interfaces.clone(),
            };
            let config = current.map(|c| c.http_config()).unwrap_or_default();
            (allowed, config)
        };

        // Loaded per transfer for the same reason: a phone paired a minute ago
        // should be usable now, not after a restart.
        let paired = relays::load();
        let paths = paths_for(&allowed, &paired, &self.default_lane);

        Ok(Box::new(dl_net::path::PathLanes::build(&paths, &spec.url, &config, &SystemInterfaces)?))
    }
}

/// Register the torrent backend, if this build has one.
///
/// A no-op without the `torrent` feature, which is the whole point: the engine
/// then refuses a magnet with an error naming the missing feature instead of
/// pushing it down the HTTP path.
///
/// Binding is only applied when exactly one interface is selected. A torrent
/// cannot be spread across NICs the way a ranged download can: a swarm sees
/// one address: so with several chosen there is no honest answer but to let
/// the OS route, which is what it does.
///
/// Read once, here, rather than per transfer as the HTTP factory does: the
/// session owns the peer, DHT and tracker sockets and cannot rebind them
/// underneath a running swarm. A changed selection therefore reaches torrents
/// at the next launch, which `docs/ui-status.md` records.
#[cfg(feature = "torrent")]
fn install_torrent_backend(engine: &Engine, config: &settings::Shared) {
    let (folder, bind_device) = config
        .read()
        .map(|c| {
            let bind = match c.interfaces.as_slice() {
                [only] => Some(only.clone()),
                _ => None,
            };
            (c.destination.clone(), bind)
        })
        .unwrap_or_else(|_| (download_dir(), None));

    engine.set_torrent_backend(Arc::new(dl_torrent::LibrqbitBackend::new(
        dl_torrent::SessionConfig { bind_device, ..dl_torrent::SessionConfig::new(folder) },
    )));
}

#[cfg(not(feature = "torrent"))]
fn install_torrent_backend(_engine: &Engine, _config: &settings::Shared) {}

/// Whether the system is in dark mode.
fn dark_mode() -> bool {
    #[cfg(target_os = "macos")]
    {
        // `AppleInterfaceStyle` is absent entirely in light mode.
        if let Ok(output) = std::process::Command::new("defaults")
            .args(["read", "-g", "AppleInterfaceStyle"])
            .output()
        {
            return String::from_utf8_lossy(&output.stdout).trim() == "Dark";
        }
    }
    true
}

/// The interface the OS will route over when none was requested: the first
/// usable one with a gateway. Falls back to a neutral label rather than
/// guessing, so the sidebar never claims a NIC that is not carrying anything.
fn default_route_interface(interfaces: &[dl_net::Interface]) -> String {
    interfaces
        .iter()
        .find(|i| i.has_gateway)
        .map(|i| i.name.clone())
        .unwrap_or_else(|| "default".into())
}

fn download_dir() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .map(|home| std::path::PathBuf::from(home).join("Downloads"))
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

/// The sidebar entries for every switched-on lane of every paired phone.
///
/// Listed alongside the network cards so a phone's lane is visible at zero
/// rather than appearing only once it happens to carry something. The id must
/// be exactly the label `PathLanes` gives that lane, because the sidebar keys
/// its meters on the lane label the engine reports.
///
/// The glyph is the neutral one: the id a phone chooses for a network is
/// opaque, and guessing "cellular" from the word "cell" would put a mast icon
/// on someone's home Wi-Fi.
fn relay_lane_rows(paired: &[relays::Paired]) -> Vec<bridge::InterfaceInfo> {
    let mut rows = Vec::new();
    for entry in paired {
        if entry.relay.key.is_none() {
            continue;
        }
        for network in &entry.enabled {
            let path =
                dl_net::path::Path::Relay { relay: entry.relay.clone(), network: network.clone() };
            let label = path.label(None);
            rows.push(bridge::InterfaceInfo {
                id: label.clone(),
                label,
                icon: "network".to_string(),
            });
        }
    }
    rows
}

/// One phone as the scan found it.
///
/// Plain data on purpose. A Slint model is not `Send`, so nothing built from
/// one may cross a thread boundary: the scan returns this and the event loop
/// turns it into rows.
struct Found {
    name: String,
    address: String,
    paired: bool,
    reachable: bool,
    lanes: Vec<FoundLane>,
}

struct FoundLane {
    id: String,
    label: String,
    icon: &'static str,
    note: String,
    on: bool,
    duplicate: bool,
}

/// Build the model, on the thread that owns it.
fn to_rows(found: Vec<Found>) -> Vec<RelayRow> {
    found
        .into_iter()
        .map(|phone| {
            let lanes: Vec<RelayLaneRow> = phone
                .lanes
                .into_iter()
                .map(|lane| RelayLaneRow {
                    id: lane.id.into(),
                    label: lane.label.into(),
                    icon: lane.icon.into(),
                    note: lane.note.into(),
                    on: lane.on,
                    duplicate: lane.duplicate,
                })
                .collect();
            RelayRow {
                name: phone.name.into(),
                address: phone.address.into(),
                paired: phone.paired,
                reachable: phone.reachable,
                lanes: std::rc::Rc::new(slint::VecModel::from(lanes)).into(),
            }
        })
        .collect()
}

/// Everything discoverable right now, paired or not.
///
/// mDNS first, then the gateway of every link that might be a tethered phone.
/// Both end at `/braid/hello`, which is what separates a companion from a
/// router that happened to accept a connection.
async fn scan_for_relays() -> Vec<Found> {
    let mut found = dl_net::discovery::browse(std::time::Duration::from_secs(3)).await;
    let tethered = dl_net::discovery::tether_candidates(&dl_net::SystemInterfaces);
    for candidate in dl_net::discovery::confirm(&tethered).await {
        if !found.iter().any(|c| c.hello.device_id == candidate.hello.device_id) {
            found.push(candidate);
        }
    }

    let paired = relays::load();
    let ours = dl_net::control::host_egress(settings::EGRESS_SERVICE).await;
    let mut rows = Vec::new();

    for entry in &paired {
        let reachable = found.iter().any(|c| c.address == entry.relay.address);
        rows.push(row_for(entry, reachable, ours.as_deref()).await);
    }

    // Then anything discovered that is not paired yet, so there is something
    // to press Pair on.
    for candidate in found {
        if paired.iter().any(|p| p.relay.address == candidate.address) {
            continue;
        }
        rows.push(Found {
            name: candidate.hello.name,
            address: candidate.address,
            paired: false,
            reachable: true,
            lanes: Vec::new(),
        });
    }
    rows
}

/// One paired phone, with whatever it says it is offering.
async fn row_for(entry: &relays::Paired, reachable: bool, ours: Option<&str>) -> Found {
    let status = dl_net::control::status(&entry.relay.address, entry.relay.key.as_deref())
        .await
        .unwrap_or_default();
    let duplicates = dl_net::control::duplicates_of(&status, ours);

    let lanes: Vec<FoundLane> = status
        .lanes
        .iter()
        .map(|lane| FoundLane {
            id: lane.id.clone(),
            label: lane.label.clone(),
            icon: icon_for(lane.kind),
            note: lane.note.clone().unwrap_or_default(),
            on: entry.enabled.contains(&lane.id),
            duplicate: duplicates.iter().any(|d| d.id == lane.id),
        })
        .collect();

    Found {
        name: entry.relay.name.clone(),
        address: entry.relay.address.clone(),
        paired: entry.relay.key.is_some(),
        reachable,
        lanes,
    }
}

/// The list as it stands before anything is scanned, so opening the page shows
/// the phones already paired rather than an empty panel.
fn rows_for_paired(paired: &[relays::Paired]) -> Vec<RelayRow> {
    to_rows(
        paired
            .iter()
            .map(|entry| Found {
                name: entry.relay.name.clone(),
                address: entry.relay.address.clone(),
                paired: entry.relay.key.is_some(),
                // Unknown until something is asked, and claiming otherwise
                // would be a guess shown as a fact.
                reachable: false,
                lanes: Vec::new(),
            })
            .collect(),
    )
}

/// Which glyph a lane gets, reusing the interface icon keys so a phone's
/// cellular lane looks like a cellular card.
fn icon_for(kind: dl_net::control::LaneKind) -> &'static str {
    use dl_net::control::LaneKind;
    match kind {
        LaneKind::Cellular => "cellular",
        LaneKind::Wifi => "wifi",
        LaneKind::Ethernet => "ethernet",
        LaneKind::Unknown => "network",
    }
}

/// What to call this computer when asking a phone to trust it.
///
/// Shown on the phone's screen, so it has to mean something to the person
/// holding it.
fn whoami() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        })
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "A computer".to_string())
}

/// Build the settings window and connect every control in it.
///
/// The window is created once and shown again on each request rather than
/// rebuilt: a settings window that forgets which page you were on is a small
/// thing that feels broken.
fn wire_settings(
    ui: &MainWindow,
    config: settings::Shared,
    usable: Vec<dl_net::Interface>,
    runtime: tokio::runtime::Handle,
) {
    let window = match SettingsWindow::new() {
        Ok(window) => window,
        Err(error) => {
            tracing::error!(%error, "could not build the settings window");
            return;
        }
    };
    window.set_dark(ui.get_dark());

    let interfaces = std::rc::Rc::new(slint::VecModel::from(interface_rows(&usable, &config)));
    window.set_interfaces(interfaces.clone().into());

    let cells = std::rc::Rc::new(slint::VecModel::from(
        config
            .read()
            .map(|c| c.schedule_cells.clone())
            .unwrap_or_else(|_| vec![false; settings::SCHEDULE_CELLS]),
    ));
    window.set_schedule_cells(cells.clone().into());

    push_settings(&window, &config);

    // ------------------------------------------------------------- General

    window.on_choose_destination({
        let config = config.clone();
        let weak = window.as_weak();
        let main = ui.as_weak();
        move || {
            let Some(folder) = pick_folder(&config) else { return };
            settings::edit(&config, |c| c.destination = folder.clone());
            let shown: slint::SharedString = folder.to_string_lossy().to_string().into();
            if let Some(window) = weak.upgrade() {
                window.set_destination(shown.clone());
            }
            if let Some(main) = main.upgrade() {
                main.set_destination(shown);
            }
        }
    });

    macro_rules! toggle {
        ($setter:ident, $getter:ident, $field:ident) => {
            window.$setter({
                let config = config.clone();
                let weak = window.as_weak();
                move |on| {
                    settings::edit(&config, |c| c.$field = on);
                    if let Some(window) = weak.upgrade() {
                        window.$getter(on);
                    }
                }
            });
        };
    }

    toggle!(on_set_ask_where, set_ask_where, ask_where);
    toggle!(on_set_reveal_on_done, set_reveal_on_done, reveal_on_done);
    toggle!(on_set_sound_on_done, set_sound_on_done, sound_on_done);
    window.on_set_keep_completed({
        let config = config.clone();
        let weak = window.as_weak();
        let main = ui.as_weak();
        move |on| {
            settings::edit(&config, |c| c.keep_completed = on);
            if let Some(window) = weak.upgrade() {
                window.set_keep_completed(on);
            }
            // The list reads this directly, so the change shows on the next
            // tick rather than the next launch.
            if let Some(main) = main.upgrade() {
                main.set_keep_completed(on);
            }
        }
    });
    toggle!(on_set_menu_bar, set_menu_bar, menu_bar);
    toggle!(on_set_manual_ignores_limit, set_manual_ignores_limit, manual_ignores_limit);
    toggle!(on_set_verify_every, set_verify_every, verify_every);
    toggle!(on_set_refetch_damaged, set_refetch_damaged, refetch_damaged);
    toggle!(on_set_keep_partial, set_keep_partial, keep_partial);
    toggle!(on_set_refresh_links, set_refresh_links, refresh_links);

    window.on_set_launch_at_login({
        let config = config.clone();
        let weak = window.as_weak();
        move |on| {
            // The stored flag follows what the system agreed to, not what was
            // asked: a switch that stays on while nothing was registered is
            // the worst of both.
            let applied = platform::set_launch_at_login(on).unwrap_or_else(|error| {
                tracing::warn!(%error, "could not change the login item");
                !on
            });
            settings::edit(&config, |c| c.launch_at_login = applied);
            if let Some(window) = weak.upgrade() {
                window.set_launch_at_login(applied);
            }
        }
    });

    window.on_set_handle_magnets({
        let config = config.clone();
        let weak = window.as_weak();
        move |on| {
            // Same rule as the login item: the switch shows what the system
            // accepted. This one fails routinely: macOS needs an app bundle,
            // Linux needs xdg-mime: so the reason is logged rather than lost.
            let applied = platform::set_magnet_handler(on).unwrap_or_else(|error| {
                tracing::warn!(%error, "could not change the magnet link handler");
                !on
            });
            settings::edit(&config, |c| c.handle_magnets = applied);
            if let Some(window) = weak.upgrade() {
                window.set_handle_magnets(applied);
            }
        }
    });

    window.on_set_seed_after_complete({
        let config = config.clone();
        let weak = window.as_weak();
        move |on| {
            settings::edit(&config, |c| c.seed_after_complete = on);
            if let Some(window) = weak.upgrade() {
                window.set_seed_after_complete(on);
            }
        }
    });

    window.on_set_menu_bar_style({
        let config = config.clone();
        let weak = window.as_weak();
        move |index| {
            settings::edit(&config, |c| {
                c.menu_bar_style = settings::MenuBarStyle::from_index(index)
            });
            if let Some(window) = weak.upgrade() {
                window.set_menu_bar_style(index);
            }
        }
    });

    window.on_set_max_concurrent({
        let config = config.clone();
        let weak = window.as_weak();
        move |value| {
            let value = value.clamp(1, 16);
            settings::edit(&config, |c| c.max_concurrent = value as usize);
            if let Some(window) = weak.upgrade() {
                window.set_max_concurrent(value);
            }
        }
    });

    let relay_rows = std::rc::Rc::new(slint::VecModel::from(rows_for_paired(&relays::load())));
    window.set_relays(relay_rows.clone().into());

    // -------------------------------------------------------------- Phones

    window.on_scan({
        let weak = window.as_weak();
        let handle = runtime.clone();
        move || {
            let Some(window) = weak.upgrade() else { return };
            window.set_scanning(true);
            let weak = weak.clone();
            // Off the event loop. A phone that is asleep takes the full
            // control timeout, and a frozen settings window reads as a crash.
            handle.spawn(async move {
                let found = scan_for_relays().await;
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(window) = weak.upgrade() else { return };
                    window
                        .set_relays(std::rc::Rc::new(slint::VecModel::from(to_rows(found))).into());
                    window.set_scanning(false);
                });
            });
        }
    });

    window.on_pair({
        let handle = runtime.clone();
        let weak = window.as_weak();
        let rows = relay_rows.clone();
        move |index| {
            let Some(row) = rows.row_data(index as usize) else { return };
            let address = row.address.to_string();
            let name = row.name.to_string();
            let weak = weak.clone();
            // Sits until someone taps accept on the phone, so this cannot be
            // on the event loop either.
            handle.spawn(async move {
                let desktop = whoami();
                let Ok(outcome) = dl_net::control::pair(&address, &desktop).await else {
                    tracing::warn!(%address, "pairing was refused or timed out");
                    return;
                };
                let hello = dl_net::control::hello(&address).await.ok();
                let mut paired = relays::load();
                paired.retain(|p| p.relay.address != address);
                paired.push(relays::Paired {
                    relay: dl_net::Relay::new(name, address, Some(outcome.key)),
                    device_id: hello.map(|h| h.device_id).unwrap_or_default(),
                    // Nothing on by default. A lane that spends someone's
                    // money is switched on by them, not by pairing.
                    enabled: Vec::new(),
                });
                relays::save(&paired);
                let refreshed = scan_for_relays().await;
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(window) = weak.upgrade() else { return };
                    window.set_relays(
                        std::rc::Rc::new(slint::VecModel::from(to_rows(refreshed))).into(),
                    );
                });
            });
        }
    });

    window.on_forget({
        let rows = relay_rows.clone();
        move |index| {
            let Some(row) = rows.row_data(index as usize) else { return };
            let address = row.address.to_string();
            let mut paired = relays::load();
            paired.retain(|p| p.relay.address != address);
            relays::save(&paired);
            rows.set_vec(rows_for_paired(&paired));
        }
    });

    window.on_set_lane({
        let rows = relay_rows.clone();
        move |index, lane, on| {
            let Some(row) = rows.row_data(index as usize) else { return };
            let address = row.address.to_string();
            let lane = lane.to_string();
            let mut paired = relays::load();
            if let Some(entry) = paired.iter_mut().find(|p| p.relay.address == address) {
                entry.enabled.retain(|id| *id != lane);
                if on {
                    entry.enabled.push(lane.clone());
                }
            }
            relays::save(&paired);

            // Reflect it immediately rather than waiting for a rescan: a
            // toggle that does not move has been pressed twice by everyone.
            let mut row = row;
            let lanes: Vec<RelayLaneRow> = row
                .lanes
                .iter()
                .map(|mut l| {
                    if l.id == lane.as_str() {
                        l.on = on;
                    }
                    l
                })
                .collect();
            row.lanes = std::rc::Rc::new(slint::VecModel::from(lanes)).into();
            rows.set_row_data(index as usize, row);
        }
    });

    // ------------------------------------------------------------- Network

    window.on_set_interface({
        let config = config.clone();
        let interfaces = interfaces.clone();
        move |name, enabled| {
            settings::edit(&config, |c| {
                c.interfaces.retain(|existing| existing != name.as_str());
                if enabled {
                    c.interfaces.push(name.to_string());
                }
            });
            for index in 0..interfaces.row_count() {
                let mut row = interfaces.row_data(index).unwrap();
                if row.name == name {
                    row.enabled = enabled;
                    interfaces.set_row_data(index, row);
                }
            }
        }
    });

    window.on_set_interface_limit({
        let config = config.clone();
        let interfaces = interfaces.clone();
        move |name, text| {
            let key = name.to_string();
            let rate = settings::parse_rate(&text);
            let blank = text.trim().is_empty();
            // Empty means unlimited; anything unparseable is left as typed and
            // changes nothing, so a half-typed "5 M" does not clamp to 5 bytes.
            if !blank && rate.is_none() {
                return;
            }
            settings::edit(&config, |c| match rate {
                Some(rate) => {
                    c.interface_limits.insert(key.clone(), rate);
                }
                None => {
                    c.interface_limits.remove(&key);
                }
            });
            for index in 0..interfaces.row_count() {
                let mut row = interfaces.row_data(index).unwrap();
                if row.name == name {
                    row.limit = rate.map(settings::format_rate_per_sec).unwrap_or_default().into();
                    interfaces.set_row_data(index, row);
                }
            }
        }
    });

    window.on_set_proxy_mode({
        let config = config.clone();
        let weak = window.as_weak();
        move |index| {
            settings::edit(&config, |c| c.proxy_mode = settings::ProxyMode::from_index(index));
            if let Some(window) = weak.upgrade() {
                window.set_proxy_mode(index);
            }
        }
    });

    window.on_set_proxy_server({
        let config = config.clone();
        move |text| settings::edit(&config, |c| c.proxy_server = text.to_string())
    });

    window.on_set_dns_mode({
        let config = config.clone();
        let weak = window.as_weak();
        move |index| {
            settings::edit(&config, |c| c.dns_mode = settings::DnsMode::from_index(index));
            if let Some(window) = weak.upgrade() {
                window.set_dns_mode(index);
            }
        }
    });

    // ----------------------------------------------------------- Bandwidth

    window.on_set_limited({
        let config = config.clone();
        let weak = window.as_weak();
        move |limited| {
            let Some(window) = weak.upgrade() else { return };
            window.set_limited(limited);
            settings::edit(&config, |c| {
                c.limit = limited
                    .then(|| settings::parse_rate(&window.get_limit_text()))
                    .flatten()
                    .or(limited.then_some(20 << 20));
            });
        }
    });

    window.on_set_limit({
        let config = config.clone();
        move |text| {
            // A typo leaves the previous value in force rather than setting a
            // limit of zero, which would stop every transfer in the app.
            if let Some(rate) = settings::parse_rate(&text) {
                settings::edit(&config, |c| {
                    if c.limit.is_some() {
                        c.limit = Some(rate);
                    }
                });
            }
        }
    });

    window.on_set_upload_limited({
        let config = config.clone();
        let weak = window.as_weak();
        move |limited| {
            let Some(window) = weak.upgrade() else { return };
            window.set_upload_limited(limited);
            settings::edit(&config, |c| {
                c.upload_limit = limited
                    .then(|| settings::parse_rate(&window.get_upload_limit_text()))
                    .flatten()
                    .or(limited.then_some(2 << 20));
            });
        }
    });

    window.on_set_upload_limit({
        let config = config.clone();
        move |text| {
            if let Some(rate) = settings::parse_rate(&text) {
                settings::edit(&config, |c| {
                    if c.upload_limit.is_some() {
                        c.upload_limit = Some(rate);
                    }
                });
            }
        }
    });

    window.on_set_schedule_on({
        let config = config.clone();
        let weak = window.as_weak();
        move |on| {
            settings::edit(&config, |c| c.schedule_enabled = on);
            if let Some(window) = weak.upgrade() {
                window.set_schedule_on(on);
            }
        }
    });

    window.on_toggle_slot({
        let config = config.clone();
        let cells = cells.clone();
        move |index| {
            let index = index as usize;
            if index >= settings::SCHEDULE_CELLS {
                return;
            }
            let value = !cells.row_data(index).unwrap_or(false);
            cells.set_row_data(index, value);
            settings::edit(&config, |c| c.schedule_cells[index] = value);
        }
    });

    window.on_add_window({
        let config = config.clone();
        let cells = cells.clone();
        move || {
            settings::edit(&config, |c| c.add_default_window());
            if let Ok(current) = config.read() {
                for (index, on) in current.schedule_cells.iter().enumerate() {
                    cells.set_row_data(index, *on);
                }
            }
        }
    });

    window.on_clear_schedule({
        let config = config.clone();
        let cells = cells.clone();
        move || {
            settings::edit(&config, |c| c.clear_schedule());
            for index in 0..settings::SCHEDULE_CELLS {
                cells.set_row_data(index, false);
            }
        }
    });

    // ----------------------------------------------------------- Integrity

    window.on_set_checksum_algorithm({
        let config = config.clone();
        let weak = window.as_weak();
        move |index| {
            settings::edit(&config, |c| c.checksum = settings::algorithm_from_index(index));
            if let Some(window) = weak.upgrade() {
                window.set_checksum_algorithm(index);
            }
        }
    });

    window.on_set_durability({
        let config = config.clone();
        let weak = window.as_weak();
        move |index| {
            settings::edit(&config, |c| c.durability = settings::durability_from_index(index));
            if let Some(window) = weak.upgrade() {
                window.set_durability(index);
            }
        }
    });

    window.on_set_retries({
        let config = config.clone();
        let weak = window.as_weak();
        move |value| {
            let value = value.clamp(0, 20);
            settings::edit(&config, |c| c.retries = value as u32);
            if let Some(window) = weak.upgrade() {
                window.set_retries(value);
            }
        }
    });

    // ------------------------------------------------------------ Advanced

    window.on_set_connections({
        let config = config.clone();
        let weak = window.as_weak();
        move |value| {
            let value = value.clamp(1, 32);
            settings::edit(&config, |c| c.connections = value as usize);
            if let Some(window) = weak.upgrade() {
                window.set_connections(value);
            }
        }
    });

    window.on_set_chunk_size({
        let config = config.clone();
        let weak = window.as_weak();
        move |index| {
            settings::edit(&config, |c| c.chunk_size = settings::chunk_size_from_index(index));
            if let Some(window) = weak.upgrade() {
                window.set_chunk_size(index);
            }
        }
    });

    window.on_set_refresh_cap({
        let config = config.clone();
        let weak = window.as_weak();
        move |value| {
            let value = value.clamp(1, 100);
            settings::edit(&config, |c| c.refresh_cap = value as u32);
            if let Some(window) = weak.upgrade() {
                window.set_refresh_cap(value);
            }
        }
    });

    window.on_reset_all({
        let config = config.clone();
        let weak = window.as_weak();
        let cells = cells.clone();
        let interfaces = interfaces.clone();
        let usable = usable.clone();
        move || {
            settings::edit(&config, |c| *c = settings::Settings::new(download_dir()));
            let Some(window) = weak.upgrade() else { return };
            push_settings(&window, &config);
            for index in 0..settings::SCHEDULE_CELLS {
                cells.set_row_data(index, false);
            }
            for (index, row) in interface_rows(&usable, &config).into_iter().enumerate() {
                interfaces.set_row_data(index, row);
            }
        }
    });

    ui.on_open_settings({
        let weak = window.as_weak();
        let ui = ui.as_weak();
        move || {
            let Some(window) = weak.upgrade() else { return };
            if let Some(ui) = ui.upgrade() {
                window.set_dark(ui.get_dark());
            }
            let _ = window.show();
            window.window().set_minimized(false);
        }
    });

    // The window outlives this function only because the callback above holds
    // a weak handle; keep the strong one alive for the life of the process.
    std::mem::forget(window);
}

/// Push every stored value into the window. Used at startup and after a reset,
/// so there is one description of what the controls should read.
fn push_settings(window: &SettingsWindow, config: &settings::Shared) {
    let Ok(current) = config.read() else { return };
    window.set_destination(current.destination.to_string_lossy().to_string().into());
    window.set_incomplete_dir(
        current
            .incomplete_dir
            .as_ref()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default()
            .into(),
    );
    window.set_ask_where(current.ask_where);
    window.set_max_concurrent(current.max_concurrent as i32);
    window.set_reveal_on_done(current.reveal_on_done);
    window.set_sound_on_done(current.sound_on_done);
    window.set_keep_completed(current.keep_completed);
    window.set_launch_at_login(current.launch_at_login);
    window.set_menu_bar(current.menu_bar);
    window.set_menu_bar_style(current.menu_bar_style.index());
    // Read from the system rather than from the settings file: the
    // registration lives outside our control and another torrent client can
    // take it without telling us.
    window.set_handle_magnets(platform::magnet_handler_is_us());
    window.set_seed_after_complete(current.seed_after_complete);
    window.set_torrents_available(cfg!(feature = "torrent"));

    window.set_proxy_mode(current.proxy_mode.index());
    window.set_proxy_server(current.proxy_server.clone().into());
    window.set_dns_mode(current.dns_mode.index());

    window.set_limited(current.limit.is_some());
    window.set_limit_text(settings::format_rate_per_sec(current.limit.unwrap_or(20 << 20)).into());
    window.set_upload_limited(current.upload_limit.is_some());
    window.set_upload_limit_text(
        settings::format_rate(current.upload_limit.unwrap_or(2 << 20)).into(),
    );
    window.set_schedule_on(current.schedule_enabled);
    window.set_manual_ignores_limit(current.manual_ignores_limit);

    window.set_verify_every(current.verify_every);
    window.set_checksum_algorithm(settings::algorithm_index(current.checksum));
    window.set_refetch_damaged(current.refetch_damaged);
    window.set_durability(settings::durability_index(current.durability));
    window.set_keep_partial(current.keep_partial);
    window.set_retries(current.retries as i32);

    window.set_connections(current.connections as i32);
    window.set_chunk_size(settings::chunk_size_index(current.chunk_size));
    window.set_refresh_links(current.refresh_links);
    window.set_refresh_cap(current.refresh_cap as i32);
}

fn interface_rows(
    usable: &[dl_net::Interface],
    config: &settings::Shared,
) -> Vec<InterfaceSetting> {
    let stored = config.read().ok();
    usable
        .iter()
        .map(|iface| {
            let enabled =
                stored.as_ref().is_some_and(|c| c.interfaces.iter().any(|n| n == &iface.name));
            let limit = stored
                .as_ref()
                .and_then(|c| c.interface_limit(&iface.name))
                .map(settings::format_rate_per_sec)
                .unwrap_or_default();
            InterfaceSetting {
                // The device id stays the identity; this is only what it is
                // called on screen. See `Interface::display_label`.
                name: iface.name.clone().into(),
                display: iface.display_label().into(),
                icon: iface.kind.icon().into(),
                address: iface
                    .routable_ipv4()
                    .map(|ip| ip.to_string())
                    .unwrap_or_else(|| "no address".into())
                    .into(),
                // What the platform will actually use, not what we would like
                // it to: a silent fall back to a plain source-address bind is
                // this feature's real failure mode, and this is where it shows.
                binding: dl_net::BindMechanism::platform_default().as_str().into(),
                status: if iface.has_gateway { "up".into() } else { "down".into() },
                enabled,
                limit: limit.into(),
            }
        })
        .collect()
}

/// Ask for a folder, starting where the current destination is.
fn pick_folder(config: &settings::Shared) -> Option<std::path::PathBuf> {
    let start = config.read().ok().map(|c| c.destination.clone()).unwrap_or_else(download_dir);
    rfd::FileDialog::new().set_directory(start).set_title("Choose a download folder").pick_folder()
}

/// Ask the origin what it has, and describe the answer the way the add sheet
/// shows it: name, size, whether ranges work, whether there is a validator.
///
/// A stale answer is worse than none: the URL may have changed three
/// keystrokes ago: so each probe carries the generation it was started for
/// and a late reply for an older one is dropped.
fn spawn_probe(
    ui: slint::Weak<MainWindow>,
    url: String,
    generation: u64,
    live: Arc<AtomicU64>,
    http: HttpConfig,
) {
    tokio::spawn(async move {
        // Long enough that typing a URL does not fire one request per
        // character, short enough that a paste feels immediate.
        tokio::time::sleep(std::time::Duration::from_millis(450)).await;
        if live.load(Ordering::SeqCst) != generation {
            return;
        }

        let probed = match HttpSource::with_config(&http, url.clone()) {
            Ok(source) => {
                use dl_core::source::ByteSource as _;
                source.probe().await
            }
            Err(error) => Err(error),
        };
        if live.load(Ordering::SeqCst) != generation {
            return;
        }

        let fallback = dl_net::filename_from_url(&url).unwrap_or_else(|| "download.bin".into());
        let (state, name, detail, kind) = match probed {
            Ok(info) => {
                let name = info.suggested_filename.clone().unwrap_or(fallback);
                let mut parts = Vec::new();
                match info.len {
                    Some(len) => parts.push(bridge::format_bytes(len)),
                    None => parts.push("unknown size".into()),
                }
                parts.push(
                    if info.supports_chunking() {
                        "Range supported"
                    } else {
                        "single connection only"
                    }
                    .into(),
                );
                if info.etag.is_some() {
                    parts.push("ETag present".into());
                } else if info.last_modified.is_some() {
                    parts.push("dated".into());
                } else {
                    // Without a validator a resumed transfer cannot prove the
                    // file did not change under it, which is worth saying here
                    // rather than at the point it goes wrong.
                    parts.push("no validator".into());
                }
                let kind = bridge::file_category(&name).to_string();
                ("ok", name, parts.join(" · "), kind)
            }
            Err(error) => ("failed", fallback, error.to_string(), "doc".to_string()),
        };

        let _ = ui.upgrade_in_event_loop(move |ui| {
            ui.set_probe_state(state.into());
            ui.set_probe_name(name.into());
            ui.set_probe_detail(detail.into());
            ui.set_probe_kind(kind.into());
        });
    });
}

/// What the add sheet can say about an input without a network request.
struct Resolved {
    state: &'static str,
    name: String,
    detail: String,
    kind: &'static str,
}

/// Describe a torrent link honestly, or `None` when the HTTP probe should run.
///
/// A magnet cannot be probed. There is no origin to ask for a length, a
/// validator or a `Content-Disposition`, and the only thing that would answer
/// is the swarm itself, minutes later. So the strip reports what is actually
/// known: the publisher's own `dn=` name and the fact that nothing has been
/// looked up yet: rather than a failed HTTP request, which would read as a
/// broken link.
fn describe_without_asking(url: &str) -> Option<Resolved> {
    let torrent_built_in = cfg!(feature = "torrent");
    match dl_core::classify(url) {
        dl_core::TransferKind::Http => None,
        dl_core::TransferKind::IncompleteMagnet => Some(Resolved {
            state: "failed",
            name: "Incomplete magnet link".into(),
            detail: "This link carries no xt=urn:btih: info hash, so there is nothing to look up."
                .into(),
            kind: "doc",
        }),
        _ if !torrent_built_in => Some(Resolved {
            state: "failed",
            name: "Torrent support is not compiled in".into(),
            detail: "This build of Braid was made without the torrent feature.".into(),
            kind: "doc",
        }),
        dl_core::TransferKind::Torrent(source) => {
            let (label, detail) = match &source {
                dl_core::TorrentSource::Magnet(_) => {
                    ("magnet link", "Peers and file list arrive once the swarm answers.")
                }
                dl_core::TorrentSource::Url(_) => {
                    ("torrent file", "The .torrent is fetched when the transfer starts.")
                }
                dl_core::TorrentSource::File(_) => {
                    ("torrent file", "Read from disk when the transfer starts.")
                }
            };
            Some(Resolved {
                state: "torrent",
                name: source.provisional_name().unwrap_or_else(|| label.to_string()),
                detail: format!("{label} · {detail}"),
                kind: "doc",
            })
        }
    }
}

/// Add a transfer that came from the system rather than from the add sheet.
///
/// Deliberately not routed through the add sheet: someone who clicked a magnet
/// link in a browser has already said what they want, and a modal asking them
/// to confirm the link they just clicked is a dialog for its own sake. The
/// app-wide destination and connection count apply, exactly as they would for
/// a transfer added with the sheet's defaults untouched.
fn add_link(engine: &Engine, config: &settings::Shared, link: &str) {
    let kind = dl_core::classify(link);
    if kind == dl_core::TransferKind::IncompleteMagnet {
        tracing::warn!(%link, "ignoring a magnet link with no info hash");
        return;
    }
    let (destination, connections) = config
        .read()
        .ok()
        .map(|c| (c.destination.clone(), c.connections))
        .unwrap_or_else(|| (download_dir(), 8));

    let target = match kind {
        // A torrent names its own contents, so the destination is the folder.
        dl_core::TransferKind::Torrent(_) => destination,
        _ => destination
            .join(dl_net::filename_from_url(link).unwrap_or_else(|| "download.bin".into())),
    };
    let mut spec = DownloadSpec::new(link, target);
    spec.connections = connections.max(1);
    let id = engine.add(spec);
    // Logged because the alternative diagnosis for "I clicked a magnet link
    // and nothing happened" is guesswork: this line separates "the system
    // never told us" from "we took it and the transfer failed".
    tracing::info!(?id, %link, "opened a link from the system");
}

/// Default log filter.
///
/// librqbit reports a peer task ending as an ERROR, including the ones that
/// end because *we* paused the torrent: "chunk tracker empty, torrent was
/// paused", once per connected peer. That is dozens of red lines for a normal
/// pause, and it buries anything that is genuinely wrong. Its peer plumbing is
/// quietened to `warn`; everything else it has to say still comes through, and
/// `RUST_LOG` overrides all of this.
const DEFAULT_LOG: &str = "info,librqbit_core::spawn_utils=warn,librqbit::peer_connection=warn";

/// Stop everything that is moving bytes.
///
/// Seeding included: a "pause all" that left the swarm running would not be
/// one.
fn pause_everything(engine: &Engine) {
    for snapshot in engine.snapshot() {
        if matches!(
            snapshot.state,
            dl_core::State::Running | dl_core::State::Queued | dl_core::State::Seeding
        ) {
            engine.pause(snapshot.id);
        }
    }
}

fn main() -> Result<()> {
    // Before anything is built: a magnet link clicked while Braid is already
    // running must reach the running copy, not start a second engine writing
    // the same files.
    let launch_link = dl_gui::ipc::argument_from(std::env::args());
    if let Some(link) = &launch_link
        && dl_gui::ipc::hand_off(link)
    {
        return Ok(());
    }
    // Installed before the event loop exists: macOS delivers a cold launch's
    // `kAEGetURL` almost immediately, and `openurl` queues anything that
    // arrives before the window is ready.
    dl_gui::openurl::listen_for_system_events();
    if let Some(link) = launch_link {
        dl_gui::openurl::deliver(link);
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| DEFAULT_LOG.into()),
        )
        .init();

    // Requested, not assumed. The blur goes through a private API, so a macOS
    // that drops it degrades to an opaque sidebar rather than to a window with
    // nothing behind it.
    let shell = platform::install(platform::Vibrancy::Requested);
    tracing::debug!(vibrancy = shell.vibrancy, "shell installed");

    // Slint owns the main thread, so the engine gets its own runtime.
    let runtime = tokio::runtime::Runtime::new()?;
    let _guard = runtime.enter();

    let usable = {
        use dl_net::InterfaceProvider as _;
        SystemInterfaces.usable()
    };
    let default_lane = default_route_interface(&usable);
    tracing::debug!(%default_lane, "unpinned transfers will be attributed here");

    // Loaded rather than defaulted: preferences that do not survive a quit are
    // not preferences.
    let config: settings::Shared = Arc::new(RwLock::new(settings::Settings::load(download_dir())));

    // The honest name for traffic we cannot attribute: the interface the OS
    // will actually route over, not an invented "default" NIC beside the real
    // ones. See `EngineConfig::unattributed_lane`.
    if let Ok(mut current) = config.write() {
        current.unattributed_lane = default_lane.clone();
    }
    let engine_config = config.read().expect("fresh lock").engine_config();
    let engine = Engine::new(
        Arc::new(HttpFactory { settings: config.clone(), default_lane }),
        engine_config,
        Budget::unlimited(),
    );
    install_torrent_backend(&engine, &config);
    // From here on, changing a preference reaches the engine without a restart.
    settings::bind_engine(engine.clone());
    // The upload ceiling is not part of `EngineConfig`, so it is pushed on its
    // own: at launch as well as on every change, or a limit saved last time
    // would not be in force until it was touched again.
    settings::apply_upload_limit(&config);

    // What the last run was doing. Before the window exists, so the first
    // frame already has the rows rather than growing them a tick later.
    let restored = transfers::restore(&engine);
    if restored > 0 {
        tracing::info!(restored, "transfers carried over from the last run");
    }
    transfers::spawn_autosave(engine.clone());

    let ui = MainWindow::new()?;
    // Follow the system appearance at launch. The property stays settable so
    // screenshot tests can pin a theme instead of depending on the host.
    ui.set_dark(dark_mode());
    // What was granted, never what was asked for: the sidebar only goes
    // translucent if there is actually blur behind it.
    ui.set_vibrancy(shell.vibrancy);
    if let Ok(current) = config.read() {
        ui.set_destination(current.destination.to_string_lossy().to_string().into());
        ui.set_connections(current.connections as i32);
        ui.set_checksum_algorithm(settings::algorithm_index(current.checksum));
        ui.set_verify_on(false);
        ui.set_keep_completed(current.keep_completed);
    }

    // "All" plus each interface by name, so one transfer can be pinned without
    // changing the app-wide selection.
    let mut options: Vec<slint::SharedString> = vec![format!("All ({})", usable.len()).into()];
    options.extend(usable.iter().map(|i| slint::SharedString::from(i.display_label())));
    ui.set_interface_options(std::rc::Rc::new(slint::VecModel::from(options)).into());

    ui.on_add_download({
        let engine = engine.clone();
        let cfg = config.clone();
        let weak = ui.as_weak();
        let names: Vec<String> = usable.iter().map(|i| i.name.clone()).collect();
        move |url| {
            let url = url.trim().to_string();
            if url.is_empty() {
                return;
            }
            let Some(ui) = weak.upgrade() else { return };

            // A magnet with no info hash names nothing. Refusing here beats
            // adding a row that exists only to go red a moment later.
            if dl_core::classify(&url) == dl_core::TransferKind::IncompleteMagnet {
                ui.set_probe_state("failed".into());
                ui.set_probe_name("Incomplete magnet link".into());
                ui.set_probe_detail(
                    "This link carries no xt=urn:btih: info hash, so there is nothing to look up."
                        .into(),
                );
                ui.set_sheet_open(true);
                return;
            }
            // A torrent names its own contents, so its destination is the
            // folder and nothing here gets to pick a filename.
            let is_torrent = matches!(dl_core::classify(&url), dl_core::TransferKind::Torrent(_));

            // The name the server gave us if the probe got one, the URL's own
            // last segment otherwise.
            let name = match ui.get_probe_state().as_str() {
                "ok" if !ui.get_probe_name().is_empty() => ui.get_probe_name().to_string(),
                _ => dl_net::filename_from_url(&url).unwrap_or_else(|| "download.bin".into()),
            };
            let (destination, connections) = cfg
                .read()
                .ok()
                .map(|s| (s.destination.clone(), s.connections))
                .unwrap_or_else(|| (download_dir(), 8));

            // "Ask where to save each file" asks here, where the transfer is
            // about to start, rather than when it finishes.
            let destination = if cfg.read().is_ok_and(|c| c.ask_where) {
                match pick_folder(&cfg) {
                    Some(folder) => folder,
                    None => return,
                }
            } else {
                destination
            };

            let target = if is_torrent { destination.clone() } else { destination.join(name) };
            let mut spec = DownloadSpec::new(url, target);
            // The sheet's value if the sheet set one, otherwise the default.
            spec.connections = (ui.get_connections() as usize).max(1).min(connections.max(32));
            // Index 0 is "All"; anything else pins this transfer to one NIC.
            let choice = ui.get_interface_choice() as usize;
            if choice > 0
                && let Some(name) = names.get(choice - 1)
            {
                spec.interfaces = vec![name.clone()];
            }
            if ui.get_verify_on() {
                let typed = ui.get_checksum_text();
                let algorithm = settings::algorithm_from_index(ui.get_checksum_algorithm());
                match settings::parse_digest(algorithm, typed.trim()) {
                    Some(digest) => spec.expect = Some(digest),
                    // Refuse rather than download 6 GB and check it against
                    // nothing: an unparseable digest is a typo, not a choice.
                    None => {
                        ui.set_probe_state("failed".into());
                        ui.set_probe_name("Checksum not understood".into());
                        ui.set_probe_detail(
                            format!(
                                "Expected {} hex characters for {}.",
                                algorithm.hex_len(),
                                algorithm.label()
                            )
                            .into(),
                        );
                        ui.set_sheet_open(true);
                        return;
                    }
                }
            }
            engine.add(spec);

            ui.set_url_input(Default::default());
            ui.set_checksum_text(Default::default());
            ui.set_verify_on(false);
            ui.set_probe_state("idle".into());
        }
    });

    // Each keystroke starts a probe; only the newest one is allowed to land.
    let probe_generation = Arc::new(AtomicU64::new(0));
    ui.on_probe_url({
        let weak = ui.as_weak();
        let cfg = config.clone();
        let generation = probe_generation.clone();
        move |url| {
            let url = url.trim().to_string();
            let next = generation.fetch_add(1, Ordering::SeqCst) + 1;
            // Answered here rather than by a request, because there is nothing
            // to ask: a magnet has no server, and a `.torrent` on disk has no
            // network at all.
            if let Some(resolved) = describe_without_asking(&url) {
                if let Some(ui) = weak.upgrade() {
                    ui.set_probe_state(resolved.state.into());
                    ui.set_probe_name(resolved.name.into());
                    ui.set_probe_detail(resolved.detail.into());
                    ui.set_probe_kind(resolved.kind.into());
                }
                return;
            }
            if url.is_empty() || !url.contains("://") {
                if let Some(ui) = weak.upgrade() {
                    ui.set_probe_state("idle".into());
                }
                return;
            }
            if let Some(ui) = weak.upgrade() {
                ui.set_probe_state("probing".into());
                ui.set_probe_name(
                    dl_net::filename_from_url(&url).unwrap_or_else(|| "…".into()).into(),
                );
                ui.set_probe_detail("Asking the server…".into());
            }
            let http = cfg.read().map(|c| c.http_config()).unwrap_or_default();
            spawn_probe(weak.clone(), url, next, generation.clone(), http);
        }
    });

    ui.on_choose_destination({
        let cfg = config.clone();
        let weak = ui.as_weak();
        move || {
            let Some(folder) = pick_folder(&cfg) else { return };
            settings::edit(&cfg, |c| c.destination = folder.clone());
            if let Some(ui) = weak.upgrade() {
                ui.set_destination(folder.to_string_lossy().to_string().into());
            }
        }
    });

    // The tray is a separate top-level component with its own copy of any
    // globals, so everything it shows is pushed to it explicitly.
    let tray = Tray::new()?;
    tray.on_show_requested({
        let weak = ui.as_weak();
        move || {
            if let Some(ui) = weak.upgrade() {
                let _ = ui.show();
                ui.window().set_minimized(false);
            }
        }
    });
    tray.on_pause_all({
        let engine = engine.clone();
        move || pause_everything(&engine)
    });
    tray.on_quit_requested(|| {
        let _ = slint::quit_event_loop();
    });

    // From here the app is ready to take links: from the launch argument, from
    // a second launch over the socket, and from the system's own event.
    dl_gui::openurl::install({
        let engine = engine.clone();
        let cfg = config.clone();
        let weak = ui.as_weak();
        // Links arrive on threads that know nothing about the runtime: the
        // single-instance listener is a plain `std::thread`, and adding a
        // transfer spawns a task. Without this the first magnet handed over
        // from a second launch panicked with "there is no reactor running".
        let runtime = runtime.handle().clone();
        move |link| {
            let _guard = runtime.enter();
            add_link(&engine, &cfg, &link);
            // Raise the window: a click in a browser that adds a row to a
            // window behind three others reads as a click that did nothing.
            let _ = weak.upgrade_in_event_loop(|ui| {
                let _ = ui.show();
                ui.window().set_minimized(false);
            });
        }
    });
    dl_gui::openurl::reclaim_system_events();
    if let Err(error) = dl_gui::ipc::listen(dl_gui::openurl::deliver) {
        // Not fatal: without it a second launch opens a second window, which
        // is worse than this but not a reason to refuse to start.
        tracing::warn!(%error, "single-instance handoff is not available");
    }

    let mut interfaces: Vec<bridge::InterfaceInfo> = usable
        .iter()
        .map(|i| bridge::InterfaceInfo {
            id: i.name.clone(),
            label: i.display_label(),
            icon: i.kind.icon().to_string(),
        })
        .collect();
    interfaces.extend(relay_lane_rows(&relays::load()));
    tracing::debug!(?interfaces, "usable interfaces");
    bridge::spawn(&ui, engine.clone(), Some(tray.as_weak()), interfaces, config.clone());

    // The schedule owns the global budget from here on, so the manual limit
    // and the timetable can never disagree about which one is in force.
    settings::spawn_scheduler(config.clone(), engine.budget().clone());
    wire_settings(&ui, config, usable, runtime.handle().clone());
    ui.run()?;
    Ok(())
}

#[cfg(test)]
mod factory_tests {
    use super::*;
    use dl_net::path::Path;

    #[test]
    fn a_paired_phone_is_listed_in_the_sidebar_before_it_carries_anything() {
        // A lane that only appears once it is busy cannot be judged, and the
        // reason to show throughput per lane is to see the ones doing nothing.
        let rows = relay_lane_rows(&[phone(Some("k"), &["cell", "wifi"])]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].label, "Pixel (cell)");
        assert_eq!(rows[1].label, "Pixel (wifi)");
    }

    #[test]
    fn the_sidebar_id_is_the_label_the_engine_will_report() {
        // The meters are keyed on the lane label. If these ever drift apart,
        // the phone's row sits at zero while its traffic is credited to a
        // second row that appears from nowhere.
        let paired = phone(Some("k"), &["cell"]);
        let rows = relay_lane_rows(std::slice::from_ref(&paired));
        let lane =
            dl_net::path::Path::Relay { relay: paired.relay.clone(), network: "cell".into() };
        assert_eq!(rows[0].id, lane.label(None));
    }

    #[test]
    fn an_unpaired_phone_gets_no_sidebar_row() {
        // It cannot serve, so a meter for it would sit at zero for ever.
        assert!(relay_lane_rows(&[phone(None, &["cell"])]).is_empty());
    }

    fn phone(key: Option<&str>, enabled: &[&str]) -> relays::Paired {
        relays::Paired {
            relay: dl_net::Relay::new("Pixel", "10.0.0.5:8710", key.map(str::to_string)),
            device_id: "abc".into(),
            enabled: enabled.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn selected_interfaces_and_enabled_relay_lanes_are_one_list() {
        // The product claim: Ethernet, Wi-Fi and a phone at once. Were these
        // separate lane sets, a transfer would have to choose between them.
        let paths = paths_for(&["en0".into()], &[phone(Some("k"), &["cell", "wifi"])], "en0");
        assert_eq!(paths.len(), 3);
        assert!(matches!(&paths[0], Path::Interface(name) if name == "en0"));
        assert!(matches!(&paths[1], Path::Relay { network, .. } if network == "cell"));
        assert!(matches!(&paths[2], Path::Relay { network, .. } if network == "wifi"));
    }

    #[test]
    fn a_relay_with_nothing_switched_on_contributes_nothing() {
        let paths = paths_for(&["en0".into()], &[phone(Some("k"), &[])], "en0");
        assert_eq!(paths.len(), 1);
    }

    #[test]
    fn an_unpaired_relay_is_never_used() {
        // No key means no authorisation, and sending traffic anyway earns a
        // 407 per chunk before the lane is parked.
        let paths = paths_for(&[], &[phone(None, &["cell"])], "en0");
        assert_eq!(paths.len(), 1);
        assert!(matches!(paths[0], Path::Default));
    }

    #[test]
    fn nothing_selected_anywhere_still_downloads() {
        // A laptop with one card and no phone, which is most of them.
        let paths = paths_for(&[], &[], "en0");
        assert_eq!(paths.len(), 1);
        assert!(matches!(paths[0], Path::Default));
    }

    #[test]
    fn several_phones_each_contribute_their_own_lanes() {
        let mut spare = phone(Some("k2"), &["cell"]);
        spare.relay.name = "Spare".into();
        let paths = paths_for(&[], &[phone(Some("k"), &["cell"]), spare], "en0");
        assert_eq!(paths.len(), 2);
    }
}
