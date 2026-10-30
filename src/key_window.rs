//! The window macOS is sending keystrokes to, asked of macOS directly.
//!
//! Rift's `is_focused` flag is its own copy of this answer, and it can be wrong
//! for applications with several windows: moving from Arc to a Slack window on
//! one display left Rift reporting Slack's main window, on the other display, as
//! focused. A binding acting on that copy moves a window the user is not
//! looking at. Commands that act on "this window" ask macOS instead, the way
//! yabai does: SkyLight's `_SLPSGetFrontProcess` names the front process (the
//! system-wide `AXFocusedApplication` is unreliable), and that application's
//! Accessibility focused window is the one the user last typed into.
//!
//! `_AXUIElementGetWindow` maps an Accessibility element to its WindowServer
//! id -- the same id Rift reports as `window_server_id`.
//!
//! Bound with `dlopen`/`dlsym` like `space_escape`, so a missing symbol on a
//! future macOS degrades to an error instead of failing to launch.

use std::ffi::{CString, c_char, c_int, c_void};
use std::ptr;
use std::sync::OnceLock;

use crate::{Result, state_error};

const RTLD_LAZY: c_int = 1;
const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
const AX_ERROR_SUCCESS: c_int = 0;
const APPLICATION_SERVICES: &str =
    "/System/Library/Frameworks/ApplicationServices.framework/ApplicationServices";
const SKYLIGHT: &str = "/System/Library/PrivateFrameworks/SkyLight.framework/SkyLight";
const CORE_FOUNDATION: &str = "/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation";

unsafe extern "C" {
    fn dlopen(path: *const c_char, mode: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

type IsProcessTrusted = unsafe extern "C" fn() -> bool;
/// `ProcessSerialNumber`: two `u32` halves.
type FrontProcess = unsafe extern "C" fn(*mut [u32; 2]) -> c_int;
type ProcessPid = unsafe extern "C" fn(*const [u32; 2], *mut c_int) -> c_int;
type CreateApplication = unsafe extern "C" fn(c_int) -> *const c_void;
type CopyAttributeValue =
    unsafe extern "C" fn(*const c_void, *const c_void, *mut *const c_void) -> c_int;
type GetWindow = unsafe extern "C" fn(*const c_void, *mut u32) -> c_int;
type StringCreate = unsafe extern "C" fn(*const c_void, *const c_char, u32) -> *const c_void;
type Release = unsafe extern "C" fn(*const c_void);

struct Api {
    is_process_trusted: IsProcessTrusted,
    front_process: FrontProcess,
    process_pid: ProcessPid,
    create_application: CreateApplication,
    copy_attribute_value: CopyAttributeValue,
    get_window: GetWindow,
    string_create: StringCreate,
    release: Release,
}

// Safety: immutable C function pointers into always-loaded system frameworks.
unsafe impl Send for Api {}
unsafe impl Sync for Api {}

/// The WindowServer id of the focused application's focused window.
pub fn window_server_id() -> Result<u32> {
    let api = api().ok_or_else(|| state_error("Accessibility API unavailable"))?;
    // Safety: symbols resolved from system frameworks; every CF object created
    // or copied here is released before returning.
    unsafe {
        if !(api.is_process_trusted)() {
            return Err(state_error(
                "rift-ergo lacks Accessibility permission; cannot read the key window",
            ));
        }
        let mut psn = [0u32; 2];
        let mut pid: c_int = 0;
        if (api.front_process)(&mut psn) != 0 || (api.process_pid)(&psn, &mut pid) != 0 {
            return Err(state_error("no front process"));
        }
        let app = (api.create_application)(pid);
        if app.is_null() {
            return Err(state_error(format!(
                "no Accessibility element for pid {pid}"
            )));
        }
        let window = copy_attribute(api, app, "AXFocusedWindow");
        (api.release)(app);
        let window =
            window.ok_or_else(|| state_error("focused application has no focused window"))?;
        let mut id: u32 = 0;
        let error = (api.get_window)(window, &mut id);
        (api.release)(window);
        if error != AX_ERROR_SUCCESS || id == 0 {
            return Err(state_error("focused window has no WindowServer id"));
        }
        Ok(id)
    }
}

/// Copy `attribute` of `element`; the caller owns and releases the result.
unsafe fn copy_attribute(
    api: &Api,
    element: *const c_void,
    attribute: &str,
) -> Option<*const c_void> {
    let name = CString::new(attribute).ok()?;
    unsafe {
        let cf_name = (api.string_create)(ptr::null(), name.as_ptr(), K_CF_STRING_ENCODING_UTF8);
        if cf_name.is_null() {
            return None;
        }
        let mut value: *const c_void = ptr::null();
        let error = (api.copy_attribute_value)(element, cf_name, &mut value);
        (api.release)(cf_name);
        (error == AX_ERROR_SUCCESS && !value.is_null()).then_some(value)
    }
}

fn api() -> Option<&'static Api> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(load).as_ref()
}

fn load() -> Option<Api> {
    // Safety: both paths are system frameworks; every symbol is checked for
    // null before being transmuted to its documented signature.
    unsafe {
        let ax = open(APPLICATION_SERVICES)?;
        let skylight = open(SKYLIGHT)?;
        let core_foundation = open(CORE_FOUNDATION)?;
        Some(Api {
            is_process_trusted: std::mem::transmute::<*mut c_void, IsProcessTrusted>(symbol(
                ax,
                "AXIsProcessTrusted",
            )?),
            front_process: std::mem::transmute::<*mut c_void, FrontProcess>(symbol(
                skylight,
                "_SLPSGetFrontProcess",
            )?),
            process_pid: std::mem::transmute::<*mut c_void, ProcessPid>(symbol(
                ax,
                "GetProcessPID",
            )?),
            create_application: std::mem::transmute::<*mut c_void, CreateApplication>(symbol(
                ax,
                "AXUIElementCreateApplication",
            )?),
            copy_attribute_value: std::mem::transmute::<*mut c_void, CopyAttributeValue>(symbol(
                ax,
                "AXUIElementCopyAttributeValue",
            )?),
            get_window: std::mem::transmute::<*mut c_void, GetWindow>(symbol(
                ax,
                "_AXUIElementGetWindow",
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
