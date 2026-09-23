//! One instance, however many times the system launches it.
//!
//! Clicking a magnet link asks the OS to open the registered handler, and the
//! OS's idea of "open" is to run the executable again. Without this, every
//! link would start a second copy of Braid with its own engine, its own
//! settings file and its own idea of what is downloading: two processes
//! writing the same `.part` files.
//!
//! So the first instance listens on a socket in the runtime directory and
//! every later launch hands its argument over and exits. The socket is a
//! filesystem object that outlives a crash, which is why a connection failure
//! is treated as "the previous instance is gone" rather than as an error.

#[cfg(unix)]
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

/// Where the socket lives.
///
/// `XDG_RUNTIME_DIR` is the right answer where it exists: it is per-user,
/// already mode 0700, and cleaned at logout. macOS has no such variable, so
/// the per-user temporary directory it hands every process is used instead,
/// with the uid in the name so two users on one machine do not collide in a
/// shared `/tmp`.
pub fn socket_path() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(dir).join("braid.sock");
    }
    let uid = user_id();
    std::env::temp_dir().join(format!("braid-{uid}.sock"))
}

#[cfg(unix)]
fn user_id() -> u32 {
    // SAFETY: `getuid` takes nothing, returns the calling process's real user
    // id, and cannot fail.
    unsafe { libc::getuid() }
}

#[cfg(not(unix))]
fn user_id() -> u32 {
    0
}

/// Hand `argument` to a running instance, if there is one.
///
/// `true` means another instance took it and this process should exit without
/// opening a window. `false` means this process is the first one and should
/// carry on and call [`listen`].
#[cfg(unix)]
pub fn hand_off(argument: &str) -> bool {
    hand_off_at(&socket_path(), argument)
}

#[cfg(unix)]
fn hand_off_at(path: &std::path::Path, argument: &str) -> bool {
    use std::os::unix::net::UnixStream;

    let Ok(mut stream) = UnixStream::connect(path) else {
        // Either nothing is running, or a previous instance died and left the
        // socket behind. Both mean "we are it"; `listen` clears the corpse.
        return false;
    };
    // Newline-terminated so the reader knows where the argument ends without
    // waiting for the peer to close, which a crashed sender never does.
    let line = format!("{}\n", argument.replace('\n', " "));
    match stream.write_all(line.as_bytes()).and_then(|()| stream.flush()) {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(%error, "could not hand the link to the running instance");
            false
        }
    }
}

#[cfg(not(unix))]
pub fn hand_off(_argument: &str) -> bool {
    // Windows needs a named pipe here, which is not built. Reporting `false`
    // starts a second window rather than silently dropping the link: worse
    // than the Unix behaviour, but visible rather than mysterious.
    false
}

/// Listen for links from later launches, for as long as the process runs.
///
/// `on_open` is called on a background thread with each argument received, so
/// callers that touch the UI must hop to the event loop themselves.
#[cfg(unix)]
pub fn listen(on_open: impl Fn(String) + Send + 'static) -> std::io::Result<()> {
    listen_at(&socket_path(), on_open)
}

#[cfg(unix)]
fn listen_at(
    path: &std::path::Path,
    on_open: impl Fn(String) + Send + 'static,
) -> std::io::Result<()> {
    use std::os::unix::net::UnixListener;

    // A socket left by a crashed instance refuses connections but still
    // occupies the name, so binding would fail forever until someone deleted
    // it by hand. `hand_off` has already established that nothing is
    // answering, so removing it here is safe.
    if path.exists() {
        let _ = std::fs::remove_file(path);
    }
    let listener = UnixListener::bind(path)?;
    restrict_to_owner(path);

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let mut line = String::new();
            if BufReader::new(stream).read_line(&mut line).is_ok() {
                let argument = line.trim().to_string();
                if !argument.is_empty() {
                    on_open(argument);
                }
            }
        }
    });
    Ok(())
}

#[cfg(not(unix))]
pub fn listen(_on_open: impl Fn(String) + Send + 'static) -> std::io::Result<()> {
    Err(std::io::Error::other("single-instance handoff is not built on this platform"))
}

/// Anything that can reach this socket can add transfers to this session, so
/// it is nobody else's business.
#[cfg(unix)]
fn restrict_to_owner(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

/// The link, file or URL a launch was asked to open, if any.
///
/// Anything that is not a flag: the system passes a magnet URI or a `.torrent`
/// path as a bare argument, and Braid has no other positional arguments.
pub fn argument_from(args: impl IntoIterator<Item = String>) -> Option<String> {
    args.into_iter().skip(1).find(|a| !a.starts_with('-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_executable_path_is_not_a_link_to_open() {
        // argv[0] is always there and is never the argument; treating it as
        // one made every launch try to download the binary.
        assert_eq!(argument_from(args(&["/usr/local/bin/downloader"])), None);
    }

    #[test]
    fn a_magnet_argument_is_found_past_any_flags() {
        assert_eq!(
            argument_from(args(&["downloader", "--light", "magnet:?xt=urn:btih:ab"])),
            Some("magnet:?xt=urn:btih:ab".to_string())
        );
        assert_eq!(
            argument_from(args(&["downloader", "/tmp/a.torrent"])),
            Some("/tmp/a.torrent".to_string())
        );
    }

    #[test]
    fn the_socket_is_per_user_rather_than_shared() {
        // A fixed `/tmp/braid.sock` is one user handing another their
        // transfer list, or being unable to start at all.
        let path = socket_path();
        let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        assert!(name.starts_with("braid"), "{name}");
        #[cfg(unix)]
        if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
            assert!(name.contains(&user_id().to_string()), "{name}");
        }
    }

    /// The round trip that matters: a second launch reaches the first one.
    ///
    /// Driven through the by-path pair rather than by setting
    /// `XDG_RUNTIME_DIR`: mutating the environment of a running multi-threaded
    /// process is unsound, and it would also aim the test at whatever socket a
    /// developer's own Braid is sitting on.
    #[cfg(unix)]
    #[test]
    fn a_second_launch_hands_its_link_to_the_first() {
        use std::sync::mpsc;

        let dir = tempfile::TempDir::with_prefix("braid-ipc").expect("a temp dir");
        let path = dir.path().join("braid.sock");

        // A corpse from a crashed instance must not stop the next one binding.
        std::fs::write(&path, b"not a socket").expect("the stale file is written");

        let (tx, rx) = mpsc::channel();
        listen_at(&path, move |link| {
            let _ = tx.send(link);
        })
        .expect("the listener binds over the stale socket");

        assert!(hand_off_at(&path, "magnet:?xt=urn:btih:ab"), "the running instance took the link");
        let received = rx.recv_timeout(std::time::Duration::from_secs(5)).expect("a link arrived");
        assert_eq!(received, "magnet:?xt=urn:btih:ab");
    }

    /// Nothing listening means this process is the first one, not an error.
    #[cfg(unix)]
    #[test]
    fn an_address_nobody_answers_means_we_are_the_first_instance() {
        let dir = tempfile::TempDir::with_prefix("braid-ipc-none").expect("a temp dir");
        assert!(!hand_off_at(&dir.path().join("braid.sock"), "magnet:?xt=urn:btih:ab"));
    }
}
