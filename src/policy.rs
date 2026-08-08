use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::path::PathBuf;

use rift_client::{DisplayData, WindowData};
use serde::Deserialize;

use crate::{Result, state_error};

const POLICY_ENV: &str = "RIFT_ERGO_POLICY";

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
    #[serde(default)]
    rules: Vec<Rule>,
}

#[derive(Debug, Deserialize)]
struct Rule {
    label: String,
    #[serde(default)]
    bundle_ids: Vec<String>,
    #[serde(default)]
    app_names: Vec<String>,
    title_substring: Option<String>,
    workspace: String,
    display: Option<String>,
}

#[derive(Clone, Debug)]
pub struct WindowHome {
    pub label: String,
    pub workspace_name: String,
    pub display_uuid: String,
}

#[derive(Clone, Debug)]
pub struct RoutedWindow {
    pub window: WindowData,
    pub home: WindowHome,
}

pub struct ResolvedPolicy {
    policy: Policy,
    aliases: HashMap<String, String>,
    profile_index: usize,
    displays: Vec<DisplayData>,
}

pub fn target_display(workspace_name: &str, displays: &[DisplayData]) -> Result<String> {
    ResolvedPolicy::load(displays)?.target_display(workspace_name)
}

impl ResolvedPolicy {
    pub fn load(displays: &[DisplayData]) -> Result<Self> {
        let policy = load_policy()?;
        let aliases = resolve_display_aliases(&policy, displays);
        let profile_index = select_profile_index(&policy, &aliases)?;
        Ok(Self {
            policy,
            aliases,
            profile_index,
            displays: displays.to_vec(),
        })
    }

    pub fn target_display(&self, workspace_name: &str) -> Result<String> {
        resolve_target_display(
            self.profile(),
            workspace_name,
            &self.aliases,
            &self.displays,
        )
    }

    pub fn route_windows(&self, windows: &[WindowData]) -> Result<Vec<RoutedWindow>> {
        let mut routed = Vec::new();
        for window in windows {
            let Some(rule) = self
                .profile()
                .rules
                .iter()
                .find(|rule| rule.matches(window))
            else {
                continue;
            };
            routed.push(RoutedWindow {
                window: window.clone(),
                home: WindowHome {
                    label: rule.label.clone(),
                    workspace_name: rule.workspace.clone(),
                    display_uuid: resolve_rule_display(
                        self.profile(),
                        rule,
                        &self.aliases,
                        &self.displays,
                    )?,
                },
            });
        }
        Ok(routed)
    }

    fn profile(&self) -> &Profile {
        &self.policy.profiles[self.profile_index]
    }
}

impl Rule {
    fn matches(&self, window: &WindowData) -> bool {
        window
            .bundle_id
            .as_ref()
            .is_some_and(|bundle_id| self.bundle_ids.contains(bundle_id))
            || window
                .app_name
                .as_ref()
                .is_some_and(|app_name| self.app_names.contains(app_name))
            || self
                .title_substring
                .as_ref()
                .is_some_and(|needle| window.title.contains(needle))
    }
}

fn load_policy() -> Result<Policy> {
    Ok(serde_json::from_slice(&fs::read(policy_path()?)?)?)
}

fn policy_path() -> Result<PathBuf> {
    if let Some(path) = env::var_os(POLICY_ENV).filter(|path| !path.is_empty()) {
        return Ok(PathBuf::from(path));
    }
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

fn select_profile_index(policy: &Policy, aliases: &HashMap<String, String>) -> Result<usize> {
    let available: HashSet<&str> = aliases.keys().map(String::as_str).collect();
    policy
        .profiles
        .iter()
        .position(|profile| {
            profile
                .required_display_aliases
                .iter()
                .all(|required| available.contains(required.as_str()))
        })
        .ok_or_else(|| state_error("no monitor profile matches the connected displays"))
}

fn resolve_rule_display(
    profile: &Profile,
    rule: &Rule,
    aliases: &HashMap<String, String>,
    displays: &[DisplayData],
) -> Result<String> {
    let display_ref = rule
        .display
        .as_ref()
        .or_else(|| profile.workspace_displays.get(&rule.workspace))
        .or(profile.fallback_display.as_ref())
        .ok_or_else(|| {
            state_error(format!(
                "profile {} has no display for rule {}",
                profile.name, rule.label
            ))
        })?;

    match resolve_display_ref(display_ref, aliases, displays) {
        Ok(display_uuid) => Ok(display_uuid),
        Err(primary_error) => {
            let Some(fallback) = profile
                .fallback_display
                .as_ref()
                .filter(|fallback| fallback.as_str() != display_ref)
            else {
                return Err(primary_error);
            };
            resolve_display_ref(fallback, aliases, displays)
        }
    }
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

    resolve_display_ref(display_ref, aliases, displays).map_err(|_| {
        state_error(format!(
            "profile {} refers to unavailable display alias {display_ref}",
            profile.name
        ))
    })
}

fn resolve_display_ref(
    display_ref: &str,
    aliases: &HashMap<String, String>,
    displays: &[DisplayData],
) -> Result<String> {
    if display_ref == "only" {
        return match displays {
            [display] => Ok(display.uuid.clone()),
            _ => Err(state_error(
                "profile requires exactly one display, but multiple are connected",
            )),
        };
    }

    aliases
        .get(display_ref)
        .cloned()
        .ok_or_else(|| state_error(format!("unavailable display alias {display_ref}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rift_client::{Point, Rect, Size, WindowId};

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

    fn profile(name: &str, required: &[&str], fallback: &str) -> Profile {
        Profile {
            name: name.into(),
            required_display_aliases: required.iter().map(|value| (*value).into()).collect(),
            fallback_display: Some(fallback.into()),
            workspace_displays: HashMap::new(),
            rules: Vec::new(),
        }
    }

    fn window() -> WindowData {
        WindowData {
            id: WindowId::new(42, 7).unwrap(),
            title: "Project Settings".into(),
            frame: Rect {
                origin: Point { x: 0.0, y: 0.0 },
                size: Size {
                    width: 100.0,
                    height: 100.0,
                },
            },
            is_floating: false,
            is_focused: true,
            bundle_id: Some("com.example.app".into()),
            app_name: Some("Example".into()),
            window_server_id: Some(7),
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
                profile("home", &["builtin", "external"], "builtin"),
                profile("single", &[], "only"),
            ],
        };
        let displays = vec![display("built", "Built-in"), display("ext", "External")];
        let aliases = resolve_display_aliases(&policy, &displays);

        let selected = select_profile_index(&policy, &aliases).unwrap();
        assert_eq!(policy.profiles[selected].name, "home");
    }

    #[test]
    fn rule_matches_bundle_app_or_title() {
        let rules = [
            Rule {
                label: "Bundle".into(),
                bundle_ids: vec!["com.example.app".into()],
                app_names: Vec::new(),
                title_substring: None,
                workspace: "1".into(),
                display: None,
            },
            Rule {
                label: "App".into(),
                bundle_ids: Vec::new(),
                app_names: vec!["Example".into()],
                title_substring: None,
                workspace: "1".into(),
                display: None,
            },
            Rule {
                label: "Title".into(),
                bundle_ids: Vec::new(),
                app_names: Vec::new(),
                title_substring: Some("Project".into()),
                workspace: "1".into(),
                display: None,
            },
        ];

        assert!(rules.iter().all(|rule| rule.matches(&window())));
    }

    #[test]
    fn resolved_policy_routes_matching_windows_once() {
        let displays = vec![display("built", "Built-in"), display("ext", "External")];
        let mut selected = profile("home", &["builtin", "external"], "builtin");
        selected.rules.push(Rule {
            label: "Example".into(),
            bundle_ids: vec!["com.example.app".into()],
            app_names: Vec::new(),
            title_substring: None,
            workspace: "W".into(),
            display: Some("external".into()),
        });
        let policy = Policy {
            display_aliases: HashMap::new(),
            profiles: vec![selected],
        };
        let resolved = ResolvedPolicy {
            policy,
            aliases: HashMap::from([
                ("builtin".into(), "built".into()),
                ("external".into(), "ext".into()),
            ]),
            profile_index: 0,
            displays,
        };

        let routed = resolved.route_windows(&[window()]).unwrap();

        assert_eq!(routed.len(), 1);
        assert_eq!(routed[0].home.workspace_name, "W");
        assert_eq!(routed[0].home.display_uuid, "ext");
    }
}
