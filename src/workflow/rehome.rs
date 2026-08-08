use rift_client::WindowData;

use crate::policy::{self, RoutedWindow};
use crate::rift::{Rift, TargetContext};
use crate::transaction::{EventExpectation, RiftTransaction};
use crate::{Result, state_error};

use super::placement;

struct RehomeSource {
    display_uuid: String,
    workspace_name: String,
    windows: Vec<WindowData>,
}

struct DestinationGroup {
    display_uuid: String,
    workspace_name: String,
    windows: Vec<RoutedWindow>,
}

pub fn rehome(workspace_name: Option<&str>) -> Result<()> {
    let rift = Rift::connect()?;
    let displays = rift.displays()?;
    let policy = policy::ResolvedPolicy::load(&displays)?;
    let source = select_source(&rift, &displays, &policy, workspace_name)?;
    let routed = policy.route_windows(&source.windows)?;

    if routed.is_empty() {
        return Ok(());
    }

    let groups = group_by_destination(routed);
    if groups_are_at_home(&rift, &groups)? {
        return Ok(());
    }

    let transaction = RiftTransaction::for_batch(&rift, source.windows.len())?;
    for group in &groups {
        place_group(&rift, &transaction, group)?;
    }
    restore_source(&rift, &transaction, &source)?;
    verify_groups(&rift, &groups)
}

fn select_source(
    rift: &Rift,
    displays: &[rift_client::DisplayData],
    policy: &policy::ResolvedPolicy,
    workspace_name: Option<&str>,
) -> Result<RehomeSource> {
    if let Some(workspace_name) = workspace_name {
        let display_uuid = policy.target_display(workspace_name)?;
        let target = rift.target_context(workspace_name, display_uuid.clone(), displays)?;
        if !target.display_is_active || !target.workspace_is_active {
            let transaction = RiftTransaction::new(rift)?;
            placement::prepare_target(rift, &transaction, &target, workspace_name)?;
        }
        let workspace = rift.workspace_at(&target, workspace_name)?;
        return Ok(RehomeSource {
            display_uuid,
            workspace_name: workspace.name,
            windows: workspace.windows,
        });
    }

    let focused = rift.focused_workspace(displays)?;
    Ok(RehomeSource {
        display_uuid: focused.display_uuid,
        workspace_name: focused.workspace.name,
        windows: focused.workspace.windows,
    })
}

fn group_by_destination(routed: Vec<RoutedWindow>) -> Vec<DestinationGroup> {
    let mut groups: Vec<DestinationGroup> = Vec::new();
    for routed_window in routed {
        let existing = groups.iter_mut().find(|group| {
            group.display_uuid == routed_window.home.display_uuid
                && group.workspace_name == routed_window.home.workspace_name
        });
        if let Some(group) = existing {
            group.windows.push(routed_window);
        } else {
            groups.push(DestinationGroup {
                display_uuid: routed_window.home.display_uuid.clone(),
                workspace_name: routed_window.home.workspace_name.clone(),
                windows: vec![routed_window],
            });
        }
    }
    groups
}

fn place_group(
    rift: &Rift,
    transaction: &RiftTransaction<'_>,
    group: &DestinationGroup,
) -> Result<()> {
    let displays = rift.displays()?;
    let target =
        rift.target_context(&group.workspace_name, group.display_uuid.clone(), &displays)?;
    placement::prepare_target(rift, transaction, &target, &group.workspace_name)?;

    let mut transfer_windows = Vec::new();
    for routed in &group.windows {
        if !rift.window_is_on_display(&target, routed.window.id)? {
            transfer_windows.push(&routed.window);
        }
    }
    if !transfer_windows.is_empty() {
        let transfer_ids = transfer_windows
            .iter()
            .map(|window| window.id)
            .collect::<Vec<_>>();
        let commands = transfer_windows
            .iter()
            .map(|window| rift.transfer_window_command(&target, window));
        transaction.confirmed_phase(
            &format!("display placement for workspace {}", group.workspace_name),
            commands,
            EventExpectation::Display(&target.display_uuid),
            |rift| {
                transfer_ids.iter().try_fold(true, |all_placed, window_id| {
                    Ok(all_placed && rift.window_is_on_display(&target, *window_id)?)
                })
            },
        )?;
    }

    let group_ids = group
        .windows
        .iter()
        .map(|routed| routed.window.id)
        .collect::<Vec<_>>();
    let missing =
        rift.windows_missing_from_workspace(&target, &group.workspace_name, &group_ids)?;
    let workspace_windows = group
        .windows
        .iter()
        .filter(|routed| missing.contains(&routed.window.id))
        .map(|routed| &routed.window)
        .collect::<Vec<_>>();
    if !workspace_windows.is_empty() {
        let workspace_ids = workspace_windows
            .iter()
            .map(|window| window.id)
            .collect::<Vec<_>>();
        let commands = workspace_windows
            .iter()
            .map(|window| rift.move_window_to_workspace_command(&target, window, false));
        transaction.confirmed_phase(
            &format!("workspace placement for {}", group.workspace_name),
            commands,
            EventExpectation::Workspace {
                display_uuid: &target.display_uuid,
                workspace_name: &group.workspace_name,
            },
            |rift| rift.windows_are_at(&target, &group.workspace_name, &workspace_ids),
        )?;
    }
    Ok(())
}

fn verify_groups(rift: &Rift, groups: &[DestinationGroup]) -> Result<()> {
    for group in groups {
        let displays = rift.displays()?;
        let target =
            rift.target_context(&group.workspace_name, group.display_uuid.clone(), &displays)?;
        verify_group(rift, group, &target)?;
    }
    Ok(())
}

fn groups_are_at_home(rift: &Rift, groups: &[DestinationGroup]) -> Result<bool> {
    for group in groups {
        let displays = rift.displays()?;
        let target =
            rift.target_context(&group.workspace_name, group.display_uuid.clone(), &displays)?;
        let window_ids = group
            .windows
            .iter()
            .map(|routed| routed.window.id)
            .collect::<Vec<_>>();
        if !rift.windows_are_at(&target, &group.workspace_name, &window_ids)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn verify_group(rift: &Rift, group: &DestinationGroup, target: &TargetContext) -> Result<()> {
    let window_ids = group
        .windows
        .iter()
        .map(|routed| routed.window.id)
        .collect::<Vec<_>>();
    if rift.windows_are_at(target, &group.workspace_name, &window_ids)? {
        return Ok(());
    }
    let labels = group
        .windows
        .iter()
        .map(|routed| routed.home.label.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    Err(state_error(format!(
        "rehome verification failed for {labels}: expected workspace {} on {}",
        group.workspace_name, group.display_uuid
    )))
}

fn restore_source(
    rift: &Rift,
    transaction: &RiftTransaction<'_>,
    source: &RehomeSource,
) -> Result<()> {
    let displays = rift.displays()?;
    let target = rift.target_context(
        &source.workspace_name,
        source.display_uuid.clone(),
        &displays,
    )?;
    placement::prepare_target(rift, transaction, &target, &source.workspace_name)
}
