//! Saved filters and deterministic ordering for agent list presentation.

use super::{Agent, Manager};
use crate::providers;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::cmp::Ordering;
use std::collections::BTreeSet;

const OPTION: &str = "@masil-agent-view";
const VERSION: u32 = 1;
const MAX_WORKSPACES: usize = 64;
const STATES: [&str; 5] = ["idle", "working", "blocked", "unknown", "exited"];

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedView {
    version: u32,
    providers: Vec<String>,
    states: Vec<String>,
    workspaces: Vec<String>,
    sort: SortOrder,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SortOrder {
    #[default]
    Priority,
    Name,
    Provider,
    Workspace,
}

impl Manager {
    pub async fn view(&self, args: &[String]) -> Result<Value, String> {
        match args.first().map(String::as_str) {
            Some("get") if args.len() == 1 => {
                let view = self.load_view().await?;
                Ok(json!({"stage":"view_loaded", "view":view}))
            }
            Some("clear") if args.len() == 1 => {
                self.command(&["set-option", "-gu", OPTION]).await?;
                Ok(json!({"stage":"view_cleared", "view":default_view()}))
            }
            Some("set") => {
                let view = parse_set(&args[1..])?;
                let encoded = super::encode(&view)?;
                self.command(&["set-option", "-g", OPTION, &encoded]).await?;
                Ok(json!({"stage":"view_saved", "view":view}))
            }
            _ => Err("view requires get, clear, or set [--provider ID] [--state STATE] [--workspace NAME] [--sort ORDER]".into()),
        }
    }

    pub async fn list_view(&self) -> Result<Vec<Agent>, String> {
        let view = self.load_view().await?;
        let mut agents: Vec<_> = self
            .list()
            .await?
            .into_iter()
            .filter(|agent| matches_view(&view, &agent.provider, &agent.state, &agent.workspace))
            .collect();
        agents.sort_by(|left, right| compare_agents(&view.sort, left, right));
        Ok(agents)
    }

    async fn load_view(&self) -> Result<SavedView, String> {
        let encoded = self.command(&["show-options", "-gqv", OPTION]).await?;
        if encoded.trim().is_empty() {
            return Ok(default_view());
        }
        let view = super::decode::<SavedView>(encoded.trim())
            .ok_or("invalid saved agent view; clear it before listing agents")?;
        validate_view(&view)?;
        Ok(view)
    }
}

fn default_view() -> SavedView {
    SavedView {
        version: VERSION,
        ..SavedView::default()
    }
}

fn parse_set(args: &[String]) -> Result<SavedView, String> {
    let mut providers = BTreeSet::new();
    let mut states = BTreeSet::new();
    let mut workspaces = BTreeSet::new();
    let mut sort = None;
    let mut index = 0;
    while index < args.len() {
        let value = args
            .get(index + 1)
            .ok_or_else(|| format!("missing value for {}", args[index]))?;
        match args[index].as_str() {
            "--provider" => {
                let provider = providers::find(value)
                    .ok_or_else(|| format!("unknown_provider: unknown agent provider: {value}"))?;
                providers.insert(provider.id.to_owned());
            }
            "--state" => {
                if !STATES.contains(&value.as_str()) {
                    return Err(format!("unsupported agent state: {value}"));
                }
                states.insert(value.clone());
            }
            "--workspace" => {
                validate_workspace(value)?;
                if workspaces.len() >= MAX_WORKSPACES && !workspaces.contains(value) {
                    return Err(format!("view supports at most {MAX_WORKSPACES} workspaces"));
                }
                workspaces.insert(value.clone());
            }
            "--sort" if sort.is_none() => {
                sort = Some(match value.as_str() {
                    "priority" => SortOrder::Priority,
                    "name" => SortOrder::Name,
                    "provider" => SortOrder::Provider,
                    "workspace" => SortOrder::Workspace,
                    _ => return Err(format!("unsupported agent view sort: {value}")),
                });
            }
            option => return Err(format!("unknown or repeated view option: {option}")),
        }
        index += 2;
    }
    let view = SavedView {
        version: VERSION,
        providers: providers.into_iter().collect(),
        states: states.into_iter().collect(),
        workspaces: workspaces.into_iter().collect(),
        sort: sort.unwrap_or_default(),
    };
    validate_view(&view)?;
    Ok(view)
}

fn validate_view(view: &SavedView) -> Result<(), String> {
    if view.version != VERSION {
        return Err(format!(
            "unsupported saved agent view version: {}",
            view.version
        ));
    }
    if view.providers.len() > providers::all().len()
        || view.states.len() > STATES.len()
        || view.workspaces.len() > MAX_WORKSPACES
    {
        return Err("saved agent view exceeds its filter bounds".into());
    }
    validate_unique(&view.providers, "providers")?;
    validate_unique(&view.states, "states")?;
    validate_unique(&view.workspaces, "workspaces")?;
    for provider in &view.providers {
        if providers::find(provider).is_none_or(|found| found.id != provider) {
            return Err("unknown_provider: saved agent view contains an unknown provider".into());
        }
    }
    if view
        .states
        .iter()
        .any(|state| !STATES.contains(&state.as_str()))
    {
        return Err("saved agent view contains an unsupported state".into());
    }
    for workspace in &view.workspaces {
        validate_workspace(workspace)?;
    }
    Ok(())
}

fn validate_unique(values: &[String], label: &str) -> Result<(), String> {
    if values.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(format!(
            "saved agent view {label} must be sorted and unique"
        ));
    }
    Ok(())
}

fn validate_workspace(workspace: &str) -> Result<(), String> {
    if workspace.is_empty() || workspace.len() > 128 || workspace.chars().any(char::is_control) {
        return Err("workspace must contain 1–128 bytes without control characters".into());
    }
    Ok(())
}

fn matches_view(view: &SavedView, provider: &str, state: &str, workspace: &str) -> bool {
    (view.providers.is_empty() || view.providers.iter().any(|value| value == provider))
        && (view.states.is_empty() || view.states.iter().any(|value| value == state))
        && (view.workspaces.is_empty() || view.workspaces.iter().any(|value| value == workspace))
}

fn compare_agents(sort: &SortOrder, left: &Agent, right: &Agent) -> Ordering {
    match sort {
        SortOrder::Priority => priority(&left.state)
            .cmp(&priority(&right.state))
            .then_with(|| left.workspace.cmp(&right.workspace))
            .then_with(|| left.name.cmp(&right.name)),
        SortOrder::Name => left
            .name
            .cmp(&right.name)
            .then_with(|| left.workspace.cmp(&right.workspace)),
        SortOrder::Provider => left
            .provider
            .cmp(&right.provider)
            .then_with(|| left.workspace.cmp(&right.workspace))
            .then_with(|| left.name.cmp(&right.name)),
        SortOrder::Workspace => left
            .workspace
            .cmp(&right.workspace)
            .then_with(|| left.name.cmp(&right.name)),
    }
    .then_with(|| pane_number(&left.pane_id).cmp(&pane_number(&right.pane_id)))
    .then_with(|| left.pane_id.cmp(&right.pane_id))
}

fn priority(state: &str) -> u8 {
    match state {
        "blocked" => 0,
        "working" => 1,
        "idle" => 2,
        "unknown" => 3,
        "exited" => 4,
        _ => 3,
    }
}

fn pane_number(pane: &str) -> u64 {
    pane.strip_prefix('%')
        .and_then(|value| value.parse().ok())
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_filters_match_all_and_exact_filters_intersect() {
        let mut view = default_view();
        assert!(matches_view(&view, "codex", "idle", "work"));
        view.providers.push("codex".into());
        view.states.push("blocked".into());
        view.workspaces.push("work".into());
        assert!(matches_view(&view, "codex", "blocked", "work"));
        assert!(!matches_view(&view, "codex", "idle", "work"));
        assert!(!matches_view(&view, "claude", "blocked", "work"));
        assert!(!matches_view(&view, "codex", "blocked", "other"));
    }

    #[test]
    fn set_deduplicates_filters_and_rejects_unsafe_values() {
        let args: Vec<String> = [
            "--provider",
            "codex",
            "--provider",
            "codex",
            "--state",
            "idle",
            "--state",
            "idle",
            "--workspace",
            "work",
            "--workspace",
            "work",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let view = parse_set(&args).unwrap();
        assert_eq!(view.providers, ["codex"]);
        assert_eq!(view.states, ["idle"]);
        assert_eq!(view.workspaces, ["work"]);
        assert!(parse_set(&["--state".into(), "running".into()]).is_err());
        assert!(parse_set(&["--workspace".into(), "bad\nvalue".into()]).is_err());
        assert!(
            parse_set(&[
                "--sort".into(),
                "name".into(),
                "--sort".into(),
                "name".into()
            ])
            .is_err()
        );
    }

    #[test]
    fn priority_and_pane_order_are_stable() {
        assert!(priority("blocked") < priority("working"));
        assert!(priority("working") < priority("idle"));
        assert!(priority("idle") < priority("unknown"));
        assert!(priority("unknown") < priority("exited"));
        assert!(pane_number("%9") < pane_number("%10"));
        assert_eq!(pane_number("bad"), u64::MAX);
    }
}
