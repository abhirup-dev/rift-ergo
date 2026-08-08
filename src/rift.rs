use rift_client::{
    DisplayData, DisplaySelector, EventKind, LayoutCommand, ReactorCommand, Rect, RiftCommand,
    RiftMachClient, RiftMachSubscription, WindowData, WindowId, WorkspaceData, WorkspaceSelector,
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
    pub focused_window: WindowData,
}

pub struct TargetContext {
    pub display_uuid: String,
    pub space: u64,
    pub workspace_index: usize,
    pub display_is_active: bool,
    pub workspace_is_active: bool,
    pub focus_anchor: Option<WindowData>,
    frame_validation: FrameValidation,
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
            space,
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
        Ok(FocusedWindow {
            window: focused.focused_window,
            location: WindowLocation {
                display_uuid: focused.display_uuid,
                workspace_name: focused.workspace.name,
                is_focused: true,
            },
        })
    }

    pub fn focused_workspace(&self, displays: &[DisplayData]) -> Result<FocusedWorkspace> {
        for display in displays {
            let Some(space) = display_space(display) else {
                continue;
            };
            let focused_window = self
                .client
                .get_windows(Some(space))?
                .into_iter()
                .find(|window| window.is_focused);
            let Some(focused_window) = focused_window else {
                continue;
            };
            for workspace in self.client.get_workspaces(Some(space))? {
                if workspace
                    .windows
                    .iter()
                    .any(|window| window.id == focused_window.id)
                {
                    return Ok(FocusedWorkspace {
                        display_uuid: display.uuid.clone(),
                        focused_window,
                        workspace,
                    });
                }
            }
        }
        Err(state_error("no focused Rift-managed window"))
    }

    pub fn subscribe(&self) -> Result<RiftMachSubscription> {
        Ok(self.client.subscribe(EventKind::All)?)
    }

    pub fn execute(&self, command: RiftCommand) -> Result<()> {
        self.client.execute(command)?;
        Ok(())
    }

    pub fn focus_destination_command(&self, target: &TargetContext) -> RiftCommand {
        let command = target.focus_anchor.as_ref().map_or_else(
            || {
                ReactorCommand::MoveMouseToDisplay(DisplaySelector::Uuid(
                    target.display_uuid.clone(),
                ))
            },
            |anchor| ReactorCommand::FocusWindow {
                window_id: anchor.id,
                window_server_id: anchor.window_server_id,
            },
        );
        RiftCommand::Reactor(command)
    }

    pub fn activate_workspace_command(&self, target: &TargetContext) -> RiftCommand {
        RiftCommand::Layout(LayoutCommand::SwitchToWorkspace(target.workspace_index))
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
        let window = self.client.get_window_info(window_id)?;
        Ok(frame_matches_display(
            &window.frame,
            target.frame_validation,
        ))
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
                || !frame_matches_display(
                    &self.client.get_window_info(*window_id)?.frame,
                    target.frame_validation,
                )
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
                || !frame_matches_display(
                    &self.client.get_window_info(*window_id)?.frame,
                    target.frame_validation,
                )
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

fn display_space(display: &DisplayData) -> Option<u64> {
    display.active_space_ids.first().copied().or(display.space)
}

fn frame_validation(
    target: &DisplayData,
    displays: &[DisplayData],
    layout_mode: &str,
) -> FrameValidation {
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

    if layout_mode == "scrolling" {
        return FrameValidation::MembershipOnly;
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
