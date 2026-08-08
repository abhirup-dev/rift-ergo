use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::path::PathBuf;

use rift_client::DisplayData;
use serde::Deserialize;

use crate::{Result, state_error};

#[derive(Debug, Deserialize)]
struct Policy {
    display_aliases: HashMap<String, DisplayAlias>,
    profiles: Vec<Profile>,
}

#[derive(Debug, Deserialize)]
struct DisplayAlias {
    uuid: Option<String>,
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Profile {
    name: String,
    #[serde(default)]
    required_display_aliases: Vec<String>,
    fallback_display: Option<String>,
    #[serde(default)]
    workspace_displays: HashMap<String, String>,
}

pub fn target_display(workspace_name: &str, displays: &[DisplayData]) -> Result<String> {
    let policy = load_policy()?;
    let aliases = resolve_display_aliases(&policy, displays);
    let profile = select_profile(&policy, &aliases)?;
    resolve_target_display(profile, workspace_name, &aliases, displays)
}

fn load_policy() -> Result<Policy> {
    Ok(serde_json::from_slice(&fs::read(policy_path()?)?)?)
}

fn policy_path() -> Result<PathBuf> {
    let home = env::var_os("HOME")
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "HOME is not set"))?;
    Ok(PathBuf::from(home).join(".config/rift/reconcile/workspace-assignments.json"))
}

fn resolve_display_aliases(policy: &Policy, displays: &[DisplayData]) -> HashMap<String, String> {
    policy
        .display_aliases
        .iter()
        .filter_map(|(alias, configured)| {
            displays
                .iter()
                .find(|display| {
                    configured.uuid.as_deref() == Some(display.uuid.as_str())
                        || configured.name.as_deref() == display.name.as_deref()
                })
                .map(|display| (alias.clone(), display.uuid.clone()))
        })
        .collect()
}

fn select_profile<'a>(
    policy: &'a Policy,
    aliases: &HashMap<String, String>,
) -> Result<&'a Profile> {
    let available: HashSet<&str> = aliases.keys().map(String::as_str).collect();
    policy
        .profiles
        .iter()
        .find(|profile| {
            profile
                .required_display_aliases
                .iter()
                .all(|required| available.contains(required.as_str()))
        })
        .ok_or_else(|| state_error("no monitor profile matches the connected displays"))
}

fn resolve_target_display(
    profile: &Profile,
    workspace_name: &str,
    aliases: &HashMap<String, String>,
    displays: &[DisplayData],
) -> Result<String> {
    let display_ref = profile
        .workspace_displays
        .get(workspace_name)
        .or(profile.fallback_display.as_ref())
        .ok_or_else(|| {
            state_error(format!(
                "profile {} has no display for workspace {workspace_name}",
                profile.name
            ))
        })?;

    if display_ref == "only" {
        return match displays {
            [display] => Ok(display.uuid.clone()),
            _ => Err(state_error(
                "profile requires exactly one display, but multiple are connected",
            )),
        };
    }

    aliases.get(display_ref).cloned().ok_or_else(|| {
        state_error(format!(
            "profile {} refers to unavailable display alias {display_ref}",
            profile.name
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rift_client::{Point, Rect, Size};

    fn display(uuid: &str, name: &str) -> DisplayData {
        DisplayData {
            uuid: uuid.into(),
            name: Some(name.into()),
            screen_id: 1,
            frame: Rect {
                origin: Point { x: 0.0, y: 0.0 },
                size: Size {
                    width: 100.0,
                    height: 100.0,
                },
            },
            space: Some(1),
            is_active_space: true,
            is_active_context: true,
            active_space_ids: vec![1],
            inactive_space_ids: vec![],
        }
    }

    #[test]
    fn selects_first_profile_with_available_aliases() {
        let policy = Policy {
            display_aliases: HashMap::from([
                (
                    "builtin".into(),
                    DisplayAlias {
                        uuid: Some("built".into()),
                        name: None,
                    },
                ),
                (
                    "external".into(),
                    DisplayAlias {
                        uuid: Some("ext".into()),
                        name: None,
                    },
                ),
            ]),
            profiles: vec![
                Profile {
                    name: "home".into(),
                    required_display_aliases: vec!["builtin".into(), "external".into()],
                    fallback_display: Some("builtin".into()),
                    workspace_displays: HashMap::new(),
                },
                Profile {
                    name: "single".into(),
                    required_display_aliases: vec![],
                    fallback_display: Some("only".into()),
                    workspace_displays: HashMap::new(),
                },
            ],
        };
        let displays = vec![display("built", "Built-in"), display("ext", "External")];
        let aliases = resolve_display_aliases(&policy, &displays);

        assert_eq!(select_profile(&policy, &aliases).unwrap().name, "home");
    }
}
