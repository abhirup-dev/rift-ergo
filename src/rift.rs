use rift_client::{
    DisplayData, DisplaySelector, EventKind, LayoutCommand, LayoutStateData, ReactorCommand, Rect,
    RiftCommand, RiftMachClient, RiftMachSubscription, WindowData, WindowId, WorkspaceData,
    WorkspaceSelector,
};

use crate::{Result, state_error};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowLocation {
    pub display_uuid: String,
    pub workspace_name: String,
    pub is_focused: bool,
}

pub struct FocusedWindow {
    pub window: WindowData,
    pub location: WindowLocation,
}

pub struct FocusedWorkspace {
    pub display_uuid: String,
    pub workspace: WorkspaceData,
    /// The window macOS reports as key, when it is one of this workspace's.
    /// Absent when focus sits outside Rift's management -- Finder's desktop, a
    /// native-fullscreen space, an unmanaged app -- which is common at the
    /// moment a keybinding fires.
    pub focused_window: Option<WindowData>,
}

impl FocusedWorkspace {
    /// The window that should own focus once this workspace lands somewhere:
    /// the key window if it is here, otherwise the first tiled window.
    pub fn focus_target(&self) -> Option<&WindowData> {
        self.focused_window.as_ref().or_else(|| {
            self.workspace
                .windows
                .iter()
                .find(|window| !window.is_floating)
                .or_else(|| self.workspace.windows.first())
        })
    }
}

pub struct TargetContext {
    pub display_uuid: String,
    /// macOS's name for the display, for messages a human reads.
    display_name: Option<String>,
    pub space: u64,
    pub frame: Rect,
    pub workspace_index: usize,
    pub display_is_active: bool,
    pub workspace_is_active: bool,
    pub focus_anchor: Option<WindowData>,
    frame_validation: FrameValidation,
}

impl TargetContext {
    /// The display as a human would name it, falling back to the UUID prefix.
    pub fn display_name(&self) -> String {
        self.display_name
            .clone()
            .unwrap_or_else(|| self.display_uuid.chars().take(8).collect())
    }
}

#[derive(Clone, Copy)]
enum FrameValidation {
    Horizontal(Rect),
    Vertical(Rect),
    MembershipOnly,
}

pub struct Rift {
    client: RiftMachClient,
}

impl Rift {
    pub fn connect() -> Result<Self> {
        let client = RiftMachClient::connect()?;
        if !client.is_available() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "Rift's Mach service is unavailable",
            )
            .into());
        }
        Ok(Self { client })
    }

    pub fn displays(&self) -> Result<Vec<DisplayData>> {
        Ok(self.client.get_displays()?)
    }

    pub fn target_context(
        &self,
        workspace_name: &str,
        display_uuid: String,
        displays: &[DisplayData],
    ) -> Result<TargetContext> {
        let display = displays
            .iter()
            .find(|display| display.uuid == display_uuid)
            .ok_or_else(|| state_error("target display disappeared"))?;
        let space = display_space(display)
            .ok_or_else(|| state_error("target display has no active macOS space"))?;
        let workspaces = self.client.get_workspaces(Some(space))?;
        let workspace = workspaces
            .iter()
            .find(|workspace| workspace.name == workspace_name)
            .ok_or_else(|| state_error(format!("unknown workspace: {workspace_name}")))?;
        let focus_anchor = workspaces
            .iter()
            .find(|workspace| workspace.is_active)
            .and_then(|workspace| {
                workspace
                    .windows
                    .iter()
                    .find(|window| window.is_focused)
                    .or_else(|| workspace.windows.first())
            })
            .cloned();

        Ok(TargetContext {
            display_uuid,
            display_name: display.name.clone(),
            space,
            frame: display.frame,
            workspace_index: workspace.index,
            display_is_active: display.is_active_context,
            workspace_is_active: workspace.is_active,
            focus_anchor,
            frame_validation: frame_validation(display, displays, &workspace.layout_mode),
        })
    }

    pub fn active_target_context(
        &self,
        display_uuid: String,
        displays: &[DisplayData],
    ) -> Result<(String, TargetContext)> {
        let display = displays
            .iter()
            .find(|display| display.uuid == display_uuid)
            .ok_or_else(|| state_error("target display disappeared"))?;
        let space = display_space(display)
            .ok_or_else(|| state_error("target display has no active macOS space"))?;
        let workspace_name = self
            .client
            .get_workspaces(Some(space))?
            .into_iter()
            .find(|workspace| workspace.is_active)
            .map(|workspace| workspace.name)
            .ok_or_else(|| state_error("target display has no active Rift workspace"))?;
        let target = self.target_context(&workspace_name, display_uuid, displays)?;
        Ok((workspace_name, target))
    }

    pub fn workspace_at(
        &self,
        target: &TargetContext,
        workspace_name: &str,
    ) -> Result<WorkspaceData> {
        self.client
            .get_workspaces(Some(target.space))?
            .into_iter()
            .find(|workspace| workspace.name == workspace_name)
            .ok_or_else(|| state_error(format!("unknown workspace: {workspace_name}")))
    }

    pub fn focused_window(&self, displays: &[DisplayData]) -> Result<FocusedWindow> {
        let focused = self.focused_workspace(displays)?;
        let window = focused
            .focused_window
            .ok_or_else(|| state_error("no focused Rift-managed window"))?;
        Ok(FocusedWindow {
            window,
            location: WindowLocation {
                display_uuid: focused.display_uuid,
                workspace_name: focused.workspace.name,
                is_focused: true,
            },
        })
    }

    /// The workspace a keybinding refers to.
    ///
    /// The key window decides, not the display macOS reports as active. Both
    /// normally agree, and a hard focus drives both within 150 ms (measured
    /// 24/24). They come apart when something moves focus between the keypress
    /// and this call -- Rift's auto-switch on app activation, or the
    /// focus-follow hook -- and acting on the stale one relocates the *other*
    /// monitor's workspace, which is silent and destructive. The key window is
    /// what the user last typed into, so it wins; `display_is_active` on the
    /// returned context tells the caller whether Rift's command routing has to
    /// be moved there first.
    ///
    /// The key window is not required: focus resting on an unmanaged window --
    /// Finder's desktop, a native-fullscreen space -- is common at the moment a
    /// binding fires, and the workspace on screen is unambiguous without it.
    pub fn focused_workspace(&self, displays: &[DisplayData]) -> Result<FocusedWorkspace> {
        let active_display = displays
            .iter()
            .find(|display| display.is_active_context)
            .filter(|display| display_space(display).is_some());
        if let Some(display) = active_display {
            let space = display_space(display).expect("filtered above");
            let workspaces = self.client.get_workspaces(Some(space))?;
            let key_window_is_here = workspaces
                .iter()
                .any(|workspace| workspace.windows.iter().any(|window| window.is_focused));
            if !key_window_is_here
                && let Some(elsewhere) =
                    self.workspace_holding_key_window(displays, &display.uuid)?
            {
                crate::logging::warn(&format!(
                    "key window is on {} workspace {} while Rift's active context is {}; \
                     following the key window",
                    elsewhere.display_uuid, elsewhere.workspace.name, display.uuid
                ));
                return Ok(elsewhere);
            }
            let workspace = workspaces
                .into_iter()
                .find(|workspace| workspace.is_active)
                .ok_or_else(|| state_error("active display has no active Rift workspace"))?;
            let focused_window = workspace
                .windows
                .iter()
                .find(|window| window.is_focused)
                .cloned();
            return Ok(FocusedWorkspace {
                display_uuid: display.uuid.clone(),
                workspace,
                focused_window,
            });
        }

        for display in displays {
            let Some(space) = display_space(display) else {
                continue;
            };
            for workspace in self.client.get_workspaces(Some(space))? {
                let focused_window = workspace
                    .windows
                    .iter()
                    .find(|window| window.is_focused)
                    .cloned();
                if let Some(focused_window) = focused_window {
                    return Ok(FocusedWorkspace {
                        display_uuid: display.uuid.clone(),
                        workspace,
                        focused_window: Some(focused_window),
                    });
                }
            }
        }
        Err(state_error("no focused Rift-managed window"))
    }

    /// The display showing the key window, when it is not the one passed in.
    ///
    /// Returns that display's *active* workspace rather than the key window's
    /// own: what the user sees is what a workspace-scoped binding means, and a
    /// key window parked in a hidden workspace is the broken state the
    /// focus-follow hook exists to repair.
    fn workspace_holding_key_window(
        &self,
        displays: &[DisplayData],
        skip_display_uuid: &str,
    ) -> Result<Option<FocusedWorkspace>> {
        for display in displays {
            if display.uuid == skip_display_uuid {
                continue;
            }
            let Some(space) = display_space(display) else {
                continue;
            };
            let workspaces = self.client.get_workspaces(Some(space))?;
            let Some(key_window) = workspaces
                .iter()
                .flat_map(|workspace| workspace.windows.iter())
                .find(|window| window.is_focused)
                .cloned()
            else {
                continue;
            };
            let Some(workspace) = workspaces.into_iter().find(|workspace| workspace.is_active)
            else {
                continue;
            };
            let focused_window = workspace
                .windows
                .iter()
                .any(|window| window.id == key_window.id)
                .then_some(key_window);
            return Ok(Some(FocusedWorkspace {
                display_uuid: display.uuid.clone(),
                workspace,
                focused_window,
            }));
        }
        Ok(None)
    }

    /// A window by id, or `None` when Rift does not manage it.
    ///
    /// A window Rift has just discovered is addressable by exact id before it
    /// appears in any workspace snapshot, which is how the new-window hook sees
    /// launches that would otherwise be missed.
    pub fn window_info(&self, window_id: WindowId) -> Option<WindowData> {
        self.client.get_window_info(window_id).ok()
    }

    /// The layout engine's own view of one workspace, including `tiled_windows`.
    ///
    /// This is the only place a window destroyed without Rift noticing is still
    /// visible: it is gone from `workspaces` and from `window_info`, but its
    /// node survives here and keeps reserving space.
    pub fn layout_state(&self, space: u64, workspace_index: usize) -> Result<LayoutStateData> {
        Ok(self
            .client
            .get_workspace_layout_state(Some(space), Some(workspace_index))?)
    }

    /// Every window Rift manages, with where it currently lives.
    pub fn window_locations(
        &self,
        displays: &[DisplayData],
    ) -> Result<Vec<(String, String, WindowData)>> {
        let mut located = Vec::new();
        for display in displays {
            let Some(space) = display_space(display) else {
                continue;
            };
            for workspace in self.client.get_workspaces(Some(space))? {
                for window in workspace.windows {
                    located.push((display.uuid.clone(), workspace.name.clone(), window));
                }
            }
        }
        Ok(located)
    }

    /// Workspace `name` on each managed display, for callers that decide by
    /// where a workspace has windows.
    pub fn workspace_on_each_display(
        &self,
        workspace_name: &str,
        displays: &[DisplayData],
    ) -> Result<Vec<(DisplayData, WorkspaceData)>> {
        let mut found = Vec::new();
        for display in displays {
            let Some(space) = display_space(display) else {
                continue;
            };
            if let Some(workspace) = self
                .client
                .get_workspaces(Some(space))?
                .into_iter()
                .find(|workspace| workspace.name == workspace_name)
            {
                found.push((display.clone(), workspace));
            }
        }
        Ok(found)
    }

    /// Workspaces on a macOS space, in configured order.
    pub fn workspaces(&self, space: u64) -> Result<Vec<WorkspaceData>> {
        Ok(self.client.get_workspaces(Some(space))?)
    }

    pub fn subscribe(&self) -> Result<RiftMachSubscription> {
        Ok(self.client.subscribe(EventKind::All)?)
    }

    pub fn execute(&self, command: RiftCommand) -> Result<()> {
        self.client.execute(command)?;
        Ok(())
    }

    /// A window Rift manages in `workspace_name` on the target's space, used
    /// to give an inactive display a key window when its visible workspace
    /// offers none. Only that workspace: raising a hidden window makes Rift
    /// bring its workspace forward a moment later, which is welcome when it is
    /// the workspace being switched to and a regression for any other.
    pub fn hidden_anchor(
        &self,
        target: &TargetContext,
        workspace_name: &str,
    ) -> Result<Option<WindowData>> {
        Ok(self
            .workspaces(target.space)?
            .into_iter()
            .filter(|workspace| workspace.name == workspace_name)
            .flat_map(|workspace| workspace.windows)
            .find(|window| !window.is_floating))
    }

    /// Any tiled window Rift manages on the target's space, in any workspace.
    pub fn any_hidden_window(&self, target: &TargetContext) -> Result<Option<WindowData>> {
        Ok(self
            .workspaces(target.space)?
            .into_iter()
            .flat_map(|workspace| workspace.windows)
            .find(|window| !window.is_floating))
    }

    pub fn activate_workspace_command(&self, target: &TargetContext) -> RiftCommand {
        RiftCommand::Layout(LayoutCommand::SwitchToWorkspace(target.workspace_index))
    }

    /// Move the active workspace of the *active* display to `target`.
    ///
    /// The command carries no source: Rift reads it from whichever display is
    /// active when the command runs. Binding that display first is the caller's
    /// job. The destination is addressed by UUID rather than by direction so
    /// Rift's notion of "next display" cannot disagree with ours.
    pub fn move_workspace_to_display_command(&self, target: &TargetContext) -> RiftCommand {
        RiftCommand::Reactor(ReactorCommand::MoveWorkspaceToDisplay {
            selector: DisplaySelector::Uuid(target.display_uuid.clone()),
            wrap_around: false,
        })
    }

    pub fn transfer_window_command(
        &self,
        target: &TargetContext,
        window: &WindowData,
    ) -> RiftCommand {
        RiftCommand::Reactor(ReactorCommand::MoveWindowToDisplay {
            selector: DisplaySelector::Uuid(target.display_uuid.clone()),
            window_id: Some(window.id.idx),
        })
    }

    pub fn focus_display_command(&self, display_uuid: &str) -> RiftCommand {
        RiftCommand::Reactor(ReactorCommand::FocusDisplay(DisplaySelector::Uuid(
            display_uuid.to_string(),
        )))
    }

    /// Move a layout node by window id alone.
    ///
    /// Unlike `move_window_to_workspace_command` this takes no `WindowData`,
    /// because the window it addresses no longer exists -- only the layout node
    /// does, and the id in that node is all there is to name it by.
    pub fn move_stale_window_command(
        &self,
        workspace_index: usize,
        window_id: WindowId,
    ) -> RiftCommand {
        RiftCommand::Layout(LayoutCommand::MoveWindowToWorkspace {
            workspace: WorkspaceSelector::Index(workspace_index),
            follow: false,
            window_id: Some(window_id.idx),
        })
    }

    pub fn focus_window_command(&self, window: &WindowData) -> RiftCommand {
        RiftCommand::Reactor(ReactorCommand::FocusWindow {
            window_id: window.id,
            window_server_id: window.window_server_id,
        })
    }

    pub fn move_within_display_command(
        &self,
        target: &TargetContext,
        window: &WindowData,
    ) -> RiftCommand {
        self.move_window_to_workspace_command(target, window, true)
    }

    pub fn move_window_to_workspace_command(
        &self,
        target: &TargetContext,
        window: &WindowData,
        follow: bool,
    ) -> RiftCommand {
        RiftCommand::Layout(LayoutCommand::MoveWindowToWorkspace {
            workspace: WorkspaceSelector::Index(target.workspace_index),
            follow,
            window_id: Some(window.id.idx),
        })
    }

    pub fn display_is_active(&self, display_uuid: &str) -> Result<bool> {
        Ok(self
            .client
            .get_displays()?
            .iter()
            .any(|display| display.uuid == display_uuid && display.is_active_context))
    }

    pub fn workspace_is_active(
        &self,
        target: &TargetContext,
        workspace_name: &str,
    ) -> Result<bool> {
        Ok(self
            .client
            .get_workspaces(Some(target.space))?
            .iter()
            .any(|workspace| workspace.name == workspace_name && workspace.is_active))
    }

    pub fn window_is_at(
        &self,
        target: &TargetContext,
        workspace_name: &str,
        window_id: WindowId,
    ) -> Result<bool> {
        Ok(self
            .window_location(target, window_id)?
            .as_ref()
            .is_some_and(|location| location.workspace_name == workspace_name))
    }

    pub fn window_is_on_display(
        &self,
        target: &TargetContext,
        window_id: WindowId,
    ) -> Result<bool> {
        if self.window_location(target, window_id)?.is_none() {
            return Ok(false);
        }
        // Deliberately the observed frame, not the layout engine's view. This
        // confirms that a physical display transfer landed, and Rift moves
        // virtual workspace membership independently of the window itself: a
        // window can be tiled in the destination workspace while still sitting
        // on the source display. Asking the engine here reports success for a
        // move that has not happened, which then cannot be reconciled by the
        // later phases, since those read geometry.
        let window = self.client.get_window_info(window_id)?;
        Ok(physically_on_target(&window, target))
    }

    /// Whether the workspace shown on the target display contains the window
    /// Rift reports focused: the sign that a display focus has fully landed and
    /// a workspace switch there will not be undone by a late refocus.
    pub fn active_workspace_has_focus(&self, target: &TargetContext) -> Result<bool> {
        Ok(self
            .workspaces(target.space)?
            .iter()
            .filter(|workspace| workspace.is_active)
            .flat_map(|workspace| workspace.windows.iter())
            .any(|window| window.is_focused))
    }

    /// Whether Rift reports `window_id` as the focused window.
    pub fn window_is_focused(&self, window_id: WindowId) -> Result<bool> {
        Ok(self.client.get_window_info(window_id)?.is_focused)
    }

    pub fn window_is_focused_at(
        &self,
        target: &TargetContext,
        workspace_name: &str,
        window_id: WindowId,
    ) -> Result<bool> {
        if !self.window_is_at(target, workspace_name, window_id)? {
            return Ok(false);
        }
        Ok(self.client.get_window_info(window_id)?.is_focused)
    }

    pub fn windows_are_at(
        &self,
        target: &TargetContext,
        workspace_name: &str,
        window_ids: &[WindowId],
    ) -> Result<bool> {
        let workspaces = self.client.get_workspaces(Some(target.space))?;
        let Some(workspace) = workspaces
            .iter()
            .find(|workspace| workspace.name == workspace_name)
        else {
            return Ok(false);
        };
        for window_id in window_ids {
            if !workspace
                .windows
                .iter()
                .any(|window| window.id == *window_id)
                || !physically_on_target(&self.client.get_window_info(*window_id)?, target)
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub fn windows_missing_from_workspace(
        &self,
        target: &TargetContext,
        workspace_name: &str,
        window_ids: &[WindowId],
    ) -> Result<Vec<WindowId>> {
        let workspaces = self.client.get_workspaces(Some(target.space))?;
        let workspace = workspaces
            .iter()
            .find(|workspace| workspace.name == workspace_name);
        let mut missing = Vec::new();
        for window_id in window_ids {
            let is_member = workspace.is_some_and(|workspace| {
                workspace
                    .windows
                    .iter()
                    .any(|window| window.id == *window_id)
            });
            if !is_member
                || !physically_on_target(&self.client.get_window_info(*window_id)?, target)
            {
                missing.push(*window_id);
            }
        }
        Ok(missing)
    }

    pub fn window_location(
        &self,
        target: &TargetContext,
        window_id: WindowId,
    ) -> Result<Option<WindowLocation>> {
        for workspace in self.client.get_workspaces(Some(target.space))? {
            if workspace
                .windows
                .iter()
                .any(|window| window.id == window_id)
            {
                let is_focused = self.client.get_window_info(window_id)?.is_focused;
                return Ok(Some(WindowLocation {
                    display_uuid: target.display_uuid.clone(),
                    workspace_name: workspace.name,
                    is_focused,
                }));
            }
        }
        Ok(None)
    }
}

pub(crate) fn display_space(display: &DisplayData) -> Option<u64> {
    display.active_space_ids.first().copied().or(display.space)
}

fn frame_validation(
    target: &DisplayData,
    displays: &[DisplayData],
    layout_mode: &str,
) -> FrameValidation {
    // Scrolling parks off-viewport windows outside the display's bounds -- far
    // to the left on a horizontal strip, far below on a vertical one -- so a
    // window that is correctly placed still fails an axis containment test.
    // This has to precede the axis checks: when displays are disjoint on an
    // axis the check below returns first, and on a vertically stacked pair the
    // scrolling case was never reached at all.
    if layout_mode == "scrolling" {
        return FrameValidation::MembershipOnly;
    }

    let vertically_disjoint = displays
        .iter()
        .filter(|display| display.uuid != target.uuid)
        .all(|display| {
            !intervals_overlap(
                target.frame.origin.y,
                target.frame.origin.y + target.frame.size.height,
                display.frame.origin.y,
                display.frame.origin.y + display.frame.size.height,
            )
        });
    if vertically_disjoint {
        return FrameValidation::Vertical(target.frame);
    }

    let horizontally_disjoint = displays
        .iter()
        .filter(|display| display.uuid != target.uuid)
        .all(|display| {
            !intervals_overlap(
                target.frame.origin.x,
                target.frame.origin.x + target.frame.size.width,
                display.frame.origin.x,
                display.frame.origin.x + display.frame.size.width,
            )
        });
    if horizontally_disjoint {
        FrameValidation::Horizontal(target.frame)
    } else {
        FrameValidation::MembershipOnly
    }
}

fn intervals_overlap(first_min: f64, first_max: f64, second_min: f64, second_max: f64) -> bool {
    first_min < second_max && second_min < first_max
}

/// Whether the WindowServer has `window` on the target display.
///
/// Rift moves virtual workspace membership independently of the window
/// itself, so a window can be tiled in the destination workspace while still
/// sitting on the source display; Rift then notices and pulls the membership
/// back to match. The WindowServer's own space list for the window is the
/// authoritative answer. The frame heuristic remains for the case where the
/// private call is unavailable, and for windows Rift has no server id for.
fn physically_on_target(window: &WindowData, target: &TargetContext) -> bool {
    if let Some(spaces) = window
        .window_server_id
        .and_then(crate::space_escape::skylight::window_spaces)
    {
        return spaces.contains(&target.space);
    }
    frame_matches_display(&window.frame, target.frame_validation)
}

fn frame_matches_display(frame: &Rect, validation: FrameValidation) -> bool {
    let center_x = frame.origin.x + frame.size.width / 2.0;
    let center_y = frame.origin.y + frame.size.height / 2.0;
    match validation {
        FrameValidation::Horizontal(display) => {
            center_x >= display.origin.x && center_x <= display.origin.x + display.size.width
        }
        FrameValidation::Vertical(display) => {
            center_y >= display.origin.y && center_y <= display.origin.y + display.size.height
        }
        FrameValidation::MembershipOnly => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rift_client::{Point, Size};

    fn rect(x: f64, y: f64, width: f64, height: f64) -> Rect {
        Rect {
            origin: Point { x, y },
            size: Size { width, height },
        }
    }

    #[test]
    fn physical_display_check_uses_the_selected_axis() {
        let display = rect(-416.0, -1409.0, 2560.0, 1409.0);
        assert!(frame_matches_display(
            &rect(-414.0, -1407.0, 2556.0, 1404.0),
            FrameValidation::Vertical(display)
        ));
        assert!(!frame_matches_display(
            &rect(2143.0, -1.0, 2556.0, 1404.0),
            FrameValidation::Vertical(display)
        ));
        assert!(frame_matches_display(
            &rect(-2207.0, -1407.0, 2045.0, 1404.0),
            FrameValidation::Vertical(display)
        ));
    }
}
