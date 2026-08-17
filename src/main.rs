mod policy;
mod rift;
mod space_escape;
mod transaction;
mod workflow;

use std::env;
use std::error::Error;
use std::io;

type DynError = Box<dyn Error + Send + Sync>;
type Result<T> = std::result::Result<T, DynError>;

fn main() {
    if let Err(error) = run() {
        eprintln!("rift-ergo: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut args = env::args().skip(1);
    let command = args.next().ok_or_else(|| usage_error("missing command"))?;
    match command.as_str() {
        "move-follow" => {
            let workspace = args
                .next()
                .ok_or_else(|| usage_error("missing workspace"))?;
            require_no_more_arguments(&mut args)?;
            workflow::move_follow(&workspace)
        }
        "switch-workspace" => {
            let workspace = args
                .next()
                .ok_or_else(|| usage_error("missing workspace"))?;
            require_no_more_arguments(&mut args)?;
            workflow::switch_workspace(&workspace)
        }
        "switch-workspace-both" => {
            let workspace = args
                .next()
                .ok_or_else(|| usage_error("missing workspace"))?;
            require_no_more_arguments(&mut args)?;
            workflow::switch_workspace_both(&workspace)
        }
        "move-window-to-display" => {
            let direction = parse_display_direction(&mut args)?;
            require_no_more_arguments(&mut args)?;
            workflow::move_window_to_display(direction)
        }
        "move-workspace-to-display" => {
            let direction = parse_display_direction(&mut args)?;
            require_no_more_arguments(&mut args)?;
            workflow::move_workspace_to_display(direction)
        }
        "rehome" => {
            let workspace = args.next();
            require_no_more_arguments(&mut args)?;
            workflow::rehome(workspace.as_deref())
        }
        "-h" | "--help" | "help" => {
            print_usage();
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
  rift-ergo move-window-to-display <next|prev>
  rift-ergo move-workspace-to-display <next|prev>
  rift-ergo rehome [workspace-name]"
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
  rift-ergo move-window-to-display <next|prev>
  rift-ergo move-workspace-to-display <next|prev>
  rift-ergo rehome [workspace-name]",
            message.into()
        ),
    )
    .into()
}

fn state_error(message: impl Into<String>) -> DynError {
    io::Error::other(message.into()).into()
}
