use rift_client::DisplayData;

use crate::Result;
use crate::policy;
use crate::rift::Rift;
use crate::space_escape;
use crate::transaction::RiftTransaction;

use super::placement;

/// Show `workspace_name` where its windows are, replacing
/// scripts/switch-workspace-smart.
///
/// Every display that holds windows of the workspace switches to it: both
/// when the workspace is spread across both, one when it lives on one. Only a
/// workspace with no windows anywhere falls back to the display the monitor
/// profile assigns it, so an empty workspace still opens somewhere sensible.
///
/// Displays are switched with the one the keyboard is on last, so focus ends
/// there when it is among them; otherwise focus follows to the display that
/// was switched. Each per-display step shares the transaction's
/// event-confirmed waits and the unmanaged-space recovery in `space_escape`.
pub fn switch_workspace(workspace_name: &str) -> Result<()> {
    let rift = Rift::connect()?;
    let mut displays = rift.displays()?;

    let origin = displays
        .iter()
        .find(|display| display.is_active_context)
        .map(|display| display.uuid.clone());

    let mut targets: Vec<String> = rift
        .workspace_on_each_display(workspace_name, &displays)?
        .into_iter()
        .filter(|(_, workspace)| !workspace.windows.is_empty())
        .map(|(display, _)| display.uuid)
        .collect();
    if targets.is_empty() {
        targets.push(policy::target_display(workspace_name, &displays)?);
    }
    if let Some(origin) = &origin
        && let Some(position) = targets.iter().position(|uuid| uuid == origin)
    {
        let origin = targets.remove(position);
        targets.push(origin);
    }

    for uuid in &targets {
        displays = activate_on_display(&rift, workspace_name, uuid, displays)?;
    }
    Ok(())
}

/// Make `workspace_name` the active workspace on `display_uuid`, acquiring that
/// display first, and return refreshed display data.
///
/// Refreshed rather than reused because getting here can leave a fullscreen
/// space and always moves focus, both of which invalidate `displays`.
pub(super) fn activate_on_display(
    rift: &Rift,
    workspace_name: &str,
    display_uuid: &str,
    displays: Vec<DisplayData>,
) -> Result<Vec<DisplayData>> {
    let displays =
        space_escape::ensure_managed_space(rift, display_uuid, displays, Some(workspace_name))?;
    let target = rift.target_context(workspace_name, display_uuid.to_owned(), &displays)?;

    // Rift's auto-back-and-forth is disabled in config, but re-issuing a switch
    // that is already satisfied can still shift context, so do nothing instead.
    if target.display_is_active && target.workspace_is_active {
        return Ok(displays);
    }

    let transaction = RiftTransaction::new(rift)?;
    placement::prepare_target(rift, &transaction, &target, workspace_name)?;
    rift.displays()
}
