//! Platform shell integration: window chrome and vibrancy.
//!
//! On macOS the window keeps its title bar for dragging and for the traffic
//! lights, but draws nothing: the content view runs full height and our own
//! sidebar paints behind the window controls. The room the UI leaves for those
//! buttons is a UI decision, and lives with the other per-target values in
//! `ui/platform/macos.slint`.
//!
//! Verified on macOS 26.6; see `docs/spikes.md`. Three constraints shape this
//! API: winit's `set_blur` is an empty body on Windows and X11 and needs the
//! KDE protocol on Wayland; it blurs only what is behind the window, not
//! `backdrop-filter`; and macOS routes it through the private
//! `CGSSetWindowBackgroundBlurRadius`, which can stop working at any release.
//!
//! So vibrancy is requested, never assumed. Callers are told what they got, and
//! the UI stays opaque unless it was granted.

/// Whether the shell should ask the compositor to blur behind the window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Vibrancy {
    /// Opaque window. The default, and the only option on most platforms.
    #[default]
    Off,
    /// Request desktop blur, falling back to opaque where unsupported.
    Requested,
}

/// What the platform actually granted. Never inferred from [`Vibrancy`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ShellCapabilities {
    pub vibrancy: bool,
}

/// What [`install`] should do, kept separate from doing it.
///
/// Installing a backend has to happen on the main thread before any window
/// exists, so the decision is the only part a unit test can reach.
/// `None` leaves the backend alone; `Some(blur)` installs ours.
fn plan(requested: Vibrancy, backend_pinned: bool) -> Option<bool> {
    // An explicit backend choice wins. Installing ours would override
    // `SLINT_BACKEND=headless` and break every headless capture.
    if backend_pinned { None } else { Some(requested == Vibrancy::Requested) }
}

/// Install the windowing backend, requesting vibrancy if asked and available.
///
/// Must be called before any window is created. Returns what was granted.
pub fn install(requested: Vibrancy) -> ShellCapabilities {
    let Some(want_blur) = plan(requested, std::env::var_os("SLINT_BACKEND").is_some()) else {
        tracing::debug!("SLINT_BACKEND is set; leaving the backend alone");
        return ShellCapabilities::default();
    };
    install_shell(want_blur)
}

#[cfg(all(target_os = "macos", feature = "native-shell"))]
fn install_shell(want_blur: bool) -> ShellCapabilities {
    use i_slint_backend_winit::winit::platform::macos::WindowAttributesExtMacOS;

    // Unlike blur, the seamless title bar is not conditional: it is the window
    // the UI is laid out for, and falling back to a grey system bar would leave
    // an empty inset strip at the top of the sidebar.
    let backend = match i_slint_backend_winit::Backend::builder()
        .with_window_attributes_hook(move |attributes| {
            let attributes = attributes
                .with_fullsize_content_view(true)
                .with_titlebar_transparent(true)
                .with_title_hidden(true);
            // Blur is invisible without transparency.
            if want_blur { attributes.with_transparent(true).with_blur(true) } else { attributes }
        })
        .build()
    {
        Ok(backend) => backend,
        Err(e) => {
            tracing::warn!(error = %e, "could not build the winit backend; using a system window");
            return ShellCapabilities::default();
        }
    };

    match slint::platform::set_platform(Box::new(backend)) {
        Ok(()) => ShellCapabilities { vibrancy: want_blur },
        Err(e) => {
            // Already set, typically because a window was created first.
            tracing::warn!(error = %e, "could not install the winit backend; using a system window");
            ShellCapabilities::default()
        }
    }
}

#[cfg(not(all(target_os = "macos", feature = "native-shell")))]
fn install_shell(_want_blur: bool) -> ShellCapabilities {
    tracing::debug!("no native shell integration on this platform or build");
    ShellCapabilities::default()
}

/// Register or remove the app as a login item.
///
/// On macOS this writes a LaunchAgent plist rather than calling
/// `SMAppService`: that API requires a bundled, signed app, and Braid has to
/// behave the same when it is run straight out of `target/release`. The plist
/// is the documented fallback and `launchctl` picks it up on the next login.
///
/// Returns what is actually in effect. An error here is reported, not
/// swallowed: a switch that stays on while nothing was registered is the
/// worst of both.
pub fn set_launch_at_login(enabled: bool) -> std::io::Result<bool> {
    #[cfg(target_os = "macos")]
    {
        let Some(home) = std::env::var_os("HOME") else {
            return Err(std::io::Error::other("no HOME to write a LaunchAgent into"));
        };
        let dir = std::path::PathBuf::from(home).join("Library/LaunchAgents");
        let plist = dir.join(format!("{BUNDLE_ID}.plist"));
        if !enabled {
            match std::fs::remove_file(&plist) {
                Ok(()) => return Ok(false),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                Err(e) => return Err(e),
            }
        }
        let executable = std::env::current_exe()?;
        std::fs::create_dir_all(&dir)?;
        std::fs::write(&plist, login_plist(&executable))?;
        Ok(true)
    }
    #[cfg(not(target_os = "macos"))]
    {
        // Nothing is registered anywhere else yet, so claiming success would
        // be a lie the user only discovers at the next login.
        let _ = enabled;
        Err(std::io::Error::other("launch at login is macOS-only so far"))
    }
}

#[cfg(target_os = "macos")]
fn login_plist(executable: &std::path::Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>{BUNDLE_ID}</string>
    <key>ProgramArguments</key><array><string>{}</string></array>
    <key>RunAtLoad</key><true/>
</dict>
</plist>
"#,
        executable.display()
    )
}

/// The bundle identifier Braid registers under. Also the LaunchAgent label,
/// so the two never drift apart.
pub const BUNDLE_ID: &str = "com.braid.downloader";

/// Register or unregister Braid as the system handler for `magnet:` links and
/// `.torrent` files.
///
/// Returns **what is actually in effect**, re-read from the system rather than
/// inferred from whether the call returned. Following `set_launch_at_login`:
/// a switch that stays on while nothing was registered is the worst of both,
/// and this one fails routinely: on macOS because the app is not in a bundle,
/// on Linux because there is no `xdg-mime`.
pub fn set_magnet_handler(enabled: bool) -> std::io::Result<bool> {
    #[cfg(target_os = "macos")]
    return macos_handler::set(enabled);

    #[cfg(target_os = "linux")]
    return linux_handler::set(enabled);

    #[cfg(windows)]
    return windows_handler::set(enabled);

    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        let _ = enabled;
        Err(std::io::Error::other("no URL handler registration on this platform"))
    }
}

/// Whether the system currently sends `magnet:` links to this app.
///
/// Read at launch so the switch shows the truth rather than what was stored
/// last time: the registration lives outside our settings file and another
/// torrent client can take it without telling us.
pub fn magnet_handler_is_us() -> bool {
    #[cfg(target_os = "macos")]
    return macos_handler::is_us();

    #[cfg(target_os = "linux")]
    return linux_handler::is_us();

    #[cfg(windows)]
    return windows_handler::is_us();

    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    false
}

#[cfg(target_os = "macos")]
mod macos_handler {
    use std::io::{Error, Result};
    use std::path::{Path, PathBuf};

    /// Launch Services' own registration tool. Documented by Apple and stable
    /// across releases; there is no public API that registers a bundle by path.
    const LSREGISTER: &str = "/System/Library/Frameworks/CoreServices.framework/Frameworks/\
                              LaunchServices.framework/Support/lsregister";

    /// The `.app` this executable is inside, or `None` when it is not.
    ///
    /// macOS binds URL schemes to bundles, not to executables. Running out of
    /// `target/release` there is simply nothing for Launch Services to
    /// register, which is a fact to report rather than a failure to hide: /// `cargo xtask bundle` is what produces one.
    pub fn bundle_path() -> Option<PathBuf> {
        let exe = std::env::current_exe().ok()?;
        // …/Braid.app/Contents/MacOS/downloader
        let bundle = exe.parent()?.parent()?.parent()?;
        match bundle.extension().is_some_and(|e| e == "app") {
            true => Some(bundle.to_path_buf()),
            false => None,
        }
    }

    pub fn set(enabled: bool) -> Result<bool> {
        let Some(bundle) = bundle_path() else {
            return Err(Error::other(
                "macOS binds URL schemes to app bundles; run Braid.app (cargo xtask bundle) \
                 rather than the bare executable",
            ));
        };
        if !enabled {
            // Launch Services has no "unclaim". The honest move is to say so
            // and leave the switch where the system actually is, rather than
            // turning it off in the UI while magnet links still open Braid.
            return Err(Error::other(
                "macOS has no way to unclaim a URL scheme; pick another handler in that app, \
                 or in System Settings",
            ));
        }
        register_bundle(&bundle)?;
        set_default_scheme_handler("magnet", super::BUNDLE_ID)?;
        Ok(is_us())
    }

    /// Tell Launch Services this bundle exists, so the `CFBundleURLTypes` and
    /// `CFBundleDocumentTypes` in its `Info.plist` are seen at all. A bundle
    /// that has never been opened from Finder is otherwise unknown to it.
    fn register_bundle(bundle: &Path) -> Result<()> {
        let status = std::process::Command::new(LSREGISTER)
            .args(["-f".as_ref(), bundle.as_os_str()])
            .status()?;
        match status.success() {
            true => Ok(()),
            false => Err(Error::other(format!("lsregister refused the bundle ({status})"))),
        }
    }

    pub fn is_us() -> bool {
        default_scheme_handler("magnet").is_some_and(|id| id.eq_ignore_ascii_case(super::BUNDLE_ID))
    }

    // ---------------------------------------------------------- CoreFoundation
    //
    // Launch Services' scheme API is C, and there is no crate in this
    // workspace that wraps it. Four symbols and two CFString helpers is less
    // surface than a dependency that pulls in an Objective-C runtime.

    type CFTypeRef = *const std::ffi::c_void;
    type CFStringRef = CFTypeRef;
    type CFAllocatorRef = CFTypeRef;

    const UTF8: u32 = 0x0800_0100;

    unsafe extern "C" {
        fn CFStringCreateWithBytes(
            allocator: CFAllocatorRef,
            bytes: *const u8,
            num_bytes: isize,
            encoding: u32,
            is_external_representation: u8,
        ) -> CFStringRef;
        fn CFStringGetCString(
            string: CFStringRef,
            buffer: *mut std::ffi::c_char,
            size: isize,
            encoding: u32,
        ) -> u8;
        fn CFRelease(cf: CFTypeRef);
        fn LSSetDefaultHandlerForURLScheme(scheme: CFStringRef, bundle_id: CFStringRef) -> i32;
        fn LSCopyDefaultHandlerForURLScheme(scheme: CFStringRef) -> CFStringRef;
    }

    /// A CFString that releases itself, so an early return cannot leak one.
    struct CfString(CFStringRef);

    impl CfString {
        fn new(value: &str) -> Option<Self> {
            // SAFETY: the pointer and length describe `value`'s own bytes,
            // which outlive the call; CF copies them.
            let raw = unsafe {
                CFStringCreateWithBytes(
                    std::ptr::null(),
                    value.as_ptr(),
                    value.len() as isize,
                    UTF8,
                    0,
                )
            };
            (!raw.is_null()).then_some(Self(raw))
        }

        fn to_rust(&self) -> Option<String> {
            let mut buffer = [0 as std::ffi::c_char; 512];
            // SAFETY: `self.0` is a live CFString and the buffer's length is
            // passed alongside it, so CF cannot write past the end.
            let ok = unsafe {
                CFStringGetCString(self.0, buffer.as_mut_ptr(), buffer.len() as isize, UTF8)
            };
            if ok == 0 {
                return None;
            }
            // SAFETY: CF wrote a NUL-terminated string into the buffer above.
            let text = unsafe { std::ffi::CStr::from_ptr(buffer.as_ptr()) };
            text.to_str().ok().map(str::to_string)
        }
    }

    impl Drop for CfString {
        fn drop(&mut self) {
            // SAFETY: created by CFStringCreateWithBytes or returned by a
            // Copy function, so this reference is ours to release.
            unsafe { CFRelease(self.0) };
        }
    }

    fn set_default_scheme_handler(scheme: &str, bundle_id: &str) -> Result<()> {
        let (Some(scheme), Some(bundle_id)) = (CfString::new(scheme), CfString::new(bundle_id))
        else {
            return Err(Error::other("could not build the CFStrings for Launch Services"));
        };
        // SAFETY: both arguments are live CFStrings for the duration of the
        // call; the function returns an OSStatus and takes no ownership.
        let status = unsafe { LSSetDefaultHandlerForURLScheme(scheme.0, bundle_id.0) };
        match status {
            0 => Ok(()),
            code => {
                Err(Error::other(format!("Launch Services refused the scheme (OSStatus {code})")))
            }
        }
    }

    fn default_scheme_handler(scheme: &str) -> Option<String> {
        let scheme = CfString::new(scheme)?;
        // SAFETY: `scheme` is live for the call. The result is a +1 reference,
        // which `CfString`'s Drop releases.
        let raw = unsafe { LSCopyDefaultHandlerForURLScheme(scheme.0) };
        if raw.is_null() {
            return None;
        }
        CfString(raw).to_rust()
    }
}

#[cfg(target_os = "linux")]
mod linux_handler {
    use std::io::{Error, Result};

    const DESKTOP_FILE: &str = "braid.desktop";
    const MIME_TYPES: [&str; 2] = ["x-scheme-handler/magnet", "application/x-bittorrent"];

    pub fn set(enabled: bool) -> Result<bool> {
        let dir = applications_dir()?;
        let path = dir.join(DESKTOP_FILE);
        if !enabled {
            let _ = std::fs::remove_file(&path);
            // Removing the file is not enough on its own: the association
            // stays in mimeapps.list until something else claims it. Ask
            // xdg-mime to forget us, and report what it left behind.
            update_database(&dir);
            return Ok(is_us());
        }

        let executable = std::env::current_exe()?;
        std::fs::create_dir_all(&dir)?;
        std::fs::write(&path, desktop_entry(&executable))?;
        update_database(&dir);

        for mime in MIME_TYPES {
            let status = std::process::Command::new("xdg-mime")
                .args(["default", DESKTOP_FILE, mime])
                .status();
            match status {
                Ok(status) if status.success() => {}
                Ok(status) => {
                    return Err(Error::other(format!("xdg-mime refused {mime} ({status})")));
                }
                Err(error) => {
                    return Err(Error::other(format!("xdg-mime is not installed: {error}")));
                }
            }
        }
        Ok(is_us())
    }

    pub fn is_us() -> bool {
        let Ok(output) = std::process::Command::new("xdg-mime")
            .args(["query", "default", MIME_TYPES[0]])
            .output()
        else {
            return false;
        };
        String::from_utf8_lossy(&output.stdout).trim() == DESKTOP_FILE
    }

    fn applications_dir() -> Result<std::path::PathBuf> {
        if let Some(data) = std::env::var_os("XDG_DATA_HOME") {
            return Ok(std::path::PathBuf::from(data).join("applications"));
        }
        let Some(home) = std::env::var_os("HOME") else {
            return Err(Error::other("no HOME to write a .desktop file into"));
        };
        Ok(std::path::PathBuf::from(home).join(".local/share/applications"))
    }

    /// Best-effort: a missing `update-desktop-database` is normal on minimal
    /// systems, and `xdg-mime` still works without it.
    fn update_database(dir: &std::path::Path) {
        let _ = std::process::Command::new("update-desktop-database").arg(dir).status();
    }

    pub(super) fn desktop_entry(executable: &std::path::Path) -> String {
        format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=Braid\n\
             Comment=Parallel multi-interface download manager\n\
             Exec={} %u\n\
             Terminal=false\n\
             Categories=Network;FileTransfer;\n\
             MimeType={};\n\
             StartupNotify=true\n",
            executable.display(),
            MIME_TYPES.join(";")
        )
    }
}

#[cfg(windows)]
mod windows_handler {
    //! **Unverified.** Written from Microsoft's documentation for
    //! application-registered URL protocols; no Windows machine was available
    //! to test it on, exactly as with the multi-NIC binding in `dl-net`.
    //! Because [`super::set_magnet_handler`] re-reads the registry and returns
    //! what it finds, a version of Windows that refuses these writes turns the
    //! switch back off rather than claiming a handler that does not exist.

    use std::io::{Error, Result};

    const KEY: &str = r"HKCU\Software\Classes\magnet";

    pub fn set(enabled: bool) -> Result<bool> {
        if !enabled {
            let _ = reg(&["delete", KEY, "/f"]);
            return Ok(is_us());
        }
        let command = format!("\"{}\" \"%1\"", std::env::current_exe()?.display());
        reg(&["add", KEY, "/ve", "/t", "REG_SZ", "/d", "URL:magnet", "/f"])?;
        reg(&["add", KEY, "/v", "URL Protocol", "/t", "REG_SZ", "/d", "", "/f"])?;
        reg(&[
            "add",
            &format!(r"{KEY}\shell\open\command"),
            "/ve",
            "/t",
            "REG_SZ",
            "/d",
            &command,
            "/f",
        ])?;
        Ok(is_us())
    }

    pub fn is_us() -> bool {
        let Ok(exe) = std::env::current_exe() else { return false };
        let Ok(output) = std::process::Command::new("reg")
            .args(["query", &format!(r"{KEY}\shell\open\command"), "/ve"])
            .output()
        else {
            return false;
        };
        String::from_utf8_lossy(&output.stdout).contains(&exe.display().to_string())
    }

    fn reg(args: &[&str]) -> Result<()> {
        let status = std::process::Command::new("reg").args(args).status()?;
        match status.success() {
            true => Ok(()),
            false => Err(Error::other(format!("reg.exe refused the write ({status})"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Off still installs the backend: the seamless title bar is not optional
    ///: but must never claim blur it did not ask for.
    #[test]
    fn off_installs_the_shell_without_blur() {
        assert_eq!(plan(Vibrancy::Off, false), Some(false));
        assert_eq!(plan(Vibrancy::Requested, false), Some(true));
    }

    /// Overriding `SLINT_BACKEND=headless` would break every headless capture,
    /// and the failure would look like a rendering bug rather than this.
    #[test]
    fn an_explicit_backend_is_left_alone() {
        assert_eq!(plan(Vibrancy::Requested, true), None);
        assert_eq!(plan(Vibrancy::Off, true), None);
    }

    /// The UI must key off what was granted, not what was asked for.
    #[test]
    fn capabilities_default_to_nothing_granted() {
        assert!(!ShellCapabilities::default().vibrancy);
    }

    /// The plist label and the bundle id have to be the same string, or
    /// Launch Services and `launchctl` disagree about which app this is.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_login_item_is_labelled_with_the_bundle_id() {
        let plist = login_plist(std::path::Path::new("/Applications/Braid.app"));
        assert!(plist.contains(BUNDLE_ID), "{plist}");
    }

    /// Run out of `target/release` there is no bundle, and macOS has nothing
    /// to bind a scheme to. Saying so is the difference between a switch that
    /// reverts with a reason and one that lies.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_bare_executable_is_not_a_bundle() {
        assert!(
            macos_handler::bundle_path().is_none()
                || std::env::current_exe()
                    .is_ok_and(|e| e.to_string_lossy().contains(".app/Contents/MacOS/")),
            "bundle detection matched something that is not an .app"
        );
    }

    /// The desktop entry has to name both associations and take a URL
    /// argument, or the link is handed over and then dropped on the floor.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_desktop_entry_claims_both_magnet_links_and_torrent_files() {
        let entry = linux_handler::desktop_entry(std::path::Path::new("/usr/bin/braid"));
        assert!(entry.contains("x-scheme-handler/magnet"), "{entry}");
        assert!(entry.contains("application/x-bittorrent"), "{entry}");
        assert!(entry.contains("%u"), "the Exec line must pass the URL through: {entry}");
    }
}
