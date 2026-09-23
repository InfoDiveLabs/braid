//! Links that arrive from the system rather than from the add sheet.
//!
//! Three ways in, and they are not interchangeable:
//!
//! * **argv**: every platform, when the launcher passes the link as an
//!   argument. This is how Linux `.desktop` `Exec=… %u` and Windows
//!   `shell\open\command "%1"` deliver one.
//! * **Apple Events**: macOS. A bundled app is *not* relaunched with the URL
//!   on its command line; Launch Services sends the running process a
//!   `kInternetEventClass`/`kAEGetURL` event instead, and a `kAEOpenDocuments`
//!   event for a `.torrent`. An app that only reads argv silently does
//!   nothing when someone clicks a magnet link.
//! * **the single-instance socket**: a second launch that found a first one.
//!
//! All three end at [`Sink`], so the rest of the app has one entry point and
//! no opinion about which door a link came through.

use std::sync::{Mutex, OnceLock};

/// Where an opened link goes.
///
/// A global because the macOS Apple Event handler is a bare `extern "C"`
/// function with nowhere to put a closure: the OS calls it with a `refcon`
/// pointer, and smuggling a boxed callback through that is a lifetime problem
/// with no upside over one process-wide sink.
static SINK: OnceLock<Box<dyn Fn(String) + Send + Sync>> = OnceLock::new();

/// Links that arrived before the window existed.
///
/// macOS sends the `kAEGetURL` for a cold launch almost immediately, often
/// before the event loop is running, and a link dropped there is a click that
/// did nothing.
static PENDING: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Install the handler for links from every source, and drain anything that
/// arrived while the app was still starting.
pub fn install(sink: impl Fn(String) + Send + Sync + 'static) {
    if SINK.set(Box::new(sink)).is_err() {
        tracing::warn!("the link sink was already installed");
        return;
    }
    let queued: Vec<String> =
        std::mem::take(&mut PENDING.lock().unwrap_or_else(|e| e.into_inner()));
    for link in queued {
        deliver(link);
    }
}

/// Hand one link to the app, queueing it if the sink is not up yet.
pub fn deliver(link: String) {
    let link = link.trim().to_string();
    if link.is_empty() {
        return;
    }
    match SINK.get() {
        Some(sink) => sink(link),
        None => PENDING.lock().unwrap_or_else(|e| e.into_inner()).push(link),
    }
}

/// Start listening for the platform's own way of delivering a link.
///
/// A no-op everywhere but macOS, where argv is not the delivery mechanism.
/// Called before the window exists so a cold launch's event is not missed;
/// see [`reclaim_system_events`] for why that is not the end of it.
pub fn listen_for_system_events() {
    #[cfg(target_os = "macos")]
    apple_events::install();
}

/// Install the handlers again, once the event loop is up.
///
/// `NSApplication` registers its own `kAEOpenDocuments` handler when it is
/// created, which is after [`listen_for_system_events`] runs: it silently
/// replaced ours, and opening a `.torrent` from Finder did nothing while a
/// magnet link, which `NSApplication` does not claim, worked. Installing a
/// second time after the loop exists takes it back. Harmless to repeat: the
/// Apple Event manager replaces the entry rather than stacking handlers.
pub fn reclaim_system_events() {
    #[cfg(target_os = "macos")]
    {
        // The timer has to outlive this function or it is cancelled on drop
        // and the callback never runs, so it lives with the event loop's own
        // thread rather than on the stack.
        thread_local! {
            static RECLAIM: slint::Timer = slint::Timer::default();
        }
        // Late enough that `NSApplication` exists, early enough that a
        // document opened at launch is still on its way in.
        RECLAIM.with(|timer| {
            timer.start(
                slint::TimerMode::SingleShot,
                std::time::Duration::from_millis(400),
                apple_events::install,
            );
        });
    }
}

#[cfg(target_os = "macos")]
mod apple_events {
    //! The Carbon Apple Event API, which is still how a macOS app is told to
    //! open a URL. `NSApplication` dispatches to handlers installed here, so
    //! no Objective-C runtime is needed: four C symbols and a callback.

    use std::ffi::c_void;

    type OSErr = i16;
    type OSStatus = i32;
    type AEEventClass = u32;
    type AEEventID = u32;
    type AEKeyword = u32;
    type DescType = u32;

    /// `kInternetEventClass` / `kAEGetURL`: "open this URL".
    const INTERNET_EVENT_CLASS: AEEventClass = four_cc(b"GURL");
    const GET_URL: AEEventID = four_cc(b"GURL");
    /// `kCoreEventClass` / `kAEOpenDocuments`: "open these files".
    const CORE_EVENT_CLASS: AEEventClass = four_cc(b"aevt");
    const OPEN_DOCUMENTS: AEEventID = four_cc(b"odoc");

    const KEY_DIRECT_OBJECT: AEKeyword = four_cc(b"----");
    const TYPE_UTF8_TEXT: DescType = four_cc(b"utf8");
    /// A `file://` URL, which [`dl_core::classify`] already understands, so
    /// the document path needs no separate decoding.
    const TYPE_FILE_URL: DescType = four_cc(b"furl");
    const TYPE_AE_LIST: DescType = four_cc(b"list");

    /// Apple's four-character codes are big-endian packed ASCII.
    const fn four_cc(code: &[u8; 4]) -> u32 {
        u32::from_be_bytes(*code)
    }

    #[repr(C)]
    struct AEDesc {
        descriptor_type: DescType,
        data_handle: *mut c_void,
    }

    type EventHandler =
        extern "C" fn(event: *const AEDesc, reply: *mut AEDesc, refcon: isize) -> OSErr;

    unsafe extern "C" {
        fn AEInstallEventHandler(
            event_class: AEEventClass,
            event_id: AEEventID,
            handler: EventHandler,
            refcon: isize,
            is_sys_handler: u8,
        ) -> OSErr;
        fn AEGetParamPtr(
            event: *const AEDesc,
            keyword: AEKeyword,
            desired_type: DescType,
            actual_type: *mut DescType,
            data: *mut c_void,
            maximum_size: isize,
            actual_size: *mut isize,
        ) -> OSStatus;
        fn AEGetParamDesc(
            event: *const AEDesc,
            keyword: AEKeyword,
            desired_type: DescType,
            result: *mut AEDesc,
        ) -> OSStatus;
        fn AECountItems(list: *const AEDesc, count: *mut isize) -> OSStatus;
        fn AEGetNthPtr(
            list: *const AEDesc,
            index: isize,
            desired_type: DescType,
            keyword: *mut AEKeyword,
            actual_type: *mut DescType,
            data: *mut c_void,
            maximum_size: isize,
            actual_size: *mut isize,
        ) -> OSStatus;
        fn AEDisposeDesc(desc: *mut AEDesc) -> OSStatus;
    }

    /// Long enough for any URL a link handler will see; a longer one is
    /// truncated rather than allowed to write past the buffer.
    const MAX_URL: usize = 4096;

    pub fn install() {
        // SAFETY: both handlers have the signature the API declares and live
        // for the whole process, which is what `isSysHandler = 0` requires.
        let magnet =
            unsafe { AEInstallEventHandler(INTERNET_EVENT_CLASS, GET_URL, on_get_url, 0, 0) };
        let documents = unsafe {
            AEInstallEventHandler(CORE_EVENT_CLASS, OPEN_DOCUMENTS, on_open_documents, 0, 0)
        };
        if magnet != 0 || documents != 0 {
            tracing::warn!(
                magnet,
                documents,
                "could not install the Apple Event handlers; magnet links will not open"
            );
        }
    }

    extern "C" fn on_get_url(event: *const AEDesc, _reply: *mut AEDesc, _refcon: isize) -> OSErr {
        if let Some(url) = direct_text(event) {
            super::deliver(url);
        }
        0
    }

    extern "C" fn on_open_documents(
        event: *const AEDesc,
        _reply: *mut AEDesc,
        _refcon: isize,
    ) -> OSErr {
        for url in document_urls(event) {
            super::deliver(url);
        }
        0
    }

    /// The event's direct object, as UTF-8 text.
    fn direct_text(event: *const AEDesc) -> Option<String> {
        let mut buffer = vec![0u8; MAX_URL];
        let mut actual_type: DescType = 0;
        let mut actual_size: isize = 0;
        // SAFETY: `event` is the descriptor the OS handed the handler, and the
        // buffer's length is passed so the call cannot overrun it.
        let status = unsafe {
            AEGetParamPtr(
                event,
                KEY_DIRECT_OBJECT,
                TYPE_UTF8_TEXT,
                &mut actual_type,
                buffer.as_mut_ptr().cast(),
                buffer.len() as isize,
                &mut actual_size,
            )
        };
        if status != 0 || actual_size <= 0 {
            return None;
        }
        buffer.truncate(actual_size.min(MAX_URL as isize) as usize);
        String::from_utf8(buffer).ok()
    }

    /// Every file in an `odoc` event, as a `file://` URL.
    fn document_urls(event: *const AEDesc) -> Vec<String> {
        let mut list = AEDesc { descriptor_type: 0, data_handle: std::ptr::null_mut() };
        // SAFETY: `event` is live for the handler call; `list` is an
        // out-parameter this function owns and disposes below.
        let status = unsafe { AEGetParamDesc(event, KEY_DIRECT_OBJECT, TYPE_AE_LIST, &mut list) };
        if status != 0 {
            return Vec::new();
        }

        let mut count: isize = 0;
        // SAFETY: `list` is a valid descriptor until it is disposed.
        if unsafe { AECountItems(&list, &mut count) } != 0 {
            // SAFETY: disposing the descriptor obtained above, exactly once.
            unsafe { AEDisposeDesc(&mut list) };
            return Vec::new();
        }

        let mut out = Vec::new();
        for index in 1..=count {
            let mut buffer = vec![0u8; MAX_URL];
            let mut keyword: AEKeyword = 0;
            let mut actual_type: DescType = 0;
            let mut actual_size: isize = 0;
            // SAFETY: the index is within the count reported above, and the
            // buffer's length bounds the write.
            let status = unsafe {
                AEGetNthPtr(
                    &list,
                    index,
                    TYPE_FILE_URL,
                    &mut keyword,
                    &mut actual_type,
                    buffer.as_mut_ptr().cast(),
                    buffer.len() as isize,
                    &mut actual_size,
                )
            };
            if status != 0 || actual_size <= 0 {
                continue;
            }
            buffer.truncate(actual_size.min(MAX_URL as isize) as usize);
            if let Ok(url) = String::from_utf8(buffer) {
                out.push(url);
            }
        }
        // SAFETY: disposing the descriptor obtained above, exactly once.
        unsafe { AEDisposeDesc(&mut list) };
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Getting these wrong means the handler is installed for an event
        /// nothing sends, which looks exactly like the feature not working.
        #[test]
        fn the_four_character_codes_match_apples_constants() {
            assert_eq!(GET_URL, 0x4755_524c);
            assert_eq!(CORE_EVENT_CLASS, 0x6165_7674);
            assert_eq!(OPEN_DOCUMENTS, 0x6f64_6f63);
            assert_eq!(KEY_DIRECT_OBJECT, 0x2d2d_2d2d);
            assert_eq!(TYPE_UTF8_TEXT, 0x7574_6638);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A link that arrives before the window is built has to wait rather than
    /// be dropped: on macOS the cold-launch `kAEGetURL` regularly beats the
    /// event loop, and losing it makes clicking a magnet link do nothing.
    #[test]
    fn a_link_that_arrives_too_early_is_queued_rather_than_lost() {
        // `SINK` is process-wide, so this test drives `PENDING` directly
        // rather than installing a sink another test would then inherit.
        PENDING.lock().unwrap().clear();
        deliver("magnet:?xt=urn:btih:ab".into());
        deliver("  ".into());
        let queued = PENDING.lock().unwrap().clone();
        assert_eq!(queued, vec!["magnet:?xt=urn:btih:ab".to_string()]);
        PENDING.lock().unwrap().clear();
    }
}
