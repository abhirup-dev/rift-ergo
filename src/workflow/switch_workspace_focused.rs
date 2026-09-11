use crate::rift::Rift;
use crate::{Result, state_error};

use super::switch_workspace::activate_on_display;

/// Show `workspace_name` on the focused display only, for Raycast's
/// Rift extension.
///
/// `switch_workspace` picks the display the profile assigns the workspace to;
/// this picks the one the keyboard is on, so the other display keeps showing
/// whatever it shows. The per-display step brings the same fullscreen-space
/// recovery and empty-workspace acquisition the other two scopes have.
pub fn switch_workspace_focused(workspace_name: &str) -> Result<()> {
    let rift = Rift::connect()?;
    let displays = rift.displays()?;
    let focused = displays
        .iter()
        .find(|display| display.is_active_context)
        .map(|display| display.uuid.clone())
        .ok_or_else(|| state_error("no display is Rift's active context"))?;
    activate_on_display(&rift, workspace_name, &focused, displays)?;
    Ok(())
}
