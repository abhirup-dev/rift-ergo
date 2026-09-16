use serde::Deserialize;

use rift_client::WindowId;

use crate::rift::Rift;
use crate::{Result, focus_lease, logging};

use super::switch_workspace::activate_on_display;

/// Rift publishes this shape on the `focused_window_changed` subscription.
/// Deserialized loosely on purpose: only these fields are needed, and an
/// unknown or reshaped event must not fail a hook that runs on every focus
/// change.
#[derive(Deserialize)]
struct FocusEvent {
    #[serde(rename = "type")]
    kind: String,
    window_id: WindowId,
    space_id: u64,
    display_uuid: Option<String>,
}

const EVENT_KIND: &str = "focused_window_changed";

/// Bring a focused window's workspace forward when Rift did not.
///
/// Rift auto-switches workspaces only while handling `ApplicationActivated`,
/// and only for the app's current main window. An app that moves focus to a
/// window Rift does not manage right after being activated -- Zoom focuses an
/// untitled helper window the moment it comes forward -- replaces that main
/// window before Rift reads it, so the switch is dropped and the real window
/// keeps focus while parked offscreen in a hidden workspace.
///
/// The focus event itself is published either way, so this hook acts on what
/// Rift observed rather than on what it decided. Registered through
/// `rift-cli subscribe cli`, it runs once per focus change and exits without
/// touching Rift in every case but the one it exists for.
///
/// `RIFT_ERGO_FOCUS_FOLLOW=off` disables it; `=dry-run` logs the switch it
/// would make without making it.
pub fn on_focus_change(event_json: Option<&str>) -> Result<()> {
    let Some(raw) = event_json else {
        return Ok(());
    };
    let mode = Mode::from_env();
    if mode == Mode::Off {
        return Ok(());
    }
    let Ok(event) = serde_json::from_str::<FocusEvent>(raw) else {
        logging::warn(&format!("focus-follow: unreadable event {raw}"));
        return Ok(());
    };
    if event.kind != EVENT_KIND {
        return Ok(());
    }
    let Some(display_uuid) = event.display_uuid else {
        return Ok(());
    };

    // Our own workflows raise hidden windows to move Rift's command context.
    // Answering those with a workspace switch would fight them mid-flight.
    if focus_lease::active() {
        logging::trace("focus-follow: suppressed, a workflow holds the lease");
        return Ok(());
    }
    let Some(_single_flight) = focus_lease::try_single_flight() else {
        logging::trace("focus-follow: suppressed, another hook is running");
        return Ok(());
    };

    let rift = Rift::connect()?;
    let workspaces = rift.workspaces(event.space_id)?;
    // Untracked windows -- Zoom's helper among them -- belong to no workspace.
    let Some(home) = workspaces.iter().find(|workspace| {
        workspace
            .windows
            .iter()
            .any(|window| window.id == event.window_id)
    }) else {
        return Ok(());
    };
    if home.is_active {
        return Ok(());
    }

    let summary = format!(
        "focus-follow: window {}:{} is in hidden workspace {} on {display_uuid}",
        event.window_id.pid, event.window_id.idx, home.name
    );
    if mode == Mode::DryRun {
        logging::info(&format!("{summary} (dry-run)"));
        return Ok(());
    }
    logging::info(&summary);

    focus_lease::renew();
    let outcome = activate_on_display(&rift, &home.name, &display_uuid, rift.displays()?);
    focus_lease::release();
    outcome?;
    Ok(())
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Mode {
    On,
    DryRun,
    Off,
}

impl Mode {
    fn from_env() -> Self {
        match std::env::var("RIFT_ERGO_FOCUS_FOLLOW").as_deref() {
            Ok("off") => Self::Off,
            Ok("dry-run") => Self::DryRun,
            _ => Self::On,
        }
    }
}
