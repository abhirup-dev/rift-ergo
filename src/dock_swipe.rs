//! Instant space switching by posting a synthetic Dock swipe.
//!
//! A Rust port of the switching core of InstantSpaceSwitcher (ISS, MIT) v3.1,
//! commit f7616e4. The upstream license is retained in THIRD_PARTY_NOTICES.
//! Refer back to it when macOS changes the event format again:
//!
//! - <https://github.com/jurplel/InstantSpaceSwitcher/blob/f7616e42651b1ed8d81625e3640eceed6efa8018/Sources/ISS/ISS.c>
//!   (`iss_post_modern_dock_swipe`, `iss_perform_switch_gesture`, `iss_modern_swipe_sign`)
//! - <https://github.com/jurplel/InstantSpaceSwitcher/blob/f7616e42651b1ed8d81625e3640eceed6efa8018/Sources/ISS/event_serialize.c>
//!   (`iss_augment_dock_swipe_event`, `iss_generate_iohid_payload`)
//! - <https://github.com/jurplel/InstantSpaceSwitcher/issues/72> for how the
//!   macOS 27 format was found (FasterSwiper's `macos27` branch).
//!
//! The Dock treats the events as a real trackpad flick, so macOS runs its own
//! Space change -- including the leave-fullscreen teardown that
//! `CGSManagedDisplaySetCurrentSpace` skips -- with near-zero travel, so there is
//! nothing to animate. One swipe moves one space.
//!
//! On macOS 27 the Dock reads the gesture only from an IOHID payload hidden in
//! the serialised event, so each event is serialised, has the payload appended,
//! and is deserialised again. Only that format is ported: on older macOS
//! [`swipe`] returns false and callers fall back.
//!
//! Bound with `dlopen`/`dlsym`, like the rest of the private surface here, so a
//! missing symbol degrades to "unavailable" instead of failing to launch.

use std::ffi::{CString, c_char, c_int, c_void};
use std::ptr;
use std::sync::OnceLock;

const RTLD_LAZY: c_int = 1;
const CORE_GRAPHICS: &str = "/System/Library/Frameworks/CoreGraphics.framework/CoreGraphics";
const CORE_FOUNDATION: &str = "/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation";
const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
const SESSION_EVENT_TAP: u32 = 1;

// Undocumented CGEvent fields, as named in ISS.c.
const FIELD_CGS_EVENT_TYPE: u32 = 55;
const FIELD_GESTURE_HID_TYPE: u32 = 110;
const FIELD_SWIPE_MOTION: u32 = 123;
const FIELD_SWIPE_PROGRESS: u32 = 124;
const FIELD_SWIPE_POSITION_X: u32 = 125;
const FIELD_SWIPE_VELOCITY_X: u32 = 129;
const FIELD_GESTURE_PHASE: u32 = 132;
const FIELD_GESTURE_PHASE_ALIAS: u32 = 134;
const FIELD_ZOOM_DELTA_Y: u32 = 138;
const FIELD_SOURCE_PID_ALIAS: u32 = 169;

const CGS_EVENT_GESTURE: i64 = 29;
const CGS_EVENT_DOCK_CONTROL: i64 = 30;
const IOHID_EVENT_TYPE_DOCK_SWIPE: i64 = 23;
const IOHID_EVENT_TYPE_VELOCITY: u32 = 9;
const IOHID_GESTURE_FLAVOR_DOCK_PRIMARY: u16 = 3;
const MOTION_HORIZONTAL: u16 = 1;
/// Field id of the appended payload in the serialised event.
const PAYLOAD_FIELD_ID: u16 = 4205;

const PHASE_BEGAN: u8 = 1;
const PHASE_CHANGED: u8 = 2;
const PHASE_ENDED: u8 = 4;

/// The smallest non-zero 16.16 value: leaves no travel to animate. Full
/// progress switches too, but flashes the previous space first.
const PROGRESS: f64 = 0.000_016;
/// The fling on the Ended phase is what commits the switch. The payload field is
/// 16.16 fixed point, so it cannot go much higher (max ~32767).
const FLING_VELOCITY: f64 = 9999.0;
const POSITION_X: f64 = 0.1;
const ZOOM_DELTA_Y: f64 = 3.0;

unsafe extern "C" {
    fn dlopen(path: *const c_char, mode: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn mach_absolute_time() -> u64;
    fn sysctlbyname(
        name: *const c_char,
        old: *mut c_void,
        old_len: *mut usize,
        new: *const c_void,
        new_len: usize,
    ) -> c_int;
}

type EventCreate = unsafe extern "C" fn(*const c_void) -> *const c_void;
type SetIntegerField = unsafe extern "C" fn(*const c_void, u32, i64);
type SetDoubleField = unsafe extern "C" fn(*const c_void, u32, f64);
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct EventPoint {
    x: f64,
    y: f64,
}
type SetLocation = unsafe extern "C" fn(*const c_void, EventPoint);
type EventCreateData = unsafe extern "C" fn(*const c_void, *const c_void) -> *const c_void;
type EventCreateFromData = unsafe extern "C" fn(*const c_void, *const c_void) -> *const c_void;
type PostEvent = unsafe extern "C" fn(u32, *const c_void);
type DataCreate = unsafe extern "C" fn(*const c_void, *const u8, isize) -> *const c_void;
type DataGetBytePtr = unsafe extern "C" fn(*const c_void) -> *const u8;
type DataGetLength = unsafe extern "C" fn(*const c_void) -> isize;
type StringCreate = unsafe extern "C" fn(*const c_void, *const c_char, u32) -> *const c_void;
type GetAppBoolean = unsafe extern "C" fn(*const c_void, *const c_void, *mut u8) -> u8;
type Release = unsafe extern "C" fn(*const c_void);

struct Api {
    event_create: EventCreate,
    set_integer: SetIntegerField,
    set_double: SetDoubleField,
    set_location: SetLocation,
    event_create_data: EventCreateData,
    event_create_from_data: EventCreateFromData,
    post: PostEvent,
    data_create: DataCreate,
    data_bytes: DataGetBytePtr,
    data_length: DataGetLength,
    string_create: StringCreate,
    get_app_boolean: GetAppBoolean,
    any_application: *const c_void,
    release: Release,
}

// Safety: immutable C function pointers and a constant CFString in
// always-loaded system frameworks.
unsafe impl Send for Api {}
unsafe impl Sync for Api {}

/// Swipe the acquired display `steps` spaces: positive towards the right of
/// Mission Control's order, negative towards the left. Give both event types
/// explicit display coordinates; do not inherit the user's pointer location.
/// Setting gesture-event coordinates does not move the real mouse pointer.
///
/// Posting only requests the switch; the Space change completes
/// asynchronously, so callers must wait for it.
pub fn swipe(steps: isize, frame: rift_client::Rect) -> bool {
    if steps == 0 {
        return true;
    }
    let Some(api) = api() else {
        return false;
    };
    let Some(target) = display_center(frame) else {
        return false;
    };
    let sign = direction_sign(api, steps > 0);
    // ISS sends all three phases: with only two, Mission Control misbehaves.
    (0..steps.unsigned_abs()).all(|_| {
        [PHASE_BEGAN, PHASE_CHANGED, PHASE_ENDED]
            .into_iter()
            .all(|phase| post_swipe(api, phase, sign, target))
    })
}

fn display_center(frame: rift_client::Rect) -> Option<EventPoint> {
    let point = EventPoint {
        x: frame.origin.x + frame.size.width / 2.0,
        y: frame.origin.y + frame.size.height / 2.0,
    };
    (frame.size.width > 0.0
        && frame.size.height > 0.0
        && point.x.is_finite()
        && point.y.is_finite())
    .then_some(point)
}

/// On macOS 27 a rightward swipe is negative, and the Dock reads the sign
/// through the natural scrolling preference, so it flips again with that off.
fn direction_sign(api: &Api, right: bool) -> f64 {
    let sign = if right { -1.0 } else { 1.0 };
    if natural_scrolling(api) { sign } else { -sign }
}

fn natural_scrolling(api: &Api) -> bool {
    let Ok(key) = CString::new("com.apple.swipescrolldirection") else {
        return true;
    };
    // Safety: symbols resolved from CoreFoundation; the key is released.
    unsafe {
        let key = (api.string_create)(ptr::null(), key.as_ptr(), K_CF_STRING_ENCODING_UTF8);
        if key.is_null() {
            return true;
        }
        let mut valid = 0u8;
        let natural = (api.get_app_boolean)(key, api.any_application, &mut valid);
        (api.release)(key);
        // Unset means the macOS default, which is natural scrolling.
        valid == 0 || natural != 0
    }
}

fn post_swipe(api: &Api, phase: u8, sign: f64, target: EventPoint) -> bool {
    let velocity_x = if phase == PHASE_ENDED {
        sign * FLING_VELOCITY
    } else {
        0.0
    };
    // Safety: symbols resolved from system frameworks; every event and data
    // object created here is released before returning.
    unsafe {
        let timestamp = mach_absolute_time();
        let event = (api.event_create)(ptr::null());
        if event.is_null() {
            return false;
        }
        (api.set_location)(event, target);
        (api.set_integer)(event, FIELD_CGS_EVENT_TYPE, CGS_EVENT_DOCK_CONTROL);
        (api.set_integer)(event, FIELD_GESTURE_HID_TYPE, IOHID_EVENT_TYPE_DOCK_SWIPE);
        (api.set_integer)(event, FIELD_GESTURE_PHASE, i64::from(phase));
        (api.set_integer)(event, FIELD_SWIPE_MOTION, i64::from(MOTION_HORIZONTAL));
        (api.set_double)(event, FIELD_SWIPE_PROGRESS, sign * PROGRESS);
        // Fields a real trackpad swipe carries, from FasterSwiper's traces.
        (api.set_integer)(event, FIELD_GESTURE_PHASE_ALIAS, i64::from(phase));
        (api.set_double)(event, FIELD_ZOOM_DELTA_Y, ZOOM_DELTA_Y);
        (api.set_double)(event, FIELD_SOURCE_PID_ALIAS, timestamp as f64);
        (api.set_double)(event, FIELD_SWIPE_POSITION_X, POSITION_X);
        if velocity_x != 0.0 {
            (api.set_double)(event, FIELD_SWIPE_VELOCITY_X, velocity_x);
        }
        let augmented = augment(
            api,
            event,
            &payload(timestamp, phase, sign * PROGRESS, velocity_x),
        );
        (api.release)(event);
        let Some(augmented) = augmented else {
            return false;
        };
        // The Dock ignores a DockControl event without a companion gesture event.
        let companion = (api.event_create)(ptr::null());
        if companion.is_null() {
            (api.release)(augmented);
            return false;
        }
        (api.set_location)(companion, target);
        (api.set_integer)(companion, FIELD_CGS_EVENT_TYPE, CGS_EVENT_GESTURE);
        (api.post)(SESSION_EVENT_TAP, augmented);
        (api.post)(SESSION_EVENT_TAP, companion);
        (api.release)(augmented);
        (api.release)(companion);
    }
    true
}

/// Re-create `event` with `payload` appended to its serialised form, tagged as
/// field [`PAYLOAD_FIELD_ID`]. `None` if the serialisation is not the version-2
/// format this was ported against.
unsafe fn augment(api: &Api, event: *const c_void, payload: &[u8]) -> Option<*const c_void> {
    // Safety: the caller passes a live CGEvent; CF objects are released here.
    unsafe {
        let data = (api.event_create_data)(ptr::null(), event);
        if data.is_null() {
            return None;
        }
        let length = usize::try_from((api.data_length)(data)).unwrap_or(0);
        let bytes = std::slice::from_raw_parts((api.data_bytes)(data), length);
        let serialised = (bytes.get(..4) == Some(&[0, 0, 0, 2]))
            .then(|| [bytes, &tag(payload.len()), payload].concat());
        (api.release)(data);
        let serialised = serialised?;
        let data = (api.data_create)(
            ptr::null(),
            serialised.as_ptr(),
            isize::try_from(serialised.len()).ok()?,
        );
        if data.is_null() {
            return None;
        }
        let result = (api.event_create_from_data)(ptr::null(), data);
        (api.release)(data);
        (!result.is_null()).then_some(result)
    }
}

/// Big-endian payload size, then the field id.
fn tag(payload_length: usize) -> [u8; 4] {
    let [size_hi, size_lo] = u16::try_from(payload_length).unwrap_or(0).to_be_bytes();
    let [id_hi, id_lo] = PAYLOAD_FIELD_ID.to_be_bytes();
    [size_hi, size_lo, id_hi, id_lo]
}

/// The IOHID system-queue element the Dock reads on macOS 27: a header, a fluid
/// touch gesture event and, on the Ended phase, a velocity event. Packed,
/// little-endian; layouts are `IOHIDSystemQueueElementHeader` (28 bytes),
/// `IOHIDFluidTouchGestureData` (40) and `IOHIDVelocityEventData` (28) in
/// event_serialize.c.
fn payload(timestamp: u64, phase: u8, progress: f64, velocity_x: f64) -> Vec<u8> {
    let with_velocity = velocity_x != 0.0 || phase == PHASE_ENDED;
    let mut out = Vec::with_capacity(96);
    // Header: timestamp, sender id, options, attribute length, event count.
    out.extend(timestamp.to_le_bytes());
    out.extend(0u64.to_le_bytes());
    out.extend(0u32.to_le_bytes());
    out.extend(0u32.to_le_bytes());
    out.extend(if with_velocity { 2u32 } else { 1 }.to_le_bytes());
    // Fluid touch gesture: base, position x/y/z, swipe mask, motion, flavor, progress.
    push_base(&mut out, 40, 23, u32::from(phase) << 24, 0);
    out.extend(fixed_16_16(POSITION_X).to_le_bytes());
    out.extend(0i32.to_le_bytes());
    out.extend(0i32.to_le_bytes());
    out.extend(0u32.to_le_bytes());
    out.extend(MOTION_HORIZONTAL.to_le_bytes());
    out.extend(IOHID_GESTURE_FLAVOR_DOCK_PRIMARY.to_le_bytes());
    out.extend(fixed_16_16(progress).to_le_bytes());
    if with_velocity {
        // Velocity: base, then x/y/z.
        push_base(&mut out, 28, IOHID_EVENT_TYPE_VELOCITY, 0, 1);
        out.extend(fixed_16_16(velocity_x).to_le_bytes());
        out.extend(0i32.to_le_bytes());
        out.extend(0i32.to_le_bytes());
    }
    out
}

/// `IOHIDEventBase`: size, type, options, depth, three reserved bytes.
fn push_base(out: &mut Vec<u8>, size: u32, kind: u32, options: u32, depth: u8) {
    out.extend(size.to_le_bytes());
    out.extend(kind.to_le_bytes());
    out.extend(options.to_le_bytes());
    out.extend([depth, 0, 0, 0]);
}

/// 16.16 fixed point, never rounding a non-zero value to zero.
fn fixed_16_16(value: f64) -> i32 {
    match (value * 65536.0) as i32 {
        0 if value > 0.0 => 1,
        0 if value < 0.0 => -1,
        fixed => fixed,
    }
}

fn api() -> Option<&'static Api> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(load).as_ref()
}

fn load() -> Option<Api> {
    if macos_major()? < 27 {
        return None;
    }
    // Safety: system framework paths; each symbol is null-checked before being
    // transmuted to its documented signature.
    unsafe {
        let cg = open(CORE_GRAPHICS)?;
        let cf = open(CORE_FOUNDATION)?;
        let any_application = *symbol(cf, "kCFPreferencesAnyApplication")?.cast::<*const c_void>();
        Some(Api {
            event_create: std::mem::transmute::<*mut c_void, EventCreate>(symbol(
                cg,
                "CGEventCreate",
            )?),
            set_integer: std::mem::transmute::<*mut c_void, SetIntegerField>(symbol(
                cg,
                "CGEventSetIntegerValueField",
            )?),
            set_double: std::mem::transmute::<*mut c_void, SetDoubleField>(symbol(
                cg,
                "CGEventSetDoubleValueField",
            )?),
            set_location: std::mem::transmute::<*mut c_void, SetLocation>(symbol(
                cg,
                "CGEventSetLocation",
            )?),
            event_create_data: std::mem::transmute::<*mut c_void, EventCreateData>(symbol(
                cg,
                "CGEventCreateData",
            )?),
            event_create_from_data: std::mem::transmute::<*mut c_void, EventCreateFromData>(
                symbol(cg, "CGEventCreateFromData")?,
            ),
            post: std::mem::transmute::<*mut c_void, PostEvent>(symbol(cg, "CGEventPost")?),
            data_create: std::mem::transmute::<*mut c_void, DataCreate>(symbol(
                cf,
                "CFDataCreate",
            )?),
            data_bytes: std::mem::transmute::<*mut c_void, DataGetBytePtr>(symbol(
                cf,
                "CFDataGetBytePtr",
            )?),
            data_length: std::mem::transmute::<*mut c_void, DataGetLength>(symbol(
                cf,
                "CFDataGetLength",
            )?),
            string_create: std::mem::transmute::<*mut c_void, StringCreate>(symbol(
                cf,
                "CFStringCreateWithCString",
            )?),
            get_app_boolean: std::mem::transmute::<*mut c_void, GetAppBoolean>(symbol(
                cf,
                "CFPreferencesGetAppBooleanValue",
            )?),
            any_application,
            release: std::mem::transmute::<*mut c_void, Release>(symbol(cf, "CFRelease")?),
        })
    }
}

/// The macOS major version, from `kern.osproductversion` ("27.0.1").
fn macos_major() -> Option<u32> {
    let name = CString::new("kern.osproductversion").ok()?;
    let mut buffer = [0u8; 32];
    let mut length = buffer.len();
    // Safety: the buffer and its length describe valid writable memory.
    let status = unsafe {
        sysctlbyname(
            name.as_ptr(),
            buffer.as_mut_ptr().cast(),
            &mut length,
            ptr::null(),
            0,
        )
    };
    if status != 0 {
        return None;
    }
    let version = std::str::from_utf8(&buffer[..length]).ok()?;
    version
        .trim_end_matches('\0')
        .split('.')
        .next()?
        .parse()
        .ok()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gesture_coordinates_target_the_display() {
        let mut frame: rift_client::Rect = serde_json::from_value(serde_json::json!({
            "origin": {"x": -104.0, "y": -1409.0},
            "size": {"width": 2560.0, "height": 1409.0}
        }))
        .unwrap();
        assert_eq!(
            display_center(frame),
            Some(EventPoint {
                x: 1176.0,
                y: -704.5
            })
        );
        frame.size.width = 0.0;
        assert_eq!(display_center(frame), None);
    }

    #[test]
    #[ignore = "read-only macOS event serialization check; posts no input"]
    fn event_target_survives_serialization_without_moving_pointer() {
        let api = api().unwrap();
        type GetLocation = unsafe extern "C" fn(*const c_void) -> EventPoint;
        unsafe {
            let cg = open(CORE_GRAPHICS).unwrap();
            let location = std::mem::transmute::<*mut c_void, GetLocation>(
                symbol(cg, "CGEventGetLocation").unwrap(),
            );
            let event = (api.event_create)(ptr::null());
            assert!(!event.is_null());
            let before = location(event);
            let target = EventPoint { x: 864.0, y: 575.5 };
            (api.set_location)(event, target);
            (api.set_integer)(event, FIELD_CGS_EVENT_TYPE, CGS_EVENT_DOCK_CONTROL);
            let augmented = augment(api, event, &payload(7, PHASE_BEGAN, PROGRESS, 0.0)).unwrap();
            let serialized_location = location(augmented);
            (api.release)(augmented);
            (api.release)(event);
            let fresh = (api.event_create)(ptr::null());
            assert!(!fresh.is_null());
            let after = location(fresh);
            (api.release)(fresh);
            assert_eq!(serialized_location, target);
            assert_eq!(
                before, after,
                "only the synthetic event may change location"
            );
        }
    }

    #[test]
    fn payload_matches_iss_layout() {
        let began = payload(7, PHASE_BEGAN, PROGRESS, 0.0);
        assert_eq!(began.len(), 28 + 40);
        assert_eq!(began[24..28], 1u32.to_le_bytes(), "one event");
        assert_eq!(
            began[36..40],
            (1u32 << 24).to_le_bytes(),
            "phase in options"
        );
        assert_eq!(
            began[64..68],
            1i32.to_le_bytes(),
            "progress never rounds to 0"
        );

        let ended = payload(7, PHASE_ENDED, -PROGRESS, -FLING_VELOCITY);
        assert_eq!(ended.len(), 28 + 40 + 28);
        assert_eq!(ended[24..28], 2u32.to_le_bytes(), "gesture plus velocity");
        assert_eq!(ended[64..68], (-1i32).to_le_bytes());
        assert_eq!(ended[84..88], fixed_16_16(-FLING_VELOCITY).to_le_bytes());
        assert_eq!(tag(ended.len()), [0, 96, 0x10, 0x6d]);
    }
}
