//! Screenshot fixture for the settings window.
//!
//! Populated the same way the app populates it: through `push_settings`-shaped
//! code over a real `Settings` value: so a capture proves the model reaches
//! the controls, not just that the controls render.
//!
//! `--page <name>` picks which page to show; the harness captures each in turn.

use dl_gui::{InterfaceSetting, SettingsWindow, settings};
use slint::ComponentHandle as _;

fn main() -> Result<(), slint::PlatformError> {
    let args: Vec<String> = std::env::args().collect();
    let page = args
        .iter()
        .position(|a| a == "--page")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| "general".into());

    let window = SettingsWindow::new()?;
    window.set_dark(!args.iter().any(|a| a == "--light"));
    window.set_page(page.into());

    // A stand-in for the machine's real interfaces: the capture has to show a
    // populated table, and a build machine with one NIC would prove nothing
    // about the column layout.
    let interfaces = vec![
        InterfaceSetting {
            name: "Wi-Fi".into(),
            display: "Wi-Fi (en0)".into(),
            icon: "wifi".into(),
            address: "192.168.1.24".into(),
            binding: "IP_BOUND_IF".into(),
            status: "up".into(),
            enabled: true,
            limit: "".into(),
        },
        InterfaceSetting {
            name: "Ethernet".into(),
            display: "Thunderbolt 1 (en1)".into(),
            icon: "ethernet".into(),
            address: "10.0.0.8".into(),
            binding: "IP_BOUND_IF".into(),
            status: "up".into(),
            enabled: true,
            limit: "".into(),
        },
        InterfaceSetting {
            name: "USB Tether".into(),
            display: "iPhone USB (en5)".into(),
            icon: "ethernet".into(),
            address: "172.20.10.2".into(),
            binding: "IP_BOUND_IF".into(),
            status: "metered".into(),
            enabled: true,
            limit: "5 MB/s".into(),
        },
        InterfaceSetting {
            name: "VPN (utun3)".into(),
            display: "VPN (utun3)".into(),
            icon: "vpn".into(),
            address: "no address".into(),
            binding: "excluded".into(),
            status: "down".into(),
            enabled: false,
            limit: "".into(),
        },
    ];
    window.set_interfaces(std::rc::Rc::new(slint::VecModel::from(interfaces)).into());

    let mut model = settings::Settings::new("/Users/you/Downloads".into());
    model.limit = Some(20 << 20);
    model.schedule_enabled = true;
    model.add_default_window();
    // Weekends throughout, so the grid shows two shapes rather than one.
    for day in 5..7 {
        for slot in 0..settings::SLOTS_PER_DAY {
            model.schedule_cells[day * settings::SLOTS_PER_DAY + slot] = true;
        }
    }

    window.set_destination(model.destination.to_string_lossy().to_string().into());
    window.set_max_concurrent(model.max_concurrent as i32);
    window.set_keep_completed(model.keep_completed);
    window.set_menu_bar(model.menu_bar);
    // Same source of truth as the real window: a capture that shows the
    // torrent rows live in a build that has them, and greyed out in one that
    // does not, is the point of capturing them at all.
    window.set_torrents_available(cfg!(feature = "torrent"));
    window.set_seed_after_complete(model.seed_after_complete);
    window.set_limited(model.limit.is_some());
    window.set_limit_text(settings::format_rate_per_sec(model.limit.unwrap_or(0)).into());
    window.set_upload_limited(model.upload_limit.is_some());
    window
        .set_upload_limit_text(settings::format_rate(model.upload_limit.unwrap_or(2 << 20)).into());
    window.set_schedule_on(model.schedule_enabled);
    window.set_schedule_cells(
        std::rc::Rc::new(slint::VecModel::from(model.schedule_cells.clone())).into(),
    );
    window.set_verify_every(model.verify_every);
    window.set_checksum_algorithm(settings::algorithm_index(model.checksum));
    window.set_refetch_damaged(model.refetch_damaged);
    window.set_durability(settings::durability_index(model.durability));
    window.set_keep_partial(model.keep_partial);
    window.set_retries(model.retries as i32);
    window.set_connections(model.connections as i32);
    window.set_chunk_size(settings::chunk_size_index(model.chunk_size));
    window.set_refresh_links(model.refresh_links);
    window.set_refresh_cap(model.refresh_cap as i32);

    window.run()
}
