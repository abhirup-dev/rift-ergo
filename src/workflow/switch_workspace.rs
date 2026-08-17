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
    let displays = space_escape::ensure_managed_space(&rift, &target_display, displays)?;
    let target = rift.target_context(workspace_name, target_display, &displays)?;

    // Rift's auto-back-and-forth is disabled in config, but re-issuing a switch
    // that is already satisfied can still shift context, so do nothing instead.
    if target.display_is_active && target.workspace_is_active {
        return Ok(());
    }

    // Nothing is being moved here, so an empty destination workspace leaves
    // prepare_target without a focus anchor. Acquire the display first; if the
    // display is already active this is a no-op.
    space_escape::focus_display(&rift, &target.display_uuid, target.space)?;

    let transaction = RiftTransaction::new(&rift)?;
    placement::prepare_target(&rift, &transaction, &target, workspace_name)
}
