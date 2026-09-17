//! Move the focused workspace to the adjacent display.
//!
//! Rift's own `MoveWorkspaceToDisplay` does the relocation: it moves the layout
//! tree of the source display's active workspace into the destination's
//! workspace at the same ordinal, shows it there, and raises the windows. That
//! is one command and roughly 150-400 ms, against several seconds for the
//! per-window transfer-and-reconcile loop it replaces, which survives as
//! `move_workspace_legacy` behind an environment switch.
//!
//! What remains here is everything the command does not do, and every way it
//! can decline to act without saying so.

use std::time::Duration;

use rift_client::{WindowData, WindowId};

use crate::rift::{Rift, TargetContext};
use crate::transaction::{EventExpectation, RiftTransaction, is_timeout};
use crate::{Result, notify, space_escape, state_error};

use super::{DisplayDirection, adjacent_display, move_workspace_legacy, placement};

const ACQUIRE_TIMEOUT: Duration = Duration::from_millis(1_500);
const MOVE_TIMEOUT: Duration = Duration::from_millis(3_000);
const FOCUS_ATTEMPTS: usize = 3;
const REPAIR_ATTEMPTS: usize = 3;
const REPAIR_TIMEOUT: Duration = Duration::from_millis(1_500);

pub fn move_workspace_to_display(direction: DisplayDirection) -> Result<()> {
    if !native_enabled() {
        return move_workspace_legacy::move_workspace_to_display(direction);
    }

    let rift = Rift::connect()?;
    let displays = rift.displays()?;

    // Capture before anything else. Every step below can move focus, and the
    // native command reads its source from whichever display is active at the
    // moment it runs -- so the answer to "which workspace am I moving?" has to
    // be a value already in hand, not a question asked later.
    let source = rift.focused_workspace(&displays)?;
    let Some(target_display) = adjacent_display(&displays, &source.display_uuid, direction)? else {
        return Ok(());
    };
    let source_display_uuid = source.display_uuid.clone();
    let workspace_name = source.workspace.name.clone();
    let windows: Vec<WindowData> = source.workspace.windows.clone();
    let window_ids: Vec<WindowId> = windows.iter().map(|window| window.id).collect();
    let Some(focus_window) = source.focus_target().cloned() else {
        crate::logging::info(&format!(
            "workspace {workspace_name} has no windows; nothing to move"
        ));
        notify::refused(format!("workspace {workspace_name} has no windows"));
        return Ok(());
    };

    // Both displays must be showing a space Rift manages: the command reads the
    // source through `workspace_command_space`, which is the active display's
    // space filtered by "is this space active", and it requires the same of the
    // destination. A native-fullscreen space on either end is a silent no-op.
    // Escaping works by activating an application, so it runs before the source
    // display is bound, never after.
    let displays = space_escape::ensure_managed_space(
        &rift,
        &target_display.uuid,
        displays,
        Some(&workspace_name),
    )?;
    let displays = space_escape::ensure_managed_space(
        &rift,
        &source_display_uuid,
        displays,
        Some(&workspace_name),
    )?;

    let target = rift.target_context(&workspace_name, target_display.uuid, &displays)?;
    let source_context =
        rift.target_context(&workspace_name, source_display_uuid.clone(), &displays)?;
    assert_same_ordinal(&source_context, &target, &workspace_name)?;

    let transaction = RiftTransaction::new(&rift)?;

    // The command moves the active workspace of the active display. Binding the
    // source is what makes "the workspace I am looking at" and "the workspace
    // Rift will move" the same thing.
    transaction.step(
        "source display focus",
        rift.focus_window_command(&focus_window),
        EventExpectation::Display(&source_display_uuid),
        |rift| rift.display_is_active(&source_display_uuid),
    )?;

    // Rift moves the workspace that is *showing* on the active display, not the
    // one holding the key window. Those differ whenever focus sits in a hidden
    // workspace -- which is exactly what focusing a window in one produces, and
    // the same gap `on-focus-change` exists to close. Without this step the
    // command silently relocates whatever happened to be on screen instead.
    if !rift.workspace_is_active(&source_context, &workspace_name)? {
        transaction.step(
            "source workspace activation",
            rift.activate_workspace_command(&source_context),
            EventExpectation::Workspace {
                display_uuid: &source_display_uuid,
                workspace_name: &workspace_name,
            },
            |rift| rift.workspace_is_active(&source_context, &workspace_name),
        )?;
    }

    // Focus is not durable: measured over twelve cross-display focuses, two had
    // been taken back by another application within 1.25 s. Re-reading here, as
    // late as possible, is the difference between refusing and relocating the
    // other display's workspace.
    confirm_source(&rift, &source_context, &workspace_name)?;

    let moved = transaction.confirmed_step_with_timeout(
        "native workspace move",
        rift.move_workspace_to_display_command(&target),
        EventExpectation::Workspace {
            display_uuid: &target.display_uuid,
            workspace_name: &workspace_name,
        },
        |rift| rift.windows_are_at(&target, &workspace_name, &window_ids),
        MOVE_TIMEOUT,
    );
    match moved {
        Ok(()) => {}
        // The command relocates the workspace as a unit, but occasionally a
        // window does not come along -- measured once in 24 moves, and the
        // straggler was never one of the terminal windows. Finishing the job by
        // hand is cheaper than failing and leaving the workspace split across
        // two displays, which is the one outcome worse than not moving at all.
        Err(error) if is_timeout(&error) => {
            repair_stragglers(&rift, &transaction, &target, &workspace_name, &windows)?
        }
        Err(error) => return Err(error),
    }

    // Rift picks the destination's focus by its own preference, which is not
    // necessarily the window that had it.
    restore_focus(&rift, &transaction, &target, &workspace_name, &focus_window)?;

    transaction.settle(
        "workspace placement",
        EventExpectation::Workspace {
            display_uuid: &target.display_uuid,
            workspace_name: &workspace_name,
        },
        |rift| rift.windows_are_at(&target, &workspace_name, &window_ids),
    )?;
    verify(&rift, &target, &workspace_name, &window_ids)?;

    notify::moved(format!(
        "{} window{} to workspace {} on {}",
        window_ids.len(),
        if window_ids.len() == 1 { "" } else { "s" },
        workspace_name,
        target.display_name()
    ));
    Ok(())
}

/// Move whatever the command left behind, one window at a time.
fn repair_stragglers(
    rift: &Rift,
    transaction: &RiftTransaction<'_>,
    target: &TargetContext,
    workspace_name: &str,
    windows: &[WindowData],
) -> Result<()> {
    let ids: Vec<WindowId> = windows.iter().map(|window| window.id).collect();
    for attempt in 1..=REPAIR_ATTEMPTS {
        let missing = rift.windows_missing_from_workspace(target, workspace_name, &ids)?;
        if missing.is_empty() {
            return Ok(());
        }
        crate::logging::warn(&format!(
            "native move left {} window(s) behind; repairing (attempt {attempt})",
            missing.len()
        ));
        for window in windows.iter().filter(|window| missing.contains(&window.id)) {
            if !rift.window_is_on_display(target, window.id)? {
                let result = transaction.confirmed_step_with_timeout(
                    &format!("straggler {} display transfer", window.id.idx),
                    rift.transfer_window_command(target, window),
                    EventExpectation::Display(&target.display_uuid),
                    |rift| rift.window_is_on_display(target, window.id),
                    REPAIR_TIMEOUT,
                );
                match result {
                    Ok(()) => {}
                    Err(error) if is_timeout(&error) => {}
                    Err(error) => return Err(error),
                }
            }
        }
        // A workspace command is ignored for windows on any other display, and
        // the native move already made the destination active, so this only
        // guards against focus having moved on in the meantime.
        if !rift.display_is_active(&target.display_uuid)? {
            placement::acquire_display(rift, transaction, target, workspace_name)?;
        }
        for window in windows.iter().filter(|window| missing.contains(&window.id)) {
            let result = transaction.confirmed_step_with_timeout(
                &format!("straggler {} workspace placement", window.id.idx),
                rift.move_window_to_workspace_command(target, window, false),
                EventExpectation::Workspace {
                    display_uuid: &target.display_uuid,
                    workspace_name,
                },
                |rift| rift.windows_are_at(target, workspace_name, &[window.id]),
                REPAIR_TIMEOUT,
            );
            match result {
                Ok(()) => {}
                Err(error) if is_timeout(&error) => {}
                Err(error) => return Err(error),
            }
        }
    }
    let missing = rift.windows_missing_from_workspace(target, workspace_name, &ids)?;
    if missing.is_empty() {
        return Ok(());
    }
    Err(state_error(format!(
        "{} window(s) could not be moved to workspace {workspace_name} on {}",
        missing.len(),
        target.display_name()
    )))
}

/// Refuse rather than move the wrong workspace.
///
/// Read immediately before the command is sent, because that is the only moment
/// whose answer the command will actually use.
fn confirm_source(rift: &Rift, source: &TargetContext, workspace_name: &str) -> Result<()> {
    let displays = rift.displays()?;
    let now = rift.focused_workspace(&displays)?;
    let follows_key_window =
        now.display_uuid == source.display_uuid && now.workspace.name == workspace_name;
    // The key window can sit in a workspace that is not the one on screen:
    // focusing a window does not show its workspace. "The window I am looking
    // at" and "the workspace I am looking at" then name different things, and
    // nothing here can tell which the keypress meant -- so refuse. From a
    // keybinding the two always agree; this state is reached by focusing a
    // window programmatically.
    let key_window_workspace = rift
        .workspaces(source.space)?
        .into_iter()
        .find(|workspace| workspace.windows.iter().any(|window| window.is_focused))
        .map(|workspace| workspace.name);
    if let Some(elsewhere) = key_window_workspace.filter(|name| name != workspace_name) {
        let message = format!(
            "workspace {workspace_name} is showing on {} but the key window is in \
             workspace {elsewhere}; refusing to guess which one to move",
            source.display_name()
        );
        crate::logging::warn(&message);
        notify::refused(&message);
        return Err(state_error(message));
    }
    // Both halves matter and they are not the same question. The first is "am I
    // still moving the workspace the user was looking at"; the second is "is
    // that the workspace Rift will actually move".
    let is_on_screen = rift.display_is_active(&source.display_uuid)?
        && rift.workspace_is_active(source, workspace_name)?;
    if follows_key_window && is_on_screen {
        return Ok(());
    }
    let message = format!(
        "source drifted before the move: key window is on {} workspace {}, \
         expected {} workspace {} showing (on screen: {is_on_screen})",
        now.display_uuid.chars().take(8).collect::<String>(),
        now.workspace.name,
        source.display_name(),
        workspace_name
    );
    crate::logging::warn(&message);
    notify::refused(&message);
    Err(state_error(message))
}

/// The destination workspace is chosen by ordinal, not by name.
///
/// Every space is built from the same configured list, so the ordinal and the
/// name are a fixed bijection and workspace 3 lands on workspace 3. Checking it
/// turns a future configuration change that breaks that assumption into an
/// error instead of a move to the wrong workspace.
fn assert_same_ordinal(
    source: &TargetContext,
    target: &TargetContext,
    workspace_name: &str,
) -> Result<()> {
    if source.workspace_index == target.workspace_index {
        return Ok(());
    }
    Err(state_error(format!(
        "workspace {workspace_name} is at position {} on the source display but {} on {}; \
         Rift moves by position, so this would land somewhere else",
        source.workspace_index,
        target.workspace_index,
        target.display_name()
    )))
}

fn restore_focus(
    rift: &Rift,
    transaction: &RiftTransaction<'_>,
    target: &TargetContext,
    workspace_name: &str,
    focus_window: &WindowData,
) -> Result<()> {
    for attempt in 1..=FOCUS_ATTEMPTS {
        if rift.window_is_focused_at(target, workspace_name, focus_window.id)? {
            return Ok(());
        }
        let result = transaction.confirmed_step_with_timeout(
            &format!("workspace focus restoration attempt {attempt}"),
            rift.focus_window_command(focus_window),
            EventExpectation::Workspace {
                display_uuid: &target.display_uuid,
                workspace_name,
            },
            |rift| rift.window_is_focused_at(target, workspace_name, focus_window.id),
            ACQUIRE_TIMEOUT,
        );
        match result {
            Ok(()) => return Ok(()),
            Err(error) if is_timeout(&error) => {}
            Err(error) => return Err(error),
        }
    }
    Err(state_error(
        "workspace moved but focus was not restored to the original window",
    ))
}

/// The command reports success by sending, not by moving.
///
/// Rift declines it -- silently, with only a line in its own log -- when a drag
/// is active, when either space is unavailable, when source and destination
/// resolve to the same space, when the workspace is empty, and when the layout
/// engine finds nothing to relocate. Every one of those returns a successful
/// IPC send, so the state check is what separates a move from a no-op.
fn verify(
    rift: &Rift,
    target: &TargetContext,
    workspace_name: &str,
    window_ids: &[WindowId],
) -> Result<()> {
    if rift.windows_are_at(target, workspace_name, window_ids)? {
        return Ok(());
    }
    Err(state_error(format!(
        "not every window reached workspace {workspace_name} on {}",
        target.display_name()
    )))
}

fn native_enabled() -> bool {
    std::env::var("RIFT_ERGO_NATIVE_WORKSPACE_MOVE").as_deref() != Ok("off")
}
