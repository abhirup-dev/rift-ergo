use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::thread;
use std::time::Duration;

use rift_client::{WindowData, WindowId};
use serde::Deserialize;

use crate::policy::{ResolvedPolicy, RoutedWindow};
use crate::rift::{Rift, TargetContext};
use crate::transaction::{EventExpectation, RiftTransaction};
use crate::{Result, focus_lease, logging, notify};

use super::placement;

/// Shared with the Python reconciler this replaces, so the two can be swapped
/// back and forth without a window being re-placed on the next event.
/// `RIFT_ERGO_NEW_WINDOW_STATE` points the baseline elsewhere, which is how a
/// dry run can be compared against the implementation currently in charge
/// without stealing its state.
const SEEN_PATH: &str = "/tmp/rift-new-window-seen.json";
const READY_PATH: &str = "/tmp/rift-new-window-ready";
const LOCK_PATH: &str = "/tmp/rift-new-window.lock";
const LAUNCHD_LABEL: &str = "sh.brew.rift";

/// Rift reports a window before its frame and workspace have settled.
const SETTLE: Duration = Duration::from_millis(200);
const PLACEMENT_TIMEOUT: Duration = Duration::from_millis(2_000);

#[derive(Deserialize)]
struct WindowsChangedEvent {
    #[serde(rename = "type")]
    kind: String,
    /// `WindowId { pid: 1, idx: 2 }` debug strings, which the protocol's
    /// `WindowId` deserializer accepts directly.
    #[serde(default)]
    windows: Vec<WindowId>,
}

const EVENT_KIND: &str = "windows_changed";

/// Place newly created windows on their configured display.
///
/// Rift's own `app_rules` already assign a workspace, but they resolve it
/// inside the space the window was born in -- so the rule means "workspace 7 of
/// whichever display macOS happened to open this app on". No version of Rift
/// can express "workspace 7 of the builtin", which is the whole reason this
/// hook exists. It fixes the display, then re-asserts the workspace, because
/// `MoveWindowToDisplay` drops a window into whatever workspace the destination
/// is showing.
///
/// Deliberately not a reconciler over all windows: each window is placed once,
/// the first time it is seen, so moving a window yourself afterwards sticks.
///
/// `RIFT_ERGO_NEW_WINDOW=off` disables it; `=dry-run` logs placements without
/// making them.
pub fn on_windows_changed(event_json: Option<&str>) -> Result<()> {
    let mode = Mode::from_env();
    if mode == Mode::Off {
        return Ok(());
    }
    if !rift_is_ready() {
        // Events that arrive before the startup pass would treat every existing
        // window as new and reorganize the desktop.
        logging::trace("new-window: not ready, skipping");
        return Ok(());
    }
    let Some(_single_flight) = focus_lease::try_single_flight_at(PathBuf::from(LOCK_PATH)) else {
        logging::trace("new-window: another pass is running");
        return Ok(());
    };
    thread::sleep(SETTLE);

    let rift = Rift::connect()?;
    let displays = rift.displays()?;
    let policy = ResolvedPolicy::load(&displays)?;
    let located = rift.window_locations(&displays)?;
    let mut seen = load_seen();
    let mut candidates: Vec<(Option<(String, String)>, WindowData)> = located
        .into_iter()
        .map(|(display_uuid, workspace_name, window)| {
            (Some((display_uuid, workspace_name)), window)
        })
        .collect();

    // A just-launched window is addressable by exact id before it shows up in a
    // workspace snapshot. Warp exposed this race; Rift names the ids in the
    // event, so ask for them directly.
    for window_id in event_window_ids(event_json) {
        if candidates.iter().any(|(_, window)| window.id == window_id) {
            continue;
        }
        if let Some(window) = rift.window_info(window_id) {
            candidates.push((None, window));
        }
    }

    let mut placed = 0usize;
    let mut changed = false;
    for (location, window) in candidates {
        let key = window_key(&window);
        if !seen.insert(key.clone()) {
            continue;
        }
        changed = true;
        let Some(routed) = policy.route_windows(std::slice::from_ref(&window))?.pop() else {
            logging::trace(&format!(
                "new-window: no rule for {key} app={:?}",
                window.app_name
            ));
            continue;
        };
        if is_home(&location, &routed) {
            continue;
        }
        if mode == Mode::DryRun {
            logging::info(&format!(
                "new-window: would place {key} {:?} at workspace {} on {} (dry-run)",
                window.app_name, routed.home.workspace_name, routed.home.display_uuid
            ));
            continue;
        }
        match place(&rift, &routed, location.as_ref()) {
            Ok(()) => placed += 1,
            // One window failing to land must not abandon the rest, and the
            // window stays marked seen: retrying on every later event is how
            // the old reconciler fought the user for placement.
            Err(error) => logging::warn(&format!("new-window: placing {key} failed: {error}")),
        }
    }

    if changed {
        save_seen(&seen);
    }
    if placed > 0 {
        notify::moved(format!(
            "placed {placed} new window{}",
            if placed == 1 { "" } else { "s" }
        ));
    }
    // A destroyed window can leave its layout node behind, and nothing in Rift
    // ever re-checks. There is no event for it, so this rides along on window
    // activity that does fire. Its failures are reported rather than swallowed:
    // the shell script this replaces hid a wrong CLI argument behind a bare
    // `except Exception` and silently did nothing for months.
    if event_json.is_some()
        && let Err(error) = super::prune_layout::prune_layout()
    {
        logging::warn(&format!("new-window: prune failed: {error}"));
    }
    Ok(())
}

/// Record every window Rift currently manages as already placed.
///
/// Runs once at Rift startup: a restart must not reorganize the desktop, only
/// establish the baseline that later windows are new relative to.
pub fn init_seen() -> Result<()> {
    let rift = Rift::connect()?;
    let displays = rift.displays()?;
    let seen: BTreeSet<String> = rift
        .window_locations(&displays)?
        .iter()
        .map(|(_, _, window)| window_key(window))
        .collect();
    let count = seen.len();
    save_seen(&seen);
    let _ = fs::write(READY_PATH, rift_pid().unwrap_or_default());
    logging::info(&format!("new-window: baseline recorded, {count} windows"));
    Ok(())
}

fn place(rift: &Rift, routed: &RoutedWindow, location: Option<&(String, String)>) -> Result<()> {
    let window = &routed.window;
    let displays = rift.displays()?;
    let target = rift.target_context(
        &routed.home.workspace_name,
        routed.home.display_uuid.clone(),
        &displays,
    )?;
    let transaction = RiftTransaction::new(rift)?;

    let on_home_display =
        location.is_some_and(|(display_uuid, _)| *display_uuid == routed.home.display_uuid);
    if !on_home_display {
        transaction.confirmed_step_with_timeout(
            &format!("display placement for {:?}", window.app_name),
            rift.transfer_window_command(&target, window),
            EventExpectation::Display(&target.display_uuid),
            |rift| rift.window_is_on_display(&target, window.id),
            PLACEMENT_TIMEOUT,
        )?;
    }

    // The transfer raises the window on the destination, which changes which
    // display Rift routes layout commands to, so the context is re-read rather
    // than reused.
    let displays = rift.displays()?;
    let target = rift.target_context(
        &routed.home.workspace_name,
        routed.home.display_uuid.clone(),
        &displays,
    )?;
    if rift.windows_are_at(&target, &routed.home.workspace_name, &[window.id])? {
        return Ok(());
    }
    // A workspace command for a window on any other display is silently
    // ignored, so the window's display has to be the active one first.
    placement::acquire_display(rift, &transaction, &target, &routed.home.workspace_name)?;
    assert_workspace(rift, &transaction, &target, routed)
}

fn assert_workspace(
    rift: &Rift,
    transaction: &RiftTransaction<'_>,
    target: &TargetContext,
    routed: &RoutedWindow,
) -> Result<()> {
    let window = &routed.window;
    transaction.confirmed_step_with_timeout(
        &format!("workspace placement for {:?}", window.app_name),
        rift.move_window_to_workspace_command(target, window, false),
        EventExpectation::Workspace {
            display_uuid: &target.display_uuid,
            workspace_name: &routed.home.workspace_name,
        },
        |rift| rift.windows_are_at(target, &routed.home.workspace_name, &[window.id]),
        PLACEMENT_TIMEOUT,
    )
}

fn is_home(location: &Option<(String, String)>, routed: &RoutedWindow) -> bool {
    location
        .as_ref()
        .is_some_and(|(display_uuid, workspace_name)| {
            *display_uuid == routed.home.display_uuid
                && *workspace_name == routed.home.workspace_name
        })
}

fn window_key(window: &WindowData) -> String {
    format!("{}:{}", window.id.pid, window.id.idx)
}

fn event_window_ids(event_json: Option<&str>) -> Vec<WindowId> {
    let Some(raw) = event_json else {
        return Vec::new();
    };
    match serde_json::from_str::<WindowsChangedEvent>(raw) {
        Ok(event) if event.kind == EVENT_KIND => event.windows,
        Ok(_) => Vec::new(),
        Err(error) => {
            logging::warn(&format!("new-window: unreadable event: {error}"));
            Vec::new()
        }
    }
}

fn seen_path() -> PathBuf {
    std::env::var_os("RIFT_ERGO_NEW_WINDOW_STATE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(SEEN_PATH))
}

fn load_seen() -> BTreeSet<String> {
    fs::read_to_string(seen_path())
        .ok()
        .and_then(|contents| serde_json::from_str(&contents).ok())
        .unwrap_or_default()
}

fn save_seen(seen: &BTreeSet<String>) {
    if let Ok(contents) = serde_json::to_string(seen) {
        let _ = fs::write(seen_path(), contents);
    }
}

/// Whether the startup baseline belongs to the Rift now running.
fn rift_is_ready() -> bool {
    match (fs::read_to_string(READY_PATH).ok(), rift_pid()) {
        (Some(recorded), Some(running)) => recorded.trim() == running,
        _ => false,
    }
}

/// The pid of the Rift the baseline was recorded against.
///
/// Read from launchd rather than the process table. `pgrep` returns nothing in
/// the environment Rift spawns hooks in -- the Python reconciler this replaces
/// used `pgrep` for the same check, so its guard never passed and it never
/// placed a window. Matching a process *name* instead only moved the problem:
/// the binary is not always called `rift`, and a renamed build silently
/// disabled the hook again. The launchd label is the one identifier that does
/// not change when the binary does.
fn rift_pid() -> Option<String> {
    let output = Command::new("/bin/launchctl")
        .args(["list", LAUNCHD_LABEL])
        .output()
        .ok()?;
    let listing = String::from_utf8_lossy(&output.stdout);
    let pid = listing
        .lines()
        .find_map(|line| line.split_once("\"PID\" ="))
        .map(|(_, rest)| rest.trim().trim_end_matches(';').trim())
        .filter(|pid| !pid.is_empty())
        .map(str::to_string);
    pid.or_else(rift_pid_from_process_table)
}

/// Fallback for a Rift started by hand rather than by launchd.
fn rift_pid_from_process_table() -> Option<String> {
    let output = Command::new("/bin/ps")
        .args(["-Ao", "pid=,comm="])
        .output()
        .ok()?;
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| {
            let (pid, command) = line.trim().split_once(char::is_whitespace)?;
            let name = command.trim().rsplit('/').next()?;
            (name == "rift").then(|| pid.trim().to_string())
        })
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Mode {
    On,
    DryRun,
    Off,
}

impl Mode {
    fn from_env() -> Self {
        match std::env::var("RIFT_ERGO_NEW_WINDOW").as_deref() {
            Ok("off") => Self::Off,
            Ok("dry-run") => Self::DryRun,
            _ => Self::On,
        }
    }
}
