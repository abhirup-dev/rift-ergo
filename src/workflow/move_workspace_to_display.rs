use rift_client::{WindowData, WindowId};
use std::io;
use std::time::Duration;

use crate::rift::{Rift, TargetContext};
use crate::space_escape;
use crate::transaction::{EventExpectation, RiftTransaction};
use crate::{Result, state_error};

use super::{DisplayDirection, adjacent_display, placement};

const MAX_RECONCILIATION_ATTEMPTS: usize = 3;
const SINGLETON_TRANSFER_ATTEMPTS: usize = 2;
const SINGLETON_FAST_TRANSFER_TIMEOUT: Duration = Duration::from_millis(250);
const SINGLETON_FOCUS_TIMEOUT: Duration = Duration::from_millis(500);
const WINDOW_TRANSFER_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MoveWorkspaceState {
    PrepareDestination,
    SubmitTransfers,
    ReassertWorkspace,
    FocusOriginalWindow,
    Verify,
    Settle,
    RestoreOrder,
    Complete,
}

pub fn move_workspace_to_display(direction: DisplayDirection) -> Result<()> {
    let rift = Rift::connect()?;
    let displays = rift.displays()?;
    let source = rift.focused_workspace(&displays)?;
    // Captured before anything moves, while the source workspace is still the
    // focused one. Rift preserves workspace membership across a display
    // transfer but not the tiled arrangement, so the order is replayed at the
    // end from this snapshot. Failing to read it only forfeits the refinement.
    let source_order = rift.tiled_order(None, None).unwrap_or_else(|error| {
        crate::logging::warn(&format!("source visual order unavailable: {error}"));
        Vec::new()
    });
    let Some(target_display) = adjacent_display(&displays, &source.display_uuid, direction)? else {
        return Ok(());
    };
    let workspace_name = source.workspace.name.clone();

    // A display showing a native-fullscreen space has no space Rift manages, so
    // `target_context` below -- and every display-targeted command after it --
    // would fail outright. Recover it first. `source` is captured above because
    // escaping works by activating an application, which moves focus.
    let displays = space_escape::ensure_managed_space(&rift, &target_display.uuid, displays)?;

    let mut target =
        rift.target_context(&workspace_name, target_display.uuid.clone(), &displays)?;

    // The deadlock `placement`'s SeedDestination handles, reached by the other
    // route: `prepare_target` focuses the destination by focusing a window
    // there, and an inactive display whose active workspace is empty offers
    // none. Seeding is not available here because `prepare_target` runs before
    // any window moves, so acquire the display up front instead.
    if !target.display_is_active && target.focus_anchor.is_none() {
        space_escape::focus_display(&rift, &target.display_uuid, target.space)?;
    }

    if source.workspace.windows.len() == 1 {
        let transaction = RiftTransaction::new(&rift)?;
        let fast_result = move_singleton(
            &rift,
            &transaction,
            &target,
            &workspace_name,
            &source.focused_window,
        );
        match fast_result {
            Ok(()) => return Ok(()),
            Err(error) if !is_timeout(&error) => return Err(error),
            Err(_) => {
                let refreshed_displays = rift.displays()?;
                target =
                    rift.target_context(&workspace_name, target_display.uuid, &refreshed_displays)?;
            }
        }
    }

    let transaction = RiftTransaction::for_batch(&rift, source.workspace.windows.len())?;
    MoveWorkspaceWorkflow {
        rift: &rift,
        transaction: &transaction,
        target,
        workspace_name: &workspace_name,
        windows: &source.workspace.windows,
        focused_window: &source.focused_window,
        source_order: &source_order,
    }
    .run()
}

fn move_singleton(
    rift: &Rift,
    transaction: &RiftTransaction<'_>,
    target: &TargetContext,
    workspace_name: &str,
    window: &WindowData,
) -> Result<()> {
    placement::prepare_target(rift, transaction, target, workspace_name)?;

    let mut transferred = false;
    for attempt in 1..=SINGLETON_TRANSFER_ATTEMPTS {
        let timeout = if attempt == 1 {
            SINGLETON_FAST_TRANSFER_TIMEOUT
        } else {
            WINDOW_TRANSFER_TIMEOUT
        };
        let result = transaction.confirmed_step_with_timeout(
            &format!("singleton physical transfer attempt {attempt}"),
            rift.transfer_window_command(target, window),
            EventExpectation::Workspace {
                display_uuid: &target.display_uuid,
                workspace_name,
            },
            |rift| {
                Ok(rift.window_is_at(target, workspace_name, window.id)?
                    && rift.window_is_on_display(target, window.id)?)
            },
            timeout,
        );
        match result {
            Ok(()) => {
                transferred = true;
                break;
            }
            Err(error) if is_timeout(&error) && attempt < SINGLETON_TRANSFER_ATTEMPTS => {}
            Err(error) => return Err(error),
        }
    }
    if !transferred {
        return Err(state_error("singleton transfer attempts were exhausted"));
    }

    let focus_result = transaction.confirmed_step_with_timeout(
        "singleton focus",
        rift.focus_window_command(window),
        EventExpectation::Workspace {
            display_uuid: &target.display_uuid,
            workspace_name,
        },
        |rift| rift.window_is_focused_at(target, workspace_name, window.id),
        SINGLETON_FOCUS_TIMEOUT,
    );
    match focus_result {
        Ok(()) => {}
        Err(error) if is_timeout(&error) => {
            transaction.confirmed_step_with_timeout(
                "singleton focus recovery",
                rift.transfer_window_command(target, window),
                EventExpectation::Workspace {
                    display_uuid: &target.display_uuid,
                    workspace_name,
                },
                |rift| {
                    Ok(
                        rift.window_is_focused_at(target, workspace_name, window.id)?
                            && rift.window_is_on_display(target, window.id)?,
                    )
                },
                WINDOW_TRANSFER_TIMEOUT,
            )?;
        }
        Err(error) => return Err(error),
    }

    placement::verify_target(rift, target, workspace_name, window)
}

struct MoveWorkspaceWorkflow<'a> {
    rift: &'a Rift,
    transaction: &'a RiftTransaction<'a>,
    target: TargetContext,
    workspace_name: &'a str,
    windows: &'a [WindowData],
    focused_window: &'a WindowData,
    source_order: &'a [WindowId],
}

impl MoveWorkspaceWorkflow<'_> {
    fn run(&self) -> Result<()> {
        let mut state = MoveWorkspaceState::PrepareDestination;
        loop {
            state = match state {
                MoveWorkspaceState::PrepareDestination => {
                    placement::prepare_target(
                        self.rift,
                        self.transaction,
                        &self.target,
                        self.workspace_name,
                    )?;
                    MoveWorkspaceState::SubmitTransfers
                }
                MoveWorkspaceState::SubmitTransfers => {
                    self.submit_transfers()?;
                    MoveWorkspaceState::ReassertWorkspace
                }
                MoveWorkspaceState::ReassertWorkspace => {
                    self.reassert_workspace()?;
                    MoveWorkspaceState::FocusOriginalWindow
                }
                MoveWorkspaceState::FocusOriginalWindow => {
                    self.focus_original_window()?;
                    MoveWorkspaceState::Verify
                }
                MoveWorkspaceState::Verify => {
                    self.verify()?;
                    MoveWorkspaceState::Settle
                }
                MoveWorkspaceState::Settle => {
                    self.settle()?;
                    MoveWorkspaceState::RestoreOrder
                }
                MoveWorkspaceState::RestoreOrder => {
                    // Visual order is a refinement, not part of the move's
                    // contract: membership and focus are already confirmed.
                    // A failure here must not fail the whole workflow.
                    if let Err(error) = self.restore_visual_order() {
                        crate::logging::warn(&format!("visual order not restored: {error}"));
                    }
                    MoveWorkspaceState::Complete
                }
                MoveWorkspaceState::Complete => return Ok(()),
            };
        }
    }

    fn submit_transfers(&self) -> Result<()> {
        for window in self.ordered_windows() {
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

    fn focus_original_window(&self) -> Result<()> {
        for attempt in 1..=MAX_RECONCILIATION_ATTEMPTS {
            if self.rift.window_is_focused_at(
                &self.target,
                self.workspace_name,
                self.focused_window.id,
            )? {
                return Ok(());
            }
            if !self
                .rift
                .window_is_at(&self.target, self.workspace_name, self.focused_window.id)?
            {
                self.reassert_workspace()?;
            }
            let result = self.transaction.step(
                &format!("workspace focus restoration attempt {attempt}"),
                self.rift.focus_window_command(self.focused_window),
                EventExpectation::Workspace {
                    display_uuid: &self.target.display_uuid,
                    workspace_name: self.workspace_name,
                },
                |rift| {
                    rift.window_is_focused_at(
                        &self.target,
                        self.workspace_name,
                        self.focused_window.id,
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

    fn reassert_workspace(&self) -> Result<()> {
        let missing = self.rift.windows_missing_from_workspace(
            &self.target,
            self.workspace_name,
            &self.window_ids(),
        )?;
        let mut pending = self.pending_windows(&missing);
        if pending.is_empty() {
            return Ok(());
        }
        for attempt in 1..=MAX_RECONCILIATION_ATTEMPTS {
            self.retransfer_missing_windows(&pending)?;
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
                Ok(())
                    if self.rift.windows_are_at(
                        &self.target,
                        self.workspace_name,
                        &self.window_ids(),
                    )? =>
                {
                    return Ok(());
                }
                Ok(()) => {}
                Err(error) if is_timeout(&error) => {}
                Err(error) => return Err(error),
            }

            let missing = self.rift.windows_missing_from_workspace(
                &self.target,
                self.workspace_name,
                &self.window_ids(),
            )?;
            pending = self.pending_windows(&missing);
            if pending.is_empty() {
                return Ok(());
            }
        }

        Err(state_error(format!(
            "workspace reconciliation did not converge after {MAX_RECONCILIATION_ATTEMPTS} attempts"
        )))
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

    fn verify(&self) -> Result<()> {
        let window_ids = self.window_ids();
        if !self
            .rift
            .windows_are_at(&self.target, self.workspace_name, &window_ids)?
        {
            self.reassert_workspace()?;
        }
        if !self.rift.window_is_focused_at(
            &self.target,
            self.workspace_name,
            self.focused_window.id,
        )? {
            self.focus_original_window()?;
        }
        if !self
            .rift
            .windows_are_at(&self.target, self.workspace_name, &window_ids)?
        {
            return Err(state_error(format!(
                "not every window reached workspace {} on {}",
                self.workspace_name, self.target.display_uuid
            )));
        }
        if !self.rift.window_is_focused_at(
            &self.target,
            self.workspace_name,
            self.focused_window.id,
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
                            self.focused_window.id,
                        )?,
                )
            },
        )?;
        self.verify()
    }

    /// Replays the source workspace's tiled order onto the destination.
    ///
    /// Rift's display transfer preserves membership and focus but reinserts
    /// windows in arrival order. Stable `node_id`s and a normalized container
    /// tree (Rift 0.5.8.1) make the source order replayable as a permutation:
    /// selection sort over the destination order, at most `n - 1` swaps.
    ///
    /// Only the windows this workflow moved participate, so windows that
    /// already lived in the destination workspace are never disturbed.
    fn restore_visual_order(&self) -> Result<()> {
        if restore_order_disabled() {
            return Ok(());
        }

        let moved = self.window_ids();
        let desired: Vec<WindowId> = self
            .source_order
            .iter()
            .copied()
            .filter(|id| moved.contains(id))
            .collect();
        if desired.len() < 2 {
            return Ok(());
        }

        let mut actual: Vec<WindowId> = self
            .rift
            .tiled_order(Some(self.target.space), Some(self.target.workspace_index))?
            .into_iter()
            .filter(|id| moved.contains(id))
            .collect();

        // A destination that does not hold exactly the moved set means the
        // transfer landed differently than assumed; reordering it would be
        // guesswork, so leave the layout alone and say so.
        if actual.len() != desired.len() {
            crate::logging::warn(&format!(
                "visual order skipped: destination has {} of {} moved windows",
                actual.len(),
                desired.len()
            ));
            return Ok(());
        }

        let swaps = order_swaps(&desired, &mut actual);
        for (first, second) in &swaps {
            self.rift
                .execute(self.rift.swap_windows_command(*first, *second))?;
        }

        if !swaps.is_empty() {
            crate::logging::info(&format!(
                "restored visual order for {} windows in {} swaps",
                desired.len(),
                swaps.len()
            ));
        }
        Ok(())
    }

    fn window_ids(&self) -> Vec<WindowId> {
        self.windows.iter().map(|window| window.id).collect()
    }

    fn ordered_windows(&self) -> impl Iterator<Item = &WindowData> {
        self.windows
            .iter()
            .filter(|window| window.id != self.focused_window.id)
            .chain(std::iter::once(self.focused_window))
    }

    fn pending_windows(&self, window_ids: &[WindowId]) -> Vec<&WindowData> {
        self.ordered_windows()
            .filter(|window| window_ids.contains(&window.id))
            .collect()
    }
}

/// Whether order restoration is switched off.
///
/// Rift invokes bindings as an argv array with no shell, so an environment
/// variable cannot be set per binding. The sentinel file is the switch that is
/// actually reachable from a keybinding-driven setup: create it to disable,
/// remove it to re-enable, no rebuild and no config edit.
fn restore_order_disabled() -> bool {
    if std::env::var_os("RIFT_ERGO_RESTORE_ORDER").is_some_and(|value| value == "0") {
        return true;
    }
    std::env::var_os("HOME").is_some_and(|home| {
        std::path::Path::new(&home)
            .join(".config/rift/rift-ergo.no-restore-order")
            .exists()
    })
}

/// Swaps that transform `actual` into `desired`, as selection sort.
///
/// Both slices must hold the same set of ids. `actual` is advanced in step so
/// each emitted pair reflects the tree state Rift will see when that swap is
/// applied, which keeps the command sequence valid without re-querying.
fn order_swaps(desired: &[WindowId], actual: &mut [WindowId]) -> Vec<(WindowId, WindowId)> {
    let mut swaps = Vec::new();
    for index in 0..desired.len() {
        if actual[index] == desired[index] {
            continue;
        }
        let Some(found) = (index + 1..actual.len()).find(|&at| actual[at] == desired[index]) else {
            continue;
        };
        swaps.push((actual[index], actual[found]));
        actual.swap(index, found);
    }
    swaps
}

fn is_timeout(error: &crate::DynError) -> bool {
    error
        .downcast_ref::<io::Error>()
        .is_some_and(|error| error.kind() == io::ErrorKind::TimedOut)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(idx: u32) -> WindowId {
        WindowId { pid: 1, idx }
    }

    fn ids(indices: &[u32]) -> Vec<WindowId> {
        indices.iter().copied().map(window).collect()
    }

    #[test]
    fn an_already_correct_order_needs_no_swaps() {
        let desired = ids(&[1, 2, 3]);
        let mut actual = ids(&[1, 2, 3]);
        assert!(order_swaps(&desired, &mut actual).is_empty());
    }

    #[test]
    fn swaps_transform_actual_into_desired() {
        let desired = ids(&[1, 2, 3, 4]);
        let mut actual = ids(&[3, 4, 1, 2]);
        let swaps = order_swaps(&desired, &mut actual);
        assert_eq!(actual, desired);
        assert!(swaps.len() < desired.len());
    }

    #[test]
    fn a_reversed_order_is_restored() {
        let desired = ids(&[1, 2, 3, 4, 5]);
        let mut actual = ids(&[5, 4, 3, 2, 1]);
        order_swaps(&desired, &mut actual);
        assert_eq!(actual, desired);
    }

    #[test]
    fn a_single_transposition_costs_one_swap() {
        let desired = ids(&[1, 2, 3]);
        let mut actual = ids(&[2, 1, 3]);
        let swaps = order_swaps(&desired, &mut actual);
        assert_eq!(swaps, vec![(window(2), window(1))]);
        assert_eq!(actual, desired);
    }
}
