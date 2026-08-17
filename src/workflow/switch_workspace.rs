use rift_client::DisplayData;

use crate::Result;
use crate::policy;
use crate::rift::Rift;
use crate::space_escape;
use crate::transaction::RiftTransaction;

use super::placement;

/// Activate `workspace_name` on the display the monitor profile assigns it to,
/// replacing scripts/switch-workspace-smart.
///
/// This is `prepare_target` with the profile lookup in front of it: focus the
/// destination display, then activate the workspace there. Going through
/// rift-ergo rather than the script means workspace switching shares the
/// transaction's event-confirmed waits and the unmanaged-space recovery in
/// `space_escape`.
pub fn switch_workspace(workspace_name: &str) -> Result<()> {
    let rift = Rift::connect()?;
    let displays = rift.displays()?;
    let target_display = policy::target_display(workspace_name, &displays)?;
    activate_on_display(&rift, workspace_name, &target_display, displays)?;
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
    let displays = space_escape::ensure_managed_space(rift, display_uuid, displays)?;
    let target = rift.target_context(workspace_name, display_uuid.to_owned(), &displays)?;

    // Rift's auto-back-and-forth is disabled in config, but re-issuing a switch
    // that is already satisfied can still shift context, so do nothing instead.
    if target.display_is_active && target.workspace_is_active {
        return Ok(displays);
    }

    // Nothing is being moved here, so an empty destination workspace leaves
    // prepare_target without a focus anchor. Acquire the display first; if the
    // display is already active this is a no-op.
    space_escape::focus_display(rift, &target.display_uuid, target.space)?;

    let transaction = RiftTransaction::new(rift)?;
    placement::prepare_target(rift, &transaction, &target, workspace_name)?;
    rift.displays()
}
