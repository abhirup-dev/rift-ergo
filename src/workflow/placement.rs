use rift_client::WindowData;

use crate::rift::{FocusedWindow, Rift, TargetContext};
use crate::transaction::{EventExpectation, RiftTransaction};
use crate::{Result, state_error};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CrossDisplayState {
    PrepareDestination,
    SeedDestination,
    TransferWindow,
    FollowTransferredWindow,
    FocusTransferredWindow,
    Verify,
    Complete,
}

pub(super) fn follow_window(
    rift: &Rift,
    transaction: &RiftTransaction<'_>,
    target: TargetContext,
    workspace_name: &str,
    focused: FocusedWindow,
) -> Result<()> {
    if focused.location.display_uuid == target.display_uuid {
        move_within_display(rift, transaction, &target, workspace_name, &focused.window)
    } else {
        CrossDisplayWorkflow {
            rift,
            transaction,
            target,
            workspace_name,
            window: focused.window,
        }
        .run()
    }
}

pub(super) fn already_at_target(
    focused: &FocusedWindow,
    target: &TargetContext,
    workspace_name: &str,
) -> bool {
    focused.location.display_uuid == target.display_uuid
        && focused.location.workspace_name == workspace_name
        && focused.location.is_focused
}

pub(super) fn prepare_target(
    rift: &Rift,
    transaction: &RiftTransaction<'_>,
    target: &TargetContext,
    workspace_name: &str,
) -> Result<()> {
    if !target.display_is_active {
        transaction.step(
            "target display focus",
            rift.focus_destination_command(target),
            EventExpectation::Display(&target.display_uuid),
            |rift| rift.display_is_active(&target.display_uuid),
        )?;
    }
    if !target.workspace_is_active {
        transaction.step(
            "target workspace activation",
            rift.activate_workspace_command(target),
            EventExpectation::Workspace {
                display_uuid: &target.display_uuid,
                workspace_name,
            },
            |rift| rift.workspace_is_active(target, workspace_name),
        )?;
    }
    Ok(())
}

struct CrossDisplayWorkflow<'a> {
    rift: &'a Rift,
    transaction: &'a RiftTransaction<'a>,
    target: TargetContext,
    workspace_name: &'a str,
    window: WindowData,
}

impl CrossDisplayWorkflow<'_> {
    fn run(&self) -> Result<()> {
        let mut state = CrossDisplayState::PrepareDestination;
        loop {
            state = match state {
                CrossDisplayState::PrepareDestination => {
                    if self.destination_has_no_focus_anchor() {
                        CrossDisplayState::SeedDestination
                    } else {
                        prepare_target(
                            self.rift,
                            self.transaction,
                            &self.target,
                            self.workspace_name,
                        )?;
                        CrossDisplayState::TransferWindow
                    }
                }
                CrossDisplayState::SeedDestination => {
                    self.seed_destination()?;
                    CrossDisplayState::FollowTransferredWindow
                }
                CrossDisplayState::TransferWindow => {
                    self.transfer_window()?;
                    CrossDisplayState::FollowTransferredWindow
                }
                CrossDisplayState::FollowTransferredWindow => {
                    self.follow_transferred_window()?;
                    CrossDisplayState::FocusTransferredWindow
                }
                CrossDisplayState::FocusTransferredWindow => {
                    self.focus_transferred_window()?;
                    CrossDisplayState::Verify
                }
                CrossDisplayState::Verify => {
                    verify_target(self.rift, &self.target, self.workspace_name, &self.window)?;
                    CrossDisplayState::Complete
                }
                CrossDisplayState::Complete => return Ok(()),
            };
        }
    }

    /// Rift can only focus a display by focusing a window on it. When the
    /// destination's active workspace is empty there is nothing to focus, and
    /// the MoveMouseToDisplay fallback is inert unless focus_follows_mouse is
    /// enabled, so `prepare_target` would time out.
    fn destination_has_no_focus_anchor(&self) -> bool {
        !self.target.display_is_active && self.target.focus_anchor.is_none()
    }

    /// Break that deadlock by transferring the window before acquiring focus:
    /// MoveWindowToDisplay takes an explicit display selector and does not
    /// require the destination to be active. The transferred window then serves
    /// as the anchor, and the ordinary follow/focus steps finish the move —
    /// including the workspace activation `prepare_target` was skipped for.
    fn seed_destination(&self) -> Result<()> {
        self.transaction.step(
            "window transfer to unfocusable display",
            self.rift
                .transfer_window_command(&self.target, &self.window),
            EventExpectation::Display(&self.target.display_uuid),
            |rift| rift.window_is_on_display(&self.target, self.window.id),
        )?;
        self.transaction.step(
            "target display focus via transferred window",
            self.rift.focus_window_command(&self.window),
            EventExpectation::Display(&self.target.display_uuid),
            |rift| rift.display_is_active(&self.target.display_uuid),
        )
    }

    fn transfer_window(&self) -> Result<()> {
        self.transaction.step(
            "window transfer",
            self.rift
                .transfer_window_command(&self.target, &self.window),
            EventExpectation::Workspace {
                display_uuid: &self.target.display_uuid,
                workspace_name: self.workspace_name,
            },
            |rift| rift.window_is_at(&self.target, self.workspace_name, self.window.id),
        )
    }

    fn follow_transferred_window(&self) -> Result<()> {
        self.transaction.step(
            "transferred window follow",
            self.rift
                .move_within_display_command(&self.target, &self.window),
            EventExpectation::Workspace {
                display_uuid: &self.target.display_uuid,
                workspace_name: self.workspace_name,
            },
            |rift| {
                Ok(rift.workspace_is_active(&self.target, self.workspace_name)?
                    && rift.window_is_at(&self.target, self.workspace_name, self.window.id)?)
            },
        )
    }

    fn focus_transferred_window(&self) -> Result<()> {
        if self
            .rift
            .window_is_focused_at(&self.target, self.workspace_name, self.window.id)?
        {
            return Ok(());
        }
        self.transaction.step(
            "transferred window focus",
            self.rift.focus_window_command(&self.window),
            EventExpectation::Workspace {
                display_uuid: &self.target.display_uuid,
                workspace_name: self.workspace_name,
            },
            |rift| rift.window_is_focused_at(&self.target, self.workspace_name, self.window.id),
        )
    }
}

fn move_within_display(
    rift: &Rift,
    transaction: &RiftTransaction<'_>,
    target: &TargetContext,
    workspace_name: &str,
    window: &WindowData,
) -> Result<()> {
    transaction.step(
        "window focus in target workspace",
        rift.move_within_display_command(target, window),
        EventExpectation::Workspace {
            display_uuid: &target.display_uuid,
            workspace_name,
        },
        |rift| rift.window_is_focused_at(target, workspace_name, window.id),
    )?;
    verify_target(rift, target, workspace_name, window)
}

pub(super) fn verify_target(
    rift: &Rift,
    target: &TargetContext,
    workspace_name: &str,
    window: &WindowData,
) -> Result<()> {
    let location = rift
        .window_location(target, window.id)?
        .ok_or_else(|| state_error("window disappeared after move"))?;
    if location.display_uuid != target.display_uuid
        || location.workspace_name != workspace_name
        || !location.is_focused
    {
        return Err(state_error(format!(
            "final placement mismatch: expected {workspace_name} on {}, got {} on {}",
            target.display_uuid, location.workspace_name, location.display_uuid
        )));
    }
    Ok(())
}
