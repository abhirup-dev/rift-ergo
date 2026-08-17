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
//! Prefer not to use it, though. `CGSManagedDisplaySetCurrentSpace` performs a
//! raw current-space swap with no leave-fullscreen teardown, so the old window's
//! layers stay composited -- the destination's windows draw inside a frame the
//! fullscreen window still owns. Activating afterwards does not repair it: by
//! then the display already reads as switched, so macOS sees no Space change to
//! perform.
//!
//! Focusing a window that lives on the destination avoids all of that. macOS
//! decides for itself that a Space change is required and runs its real,
//! animated, fully composited transition. That is the whole of AeroSpace's
//! approach -- it drives no Spaces at all, and its entire private surface is
//! `_AXUIElementGetWindow` -- which is why it needs no SIP changes and does not
//! suffer this. So: activate first, and fall back to the direct space call only
//! when the destination has no window to focus.
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

/// Finder owns no windows, so making it frontmost releases the fullscreen
/// application without pulling in a space of its own.
const FOCUS_PARK_BUNDLE_ID: &str = "com.apple.finder";

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

    // Pick the Desktop by space type rather than trying candidates in turn.
    // Switching to another fullscreen space never recovers the display, and
    // doing so churns macOS's fullscreen space ids. Stale ids report as
    // non-desktop, so they are filtered out here too.
    let Some(desktop) = display
        .inactive_space_ids
        .iter()
        .copied()
        .find(|space| skylight::is_desktop(*space))
    else {
        return Ok(displays);
    };

    // Prefer letting macOS run the transition itself. Focusing a window that
    // lives on the destination makes the WindowServer decide a Space change is
    // needed, and it then performs the real, fully composited leave-fullscreen
    // animation. This is all AeroSpace does, and why it needs no CGS calls.
    if let Some(occupant) = occupant_bundle_id(rift, desktop)? {
        skylight::activate(&occupant);
        // The transition is asynchronous; without settling, Rift still reports
        // the old space and the caller reads a working switch as a failure.
        if let Some(refreshed) = await_managed_space(rift, display_uuid)? {
            return Ok(refreshed);
        }
    }

    // Nothing on the destination to focus, so there is no window whose raise
    // would imply a Space change. Switch it directly instead. This flips the
    // current space without the leave-fullscreen teardown, so the old window's
    // layers can stay composited until something forces a redraw -- acceptable
    // only because the alternative is not switching at all.
    if !skylight::show_space(display_uuid, desktop) {
        return Ok(displays);
    }
    let Some(refreshed) = await_managed_space(rift, display_uuid)? else {
        return Ok(displays);
    };
    skylight::activate(FOCUS_PARK_BUNDLE_ID);
    Ok(refreshed)
}

/// An application with a window on `space`, preferring the active workspace so
/// focus lands where the caller wants it. Rift's own focus commands are no use
/// here; they cannot cross out of a space Rift is not tracking.
fn occupant_bundle_id(rift: &Rift, space: u64) -> Result<Option<String>> {
    let workspaces = rift.workspaces(space)?;
    let preferred = workspaces
        .iter()
        .find(|workspace| workspace.is_active)
        .into_iter()
        .chain(workspaces.iter());
    Ok(preferred
        .flat_map(|workspace| workspace.windows.iter())
        .find_map(|window| window.bundle_id.clone()))
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
/// enabled, so `prepare_target` waits for a focus that never arrives.
///
/// Activating an application on the display is enough to make it active, and is
/// preferred for the same reason as in `ensure_managed_space`. Fall back to the
/// direct space call only when the display has nothing to focus.
///
/// Best-effort, for the same reason as above.
pub fn focus_display(rift: &Rift, display_uuid: &str, space: u64) -> Result<()> {
    if rift.display_is_active(display_uuid)? {
        return Ok(());
    }
    match occupant_bundle_id(rift, space)? {
        Some(occupant) => skylight::activate(&occupant),
        None => {
            skylight::show_space(display_uuid, space);
            skylight::activate(FOCUS_PARK_BUNDLE_ID);
        }
    }
    Ok(())
}

/// Minimal binding to the private SkyLight space-switching call.
mod skylight {
    use std::ffi::{CString, c_char, c_int, c_void};
    use std::process::Command;
    use std::ptr;
    use std::sync::OnceLock;

    /// `CGSSpaceGetType` reports 0 for an ordinary Desktop. Fullscreen spaces
    /// report 3, as do ids that no longer exist, so this doubles as a staleness
    /// filter.
    const SPACE_TYPE_DESKTOP: c_int = 0;

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
    type SpaceGetType = unsafe extern "C" fn(c_int, u64) -> c_int;
    type StringCreate = unsafe extern "C" fn(*const c_void, *const c_char, u32) -> *const c_void;
    type Release = unsafe extern "C" fn(*const c_void);

    struct Api {
        connection: MainConnectionId,
        set_current_space: SetCurrentSpace,
        space_get_type: SpaceGetType,
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

    /// Whether `space` is an ordinary Desktop rather than a fullscreen space.
    pub fn is_desktop(space: u64) -> bool {
        let Some(api) = api() else {
            return false;
        };
        // Safety: resolved from SkyLight above; takes a connection and a space id.
        unsafe { (api.space_get_type)((api.connection)(), space) == SPACE_TYPE_DESKTOP }
    }

    /// Make an application frontmost, by bundle id so no dependency on process
    /// names (Teams is "MSTeams", Sublime Text is "sublime_text"). Best-effort;
    /// failure just means the display may not complete its transition, which is
    /// the behaviour without this module at all.
    pub fn activate(bundle_id: &str) {
        let _ = Command::new("/usr/bin/osascript")
            .arg("-e")
            .arg(format!("tell application id \"{bundle_id}\" to activate"))
            .output();
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
                space_get_type: std::mem::transmute::<*mut c_void, SpaceGetType>(symbol(
                    skylight,
                    "CGSSpaceGetType",
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
