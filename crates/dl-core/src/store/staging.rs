//! Where a transfer's bytes live while it is still a transfer.
//!
//! By default a partial file sits beside its destination as `name.part`, which
//! keeps a move at the end to a rename within one directory: the cheapest and
//! most atomic thing a filesystem offers.
//!
//! A configured incomplete folder trades that for tidiness: nothing
//! half-finished appears in the download folder at all. The cost is that the
//! final move may cross filesystems, where `rename` fails outright and the
//! bytes have to be copied.

use crate::error::{Error, Result};
use std::path::{Path, PathBuf};

/// Where partial files are kept.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Staging {
    /// `name.part` beside the destination.
    #[default]
    Alongside,
    /// A folder of its own, shared by every transfer.
    Folder(PathBuf),
}

impl Staging {
    pub fn folder(path: impl Into<PathBuf>) -> Self {
        Self::Folder(path.into())
    }

    /// The partial file for `destination`, and its journal.
    ///
    /// In a shared folder the name carries a digest of the **full destination
    /// path**, not just the file name: two transfers both saving `image.iso`
    /// to different folders would otherwise land on the same partial file and
    /// quietly overwrite each other. Derived rather than random so that a
    /// resume finds the same file it left.
    pub fn paths_for(&self, destination: &Path) -> (PathBuf, PathBuf) {
        match self {
            Self::Alongside => (suffixed(destination, ".part"), suffixed(destination, ".dlmeta")),
            Self::Folder(folder) => {
                let name = destination
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "download".into());
                let stem = format!("{name}.{}", tag(destination));
                (folder.join(format!("{stem}.part")), folder.join(format!("{stem}.dlmeta")))
            }
        }
    }

    /// Create the staging folder if there is one.
    pub fn prepare(&self) -> Result<()> {
        match self {
            Self::Alongside => Ok(()),
            Self::Folder(folder) => {
                std::fs::create_dir_all(folder).map_err(|e| Error::io(folder.display(), e))
            }
        }
    }
}

/// A short, stable tag for a path.
///
/// Sixteen hex characters of BLAKE3: long enough that a collision between two
/// destinations on one machine is not a thing that happens, short enough to
/// leave the name readable in a folder someone may open.
pub fn short_tag(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex()[..16].to_string()
}

fn tag(path: &Path) -> String {
    short_tag(path.as_os_str().as_encoded_bytes())
}

/// The sidecar path for a destination: `x.iso` → `x.iso.part`.
pub fn suffixed(destination: &Path, suffix: &str) -> PathBuf {
    let mut name = destination.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    destination.with_file_name(name)
}

/// Move a finished file to where it belongs.
///
/// A rename when the two sit on one filesystem, which is atomic and instant.
/// Across filesystems `rename` fails with `EXDEV` and there is no atomic
/// option, so the bytes are copied to a temporary name beside the destination
/// and *then* renamed into place: a copy straight onto the destination would
/// leave a half-written file under the final name if the machine died in the
/// middle of it.
pub fn publish(from: &Path, to: &Path) -> std::io::Result<()> {
    if let Some(parent) = to.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::rename(from, to) {
        Ok(()) => return Ok(()),
        Err(error) if error.raw_os_error() != Some(EXDEV) => return Err(error),
        Err(_) => {}
    }

    let staged = suffixed(to, ".incoming");
    std::fs::copy(from, &staged)?;
    match std::fs::rename(&staged, to) {
        Ok(()) => {
            std::fs::remove_file(from)?;
            Ok(())
        }
        Err(error) => {
            // Leave the source alone: it is still the only complete copy.
            let _ = std::fs::remove_file(&staged);
            Err(error)
        }
    }
}

#[cfg(unix)]
const EXDEV: i32 = libc_exdev::EXDEV;

#[cfg(unix)]
mod libc_exdev {
    /// `EXDEV` is 18 on Linux and on Apple platforms alike.
    pub const EXDEV: i32 = 18;
}

#[cfg(windows)]
/// `ERROR_NOT_SAME_DEVICE`.
const EXDEV: i32 = 17;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alongside_puts_the_partial_next_to_the_destination() {
        let (part, meta) = Staging::Alongside.paths_for(Path::new("/tmp/a/x.iso"));
        assert_eq!(part, Path::new("/tmp/a/x.iso.part"));
        assert_eq!(meta, Path::new("/tmp/a/x.iso.dlmeta"));
    }

    #[test]
    fn a_folder_keeps_the_name_readable() {
        let (part, _) = Staging::folder("/var/incomplete").paths_for(Path::new("/tmp/x.iso"));
        let name = part.file_name().unwrap().to_string_lossy().into_owned();
        assert!(part.starts_with("/var/incomplete"));
        assert!(name.starts_with("x.iso."), "{name}");
        assert!(name.ends_with(".part"), "{name}");
    }

    #[test]
    fn two_destinations_with_the_same_name_do_not_share_a_partial() {
        // Saving `image.iso` to two folders, both staging into one shared
        // directory, must not have them overwrite each other.
        let staging = Staging::folder("/var/incomplete");
        let (a, _) = staging.paths_for(Path::new("/home/me/one/image.iso"));
        let (b, _) = staging.paths_for(Path::new("/home/me/two/image.iso"));
        assert_ne!(a, b);
    }

    #[test]
    fn the_partial_for_one_destination_is_always_the_same() {
        // Otherwise a resume would never find what the last run left.
        let staging = Staging::folder("/var/incomplete");
        assert_eq!(
            staging.paths_for(Path::new("/tmp/x.iso")),
            staging.paths_for(Path::new("/tmp/x.iso"))
        );
    }

    #[test]
    fn the_journal_sits_with_its_own_partial() {
        let (part, meta) = Staging::folder("/var/incomplete").paths_for(Path::new("/tmp/x.iso"));
        assert_eq!(part.parent(), meta.parent());
        assert_eq!(
            part.file_name().unwrap().to_string_lossy().trim_end_matches(".part"),
            meta.file_name().unwrap().to_string_lossy().trim_end_matches(".dlmeta")
        );
    }

    #[test]
    fn publishing_within_one_filesystem_moves_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("staged.part");
        let to = dir.path().join("final/x.iso");
        std::fs::write(&from, b"bytes").unwrap();

        publish(&from, &to).unwrap();
        assert!(!from.exists());
        assert_eq!(std::fs::read(&to).unwrap(), b"bytes");
    }

    #[test]
    fn publishing_leaves_no_temporary_behind() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("staged.part");
        let to = dir.path().join("x.iso");
        std::fs::write(&from, b"bytes").unwrap();

        publish(&from, &to).unwrap();
        assert!(!suffixed(&to, ".incoming").exists());
    }
}
