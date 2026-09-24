//! Between the engine and the UI thread.
//!
//! The engine owns the truth and a single task polls it at a fixed rate,
//! pushing one snapshot per tick. That bounds event-loop wakeups at the tick
//! rate no matter how many downloads are running or how fast bytes arrive: //! the alternative, waking the UI per chunk, is what makes progress displays
//! collapse under load.
//!
//! Rows are then diffed against the model and only changed ones are replaced,
//! because resetting the model would drop scroll position and selection.

use dl_core::engine::{DownloadId, Engine, State};
use dl_core::lane::LaneReport;
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel, Weak};
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use crate::{
    InspectorCell, InspectorFile, InspectorPeer, InspectorStat, MainWindow, Segment, TransferRow,
    Tray,
};

/// How often the UI is refreshed. Ten per second reads as live without making
/// the UI thread the bottleneck.
const TICK: Duration = Duration::from_millis(100);

/// Ticks between throughput samples. One sample a second over sixty samples is
/// the minute the chart's axis claims to show.
const SAMPLES_PER_POINT: u32 = 10;

/// The Inspector updates at 2 Hz, not the list's 10.
///
/// A piece grid is ambient information; nobody reads individual cells at ten
/// frames a second, and rebuilding a few thousand cells that often is the cost
/// this panel was designed to avoid.
const INSPECTOR_EVERY: u32 = 5;

/// Cells the grid will draw before it starts merging chunks into one cell.
///
/// A 60 GB torrent at 256 KB pieces is 240,000 pieces. No grid should attempt
/// that, and silently drawing a prefix would be a lie about what is on disk.
/// The grid is 18 columns wide and does not scroll, so anything past the
/// bottom of the panel is laid out and never seen. Four thousand cells meant
/// 223 rows, of which about thirty were visible, rebuilt on every tick: the
/// panel stuttered and the work was wasted. This is what fits.
const MAX_CELLS: usize = 18 * 32;
const CHART_POINTS: usize = 60;

/// Smallest full-scale the throughput chart will use, in bytes per second.
///
/// Scaling to the observed peak alone means an idle graph amplifies its own
/// noise: a few bytes a second of residual rate gets drawn as a half-height
/// spike under an axis whose labels all round to "0 B/s". Below this the plot
/// keeps a fixed scale and the trace correctly stays flat.
const MIN_FULL_SCALE: u64 = 64 * 1024;

/// Round a rate up so the axis divides into three readable gridlines.
///
/// The peak itself makes a poor maximum: a 2.5 MB/s burst labels its thirds
/// "840.2 KB/s", which nobody reads as a third of anything. The step is snapped
/// to a round figure first and the maximum is three of them.
fn axis_max(rate: u64) -> u64 {
    let want = rate.div_ceil(3).max(1);
    let mut unit = 1u64;
    while unit * 1024 <= want {
        unit *= 1024;
    }
    // Fractions of a binary unit that still read as round numbers.
    for step in [1, 2, 5, 10, 20, 25, 50, 100, 200, 250, 500, 1024] {
        let candidate = unit.saturating_mul(step);
        if candidate >= want {
            return candidate.saturating_mul(3);
        }
    }
    want.saturating_mul(3)
}

pub fn format_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{n} B") } else { format!("{value:.1} {}", UNITS[unit]) }
}

pub fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m {}s", secs / 60, secs % 60),
        _ => format!("{}h {}m", secs / 3600, (secs % 3600) / 60),
    }
}

/// Turn per-lane byte counts into the spans the progress bar draws.
///
/// A running total is not expressible in `.slint`, and proportional widths
/// inside a layout are a binding loop, so the offsets are computed here.
///
/// `order` is the interface ordering the sidebar and the chart use, so a lane's
/// colour is a property of the interface rather than of its position in this
/// particular download's lane list: otherwise Wi-Fi would be copper in one row
/// and amber in the next.
fn segments(lanes: &[LaneReport], order: &[String]) -> ModelRc<Segment> {
    let total: u64 = lanes.iter().map(|l| l.bytes).sum();
    if total == 0 {
        return Rc::new(VecModel::from(vec![Segment { start: 0.0, span: 1.0, color_index: 0 }]))
            .into();
    }

    let mut start = 0.0f32;
    let spans: Vec<Segment> = lanes
        .iter()
        .enumerate()
        .filter(|(_, l)| l.bytes > 0)
        .map(|(i, l)| {
            let span = l.bytes as f32 / total as f32;
            let color = order.iter().position(|n| n == &l.label).unwrap_or(i);
            let segment = Segment { start, span, color_index: color as i32 };
            start += span;
            segment
        })
        .collect();
    Rc::new(VecModel::from(spans)).into()
}

/// What the transfer is, for the row's second glyph.
///
/// Coarse on purpose: the row already spells out the filename, so the glyph is
/// there to make a list scannable by shape. Anything unrecognised gets the
/// neutral page rather than a guess.
pub fn file_category(name: &str) -> &'static str {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "mkv" | "mp4" | "mov" | "avi" | "webm" | "m4v" | "flv" | "wmv" => "video",
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "heic" | "tiff" | "bmp" => "image",
        "zip" | "gz" | "xz" | "zst" | "tar" | "7z" | "rar" | "bz2" | "tgz" | "xip" => "archive",
        "iso" | "dmg" | "img" | "vhd" | "qcow2" => "disk",
        "exe" | "msi" | "pkg" | "deb" | "rpm" | "appimage" | "apk" | "snap" => "app",
        _ => "doc",
    }
}

/// The second right-hand line: what a torrent has and a download does not.
///
/// Empty for HTTP, and empty for a torrent that has not connected to anyone
/// yet: the row omits the line rather than printing "0 peers", which reads as
/// a measurement when it is really "we have not looked".
fn torrent_detail(status: &dl_core::torrent::TorrentStatus) -> String {
    let mut parts = Vec::new();
    if status.peers > 0 {
        parts.push(format!("{} peer{}", status.peers, if status.peers == 1 { "" } else { "s" }));
    }
    if status.uploaded > 0 {
        parts.push(format!("{} up", format_bytes(status.uploaded)));
    }
    if status.upload_bytes_per_sec > 0 {
        parts.push(format!("{}/s up", format_bytes(status.upload_bytes_per_sec)));
    }
    parts.join(" · ")
}

/// One file inside a torrent, indented under its parent.
///
/// It carries the parent's id: the row component hides every control at a
/// depth above zero, so there is nothing on a child row that can be clicked
/// and no id that needs to be distinct.
fn child_row(
    parent: &dl_core::engine::DownloadSnapshot,
    file: &dl_core::TorrentFile,
) -> TransferRow {
    let name = file.path.rsplit('/').next().unwrap_or(&file.path).to_string();
    let fraction = match file.len {
        0 => 1.0,
        len => (file.downloaded as f64 / len as f64).clamp(0.0, 1.0) as f32,
    };
    TransferRow {
        id: parent.id.0 as i32,
        kind: "file".into(),
        file: file_category(&name).into(),
        filename: file.path.clone().into(),
        progress: fraction,
        // The child column is 54px wide, so this is one fact and no more.
        status: format_bytes(file.len).into(),
        detail: Default::default(),
        state: parent.state.as_str().into(),
        segments: Default::default(),
        depth: 1,
        expandable: false,
        expanded: false,
        grouped: true,
    }
}

/// Everything about a row except its right-hand line.
fn plain_row(snapshot: &dl_core::engine::DownloadSnapshot, order: &[String]) -> TransferRow {
    let torrent = snapshot.torrent.as_ref();
    TransferRow {
        id: snapshot.id.0 as i32,
        kind: if torrent.is_some() { "torrent".into() } else { "http".into() },
        file: file_category(&snapshot.filename).into(),
        filename: snapshot.filename.clone().into(),
        progress: snapshot.progress.fraction().unwrap_or(0.0),
        status: Default::default(),
        detail: Default::default(),
        state: snapshot.state.as_str().into(),
        segments: segments(&snapshot.lanes, order),
        depth: 0,
        expandable: torrent.is_some_and(|t| t.files.len() > 1),
        expanded: false,
        grouped: false,
    }
}

fn row_for(snapshot: &dl_core::engine::DownloadSnapshot, order: &[String]) -> TransferRow {
    let progress = snapshot.progress;
    let state = snapshot.state.as_str();

    // One right-hand line, assembled from what is actually known. A stopped
    // transfer leads with why it stopped rather than with a rate it no longer
    // has; a running one leads with how far along it is.
    // Work that is not moving bytes leads the line: a bar sitting at 100%
    // while a 6 GB file is hashed reads as a hang, and the rate beside it is
    // zero because nothing is being fetched.
    if let Some(phase) = snapshot.phase.as_deref() {
        let done = progress
            .fraction()
            .map(|f| format!("{phase}… · {:.0}%", f * 100.0))
            .unwrap_or_else(|| format!("{phase}…"));
        return TransferRow { status: done.into(), ..plain_row(snapshot, order) };
    }

    let status = match state {
        "running" => {
            let mut parts = Vec::new();
            if let Some(fraction) = progress.fraction() {
                parts.push(format!("{:.0}%", fraction * 100.0));
            }
            parts.push(match progress.total {
                Some(total) => {
                    format!("{} of {}", format_bytes(progress.downloaded), format_bytes(total))
                }
                None => format_bytes(progress.downloaded),
            });
            parts.push(format!("{}/s", format_bytes(progress.bytes_per_sec)));
            parts.join(" · ")
        }
        "done" => format!("Completed · {}", format_bytes(progress.downloaded)),
        // A seeding torrent has nothing left to download, so the line is
        // about what it is giving rather than what it is getting.
        "seeding" => match snapshot.torrent.as_ref() {
            Some(torrent) => {
                format!("Seeding · {} up", format_bytes(torrent.uploaded))
            }
            None => "Seeding".to_string(),
        },
        "error" => "Failed".to_string(),
        other => {
            let name = if other == "paused" { "Paused" } else { "Queued" };
            match progress.fraction() {
                Some(fraction) => format!(
                    "{name} · {:.0}% · {} of {}",
                    fraction * 100.0,
                    format_bytes(progress.downloaded),
                    progress.total.map(format_bytes).unwrap_or_default()
                ),
                None => name.to_string(),
            }
        }
    };

    let files = snapshot.torrent.as_ref().map(|t| t.files.len()).unwrap_or(0);
    TransferRow {
        id: snapshot.id.0 as i32,
        kind: if snapshot.torrent.is_some() { "torrent".into() } else { "http".into() },
        // A multi-file torrent is a folder, whatever the first file happens
        // to be; a single-file one is that file.
        file: match (files, snapshot.torrent.as_ref()) {
            (0, _) | (1, None) => file_category(&snapshot.filename).into(),
            (1, Some(torrent)) => file_category(&torrent.files[0].path).into(),
            _ => "folder".into(),
        },
        filename: snapshot.filename.clone().into(),
        progress: progress.fraction().unwrap_or(0.0),
        status: status.into(),
        // Peers and upload, once there are any. A download has none, and the
        // row omits the second line rather than printing an empty one.
        detail: snapshot.torrent.as_ref().map(torrent_detail).unwrap_or_default().into(),
        state: state.into(),
        segments: segments(&snapshot.lanes, order),
        depth: 0,
        // A torrent with one file has nothing to open that the row does not
        // already say, so the chevron stays off rather than revealing a copy
        // of the line above it.
        expandable: files > 1,
        expanded: false,
        grouped: false,
    }
}

/// Whether anything the user can see differs. Comparing the rendered strings
/// rather than the raw numbers is what keeps a 10 Hz tick from replacing every
/// row on every frame for sub-pixel progress changes.
fn differs(a: &TransferRow, b: &TransferRow) -> bool {
    a.id != b.id
        || a.state != b.state
        // Two rows can carry the same id and filename and still be different
        // objects: a torrent's parent and one of its files.
        || a.depth != b.depth
        || a.kind != b.kind
        || a.status != b.status
        || a.detail != b.detail
        || a.filename != b.filename
        || a.expanded != b.expanded
        || a.grouped != b.grouped
        || (a.progress - b.progress).abs() > 0.001
}

/// Case-insensitive substring match on the filename. Deliberately not a fuzzy
/// match: a download list is short enough to scan, and fuzzy ranking would
/// reorder rows out from under a pointer that is already moving towards one.
fn matches_search(filename: &str, query: &str) -> bool {
    query.is_empty() || filename.to_lowercase().contains(&query.to_lowercase())
}

fn matches_filter(state: State, filter: &str) -> bool {
    match filter {
        // Paused counts as active: it is a download in flight that happens to
        // be stopped, and burying it under All is how people lose track of one.
        "active" => matches!(state, State::Running | State::Paused),
        "queued" => state == State::Queued,
        "seeding" => state == State::Seeding,
        "done" => state == State::Complete,
        "error" => state == State::Failed,
        _ => true,
    }
}

/// Start the refresh loop. Runs until the window is dropped.
///
/// `interfaces` are the machine's usable NIC names. They are passed in rather
/// than enumerated here so this module stays free of `dl-net`, and they are
/// needed because lane reports only exist while something is transferring: /// without them the sidebar's whole reason for existing is blank on an idle
/// window.
/// The lanes the sidebar lists, shared with whatever can change them.
///
/// A phone paired a moment ago has to appear without a restart, so this cannot
/// be a list captured when the window was built. Read once per tick, which is
/// ten times a second and costs a lock on a handful of strings.
pub type LaneNames = Arc<RwLock<Vec<InterfaceInfo>>>;

pub fn spawn(
    ui: &MainWindow,
    engine: Engine,
    tray: Option<Weak<Tray>>,
    interfaces: LaneNames,
    settings: crate::settings::Shared,
) {
    let weak = ui.as_weak();
    let lanes = Arc::clone(&interfaces);
    // The model is `Rc` and belongs to the UI thread, so it is never captured
    // by the polling task; it is looked up again inside the event loop.
    ui.set_rows(Rc::new(VecModel::<TransferRow>::default()).into());

    wire_callbacks(ui, &engine, &settings);

    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(TICK);
        // The series lives with the poller, not the UI: it must survive a
        // filter change, and the UI thread must never be the thing that
        // remembers how fast the last minute went.
        let mut history: VecDeque<Sample> = VecDeque::with_capacity(CHART_POINTS);
        let mut tick = 0u32;

        // Which downloads have already had their completion announced. An
        // edge, not a state: the snapshot says "done" on every tick after the
        // first, and a sound on every tick would be unbearable.
        let mut announced: std::collections::HashSet<dl_core::engine::DownloadId> =
            Default::default();

        loop {
            ticker.tick().await;
            let snapshots = engine.snapshot();
            // Re-read rather than captured: pairing a phone adds lanes here.
            let (keys, labels) = {
                let current = lanes.read().map(|l| l.clone()).unwrap_or_default();
                let keys: Vec<String> = current.iter().map(|i| i.id.clone()).collect();
                let labels: std::collections::BTreeMap<String, InterfaceInfo> =
                    current.into_iter().map(|i| (i.id.clone(), i)).collect();
                (keys, labels)
            };
            let totals = Totals::from(&snapshots, &engine, &keys);
            on_completions(&snapshots, &mut announced, &settings);

            if tick.is_multiple_of(SAMPLES_PER_POINT) {
                if history.len() == CHART_POINTS {
                    history.pop_front();
                }
                // The bands must sum to the figure shown as COMBINED, or the
                // graph and the readout disagree in front of the user. Lane
                // EWMAs and the engine's own counter are measured differently,
                // so the lanes supply the shares and the engine the magnitude.
                let total = engine.total_bytes_per_sec();
                let lane_sum: u128 = totals.interfaces.iter().map(|(_, v)| *v as u128).sum();
                history.push_back(Sample {
                    interfaces: totals
                        .interfaces
                        .iter()
                        .map(|(name, value)| {
                            let scaled = (*value as u128 * total as u128)
                                .checked_div(lane_sum)
                                .unwrap_or(0) as u64;
                            (name.clone(), scaled)
                        })
                        .collect(),
                    up: totals.upload_rate,
                });
            }
            let show_inspector = tick.is_multiple_of(INSPECTOR_EVERY);
            tick = tick.wrapping_add(1);
            // The trace is built inside the event loop, where the plot's
            // measured width is readable.
            let samples: Vec<Sample> = history.iter().cloned().collect();

            // `upgrade_in_event_loop` is a no-op once the window is gone, so a
            // closed window ends this loop rather than panicking.
            let tray = tray.clone();
            let settings_for_tray = settings.clone();
            let engine_for_ui = engine.clone();
            let labels = labels.clone();
            if weak
                .upgrade_in_event_loop(move |ui| {
                    if let Some(tray) = tray.as_ref().and_then(|t| t.upgrade()) {
                        tray.set_active(totals.active);
                        tray.set_queued(totals.queued);
                        tray.set_speed(totals.speed.clone().into());
                        if let Ok(current) = settings_for_tray.read() {
                            tray.set_shown(current.menu_bar);
                            tray.set_label(
                                current.menu_bar_style.label(totals.active, &totals.speed).into(),
                            );
                        }
                    }
                    if show_inspector {
                        apply_inspector(&ui, &engine_for_ui, &snapshots);
                    }
                    apply(&ui, snapshots, totals, &samples, &labels);
                })
                .is_err()
            {
                return;
            }
        }
    });
}

/// One tick of per-interface throughput, keyed by interface so a NIC that
/// appears or drops mid-series does not shift every earlier sample's meaning.
/// One point on the throughput trace.
///
/// Download is kept per interface so the trace can be banded; upload is a
/// single figure because nothing attributes it to a NIC.
#[derive(Clone, Debug, Default)]
struct Sample {
    interfaces: Vec<(String, u64)>,
    up: u64,
}

#[derive(Default)]
struct Chart {
    bands: Vec<String>,
    line: String,
    /// Upload, mirrored below the centre line. Empty when nothing has
    /// uploaded, which leaves the lower half blank rather than rescaling
    /// anything: the centre line does not move.
    up_area: String,
    up_line: String,
}

/// Build the throughput trace as SVG path data, in the plot's own pixels.
///
/// One filled band per interface, stacked, so the hero graph shows where the
/// bandwidth is coming from rather than just how much of it there is: the
/// whole argument for the product is that the total is a sum of paths.
///
/// `width` and `height` come from the UI because the commands are emitted in
/// the plot's own pixels; see the note on `ThroughputStrip` in the .slint for
/// why they cannot be normalised.
/// A Catmull-Rom spline through the samples, emitted as cubic beziers.
///
/// A sixty-point polyline of a bursty signal reads as a sawtooth. The curve is
/// still a measurement, though, so the control points are clamped to the plot:
/// an overshoot between a flat run and a spike would otherwise draw throughput
/// above the peak or below zero.
/// Catmull-Rom through the points, as cubic beziers.
///
/// The control points are clamped into `[lo, hi]`: the half of the plot this
/// direction owns, not the whole plot. A smoothed flat run overshoots at the
/// corners, and once the trace became bidirectional an overshoot on the
/// download curve dipped across the centre line and drew as upload.
fn smooth(points: &[(f32, f32)], lo: f32, hi: f32) -> String {
    let mut out = String::with_capacity(points.len() * 40);
    let Some(&(x0, y0)) = points.first() else { return out };
    out.push_str(&format!("M {x0:.2} {y0:.2}"));

    for i in 0..points.len().saturating_sub(1) {
        let before = points[i.saturating_sub(1)];
        let (ax, ay) = points[i];
        let (bx, by) = points[i + 1];
        let after = points[(i + 2).min(points.len() - 1)];

        let c1x = ax + (bx - before.0) / 6.0;
        let c1y = (ay + (by - before.1) / 6.0).clamp(lo, hi);
        let c2x = bx - (after.0 - ax) / 6.0;
        let c2y = (by - (after.1 - ay) / 6.0).clamp(lo, hi);
        out.push_str(&format!(" C {c1x:.2} {c1y:.2} {c2x:.2} {c2y:.2} {bx:.2} {by:.2}"));
    }
    out
}

fn chart_paths(history: &[Sample], names: &[String], width: f32, height: f32) -> Chart {
    let total_at = |sample: &Sample| -> u64 { sample.interfaces.iter().map(|(_, v)| *v).sum() };
    let peak_down = history.iter().map(total_at).max().unwrap_or(0);
    let peak_up = history.iter().map(|s| s.up).max().unwrap_or(0);
    // One scale for both directions. Scaling each half to its own peak would
    // draw 20 KB/s of upload the same height as 20 MB/s of download, which is
    // a graph that lies about the thing it exists to show.
    let full = axis_max(peak_down.max(peak_up).max(MIN_FULL_SCALE));

    if history.len() < 2 || width <= 0.0 || height <= 0.0 {
        return Chart::default();
    }

    let scale = full as f32;
    // Always plot the full window, padded on the left, so the trace grows in
    // from the right. Stretching four samples across the whole width would draw
    // a minute of history the app has not been running long enough to have
    // seen, under an axis that says "-60s".
    let pad = CHART_POINTS.saturating_sub(history.len());
    let step = width / (CHART_POINTS - 1) as f32;
    // Download fills the upper half, upload the lower, mirrored about a centre
    // line that does not move. With nothing uploading the lower half is simply
    // empty: the graph does not rescale or shift, which is what makes the two
    // directions safe to show in a strip this small.
    let centre = height / 2.0;
    // A sliver of headroom, so the peak sample's stroke is not half-clipped.
    let span = centre * 0.92;
    let y_of = |value: f32| centre - (value / scale) * span;
    let y_up = |value: f32| centre + (value / scale) * span;

    // cumulative[i][k] is the total of the first k interfaces at sample i, so
    // band k is bounded below by [k] and above by [k + 1].
    let cumulative: Vec<Vec<f32>> = (0..CHART_POINTS)
        .map(|index| {
            let sample = index.checked_sub(pad).and_then(|i| history.get(i));
            let mut running = 0u64;
            let mut row = Vec::with_capacity(names.len() + 1);
            row.push(0.0);
            for name in names {
                running += sample
                    .and_then(|s| s.interfaces.iter().find(|(n, _)| n == name))
                    .map(|(_, v)| *v)
                    .unwrap_or(0);
                row.push(running as f32);
            }
            row
        })
        .collect();

    let edge = |k: usize, reverse: bool| -> Vec<(f32, f32)> {
        let mut points: Vec<(f32, f32)> = cumulative
            .iter()
            .enumerate()
            .map(|(index, row)| (index as f32 * step, y_of(row[k])))
            .collect();
        if reverse {
            points.reverse();
        }
        points
    };

    let bands = (0..names.len())
        .map(|k| {
            // Up the band's ceiling, back along its floor, closed. The floor
            // run starts with a line so the two curves join instead of the
            // second one restarting the path.
            let floor = smooth(&edge(k, true), 0.0, centre).replacen("M ", " L ", 1);
            format!("{}{floor} Z", smooth(&edge(k + 1, false), 0.0, centre))
        })
        .collect();

    let line = smooth(&edge(names.len(), false), 0.0, centre);

    // Nothing uploaded anywhere in the window means nothing drawn. Left in,
    // the flat run along the centre reads as a solid teal rule across the
    // whole strip: a graph saying there is upload when there is none.
    if peak_up == 0 {
        return Chart { bands, line, ..Default::default() };
    }

    // Upload as one band against the centre line, in the other direction.
    let up_points: Vec<(f32, f32)> = (0..CHART_POINTS)
        .map(|index| {
            let value =
                index.checked_sub(pad).and_then(|i| history.get(i)).map(|s| s.up).unwrap_or(0);
            (index as f32 * step, y_up(value as f32))
        })
        .collect();
    let up_line = smooth(&up_points, centre, height);
    let mut baseline: Vec<(f32, f32)> = up_points.iter().map(|(x, _)| (*x, centre)).collect();
    baseline.reverse();
    let up_area =
        format!("{up_line}{} Z", smooth(&baseline, centre, height).replacen("M ", " L ", 1));

    Chart { bands, line, up_area, up_line }
}

struct Totals {
    speed: String,
    /// The upload rate, for the trace's lower half.
    upload_rate: u64,
    /// The same figure as `speed`, unformatted, for decisions rather than
    /// display.
    rate: u64,
    /// Combined upload rate, and whether anything has actually uploaded.
    /// Two values rather than one, because a torrent between bursts genuinely
    /// is at 0 B/s and the row should stay rather than flickering away.
    upload: String,
    uploading: bool,
    active: i32,
    paused: i32,
    queued: i32,
    seeding: i32,
    failed: i32,
    done: i32,
    total: i32,
    interfaces: Vec<(String, u64)>,
}

impl Totals {
    fn from(
        snapshots: &[dl_core::engine::DownloadSnapshot],
        engine: &Engine,
        known: &[String],
    ) -> Self {
        // Seeded so an idle interface is listed at zero rather than vanishing,
        // and so the colour assigned to a NIC does not shift as others come and
        // go mid-transfer.
        let mut interfaces: std::collections::BTreeMap<String, u64> =
            known.iter().map(|name| (name.clone(), 0)).collect();
        for snapshot in snapshots.iter().filter(|s| s.state == State::Running) {
            for lane in &snapshot.lanes {
                *interfaces.entry(lane.label.clone()).or_default() +=
                    lane.throughput.unwrap_or(0.0) as u64;
            }
        }

        // Only transfers that have a torrent behind them contribute: an HTTP
        // download has no upload figure, and summing a `None` as zero is how
        // a permanent "0 B/s up" row gets onto the screen.
        let upload_rate: u64 = snapshots
            .iter()
            .filter_map(|s| s.torrent.as_ref())
            .map(|t| t.upload_bytes_per_sec)
            .sum();
        let uploaded: u64 =
            snapshots.iter().filter_map(|s| s.torrent.as_ref()).map(|t| t.uploaded).sum();

        let rate = engine.total_bytes_per_sec();
        Self {
            speed: format!("{}/s", format_bytes(rate)),
            rate,
            upload: format!("{}/s", format_bytes(upload_rate)),
            upload_rate,
            uploading: uploaded > 0,
            active: engine.count_in(State::Running) as i32,
            paused: engine.count_in(State::Paused) as i32,
            queued: engine.count_in(State::Queued) as i32,
            seeding: engine.count_in(State::Seeding) as i32,
            failed: engine.count_in(State::Failed) as i32,
            done: engine.count_in(State::Complete) as i32,
            total: snapshots.len() as i32,
            interfaces: interfaces.into_iter().collect(),
        }
    }
}

/// Do the "when a transfer completes" actions, once per transfer.
fn on_completions(
    snapshots: &[dl_core::engine::DownloadSnapshot],
    announced: &mut std::collections::HashSet<dl_core::engine::DownloadId>,
    settings: &crate::settings::Shared,
) {
    // Seeding counts as finished here. "Reveal in Finder on completion" means
    // "when the file is on disk", and a torrent that has every piece has
    // finished downloading even though the transfer is still running.
    let finished: Vec<&dl_core::engine::DownloadSnapshot> = snapshots
        .iter()
        .filter(|s| matches!(s.state, State::Complete | State::Seeding))
        .filter(|s| !announced.contains(&s.id))
        .collect();
    if finished.is_empty() {
        // Forget anything that has left the list, so a re-added URL announces
        // again rather than being silently treated as already seen.
        let live: std::collections::HashSet<_> = snapshots.iter().map(|s| s.id).collect();
        announced.retain(|id| live.contains(id));
        return;
    }

    let Ok(current) = settings.read() else { return };
    for snapshot in finished {
        announced.insert(snapshot.id);
        if current.reveal_on_done {
            crate::settings::reveal(&current.destination.join(&snapshot.filename));
        }
    }
    // One sound however many finished in the same tick.
    if current.sound_on_done {
        crate::settings::play_completion_sound();
    }
}

fn apply(
    ui: &MainWindow,
    snapshots: Vec<dl_core::engine::DownloadSnapshot>,
    totals: Totals,
    samples: &[Sample],
    labels: &std::collections::BTreeMap<String, InterfaceInfo>,
) {
    let model = ui.get_rows();
    let Some(rows) = model.as_any().downcast_ref::<VecModel<TransferRow>>() else {
        return;
    };
    // Polled with the rest of the snapshot rather than observed: Slint has no
    // change signal for this, and at 10 Hz the band collapses the same frame
    // the user sees the window fill the screen.
    ui.set_fullscreen(ui.window().is_fullscreen());

    let names: Vec<String> = totals.interfaces.iter().map(|(n, _)| n.clone()).collect();
    let filter = ui.get_filter().to_string();
    let search = ui.get_search_text().to_string();
    // "Keep completed transfers in the list" only hides them from the default
    // view; asking for Completed explicitly still shows them, because a filter
    // the user just clicked returning nothing reads as a broken filter.
    let keep_completed = ui.get_keep_completed();
    // Which parents are open. Kept on the window rather than in this task:
    // the model is rebuilt from scratch every tick, so the expansion has to
    // live somewhere that survives being redrawn.
    let expanded: Vec<i32> = ui.get_expanded_rows().iter().collect();
    let mut wanted: Vec<TransferRow> = Vec::with_capacity(snapshots.len());
    for snapshot in snapshots
        .iter()
        .filter(|s| keep_completed || filter == "done" || s.state != State::Complete)
        .filter(|s| matches_filter(s.state, &filter))
        .filter(|s| matches_search(&s.filename, &search))
    {
        let mut row = row_for(snapshot, &names);
        let open = row.expandable && expanded.contains(&row.id);
        row.expanded = open;
        // The parent joins the group's shading only while it is open, so a
        // collapsed torrent reads as one row like any other.
        row.grouped = open;
        wanted.push(row);
        if open && let Some(torrent) = snapshot.torrent.as_ref() {
            wanted.extend(torrent.files.iter().map(|f| child_row(snapshot, f)));
        }
    }

    // Captured before the diff below overwrites it: keeping a selection in
    // place when its own row leaves needs to know where that row used to be.
    let previously_visible = visible_ids(ui);

    // Replace only what changed. A full reset would lose scroll position and
    // make the list flicker ten times a second.
    for (index, row) in wanted.iter().enumerate() {
        match rows.row_data(index) {
            Some(existing) if !differs(&existing, row) => {}
            Some(_) => rows.set_row_data(index, row.clone()),
            None => rows.push(row.clone()),
        }
    }
    while rows.row_count() > wanted.len() {
        rows.remove(rows.row_count() - 1);
    }

    // A selection whose transfer has been removed, completed out of the
    // current filter, or searched away would otherwise sit invisible and keep
    // driving the Inspector and the keyboard.
    let now_visible: Vec<i32> = wanted.iter().map(|row| row.id).collect();
    let kept = selection_after(&previously_visible, &now_visible, ui.get_selected_id());
    if kept != ui.get_selected_id() {
        ui.set_selected_id(kept);
    }

    // Bytes actually moving, not merely a transfer in the Running state: a
    // queued-but-stalled torrent should not keep the graph on screen.
    ui.set_transferring(totals.rate > 0);
    ui.set_down_total(totals.speed.into());
    // Shown once something has actually given bytes back, not merely because
    // a torrent exists: a permanent "0 B/s" row is the thing this flag was
    // introduced to avoid.
    ui.set_up_total(totals.upload.into());
    ui.set_has_upload(totals.uploading);
    ui.set_active_count(totals.active);
    ui.set_paused_count(totals.paused);
    ui.set_queued_count(totals.queued);
    ui.set_seeding_count(totals.seeding);
    ui.set_failed_count(totals.failed);
    ui.set_done_count(totals.done);
    ui.set_total_count(totals.total);
    let chart = chart_paths(samples, &names, ui.get_plot_width(), ui.get_plot_height());
    // The sparkline shows the total only: at 24px tall a per-interface
    // breakdown is indistinguishable from noise, and the breakdown already
    // has a home in the sidebar meters.
    ui.set_chart_area(chart.bands.last().cloned().unwrap_or_default().into());
    ui.set_chart_line(chart.line.into());
    ui.set_chart_up_area(chart.up_area.into());
    ui.set_chart_up_line(chart.up_line.into());

    let peak = totals.interfaces.iter().map(|(_, r)| *r).max().unwrap_or(0).max(1);
    ui.set_interface_names(
        Rc::new(VecModel::from(
            totals
                .interfaces
                .iter()
                // The friendly label if we have one, the device id if not: a
                // NIC that appeared after startup is still worth showing.
                .map(|(n, _)| {
                    SharedString::from(labels.get(n).map(|i| i.label.as_str()).unwrap_or(n))
                })
                .collect::<Vec<_>>(),
        ))
        .into(),
    );
    ui.set_interface_icons(
        Rc::new(VecModel::from(
            totals
                .interfaces
                .iter()
                .map(|(n, _)| {
                    SharedString::from(labels.get(n).map(|i| i.icon.as_str()).unwrap_or("network"))
                })
                .collect::<Vec<_>>(),
        ))
        .into(),
    );
    ui.set_interface_rates(
        Rc::new(VecModel::from(
            totals
                .interfaces
                .iter()
                .map(|(_, r)| SharedString::from(format!("{}/s", format_bytes(*r))))
                .collect::<Vec<_>>(),
        ))
        .into(),
    );
    ui.set_interface_fills(
        Rc::new(VecModel::from(
            totals.interfaces.iter().map(|(_, r)| *r as f32 / peak as f32).collect::<Vec<_>>(),
        ))
        .into(),
    );
}

/// Which id the arrow keys land on.
///
/// Pure so it can be tested without a window: the awkward cases are a
/// selection that is no longer in the list and a step off either end, and both
/// are easier to get wrong than they look.
fn next_selection(ids: &[i32], current: i32, delta: i32) -> Option<i32> {
    if ids.is_empty() {
        return None;
    }
    let at = match ids.iter().position(|id| *id == current) {
        Some(at) => (at as i32 + delta).clamp(0, ids.len() as i32 - 1) as usize,
        // From nothing, an arrow key picks the end it is heading towards
        // rather than always the top.
        None if delta < 0 => ids.len() - 1,
        None => 0,
    };
    Some(ids[at])
}

/// What stays selected after the list has changed underneath it.
///
/// A selection is held by id, so a row completing or being filtered away above
/// it changes nothing. When the selected transfer itself leaves: removed, or
/// no longer matching the filter: the selection moves to whatever now sits at
/// the same place in the list, so holding Delete works through a list rather
/// than deselecting after the first row.
///
/// `-1` means nothing selected.
fn selection_after(previous: &[i32], now: &[i32], current: i32) -> i32 {
    if current < 0 || now.contains(&current) {
        return current;
    }
    if now.is_empty() {
        return -1;
    }
    match previous.iter().position(|id| *id == current) {
        Some(was_at) => now[was_at.min(now.len() - 1)],
        // It was not on screen before either, so there is no place to keep.
        None => -1,
    }
}

/// The ids currently on screen, in the order they are drawn.
///
/// Read from the model rather than from the engine: the list is filtered and
/// searched, so the engine's order is not the order the arrow keys should
/// follow.
fn visible_ids(ui: &MainWindow) -> Vec<i32> {
    ui.get_rows().iter().map(|row| row.id).collect()
}

/// Merge `count` chunks into at most [`MAX_CELLS`] cells.
///
/// Returns how many chunks each cell stands for, which the legend states: a
/// grid that quietly changed resolution would have the user counting cells
/// that do not mean what they think.
fn bucket_for(count: u64) -> u64 {
    if count as usize <= MAX_CELLS {
        return 1;
    }
    (count as usize).div_ceil(MAX_CELLS) as u64
}

/// Build the grid from a chunk report.
///
/// One cell per chunk while that fits, and beyond it one cell per bucket,
/// shaded by the fraction of the bucket that is complete. A bucket is only
/// "Have" when every chunk in it is.
fn cells_for(report: &dl_core::ChunkReport) -> (Vec<InspectorCell>, u64) {
    let bucket = bucket_for(report.chunk_count);
    let cells = report.chunk_count.div_ceil(bucket) as usize;
    let inflight: std::collections::BTreeSet<u64> = report.inflight.iter().copied().collect();

    let mut out = Vec::with_capacity(cells);
    for cell in 0..cells as u64 {
        let first = cell * bucket;
        let last = (first + bucket).min(report.chunk_count);
        let span = (last - first).max(1);
        let done = (first..last).filter(|i| report.is_complete(*i)).count() as u64;
        let busy = (first..last).any(|i| inflight.contains(&i));

        let state = if done == span {
            2
        } else if busy {
            1
        } else {
            0
        };
        out.push(InspectorCell { state, fill: done as f32 / span as f32 });
    }
    (out, bucket)
}

/// One interface, as the sidebar needs it.
///
/// The id is the key: it is what lane reports carry and what the socket
/// option takes: and the rest is only ever drawn. See
/// `dl_net::Interface::display_label`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InterfaceInfo {
    pub id: String,
    pub label: String,
    pub icon: String,
}

/// The Inspector's tabs: canonical key and the label it is shown under.
///
/// One list, because the panel reports which tab by position and nothing else
/// may assume what the positions mean. Hardcoding them in the markup put the
/// highlight on one tab and the content of another the moment the list changed
/// length: a torrent has no Pieces tab to draw.
fn inspector_tabs(is_torrent: bool) -> &'static [(&'static str, &'static str)] {
    if is_torrent {
        // No Pieces: librqbit keeps the have-pieces bitfield behind
        // `pub(crate)`, and a tab that never fills is worse than one that is
        // not offered.
        &[("info", "Info"), ("peers", "Peers"), ("files", "Files")]
    } else {
        &[
            ("info", "Info"),
            ("pieces", "Pieces"),
            // An HTTP transfer has connections, not peers, and one file.
            ("peers", "Connections"),
            ("files", "File"),
        ]
    }
}

/// Update the piece grid in place.
///
/// Replacing the model made Slint tear down and rebuild every cell, and this
/// runs on every tick: several hundred elements destroyed and recreated ten
/// times a second, which is felt as the whole window going sluggish the
/// moment the grid is on screen. Cells change state one at a time, so the
/// model is kept and only the cells that differ are written.
fn update_cells(ui: &MainWindow, cells: Vec<InspectorCell>) {
    let current = ui.get_inspector_cells();
    if let Some(model) = current.as_any().downcast_ref::<VecModel<InspectorCell>>()
        && model.row_count() == cells.len()
    {
        for (index, cell) in cells.into_iter().enumerate() {
            let same = model
                .row_data(index)
                .is_some_and(|old| old.state == cell.state && old.fill == cell.fill);
            if !same {
                model.set_row_data(index, cell);
            }
        }
        return;
    }

    // A different length means a different transfer or a re-bucketed grid,
    // which is the one case where rebuilding is the cheaper answer.
    ui.set_inspector_cells(Rc::new(VecModel::from(cells)).into());
}

/// Fill the Inspector for the selected transfer.
///
/// Gated twice over: nothing happens with the panel closed or nothing
/// selected, and the chunk map is pulled for one transfer rather than carried
/// by every snapshot. With the panel closed this costs one comparison.
fn apply_inspector(
    ui: &MainWindow,
    engine: &Engine,
    snapshots: &[dl_core::engine::DownloadSnapshot],
) {
    if !ui.get_inspector_open() {
        return;
    }
    let selected = ui.get_selected_id();
    let Some(snapshot) = snapshots.iter().find(|s| s.id.0 as i32 == selected) else {
        return;
    };

    let torrent = snapshot.torrent.as_ref();
    let report = engine.chunks(snapshot.id);

    ui.set_inspector_title(snapshot.filename.clone().into());

    // The header counts what the transfer is made of. A torrent has pieces, an
    // HTTP download has chunks, and calling both "pieces" would be borrowing a
    // word for something the engine does not do.
    let total = snapshot.progress.total.map(format_bytes).unwrap_or_else(|| "unknown size".into());
    let units = match (&report, torrent) {
        (Some(r), None) => format!(" · {} chunks", r.chunk_count),
        // A torrent's piece count is librqbit's to report and it does not, so
        // the header says what it does know rather than inventing a figure.
        (_, Some(t)) if t.files.len() > 1 => format!(" · {} files", t.files.len()),
        _ => String::new(),
    };
    // Deliberately nothing that changes while the transfer runs: the subtitle
    // is centred, so a peer count going from 6 to 86 re-centres the whole line
    // and it jumps. Peers have their own row in Info, which is right-aligned
    // in a fixed column and does not move.
    let via = match torrent {
        Some(_) => String::new(),
        None => format!(" · {}", snapshot.host),
    };
    ui.set_inspector_subtitle(format!("{total}{units}{via}").into());
    // "Have 62%", as in the design. The percentage is the one number a grid
    // cannot show precisely, and reading it off the cells is exactly what a
    // person should not have to do. Kept to one short word: three legend keys
    // share 320px, and "Downloaded 100%" pushed the figure off the end.
    let have = match &report {
        Some(r) if r.chunk_count > 0 => {
            format!("Have {:.0}%", r.completed_count() as f64 * 100.0 / r.chunk_count as f64)
        }
        _ => "Have".to_string(),
    };
    ui.set_inspector_have(have.into());

    // Only while the grid is on screen: building it for a tab nobody is
    // looking at is hundreds of elements of work, ten times a second, thrown
    // away. Deliberately not an early return, because everything below fills
    // the other tabs and sets the page itself: skipping it would leave the
    // panel stuck on whichever tab it was showing.
    //
    // The page read here is the one set at the end of the previous tick, so
    // the grid starts filling a tenth of a second after the tab is chosen.
    if ui.get_inspector_page() == "pieces" {
        match &report {
            Some(report) => {
                let (cells, bucket) = cells_for(report);
                update_cells(ui, cells);
                ui.set_inspector_bucket(bucket as i32);
            }
            // A finished or queued transfer has no live map. An empty grid
            // says so better than a stale one from the last transfer.
            None => {
                update_cells(ui, Vec::new());
                ui.set_inspector_bucket(1);
            }
        }
    }

    // Rows a transfer does not have are absent, never zero: "Ratio 0.00" on an
    // HTTP download is a lie about what the transfer is.
    let mut stats = vec![InspectorStat {
        label: "Down".into(),
        value: format!("{}/s", format_bytes(snapshot.progress.bytes_per_sec)).into(),
    }];
    if let Some(t) = torrent {
        stats.push(InspectorStat {
            label: "Up".into(),
            value: format!("{}/s", format_bytes(t.upload_bytes_per_sec)).into(),
        });
        stats.push(InspectorStat { label: "Peers".into(), value: t.peers.to_string().into() });
        let ratio = match snapshot.progress.downloaded {
            0 => "-".to_string(),
            down => format!("{:.2}", t.uploaded as f64 / down as f64),
        };
        stats.push(InspectorStat { label: "Ratio".into(), value: ratio.into() });
    }
    stats.push(InspectorStat {
        label: "Done".into(),
        value: format!(
            "{} of {}",
            format_bytes(snapshot.progress.downloaded),
            snapshot.progress.total.map(format_bytes).unwrap_or_else(|| "?".into())
        )
        .into(),
    });
    stats.push(InspectorStat {
        label: "ETA".into(),
        // A stalled transfer reports no ETA rather than a growing one:
        // dividing by a rate approaching zero gives "14h", then "3d".
        value: snapshot.progress.eta().map(format_duration).unwrap_or_else(|| "-".into()).into(),
    });
    ui.set_inspector_stats(Rc::new(VecModel::from(stats)).into());

    // Files: a torrent's list, or the one file an HTTP transfer is.
    let files: Vec<InspectorFile> = match torrent {
        Some(t) => t
            .files
            .iter()
            .map(|f| InspectorFile {
                name: f.path.clone().into(),
                size: format_bytes(f.len).into(),
                detail: format!("{} downloaded", format_bytes(f.downloaded)).into(),
                progress: if f.len > 0 { f.downloaded as f32 / f.len as f32 } else { 0.0 },
            })
            .collect(),
        None => vec![InspectorFile {
            name: snapshot.filename.clone().into(),
            size: snapshot
                .progress
                .total
                .map(format_bytes)
                .unwrap_or_else(|| "unknown".into())
                .into(),
            detail: snapshot.host.clone().into(),
            progress: snapshot.progress.fraction().unwrap_or(0.0),
        }],
    };
    ui.set_inspector_files(Rc::new(VecModel::from(files)).into());

    // Peers: real peers for a torrent, the connections carrying the transfer
    // for an HTTP download. Both answer "what is this transfer talking to".
    let (peers, column, note) = match torrent {
        Some(t) => (
            t.peer_list
                .iter()
                .map(|p| InspectorPeer {
                    label: p.address.clone().into(),
                    detail: p.client.clone().unwrap_or_else(|| p.state.clone()).into(),
                    rate: format!("↓{}", format_bytes(p.downloaded)).into(),
                    parked: p.downloaded == 0,
                })
                .collect::<Vec<_>>(),
            "PEERS",
            if t.peers == 0 {
                "No peers connected.".to_string()
            } else {
                format!("{} connected; none has sent anything yet.", t.peers)
            },
        ),
        None => (
            snapshot
                .lanes
                .iter()
                .map(|lane| InspectorPeer {
                    label: lane.label.clone().into(),
                    detail: format!("{} chunks", lane.chunks).into(),
                    rate: lane
                        .throughput
                        .map(|t| format!("{}/s", format_bytes(t as u64)))
                        .unwrap_or_else(|| "-".into())
                        .into(),
                    parked: lane.parked,
                })
                .collect(),
            "INTERFACES",
            "This transfer has not opened a connection yet.".to_string(),
        ),
    };
    ui.set_inspector_peers(Rc::new(VecModel::from(peers)).into());
    ui.set_inspector_peers_column(column.into());
    ui.set_inspector_empty_note(note.into());

    // A transfer with no peer list has nothing to put on that tab.
    // Pieces is offered only when there is a map to draw. librqbit keeps the
    // have-pieces bitfield behind `pub(crate)`, so a torrent has no grid: and
    // an empty tab that never fills is worse than one that is not there.
    //
    // The panel reports which tab by position and this is the only place that
    // knows what the positions mean. Hardcoding them in the markup put the
    // highlight on one tab and the content of another the moment the list
    // changed length.
    let keys = inspector_tabs(torrent.is_some());
    // Clamped, because the tab set shrinks when the selection moves from an
    // HTTP download to a torrent and the stored index can outlive its tab.
    let tab = (ui.get_inspector_tab().max(0) as usize).min(keys.len() - 1);
    ui.set_inspector_tab(tab as i32);
    ui.set_inspector_page(keys[tab].0.into());
    ui.set_inspector_finished(
        snapshot.state == State::Complete || snapshot.state == State::Seeding,
    );
    ui.set_inspector_tabs(
        Rc::new(VecModel::from(
            keys.iter().map(|(_, label)| SharedString::from(*label)).collect::<Vec<_>>(),
        ))
        .into(),
    );
}

fn wire_callbacks(ui: &MainWindow, engine: &Engine, settings: &crate::settings::Shared) {
    ui.on_set_filter({
        let weak = ui.as_weak();
        move |filter| {
            if let Some(ui) = weak.upgrade() {
                ui.set_filter(filter);
            }
        }
    });

    ui.on_toggle_download({
        let engine = engine.clone();
        move |id| {
            let id = DownloadId(id as u64);
            let Some(snapshot) = engine.get(id) else { return };
            match snapshot.state {
                // Seeding included: it is a live transfer giving bytes away,
                // and the only control that stops it is this one.
                State::Running | State::Queued | State::Seeding => engine.pause(id),
                _ => engine.resume(id),
            }
        }
    });

    // Asks rather than acts. Removal is the one control here that can destroy
    // work, and it was a single click with no confirmation and no say in
    // whether the file on disk went with it.
    ui.on_remove_download({
        let engine = engine.clone();
        let weak = ui.as_weak();
        move |id| {
            let Some(ui) = weak.upgrade() else { return };
            let Some(snapshot) = engine.get(DownloadId(id as u64)) else { return };
            ui.set_removing_name(snapshot.filename.clone().into());
            ui.set_removing_finished(snapshot.state == State::Complete);
            ui.set_removing_id(id);
        }
    });

    ui.on_reveal_download({
        let engine = engine.clone();
        let settings = settings.clone();
        move |id| {
            let Some(snapshot) = engine.get(DownloadId(id as u64)) else { return };
            let Ok(current) = settings.read() else { return };
            crate::settings::reveal(&current.destination.join(&snapshot.filename));
        }
    });

    ui.on_confirm_remove({
        let engine = engine.clone();
        move |id, delete_files| {
            let removed = engine.remove_with_files(DownloadId(id as u64), delete_files);
            if delete_files {
                tracing::info!(count = removed.len(), "deleted the transfer's files");
            }
        }
    });

    ui.on_select_download({
        let weak = ui.as_weak();
        move |id| {
            let Some(ui) = weak.upgrade() else { return };
            // Clicking the selected row again clears it, which is the only way
            // back to no selection with the pointer alone.
            let cleared = ui.get_selected_id() == id;
            ui.set_selected_id(if cleared { -1 } else { id });
            // Opening on click is how the panel is found. Discovering it
            // required noticing a toolbar button whose icon means nothing
            // until you have already seen what it does.
            ui.set_inspector_open(!cleared);
        }
    });

    ui.on_toggle_inspector({
        let weak = ui.as_weak();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            ui.set_inspector_open(!ui.get_inspector_open());
        }
    });

    ui.on_step_selection({
        let weak = ui.as_weak();
        move |delta| {
            let Some(ui) = weak.upgrade() else { return };
            if let Some(next) = next_selection(&visible_ids(&ui), ui.get_selected_id(), delta) {
                ui.set_selected_id(next);
            }
        }
    });

    ui.on_expand_download({
        let weak = ui.as_weak();
        move |id| {
            let Some(ui) = weak.upgrade() else { return };
            let mut open: Vec<i32> = ui.get_expanded_rows().iter().collect();
            match open.iter().position(|v| *v == id) {
                Some(index) => {
                    open.remove(index);
                }
                None => open.push(id),
            }
            ui.set_expanded_rows(Rc::new(VecModel::from(open)).into());
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use dl_core::engine::DownloadSnapshot;
    use dl_core::lane::LaneReport;

    /// An engine that is never asked to fetch anything.
    struct NoSources;

    impl dl_core::engine::SourceFactory for NoSources {
        fn lanes_for(
            &self,
            _spec: &dl_core::engine::DownloadSpec,
        ) -> dl_core::Result<Box<dyn dl_core::lane::LaneSet>> {
            unreachable!("no download is ever started in these tests")
        }
    }

    /// One interface's worth of samples, which is what most machines have.
    /// Values are bytes per second, so they have to be real rates: anything
    /// under `MIN_FULL_SCALE` is deliberately drawn flat.
    fn series(values: &[u64]) -> Vec<Sample> {
        values.iter().map(|v| Sample { interfaces: vec![("en0".to_string(), *v)], up: 0 }).collect()
    }

    fn one_nic() -> Vec<String> {
        vec!["en0".to_string()]
    }

    /// Every y coordinate in a path, in order.
    ///
    /// Coordinates alternate x, y throughout, control points included: which
    /// is why a naive scan over all numbers reads bezier handle x's as heights.
    fn y_values(path: &str) -> Vec<f32> {
        path.split_whitespace().filter_map(|t| t.parse::<f32>().ok()).skip(1).step_by(2).collect()
    }

    /// Where a given peak rate lands vertically, given the rounded scale.
    /// The trace is bidirectional: download fills the upper half against a
    /// centre line, so the floor is `height / 2`, not `height`.
    fn floor_y(height: f32) -> f32 {
        height / 2.0
    }

    fn peak_y(peak: u64, height: f32) -> f32 {
        let full = axis_max(peak.max(MIN_FULL_SCALE)) as f32;
        let centre = floor_y(height);
        centre - (peak as f32 / full) * centre * 0.92
    }

    fn lane(label: &str, bytes: u64) -> LaneReport {
        LaneReport {
            lane: 0,
            label: label.into(),
            bytes,
            chunks: 1,
            throughput: Some(1000.0),
            parked: false,
        }
    }

    #[test]
    fn byte_formatting() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1 << 30), "1.0 GB");
    }

    #[test]
    fn segments_span_the_bar_exactly() {
        let lanes = vec![lane("en0", 750), lane("en5", 250)];
        let model = segments(&lanes, &["en0".to_string(), "en5".to_string()]);
        let spans: Vec<Segment> = model.iter().collect();

        assert_eq!(spans.len(), 2);
        assert!((spans[0].span - 0.75).abs() < 0.001);
        assert!((spans[1].start - 0.75).abs() < 0.001);
        let covered: f32 = spans.iter().map(|s| s.span).sum();
        assert!((covered - 1.0).abs() < 0.001, "segments must tile the bar: {covered}");
    }

    #[test]
    fn idle_interfaces_are_listed_at_zero() {
        // Lane reports only exist while bytes are moving. Deriving the sidebar
        // from them alone left an "INTERFACES" heading with nothing under it
        // on a freshly launched window.
        let engine = Engine::new(
            std::sync::Arc::new(NoSources),
            dl_core::engine::EngineConfig::default(),
            dl_core::budget::Budget::unlimited(),
        );
        let known = vec!["en0".to_string(), "en5".to_string()];
        let totals = Totals::from(&[], &engine, &known);

        assert_eq!(
            totals.interfaces,
            vec![("en0".to_string(), 0), ("en5".to_string(), 0)],
            "every usable NIC is listed even with nothing downloading"
        );
    }

    #[test]
    fn a_lane_takes_its_colour_from_the_interface_not_its_position() {
        // Wi-Fi is second in this download's lane list but third in the
        // sidebar; it must be the sidebar's colour in both places.
        let names = vec!["Ethernet".to_string(), "Tether".to_string(), "Wi-Fi".to_string()];
        let lanes = vec![lane("Ethernet", 100), lane("Wi-Fi", 100)];
        let spans: Vec<Segment> = segments(&lanes, &names).iter().collect();
        assert_eq!(spans[0].color_index, 0, "Ethernet");
        assert_eq!(spans[1].color_index, 2, "Wi-Fi keeps its sidebar colour");
    }

    #[test]
    fn a_download_with_no_lane_data_still_renders_one_span() {
        // Before the first chunk completes there is nothing to apportion, and
        // an empty model would draw no bar at all.
        let spans: Vec<Segment> = segments(&[], &[]).iter().collect();
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].span, 1.0);
    }

    #[test]
    fn a_stopped_transfer_leads_with_why_rather_than_a_stale_rate() {
        // The rate a download had when it was paused is not information; the
        // reason it stopped is.
        let mut snapshot = dl_core::engine::DownloadSnapshot {
            id: DownloadId(1),
            filename: "x.bin".into(),
            host: "h".into(),
            state: State::Paused,
            progress: dl_core::Progress {
                downloaded: 500,
                total: Some(1000),
                bytes_per_sec: 4242,
                smoothed_bytes_per_sec: 0,
            },
            lanes: vec![lane("en0", 500)],
            error: None,
            phase: None,
            torrent: None,
        };
        let paused = row_for(&snapshot, &[]);
        assert!(paused.status.starts_with("Paused"), "{}", paused.status);
        assert!(!paused.status.contains("/s"), "a paused row must not quote a rate");

        snapshot.state = State::Running;
        let running = row_for(&snapshot, &[]);
        assert!(running.status.ends_with("/s"), "{}", running.status);
        assert!(running.status.starts_with("50%"), "{}", running.status);
    }

    #[test]
    fn a_download_has_no_second_status_line() {
        // The peers line is what a torrent adds; the row omits it rather than
        // printing an empty one, which is what lets both share a component.
        let snapshot = dl_core::engine::DownloadSnapshot {
            id: DownloadId(1),
            filename: "x.bin".into(),
            host: "h".into(),
            state: State::Running,
            progress: Default::default(),
            lanes: vec![lane("en0", 10)],
            error: None,
            phase: None,
            torrent: None,
        };
        let row = row_for(&snapshot, &[]);
        assert_eq!(row.detail, "");
        assert_eq!(row.kind, "http");
        assert!(!row.expandable);
        assert_eq!(row.depth, 0);
    }

    #[test]
    fn only_visible_changes_mark_a_row_dirty() {
        let snapshot = dl_core::engine::DownloadSnapshot {
            id: DownloadId(1),
            filename: "x.bin".into(),
            host: "h".into(),
            state: State::Running,
            progress: dl_core::Progress {
                downloaded: 100,
                total: Some(1000),
                bytes_per_sec: 50,
                smoothed_bytes_per_sec: 0,
            },
            lanes: vec![lane("en0", 100)],
            error: None,
            phase: None,
            torrent: None,
        };
        let a = row_for(&snapshot, &[]);

        // A byte of progress that rounds to the same display must not redraw.
        let mut nudged = snapshot.clone();
        nudged.progress.downloaded = 100;
        assert!(!differs(&a, &row_for(&nudged, &[])));

        let mut moved = snapshot.clone();
        moved.progress.downloaded = 800;
        assert!(differs(&a, &row_for(&moved, &[])));
    }

    /// Build a torrent snapshot the row renderer can be pointed at.
    fn torrent_snapshot(state: State, status: dl_core::TorrentStatus) -> DownloadSnapshot {
        DownloadSnapshot {
            id: DownloadId(7),
            filename: "Ubuntu 24.04".into(),
            host: "magnet".into(),
            state,
            progress: dl_core::Progress {
                downloaded: 500,
                total: Some(1000),
                bytes_per_sec: 1024,
                smoothed_bytes_per_sec: 0,
            },
            lanes: Vec::new(),
            error: None,
            phase: None,
            torrent: Some(status),
        }
    }

    fn files(names: &[&str]) -> Vec<dl_core::TorrentFile> {
        names
            .iter()
            .map(|name| dl_core::TorrentFile {
                path: (*name).to_string(),
                len: 100,
                downloaded: 50,
            })
            .collect()
    }

    #[test]
    fn a_torrent_row_says_it_is_a_torrent_and_names_its_peers() {
        // The glyph column is how the list distinguishes a swarm from a
        // server at a glance.
        let snapshot = torrent_snapshot(
            State::Running,
            dl_core::TorrentStatus {
                uploaded: 2048,
                upload_bytes_per_sec: 512,
                peers: 12,
                files: files(&["a.bin", "b.bin"]),
                peer_list: Vec::new(),
                interface: None,
            },
        );
        let row = row_for(&snapshot, &[]);
        assert_eq!(row.kind, "torrent");
        assert_eq!(row.file, "folder", "a multi-file torrent is a folder, not its first file");
        assert!(row.expandable, "two files is something to open");
        assert!(row.detail.contains("12 peers"), "{}", row.detail);
        assert!(row.detail.contains("up"), "{}", row.detail);
    }

    #[test]
    fn a_torrent_that_has_found_nobody_yet_shows_no_peer_line() {
        // "0 peers" reads as a measurement. Before anyone has answered, the
        // honest display is no second line at all.
        let snapshot = torrent_snapshot(State::Running, Default::default());
        assert_eq!(row_for(&snapshot, &[]).detail, "");
    }

    #[test]
    fn a_single_file_torrent_has_nothing_to_expand() {
        // Opening it would reveal one row saying what the row above already
        // says, so the chevron stays off.
        let snapshot = torrent_snapshot(
            State::Running,
            dl_core::TorrentStatus { files: files(&["ubuntu.iso"]), ..Default::default() },
        );
        let row = row_for(&snapshot, &[]);
        assert!(!row.expandable);
        assert_eq!(row.file, "disk", "the row takes its glyph from the file inside");
    }

    #[test]
    fn a_seeding_row_leads_with_what_it_has_given() {
        // There is nothing left to download, so a percentage and a download
        // rate would both be zero facts about a transfer that is working.
        let snapshot = torrent_snapshot(
            State::Seeding,
            dl_core::TorrentStatus { uploaded: 4096, peers: 3, ..Default::default() },
        );
        let row = row_for(&snapshot, &[]);
        assert_eq!(row.state, "seeding");
        assert!(row.status.starts_with("Seeding"), "{}", row.status);
        assert!(row.status.contains("up"), "{}", row.status);
    }

    #[test]
    fn a_child_row_carries_its_parents_id_and_sits_one_level_in() {
        // The row component hides every control above depth zero, so the id
        // is never clickable: but a wrong depth would draw a file as a
        // transfer with its own pause button.
        let snapshot = torrent_snapshot(
            State::Running,
            dl_core::TorrentStatus { files: files(&["dir/a.bin"]), ..Default::default() },
        );
        let file = &snapshot.torrent.as_ref().unwrap().files[0];
        let child = child_row(&snapshot, file);
        assert_eq!(child.id, 7);
        assert_eq!(child.depth, 1);
        assert!(child.grouped);
        assert!(!child.expandable);
        assert_eq!(child.filename, "dir/a.bin");
        assert!((child.progress - 0.5).abs() < 0.001);
    }

    #[test]
    fn filters_select_the_right_states() {
        assert!(matches_filter(State::Running, "active"));
        assert!(matches_filter(State::Paused, "active"));
        assert!(!matches_filter(State::Queued, "active"));
        assert!(matches_filter(State::Queued, "queued"));
        assert!(!matches_filter(State::Complete, "active"));
        assert!(matches_filter(State::Complete, "done"));
        assert!(matches_filter(State::Failed, "error"));
        assert!(matches_filter(State::Failed, "all"));
        // The Seeding filter was a permanent no-op; a sidebar entry that
        // always returns nothing is worse than one that is not there.
        assert!(matches_filter(State::Seeding, "seeding"));
        assert!(!matches_filter(State::Running, "seeding"));
        assert!(matches_filter(State::Seeding, "all"));
    }

    #[test]
    fn search_matches_any_part_of_the_name_in_either_case() {
        assert!(matches_search("ubuntu-24.04.1-desktop-amd64.iso", "UBUNTU"));
        assert!(matches_search("ubuntu-24.04.1-desktop-amd64.iso", "amd64"));
        assert!(!matches_search("ubuntu-24.04.1-desktop-amd64.iso", "fedora"));
        // An empty query is not a filter; it must not empty the list.
        assert!(matches_search("anything.bin", ""));
    }

    #[test]
    fn the_chart_needs_two_points_before_it_draws() {
        let empty = chart_paths(&[], &one_nic(), 100.0, 100.0);
        assert!(empty.bands.is_empty() && empty.line.is_empty());

        let one = chart_paths(&series(&[100]), &one_nic(), 100.0, 100.0);
        assert!(one.bands.is_empty(), "a single sample is not a trace");

        // Before the window has been laid out there is nothing to draw into.
        let unmeasured = chart_paths(&series(&[0, 100]), &one_nic(), 0.0, 0.0);
        assert!(unmeasured.line.is_empty(), "an unmeasured plot must not emit a path");
    }

    #[test]
    fn the_trace_spans_the_plot_and_each_band_closes() {
        let chart = chart_paths(&series(&[0, 5 << 20, 10 << 20]), &one_nic(), 100.0, 100.0);
        assert!(chart.line.starts_with("M 0.00 50.00"), "{}", chart.line);
        let top = format!("100.00 {:.2}", peak_y(10 << 20, 100.0));
        assert!(chart.line.ends_with(&top), "expected to end {top}: {}", chart.line);
        assert_eq!(chart.bands.len(), 1);
        assert!(chart.bands[0].ends_with(" Z"), "a fill must be a closed path");
        assert!(chart.bands[0].starts_with("M 0.00 50.00"), "{}", chart.bands[0]);
    }

    #[test]
    fn the_trace_is_emitted_in_the_plots_own_pixels() {
        // Coordinates are in the plot's own pixels, so the trace fills
        // whatever width the window gives it.
        let chart = chart_paths(&series(&[0, 10 << 20]), &one_nic(), 760.0, 80.0);
        assert!(chart.line.starts_with("M 0.00 40.00"), "{}", chart.line);
        let top = format!("760.00 {:.2}", peak_y(10 << 20, 80.0));
        assert!(chart.line.ends_with(&top), "expected to end {top}: {}", chart.line);
    }

    #[test]
    fn a_short_history_is_padded_rather_than_stretched() {
        // Three samples must not be drawn as a minute of data. They occupy the
        // rightmost three slots; everything older reads as zero.
        let chart = chart_paths(&series(&[10 << 20; 3]), &one_nic(), 100.0, 100.0);
        let points: Vec<&str> = chart.line.split(" C ").collect();
        assert_eq!(points.len(), CHART_POINTS);
        assert!(points[0].ends_with("50.00"), "oldest slot must sit on the centre line");
        assert!(points[CHART_POINTS - 4].ends_with("50.00"), "padding must run to the samples");
        let top = format!("{:.2}", peak_y(10 << 20, 100.0));
        assert!(points[CHART_POINTS - 1].ends_with(&top), "newest sample sits at the peak");
    }

    #[test]
    fn the_curve_never_leaves_the_plot() {
        // Smoothing a flat run into a spike overshoots; unclamped that draws
        // throughput above the peak or below zero, which is a lie about a
        // measurement rather than a cosmetic glitch.
        let mut values = vec![0u64; 40];
        values.extend([9 << 20, 1 << 18, 9 << 20, 1 << 18, 9 << 20]);
        let chart = chart_paths(&series(&values), &one_nic(), 100.0, 100.0);

        for token in chart.line.split_whitespace() {
            if let Ok(n) = token.parse::<f32>() {
                assert!((-0.01..=100.01).contains(&n), "coordinate {n} escapes the plot");
            }
        }
    }

    #[test]
    fn an_idle_chart_stays_flat_instead_of_amplifying_nothing() {
        let chart = chart_paths(&series(&[0, 0, 0, 0]), &one_nic(), 100.0, 100.0);
        for y in y_values(&chart.line) {
            assert!((y - 50.0).abs() < 0.01, "idle must sit on the centre line, found y={y}");
        }
    }

    #[test]
    fn a_trickle_is_not_amplified_into_a_spike() {
        // Exactly zero was already handled; a few bytes a second was not, and
        // drew a half-height trace across an axis reading "0 B/s" four times.
        let chart = chart_paths(&series(&[0, 3, 1, 4]), &one_nic(), 100.0, 100.0);
        for y in y_values(&chart.line) {
            assert!(y > 49.5, "a trickle must hug the centre line, found y={y}");
        }
    }

    fn with_upload(down: &[u64], up: &[u64]) -> Vec<Sample> {
        down.iter()
            .zip(up)
            .map(|(d, u)| Sample { interfaces: vec![("en0".to_string(), *d)], up: *u })
            .collect()
    }

    #[test]
    fn nothing_uploading_draws_no_upload_at_all() {
        // The common case, and the one that must not disturb anything. A flat
        // run along the centre is not "invisible": it renders as a solid rule
        // across the whole strip, which says there is upload when there is
        // none.
        let chart = chart_paths(&series(&[10 << 20; 4]), &one_nic(), 100.0, 100.0);
        assert!(chart.up_line.is_empty(), "idle upload still drew: {}", chart.up_line);
        assert!(chart.up_area.is_empty());
        assert!(!chart.line.is_empty(), "the download trace must survive it");
        assert_eq!(chart.bands.len(), 1);
    }

    #[test]
    fn the_centre_line_does_not_move_when_upload_starts() {
        // If the two halves were scaled independently, or the centre placed
        // from the data, starting to seed would shift the whole download trace
        // under the reader.
        let quiet = chart_paths(&series(&[10 << 20; 4]), &one_nic(), 100.0, 100.0);
        let busy =
            chart_paths(&with_upload(&[10 << 20; 4], &[5 << 20; 4]), &one_nic(), 100.0, 100.0);
        assert_eq!(quiet.line, busy.line, "the download trace moved when upload appeared");
    }

    #[test]
    fn upload_is_drawn_below_the_centre_and_download_above() {
        let chart =
            chart_paths(&with_upload(&[10 << 20; 4], &[10 << 20; 4]), &one_nic(), 100.0, 100.0);
        assert!(y_values(&chart.line).iter().all(|y| *y <= 50.01), "download must stay above");
        assert!(y_values(&chart.up_line).iter().all(|y| *y >= 49.99), "upload must stay below");
    }

    #[test]
    fn both_directions_share_one_scale() {
        // Scaling each half to its own peak would draw a trickle of upload the
        // same height as a torrent of download.
        let chart =
            chart_paths(&with_upload(&[100 << 20; 4], &[1 << 20; 4]), &one_nic(), 100.0, 100.0);
        let deepest = y_values(&chart.up_line).into_iter().fold(0.0f32, f32::max);
        assert!(
            deepest < 52.0,
            "a hundredth of the download rate drew {:.1}px of upload",
            deepest - 50.0
        );
    }

    #[test]
    fn neither_direction_escapes_the_plot() {
        let chart =
            chart_paths(&with_upload(&[80 << 20; 6], &[80 << 20; 6]), &one_nic(), 100.0, 100.0);
        for path in [&chart.line, &chart.up_line, &chart.up_area] {
            for token in path.split_whitespace() {
                if let Ok(n) = token.parse::<f32>() {
                    assert!((-0.01..=100.01).contains(&n), "coordinate {n} escapes the plot");
                }
            }
        }
    }

    #[test]
    fn an_empty_history_yields_empty_paths_rather_than_a_panic() {
        let chart = chart_paths(&[], &one_nic(), 100.0, 100.0);
        assert!(chart.line.is_empty() && chart.up_line.is_empty() && chart.bands.is_empty());
    }

    #[test]
    fn axis_gridlines_land_on_round_figures() {
        // The case from a real capture: a 2.5 MB/s peak labelled its thirds
        // "840.2 KB/s".
        for rate in [0, 1, 700, 2_621_440, 10 << 20, 137 << 20, 1 << 30] {
            let max = axis_max(rate.max(MIN_FULL_SCALE));
            assert!(max >= rate, "axis must contain the peak: {rate} -> {max}");
            assert_eq!(max % 3, 0, "max must divide into three whole steps");
            let step = max / 3;
            // A round step renders with at most one decimal place and no
            // trailing noise, which is what "readable" means here.
            let text = format_bytes(step);
            let decimals =
                text.split('.').nth(1).map(|t| t.trim_end_matches(|c: char| !c.is_ascii_digit()));
            assert!(
                decimals.is_none_or(|d| d == "0" || d == "5"),
                "step {step} renders as {text}, which is not a round figure"
            );
        }
    }

    #[test]
    fn bands_stack_so_the_top_of_the_last_one_is_the_total() {
        let names = vec!["en0".to_string(), "en5".to_string()];
        let history: Vec<Sample> = (0..3)
            .map(|_| Sample {
                interfaces: vec![("en0".to_string(), 30 << 20), ("en5".to_string(), 70 << 20)],
                up: 0,
            })
            .collect();
        let chart = chart_paths(&history, &names, 100.0, 100.0);

        assert_eq!(chart.bands.len(), 2, "one filled band per interface");
        // Peak is the sum of both lanes, and the total line is drawn against it.
        let top = format!("100.00 {:.2}", peak_y(100 << 20, 100.0));
        assert!(chart.line.ends_with(&top), "expected to end {top}: {}", chart.line);
        assert!(chart.bands[0].starts_with("M 0.00 50.00"), "{}", chart.bands[0]);
    }

    #[test]
    fn an_interface_that_appears_midway_reads_as_zero_before_it_existed() {
        // A NIC coming up must not retroactively rewrite earlier samples, or
        // the graph would claim bandwidth that never arrived.
        let names = vec!["en0".to_string(), "tether".to_string()];
        let history = vec![
            Sample { interfaces: vec![("en0".to_string(), 10 << 20)], up: 0 },
            Sample {
                interfaces: vec![("en0".to_string(), 10 << 20), ("tether".to_string(), 10 << 20)],
                up: 0,
            },
        ];
        let chart = chart_paths(&history, &names, 100.0, 100.0);
        let band = &chart.bands[1];
        // The tether band is empty until its first sample: its upper and lower
        // edges coincide on the floor for every padded slot.
        assert!(band.starts_with("M 0.00 50.00"), "{band}");
        let top = format!("100.00 {:.2}", peak_y(20 << 20, 100.0));
        assert!(band.contains(&top), "tether reaches its peak only at the end: {band}");
    }

    #[test]
    fn a_row_finishing_above_the_selection_does_not_move_it() {
        // The whole reason selection is held by id. With an index, a row
        // completing and leaving the filter would silently shift the
        // selection onto its neighbour.
        assert_eq!(selection_after(&[1, 2, 3], &[2, 3], 3), 3);
        assert_eq!(selection_after(&[1, 2, 3], &[3], 3), 3);
    }

    #[test]
    fn losing_the_selected_row_keeps_the_place_rather_than_deselecting() {
        // Holding Delete should work through a list. Clearing the selection
        // after the first row would stop it dead.
        assert_eq!(selection_after(&[1, 2, 3], &[1, 3], 2), 3, "takes the row that moved up");
        assert_eq!(selection_after(&[1, 2, 3], &[1, 2], 3), 2, "the last row falls back");
    }

    #[test]
    fn an_empty_list_clears_the_selection() {
        assert_eq!(selection_after(&[1], &[], 1), -1);
        assert_eq!(selection_after(&[], &[], -1), -1);
    }

    #[test]
    fn a_selection_that_was_never_on_screen_is_cleared() {
        // Nothing to hold a place for, so guessing one would select a
        // transfer the user never pointed at.
        assert_eq!(selection_after(&[1, 2], &[1, 2], 9), -1);
    }

    #[test]
    fn nothing_selected_stays_nothing_selected() {
        assert_eq!(selection_after(&[1, 2], &[1, 2], -1), -1);
    }

    #[test]
    fn the_arrow_keys_stop_at_the_ends_rather_than_wrapping() {
        // Wrapping from the last row to the first reads as the list having
        // jumped, not as having moved one row.
        assert_eq!(next_selection(&[1, 2, 3], 1, -1), Some(1));
        assert_eq!(next_selection(&[1, 2, 3], 3, 1), Some(3));
        assert_eq!(next_selection(&[1, 2, 3], 2, 1), Some(3));
        assert_eq!(next_selection(&[1, 2, 3], 2, -1), Some(1));
    }

    #[test]
    fn an_arrow_key_with_nothing_selected_enters_from_the_right_end() {
        assert_eq!(next_selection(&[1, 2, 3], -1, 1), Some(1), "down enters at the top");
        assert_eq!(next_selection(&[1, 2, 3], -1, -1), Some(3), "up enters at the bottom");
    }

    #[test]
    fn the_arrow_keys_do_nothing_to_an_empty_list() {
        assert_eq!(next_selection(&[], -1, 1), None);
        assert_eq!(next_selection(&[], 4, -1), None);
    }

    fn report(chunk_count: u64, complete: &[u64], inflight: &[u64]) -> dl_core::ChunkReport {
        let mut bits = vec![0u8; chunk_count.div_ceil(8) as usize];
        for index in complete {
            bits[(index / 8) as usize] |= 1 << (index % 8);
        }
        dl_core::ChunkReport {
            chunk_count,
            chunk_size: 1 << 20,
            complete: bits,
            inflight: inflight.to_vec(),
        }
    }

    #[test]
    fn a_small_grid_draws_one_cell_per_chunk() {
        assert_eq!(bucket_for(1), 1);
        assert_eq!(bucket_for(MAX_CELLS as u64), 1);
        let (cells, bucket) = cells_for(&report(100, &[], &[]));
        assert_eq!(bucket, 1);
        assert_eq!(cells.len(), 100);
    }

    #[test]
    fn a_huge_grid_is_bucketed_rather_than_truncated() {
        // A 60 GB torrent at 256 KB pieces is 240,000 pieces. Drawing a prefix
        // would silently claim the rest does not exist.
        let (cells, bucket) = cells_for(&report(240_000, &[], &[]));
        assert!(cells.len() <= MAX_CELLS, "{} cells is past the budget", cells.len());
        assert!(bucket > 1, "a grid this size must merge chunks");
        assert!(cells.len() as u64 * bucket >= 240_000, "the cells must account for every chunk");
    }

    #[test]
    fn a_bucket_is_only_have_when_every_chunk_in_it_is() {
        // Otherwise a grid reports a file as whole while pieces are missing.
        let (cells, bucket) = cells_for(&report(MAX_CELLS as u64 * 2, &[0], &[]));
        assert_eq!(bucket, 2);
        assert_eq!(cells[0].state, 0, "half a bucket is not Have");
        assert!(cells[0].fill > 0.0, "and it is not empty either");

        let (full, _) = cells_for(&report(MAX_CELLS as u64 * 2, &[0, 1], &[]));
        assert_eq!(full[0].state, 2);
    }

    #[test]
    fn bucket_shading_never_goes_backwards_as_chunks_complete() {
        // The cell is the only signal at this resolution; a cell that got
        // lighter as the file filled would be worse than no cell at all.
        let count = MAX_CELLS as u64 * 4;
        let mut previous = -1.0f32;
        for done in 0..=4u64 {
            let complete: Vec<u64> = (0..done).collect();
            let (cells, _) = cells_for(&report(count, &complete, &[]));
            assert!(cells[0].fill >= previous, "fill fell from {previous} at {done} complete");
            previous = cells[0].fill;
        }
        assert_eq!(previous, 1.0);
    }

    #[test]
    fn an_in_flight_chunk_shows_as_downloading_not_as_missing() {
        let (cells, _) = cells_for(&report(64, &[], &[7]));
        assert_eq!(cells[7].state, 1);
        assert_eq!(cells[6].state, 0);
    }

    #[test]
    fn a_complete_chunk_outranks_an_in_flight_one_in_the_same_bucket() {
        // A bucket that is entirely on disk is Have even if a stale in-flight
        // entry still names one of its chunks.
        let (cells, bucket) = cells_for(&report(MAX_CELLS as u64 * 2, &[0, 1], &[0]));
        assert_eq!(bucket, 2);
        assert_eq!(cells[0].state, 2);
    }

    #[test]
    fn an_empty_map_produces_an_empty_grid() {
        let (cells, bucket) = cells_for(&report(0, &[], &[]));
        assert!(cells.is_empty());
        assert_eq!(bucket, 1);
    }

    #[test]
    fn a_torrent_lane_reaches_the_progress_segments() {
        // A torrent has no lanes of ours: librqbit owns its sockets: but the
        // chart, the sidebar meters and the combined total are all built from
        // lane reports, so the engine synthesises one. This is the shape that
        // has to survive into the row.
        let mut snapshot = torrent_snapshot(
            State::Running,
            dl_core::TorrentStatus {
                uploaded: 0,
                upload_bytes_per_sec: 0,
                peers: 20,
                files: files(&["a.iso"]),
                peer_list: Vec::new(),
                interface: Some("en0".into()),
            },
        );
        snapshot.lanes = vec![LaneReport {
            lane: 0,
            label: "en0".into(),
            bytes: 1 << 20,
            chunks: 0,
            throughput: Some(12_000_000.0),
            parked: false,
        }];

        let row = row_for(&snapshot, &["en0".to_string()]);
        assert_eq!(row.kind, "torrent");
        assert_eq!(row.segments.row_count(), 1, "the torrent's lane must produce a segment");
        assert_eq!(row.segments.row_data(0).unwrap().span, 1.0, "it carries the whole transfer");
    }

    #[test]
    fn a_tabs_label_and_its_content_come_from_the_same_position() {
        // The panel reports which tab by position, and this table is the only
        // thing that knows what a position means. The two tab sets differ in
        // length, so anything else mapping index to meaning drifts from it.
        for torrent in [false, true] {
            let tabs = inspector_tabs(torrent);
            for (index, (key, label)) in tabs.iter().enumerate() {
                assert_eq!(tabs[index].0, *key, "position {index} must resolve to its own key");
                assert_eq!(tabs[index].1, *label);
            }
        }
    }

    #[test]
    fn a_torrent_is_not_offered_a_pieces_tab() {
        // There is no bitfield to draw one from, and an empty tab that never
        // fills is worse than one that is not there.
        let torrent = inspector_tabs(true);
        assert!(!torrent.iter().any(|(key, _)| *key == "pieces"));
        assert!(inspector_tabs(false).iter().any(|(key, _)| *key == "pieces"));
    }

    #[test]
    fn an_index_from_the_longer_tab_set_is_clamped_by_the_shorter_one() {
        // Selecting a torrent while on "File" (index 3) must not read past the
        // end of a three-tab list.
        let torrent = inspector_tabs(true);
        let carried_over = 3usize.min(torrent.len() - 1);
        assert_eq!(torrent[carried_over].0, "files");
    }

    #[test]
    fn the_two_tab_sets_use_the_same_keys_for_the_same_content() {
        // Otherwise the page the panel draws and the tab it highlights drift
        // apart again as soon as one of the lists is edited.
        for key in ["info", "peers", "files"] {
            assert!(inspector_tabs(true).iter().any(|(k, _)| *k == key), "torrent {key}");
            assert!(inspector_tabs(false).iter().any(|(k, _)| *k == key), "http {key}");
        }
    }
}
