mod dock_swipe;
mod focus_lease;
mod key_window;
mod logging;
mod native_focus;
mod notify;
mod policy;
mod rift;
mod space_escape;
mod transaction;
mod workflow;

use std::env;
use std::error::Error;
use std::io;
use std::time::Instant;

type DynError = Box<dyn Error + Send + Sync>;
type Result<T> = std::result::Result<T, DynError>;

fn main() {
    let invocation: Vec<String> = env::args().skip(1).collect();
    // Hooks are invoked with the whole event JSON appended, which is unreadable
    // in a notification title; the command alone identifies what ran.
    let command = invocation.first().cloned().unwrap_or_default();
    let started = Instant::now();
    logging::info(&format!("invoke [{}]", invocation.join(" ")));

    match run() {
        Ok(()) => {
            let elapsed = started.elapsed().as_millis();
            logging::info(&format!("ok [{}] in {} ms", invocation.join(" "), elapsed));
            // One report, one notification. A workflow that refused reports it
            // and returns Ok, so the refusal is announced exactly once.
            match notify::report() {
                Some(report) => notify::notify(
                    report.outcome,
                    &format!("rift-ergo: {command}"),
                    &format!("{} ({elapsed} ms)", report.message),
                ),
                None => notify::notify(
                    notify::Outcome::Quiet,
                    &format!("rift-ergo: {command}"),
                    &format!("done in {elapsed} ms"),
                ),
            }
        }
        Err(error) => {
            // Rift's Mach transport surfaces its own failures verbatim; attribute
            // those to Rift so an upgrade regression is not mistaken for ours.
            let text = error.to_string();
            let source = if text.contains("Mach service") || text.contains("Communication error") {
                "rift"
            } else {
                "rift-ergo"
            };
            logging::error(
                source,
                &format!(
                    "failed [{}] after {} ms: {text}",
                    invocation.join(" "),
                    started.elapsed().as_millis()
                ),
            );
            notify::notify(
                notify::Outcome::Failed,
                &format!("rift-ergo: {command} failed"),
                &text,
            );
            eprintln!("rift-ergo: {error}");
            std::process::exit(1);
        }
    }
}

fn run() -> Result<()> {
    let mut args = env::args().skip(1);
    let command = args.next().ok_or_else(|| usage_error("missing command"))?;

    // Commands that move focus hold a lease for as long as they drive Rift, so
    // the focus-follow hook does not answer their own focus moves with a
    // workspace switch. `on-focus-change` manages the lease itself.
    let holds_focus_lease = matches!(
        command.as_str(),
        "move-follow"
            | "switch-workspace"
            | "switch-workspace-both"
            | "switch-workspace-focused"
            | "move-window-to-display"
            | "move-workspace-to-display"
            | "rehome"
            | "on-windows-changed"
            | "prune-layout"
    );
    if holds_focus_lease {
        focus_lease::renew();
    }
    let outcome = dispatch(&command, &mut args);
    if holds_focus_lease {
        focus_lease::release();
    }
    outcome
}

fn dispatch(command: &str, args: &mut impl Iterator<Item = String>) -> Result<()> {
    match command {
        "move-follow" => {
            let workspace = args
                .next()
                .ok_or_else(|| usage_error("missing workspace"))?;
            require_no_more_arguments(args)?;
            workflow::move_follow(&workspace)
        }
        "switch-workspace" => {
            let workspace = args
                .next()
                .ok_or_else(|| usage_error("missing workspace"))?;
            require_no_more_arguments(args)?;
            workflow::switch_workspace(&workspace)
        }
        "switch-workspace-both" => {
            let workspace = args
                .next()
                .ok_or_else(|| usage_error("missing workspace"))?;
            require_no_more_arguments(args)?;
            workflow::switch_workspace_both(&workspace)
        }
        "switch-workspace-focused" => {
            let workspace = args
                .next()
                .ok_or_else(|| usage_error("missing workspace"))?;
            require_no_more_arguments(args)?;
            workflow::switch_workspace_focused(&workspace)
        }
        "move-window-to-display" => {
            let direction = parse_display_direction(args)?;
            require_no_more_arguments(args)?;
            workflow::move_window_to_display(direction)
        }
        "move-workspace-to-display" => {
            let direction = parse_display_direction(args)?;
            require_no_more_arguments(args)?;
            workflow::move_workspace_to_display(direction)
        }
        "prune-layout" => {
            require_no_more_arguments(args)?;
            workflow::prune_layout()
        }
        "on-windows-changed" => {
            let first = args.next();
            require_no_more_arguments(args)?;
            match first.as_deref() {
                Some("--init") => workflow::init_seen(),
                event => workflow::on_windows_changed(event),
            }
        }
        "on-focus-change" => {
            let event = args.next();
            require_no_more_arguments(args)?;
            workflow::on_focus_change(event.as_deref())
        }
        "rehome" => {
            let workspace = args.next();
            require_no_more_arguments(args)?;
            workflow::rehome(workspace.as_deref())
        }
        "-h" | "--help" | "help" => {
            print_usage();
            Ok(())
        }
        "--log-path" => {
            require_no_more_arguments(args)?;
            println!("{}", logging::describe());
            Ok(())
        }
        _ => Err(usage_error(format!("unknown command: {command}"))),
    }
}

fn print_usage() {
    println!(
        "usage:
  rift-ergo move-follow <workspace-name>
  rift-ergo switch-workspace <workspace-name>
  rift-ergo switch-workspace-both <workspace-name>
  rift-ergo switch-workspace-focused <workspace-name>
  rift-ergo move-window-to-display <next|prev>
  rift-ergo move-workspace-to-display <next|prev>
  rift-ergo on-focus-change <event-json>
  rift-ergo on-windows-changed [--init | <event-json>]
  rift-ergo rehome [workspace-name]\n  rift-ergo --log-path"
    );
}

fn parse_display_direction(
    args: &mut impl Iterator<Item = String>,
) -> Result<workflow::DisplayDirection> {
    let direction = args
        .next()
        .ok_or_else(|| usage_error("missing display direction"))?;
    workflow::DisplayDirection::parse(&direction)
}

fn require_no_more_arguments(args: &mut impl Iterator<Item = String>) -> Result<()> {
    if args.next().is_some() {
        return Err(usage_error("too many arguments"));
    }
    Ok(())
}

fn usage_error(message: impl Into<String>) -> DynError {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
            "{}
usage:
  rift-ergo move-follow <workspace-name>
  rift-ergo switch-workspace <workspace-name>
  rift-ergo switch-workspace-both <workspace-name>
  rift-ergo switch-workspace-focused <workspace-name>
  rift-ergo move-window-to-display <next|prev>
  rift-ergo move-workspace-to-display <next|prev>
  rift-ergo on-focus-change <event-json>
  rift-ergo rehome [workspace-name]",
            message.into()
        ),
    )
    .into()
}

fn state_error(message: impl Into<String>) -> DynError {
    io::Error::other(message.into()).into()
}
