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
//! Recovery is one shared Crossbeam transaction:
//! acquire native focus through a window already visible on the target Space,
//! post an ISS-style Dock gesture, then confirm the exact managed Desktop in
//! both WindowServer and Rift. Fullscreen windows never need to be in Rift's
//! model, and Space distance comes from WindowServer's complete native order.
//!
//! Focus and gesture phases are serialized because native display focus is
//! global. The action and its confirmation use `RiftTransaction::external_step`,
//! not an independent timer or polling loop.
//!
//! The previous Desktop-app activation and raw current-Space swap are retired:
//! the former animated the transition, and the latter left fullscreen layers
//! incorrectly composited. Missing APIs, unsafe focus, or an unconfirmed switch
//! now fail explicitly instead of silently taking either old path.
//!
//! Private symbols are bound with `dlopen`/`dlsym`; a missing symbol disables
//! recovery without preventing the helper from launching.

use std::time::Duration;

use rift_client::{DisplayData, Rect};

use crate::rift::{Rift, display_space};
use crate::transaction::{EventExpectation, RiftTransaction, is_timeout};
use crate::{Result, state_error};
use crate::{dock_swipe, native_focus};

const SETTLE_TIMEOUT: Duration = Duration::from_millis(1_500);
const NATIVE_FOCUS_TIMEOUT: Duration = Duration::from_millis(350);

/// Ensure `display_uuid` is showing a space Rift manages, returning refreshed
/// display data. A display that already has one is returned untouched, so this
/// is a no-op on every ordinary keypress.
///
/// Switching a display's space also makes it the active one. Callers that care
/// about the focused window must capture it beforehand.
///
/// Recovery fails closed when native focus or the selected Desktop cannot be
/// confirmed. It never silently substitutes an animated or raw Space swap.
pub fn ensure_managed_space(
    rift: &Rift,
    display_uuid: &str,
    displays: Vec<DisplayData>,
) -> Result<Vec<DisplayData>> {
    let Some(display) = displays.iter().find(|display| display.uuid == display_uuid) else {
        return Ok(displays);
    };
    let native = skylight::current_space(display_uuid);
    if managed_space_matches(native, display_space(display)) {
        return Ok(displays);
    }

    // Pick the Desktop by space type rather than trying candidates in turn.
    // Switching to another fullscreen space never recovers the display, and
    // doing so churns macOS's fullscreen space ids. Stale ids report as
    // non-desktop, so they are filtered out here too.
    let desktop = managed_desktop_candidate(display, skylight::is_desktop)
        .ok_or_else(|| state_error("target display has no known managed Desktop"))?;

    // Subscribed before acting, so no native focus/Space change can be missed.
    let transaction = RiftTransaction::new(rift)?;

    // Swiping makes the Dock run the real Space change, instantly. The
    // transition is still asynchronous; without settling, Rift still reports
    // the old space and the caller reads a working switch as a failure.
    if !acquire_native_display(&transaction, display)? {
        return Err(state_error(format!(
            "cannot acquire native focus on display {display_uuid}"
        )));
    }
    let plan = swipe_plan(display, desktop).ok_or_else(|| {
        state_error("native Space order or display focus changed before recovery")
    })?;
    recover_desktop_with(&transaction, display_uuid, desktop, || {
        // Focus can be stolen after planning. Never send a gesture to another
        // display, or apply a stale distance after the user changes Spaces.
        skylight::current_space(display_uuid) == Some(plan.from)
            && skylight::active_space() == Some(plan.from)
            && dock_swipe::swipe(plan.steps, display.frame)
    })?
    .ok_or_else(|| state_error("native Dock recovery unavailable or display focus changed"))
}

fn managed_space_matches(native: Option<u64>, managed: Option<u64>) -> bool {
    // A missing native API must not disable ordinary Rift commands. But when
    // WindowServer disagrees, Rift's cached active-Space flag is not evidence
    // that fullscreen recovery is unnecessary.
    managed.is_some_and(|managed| native.is_none_or(|native| native == managed))
}

fn managed_desktop_candidate(
    display: &DisplayData,
    mut is_desktop: impl FnMut(u64) -> bool,
) -> Option<u64> {
    display
        .active_space_ids
        .iter()
        .chain(&display.inactive_space_ids)
        .copied()
        .chain(display.space)
        .find(|space| is_desktop(*space))
}

/// Native focus also covers fullscreen Spaces that Rift omits. Use this when
/// capturing/restoring a command's origin; Rift's context can still name the
/// other display until fullscreen recovery has completed.
pub fn active_display_uuid(displays: &[DisplayData]) -> Option<String> {
    let active = skylight::active_space();
    displays
        .iter()
        .find(|display| {
            active.is_some_and(|space| skylight::current_space(&display.uuid) == Some(space))
        })
        .or_else(|| displays.iter().find(|display| display.is_active_context))
        .map(|display| display.uuid.clone())
}

/// Acquire the display through a window already visible on its current native
/// Space, rather than activating an application on the Desktop and animating
/// out of fullscreen. Rift's active-context flag can lag or omit fullscreen,
/// so use WindowServer for both the acquisition guard and its confirmation.
fn acquire_native_display(
    transaction: &RiftTransaction<'_>,
    display: &DisplayData,
) -> Result<bool> {
    let Some(current) = skylight::current_space(&display.uuid) else {
        return Ok(false);
    };
    if skylight::active_space() == Some(current) {
        return Ok(true);
    }
    let belongs_here = |window_id| {
        skylight::window_spaces(window_id).is_some_and(|spaces| spaces.contains(&current))
    };
    let candidates = native_focus::visible_windows(display.frame);
    for window in candidates
        .into_iter()
        .filter(|window| belongs_here(window.id))
        .take(3)
    {
        match transaction.external_step(
            "native fullscreen display focus",
            EventExpectation::Display(&display.uuid),
            || {
                skylight::current_space(&display.uuid) == Some(current)
                    && belongs_here(window.id)
                    && native_focus::focus(window)
            },
            |_| {
                Ok(skylight::current_space(&display.uuid) == Some(current)
                    && skylight::active_space() == Some(current))
            },
            NATIVE_FOCUS_TIMEOUT,
        ) {
            Ok(true) => return Ok(true),
            Ok(false) => {}
            Err(error) if is_timeout(&error) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}

/// How many spaces, and which way, to swipe `display` from the space it shows
/// to `desktop`. Native fullscreen spaces are absent from Rift's space lists,
/// so only WindowServer can supply the complete Mission Control order.
/// Refuse when either id is stale or this is not the active display: a swipe
/// would otherwise move the wrong display or nothing.
struct SwipePlan {
    from: u64,
    steps: isize,
}

fn swipe_plan(display: &DisplayData, desktop: u64) -> Option<SwipePlan> {
    let current = skylight::current_space(&display.uuid)?;
    let active = skylight::active_space()?;
    if active != current {
        return None;
    }
    let spaces = skylight::ordered_spaces(&display.uuid)?;
    let steps = swipe_steps_in_order(&spaces, current, active, desktop)?;
    Some(SwipePlan {
        from: current,
        steps,
    })
}

fn swipe_steps_in_order(spaces: &[u64], current: u64, active: u64, desktop: u64) -> Option<isize> {
    if current == 0 || active != current || desktop == 0 {
        return None;
    }
    // Ambiguous or malformed topology is safer to leave to the fallback.
    if spaces
        .iter()
        .enumerate()
        .any(|(index, space)| *space == 0 || spaces[..index].contains(space))
    {
        return None;
    }
    let position = |space| spaces.iter().position(|candidate| *candidate == space);
    let (from, to) = (position(current)?, position(desktop)?);
    Some(isize::try_from(to).ok()? - isize::try_from(from).ok()?)
}

#[cfg(test)]
mod tests {
    use super::{
        desktop_is_confirmed, managed_desktop_candidate, managed_space_matches,
        swipe_steps_in_order,
    };

    #[test]
    fn native_fullscreen_overrides_stale_rift_active_desktop() {
        assert!(!managed_space_matches(Some(409), Some(485)));
        assert!(!managed_space_matches(Some(485), None));
        assert!(managed_space_matches(Some(485), Some(485)));
        assert!(managed_space_matches(None, Some(485)));
    }

    #[test]
    fn desktop_candidate_survives_stale_rift_active_flag() {
        let display = serde_json::from_value(serde_json::json!({
            "active_space_ids": [485], "inactive_space_ids": [],
            "backing_scale": 2.0, "frame": {"origin": {"x": 0.0, "y": 0.0},
            "size": {"width": 1000.0, "height": 1000.0}},
            "is_active_context": true, "is_active_space": true,
            "is_visible_space": true, "name": "external", "screen_id": 3,
            "space": 485, "uuid": "external"
        }))
        .unwrap();
        assert_eq!(
            managed_desktop_candidate(&display, |space| space == 485),
            Some(485)
        );
    }

    #[test]
    fn recovery_requires_the_selected_desktop_in_both_views() {
        assert!(desktop_is_confirmed(485, Some(485), Some(485)));
        assert!(!desktop_is_confirmed(485, Some(409), Some(485)));
        assert!(!desktop_is_confirmed(485, Some(485), None));
        assert!(!desktop_is_confirmed(485, Some(1), Some(1)));
        assert!(!desktop_is_confirmed(485, None, Some(485)));
    }

    #[test]
    fn native_order_recovers_fullscreen_missing_from_rift() {
        // Captured during Option+A: Rift listed only Desktop 485 while
        // WindowServer reported fullscreen 409 as the current/active space.
        assert_eq!(swipe_steps_in_order(&[485], 409, 409, 485), None);
        assert_eq!(
            swipe_steps_in_order(&[485, 409, 295], 409, 409, 485),
            Some(-1)
        );
    }

    #[test]
    fn swipe_distance_preserves_native_order_and_fullscreen_gaps() {
        // Space ids are identities, not positions, and need not be sorted.
        let spaces = [485, 409, 70, 900];
        assert_eq!(swipe_steps_in_order(&spaces, 900, 900, 485), Some(-3));
        assert_eq!(swipe_steps_in_order(&spaces, 485, 485, 900), Some(3));
        assert_eq!(swipe_steps_in_order(&spaces, 485, 485, 485), Some(0));
    }

    #[test]
    fn swipe_refuses_inactive_display_or_stale_ids() {
        assert_eq!(swipe_steps_in_order(&[485, 409], 409, 1, 485), None);
        assert_eq!(swipe_steps_in_order(&[485], 409, 409, 485), None);
        assert_eq!(swipe_steps_in_order(&[409], 409, 409, 485), None);
        assert_eq!(swipe_steps_in_order(&[], 409, 409, 485), None);
    }

    #[test]
    #[ignore = "read-only native topology check; requires a running Rift on macOS"]
    fn native_order_contains_current_space_on_each_display() {
        let rift = crate::rift::Rift::connect().unwrap();
        for display in rift.displays().unwrap() {
            let spaces = super::skylight::ordered_spaces(&display.uuid).unwrap();
            let current = super::skylight::current_space(&display.uuid).unwrap();
            assert!(
                spaces.contains(&current),
                "{:?}: {current} missing from {spaces:?}",
                display.name
            );
            assert_eq!(
                swipe_steps_in_order(&spaces, current, current, current),
                Some(0)
            );
            println!(
                "{:?}: native={spaces:?} current={current} rift_inactive={:?}",
                display.name, display.inactive_space_ids
            );
        }
        assert!(super::skylight::ordered_spaces("rift-ergo-nonexistent-display").is_none());
    }

    #[test]
    #[ignore = "read-only visible-window check; requires native fullscreen on macOS"]
    fn native_fullscreen_has_focus_candidate_on_its_display() {
        let rift = crate::rift::Rift::connect().unwrap();
        for display in rift.displays().unwrap() {
            let current = super::skylight::current_space(&display.uuid).unwrap();
            if super::skylight::is_desktop(current) {
                continue;
            }
            let candidates: Vec<_> = crate::native_focus::visible_windows(display.frame)
                .into_iter()
                .filter(|window| {
                    super::skylight::window_spaces(window.id)
                        .is_some_and(|spaces| spaces.contains(&current))
                })
                .map(|window| window.id)
                .collect();
            assert!(
                !candidates.is_empty(),
                "no visible candidate on native Space {current}"
            );
            println!(
                "{:?}: fullscreen={current}, focus candidates={candidates:?}",
                display.name
            );
        }
    }

    #[test]
    fn swipe_refuses_malformed_order() {
        assert_eq!(swipe_steps_in_order(&[485, 409, 409], 409, 409, 485), None);
        assert_eq!(swipe_steps_in_order(&[485, 0, 409], 409, 409, 485), None);
        assert_eq!(swipe_steps_in_order(&[0, 485], 0, 0, 485), None);
        assert_eq!(swipe_steps_in_order(&[0, 409], 409, 409, 0), None);
    }
}

fn desktop_is_confirmed(desktop: u64, native: Option<u64>, managed: Option<u64>) -> bool {
    native == Some(desktop) && managed == Some(desktop)
}

fn recover_desktop_with(
    transaction: &RiftTransaction<'_>,
    display_uuid: &str,
    desktop: u64,
    action: impl FnOnce() -> bool,
) -> Result<Option<Vec<DisplayData>>> {
    let recovered = |displays: &[DisplayData]| {
        let managed = displays
            .iter()
            .find(|display| display.uuid == display_uuid)
            .and_then(display_space);
        desktop_is_confirmed(desktop, skylight::current_space(display_uuid), managed)
    };
    let mut refreshed = None;
    match transaction.external_step(
        "native Dock desktop recovery",
        EventExpectation::Display(display_uuid),
        action,
        |rift| {
            let displays = rift.displays()?;
            if recovered(&displays) {
                refreshed = Some(displays);
                Ok(true)
            } else {
                Ok(false)
            }
        },
        SETTLE_TIMEOUT,
    ) {
        Ok(true) => Ok(refreshed),
        Ok(false) => Ok(None),
        Err(error) => Err(error),
    }
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
    type GetCurrentSpace = unsafe extern "C" fn(c_int, *const c_void) -> u64;
    type GetActiveSpace = unsafe extern "C" fn(c_int) -> u64;
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
    type CopyManagedDisplaySpaces = unsafe extern "C" fn(c_int, *const c_void) -> *const c_void;
    type DictionaryGetValue = unsafe extern "C" fn(*const c_void, *const c_void) -> *const c_void;
    type Equal = unsafe extern "C" fn(*const c_void, *const c_void) -> u8;
    type GetTypeId = unsafe extern "C" fn(*const c_void) -> usize;
    type TypeId = unsafe extern "C" fn() -> usize;

    /// Loaded separately so topology lookup cannot disable unrelated native
    /// state queries and window-placement validation.
    struct SpaceOrderApi {
        copy_spaces: CopyManagedDisplaySpaces,
        dictionary_get_value: DictionaryGetValue,
        equal: Equal,
        get_type_id: GetTypeId,
        array_type_id: TypeId,
        dictionary_type_id: TypeId,
        number_type_id: TypeId,
    }

    /// `kCFNumberSInt32Type` and `kCFNumberSInt64Type`.
    const CF_NUMBER_SINT32: c_int = 3;
    const CF_NUMBER_SINT64: c_int = 4;
    /// Every space kind: `CGSSpaceMask` all-spaces.
    const CGS_SPACE_MASK_ALL: c_int = 0x7;

    struct Api {
        connection: MainConnectionId,
        current_space: GetCurrentSpace,
        active_space: GetActiveSpace,
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
            // Null callbacks make this a non-retaining array. Keep `number`
            // alive until WindowServer has finished reading the array.
            let ids = (api.array_create)(ptr::null(), values.as_ptr(), 1, ptr::null());
            if ids.is_null() {
                (api.release)(number);
                return None;
            }
            let spaces = (api.copy_spaces_for_windows)((api.connection)(), CGS_SPACE_MASK_ALL, ids);
            (api.release)(ids);
            (api.release)(number);
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

    /// Complete Mission Control order, including native fullscreen spaces.
    /// Mirrors ISS v3.1's `load_space_info_for_display`/`id64` extraction:
    /// <https://github.com/jurplel/InstantSpaceSwitcher/blob/f7616e42651b1ed8d81625e3640eceed6efa8018/Sources/ISS/ISS.c>.
    /// Match the UUID exactly; never borrow another display's order.
    pub fn ordered_spaces(display_uuid: &str) -> Option<Vec<u64>> {
        let api = api()?;
        let order = space_order_api()?;
        let uuid = CString::new(display_uuid).ok()?;
        // Safety: Copy returns owned CF objects, container accessors return
        // borrowed objects. Check types before interpreting values, and release
        // the copied array and every created string on all return paths.
        unsafe {
            let keys = [uuid.as_c_str(), c"Display Identifier", c"Spaces", c"id64"].map(|key| {
                (api.string_create)(ptr::null(), key.as_ptr(), K_CF_STRING_ENCODING_UTF8)
            });
            if keys.iter().any(|key| key.is_null()) {
                for key in keys.into_iter().filter(|key| !key.is_null()) {
                    (api.release)(key);
                }
                return None;
            }
            let displays = (order.copy_spaces)((api.connection)(), ptr::null());
            let has_type = |value: *const c_void, type_id: TypeId| {
                !value.is_null() && (order.get_type_id)(value) == type_id()
            };
            let result = (|| {
                if !has_type(displays, order.array_type_id) {
                    return None;
                }
                for index in 0..(api.array_get_count)(displays) {
                    let display = (api.array_get_value_at_index)(displays, index);
                    if !has_type(display, order.dictionary_type_id) {
                        continue;
                    }
                    let identifier = (order.dictionary_get_value)(display, keys[1]);
                    if identifier.is_null() || (order.equal)(identifier, keys[0]) == 0 {
                        continue;
                    }
                    let spaces = (order.dictionary_get_value)(display, keys[2]);
                    if !has_type(spaces, order.array_type_id) {
                        return None;
                    }
                    let mut ids = Vec::new();
                    for index in 0..(api.array_get_count)(spaces) {
                        let space = (api.array_get_value_at_index)(spaces, index);
                        if !has_type(space, order.dictionary_type_id) {
                            return None;
                        }
                        let number = (order.dictionary_get_value)(space, keys[3]);
                        if !has_type(number, order.number_type_id) {
                            return None;
                        }
                        let mut id: i64 = 0;
                        if !(api.number_get_value)(
                            number,
                            CF_NUMBER_SINT64,
                            ptr::from_mut(&mut id).cast(),
                        ) {
                            return None;
                        }
                        ids.push(u64::try_from(id).ok().filter(|id| *id != 0)?);
                    }
                    return (!ids.is_empty()).then_some(ids);
                }
                None
            })();
            if !displays.is_null() {
                (api.release)(displays);
            }
            for key in keys {
                (api.release)(key);
            }
            result
        }
    }

    fn space_order_api() -> Option<&'static SpaceOrderApi> {
        static API: OnceLock<Option<SpaceOrderApi>> = OnceLock::new();
        API.get_or_init(|| {
            // Safety: immutable function pointers from system frameworks;
            // missing private symbols disable only the swipe-order query.
            unsafe {
                let skylight = open(SKYLIGHT)?;
                let cf = open(CORE_FOUNDATION)?;
                Some(SpaceOrderApi {
                    copy_spaces: std::mem::transmute::<*mut c_void, CopyManagedDisplaySpaces>(
                        symbol(skylight, "CGSCopyManagedDisplaySpaces")?,
                    ),
                    dictionary_get_value: std::mem::transmute::<*mut c_void, DictionaryGetValue>(
                        symbol(cf, "CFDictionaryGetValue")?,
                    ),
                    equal: std::mem::transmute::<*mut c_void, Equal>(symbol(cf, "CFEqual")?),
                    get_type_id: std::mem::transmute::<*mut c_void, GetTypeId>(symbol(
                        cf,
                        "CFGetTypeID",
                    )?),
                    array_type_id: std::mem::transmute::<*mut c_void, TypeId>(symbol(
                        cf,
                        "CFArrayGetTypeID",
                    )?),
                    dictionary_type_id: std::mem::transmute::<*mut c_void, TypeId>(symbol(
                        cf,
                        "CFDictionaryGetTypeID",
                    )?),
                    number_type_id: std::mem::transmute::<*mut c_void, TypeId>(symbol(
                        cf,
                        "CFNumberGetTypeID",
                    )?),
                })
            }
        })
        .as_ref()
    }

    /// The space `display_uuid` is showing, whether or not Rift manages it.
    pub fn current_space(display_uuid: &str) -> Option<u64> {
        let api = api()?;
        let uuid = CString::new(display_uuid).ok()?;
        // Safety: symbols resolved from system frameworks; the CFString is
        // released before returning.
        unsafe {
            let cf_uuid =
                (api.string_create)(ptr::null(), uuid.as_ptr(), K_CF_STRING_ENCODING_UTF8);
            if cf_uuid.is_null() {
                return None;
            }
            let space = (api.current_space)((api.connection)(), cf_uuid);
            (api.release)(cf_uuid);
            (space != 0).then_some(space)
        }
    }

    /// The space of the active display: the one holding the key window.
    pub fn active_space() -> Option<u64> {
        let api = api()?;
        // Safety: resolved from SkyLight above; takes a connection.
        let space = unsafe { (api.active_space)((api.connection)()) };
        (space != 0).then_some(space)
    }

    /// Whether `space` is an ordinary Desktop rather than a fullscreen space.
    pub fn is_desktop(space: u64) -> bool {
        let Some(api) = api() else {
            return false;
        };
        // Safety: resolved from SkyLight above; takes a connection and a space id.
        unsafe { (api.space_get_type)((api.connection)(), space) == SPACE_TYPE_DESKTOP }
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
                current_space: std::mem::transmute::<*mut c_void, GetCurrentSpace>(symbol(
                    skylight,
                    "CGSManagedDisplayGetCurrentSpace",
                )?),
                active_space: std::mem::transmute::<*mut c_void, GetActiveSpace>(symbol(
                    skylight,
                    "CGSGetActiveSpace",
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
