use crate::Result;
use crate::rift::{Rift, display_space};
use crate::space_escape;

use super::switch_workspace::activate_on_display;

/// Show `workspace_name` on every display, replacing
/// scripts/switch-workspace-both.
///
/// This is `switch_workspace` in a loop: the per-display step already handles
/// leaving a native-fullscreen space and acquiring a display with no focus
/// anchor, so this binding inherits both without repeating either.
pub fn switch_workspace_both(workspace_name: &str) -> Result<()> {
    let rift = Rift::connect()?;
    let mut displays = rift.displays()?;

    // Where the keyboard was. This binding changes what the other displays
    // show; it is not a request to move there.
    let origin = displays
        .iter()
        .find(|display| display.is_active_context)
        .map(|display| display.uuid.clone());

    // Taken up front: each activation returns fresh data, and iterating that
    // while it is being replaced would borrow it for the whole loop.
    let uuids = displays
        .iter()
        .map(|display| display.uuid.clone())
        .collect::<Vec<_>>();

    for uuid in &uuids {
        displays = activate_on_display(&rift, workspace_name, uuid, displays)?;
    }

    let Some(origin) = origin else {
        return Ok(());
    };
    // A no-op when the origin was the last display activated.
    let space = displays
        .iter()
        .find(|display| display.uuid == origin)
        .and_then(display_space);
    if let Some(space) = space {
        space_escape::focus_display(&rift, &origin, space)?;
    }
    Ok(())
}
