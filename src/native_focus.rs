//! Acquire native display focus without activating a window on another Space.
//!
//! WindowServer supplies visible window metadata (no titles or screen capture).
//! Callers verify Space membership before requesting exact-window focus via
//! `_SLPSSetFrontProcessWithOptions`, then confirm native focus through the
//! shared Crossbeam transaction. No clicks, AX prompts, or Space switches here.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::ptr;
use std::sync::OnceLock;

use rift_client::Rect;

const RTLD_LAZY: c_int = 1;
const UTF8: u32 = 0x0800_0100;
const CF_SINT64: c_int = 4;
const CF_FLOAT64: c_int = 6;
const USER_GENERATED: u32 = 0x200;
const CORE_GRAPHICS: &str = "/System/Library/Frameworks/CoreGraphics.framework/CoreGraphics";
const CORE_FOUNDATION: &str = "/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation";
const APPLICATION_SERVICES: &str =
    "/System/Library/Frameworks/ApplicationServices.framework/ApplicationServices";
const SKYLIGHT: &str = "/System/Library/PrivateFrameworks/SkyLight.framework/SkyLight";

unsafe extern "C" {
    fn dlopen(path: *const c_char, mode: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
}

type CopyWindows = unsafe extern "C" fn(u32, u32) -> *const c_void;
type ArrayCount = unsafe extern "C" fn(*const c_void) -> isize;
type ArrayValue = unsafe extern "C" fn(*const c_void, isize) -> *const c_void;
type DictionaryValue = unsafe extern "C" fn(*const c_void, *const c_void) -> *const c_void;
type GetType = unsafe extern "C" fn(*const c_void) -> usize;
type TypeId = unsafe extern "C" fn() -> usize;
type NumberValue = unsafe extern "C" fn(*const c_void, c_int, *mut c_void) -> bool;
type StringCreate = unsafe extern "C" fn(*const c_void, *const c_char, u32) -> *const c_void;
type Release = unsafe extern "C" fn(*const c_void);
type ProcessForPid = unsafe extern "C" fn(c_int, *mut [u32; 2]) -> c_int;
type SetFront = unsafe extern "C" fn(*const [u32; 2], u32, u32) -> c_int;

struct Api {
    copy_windows: CopyWindows,
    array_count: ArrayCount,
    array_value: ArrayValue,
    dictionary_value: DictionaryValue,
    get_type: GetType,
    array_type: TypeId,
    dictionary_type: TypeId,
    number_type: TypeId,
    number_value: NumberValue,
    string_create: StringCreate,
    release: Release,
    process_for_pid: ProcessForPid,
    set_front: SetFront,
}

#[derive(Clone, Copy, Debug)]
pub struct NativeWindow {
    pub id: u32,
    pid: c_int,
}

#[derive(Clone, Copy)]
struct Bounds {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

impl From<Rect> for Bounds {
    fn from(rect: Rect) -> Self {
        Self {
            x: rect.origin.x,
            y: rect.origin.y,
            width: rect.size.width,
            height: rect.size.height,
        }
    }
}

/// Prefer the largest visible window whose center is on this display. Space
/// membership is checked by the caller before focusing any candidate. This
/// excludes another display's overlapping overlays and tiny fullscreen chrome.
pub fn visible_windows(display: Rect) -> Vec<NativeWindow> {
    let Some(api) = api() else { return Vec::new() };
    let mut candidates = Vec::new();
    // Safety: Copy owns the array; all nested objects are borrowed. Every
    // container/number is type-checked before access; the array is released.
    unsafe {
        // On-screen only | exclude Desktop windows.
        let windows = (api.copy_windows)(1 | 16, 0);
        if !has_type(api, windows, api.array_type) {
            if !windows.is_null() {
                (api.release)(windows);
            }
            return Vec::new();
        }
        for index in 0..(api.array_count)(windows) {
            let window = (api.array_value)(windows, index);
            let candidate = (|| {
                if integer(api, window, c"kCGWindowLayer")? != 0 {
                    return None;
                }
                if double(api, window, c"kCGWindowAlpha").is_some_and(|alpha| alpha <= 0.0) {
                    return None;
                }
                let bounds = attribute(api, window, c"kCGWindowBounds")?;
                let bounds = Bounds {
                    x: double(api, bounds, c"X")?,
                    y: double(api, bounds, c"Y")?,
                    width: double(api, bounds, c"Width")?,
                    height: double(api, bounds, c"Height")?,
                };
                let area = candidate_area(bounds, display.into())?;
                let id = u32::try_from(integer(api, window, c"kCGWindowNumber")?).ok()?;
                let pid = c_int::try_from(integer(api, window, c"kCGWindowOwnerPID")?).ok()?;
                (id != 0 && pid > 0).then_some((NativeWindow { id, pid }, area))
            })();
            if let Some(candidate) = candidate {
                candidates.push(candidate);
            }
        }
        (api.release)(windows);
    }
    candidates.sort_by(|a, b| b.1.total_cmp(&a.1));
    candidates.into_iter().map(|(window, _)| window).collect()
}

/// Request focus on this exact window, not its application's arbitrary main
/// window. Success is only acceptance; the transaction must confirm which
/// display owns native focus before posting a Dock gesture.
pub fn focus(window: NativeWindow) -> bool {
    let Some(api) = api() else { return false };
    let mut psn = [0u32; 2];
    // Safety: resolved native signatures; no input events or permission prompts.
    unsafe {
        (api.process_for_pid)(window.pid, &mut psn) == 0
            && (api.set_front)(&psn, window.id, USER_GENERATED) == 0
    }
}

fn candidate_area(window: Bounds, display: Bounds) -> Option<f64> {
    if [
        window.x,
        window.y,
        window.width,
        window.height,
        display.x,
        display.y,
        display.width,
        display.height,
    ]
    .iter()
    .any(|value| !value.is_finite())
        || window.width <= 0.0
        || window.height <= 0.0
        || display.width <= 0.0
        || display.height <= 0.0
    {
        return None;
    }
    let center_x = window.x + window.width / 2.0;
    let center_y = window.y + window.height / 2.0;
    if center_x < display.x
        || center_x >= display.x + display.width
        || center_y < display.y
        || center_y >= display.y + display.height
    {
        return None;
    }
    let width = (window.x + window.width).min(display.x + display.width) - window.x.max(display.x);
    let height =
        (window.y + window.height).min(display.y + display.height) - window.y.max(display.y);
    let area = width * height;
    (width > 0.0 && height > 0.0 && area.is_finite()).then_some(area)
}

unsafe fn has_type(api: &Api, value: *const c_void, expected: TypeId) -> bool {
    unsafe { !value.is_null() && (api.get_type)(value) == expected() }
}

/// Borrowed value; the enclosing copied array keeps it alive.
unsafe fn attribute(api: &Api, dictionary: *const c_void, key: &CStr) -> Option<*const c_void> {
    unsafe {
        if !has_type(api, dictionary, api.dictionary_type) {
            return None;
        }
        let key = (api.string_create)(ptr::null(), key.as_ptr(), UTF8);
        if key.is_null() {
            return None;
        }
        let value = (api.dictionary_value)(dictionary, key);
        (api.release)(key);
        (!value.is_null()).then_some(value)
    }
}

unsafe fn integer(api: &Api, dictionary: *const c_void, key: &CStr) -> Option<i64> {
    unsafe {
        let value = attribute(api, dictionary, key)?;
        if !has_type(api, value, api.number_type) {
            return None;
        }
        let mut out = 0i64;
        (api.number_value)(value, CF_SINT64, ptr::from_mut(&mut out).cast()).then_some(out)
    }
}

unsafe fn double(api: &Api, dictionary: *const c_void, key: &CStr) -> Option<f64> {
    unsafe {
        let value = attribute(api, dictionary, key)?;
        if !has_type(api, value, api.number_type) {
            return None;
        }
        let mut out = 0f64;
        (api.number_value)(value, CF_FLOAT64, ptr::from_mut(&mut out).cast()).then_some(out)
    }
}

fn api() -> Option<&'static Api> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(load).as_ref()
}

fn load() -> Option<Api> {
    // Safety: symbols and signatures from system frameworks. Missing symbols
    // disable acquisition instead of preventing the helper from launching.
    unsafe {
        let cg = open(CORE_GRAPHICS)?;
        let cf = open(CORE_FOUNDATION)?;
        let ax = open(APPLICATION_SERVICES)?;
        let sl = open(SKYLIGHT)?;
        Some(Api {
            copy_windows: std::mem::transmute::<*mut c_void, CopyWindows>(symbol(
                cg,
                "CGWindowListCopyWindowInfo",
            )?),
            array_count: std::mem::transmute::<*mut c_void, ArrayCount>(symbol(
                cf,
                "CFArrayGetCount",
            )?),
            array_value: std::mem::transmute::<*mut c_void, ArrayValue>(symbol(
                cf,
                "CFArrayGetValueAtIndex",
            )?),
            dictionary_value: std::mem::transmute::<*mut c_void, DictionaryValue>(symbol(
                cf,
                "CFDictionaryGetValue",
            )?),
            get_type: std::mem::transmute::<*mut c_void, GetType>(symbol(cf, "CFGetTypeID")?),
            array_type: std::mem::transmute::<*mut c_void, TypeId>(symbol(cf, "CFArrayGetTypeID")?),
            dictionary_type: std::mem::transmute::<*mut c_void, TypeId>(symbol(
                cf,
                "CFDictionaryGetTypeID",
            )?),
            number_type: std::mem::transmute::<*mut c_void, TypeId>(symbol(
                cf,
                "CFNumberGetTypeID",
            )?),
            number_value: std::mem::transmute::<*mut c_void, NumberValue>(symbol(
                cf,
                "CFNumberGetValue",
            )?),
            string_create: std::mem::transmute::<*mut c_void, StringCreate>(symbol(
                cf,
                "CFStringCreateWithCString",
            )?),
            release: std::mem::transmute::<*mut c_void, Release>(symbol(cf, "CFRelease")?),
            process_for_pid: std::mem::transmute::<*mut c_void, ProcessForPid>(symbol(
                ax,
                "GetProcessForPID",
            )?),
            set_front: std::mem::transmute::<*mut c_void, SetFront>(symbol(
                sl,
                "_SLPSSetFrontProcessWithOptions",
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
    let value = unsafe { dlsym(handle, name.as_ptr()) };
    (!value.is_null()).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::{Bounds, candidate_area};

    #[test]
    fn fullscreen_content_beats_chrome_on_target_display() {
        let display = Bounds {
            x: -104.0,
            y: -1409.0,
            width: 2560.0,
            height: 1409.0,
        };
        let content = Bounds {
            x: -104.0,
            y: -1404.0,
            width: 2560.0,
            height: 1404.0,
        };
        let chrome = Bounds {
            x: -104.0,
            y: -1440.0,
            width: 2560.0,
            height: 68.0,
        };
        assert!(
            candidate_area(content, display).unwrap() > candidate_area(chrome, display).unwrap()
        );
        let other_display = Bounds {
            x: 0.0,
            y: 0.0,
            width: 1728.0,
            height: 1117.0,
        };
        assert!(candidate_area(other_display, display).is_none());
    }

    #[test]
    fn reject_overlapping_overlay_centered_on_another_display() {
        let display = Bounds {
            x: -104.0,
            y: -1409.0,
            width: 2560.0,
            height: 1409.0,
        };
        let overlay = Bounds {
            x: 0.0,
            y: -323.0,
            width: 2560.0,
            height: 1440.0,
        };
        assert!(candidate_area(overlay, display).is_none());
    }

    #[test]
    fn reject_invalid_bounds() {
        let display = Bounds {
            x: 0.0,
            y: 0.0,
            width: 1000.0,
            height: 1000.0,
        };
        for width in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let window = Bounds { width, ..display };
            assert!(candidate_area(window, display).is_none());
        }
    }
}
