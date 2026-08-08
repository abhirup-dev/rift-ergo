use crate::Result;
use crate::rift::Rift;
use crate::transaction::RiftTransaction;

use super::{DisplayDirection, adjacent_display, placement};

pub fn move_window_to_display(direction: DisplayDirection) -> Result<()> {
    let rift = Rift::connect()?;
    let displays = rift.displays()?;
    let focused = rift.focused_window(&displays)?;
    let Some(target_display) =
        adjacent_display(&displays, &focused.location.display_uuid, direction)?
    else {
        return Ok(());
    };
    let (workspace_name, target) = rift.active_target_context(target_display.uuid, &displays)?;
    let transaction = RiftTransaction::new(&rift)?;
    placement::follow_window(&rift, &transaction, target, &workspace_name, focused)
}
