use crate::Result;
use crate::policy;
use crate::rift::Rift;
use crate::space_escape;
use crate::transaction::RiftTransaction;

use super::placement;

pub fn move_follow(workspace_name: &str) -> Result<()> {
    let rift = Rift::connect()?;
    let displays = rift.displays()?;
    let target_display = policy::target_display(workspace_name, &displays)?;
    // Capture the focused window before escaping: leaving a fullscreen space
    // works by activating another application, which moves focus.
    let focused = rift.focused_window(&displays)?;
    let displays = space_escape::ensure_managed_space(&rift, &target_display, displays)?;
    let target = rift.target_context(workspace_name, target_display, &displays)?;

    if placement::already_at_target(&focused, &target, workspace_name) {
        return Ok(());
    }

    let transaction = RiftTransaction::new(&rift)?;
    placement::follow_window(&rift, &transaction, target, workspace_name, focused)
}
