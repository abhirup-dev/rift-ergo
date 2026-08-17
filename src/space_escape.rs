//! Recovery from macOS spaces that Rift does not manage.
//!
//! A window put into native macOS fullscreen gets its own macOS space. Rift
//! does not manage those, so the display reports no active space, `display_space`
//! yields nothing, and every display-targeted command fails before it starts:
//!
//! ```text
//! rift-ergo: target display has no active macOS space
//! ```
//!
//! Rift's own focus commands cannot fix this -- they cannot cross into a space
//! Rift is not tracking. Activating an application whose window lives on one of
//! the display's other spaces does: macOS switches that display off the
//! fullscreen space to show the activated window.
//!
//! This module is deliberately standalone. It reads Rift state and shells out to
//! `osascript`; it changes no existing behaviour and holds no state. To remove
//! it: delete this file, drop `mod space_escape;` from `main.rs`, and delete the
//! `ensure_managed_space` call in `workflow/move_follow.rs`.

use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use rift_client::DisplayData;

use crate::Result;
use crate::rift::{Rift, display_space};

const ESCAPE_TIMEOUT: Duration = Duration::from_millis(1_500);
const PROBE_INTERVAL: Duration = Duration::from_millis(50);

/// Ensure `display_uuid` is showing a space Rift manages, returning refreshed
/// display data. A display that already has one is returned untouched, so this
/// is a no-op on every ordinary keypress.
///
/// Escaping activates another application, which moves focus to that display.
/// Callers that care about the focused window must capture it beforehand.
///
/// Best-effort: if no anchor recovers the display, the original data is returned
/// and the caller fails exactly as it did before this module existed.
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

    // Candidates are this display's own spaces, so the loop is bounded by how
    // many Desktops it has. A fullscreen space contains only its own window, so
    // anchoring there does not recover the display and the next one is tried.
    for space in display.inactive_space_ids.clone() {
        let Some(anchor) = anchor_bundle_id(rift, space)? else {
            continue;
        };
        activate_application(&anchor)?;
        if let Some(displays) = await_managed_space(rift, display_uuid)? {
            return Ok(displays);
        }
    }
    Ok(displays)
}

/// First window of the lowest-numbered occupied workspace on `space`. Rift
/// returns workspaces in configured order, so this prefers workspace 1, then 2,
/// and so on, rather than depending on any particular app being open.
fn anchor_bundle_id(rift: &Rift, space: u64) -> Result<Option<String>> {
    Ok(rift
        .workspaces(space)?
        .into_iter()
        .find(|workspace| !workspace.windows.is_empty())
        .and_then(|workspace| {
            workspace
                .windows
                .first()
                .and_then(|window| window.bundle_id.clone())
        }))
}

fn await_managed_space(rift: &Rift, display_uuid: &str) -> Result<Option<Vec<DisplayData>>> {
    let deadline = Instant::now() + ESCAPE_TIMEOUT;
    while Instant::now() < deadline {
        thread::sleep(PROBE_INTERVAL);
        let displays = rift.displays()?;
        let escaped = displays
            .iter()
            .find(|display| display.uuid == display_uuid)
            .is_some_and(|display| display_space(display).is_some());
        if escaped {
            return Ok(Some(displays));
        }
    }
    Ok(None)
}

/// Activating by bundle id avoids depending on an application's process name,
/// which varies unhelpfully (Teams is "MSTeams", Sublime Text is "sublime_text").
fn activate_application(bundle_id: &str) -> Result<()> {
    Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(format!("tell application id \"{bundle_id}\" to activate"))
        .output()?;
    Ok(())
}
