//! The pre-`MoveWorkspaceToDisplay` implementation: transfer each window
//! individually and reconcile.
//!
//! Kept only as the fallback behind `RIFT_ERGO_NATIVE_WORKSPACE_MOVE=off` while
//! the native path proves itself on real hardware. Delete this file and the one
//! branch in `move_workspace_to_display` once it has.

use rift_client::{WindowData, WindowId};
use std::time::Duration;

use crate::rift::{Rift, TargetContext};
use crate::space_escape;
use crate::transaction::{EventExpectation, RiftTransaction, is_timeout};
use crate::{Result, state_error};

use super::{DisplayDirection, adjacent_display, placement};

const MAX_RECONCILIATION_ATTEMPTS: usize = 3;
const WINDOW_TRANSFER_TIMEOUT: Duration = Duration::from_secs(1);

/// Where transferred windows land on the destination.
///
/// Rift's display transfer drops a window into whatever workspace the
/// destination is showing at that moment; it does not look for the same name.
/// Showing the target workspace there first therefore makes every transfer
/// land in place. When the destination cannot be acquired up front -- it has
/// nothing to focus, or macOS did not flip the active display in time -- the
/// windows land in the destination's current workspace and are re-homed
/// afterwards, once a transferred window has given the destination focus.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Landing {
    TargetWorkspace,
    ActiveWorkspace,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MoveWorkspaceState {
    PrepareDestination,
    SubmitTransfers,
    Rehome,
    FocusOriginalWindow,
    Verify,
    Settle,
    Complete,
}

pub fn move_workspace_to_display(direction: DisplayDirection) -> Result<()> {
    let rift = Rift::connect()?;
    let displays = rift.displays()?;
    let source = rift.focused_workspace(&displays)?;
    let Some(target_display) = adjacent_display(&displays, &source.display_uuid, direction)? else {
        return Ok(());
    };
    let workspace_name = source.workspace.name.clone();
    let Some(focus_window) = source.focus_target().cloned() else {
        crate::logging::info(&format!(
            "workspace {workspace_name} has no windows; nothing to move"
        ));
        crate::notify::refused(format!("workspace {workspace_name} has no windows"));
        return Ok(());
    };

    // A display showing a native-fullscreen space has no space Rift manages, so
    // `target_context` below -- and every display-targeted command after it --
    // would fail outright. Recover it first. `source` is captured above because
    // escaping works by activating an application, which moves focus.
    let displays = space_escape::ensure_managed_space(
        &rift,
        &target_display.uuid,
        displays,
        Some(&workspace_name),
    )?;
    let target = rift.target_context(&workspace_name, target_display.uuid, &displays)?;

    let transaction = RiftTransaction::for_batch(&rift, source.workspace.windows.len())?;
    MoveWorkspaceWorkflow {
        rift: &rift,
        transaction: &transaction,
        target,
        workspace_name: &workspace_name,
        windows: &source.workspace.windows,
        focus_window: &focus_window,
    }
    .run()
}

struct MoveWorkspaceWorkflow<'a> {
    rift: &'a Rift,
    transaction: &'a RiftTransaction<'a>,
    target: TargetContext,
    workspace_name: &'a str,
    windows: &'a [WindowData],
    /// The window that owns focus at the end: the key window when it was one
    /// of ours, otherwise the workspace's first tiled window.
    focus_window: &'a WindowData,
}

impl MoveWorkspaceWorkflow<'_> {
    fn run(&self) -> Result<()> {
        let mut state = MoveWorkspaceState::PrepareDestination;
        let mut landing = Landing::ActiveWorkspace;
        let mut seeded: Option<WindowId> = None;
        let started = std::time::Instant::now();
        loop {
            crate::logging::trace(&format!(
                "phase {state:?} at {} ms",
                started.elapsed().as_millis()
            ));
            state = match state {
                MoveWorkspaceState::PrepareDestination => {
                    (landing, seeded) = self.prepare_destination()?;
                    MoveWorkspaceState::SubmitTransfers
                }
                MoveWorkspaceState::SubmitTransfers => {
                    self.submit_transfers(seeded)?;
                    MoveWorkspaceState::Rehome
                }
                MoveWorkspaceState::Rehome => {
                    // Cheap when the transfers landed in place: one membership
                    // query finds nothing pending and returns.
                    if landing == Landing::ActiveWorkspace {
                        crate::logging::info(
                            "destination was not prepared; re-homing windows after transfer",
                        );
                    }
                    self.rehome()?;
                    MoveWorkspaceState::FocusOriginalWindow
                }
                MoveWorkspaceState::FocusOriginalWindow => {
                    self.focus_original_window()?;
                    MoveWorkspaceState::Verify
                }
                MoveWorkspaceState::Verify => {
                    self.repair()?;
                    MoveWorkspaceState::Settle
                }
                MoveWorkspaceState::Settle => {
                    self.settle()?;
                    MoveWorkspaceState::Complete
                }
                MoveWorkspaceState::Complete => {
                    crate::notify::moved(format!(
                        "{} window{} to workspace {} on {}",
                        self.windows.len(),
                        if self.windows.len() == 1 { "" } else { "s" },
                        self.workspace_name,
                        self.target.display_name()
                    ));
                    return Ok(());
                }
            };
        }
    }

    /// Show the target workspace on the destination before anything moves.
    ///
    /// Returns where the transfers will land and, if the destination had to be
    /// seeded, the window already moved there. Failing to show the workspace
    /// is not an error: transfers land in the destination's current workspace
    /// and `rehome` moves them afterwards.
    fn prepare_destination(&self) -> Result<(Landing, Option<WindowId>)> {
        match placement::acquire_display(
            self.rift,
            self.transaction,
            &self.target,
            self.workspace_name,
        ) {
            Ok(()) => {}
            Err(error) if is_timeout(&error) => {
                crate::logging::info(&format!(
                    "destination has nothing to focus ({error}); seeding it"
                ));
                return self.seed_destination();
            }
            Err(error) => return Err(error),
        }
        match placement::activate_workspace(
            self.rift,
            self.transaction,
            &self.target,
            self.workspace_name,
        ) {
            Ok(()) => Ok((Landing::TargetWorkspace, None)),
            Err(error) if is_timeout(&error) => {
                crate::logging::warn(&format!(
                    "target workspace not shown before transfer: {error}"
                ));
                Ok((Landing::ActiveWorkspace, None))
            }
            Err(error) => Err(error),
        }
    }

    /// Acquire a destination that offers no window to focus by giving it one:
    /// move the window that will end up focused, focus it there, and let
    /// Rift's follow-on-move show the target workspace with it. The remaining
    /// windows then transfer into a prepared destination like any other.
    ///
    /// Transfers to a display that is not active are also slow to take
    /// effect -- Rift only arranges the destination once something makes it
    /// active -- so seeding one window and acquiring focus through it is
    /// faster than transferring everything and re-homing afterwards.
    fn seed_destination(&self) -> Result<(Landing, Option<WindowId>)> {
        let seed = self.focus_window;
        self.transaction.confirmed_step_with_timeout(
            "seed window transfer",
            self.rift.transfer_window_command(&self.target, seed),
            EventExpectation::Display(&self.target.display_uuid),
            |rift| Ok(rift.window_location(&self.target, seed.id)?.is_some()),
            WINDOW_TRANSFER_TIMEOUT,
        )?;
        self.transaction.step(
            "target display focus via seed window",
            self.rift.focus_window_command(seed),
            EventExpectation::Display(&self.target.display_uuid),
            |rift| {
                Ok(rift.display_is_active(&self.target.display_uuid)?
                    && rift.window_is_focused(seed.id)?)
            },
        )?;
        if !self
            .rift
            .window_is_at(&self.target, self.workspace_name, seed.id)?
        {
            self.transaction.step(
                "seed window follow into target workspace",
                self.rift
                    .move_window_to_workspace_command(&self.target, seed, true),
                EventExpectation::Workspace {
                    display_uuid: &self.target.display_uuid,
                    workspace_name: self.workspace_name,
                },
                |rift| {
                    Ok(rift.workspace_is_active(&self.target, self.workspace_name)?
                        && rift.window_is_at(&self.target, self.workspace_name, seed.id)?)
                },
            )?;
        } else {
            placement::activate_workspace(
                self.rift,
                self.transaction,
                &self.target,
                self.workspace_name,
            )?;
        }
        Ok((Landing::TargetWorkspace, Some(seed.id)))
    }

    /// Transfer the windows one at a time, each confirmed on the destination
    /// display by the WindowServer before the next is issued.
    ///
    /// Not batched: with several transfers queued, Rift drops the one for the
    /// focused window when it is not first, and re-ordering to put it first
    /// changes where it lands. Sequential transfers into a prepared
    /// destination take 200-500 ms each. A window still missing at its
    /// deadline is left to `rehome`, which retries it.
    fn submit_transfers(&self, already_there: Option<WindowId>) -> Result<()> {
        for window in self
            .ordered_windows()
            .filter(|window| Some(window.id) != already_there)
        {
            let result = self.transaction.confirmed_step_with_timeout(
                &format!("window {} display transfer", window.id.idx),
                self.rift.transfer_window_command(&self.target, window),
                EventExpectation::Display(&self.target.display_uuid),
                |rift| rift.window_is_on_display(&self.target, window.id),
                WINDOW_TRANSFER_TIMEOUT,
            );
            match result {
                Ok(()) => {}
                Err(error) if is_timeout(&error) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    /// Put every transferred window into the target workspace on the
    /// destination, and show that workspace there.
    ///
    /// Moving a window between workspaces is a layout command, and Rift runs
    /// those against the active display; a window that is not on that display
    /// is silently ignored. So the destination must be the active display
    /// first. By now the transferred windows live there, and Rift raised the
    /// last one it moved, so focusing our own window is a reliable way in.
    fn rehome(&self) -> Result<()> {
        let mut pending = self.pending_windows()?;
        if pending.is_empty() {
            return self.show_target_workspace();
        }
        for attempt in 1..=MAX_RECONCILIATION_ATTEMPTS {
            self.retransfer_missing_windows(&pending)?;
            self.acquire_destination_via_transferred_window()?;

            let pending_ids = pending.iter().map(|window| window.id).collect::<Vec<_>>();
            let commands = pending.iter().map(|window| {
                self.rift
                    .move_window_to_workspace_command(&self.target, window, false)
            });
            let result = self.transaction.confirmed_phase(
                &format!("workspace membership reassertion attempt {attempt}"),
                commands,
                EventExpectation::Workspace {
                    display_uuid: &self.target.display_uuid,
                    workspace_name: self.workspace_name,
                },
                |rift| rift.windows_are_at(&self.target, self.workspace_name, &pending_ids),
            );
            match result {
                Ok(()) => {}
                Err(error) if is_timeout(&error) => {}
                Err(error) => return Err(error),
            }

            pending = self.pending_windows()?;
            if pending.is_empty() {
                return self.show_target_workspace();
            }
        }
        Err(state_error(format!(
            "workspace reconciliation did not converge after {MAX_RECONCILIATION_ATTEMPTS} attempts"
        )))
    }

    /// Windows not yet both on the destination display and in the target
    /// workspace, in transfer order.
    fn pending_windows(&self) -> Result<Vec<&WindowData>> {
        let missing = self.rift.windows_missing_from_workspace(
            &self.target,
            self.workspace_name,
            &self.window_ids(),
        )?;
        Ok(self
            .ordered_windows()
            .filter(|window| missing.contains(&window.id))
            .collect())
    }

    fn acquire_destination_via_transferred_window(&self) -> Result<()> {
        if self.rift.display_is_active(&self.target.display_uuid)? {
            return Ok(());
        }
        // Any window already on the destination will do; prefer the one that
        // ends up focused so this does not cost an extra focus change later.
        let mut candidates = std::iter::once(self.focus_window).chain(self.windows.iter());
        let anchor = candidates
            .find(|window| {
                self.rift
                    .window_is_on_display(&self.target, window.id)
                    .unwrap_or(false)
            })
            .ok_or_else(|| state_error("no transferred window reached the destination"))?;
        self.transaction.step(
            "target display focus via transferred window",
            self.rift.focus_window_command(anchor),
            EventExpectation::Display(&self.target.display_uuid),
            |rift| rift.display_is_active(&self.target.display_uuid),
        )
    }

    /// Show the target workspace on the destination once its windows are in
    /// it. Rift's follow-on-move would do this too, but a plain switch is one
    /// command for the whole batch rather than one per window.
    fn show_target_workspace(&self) -> Result<()> {
        if self
            .rift
            .workspace_is_active(&self.target, self.workspace_name)?
        {
            return Ok(());
        }
        self.acquire_destination_via_transferred_window()?;
        self.transaction.step(
            "target workspace activation",
            self.rift.activate_workspace_command(&self.target),
            EventExpectation::Workspace {
                display_uuid: &self.target.display_uuid,
                workspace_name: self.workspace_name,
            },
            |rift| rift.workspace_is_active(&self.target, self.workspace_name),
        )
    }

    fn retransfer_missing_windows(&self, windows: &[&WindowData]) -> Result<()> {
        for window in windows {
            if self.rift.window_is_on_display(&self.target, window.id)? {
                continue;
            }
            let result = self.transaction.confirmed_step_with_timeout(
                &format!("window {} display retry", window.id.idx),
                self.rift.transfer_window_command(&self.target, window),
                EventExpectation::Display(&self.target.display_uuid),
                |rift| rift.window_is_on_display(&self.target, window.id),
                WINDOW_TRANSFER_TIMEOUT,
            );
            match result {
                Ok(()) => {}
                Err(error) if is_timeout(&error) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn focus_original_window(&self) -> Result<()> {
        for attempt in 1..=MAX_RECONCILIATION_ATTEMPTS {
            if self.rift.window_is_focused_at(
                &self.target,
                self.workspace_name,
                self.focus_window.id,
            )? {
                return Ok(());
            }
            if !self
                .rift
                .window_is_at(&self.target, self.workspace_name, self.focus_window.id)?
            {
                self.rehome()?;
            }
            let result = self.transaction.step(
                &format!("workspace focus restoration attempt {attempt}"),
                self.rift.focus_window_command(self.focus_window),
                EventExpectation::Workspace {
                    display_uuid: &self.target.display_uuid,
                    workspace_name: self.workspace_name,
                },
                |rift| {
                    rift.window_is_focused_at(
                        &self.target,
                        self.workspace_name,
                        self.focus_window.id,
                    )
                },
            );
            match result {
                Ok(()) => return Ok(()),
                Err(error) if is_timeout(&error) => {}
                Err(error) => return Err(error),
            }
        }
        Err(state_error(format!(
            "workspace focus restoration did not converge after {MAX_RECONCILIATION_ATTEMPTS} attempts"
        )))
    }

    /// Fix what the main phases left undone, without judging: focus in
    /// particular flaps for a moment after Rift raises transferred windows and
    /// switches the destination workspace, and only `settle` can tell a flap
    /// from a failure.
    fn repair(&self) -> Result<()> {
        if !self
            .rift
            .windows_are_at(&self.target, self.workspace_name, &self.window_ids())?
        {
            self.rehome()?;
        }
        if !self.rift.window_is_focused_at(
            &self.target,
            self.workspace_name,
            self.focus_window.id,
        )? {
            self.focus_original_window()?;
        }
        Ok(())
    }

    fn verify(&self) -> Result<()> {
        if !self
            .rift
            .windows_are_at(&self.target, self.workspace_name, &self.window_ids())?
        {
            return Err(state_error(format!(
                "not every window reached workspace {} on {}",
                self.workspace_name, self.target.display_uuid
            )));
        }
        if !self.rift.window_is_focused_at(
            &self.target,
            self.workspace_name,
            self.focus_window.id,
        )? {
            return Err(state_error("original workspace focus was not restored"));
        }
        Ok(())
    }

    fn settle(&self) -> Result<()> {
        let window_ids = self.window_ids();
        self.transaction.settle(
            "workspace placement",
            EventExpectation::Workspace {
                display_uuid: &self.target.display_uuid,
                workspace_name: self.workspace_name,
            },
            |rift| {
                Ok(
                    rift.windows_are_at(&self.target, self.workspace_name, &window_ids)?
                        && rift.window_is_focused_at(
                            &self.target,
                            self.workspace_name,
                            self.focus_window.id,
                        )?,
                )
            },
        )?;
        self.verify()
    }

    fn window_ids(&self) -> Vec<WindowId> {
        self.windows.iter().map(|window| window.id).collect()
    }

    /// Transfer order: the window that should end up focused goes last, so
    /// Rift's raise-on-transfer leaves focus where the workflow wants it.
    fn ordered_windows(&self) -> impl Iterator<Item = &WindowData> {
        self.windows
            .iter()
            .filter(|window| window.id != self.focus_window.id)
            .chain(std::iter::once(self.focus_window))
    }
}
