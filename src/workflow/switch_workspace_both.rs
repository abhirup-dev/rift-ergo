use crate::Result;
use crate::rift::Rift;
use crate::space_escape;
use crate::transaction::RiftTransaction;

use super::placement;

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
    let origin = space_escape::active_display_uuid(&displays);

    // Taken up front: each activation returns fresh data, and iterating that
    // while it is being replaced would borrow it for the whole loop.
    let mut uuids = displays
        .iter()
        .map(|display| display.uuid.clone())
        .collect::<Vec<_>>();
    // Recover the other displays first. Finishing on the origin avoids an
    // extra focus round trip and reduces fullscreen app-activation churn.
    if let Some(origin) = &origin
        && let Some(position) = uuids.iter().position(|uuid| uuid == origin)
    {
        let origin = uuids.remove(position);
        uuids.push(origin);
    }

    for uuid in &uuids {
        displays = activate_on_display(&rift, workspace_name, uuid, displays)?;
    }
    crate::notify::switched(format!("workspace {workspace_name} on every display"));

    let Some(origin) = origin else {
        return Ok(());
    };
    // A no-op when the origin was the last display activated.
    let target = rift.target_context(workspace_name, origin, &displays)?;
    if target.display_is_active && target.workspace_is_active {
        return Ok(());
    }
    let transaction = RiftTransaction::new(&rift)?;
    placement::prepare_target(&rift, &transaction, &target, workspace_name)
}
