//! The settings the user can change, how they persist, and the task that
//! applies the bandwidth schedule.
//!
//! This is the whole of mockup/macos/settings-*.jpg as data. Some of it the
//! engine honours today and some of it it does not yet; docs/ui-status.md is
//! the register of which is which, and every field here carries a note where
//! the answer is "not yet".
//!
//! Persisted as flat `key = value` lines rather than JSON: it is a few dozen
//! scalars, the format survives a hand edit, and it costs no dependency.

use dl_core::budget::Budget;
use dl_core::schedule::{LocalTime, Schedule, TimeWindow, Weekday};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// Hours covered by one cell of the weekly schedule grid.
pub const SLOT_HOURS: u32 = 3;
/// Cells per day: 24 hours in three-hour slots.
pub const SLOTS_PER_DAY: usize = (24 / SLOT_HOURS) as usize;
/// The whole grid, day-major, so cell `d * SLOTS_PER_DAY + s` is day `d`.
pub const SCHEDULE_CELLS: usize = 7 * SLOTS_PER_DAY;

/// How often the schedule is re-evaluated. Windows have minute resolution, so
/// this only has to be fine enough that a boundary is not visibly late.
const SCHEDULE_TICK: Duration = Duration::from_secs(20);

/// How hard the storage layer works to survive a power cut. The engine's own
/// type, so the three labels on the Integrity page mean exactly what the store
/// does: see `dl_core::store::durability`.
pub use dl_core::store::Durability;

/// Dropdown order on the Integrity page.
pub fn durability_from_index(index: i32) -> Durability {
    match index {
        0 => Durability::Safe,
        2 => Durability::Fast,
        _ => Durability::Balanced,
    }
}

pub fn durability_index(durability: Durability) -> i32 {
    match durability {
        Durability::Safe => 0,
        Durability::Balanced => 1,
        Durability::Fast => 2,
    }
}

/// Which digest a transfer is checked against. The engine's own type: the
/// UI does not get a second opinion about what a checksum is.
pub use dl_core::integrity::Algorithm as ChecksumAlgorithm;

/// Dropdown order on the Integrity page and in the add sheet.
pub fn algorithm_from_index(index: i32) -> ChecksumAlgorithm {
    match index {
        1 => ChecksumAlgorithm::Sha256,
        2 => ChecksumAlgorithm::Md5,
        _ => ChecksumAlgorithm::Blake3,
    }
}

pub fn algorithm_index(algorithm: ChecksumAlgorithm) -> i32 {
    match algorithm {
        ChecksumAlgorithm::Blake3 => 0,
        ChecksumAlgorithm::Sha256 => 1,
        ChecksumAlgorithm::Md5 => 2,
    }
}

/// Read a digest the user pasted into the add sheet.
pub fn parse_digest(algorithm: ChecksumAlgorithm, text: &str) -> Option<dl_core::Digest> {
    dl_core::Digest::parse(algorithm, text)
}

/// What the macOS menu bar entry shows beside its icon.
///
/// `SystemTrayIcon::title` is the label rendered next to the icon there: the
/// same mechanism the clock and the battery percentage use. On Windows it has
/// no visible effect and on Linux it reaches accessibility tools only, so this
/// is a macOS preference that costs nothing elsewhere.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MenuBarStyle {
    /// Icon alone. The quietest, and what most menu bar apps do.
    #[default]
    IconOnly,
    /// Icon and the combined throughput, while anything is moving.
    Speed,
    /// Icon and how many transfers are running.
    Count,
}

impl MenuBarStyle {
    pub fn from_index(index: i32) -> Self {
        match index {
            1 => Self::Speed,
            2 => Self::Count,
            _ => Self::IconOnly,
        }
    }

    pub fn index(self) -> i32 {
        match self {
            Self::IconOnly => 0,
            Self::Speed => 1,
            Self::Count => 2,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::IconOnly => "icon",
            Self::Speed => "speed",
            Self::Count => "count",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "icon" => Some(Self::IconOnly),
            "speed" => Some(Self::Speed),
            "count" => Some(Self::Count),
            _ => None,
        }
    }

    /// The label to put beside the icon.
    ///
    /// Empty while nothing is running whatever the choice: a menu bar that
    /// permanently reads "0 B/s" is one the user turns off. The label also
    /// changes width as the figure does, and everything to its left on the
    /// menu bar shifts with it, so it earns its place only while there is
    /// something to report.
    pub fn label(self, active: i32, speed: &str) -> String {
        if active == 0 {
            return String::new();
        }
        match self {
            Self::IconOnly => String::new(),
            Self::Speed => speed.to_string(),
            Self::Count => active.to_string(),
        }
    }
}

/// Where a proxy comes from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProxyMode {
    #[default]
    System,
    None,
    Manual,
}

impl ProxyMode {
    pub fn from_index(index: i32) -> Self {
        match index {
            1 => Self::None,
            2 => Self::Manual,
            _ => Self::System,
        }
    }

    pub fn index(self) -> i32 {
        match self {
            Self::System => 0,
            Self::None => 1,
            Self::Manual => 2,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::None => "none",
            Self::Manual => "manual",
        }
    }

    fn parse(text: &str) -> Self {
        match text {
            "none" => Self::None,
            "manual" => Self::Manual,
            _ => Self::System,
        }
    }
}

/// Which interface hostnames are resolved on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DnsMode {
    #[default]
    Fastest,
    System,
}

impl DnsMode {
    pub fn from_index(index: i32) -> Self {
        if index == 1 { Self::System } else { Self::Fastest }
    }

    pub fn index(self) -> i32 {
        match self {
            Self::Fastest => 0,
            Self::System => 1,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fastest => "fastest",
            Self::System => "system",
        }
    }

    fn parse(text: &str) -> Self {
        if text == "system" { Self::System } else { Self::Fastest }
    }
}

/// The chunk-size choices the Advanced page offers, as an index so the UI can
/// stay a plain dropdown. `None` is "Automatic", which lets the engine scale
/// the chunk to the file and the connection count.
pub fn chunk_size_from_index(index: i32) -> Option<u64> {
    match index {
        1 => Some(1 << 20),
        2 => Some(4 << 20),
        3 => Some(16 << 20),
        4 => Some(64 << 20),
        _ => None,
    }
}

pub fn chunk_size_index(size: Option<u64>) -> i32 {
    match size {
        Some(s) if s == 1 << 20 => 1,
        Some(s) if s == 4 << 20 => 2,
        Some(s) if s == 16 << 20 => 3,
        Some(s) if s == 64 << 20 => 4,
        _ => 0,
    }
}

#[derive(Clone, Debug)]
pub struct Settings {
    /// What to call traffic the engine cannot attribute to one interface.
    /// Set once at startup from the default route; not a user preference.
    pub unattributed_lane: String,

    // ---- General
    pub destination: PathBuf,
    /// Where partial files live while a transfer runs.
    ///
    /// `None` keeps them beside the destination as `name.part`, which makes
    /// the final move a rename within one directory. A folder keeps anything
    /// half-finished out of the download folder entirely, at the cost of a
    /// copy if the two are on different filesystems.
    ///
    /// HTTP transfers only. A torrent writes into the download folder start to
    /// finish, because one that keeps seeding is still serving those files and
    /// they cannot be moved out from under it.
    pub incomplete_dir: Option<PathBuf>,
    pub ask_where: bool,
    pub max_concurrent: usize,
    pub reveal_on_done: bool,
    pub sound_on_done: bool,
    pub keep_completed: bool,
    pub launch_at_login: bool,
    pub menu_bar: bool,
    /// What the menu bar entry shows beside its icon.
    pub menu_bar_style: MenuBarStyle,
    /// Whether Braid is registered with the system for `magnet:` links and
    /// `.torrent` files. Stored so the switch can be restored, but always
    /// re-checked against the system on load: the registration lives outside
    /// this file and another app can take it.
    pub handle_magnets: bool,
    /// Stay in the swarm once a torrent has every piece.
    pub seed_after_complete: bool,

    // ---- Network
    /// Interfaces the user allows. Empty means "whatever the OS routes over",
    /// which is not the same as "all of them": it is the unpinned path.
    pub interfaces: Vec<String>,
    /// Per-interface ceilings, by interface name. Absent means unlimited.
    pub interface_limits: BTreeMap<String, u64>,
    pub proxy_mode: ProxyMode,
    pub proxy_server: String,
    pub dns_mode: DnsMode,

    // ---- Bandwidth
    /// `None` is unlimited. Applies to the whole app, not per transfer.
    pub limit: Option<u64>,
    pub upload_limit: Option<u64>,
    /// One flag per three-hour slot, day-major. When the schedule is on, the
    /// download limit applies inside these slots and nowhere else.
    pub schedule_cells: Vec<bool>,
    pub schedule_enabled: bool,
    pub manual_ignores_limit: bool,

    // ---- Integrity
    pub verify_every: bool,
    pub checksum: ChecksumAlgorithm,
    pub refetch_damaged: bool,
    pub durability: Durability,
    pub keep_partial: bool,
    pub retries: u32,

    // ---- Advanced
    pub connections: usize,
    pub chunk_size: Option<u64>,
    pub refresh_links: bool,
    pub refresh_cap: u32,
}

impl Settings {
    pub fn new(destination: PathBuf) -> Self {
        Self {
            unattributed_lane: "network".into(),
            destination,
            incomplete_dir: None,
            ask_where: false,
            max_concurrent: 3,
            reveal_on_done: false,
            sound_on_done: false,
            keep_completed: true,
            launch_at_login: false,
            menu_bar: true,
            menu_bar_style: MenuBarStyle::default(),
            handle_magnets: false,
            seed_after_complete: true,

            interfaces: Vec::new(),
            interface_limits: BTreeMap::new(),
            proxy_mode: ProxyMode::default(),
            proxy_server: String::new(),
            dns_mode: DnsMode::default(),

            limit: None,
            upload_limit: None,
            schedule_cells: vec![false; SCHEDULE_CELLS],
            schedule_enabled: false,
            manual_ignores_limit: false,

            verify_every: true,
            checksum: ChecksumAlgorithm::default(),
            refetch_damaged: true,
            durability: Durability::default(),
            keep_partial: true,
            retries: 5,

            connections: 8,
            chunk_size: None,
            refresh_links: true,
            refresh_cap: 20,
        }
    }

    /// Turn the grid into the windows the scheduler understands.
    ///
    /// Adjacent slots on the same day are merged, so a full evening is one
    /// window rather than four. The engine does not care, but a list of
    /// windows is what gets persisted and read back by a human.
    pub fn windows(&self) -> Vec<TimeWindow> {
        let mut windows = Vec::new();
        for day in 0..7u8 {
            let Some(weekday) = Weekday::from_index(day) else { continue };
            let mut slot = 0;
            while slot < SLOTS_PER_DAY {
                if !self.schedule_cells[day as usize * SLOTS_PER_DAY + slot] {
                    slot += 1;
                    continue;
                }
                let start = slot;
                while slot < SLOTS_PER_DAY
                    && self.schedule_cells[day as usize * SLOTS_PER_DAY + slot]
                {
                    slot += 1;
                }
                let start_min = (start as u32 * SLOT_HOURS * 60) as u16;
                // A run reaching the end of the day ends at 23:59 rather than
                // 24:00, which the window type would read as wrapping midnight.
                let end_min = ((slot as u32 * SLOT_HOURS * 60).min(24 * 60 - 1)) as u16;
                windows.push(TimeWindow::new(vec![weekday], start_min, end_min, self.limit));
            }
        }
        windows
    }

    /// The schedule as the engine sees it.
    ///
    /// With the schedule off, the limit applies at all times. With it on, the
    /// limit applies inside the painted windows and nowhere else: which is
    /// what "Outside these windows Braid runs unlimited" on the Bandwidth page
    /// promises.
    pub fn schedule(&self) -> Schedule {
        if !self.schedule_enabled {
            return Schedule::with_default(self.limit);
        }
        let mut schedule = Schedule::with_default(None);
        for window in self.windows() {
            schedule.push(window);
        }
        schedule
    }

    /// The ceiling for one interface, or `None` for unlimited.
    pub fn interface_limit(&self, name: &str) -> Option<u64> {
        self.interface_limits.get(name).copied()
    }

    /// Fill in a default evening window, Monday to Friday, 18:00 to midnight.
    /// The grid is still the way to shape it; this is the starting point the
    /// Bandwidth page's "Add window" button lays down.
    pub fn add_default_window(&mut self) {
        for day in 0..5usize {
            for slot in 6..SLOTS_PER_DAY {
                self.schedule_cells[day * SLOTS_PER_DAY + slot] = true;
            }
        }
    }

    pub fn clear_schedule(&mut self) {
        self.schedule_cells = vec![false; SCHEDULE_CELLS];
    }

    // -------------------------------------------------------------- storage

    /// `~/Library/Application Support/Braid/settings.conf` and its equivalents.
    pub fn path() -> Option<PathBuf> {
        #[cfg(not(target_os = "windows"))]
        let home = std::env::var_os("HOME").map(PathBuf::from);
        #[cfg(target_os = "macos")]
        let dir = home.map(|h| h.join("Library/Application Support/Braid"));
        #[cfg(target_os = "windows")]
        let dir = std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("Braid"));
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        let dir = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| home.map(|h| h.join(".config")))
            .map(|c| c.join("braid"));
        dir.map(|d| d.join("settings.conf"))
    }

    /// Load from disk, falling back to defaults for anything missing. A
    /// corrupt or partial file costs the settings it could not parse and
    /// nothing else: it never stops the app from starting.
    pub fn load(destination: PathBuf) -> Self {
        let mut settings = Self::new(destination);
        let Some(path) = Self::path() else { return settings };
        let Ok(text) = std::fs::read_to_string(&path) else { return settings };
        settings.apply_config(&text);
        settings
    }

    fn apply_config(&mut self, text: &str) {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else { continue };
            let (key, value) = (key.trim(), value.trim());
            match key {
                "destination" => self.destination = PathBuf::from(value),
                "incomplete_dir" => {
                    self.incomplete_dir = (!value.is_empty()).then(|| PathBuf::from(value));
                }
                "ask_where" => self.ask_where = value == "true",
                "max_concurrent" => {
                    if let Ok(v) = value.parse() {
                        self.max_concurrent = v;
                    }
                }
                "reveal_on_done" => self.reveal_on_done = value == "true",
                "sound_on_done" => self.sound_on_done = value == "true",
                "keep_completed" => self.keep_completed = value == "true",
                "launch_at_login" => self.launch_at_login = value == "true",
                "menu_bar" => self.menu_bar = value == "true",
                "menu_bar_style" => {
                    if let Some(style) = MenuBarStyle::parse(value) {
                        self.menu_bar_style = style;
                    }
                }

                "interfaces" => {
                    self.interfaces = value
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect();
                }
                "interface_limits" => {
                    self.interface_limits = value
                        .split(',')
                        .filter_map(|pair| pair.split_once(':'))
                        .filter_map(|(name, rate)| {
                            rate.trim().parse().ok().map(|r| (name.trim().to_string(), r))
                        })
                        .collect();
                }
                "proxy_mode" => self.proxy_mode = ProxyMode::parse(value),
                "proxy_server" => self.proxy_server = value.to_string(),
                "dns_mode" => self.dns_mode = DnsMode::parse(value),

                "limit" => self.limit = value.parse().ok().filter(|v| *v > 0),
                "upload_limit" => self.upload_limit = value.parse().ok().filter(|v| *v > 0),
                "schedule_cells" => {
                    let cells: Vec<bool> = value.chars().map(|c| c == '1').collect();
                    if cells.len() == SCHEDULE_CELLS {
                        self.schedule_cells = cells;
                    }
                }
                "schedule_enabled" => self.schedule_enabled = value == "true",
                "manual_ignores_limit" => self.manual_ignores_limit = value == "true",
                "handle_magnets" => self.handle_magnets = value == "true",
                "seed_after_complete" => self.seed_after_complete = value == "true",

                "verify_every" => self.verify_every = value == "true",
                "checksum" => {
                    if let Some(algorithm) = ChecksumAlgorithm::parse(value) {
                        self.checksum = algorithm;
                    }
                }
                "refetch_damaged" => self.refetch_damaged = value == "true",
                "durability" => {
                    if let Some(mode) = Durability::parse(value) {
                        self.durability = mode;
                    }
                }
                "keep_partial" => self.keep_partial = value == "true",
                "retries" => {
                    if let Ok(v) = value.parse() {
                        self.retries = v;
                    }
                }

                "connections" => {
                    if let Ok(v) = value.parse() {
                        self.connections = v;
                    }
                }
                "chunk_size" => self.chunk_size = value.parse().ok().filter(|v| *v > 0),
                "refresh_links" => self.refresh_links = value == "true",
                "refresh_cap" => {
                    if let Ok(v) = value.parse() {
                        self.refresh_cap = v;
                    }
                }
                _ => {}
            }
        }
    }

    fn to_config(&self) -> String {
        let cells: String =
            self.schedule_cells.iter().map(|on| if *on { '1' } else { '0' }).collect();
        let limits: Vec<String> =
            self.interface_limits.iter().map(|(n, r)| format!("{n}:{r}")).collect();
        let mut out = String::from("# Braid settings. Written by the app; safe to edit by hand.\n");
        let push = |out: &mut String, key: &str, value: String| {
            out.push_str(key);
            out.push_str(" = ");
            out.push_str(&value);
            out.push('\n');
        };
        push(&mut out, "destination", self.destination.display().to_string());
        push(
            &mut out,
            "incomplete_dir",
            self.incomplete_dir.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
        );
        push(&mut out, "ask_where", self.ask_where.to_string());
        push(&mut out, "max_concurrent", self.max_concurrent.to_string());
        push(&mut out, "reveal_on_done", self.reveal_on_done.to_string());
        push(&mut out, "sound_on_done", self.sound_on_done.to_string());
        push(&mut out, "keep_completed", self.keep_completed.to_string());
        push(&mut out, "launch_at_login", self.launch_at_login.to_string());
        push(&mut out, "menu_bar", self.menu_bar.to_string());
        push(&mut out, "menu_bar_style", self.menu_bar_style.as_str().to_string());
        push(&mut out, "interfaces", self.interfaces.join(","));
        push(&mut out, "interface_limits", limits.join(","));
        push(&mut out, "proxy_mode", self.proxy_mode.as_str().to_string());
        push(&mut out, "proxy_server", self.proxy_server.clone());
        push(&mut out, "dns_mode", self.dns_mode.as_str().to_string());
        push(&mut out, "limit", self.limit.unwrap_or(0).to_string());
        push(&mut out, "upload_limit", self.upload_limit.unwrap_or(0).to_string());
        push(&mut out, "schedule_cells", cells);
        push(&mut out, "schedule_enabled", self.schedule_enabled.to_string());
        push(&mut out, "manual_ignores_limit", self.manual_ignores_limit.to_string());
        push(&mut out, "handle_magnets", self.handle_magnets.to_string());
        push(&mut out, "seed_after_complete", self.seed_after_complete.to_string());
        push(&mut out, "verify_every", self.verify_every.to_string());
        push(&mut out, "checksum", self.checksum.as_str().to_string());
        push(&mut out, "refetch_damaged", self.refetch_damaged.to_string());
        push(&mut out, "durability", self.durability.as_str().to_string());
        push(&mut out, "keep_partial", self.keep_partial.to_string());
        push(&mut out, "retries", self.retries.to_string());
        push(&mut out, "connections", self.connections.to_string());
        push(&mut out, "chunk_size", self.chunk_size.unwrap_or(0).to_string());
        push(&mut out, "refresh_links", self.refresh_links.to_string());
        push(&mut out, "refresh_cap", self.refresh_cap.to_string());
        out
    }

    /// Write through a temporary file and rename, so an interrupted save
    /// leaves the previous settings rather than half of the new ones.
    pub fn save(&self) {
        let Some(path) = Self::path() else { return };
        if let Some(parent) = path.parent()
            && std::fs::create_dir_all(parent).is_err()
        {
            return;
        }
        let temp = path.with_extension("conf.tmp");
        if std::fs::write(&temp, self.to_config()).is_ok() {
            let _ = std::fs::rename(&temp, &path);
        }
    }
}

impl Settings {
    /// The engine configuration these settings describe. Pushed on every
    /// change, so there is one translation from preferences to engine rather
    /// than one per callback.
    pub fn engine_config(&self) -> dl_core::EngineConfig {
        dl_core::EngineConfig {
            max_concurrent: self.max_concurrent.max(1),
            chunk_size: self.chunk_size,
            durability: self.durability,
            staging: match &self.incomplete_dir {
                Some(folder) => dl_core::store::Staging::folder(folder),
                None => dl_core::store::Staging::Alongside,
            },
            interface_limits: self.interface_limits.clone(),
            verify_existing: self.refetch_damaged,
            verify_every: self.verify_every.then_some(self.checksum),
            unattributed_lane: self.unattributed_lane.clone(),
            keep_partial: self.keep_partial,
            retries: self.retries,
            seed_after_complete: self.seed_after_complete,
        }
    }

    /// The HTTP configuration these settings describe.
    ///
    /// Read per transfer rather than pushed, because a client is built per
    /// transfer anyway and rebuilding every existing one on a proxy change
    /// would drop connections that are working.
    pub fn http_config(&self) -> dl_net::HttpConfig {
        dl_net::HttpConfig {
            proxy: match self.proxy_mode {
                ProxyMode::System => dl_net::ProxyMode::System,
                ProxyMode::None => dl_net::ProxyMode::None,
                ProxyMode::Manual if !self.proxy_server.trim().is_empty() => {
                    dl_net::ProxyMode::Manual(self.proxy_server.trim().to_string())
                }
                // "Manual" with an empty field is a half-finished choice, not
                // an instruction to go direct.
                ProxyMode::Manual => dl_net::ProxyMode::System,
            },
            ..Default::default()
        }
    }
}

pub type Shared = Arc<RwLock<Settings>>;

/// Change one field, persist the result, and hand the engine the parts of it
/// the engine owns.
///
/// Every callback in the settings window goes through here, so there is one
/// place that can forget to save and one place that can forget to apply.
pub fn edit(settings: &Shared, change: impl FnOnce(&mut Settings)) {
    let applied = {
        let Ok(mut current) = settings.write() else { return };
        change(&mut current);
        current.save();
        current.engine_config()
    };
    if let Some(engine) = ENGINE.get() {
        engine.set_config(applied);
    }
    apply_upload_limit(settings);
}

/// Push the upload ceiling at the engine.
///
/// Separate from [`Settings::engine_config`] because it is a live budget
/// rather than a configuration value: a running torrent reads it every tick,
/// where a running download keeps the config it started with.
pub fn apply_upload_limit(settings: &Shared) {
    let Some(engine) = ENGINE.get() else { return };
    let Ok(current) = settings.read() else { return };
    let want = current.upload_limit.unwrap_or(0);
    // Only on a change: `set_rate` refills the bucket, so setting the same
    // value repeatedly would hand out a fresh burst each time.
    if engine.upload_budget().rate() != want {
        engine.upload_budget().set_rate(want);
    }
}

/// The engine the settings apply to. Set once at startup; `edit` needs it and
/// threading a handle through every closure in the settings window bought
/// nothing but noise.
static ENGINE: std::sync::OnceLock<dl_core::Engine> = std::sync::OnceLock::new();

pub fn bind_engine(engine: dl_core::Engine) {
    let _ = ENGINE.set(engine);
}

/// Read a rate the user typed. Bare numbers mean MB/s, which is what people
/// mean by "limit to 20". Anything unparseable returns `None` so the caller can
/// leave the previous value alone rather than silently setting zero: a limit
/// of zero stops every transfer, which is not what a typo should do.
pub fn parse_rate(text: &str) -> Option<u64> {
    let text = text.trim().to_ascii_lowercase();
    let text = text.strip_suffix("/s").unwrap_or(&text).trim().to_string();
    let digits: String = text.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    let value: f64 = digits.parse().ok()?;
    if value <= 0.0 || !value.is_finite() {
        return None;
    }
    let unit = text[digits.len()..].trim();
    let scale = match unit {
        "" | "m" | "mb" | "mib" => 1u64 << 20,
        "k" | "kb" | "kib" => 1 << 10,
        "g" | "gb" | "gib" => 1 << 30,
        "b" => 1,
        _ => return None,
    };
    Some((value * scale as f64) as u64)
}

/// Render a rate back into the field it came from.
pub fn format_rate(bytes_per_sec: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes_per_sec as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if (value - value.round()).abs() < 0.05 {
        format!("{} {}", value.round() as u64, UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// A rate as a field shows it, with the unit the Bandwidth page's labels use.
pub fn format_rate_per_sec(bytes_per_sec: u64) -> String {
    format!("{}/s", format_rate(bytes_per_sec))
}

/// Apply the schedule to the global budget, for as long as the app runs.
///
/// The budget is the single place a rate is enforced, so the schedule does not
/// need to reach the transfers themselves: it only has to keep this one
/// number correct.
pub fn spawn_scheduler(settings: Shared, budget: Arc<Budget>) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(SCHEDULE_TICK);
        let mut applied: Option<u64> = None;
        loop {
            ticker.tick().await;
            let rate = {
                let Ok(settings) = settings.read() else { return };
                settings.schedule().rate_at(now_local())
            };
            // Only on a change: `set_rate` refills the bucket, and doing that
            // three times a minute would quietly defeat the limit it enforces.
            if applied != Some(rate) {
                budget.set_rate(rate);
                applied = Some(rate);
                tracing::debug!(rate, "bandwidth limit applied");
            }
        }
    });
}

/// Wall-clock weekday and minute.
///
/// Deliberately not a date library: the schedule is expressed in local
/// wall-clock terms, so the only thing that matters is what the clock on the
/// wall says, including across a daylight-saving change.
fn now_local() -> LocalTime {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let offset = local_offset_seconds();
    let local = secs as i64 + offset;
    let days = local.div_euclid(86_400);
    let within = local.rem_euclid(86_400);
    // 1 Jan 1970 was a Thursday, which is index 3 with Monday at 0.
    let weekday = Weekday::from_index(((days + 3).rem_euclid(7)) as u8).unwrap_or(Weekday::Monday);
    LocalTime::new(weekday, (within / 3600) as u8, ((within % 3600) / 60) as u8)
}

#[cfg(unix)]
fn local_offset_seconds() -> i64 {
    // SAFETY: `localtime_r` writes into the `tm` we own, and `time` takes a
    // null pointer to mean "now". Neither retains anything.
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&now, &mut tm).is_null() {
            return 0;
        }
        tm.tm_gmtoff
    }
}

#[cfg(not(unix))]
fn local_offset_seconds() -> i64 {
    0
}

/// Reveal a finished file in the system file manager.
pub fn reveal(path: &Path) {
    #[cfg(target_os = "macos")]
    let command = ("open", vec!["-R".into(), path.display().to_string()]);
    #[cfg(target_os = "windows")]
    let command = ("explorer", vec![format!("/select,{}", path.display())]);
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let command = ("xdg-open", vec![path.parent().unwrap_or(path).display().to_string()]);
    let _ = std::process::Command::new(command.0).args(command.1).spawn();
}

/// The system's own completion sound. Nothing is bundled: an app that ships
/// its own alert sound is an app that ignores the user's chosen one.
pub fn play_completion_sound() {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("afplay").arg("/System/Library/Sounds/Glass.aiff").spawn();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("canberra-gtk-play").args(["-i", "complete"]).spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> Settings {
        Settings::new(PathBuf::from("/tmp"))
    }

    #[test]
    fn a_bare_number_is_read_as_megabytes() {
        assert_eq!(parse_rate("20"), Some(20 << 20));
        assert_eq!(parse_rate("20 MB/s"), Some(20 << 20));
        assert_eq!(parse_rate("500 KB/s"), Some(500 << 10));
        assert_eq!(parse_rate("1.5 GB"), Some(1610612736));
    }

    #[test]
    fn a_typo_leaves_the_limit_alone_rather_than_stopping_everything() {
        // Returning 0 here would be read as a limit of zero bytes a second,
        // which halts every transfer in the app.
        assert_eq!(parse_rate(""), None);
        assert_eq!(parse_rate("fast"), None);
        assert_eq!(parse_rate("0"), None);
        assert_eq!(parse_rate("-4"), None);
        assert_eq!(parse_rate("12 furlongs"), None);
    }

    #[test]
    fn rates_round_trip_through_the_field() {
        // Both spellings, because the field shows the "/s" form and parses
        // back whatever the user leaves in it.
        for rate in [1 << 10, 20 << 20, 1536 << 10] {
            for text in [format_rate(rate), format_rate_per_sec(rate)] {
                assert_eq!(parse_rate(&text), Some(rate), "{text} did not round-trip");
            }
        }
        assert_eq!(format_rate_per_sec(20 << 20), "20 MB/s");
    }

    #[test]
    fn an_empty_grid_produces_no_windows() {
        assert!(settings().windows().is_empty());
    }

    #[test]
    fn adjacent_slots_merge_into_one_window() {
        let mut s = settings();
        s.schedule_enabled = true;
        // Monday 00:00 to 09:00 as three consecutive slots.
        for slot in 0..3 {
            s.schedule_cells[slot] = true;
        }
        let windows = s.windows();
        assert_eq!(windows.len(), 1, "three adjacent slots are one window: {windows:?}");
        assert_eq!(windows[0].start, 0);
        assert_eq!(windows[0].end, 9 * 60);
    }

    #[test]
    fn a_gap_splits_the_day_into_two_windows() {
        let mut s = settings();
        s.schedule_cells[0] = true;
        s.schedule_cells[2] = true;
        assert_eq!(s.windows().len(), 2);
    }

    #[test]
    fn a_run_to_midnight_does_not_wrap() {
        // 24:00 would be read as a window crossing into the next day, which
        // would silently limit Tuesday morning as well.
        let mut s = settings();
        for slot in 0..SLOTS_PER_DAY {
            s.schedule_cells[slot] = true;
        }
        let windows = s.windows();
        assert_eq!(windows.len(), 1);
        assert!(!windows[0].wraps_midnight(), "a full day must not wrap: {:?}", windows[0]);
    }

    #[test]
    fn a_schedule_limits_inside_its_windows_and_nowhere_else() {
        // What the Bandwidth page promises in so many words: "Outside these
        // windows Braid runs unlimited."
        let mut s = settings();
        s.limit = Some(5 << 20);
        s.schedule_enabled = true;
        s.schedule_cells[0] = true;

        let schedule = s.schedule();
        assert_eq!(schedule.rate_at(LocalTime::new(Weekday::Monday, 1, 0)), 5 << 20);
        // Zero is the unlimited sentinel `Budget` uses, not a rate of nothing.
        assert_eq!(schedule.rate_at(LocalTime::new(Weekday::Monday, 12, 0)), 0);
    }

    #[test]
    fn a_disabled_schedule_applies_the_limit_at_all_times() {
        let mut s = settings();
        s.limit = Some(5 << 20);
        s.schedule_cells[0] = true;
        assert!(!s.schedule_enabled);
        assert_eq!(s.schedule().rate_at(LocalTime::new(Weekday::Monday, 1, 0)), 5 << 20);
        assert_eq!(s.schedule().rate_at(LocalTime::new(Weekday::Monday, 12, 0)), 5 << 20);
    }

    #[test]
    fn the_default_window_covers_weekday_evenings_only() {
        let mut s = settings();
        s.add_default_window();
        s.schedule_enabled = true;
        let windows = s.windows();
        assert_eq!(windows.len(), 5, "one per weekday: {windows:?}");
        assert_eq!(windows[0].start, 18 * 60);
        s.clear_schedule();
        assert!(s.windows().is_empty());
    }

    #[test]
    fn settings_survive_a_write_and_read() {
        let mut original = settings();
        original.limit = Some(20 << 20);
        original.durability = Durability::Safe;
        original.checksum = ChecksumAlgorithm::Sha256;
        original.proxy_mode = ProxyMode::Manual;
        original.proxy_server = "127.0.0.1:8080".into();
        original.interfaces = vec!["en0".into(), "en1".into()];
        original.interface_limits.insert("en1".into(), 5 << 20);
        original.retries = 9;
        original.chunk_size = Some(16 << 20);
        original.add_default_window();

        let mut restored = settings();
        restored.apply_config(&original.to_config());

        assert_eq!(restored.limit, original.limit);
        assert_eq!(restored.durability, Durability::Safe);
        assert_eq!(restored.checksum, ChecksumAlgorithm::Sha256);
        assert_eq!(restored.proxy_mode, ProxyMode::Manual);
        assert_eq!(restored.proxy_server, "127.0.0.1:8080");
        assert_eq!(restored.interfaces, original.interfaces);
        assert_eq!(restored.interface_limit("en1"), Some(5 << 20));
        assert_eq!(restored.retries, 9);
        assert_eq!(restored.chunk_size, Some(16 << 20));
        assert_eq!(restored.schedule_cells, original.schedule_cells);
    }

    #[test]
    fn a_truncated_config_keeps_the_defaults_for_what_it_lost() {
        // A half-written file must cost only the settings it could not carry.
        let mut s = settings();
        s.apply_config("limit = 1048576\nschedule_cells = 1010\ngarbage\nretries = nope\n");
        assert_eq!(s.limit, Some(1 << 20));
        assert_eq!(s.schedule_cells.len(), SCHEDULE_CELLS, "a short grid is rejected whole");
        assert!(!s.schedule_cells.iter().any(|c| *c));
        assert_eq!(s.retries, 5);
    }

    #[test]
    fn a_zero_limit_on_disk_reads_as_unlimited() {
        // `limit = 0` is how "Unlimited" is written; reading it as a rate of
        // zero would stop every transfer on the next launch.
        let mut s = settings();
        s.apply_config("limit = 0\nupload_limit = 0\nchunk_size = 0\n");
        assert_eq!(s.limit, None);
        assert_eq!(s.upload_limit, None);
        assert_eq!(s.chunk_size, None);
    }

    #[test]
    fn the_menu_bar_says_nothing_while_nothing_is_transferring() {
        // A menu bar entry permanently reading "0 B/s" is one the user turns
        // off, and the label's width drags everything left of it on the bar.
        for style in [MenuBarStyle::IconOnly, MenuBarStyle::Speed, MenuBarStyle::Count] {
            assert_eq!(style.label(0, "0 B/s"), "", "{style:?}");
        }
    }

    #[test]
    fn each_style_shows_what_it_says_it_does() {
        assert_eq!(MenuBarStyle::IconOnly.label(3, "5 MB/s"), "");
        assert_eq!(MenuBarStyle::Speed.label(3, "5 MB/s"), "5 MB/s");
        assert_eq!(MenuBarStyle::Count.label(3, "5 MB/s"), "3");
    }

    #[test]
    fn the_style_round_trips_through_the_dropdown_and_the_file() {
        for style in [MenuBarStyle::IconOnly, MenuBarStyle::Speed, MenuBarStyle::Count] {
            assert_eq!(MenuBarStyle::from_index(style.index()), style);
            let mut restored = settings();
            restored.apply_config(&format!("menu_bar_style = {}\n", style.as_str()));
            assert_eq!(restored.menu_bar_style, style);
        }
    }

    #[test]
    fn an_unknown_style_on_disk_keeps_the_default() {
        let mut s = settings();
        s.menu_bar_style = MenuBarStyle::Speed;
        s.apply_config("menu_bar_style = semaphore\n");
        assert_eq!(s.menu_bar_style, MenuBarStyle::Speed, "a typo must not silently reset it");
    }
}
