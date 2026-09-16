//! Desktop notifications for a CLI that runs from keybindings.
//!
//! `exec` bindings discard stderr, so a workflow that fails -- or that refuses
//! to act because the state was ambiguous -- is invisible at the moment it
//! matters. The log records everything, but nobody reads a log after pressing
//! a hotkey. A notification is the only channel that reaches the user in the
//! second they are wondering why nothing moved.
//!
//! Notifications are fired after the work completes and never waited on, so
//! they cost the workflow nothing. `RIFT_ERGO_NOTIFY` selects what is worth
//! interrupting for:
//!
//! - `off`: nothing
//! - `errors`: failures and refusals only
//! - `moves`: the default. Failures, refusals, and commands that relocated
//!   windows. Workspace switches stay silent because they are self-evident on
//!   screen and fire constantly.
//! - `all`: every invocation

use std::env;
use std::process::{Command, Stdio};
use std::sync::Mutex;

const TERMINAL_NOTIFIER: &str = "/opt/homebrew/bin/terminal-notifier";

/// Set by a workflow that wants the notification to say what it did, rather
/// than repeating the command line.
static SUMMARY: Mutex<Option<String>> = Mutex::new(None);

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum Outcome {
    /// Windows were relocated.
    Moved,
    /// Nothing was relocated: a switch, a query, a no-op.
    Quiet,
    /// The workflow refused to act, or failed.
    Failed,
}

#[derive(Clone, Copy, Eq, PartialEq, PartialOrd)]
enum Mode {
    Off,
    Errors,
    Moves,
    All,
}

impl Mode {
    fn from_env() -> Self {
        match env::var("RIFT_ERGO_NOTIFY").as_deref() {
            Ok("off") => Self::Off,
            Ok("errors") => Self::Errors,
            Ok("all") => Self::All,
            _ => Self::Moves,
        }
    }

    fn allows(self, outcome: Outcome) -> bool {
        match outcome {
            Outcome::Failed => self != Self::Off,
            Outcome::Moved => matches!(self, Self::Moves | Self::All),
            Outcome::Quiet => self == Self::All,
        }
    }
}

/// Record what this invocation actually did, for the notification body.
pub fn summarize(summary: impl Into<String>) {
    if let Ok(mut slot) = SUMMARY.lock() {
        *slot = Some(summary.into());
    }
}

/// Whatever `summarize` last recorded.
pub fn summary() -> Option<String> {
    SUMMARY.lock().ok().and_then(|slot| slot.clone())
}

pub fn notify(outcome: Outcome, title: &str, message: &str) {
    if !Mode::from_env().allows(outcome) {
        return;
    }
    let spawned = Command::new(TERMINAL_NOTIFIER)
        .args([
            "-title",
            title,
            "-message",
            message,
            "-group",
            "rift-ergo",
            "-sender",
            "com.apple.Terminal",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if spawned.is_ok() {
        return;
    }
    // terminal-notifier is a Homebrew package, so it can disappear under us.
    let script = format!(
        "display notification {} with title {}",
        applescript_string(message),
        applescript_string(title)
    );
    let _ = Command::new("/usr/bin/osascript")
        .args(["-e", &script])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

fn applescript_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_gate_outcomes() {
        assert!(Mode::Errors.allows(Outcome::Failed));
        assert!(!Mode::Errors.allows(Outcome::Moved));
        assert!(Mode::Moves.allows(Outcome::Moved));
        assert!(!Mode::Moves.allows(Outcome::Quiet));
        assert!(Mode::All.allows(Outcome::Quiet));
        assert!(!Mode::Off.allows(Outcome::Failed));
    }

    #[test]
    fn applescript_strings_are_escaped() {
        assert_eq!(applescript_string(r#"a "b" \c"#), r#""a \"b\" \\c""#);
    }
}
