use std::io;
use std::time::Duration;

use rift_client::WindowData;

use crate::rift::{FocusedWindow, Rift, TargetContext};
use crate::space_escape;
use crate::transaction::{EventExpectation, RiftTransaction, is_timeout};
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
    // The single shared point where one window changes place, so every command
    // that moves one window reports in the same words.
    let description = format!(
        "{} to workspace {workspace_name} on {}",
        focused.window.app_name.as_deref().unwrap_or("window"),
        target.display_name()
    );
    let result = if focused.location.display_uuid == target.display_uuid {
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
    };
    if result.is_ok() {
        crate::notify::moved(description);
    }
    result
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

/// How long to give a window raise to make the destination the active
/// display. Measured at 60-430 ms when it works; when it does not, more
/// waiting does not help and the next method should be tried.
const ANCHOR_FOCUS_TIMEOUT: Duration = Duration::from_millis(1_000);
/// A desktop click flips the primary display within a few hundred
/// milliseconds; Rift can take most of a second more to notice under load.
const CLICK_FOCUS_TIMEOUT: Duration = Duration::from_millis(1_000);
/// Focusing a hidden window flips the display in ~250 ms; Rift then takes up
/// to a second more to bring that window's workspace forward.
const HIDDEN_WINDOW_FOCUS_TIMEOUT: Duration = Duration::from_millis(2_500);

pub(super) fn prepare_target(
    rift: &Rift,
    transaction: &RiftTransaction<'_>,
    target: &TargetContext,
    workspace_name: &str,
) -> Result<()> {
    acquire_display(rift, transaction, target, workspace_name)?;
    activate_workspace(rift, transaction, target, workspace_name)
}

/// Make the destination display Rift's command context.
///
/// Every layout command -- switching a workspace, moving a window into one --
/// runs against the display macOS reports as active, and Rift learns that
/// from macOS rather than deciding it. So a display is acquired by giving it
/// the key window, in order of how little else it disturbs:
///
/// 1. Focus a window visible in the display's active workspace. Nothing else
///    changes on screen.
/// 2. Focus a window Rift manages in `workspace_name` on that display. It is
///    hidden, so nothing shows, and Rift will bring that workspace forward --
///    which is where the caller is going anyway.
/// 3. Click the display's desktop. Works on the primary display only.
/// 4. Focus any window Rift manages on that display and wait for Rift to
///    bring its workspace forward; the caller's switch then replaces it. The
///    other workspace shows for a moment, so this comes last.
///
/// Each step confirms both the display flip and, where a window was raised,
/// that Rift reports it focused: the two arrive by separate macOS
/// notifications, and switching workspaces between them makes Rift switch
/// straight back to the raised window's workspace.
///
/// Fails with a timeout when nothing worked, which callers that can move a
/// window to the display first treat as "seed it, then focus that window".
pub(super) fn acquire_display(
    rift: &Rift,
    transaction: &RiftTransaction<'_>,
    target: &TargetContext,
    workspace_name: &str,
) -> Result<()> {
    if target.display_is_active {
        return Ok(());
    }
    let display_active = |rift: &Rift| rift.display_is_active(&target.display_uuid);

    let hidden_anchor = if target.focus_anchor.is_none() {
        rift.hidden_anchor(target, workspace_name)?
    } else {
        None
    };
    let anchors = target.focus_anchor.iter().chain(hidden_anchor.iter());
    for (anchor, description) in anchors.zip([
        "target display focus",
        "target display focus via hidden window",
    ]) {
        let result = transaction.confirmed_step_with_timeout(
            description,
            rift.focus_window_command(anchor),
            EventExpectation::Display(&target.display_uuid),
            |rift| Ok(display_active(rift)? && rift.window_is_focused(anchor.id)?),
            ANCHOR_FOCUS_TIMEOUT,
        );
        match result {
            Ok(()) => return Ok(()),
            Err(error) if is_timeout(&error) => {}
            Err(error) => return Err(error),
        }
    }

    if is_primary_display(target.frame) {
        space_escape::click_display(target.frame);
        let result = transaction.await_state(
            "target display focus via desktop click",
            EventExpectation::Display(&target.display_uuid),
            display_active,
            CLICK_FOCUS_TIMEOUT,
        );
        match result {
            Ok(()) => return Ok(()),
            Err(error) if is_timeout(&error) => {}
            Err(error) => return Err(error),
        }
    }

    let Some(any_window) = rift.any_hidden_window(target)? else {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "timed out waiting for target display focus: nothing on the display can take focus",
        )
        .into());
    };
    transaction.confirmed_step_with_timeout(
        "target display focus via hidden window",
        rift.focus_window_command(&any_window),
        EventExpectation::Display(&target.display_uuid),
        |rift| Ok(display_active(rift)? && rift.active_workspace_has_focus(target)?),
        HIDDEN_WINDOW_FOCUS_TIMEOUT,
    )
}

/// The display whose top-left corner is the global origin: macOS's main
/// display, the one whose desktop takes focus on a click. Rift reports the
/// visible frame, so the origin sits just below the menu bar.
fn is_primary_display(frame: rift_client::Rect) -> bool {
    frame.origin.x == 0.0 && (0.0..64.0).contains(&frame.origin.y)
}

/// Show `workspace_name` on the destination. Only valid once the destination
/// is the active display: Rift resolves the workspace index against that
/// display, so issuing this earlier would switch whichever display is active.
pub(super) fn activate_workspace(
    rift: &Rift,
    transaction: &RiftTransaction<'_>,
    target: &TargetContext,
    workspace_name: &str,
) -> Result<()> {
    // Checked live rather than from `target`: acquiring the display may have
    // changed what it shows, when Rift brought a raised window's workspace
    // forward, and switching on a stale answer sends the next transfers into
    // the wrong workspace.
    if !rift.workspace_is_active(target, workspace_name)? {
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
