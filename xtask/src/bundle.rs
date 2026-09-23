//! Build `Braid.app`.
//!
//! macOS binds URL schemes and document types to *bundles*, not to
//! executables. Running `target/release/downloader` directly, Launch Services
//! has nothing to register, so no amount of code in the app can make it the
//! magnet handler. This is the missing half of that feature, not packaging
//! polish.
//!
//! The app still has to work when it is run out of `target/release`, which it
//! does: it simply reports that it cannot become the handler, and says why.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Must match `dl_gui::platform::BUNDLE_ID`, or Launch Services registers one
/// identifier and the app asks about another.
const BUNDLE_ID: &str = "com.braid.downloader";

pub fn run(out: Option<&Path>, release: bool, features: Option<&str>) -> Result<PathBuf> {
    if !cfg!(target_os = "macos") {
        bail!("an .app bundle is only meaningful on macOS");
    }
    let root = crate::app::workspace_root();
    let profile = if release { "release" } else { "debug" };

    let mut cargo = Command::new(crate::app::cargo());
    cargo.args(["build", "-p", "dl-gui", "--bin", "braid"]);
    if release {
        cargo.arg("--release");
    }
    if let Some(features) = features {
        cargo.args(["--features", features]);
    }
    if !cargo.status().context("running cargo build")?.success() {
        bail!("building dl-gui failed");
    }

    let executable = root.join("target").join(profile).join("braid");
    let bundle = out
        .map(Path::to_path_buf)
        .unwrap_or_else(|| root.join("target").join(profile).join("Braid.app"));

    // Rebuilt from scratch: a stale `Info.plist` from an older run is exactly
    // the kind of thing that makes a handler registration fail for reasons
    // nothing in the source explains.
    if bundle.exists() {
        std::fs::remove_dir_all(&bundle).context("clearing the previous bundle")?;
    }
    let contents = bundle.join("Contents");
    std::fs::create_dir_all(contents.join("MacOS"))?;
    std::fs::create_dir_all(contents.join("Resources"))?;

    std::fs::copy(&executable, contents.join("MacOS/braid"))
        .with_context(|| format!("copying {}", executable.display()))?;
    std::fs::write(contents.join("Info.plist"), info_plist())?;
    // Classic-era marker. Still read by some of Launch Services' older paths,
    // and it costs eight bytes.
    std::fs::write(contents.join("PkgInfo"), "APPL????")?;

    write_icon(&root, &contents.join("Resources"))?;

    println!("bundled {}", bundle.display());
    println!("  register it with:  open {}", bundle.display());
    println!("  then Settings → General → Handle magnet links");
    Ok(bundle)
}

/// The icon, as an `.icns` built from the brand PNGs.
///
/// Best-effort: `iconutil` ships with Xcode's command line tools and may not
/// be installed. A bundle with no icon still registers and still opens magnet
/// links, so a missing icon is a warning rather than a failure.
fn write_icon(root: &Path, resources: &Path) -> Result<()> {
    let brand = root.join("crates/dl-gui/ui/brand");
    let staging = resources.join("Braid.iconset");
    std::fs::create_dir_all(&staging)?;

    // The sizes `iconutil` expects, by the names it expects them under.
    for (size, name) in [
        (16, "icon_16x16.png"),
        (32, "icon_16x16@2x.png"),
        (32, "icon_32x32.png"),
        (64, "icon_32x32@2x.png"),
        (128, "icon_128x128.png"),
        (256, "icon_128x128@2x.png"),
        (256, "icon_256x256.png"),
        (512, "icon_256x256@2x.png"),
        (512, "icon_512x512.png"),
    ] {
        let source = brand.join(format!("icon-{size}.png"));
        if source.exists() {
            std::fs::copy(&source, staging.join(name))?;
        }
    }

    let built = Command::new("iconutil")
        .args(["-c".as_ref(), "icns".as_ref(), staging.as_os_str()])
        .status();
    let _ = std::fs::remove_dir_all(&staging);
    match built {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => {
            println!("  note: iconutil failed ({status}); the bundle has no icon");
            Ok(())
        }
        Err(error) => {
            println!("  note: iconutil is not installed ({error}); the bundle has no icon");
            Ok(())
        }
    }
}

/// The part that matters: `CFBundleURLTypes` claims `magnet:` and
/// `CFBundleDocumentTypes` claims `.torrent`. Launch Services reads these when
/// the bundle is registered, and the app's own switch then makes it the
/// default rather than merely one of the candidates.
fn info_plist() -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>Braid</string>
    <key>CFBundleDisplayName</key><string>Braid</string>
    <key>CFBundleIdentifier</key><string>{BUNDLE_ID}</string>
    <key>CFBundleExecutable</key><string>braid</string>
    <key>CFBundleIconFile</key><string>Braid</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
    <key>CFBundleShortVersionString</key><string>{version}</string>
    <key>CFBundleVersion</key><string>{version}</string>
    <key>LSMinimumSystemVersion</key><string>11.0</string>
    <key>NSHighResolutionCapable</key><true/>

    <key>CFBundleURLTypes</key>
    <array>
        <dict>
            <key>CFBundleURLName</key><string>BitTorrent magnet link</string>
            <key>CFBundleTypeRole</key><string>Viewer</string>
            <key>CFBundleURLSchemes</key>
            <array><string>magnet</string></array>
        </dict>
    </array>

    <key>CFBundleDocumentTypes</key>
    <array>
        <dict>
            <key>CFBundleTypeName</key><string>BitTorrent metainfo file</string>
            <key>CFBundleTypeRole</key><string>Viewer</string>
            <key>LSHandlerRank</key><string>Owner</string>
            <key>LSItemContentTypes</key>
            <array><string>org.bittorrent.torrent</string></array>
            <key>CFBundleTypeExtensions</key>
            <array><string>torrent</string></array>
        </dict>
    </array>
</dict>
</plist>
"#,
        version = env!("CARGO_PKG_VERSION")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three keys the whole feature rests on. A plist that builds and
    /// installs but declares none of them produces an app that Launch
    /// Services happily registers and never sends a link to.
    #[test]
    fn the_plist_claims_magnet_links_and_torrent_files() {
        let plist = info_plist();
        assert!(plist.contains("CFBundleURLSchemes"), "{plist}");
        assert!(plist.contains("<string>magnet</string>"), "{plist}");
        assert!(plist.contains("CFBundleDocumentTypes"), "{plist}");
        assert!(plist.contains("org.bittorrent.torrent"), "{plist}");
    }

    /// The identifier is written in two crates. If they drift, the app
    /// registers under one name and asks Launch Services about another, and
    /// the switch reverts with no explanation.
    #[test]
    fn the_bundle_identifier_matches_the_one_the_app_registers() {
        let source = include_str!("../../crates/dl-gui/src/platform.rs");
        assert!(
            source.contains(&format!("BUNDLE_ID: &str = \"{BUNDLE_ID}\"")),
            "dl-gui and xtask disagree about the bundle identifier"
        );
    }
}
