//! Evict layout nodes whose windows no longer exist.
//!
//! A window can be destroyed without Rift learning about it. Rift's reactor
//! deliberately keeps the logical window on `WindowInvalidated` -- AX reports
//! spurious destructions during display transitions, so two earlier upstream
//! fixes made AX destruction non-authoritative -- and it asks for a fresh window
//! inventory exactly once, immediately, while the WindowServer window usually
//! still exists. Nothing re-checks afterwards. The node therefore survives
//! forever, reserving a column for a window that is gone, which is what makes
//! the remaining windows look oddly tiled. Upstream issue #486, still open.
//!
//! A second, unrelated route produces the same distortion: a live window that
//! leaves the managed space by going native-fullscreen. Rift only releases the
//! node when three internal conditions all hold, and emits no layout event at
//! all otherwise, so the column stays reserved for a window that is now on its
//! own space. Upstream issue #363, closed as fixed but still reproducing here.
//! Nothing re-checks afterwards in either case: the full reconcile sweep is
//! compiled only for tests, and the production one looks at active spaces,
//! which a fullscreen-parked window is never part of.
//!
//! Detection uses Rift's own two views of a workspace against each other. The
//! layout's `container_tree` is what geometry is calculated from, and the
//! workspace's window list is what the removal path maintains; they come from
//! independent stores that nothing cross-validates. A window node the tree
//! still holds and the membership list no longer knows about is therefore a
//! node reserving space for a window that is not there. `tiled_windows` is not
//! used: it agreed with the tree for a destroyed window and disagreed for a
//! fullscreened one, so it detects only half the problem.
//!
//! Delete this workflow if #486 and #363 both land.

use std::collections::BTreeSet;
use std::time::Duration;

use rift_client::{ContainerTreeNode, DisplayData, WindowId};

use crate::rift::{Rift, display_space};
use crate::space_escape::skylight;
use crate::transaction::{EventExpectation, RiftTransaction, is_timeout};
use crate::{Result, logging, notify};

/// Where evicted nodes are parked.
///
/// Only ever holds nodes whose windows are destroyed. A node for a live window
/// cannot be moved here at all: `MoveWindowToWorkspace` looks the window up in
/// the workspace's membership set first and returns without doing anything when
/// it is absent, which is precisely the state that makes a node look stale.
///
/// Not to be confused with Rift's own "quarantine", which drops WindowServer
/// space events while asleep or during display churn and has nothing to do with
/// workspaces.
///
/// Parking is a relocation, not a deletion: the dead node keeps existing, it
/// just stops distorting a workspace that is used. Measured -- moving the node
/// out is the only eviction that works. Showing the workspace so Rift's arrange
/// pass runs over it, the workaround most reported upstream, left the node in
/// place across a full switch-away-and-back here. The parked nodes accumulate
/// until Rift restarts, which discards all layout state anyway.
const EVICTION_WORKSPACE: &str = "Z";
const EVICTION_TIMEOUT: Duration = Duration::from_millis(1_500);

/// Remove dead layout nodes from every workspace on every display.
///
/// Reports how many were evicted. Nothing to do is the overwhelmingly common
/// case and costs one layout query per workspace.
pub fn prune_layout() -> Result<()> {
    let rift = Rift::connect()?;
    let displays = rift.displays()?;
    let mut evicted = 0usize;
    let mut failed = 0usize;
    let mut stranded = 0usize;

    for display in &displays {
        // While a display shows a native-fullscreen space Rift reports it with
        // no managed space, and the layout query for the space it *used* to
        // show answers "Space or workspace not found". So the display cannot be
        // inspected at all until the user leaves fullscreen -- which is also the
        // moment the reserved column becomes visible again. Said out loud
        // rather than skipped silently, because this is exactly the situation
        // that strands a node.
        let Some(space) = display_space(display) else {
            logging::info(&format!(
                "prune-layout: {} has no managed space right now; not inspected",
                display.name.as_deref().unwrap_or(&display.uuid)
            ));
            continue;
        };
        let stale = stale_nodes(&rift, space)?;
        if stale.is_empty() {
            continue;
        }
        let name = display.name.as_deref().unwrap_or(&display.uuid);
        // A node whose window is still alive is left strictly alone. Evicting it
        // would relocate the user's real window -- for a fullscreened app, out
        // of fullscreen and into a workspace they never asked for. Reported
        // instead, because the reserved column is visible and unexplained.
        let (dead, alive): (Vec<StaleNode>, Vec<StaleNode>) = stale
            .iter()
            .copied()
            .partition(|node| node.fate == NodeFate::Destroyed);
        if !alive.is_empty() {
            let ids = alive
                .iter()
                .map(|node| node.window_id.idx)
                .collect::<Vec<_>>();
            logging::warn(&format!(
                "prune-layout: {name} reserves space for {} live window(s) that left \
                 the workspace: {ids:?}; not touching them",
                alive.len()
            ));
            stranded += alive.len();
        }
        if dead.is_empty() {
            continue;
        }
        logging::info(&format!(
            "prune-layout: {} dead node(s) on {name}: {:?}",
            dead.len(),
            dead.iter()
                .map(|node| node.window_id.idx)
                .collect::<Vec<_>>()
        ));
        let dead_ids = dead.iter().map(|node| node.window_id).collect::<Vec<_>>();
        match evict(&rift, display, space, &dead_ids) {
            Ok(count) => {
                evicted += count;
                failed += dead_ids.len() - count;
            }
            Err(error) => {
                // One display failing must not hide a successful sweep of the
                // other, and the caller is usually a hook nobody is watching.
                logging::warn(&format!("prune-layout: {} failed: {error}", display.uuid));
                failed += dead_ids.len();
            }
        }
    }

    if evicted > 0 {
        notify::moved(format!(
            "evicted {evicted} dead layout node{}",
            if evicted == 1 { "" } else { "s" }
        ));
    } else if stranded > 0 {
        // Worth interrupting for, because the reserved column is unexplained
        // from the outside. The node is not garbage: Rift is holding the
        // window's place while it sits on a space of its own, and leaving
        // fullscreen puts the window back into it and reconciles the workspace.
        // Measured -- moving the node is not an alternative, because the
        // command that would move it no-ops on a window the workspace no longer
        // counts as a member, which is exactly this window.
        notify::refused(format!(
            "{stranded} fullscreen window{} still holding a column; leave fullscreen to release it",
            if stranded == 1 { " is" } else { "s are" }
        ));
    }
    if failed > 0 {
        return Err(crate::state_error(format!(
            "{failed} dead layout node(s) could not be evicted"
        )));
    }
    Ok(())
}

/// Why a node is reserving space for a window that is not in the workspace.
///
/// Rift alone cannot tell these apart: it still answers `window_info` for a
/// window that has gone fullscreen, and it keeps the node in both cases. The
/// WindowServer can, so the classification comes from SkyLight rather than from
/// Rift -- the same oracle the placement checks already trust over Rift's own
/// frames.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum NodeFate {
    /// The window no longer exists anywhere. Safe to move the node out.
    Destroyed,
    /// The window is alive on a native-fullscreen space of its own. Moving the
    /// node would drag the user's real window out of fullscreen.
    Fullscreen,
    /// Alive on some other Desktop space. Also not ours to move.
    Elsewhere,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) struct StaleNode {
    pub window_id: WindowId,
    pub fate: NodeFate,
}

/// What the WindowServer says became of the window behind a node.
///
/// `WindowId::idx` is the WindowServer's own window id -- the same number
/// `window_server_id` carries and the same one `CGWindowListCopyWindowInfo`
/// reports -- so no correlation by pid or title is needed to ask SkyLight about
/// a window Rift named.
fn classify(window_id: WindowId) -> NodeFate {
    let spaces = skylight::window_spaces(window_id.idx).unwrap_or_default();
    if spaces.is_empty() {
        // Gone. A window Rift merely hides in an inactive workspace also reports
        // no space, but such a window is still a member of its workspace and so
        // never reaches here.
        return NodeFate::Destroyed;
    }
    if spaces.iter().any(|space| !skylight::is_desktop(*space)) {
        return NodeFate::Fullscreen;
    }
    NodeFate::Elsewhere
}

/// Nodes the layout tree holds that the workspace's own window list does not.
///
/// The eviction workspace is skipped: it is where dead nodes are parked, so
/// finding them there is the expected outcome rather than a problem.
fn stale_nodes(rift: &Rift, space: u64) -> Result<Vec<StaleNode>> {
    let mut stale = Vec::new();
    let mut seen = BTreeSet::new();
    for workspace in rift.workspaces(space)? {
        if workspace.name == EVICTION_WORKSPACE {
            continue;
        }
        let Ok(layout) = rift.layout_state(space, workspace.index) else {
            continue;
        };
        let members: BTreeSet<WindowId> =
            workspace.windows.iter().map(|window| window.id).collect();
        for window_id in window_nodes(&layout.container_tree) {
            if members.contains(&window_id) || !seen.insert(window_id) {
                continue;
            }
            stale.push(StaleNode {
                window_id,
                fate: classify(window_id),
            });
        }
    }
    Ok(stale)
}

/// Every window this layout tree reserves space for.
fn window_nodes(node: &ContainerTreeNode) -> Vec<WindowId> {
    let mut found = node.window_id.into_iter().collect::<Vec<_>>();
    for child in &node.children {
        found.extend(window_nodes(child));
    }
    found
}

/// Move dead nodes to the eviction workspace, confirming each one is gone.
///
/// Returns how many left. A node that will not move is left alone rather than
/// retried into the ground: the next invocation sees it again, and the
/// alternative is a hook that fights the layout on every window event.
fn evict(rift: &Rift, display: &DisplayData, space: u64, stale: &[WindowId]) -> Result<usize> {
    let workspaces = rift.workspaces(space)?;
    let Some(eviction_index) = workspaces
        .iter()
        .find(|workspace| workspace.name == EVICTION_WORKSPACE)
        .map(|workspace| workspace.index)
    else {
        return Err(crate::state_error(format!(
            "no workspace {EVICTION_WORKSPACE} on {}",
            display.uuid
        )));
    };

    let transaction = RiftTransaction::new(rift)?;
    // A workspace command is ignored for anything on another display.
    if !rift.display_is_active(&display.uuid)? {
        transaction.step(
            "eviction display focus",
            rift.focus_display_command(&display.uuid),
            EventExpectation::Display(&display.uuid),
            |rift| rift.display_is_active(&display.uuid),
        )?;
    }

    let mut evicted = 0usize;
    for window_id in stale {
        let result = transaction.confirmed_step_with_timeout(
            &format!("evict dead node {}", window_id.idx),
            rift.move_stale_window_command(eviction_index, *window_id),
            EventExpectation::Display(&display.uuid),
            |rift| {
                Ok(!stale_nodes(rift, space)?
                    .iter()
                    .any(|node| node.window_id == *window_id))
            },
            EVICTION_TIMEOUT,
        );
        match result {
            Ok(()) => evicted += 1,
            Err(error) if is_timeout(&error) => {
                logging::warn(&format!(
                    "prune-layout: node {} did not move; leaving it",
                    window_id.idx
                ));
            }
            Err(error) => return Err(error),
        }
    }
    Ok(evicted)
}
