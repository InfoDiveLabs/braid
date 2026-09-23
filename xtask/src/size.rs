//! Release binary size measurement, and proof that dev-only code stays out.
//!
//! Binary size is a stated product goal: femtovg was chosen over skia for it.
//! So it is worth measuring on every release build and printing where someone
//! will see it.
//!
//! Reported per platform rather than checked against a stored figure. A macOS
//! arm64 binary and a Linux x86_64 one built from identical source differ by
//! half their size, so a single number does not say how the binary is
//! trending, only which machine measured it.
//!
//! [`verify_release_is_clean`] is the half worth running everywhere: it is a
//! `cargo tree`, needs no build, and catches dev-only crates reaching a
//! shipped binary.

use anyhow::{Context, Result, bail};
use std::process::Command;

const BINARIES: &[(&str, &str)] = &[("dl-cli", "dl"), ("dl-gui", "braid")];

/// What the host calls an executable.
const EXE: &str = if cfg!(windows) { ".exe" } else { "" };

pub fn run() -> Result<()> {
    let root = crate::app::workspace_root();

    for (package, _) in BINARIES {
        let status = Command::new(crate::app::cargo())
            .args(["build", "--release", "-p", package])
            .status()
            .context("running cargo build --release")?;
        if !status.success() {
            bail!("release build of {package} failed");
        }
    }

    let target = host_target()?;
    println!("\nrelease binaries for {target}");
    for (_, bin) in BINARIES {
        let path = root.join("target/release").join(format!("{bin}{EXE}"));
        let bytes =
            std::fs::metadata(&path).with_context(|| format!("stat {}", path.display()))?.len();
        println!("  {bin:<8} {bytes:>12} bytes ({:.2} MB)", bytes as f64 / 1_048_576.0);
    }

    verify_release_is_clean()
}

/// The triple the binaries were built for, so a reported figure says which
/// platform it belongs to rather than floating free.
fn host_target() -> Result<String> {
    let out = Command::new("rustc").arg("-vV").output().context("running rustc -vV")?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("host: "))
        .map(str::to_string)
        .context("rustc -vV did not report a host triple")
}

/// Prove the `devtools` feature really is absent from a default release build.
/// A feature that is only *believed* to be compiled out is not compiled out.
pub fn verify_release_is_clean() -> Result<()> {
    for package in ["dl-cli", "dl-gui"] {
        // `-e normal` excludes dev- and build-dependencies, which never reach the
        // shipped binary. Without it this trips on dl-gui's dev-dependency on
        // i-slint-backend-testing.
        let out = Command::new(crate::app::cargo())
            .args([
                "tree",
                "--no-default-features",
                "-p",
                package,
                "--prefix",
                "none",
                "-e",
                "normal",
            ])
            .output()
            .context("running cargo tree")?;
        let tree = String::from_utf8_lossy(&out.stdout);
        for forbidden in ["dl-testkit", "i-slint-backend-testing"] {
            if tree.lines().any(|l| l.trim_start().starts_with(forbidden)) {
                bail!(
                    "{forbidden} is in {package}'s default dependency graph: \
                     dev-only code is leaking into release builds"
                );
            }
        }
    }
    println!("\nverified: dl-testkit and i-slint-backend-testing are absent from release builds");
    Ok(())
}
