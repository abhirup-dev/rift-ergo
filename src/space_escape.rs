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

use rift_client::{DisplayData, Rect};

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
    preferred_workspace: Option<&str>,
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
    if let Some(occupant) = occupant_bundle_id(rift, desktop, preferred_workspace)? {
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

/// An application whose activation leaves `space` showing what the caller
/// wants: one with a window in the active workspace, or in
/// `preferred_workspace` when the caller is about to show that workspace
/// anyway. Rift's own focus commands are no use here; they cannot cross out of
/// a space Rift is not tracking, and they ignore windows in hidden workspaces.
///
/// Never an application from any other workspace. Activating one raises its
/// hidden window, and Rift answers by switching the display to that window's
/// workspace -- so a switch to `2` would land on `A`, and windows transferred a
/// moment later would arrive in `1`. Returning nothing and letting the caller
/// fall back to the direct space switch is the safer failure.
fn occupant_bundle_id(
    rift: &Rift,
    space: u64,
    preferred_workspace: Option<&str>,
) -> Result<Option<String>> {
    let workspaces = rift.workspaces(space)?;
    let candidates = workspaces
        .iter()
        .filter(|workspace| workspace.is_active)
        .chain(
            workspaces
                .iter()
                .filter(|workspace| Some(workspace.name.as_str()) == preferred_workspace),
        );
    Ok(candidates
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

/// Last resort for making a display active: click its desktop.
///
/// macOS makes the display that owns the key window active, and Rift learns
/// the active display from macOS. A display whose visible workspace is empty
/// offers no window to focus, and Rift's own commands cannot help: `FocusDisplay`
/// and `MoveMouseToDisplay` do nothing without a window there. A click on the
/// desktop does make the display active -- on the primary display. On a
/// secondary display macOS keeps the key window where it was, so this returns
/// and the caller's wait times out; callers handle that by moving a window
/// there first and focusing it.
///
/// Parking focus on Finder, the previous fallback, never changed the active
/// display and left Finder frontmost, which made Rift's next window raises
/// miss for a couple of seconds.
pub fn click_display(frame: Rect) -> bool {
    pointer::click_desktop(frame)
}

/// Synthetic pointer input, for the one thing no window-manager command can
/// do: make a display active when it shows nothing that can take focus.
mod pointer {
    use std::ffi::{CString, c_char, c_int, c_void};
    use std::sync::OnceLock;
    use std::thread;
    use std::time::Duration;

    use rift_client::Rect;

    const RTLD_LAZY: c_int = 1;
    const CORE_GRAPHICS: &str = "/System/Library/Frameworks/CoreGraphics.framework/CoreGraphics";
    const CORE_FOUNDATION: &str =
        "/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation";
    const LEFT_MOUSE_DOWN: u32 = 1;
    const LEFT_MOUSE_UP: u32 = 2;
    const LEFT_BUTTON: u32 = 0;
    const HID_EVENT_TAP: u32 = 0;
    /// Fraction of the display height at which to click. The centre of an
    /// empty workspace shows the desktop; the edges do not -- the menu bar
    /// sits at the top and an auto-hiding Dock pops up under a click at the
    /// bottom and swallows it, which is how a bottom-corner click failed.
    const CLICK_Y_FRACTION: f64 = 0.5;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CGPoint {
        x: f64,
        y: f64,
    }

    unsafe extern "C" {
        fn dlopen(path: *const c_char, mode: c_int) -> *mut c_void;
        fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    }

    type WarpCursor = unsafe extern "C" fn(CGPoint) -> c_int;
    type CreateMouseEvent = unsafe extern "C" fn(*const c_void, u32, CGPoint, u32) -> *const c_void;
    type PostEvent = unsafe extern "C" fn(u32, *const c_void);
    type Release = unsafe extern "C" fn(*const c_void);

    struct Api {
        warp: WarpCursor,
        create_mouse_event: CreateMouseEvent,
        post: PostEvent,
        release: Release,
    }

    // Safety: immutable C function pointers into always-loaded system frameworks.
    unsafe impl Send for Api {}
    unsafe impl Sync for Api {}

    /// Click the desktop at the centre of `frame`. Returns false when
    /// CoreGraphics could not be bound.
    ///
    /// Only meaningful on a display whose active workspace is empty, where
    /// that point shows the desktop; callers establish that first.
    pub fn click_desktop(frame: Rect) -> bool {
        let Some(api) = api() else {
            return false;
        };
        let point = CGPoint {
            x: frame.origin.x + frame.size.width / 2.0,
            y: frame.origin.y + frame.size.height * CLICK_Y_FRACTION,
        };
        // Safety: symbols resolved from system frameworks; events are released
        // after posting.
        unsafe {
            (api.warp)(point);
            thread::sleep(Duration::from_millis(20));
            for kind in [LEFT_MOUSE_DOWN, LEFT_MOUSE_UP] {
                let event = (api.create_mouse_event)(std::ptr::null(), kind, point, LEFT_BUTTON);
                if event.is_null() {
                    return false;
                }
                (api.post)(HID_EVENT_TAP, event);
                (api.release)(event);
                thread::sleep(Duration::from_millis(15));
            }
        }
        true
    }

    fn api() -> Option<&'static Api> {
        static API: OnceLock<Option<Api>> = OnceLock::new();
        API.get_or_init(load).as_ref()
    }

    fn load() -> Option<Api> {
        // Safety: system framework paths; each symbol is null-checked before
        // being transmuted to its documented signature.
        unsafe {
            let core_graphics = open(CORE_GRAPHICS)?;
            let core_foundation = open(CORE_FOUNDATION)?;
            Some(Api {
                warp: std::mem::transmute::<*mut c_void, WarpCursor>(symbol(
                    core_graphics,
                    "CGWarpMouseCursorPosition",
                )?),
                create_mouse_event: std::mem::transmute::<*mut c_void, CreateMouseEvent>(symbol(
                    core_graphics,
                    "CGEventCreateMouseEvent",
                )?),
                post: std::mem::transmute::<*mut c_void, PostEvent>(symbol(
                    core_graphics,
                    "CGEventPost",
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

/// Minimal binding to the private SkyLight space calls.
pub(crate) mod skylight {
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
    type CopySpacesForWindows = unsafe extern "C" fn(c_int, c_int, *const c_void) -> *const c_void;
    type StringCreate = unsafe extern "C" fn(*const c_void, *const c_char, u32) -> *const c_void;
    type NumberCreate = unsafe extern "C" fn(*const c_void, c_int, *const c_void) -> *const c_void;
    type NumberGetValue = unsafe extern "C" fn(*const c_void, c_int, *mut c_void) -> bool;
    type ArrayCreate = unsafe extern "C" fn(
        *const c_void,
        *const *const c_void,
        isize,
        *const c_void,
    ) -> *const c_void;
    type ArrayGetCount = unsafe extern "C" fn(*const c_void) -> isize;
    type ArrayGetValueAtIndex = unsafe extern "C" fn(*const c_void, isize) -> *const c_void;
    type Release = unsafe extern "C" fn(*const c_void);

    /// `kCFNumberSInt32Type` and `kCFNumberSInt64Type`.
    const CF_NUMBER_SINT32: c_int = 3;
    const CF_NUMBER_SINT64: c_int = 4;
    /// Every space kind: `CGSSpaceMask` all-spaces.
    const CGS_SPACE_MASK_ALL: c_int = 0x7;

    struct Api {
        connection: MainConnectionId,
        set_current_space: SetCurrentSpace,
        space_get_type: SpaceGetType,
        copy_spaces_for_windows: CopySpacesForWindows,
        string_create: StringCreate,
        number_create: NumberCreate,
        number_get_value: NumberGetValue,
        array_create: ArrayCreate,
        array_get_count: ArrayGetCount,
        array_get_value_at_index: ArrayGetValueAtIndex,
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

    /// The macOS spaces the WindowServer currently places window
    /// `window_server_id` on. `None` when the private API is unavailable.
    ///
    /// This is the WindowServer's own answer, the same one Rift consults when
    /// it reconciles its model against what is on screen, so it settles the
    /// question a frame heuristic cannot: whether a display transfer has
    /// physically happened yet.
    pub fn window_spaces(window_server_id: u32) -> Option<Vec<u64>> {
        let api = api()?;
        // Safety: symbols resolved from system frameworks; every CF object
        // created here is released before returning, and the array returned by
        // SkyLight is owned by us per the Copy rule.
        unsafe {
            let id = window_server_id;
            let number = (api.number_create)(
                ptr::null(),
                CF_NUMBER_SINT32,
                ptr::from_ref(&id).cast::<c_void>(),
            );
            if number.is_null() {
                return None;
            }
            let values = [number];
            let ids = (api.array_create)(ptr::null(), values.as_ptr(), 1, ptr::null());
            (api.release)(number);
            if ids.is_null() {
                return None;
            }
            let spaces = (api.copy_spaces_for_windows)((api.connection)(), CGS_SPACE_MASK_ALL, ids);
            (api.release)(ids);
            if spaces.is_null() {
                return None;
            }
            let count = (api.array_get_count)(spaces);
            let mut result = Vec::with_capacity(usize::try_from(count).unwrap_or(0));
            for index in 0..count {
                let value = (api.array_get_value_at_index)(spaces, index);
                let mut space: i64 = 0;
                if !value.is_null()
                    && (api.number_get_value)(
                        value,
                        CF_NUMBER_SINT64,
                        ptr::from_mut(&mut space).cast::<c_void>(),
                    )
                    && let Ok(space) = u64::try_from(space)
                {
                    result.push(space);
                }
            }
            (api.release)(spaces);
            Some(result)
        }
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
                copy_spaces_for_windows: std::mem::transmute::<*mut c_void, CopySpacesForWindows>(
                    symbol(skylight, "CGSCopySpacesForWindows")?,
                ),
                string_create: std::mem::transmute::<*mut c_void, StringCreate>(symbol(
                    core_foundation,
                    "CFStringCreateWithCString",
                )?),
                number_create: std::mem::transmute::<*mut c_void, NumberCreate>(symbol(
                    core_foundation,
                    "CFNumberCreate",
                )?),
                number_get_value: std::mem::transmute::<*mut c_void, NumberGetValue>(symbol(
                    core_foundation,
                    "CFNumberGetValue",
                )?),
                array_create: std::mem::transmute::<*mut c_void, ArrayCreate>(symbol(
                    core_foundation,
                    "CFArrayCreate",
                )?),
                array_get_count: std::mem::transmute::<*mut c_void, ArrayGetCount>(symbol(
                    core_foundation,
                    "CFArrayGetCount",
                )?),
                array_get_value_at_index: std::mem::transmute::<*mut c_void, ArrayGetValueAtIndex>(
                    symbol(core_foundation, "CFArrayGetValueAtIndex")?,
                ),
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
