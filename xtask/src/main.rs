//! Repo automation. Run as `cargo xtask <command>`.

mod app;
mod bundle;
mod mcp;
mod package;
mod size;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "xtask", about = "Repo automation for the Braid workspace")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Render the UI headlessly and write PNGs. No display server required.
    Screenshots {
        /// Directory to write PNGs into.
        #[arg(long, default_value = "artifacts/screens")]
        out: std::path::PathBuf,
        /// Port for the app's MCP endpoint.
        #[arg(long, default_value_t = 8730)]
        port: u16,
        /// Screenshot the release build instead of debug.
        #[arg(long)]
        release: bool,
    },
    /// Capture the visual-language prototype in both themes.
    Hero {
        #[arg(long, default_value = "artifacts/hero")]
        out: std::path::PathBuf,
        #[arg(long, default_value_t = 8731)]
        port: u16,
    },
    /// Capture every settings page.
    Settings {
        #[arg(long, default_value = "artifacts/screens")]
        out: std::path::PathBuf,
        #[arg(long, default_value_t = 8732)]
        port: u16,
    },
    /// Build `Braid.app`. macOS binds URL schemes to bundles, not executables,
    /// so this is what lets Braid become the magnet link handler at all.
    Bundle {
        /// Where to write the bundle. Defaults to `target/<profile>/Braid.app`.
        #[arg(long)]
        out: Option<std::path::PathBuf>,
        /// Bundle the release build instead of debug.
        #[arg(long)]
        release: bool,
        /// Cargo features for the bundled binary, e.g. `torrent`.
        #[arg(long)]
        features: Option<String>,
    },
    /// Build the host platform's native installer.
    Package {
        #[arg(long, default_value = "dist")]
        out: std::path::PathBuf,
    },
    /// Check the Windows installer definition without building the app.
    ///
    /// Minutes rather than the twenty a release build costs, because WiX does
    /// not care what is inside the executables it packages.
    InstallerCheck,
    /// Build the release binaries and report their size for this platform.
    Size,
    /// Verify dev-only crates are absent from the default dependency graph.
    ///
    /// Separate from `size` because it needs no build: it is a `cargo tree`,
    /// so it can run on every push where a release build cannot.
    VerifyRelease,
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Screenshots { out, port, release } => screenshots(&out, port, release),
        Command::Hero { out, port } => hero(&out, port),
        Command::Settings { out, port } => settings(&out, port),
        Command::Bundle { out, release, features } => {
            bundle::run(out.as_deref(), release, features.as_deref()).map(|_| ())
        }
        Command::Package { out } => package::run(&out),
        Command::InstallerCheck => package::installer_check(),
        Command::Size => size::run(),
        Command::VerifyRelease => size::verify_release_is_clean(),
    }
}

/// Drive the real UI headlessly and assert it responds.
///
/// Every check pairs a screenshot with a property read. Slint's own docs warn
/// that "no pixel diff" can mean a stale snapshot rather than a working UI, so
/// a picture alone is not evidence that anything happened.
fn screenshots(out: &std::path::Path, port: u16, release: bool) -> Result<()> {
    let binary = app::HeadlessApp::build_example(release, "window")?;
    let app = app::HeadlessApp::launch_with_args(&binary, port, &[])?;
    let mcp = &app.mcp;

    let window = mcp.first_window()?;
    let root = mcp.root_element(&window)?;

    // The fixture starts real transfers against a local mock origin. Long
    // enough that the throughput strip has more than a couple of samples.
    std::thread::sleep(std::time::Duration::from_secs(8));

    let elements = mcp.elements(&root)?;
    println!("element tree: {} elements", elements.len());

    let rows = count_rows(mcp, &root)?;
    anyhow::ensure!(rows > 0, "the download list is empty; the engine produced no rows");
    println!("  {rows} download row(s) present");

    mcp.screenshot(&window, &out.join("01-downloading.png"))?;
    println!("captured 01-downloading.png");

    // Pause the first download and confirm the row itself changes state.
    let Some((label, pause)) = mcp.find_by_label(&root, |l| l.starts_with("Pause "))? else {
        anyhow::bail!("no pause control found; the rows are not rendering their controls");
    };
    let filename = label.trim_start_matches("Pause ").to_string();
    println!("  pausing {filename:?}");

    let before = row_state(mcp, &root, &filename)?;
    anyhow::ensure!(
        before.contains("running"),
        "expected a running download before pausing, got {before:?}"
    );

    mcp.click(&pause)?;
    std::thread::sleep(std::time::Duration::from_millis(900));

    let after = row_state(mcp, &root, &filename)?;
    anyhow::ensure!(
        after.contains("paused"),
        "clicking pause did not pause the download: {before:?} -> {after:?}"
    );
    println!("  verified state {before:?} -> {after:?}");

    mcp.screenshot(&window, &out.join("02-paused.png"))?;
    println!("captured 02-paused.png");

    // Selection. Clicking a row is the only way into the Inspector, and a
    // selected row repaints almost every ink in it: a branch no other
    // capture reaches.
    let Some((selected_name, row)) = mcp.find_by_label(&root, |l| l.ends_with(".iso"))? else {
        anyhow::bail!("no download row found to select");
    };
    mcp.click(&row)?;
    std::thread::sleep(std::time::Duration::from_millis(300));

    let after = mcp.root_element(&window)?;
    let Some((_, handle)) = mcp.find_by_label(&after, |l| l == selected_name)? else {
        anyhow::bail!("the selected row vanished");
    };
    let props = mcp.properties(&handle)?;
    // Reported as a JSON bool, not as the string a Slint property read
    // returns elsewhere; accept either rather than depending on which.
    let selected = props
        .get("accessibleItemSelected")
        .is_some_and(|v| v.as_bool() == Some(true) || v.as_str() == Some("true"));
    anyhow::ensure!(selected, "clicking {selected_name:?} did not select it: {props}");
    println!("  selected {selected_name:?}");

    mcp.screenshot(&window, &out.join("05-selected.png"))?;
    println!("captured 05-selected.png");

    // The Inspector opens on selection, so the click above should already
    // have done it. Asserting that here is the point: the toolbar toggle is
    // not how anyone finds this panel.
    std::thread::sleep(std::time::Duration::from_millis(1200));

    let opened = mcp.root_element(&window)?;
    for tab in ["Info", "Pieces", "Connections", "File"] {
        anyhow::ensure!(
            mcp.find_by_label(&opened, |l| l == tab)?.is_some(),
            "the inspector is missing its {tab} tab"
        );
    }
    mcp.screenshot(&window, &out.join("06-inspector.png"))?;
    println!("captured 06-inspector.png");

    // Removal must ask before it destroys anything, and must offer the choice
    // about the file on disk.
    let Some((_, remove)) = mcp.find_by_label(&opened, |l| l.starts_with("Remove "))? else {
        anyhow::bail!("no remove control on any row");
    };
    mcp.click(&remove)?;
    std::thread::sleep(std::time::Duration::from_millis(400));

    let asking = mcp.root_element(&window)?;
    for control in ["Cancel", "Remove", "Delete files"] {
        anyhow::ensure!(
            mcp.find_by_label(&asking, |l| l == control)?.is_some(),
            "the remove confirmation has no {control:?} control"
        );
    }
    mcp.screenshot(&window, &out.join("10-remove.png"))?;
    println!("captured 10-remove.png");

    let Some((_, cancel)) = mcp.find_by_label(&asking, |l| l == "Cancel")? else {
        anyhow::bail!("no Cancel in the remove confirmation");
    };
    mcp.click(&cancel)?;
    std::thread::sleep(std::time::Duration::from_millis(400));
    anyhow::ensure!(
        count_rows(mcp, &mcp.root_element(&window)?)? == rows,
        "cancelling the confirmation still removed the transfer"
    );
    println!("  cancelling leaves the transfer alone");

    // Pieces, which is the tab with the grid in it.
    let Some((_, pieces)) = mcp.find_by_label(&opened, |l| l == "Pieces")? else {
        anyhow::bail!("no Pieces tab");
    };
    mcp.click(&pieces)?;
    std::thread::sleep(std::time::Duration::from_millis(900));
    mcp.screenshot(&window, &out.join("07-pieces.png"))?;
    println!("captured 07-pieces.png");

    // The remaining two tabs, so a panel that renders only its first one
    // cannot pass.
    for (tab, shot) in [("Connections", "08-connections.png"), ("File", "09-file.png")] {
        let root = mcp.root_element(&window)?;
        let Some((_, handle)) = mcp.find_by_label(&root, |l| l == tab)? else {
            anyhow::bail!("no {tab} tab");
        };
        mcp.click(&handle)?;
        std::thread::sleep(std::time::Duration::from_millis(900));
        mcp.screenshot(&window, &out.join(shot))?;
        println!("captured {shot}");
    }

    // The add sheet is a modal branch that nothing else reaches, which is
    // exactly where a layout mistake sits unnoticed.
    let Some((_, add)) = mcp.find_by_label(&root, |l| l == "Add Download")? else {
        anyhow::bail!("no Add control found in the toolbar");
    };
    mcp.click(&add)?;
    std::thread::sleep(std::time::Duration::from_millis(400));

    let sheet_root = mcp.root_element(&window)?;
    anyhow::ensure!(
        mcp.find_by_label(&sheet_root, |l| l == "Add Transfer")?.is_some(),
        "clicking Add did not open the sheet"
    );
    mcp.screenshot(&window, &out.join("03-add-sheet.png"))?;
    println!("captured 03-add-sheet.png");

    let Some((_, cancel)) = mcp.find_by_label(&sheet_root, |l| l == "Cancel")? else {
        anyhow::bail!("the sheet has no Cancel control");
    };
    mcp.click(&cancel)?;
    std::thread::sleep(std::time::Duration::from_millis(400));
    anyhow::ensure!(
        mcp.find_by_label(&mcp.root_element(&window)?, |l| l == "Add Transfer")?.is_none(),
        "cancelling did not dismiss the sheet"
    );
    println!("  sheet opens and dismisses");

    drop(app);

    // The empty list is a real state with its own copy, and an unexercised
    // branch is where a layout mistake hides.
    let empty = app::HeadlessApp::launch_with_args(&binary, port, &["--empty"])?;
    let window = empty.mcp.first_window()?;
    let root = empty.mcp.root_element(&window)?;
    // Long enough for the throughput strip to accumulate samples: an idle
    // window that has only just opened draws no trace at all, which is exactly
    // the state a bug in the idle trace would hide behind.
    std::thread::sleep(std::time::Duration::from_secs(12));

    anyhow::ensure!(count_rows(&empty.mcp, &root)? == 0, "the empty fixture produced rows");
    anyhow::ensure!(
        empty.mcp.find_by_label(&root, |l| l.starts_with("Pause "))?.is_none(),
        "an empty list still rendered row controls"
    );
    empty.mcp.screenshot(&window, &out.join("04-empty.png"))?;
    println!("captured 04-empty.png");

    println!("\nscreenshots written to {}", out.display());
    Ok(())
}

/// Download rows announce themselves as list items.
fn count_rows(mcp: &mcp::Mcp, root: &mcp::ElementHandle) -> Result<usize> {
    Ok(mcp
        .elements(root)?
        .iter()
        .filter(|e| e.get("accessibleRole").and_then(serde_json::Value::as_str) == Some("ListItem"))
        .count())
}

/// The accessible value of one row, which carries its progress and state.
fn row_state(mcp: &mcp::Mcp, root: &mcp::ElementHandle, filename: &str) -> Result<String> {
    let Some((_, handle)) = mcp.find_by_label(root, |l| l == filename)? else {
        anyhow::bail!("no row labelled {filename:?}");
    };
    mcp.accessible_value(&handle)?
        .ok_or_else(|| anyhow::anyhow!("row {filename:?} has no accessible-value"))
}

/// Every settings page, with a property read alongside each capture.
///
/// The pages are `if` branches on one property, so a page that fails to build
/// renders as an empty pane rather than an error: which a screenshot alone
/// would not catch. Hence the element count check on each.
fn settings(out: &std::path::Path, port: u16) -> Result<()> {
    let binary = app::HeadlessApp::build_example(false, "settings")?;
    std::fs::create_dir_all(out)?;

    for page in ["general", "network", "bandwidth", "integrity", "advanced"] {
        let app = app::HeadlessApp::launch_with_args(&binary, port, &["--page", page])?;
        let window = app.mcp.first_window()?;
        let root = app.mcp.root_element(&window)?;
        std::thread::sleep(std::time::Duration::from_millis(600));

        let elements = app.mcp.elements(&root)?;
        // The source list alone is five rows; a page that rendered nothing
        // would still clear a lower bar than this.
        anyhow::ensure!(
            elements.len() > 12,
            "the {page} page rendered only {} elements, so it is probably empty",
            elements.len()
        );
        anyhow::ensure!(
            app.mcp.find_by_label(&root, |l| l == "Network settings")?.is_some(),
            "the {page} page has no source list"
        );

        app.mcp.screenshot(&window, &out.join(format!("settings-{page}.png")))?;
        println!("captured settings-{page}.png ({} elements)", elements.len());
        drop(app);
    }

    println!("\nsettings screenshots written to {}", out.display());
    Ok(())
}

/// Spike 2. Renders the prototype in dark and light. Theme is an explicit
/// property on the window rather than a read of the system palette, so these
/// captures do not depend on the host's appearance setting.
fn hero(out: &std::path::Path, port: u16) -> Result<()> {
    let binary = app::HeadlessApp::build_example(false, "window")?;

    for (name, args) in [("hero-dark.png", &[][..]), ("hero-light.png", &["--light"][..])] {
        let app = app::HeadlessApp::launch_with_args(&binary, port, args)?;
        let window = app.mcp.first_window()?;

        // The fixture starts real transfers; capturing immediately would show
        // a list of rows at zero rather than the UI doing its job. The wait is
        // long enough for the throughput strip to accumulate a trace too: it
        // samples once a second, and a chart with two points proves nothing.
        std::thread::sleep(std::time::Duration::from_secs(8));

        app.mcp.screenshot(&window, &out.join(name))?;
        println!("captured {name}");
    }
    println!("\nwritten to {}", out.display());
    Ok(())
}
