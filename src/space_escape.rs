//! Recovery from macOS spaces that Rift does not manage.
//!
//! A window put into native macOS fullscreen gets its own macOS space. Rift does
//! not manage those, so the display reports no active space, `display_space`
//! yields nothing, and every display-targeted command fails before it starts:
//!
//! ```text
//! rift-ergo: target display has no active macOS space
//! ```
//!
//! Rift cannot fix this itself. `display focus`, `window focus` and
//! `space switch` all report success and change nothing, because Rift's active
//! context never follows onto a space it is not tracking. Activating an
//! application only helps when a window already sits on one of the display's
//! managed spaces, which is not true of a monitor whose sole window is
//! fullscreen.
//!
//! macOS does expose the operation, through the private SkyLight call
//! `CGSManagedDisplaySetCurrentSpace` -- what Hammerspoon's `hs.spaces.gotoSpace`
//! wraps. It targets a display directly, so it needs no anchor window, works on
//! an unfocused display, and leaves the fullscreen space intact rather than
//! destroying it. There is no AppleScript equivalent; macOS exposes no scripting
//! API for Spaces.
//!
//! Bound with `dlopen`/`dlsym` rather than linked, so a missing symbol on a
//! future macOS degrades to "escape unavailable" instead of failing to launch.
//!
//! This module is deliberately standalone. To remove it: delete this file, drop
//! `mod space_escape;` from `main.rs`, and delete the calls in
//! `workflow/move_follow.rs` and `workflow/switch_workspace.rs`.

use std::thread;
use std::time::{Duration, Instant};

use rift_client::DisplayData;

use crate::Result;
use crate::rift::{Rift, display_space};

const SETTLE_TIMEOUT: Duration = Duration::from_millis(1_500);
const PROBE_INTERVAL: Duration = Duration::from_millis(50);

/// Ensure `display_uuid` is showing a space Rift manages, returning refreshed
/// display data. A display that already has one is returned untouched, so this
/// is a no-op on every ordinary keypress.
///
/// Switching a display's space also makes it the active one. Callers that care
/// about the focused window must capture it beforehand.
///
/// Best-effort: if no candidate space recovers the display, the original data is
/// returned and the caller fails exactly as it did before this module existed.
pub fn ensure_managed_space(
    rift: &Rift,
    display_uuid: &str,
    displays: Vec<DisplayData>,
) -> Result<Vec<DisplayData>> {
    let Some(display) = displays.iter().find(|display| display.uuid == display_uuid) else {
        return Ok(displays);
    };
    if display_space(display).is_some() {
        return Ok(displays);
    }

    // Rift reports workspaces for fullscreen spaces too, so a managed space
    // cannot be identified up front -- show each candidate and let
    // `display_space` adjudicate. Bounded by this display's own space count.
    for space in display.inactive_space_ids.clone() {
        if !skylight::show_space(display_uuid, space) {
            break;
        }
        // The switch is asynchronous. Without settling, Rift still reports the
        // old space, this reads it as a failure, and the next candidate undoes
        // the switch that just worked.
        if let Some(displays) = await_managed_space(rift, display_uuid)? {
            return Ok(displays);
        }
    }
    Ok(displays)
}

fn await_managed_space(rift: &Rift, display_uuid: &str) -> Result<Option<Vec<DisplayData>>> {
    let deadline = Instant::now() + SETTLE_TIMEOUT;
    while Instant::now() < deadline {
        thread::sleep(PROBE_INTERVAL);
        let displays = rift.displays()?;
        let recovered = displays
            .iter()
            .find(|display| display.uuid == display_uuid)
            .is_some_and(|display| display_space(display).is_some());
        if recovered {
            return Ok(Some(displays));
        }
    }
    Ok(None)
}

/// Make `display_uuid` the active display.
///
/// `prepare_target` focuses a destination by focusing a window in that display's
/// active workspace. When that workspace is empty there is no anchor, and the
/// `MoveMouseToDisplay` fallback is inert unless `focus_follows_mouse` is
/// enabled. Re-showing the space the display is already on activates it without
/// needing a window.
///
/// Best-effort, for the same reason as above.
pub fn focus_display(rift: &Rift, display_uuid: &str, space: u64) -> Result<()> {
    if rift.display_is_active(display_uuid)? {
        return Ok(());
    }
    skylight::show_space(display_uuid, space);
    Ok(())
}

/// Minimal binding to the private SkyLight space-switching call.
mod skylight {
    use std::ffi::{CString, c_char, c_int, c_void};
    use std::ptr;
    use std::sync::OnceLock;

    const RTLD_LAZY: c_int = 1;
    const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
    const SKYLIGHT: &str = "/System/Library/PrivateFrameworks/SkyLight.framework/SkyLight";
    const CORE_FOUNDATION: &str =
        "/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation";

    unsafe extern "C" {
        fn dlopen(path: *const c_char, mode: c_int) -> *mut c_void;
        fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    }

    type MainConnectionId = unsafe extern "C" fn() -> c_int;
    type SetCurrentSpace = unsafe extern "C" fn(c_int, *const c_void, u64);
    type StringCreate = unsafe extern "C" fn(*const c_void, *const c_char, u32) -> *const c_void;
    type Release = unsafe extern "C" fn(*const c_void);

    struct Api {
        connection: MainConnectionId,
        set_current_space: SetCurrentSpace,
        string_create: StringCreate,
        release: Release,
    }

    // Safety: these are immutable C function pointers into always-loaded
    // system frameworks.
    unsafe impl Send for Api {}
    unsafe impl Sync for Api {}

    /// Show `space` on `display_uuid`. Returns false when the private API is
    /// unavailable, so callers can fall back rather than assume success.
    pub fn show_space(display_uuid: &str, space: u64) -> bool {
        let Some(api) = api() else {
            return false;
        };
        let Ok(uuid) = CString::new(display_uuid) else {
            return false;
        };
        // Safety: the symbols were resolved from the system frameworks above,
        // and the CFString is released before returning.
        unsafe {
            let cf_uuid =
                (api.string_create)(ptr::null(), uuid.as_ptr(), K_CF_STRING_ENCODING_UTF8);
            if cf_uuid.is_null() {
                return false;
            }
            (api.set_current_space)((api.connection)(), cf_uuid, space);
            (api.release)(cf_uuid);
        }
        true
    }

    fn api() -> Option<&'static Api> {
        static API: OnceLock<Option<Api>> = OnceLock::new();
        API.get_or_init(load).as_ref()
    }

    fn load() -> Option<Api> {
        // Safety: both paths are system frameworks; every symbol is checked for
        // null before being transmuted to its documented signature.
        unsafe {
            let skylight = open(SKYLIGHT)?;
            let core_foundation = open(CORE_FOUNDATION)?;
            Some(Api {
                connection: std::mem::transmute::<*mut c_void, MainConnectionId>(symbol(
                    skylight,
                    "CGSMainConnectionID",
                )?),
                set_current_space: std::mem::transmute::<*mut c_void, SetCurrentSpace>(symbol(
                    skylight,
                    "CGSManagedDisplaySetCurrentSpace",
                )?),
                string_create: std::mem::transmute::<*mut c_void, StringCreate>(symbol(
                    core_foundation,
                    "CFStringCreateWithCString",
                )?),
                release: std::mem::transmute::<*mut c_void, Release>(symbol(
                    core_foundation,
                    "CFRelease",
                )?),
            })
        }
    }

    unsafe fn open(path: &str) -> Option<*mut c_void> {
        let path = CString::new(path).ok()?;
        let handle = unsafe { dlopen(path.as_ptr(), RTLD_LAZY) };
        (!handle.is_null()).then_some(handle)
    }

    unsafe fn symbol(handle: *mut c_void, name: &str) -> Option<*mut c_void> {
        let name = CString::new(name).ok()?;
        let symbol = unsafe { dlsym(handle, name.as_ptr()) };
        (!symbol.is_null()).then_some(symbol)
    }
}
