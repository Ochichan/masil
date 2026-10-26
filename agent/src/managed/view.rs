//! Saved filters and deterministic ordering for agent list presentation.

use super::operations::{Store, ViewChange, ViewPreference};
use super::{Agent, Manager};
use crate::providers;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::path::Path;

pub(super) const OPTION: &str = "@masil-agent-view";
const USAGE: &str = "view requires get, clear, or set [--provider ID] [--state STATE] [--workspace NAME] [--sort ORDER], and --client-view for one terminal client's view";
const VERSION: u32 = 1;
const MAX_WORKSPACES: usize = 64;
const STATES: [&str; 5] = ["idle", "working", "blocked", "unknown", "exited"];
/// Selects the per-client view in `view` and `list` (D1, RL-12).
pub(crate) const CLIENT_FLAG: &str = "--client-view";
/// The longest agent UI state a client row keeps.
const MAX_UI: usize = 4096;
const UNSTABLE: &str = "client_profile_unstable: this terminal gives masil no stable identity; set MASIL_CLIENT_KEY in it before attaching";

/// The client a `--client-view` command or the agent UI acts for, and what
/// the operation store keeps for it.
#[derive(Clone, Debug)]
pub(crate) struct ClientView {
    /// The client's terminal profile; None when its terminal gives no
    /// stable identity (`unstable`), and nothing is kept for it.
    pub(crate) profile: Option<String>,
    stored: Option<ViewPreference>,
}

impl ClientView {
    /// The client's own saved view as stored, when it has one.
    pub(crate) fn view_text(&self) -> Option<&str> {
        self.stored.as_ref()?.view.as_deref()
    }

    /// The agent UI state the client last left, when there is one.
    pub(crate) fn ui(&self) -> Option<&str> {
        self.stored.as_ref().map(|row| row.ui.as_str())
    }

    /// Fails for a damaged saved view; a client without one passes.
    pub(crate) fn check(&self) -> Result<(), String> {
        self.view_text()
            .map_or(Ok(()), |text| client_view_from_text(text).map(drop))
    }
}

/// `profile` when it is a stable `masil_client_profile`; None for
/// `unstable`.
fn stable_profile(profile: &str) -> Result<Option<String>, String> {
    if profile == "unstable" {
        return Ok(None);
    }
    if (16..=128).contains(&profile.len())
        && profile
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        Ok(Some(profile.to_owned()))
    } else {
        Err("invalid_target: the client's profile is malformed".into())
    }
}

/// The client view of a server, read without creating the store.
pub(super) async fn resolve_client(
    native: &crate::native_ui::Context,
) -> Result<ClientView, String> {
    let profile = stable_profile(&native.client_profile().await?)?;
    let stored = match &profile {
        Some(profile) => Store::view_preference_readonly(&native.socket, profile)?,
        None => None,
    };
    Ok(ClientView { profile, stored })
}

/// A client's saved view; a damaged one names its own way out.
fn client_view_from_text(encoded: &str) -> Result<SavedView, String> {
    view_from_text(encoded).map_err(|_| {
        "invalid client agent view; clear it with agent view clear --client-view".into()
    })
}

/// Keeps the agent UI state of a client, for the next UI it opens.
pub(crate) fn save_client_ui(socket: &Path, profile: &str, ui: &str) -> Result<(), String> {
    if ui.len() > MAX_UI {
        return Err(format!("agent UI state exceeds {MAX_UI} bytes"));
    }
    stable_profile(profile)?.ok_or(UNSTABLE)?;
    Store::open(socket)?.set_view_preference(
        profile,
        &ViewChange::Ui(ui.to_owned()),
        crate::observation::now_ms(),
    )
}

/// `args` without `CLIENT_FLAG`, and whether it was there once. Twice is a
/// usage error.
pub(super) fn take_client_flag(args: &[String]) -> Result<(bool, Vec<String>), String> {
    let rest: Vec<String> = args
        .iter()
        .filter(|arg| *arg != CLIENT_FLAG)
        .cloned()
        .collect();
    match args.len() - rest.len() {
        0 => Ok((false, rest)),
        1 => Ok((true, rest)),
        _ => Err(format!("usage: repeated {CLIENT_FLAG}")),
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SavedView {
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
        let (client, args) = take_client_flag(args)?;
        if client {
            return self.client_view_command(&args).await;
        }
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
                self.command(&["set-option", "-g", OPTION, &encoded])
                    .await?;
                Ok(json!({"stage":"view_saved", "view":view}))
            }
            _ => Err(USAGE.into()),
        }
    }

    /// `view get|clear|set --client-view`: the view of one terminal client.
    async fn client_view_command(&self, args: &[String]) -> Result<Value, String> {
        // A usage error is reported before any client is looked up.
        let set = match args.first().map(String::as_str) {
            Some("get" | "clear") if args.len() == 1 => None,
            Some("set") => Some(parse_set(&args[1..])?),
            _ => return Err(USAGE.into()),
        };
        let client = resolve_client(&self.native).await?;
        let stable = if client.profile.is_some() {
            "stable"
        } else {
            "unstable"
        };
        if let Some(view) = set {
            let profile = client.profile.as_deref().ok_or(UNSTABLE)?;
            let encoded = super::encode(&view)?;
            Store::open(&self.native.socket)?.set_view_preference(
                profile,
                &ViewChange::View(Some(encoded)),
                crate::observation::now_ms(),
            )?;
            return Ok(
                json!({"stage":"view_saved","view":view,"source":"client","profile":stable}),
            );
        }
        match args.first().map(String::as_str) {
            Some("get") => {
                let (view, source) = match client.view_text() {
                    Some(text) => (client_view_from_text(text)?, "client"),
                    None => self.server_view().await?,
                };
                Ok(json!({"stage":"view_loaded","view":view,"source":source,"profile":stable}))
            }
            _ => {
                let profile = client.profile.as_deref().ok_or(UNSTABLE)?;
                // A client without a view of its own has nothing to clear,
                // and no store is created for it.
                if client.view_text().is_some() {
                    Store::open(&self.native.socket)?.set_view_preference(
                        profile,
                        &ViewChange::View(None),
                        crate::observation::now_ms(),
                    )?;
                }
                let (view, source) = self.server_view().await?;
                Ok(json!({"stage":"view_cleared","view":view,"source":source,"profile":stable}))
            }
        }
    }

    /// The server's view and where it comes from (`server`, or `default`
    /// when none is saved).
    async fn server_view(&self) -> Result<(SavedView, &'static str), String> {
        let encoded = self.command(&["show-options", "-gqv", OPTION]).await?;
        let source = if encoded.trim().is_empty() {
            "default"
        } else {
            "server"
        };
        Ok((view_from_text(&encoded)?, source))
    }

    /// The client view of the `--client` client, or of the one attached
    /// client when no client is named.
    pub(crate) async fn client_view(&self) -> Result<ClientView, String> {
        resolve_client(&self.native).await
    }

    /// The agents under a client's saved view (as stored), or under the
    /// server's view when the client has none.
    pub(crate) async fn list_view_for(&self, view: Option<&str>) -> Result<Vec<Agent>, String> {
        match view {
            Some(text) => Ok(apply_view(
                &client_view_from_text(text)?,
                self.list().await?,
            )),
            None => self.list_view().await,
        }
    }

    pub async fn list_view(&self) -> Result<Vec<Agent>, String> {
        let view = self.load_view().await?;
        Ok(apply_view(&view, self.list().await?))
    }

    async fn load_view(&self) -> Result<SavedView, String> {
        let encoded = self.command(&["show-options", "-gqv", OPTION]).await?;
        view_from_text(&encoded)
    }
}

/// The saved view an option value holds; the default view when it is empty.
pub(super) fn view_from_text(encoded: &str) -> Result<SavedView, String> {
    if encoded.trim().is_empty() {
        return Ok(default_view());
    }
    let view = super::decode::<SavedView>(encoded.trim())
        .ok_or("invalid saved agent view; clear it before listing agents")?;
    validate_view(&view)?;
    Ok(view)
}

/// The agents a view shows, in its order.
pub(super) fn apply_view(view: &SavedView, agents: Vec<Agent>) -> Vec<Agent> {
    let mut agents: Vec<_> = agents
        .into_iter()
        .filter(|agent| matches_view(view, &agent.provider, &agent.state, &agent.workspace))
        .collect();
    agents.sort_by(|left, right| compare_agents(&view.sort, left, right));
    agents
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
    fn client_profiles_are_hex_or_unstable() {
        assert_eq!(stable_profile("unstable").unwrap(), None);
        assert_eq!(
            stable_profile("0123456789abcdef").unwrap().as_deref(),
            Some("0123456789abcdef")
        );
        assert!(stable_profile("0123456789abcde").is_err());
        assert!(stable_profile("0123456789ABCDEF").is_err());
        assert!(stable_profile(&"a".repeat(129)).is_err());
        assert!(stable_profile("").is_err());
    }

    #[test]
    fn the_client_flag_is_taken_once() {
        let args = |list: &[&str]| list.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
        assert_eq!(
            take_client_flag(&args(&["set", "--client-view", "--sort", "name"])).unwrap(),
            (true, args(&["set", "--sort", "name"]))
        );
        assert_eq!(
            take_client_flag(&args(&["get"])).unwrap(),
            (false, args(&["get"]))
        );
        assert!(take_client_flag(&args(&["get", "--client-view", "--client-view"])).is_err());
    }

    #[test]
    fn a_client_view_reads_its_own_view_or_reports_damage() {
        let own = ClientView {
            profile: Some("0123456789abcdef".into()),
            stored: Some(ViewPreference {
                view: Some(super::super::encode(&default_view()).unwrap()),
                ui: "{}".into(),
            }),
        };
        assert!(own.check().is_ok());
        assert_eq!(own.ui(), Some("{}"));
        let damaged = ClientView {
            profile: Some("0123456789abcdef".into()),
            stored: Some(ViewPreference {
                view: Some("zz".into()),
                ui: "{}".into(),
            }),
        };
        assert!(
            damaged
                .check()
                .unwrap_err()
                .contains("view clear --client-view")
        );
        let none = ClientView {
            profile: None,
            stored: None,
        };
        assert_eq!(none.view_text(), None);
        assert!(none.check().is_ok());
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
