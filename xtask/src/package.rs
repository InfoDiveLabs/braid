//! Native installers, one per desktop.
//!
//! Each platform gets the artifact its users expect to double-click: a `.dmg`
//! on macOS, `.deb` and `.rpm` on Linux, an MSI on Windows. The metadata that
//! makes them behave: the icon, the MIME claims for `magnet:` and
//! `.torrent`, the desktop entry: lives here and in the manifests rather than
//! being typed into a release checklist.
//!
//! Only the host's own format can be built: there is no cross-packaging here,
//! and a release comes from a CI matrix with one runner per target.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(any(target_os = "macos", target_os = "linux"))]
const APP: &str = "Braid";
#[cfg(target_os = "linux")]
const BINARY: &str = "braid";

pub fn run(out: &Path) -> Result<()> {
    std::fs::create_dir_all(out)?;
    let version = version()?;

    #[cfg(target_os = "macos")]
    println!("built {}", dmg(out, &version)?.display());

    #[cfg(target_os = "linux")]
    for artifact in linux(out, &version)? {
        println!("built {}", artifact.display());
    }

    #[cfg(target_os = "windows")]
    println!("built {}", msi(out, &version)?.display());

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    bail!("no packaging defined for this platform");

    Ok(())
}

fn version() -> Result<String> {
    let manifest = std::fs::read_to_string("Cargo.toml")?;
    manifest
        .lines()
        .find_map(|line| line.strip_prefix("version = \"")?.strip_suffix('"'))
        .map(str::to_string)
        .context("no version in the workspace manifest")
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn cargo(args: &[&str]) -> Result<()> {
    let status = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args(args)
        .status()?;
    if !status.success() {
        bail!("cargo {} failed", args.join(" "));
    }
    Ok(())
}

/// A disk image holding the `.app` and a link to `/Applications`.
///
/// The link is what makes the window a drag-and-drop install rather than a
/// folder someone has to know what to do with.
///
/// Compiled on macOS only: `hdiutil` and `std::os::unix` are both absent
/// elsewhere, and `cfg!` alone would still have to typecheck the body.
#[cfg(target_os = "macos")]
fn dmg(out: &Path, version: &str) -> Result<PathBuf> {
    let app = crate::bundle::run(None, true, None)?;
    make_universal(&app)?;
    let staging = out.join("dmg-root");
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)?;

    copy_tree(&app, &staging.join(format!("{APP}.app")))?;
    let link = staging.join("Applications");
    std::os::unix::fs::symlink("/Applications", &link)?;
    std::fs::copy("README.md", staging.join("README.md"))?;
    std::fs::copy("LICENSE", staging.join("LICENSE"))?;

    let dmg = out.join(format!("{APP}-{version}-macos.dmg"));
    let _ = std::fs::remove_file(&dmg);
    let status = Command::new("hdiutil")
        .args(["create", "-volname", APP, "-srcfolder"])
        .arg(&staging)
        .args(["-ov", "-format", "UDZO"])
        .arg(&dmg)
        .status()?;
    if !status.success() {
        bail!("hdiutil failed");
    }
    std::fs::remove_dir_all(&staging)?;
    Ok(dmg)
}

/// `.deb` and `.rpm`, through the tools that know those formats.
///
/// The desktop entry and the icons are written here first: both packagers read
/// them from the tree, and a package without them installs an application the
/// launcher cannot find and the file manager will not offer for a `.torrent`.
#[cfg(target_os = "linux")]
fn linux(out: &Path, _version: &str) -> Result<Vec<PathBuf>> {
    write_desktop_files()?;
    cargo(&["build", "--release", "-p", "dl-gui", "-p", "dl-cli"])?;

    let mut built = Vec::new();
    for (tool, args) in [
        ("cargo-deb", vec!["deb", "-p", "dl-gui", "--no-build", "-o"]),
        ("cargo-generate-rpm", vec!["generate-rpm", "-p", "crates/dl-gui", "-o"]),
    ] {
        if which(tool).is_none() {
            eprintln!("{tool} is not installed; skipping that format");
            continue;
        }
        let mut argv = args;
        let target = out.to_string_lossy().into_owned();
        argv.push(&target);
        cargo(&argv)?;
    }
    for entry in std::fs::read_dir(out)?.flatten() {
        let path = entry.path();
        if matches!(path.extension().and_then(|e| e.to_str()), Some("deb" | "rpm")) {
            built.push(path);
        }
    }
    Ok(built)
}

/// The launcher entry and MIME claims.
///
/// `x-scheme-handler/magnet` is what makes a magnet link in a browser open
/// here; `application/x-bittorrent` does the same for a `.torrent` file in a
/// file manager. Without them the application installs and is never offered.
#[cfg(target_os = "linux")]
fn write_desktop_files() -> Result<()> {
    let dir = Path::new("packaging/linux");
    std::fs::create_dir_all(dir)?;
    std::fs::write(
        dir.join("braid.desktop"),
        format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name={APP}\n\
             GenericName=Download Manager\n\
             Comment=Split one download across every network path you have\n\
             Exec={BINARY} %u\n\
             Icon=braid\n\
             Terminal=false\n\
             Categories=Network;FileTransfer;\n\
             MimeType=x-scheme-handler/magnet;application/x-bittorrent;\n\
             StartupWMClass=Braid\n"
        ),
    )?;

    // hicolor is the theme every desktop falls back to, and the sizes are the
    // ones launchers and notification daemons actually ask for.
    for size in [16, 32, 64, 128, 256, 512] {
        let from = PathBuf::from(format!("crates/dl-gui/ui/brand/icon-{size}.png"));
        let to = dir.join(format!("icons/hicolor/{size}x{size}/apps/braid.png"));
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&from, &to)
            .with_context(|| format!("copying {} to {}", from.display(), to.display()))?;
    }
    Ok(())
}

/// An MSI, through WiX.
///
/// Unverified: there is no Windows hardware here and no way to run the result.
/// The configuration follows WiX's documentation and is built by CI, which
/// proves it compiles and nothing more. Treat a first install as untested.
#[cfg(target_os = "windows")]
fn msi(out: &Path, _version: &str) -> Result<PathBuf> {
    if which("cargo-wix").is_none() {
        bail!("cargo-wix is not installed: cargo install cargo-wix");
    }
    cargo(&["build", "--release", "-p", "dl-gui", "-p", "dl-cli"])?;
    write_sidecars()?;
    cargo(&["wix", "-p", "dl-gui", "--nocapture", "-o", &out.to_string_lossy()])?;
    for entry in std::fs::read_dir(out)?.flatten() {
        if entry.path().extension().and_then(|e| e.to_str()) == Some("msi") {
            return Ok(entry.path());
        }
    }
    bail!("cargo-wix reported success but produced no .msi")
}

#[cfg(target_os = "windows")]
/// The three files `main.wxs` refers to.
///
/// Generated rather than committed, so the installer's icon is the same image
/// the other two platforms ship and the licence in its dialog is the licence
/// in the repository.
fn write_sidecars() -> Result<()> {
    let wix = Path::new("crates/dl-gui/wix");
    std::fs::create_dir_all(wix)?;
    write_ico(&wix.join("braid.ico"))?;
    std::fs::copy("LICENSE", wix.join("LICENSE.txt")).context("copying LICENSE for the MSI")?;

    let rtf = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args(["wix", "print", "GPL-3.0", "-p", "dl-gui"])
        .output()
        .context("running cargo wix print GPL-3.0")?;
    if !rtf.status.success() {
        bail!("cargo wix could not render the licence for the installer dialog");
    }
    std::fs::write(wix.join("License.rtf"), rtf.stdout)?;
    Ok(())
}

/// Build the installer against placeholder executables.
///
/// The installer definition does not depend on what is inside the binaries,
/// only on their names, so this answers "does WiX accept this?" without the
/// `lto = "fat"` release build that answers a different question and takes
/// twenty minutes to do it. The MSI it leaves behind installs nothing that
/// runs; it exists to be discarded.
#[cfg(target_os = "windows")]
pub fn installer_check() -> Result<()> {
    if which("cargo-wix").is_none() {
        bail!("cargo-wix is not installed: cargo install cargo-wix");
    }

    let release = Path::new("target/release");
    std::fs::create_dir_all(release)?;
    for binary in ["braid.exe", "dl.exe"] {
        let path = release.join(binary);
        if !path.exists() {
            std::fs::write(&path, b"placeholder, not a program")?;
            println!("stubbed {}", path.display());
        }
    }

    write_sidecars()?;

    let out = Path::new("target/installer-check");
    std::fs::create_dir_all(out)?;
    cargo(&["wix", "-p", "dl-gui", "--nocapture", "-o", &out.to_string_lossy()])?;

    for entry in std::fs::read_dir(out)?.flatten() {
        if entry.path().extension().and_then(|e| e.to_str()) == Some("msi") {
            println!("\nWiX accepted the installer definition: {}", entry.path().display());
            println!("Not installable: the executables in it are placeholders.");
            return Ok(());
        }
    }
    bail!("cargo-wix reported success but produced no .msi")
}

/// Every other platform builds its own installer, and there is nothing here to
/// check without WiX.
#[cfg(not(target_os = "windows"))]
pub fn installer_check() -> Result<()> {
    bail!("the installer check needs WiX, which runs on Windows only")
}

#[cfg(target_os = "windows")]
/// A Windows icon, from the same PNGs every other platform is given.
///
/// Written here rather than drawn again: an installer whose icon does not
/// match the application's is a different application as far as anyone
/// looking at it is concerned.
///
/// The sizes are stored as PNG inside the `.ico`, which Windows has read since
/// Vista and which keeps a 256px icon from costing 256 KB uncompressed.
fn write_ico(to: &Path) -> Result<()> {
    // 256 is last: the directory is conventionally ordered smallest first, and
    // a 256 entry records its size as 0.
    const SIZES: [u32; 5] = [16, 32, 64, 128, 256];

    let images = SIZES
        .iter()
        .map(|size| {
            let from = format!("crates/dl-gui/ui/brand/icon-{size}.png");
            std::fs::read(&from).with_context(|| format!("reading {from}"))
        })
        .collect::<Result<Vec<_>>>()?;

    std::fs::write(to, ico_bytes(&SIZES, &images)).with_context(|| format!("writing {to:?}"))
}

/// The container: a six byte header, one sixteen byte entry per image, then
/// the images themselves.
#[cfg(any(target_os = "windows", test))]
fn ico_bytes(sizes: &[u32], images: &[Vec<u8>]) -> Vec<u8> {
    let count = images.len() as u16;
    let mut out = Vec::new();
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&1u16.to_le_bytes()); // an icon, not a cursor
    out.extend_from_slice(&count.to_le_bytes());

    let mut offset = 6 + 16 * images.len() as u32;
    for (size, image) in sizes.iter().zip(images) {
        // 256 does not fit in a byte and is written as zero, which is the
        // format's own way of saying it.
        let dimension = if *size >= 256 { 0u8 } else { *size as u8 };
        out.push(dimension);
        out.push(dimension);
        out.push(0); // palette size: none, the image is true colour
        out.push(0); // reserved
        out.extend_from_slice(&1u16.to_le_bytes()); // colour planes
        out.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
        out.extend_from_slice(&(image.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += image.len() as u32;
    }
    for image in images {
        out.extend_from_slice(image);
    }
    out
}

/// Replace the bundled executable with one that runs on both Mac
/// architectures.
///
/// Built separately and joined with `lipo`, so a single download works on
/// Apple silicon and on Intel. A missing target is not fatal: a developer
/// build on one machine should not require both toolchains, and the bundle is
/// still correct for the host.
#[cfg(target_os = "macos")]
fn make_universal(app: &Path) -> Result<()> {
    let executable = app.join("Contents/MacOS").join(APP.to_lowercase());
    let mut slices = Vec::new();

    for target in ["aarch64-apple-darwin", "x86_64-apple-darwin"] {
        let status = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
            .args(["build", "--release", "-p", "dl-gui", "--bin", "braid", "--target", target])
            .status()?;
        if !status.success() {
            eprintln!("{target} did not build; the bundle stays single-architecture");
            return Ok(());
        }
        slices.push(PathBuf::from("target").join(target).join("release/braid"));
    }

    let joined = app.join("Contents/MacOS/braid-universal");
    let mut lipo = Command::new("lipo");
    lipo.arg("-create");
    for slice in &slices {
        lipo.arg(slice);
    }
    if !lipo.arg("-output").arg(&joined).status()?.success() {
        eprintln!("lipo failed; the bundle stays single-architecture");
        return Ok(());
    }
    std::fs::rename(&joined, &executable)?;
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn which(tool: &str) -> Option<PathBuf> {
    let name = tool.strip_prefix("cargo-").unwrap_or(tool);
    Command::new("cargo")
        .args([name, "--version"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|_| PathBuf::from(tool))
}

#[cfg(target_os = "macos")]
fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    if from.is_dir() {
        std::fs::create_dir_all(to)?;
        for entry in std::fs::read_dir(from)?.flatten() {
            copy_tree(&entry.path(), &to.join(entry.file_name()))?;
        }
    } else {
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(from, to)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Windows reads an icon by seeking to the offsets in its directory. Get
    /// one wrong and the file opens to nothing, on a platform where that
    /// cannot be seen from here.
    #[test]
    fn every_image_is_where_the_directory_says_it_is() {
        let sizes = [16, 256];
        let images = vec![vec![0xAAu8; 40], vec![0xBBu8; 70]];
        let ico = ico_bytes(&sizes, &images);

        assert_eq!(&ico[0..2], &[0, 0], "reserved");
        assert_eq!(&ico[2..4], &[1, 0], "an icon, not a cursor");
        assert_eq!(&ico[4..6], &[2, 0], "two images");

        for (index, image) in images.iter().enumerate() {
            let entry = 6 + 16 * index;
            let len = u32::from_le_bytes(ico[entry + 8..entry + 12].try_into().unwrap()) as usize;
            let at = u32::from_le_bytes(ico[entry + 12..entry + 16].try_into().unwrap()) as usize;
            assert_eq!(len, image.len());
            assert_eq!(&ico[at..at + len], image.as_slice(), "image {index} is not at its offset");
        }
    }

    #[test]
    fn a_256_pixel_icon_records_its_size_as_zero() {
        // The field is one byte, so the largest size the format allows does
        // not fit in it. Writing 255, or truncating to 0 by accident, are
        // different bugs with the same symptom.
        let ico = ico_bytes(&[16, 256], &[vec![0; 1], vec![0; 1]]);
        assert_eq!((ico[6], ico[7]), (16, 16));
        assert_eq!((ico[22], ico[23]), (0, 0));
    }
}
