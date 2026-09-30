use ratatui::layout::Rect;
use serde_json::Value;
use std::collections::BTreeMap;
use std::str::FromStr;
use std::time::Instant;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Language {
    #[default]
    English,
    Korean,
}

impl Language {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::English => "en",
            Self::Korean => "ko",
        }
    }

    pub const fn next(self) -> Self {
        match self {
            Self::English => Self::Korean,
            Self::Korean => Self::English,
        }
    }
}

impl FromStr for Language {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "en" | "english" => Ok(Self::English),
            "ko" | "kr" | "korean" | "한국어" => Ok(Self::Korean),
            _ => Err(format!("unsupported UI language: {value}")),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Theme {
    #[default]
    Dark,
    Light,
    Terminal,
}

impl Theme {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
            Self::Terminal => "terminal",
        }
    }

    pub const fn next(self) -> Self {
        match self {
            Self::Dark => Self::Light,
            Self::Light => Self::Terminal,
            Self::Terminal => Self::Dark,
        }
    }
}

impl FromStr for Theme {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "dark" => Ok(Self::Dark),
            "light" => Ok(Self::Light),
            "terminal" | "term" => Ok(Self::Terminal),
            _ => Err(format!("unsupported UI theme: {value}")),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Effect {
    Ack {
        id: String,
        epoch: String,
        revision: String,
    },
    Navigate {
        id: String,
        pane_id: String,
        pty_generation: String,
        epoch: String,
    },
    Expand,
    Inspect {
        id: String,
        epoch: String,
    },
    CopyId {
        value: String,
        epoch: String,
    },
    Retry,
    NewAgent,
    RenameAgent,
    ResumeAgent,
    PrepareDraft,
    SendPrompt,
    InterruptAgent,
    ReadScreen,
    CloseAgent,
    Quit,
    Preferences {
        language: Language,
        theme: Theme,
    },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum Filter {
    #[default]
    All,
    Attention,
    Working,
    Unavailable,
}

impl Filter {
    pub(crate) const ALL: [Self; 4] =
        [Self::All, Self::Attention, Self::Working, Self::Unavailable];
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum Focus {
    Header,
    Filters,
    Search,
    #[default]
    List,
    Inspector,
    Actions,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Overlay {
    Help,
    Context,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Action {
    MarkSeen,
    GoToPane,
    Details,
    CopyId,
    Expand,
    Retry,
    NewAgent,
    RenameAgent,
    ResumeAgent,
    PrepareDraft,
    SendPrompt,
    InterruptAgent,
    ReadScreen,
    CloseAgent,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum HitTarget {
    Language,
    Theme,
    Help,
    Close,
    Filter(Filter),
    Search,
    ClearSearch,
    Row { id: String, revision: String },
    Action(Action),
    Divider,
    ListTrack,
    ListThumb,
    InspectorBody,
    InspectorTrack,
    InspectorThumb,
    HelpTrack,
    HelpThumb,
    HelpBody,
    ContextItem(Action),
    DismissOverlay,
    OverlaySurface,
}

#[derive(Clone, Debug)]
pub(crate) struct HitRegion {
    pub rect: Rect,
    pub target: HitTarget,
    pub z: u8,
}

#[derive(Clone, Debug)]
pub(crate) struct Observation {
    pub id: String,
    pub source_id: String,
    pub session_id: String,
    pub pane_id: Option<String>,
    pub activity: String,
    pub last_activity: Option<String>,
    pub native_exists: Option<bool>,
    pub native_freshness: String,
    pub observed_at_ms: u64,
    pub attention_kind: String,
    pub permission_count: u64,
    pub question_count: u64,
    pub pending_count: u64,
    pub attention_revision: String,
    pub acknowledged: bool,
    pub pending: bool,
    pub attention_available: bool,
    pub core_process: String,
    pub pty_generation: Option<String>,
    pub core_freshness: String,
    pub binding: String,
    pub frontend_verified: bool,
}

impl Observation {
    fn parse(value: &Value) -> Option<Self> {
        let id = value.get("id")?.as_str()?.to_owned();
        let native = value.get("native")?;
        let core = value.get("core")?;
        let attention = value.get("attention")?;
        Some(Self {
            id,
            source_id: string(value, "source_id"),
            session_id: string(value, "session_id"),
            pane_id: value
                .get("pane_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            activity: string(native, "activity"),
            last_activity: native
                .get("last_activity")
                .and_then(Value::as_str)
                .map(str::to_owned),
            native_exists: native.get("exists").and_then(Value::as_bool),
            native_freshness: string(native, "freshness"),
            observed_at_ms: native
                .get("observed_at_ms")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            attention_kind: string(native, "attention"),
            permission_count: native
                .get("permission_count")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            question_count: native
                .get("question_count")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            pending_count: native
                .get("pending_count")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            attention_revision: attention
                .get("revision")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            acknowledged: attention
                .get("acknowledged")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            pending: attention
                .get("pending")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            attention_available: attention
                .get("available")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            core_process: string(core, "process"),
            pty_generation: core
                .get("pty_generation")
                .and_then(Value::as_str)
                .map(str::to_owned),
            core_freshness: string(core, "freshness"),
            binding: string(value, "binding"),
            frontend_verified: value
                .get("frontend_verified")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }

    pub(crate) fn is_unavailable(&self) -> bool {
        self.native_exists != Some(true)
            || self.native_freshness == "stale"
            || self.binding == "invalidated"
    }

    pub(crate) fn attention_total(&self) -> u64 {
        self.permission_count
            .saturating_add(self.question_count)
            .saturating_add(self.pending_count)
    }
}

fn string(value: &Value, field: &str) -> String {
    value
        .get(field)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

#[derive(Clone, Debug)]
pub(crate) struct Pressed {
    pub target: HitTarget,
    pub at: Instant,
    pub identity: Option<(String, String)>,
}

#[derive(Clone, Debug)]
pub(crate) enum Drag {
    Divider { start_x: u16, start_width: u16 },
    ListThumb { grab: u16 },
    InspectorThumb { grab: u16 },
    HelpThumb { grab: u16 },
}

#[derive(Clone, Debug)]
pub(crate) struct ClickRecord {
    pub id: String,
    pub column: u16,
    pub row: u16,
    pub at: Instant,
}

pub struct App {
    pub dirty: bool,
    pub language: Language,
    pub theme: Theme,
    pub(crate) compact: bool,
    pub(crate) managed: bool,
    pub(crate) connected: bool,
    pub(crate) connection_message: Option<String>,
    pub(crate) native_available: bool,
    pub(crate) epoch: String,
    pub(crate) revision: String,
    pub(crate) rows: Vec<Observation>,
    pub(crate) details: BTreeMap<String, Value>,
    pub(crate) selected_id: Option<String>,
    pub(crate) filter: Filter,
    pub(crate) search: String,
    pub(crate) search_cursor: usize,
    pub(crate) search_view_start: usize,
    pub(crate) focus: Focus,
    pub(crate) header_focus: usize,
    pub(crate) header_controls: usize,
    pub(crate) filter_focus: usize,
    pub(crate) action_focus: usize,
    pub(crate) action_controls: usize,
    pub(crate) details_open: bool,
    pub(crate) list_scroll: usize,
    pub(crate) inspector_scroll: usize,
    pub(crate) help_scroll: usize,
    pub(crate) list_page: usize,
    pub(crate) inspector_page: usize,
    pub(crate) help_page: usize,
    pub(crate) inspector_lines: usize,
    pub(crate) help_lines: usize,
    pub(crate) divider_width: u16,
    pub(crate) overlay: Option<Overlay>,
    pub(crate) context_selection: usize,
    pub(crate) context_anchor: (u16, u16),
    pub(crate) hits: Vec<HitRegion>,
    pub(crate) hover: Option<HitTarget>,
    pub(crate) pressed: Option<Pressed>,
    pub(crate) drag: Option<Drag>,
    pub(crate) last_click: Option<ClickRecord>,
    pub(crate) ack_in_flight: Option<(String, String)>,
    pub(crate) prompt_in_flight: bool,
    pub(crate) toast: Option<String>,
    /// Server status line on the last footer row, and whether one is offline.
    pub(crate) endpoint_line: Option<(String, bool)>,
    pub(crate) last_area: Rect,
}

impl App {
    pub fn new(compact: bool, language: Language, theme: Theme) -> Self {
        Self {
            dirty: true,
            language,
            theme,
            compact,
            managed: false,
            connected: false,
            connection_message: None,
            native_available: false,
            epoch: String::new(),
            revision: String::new(),
            rows: Vec::new(),
            details: BTreeMap::new(),
            selected_id: None,
            filter: Filter::All,
            search: String::new(),
            search_cursor: 0,
            search_view_start: 0,
            focus: Focus::List,
            header_focus: 0,
            header_controls: 0,
            filter_focus: 0,
            action_focus: 0,
            action_controls: 0,
            details_open: false,
            list_scroll: 0,
            inspector_scroll: 0,
            help_scroll: 0,
            list_page: 1,
            inspector_page: 1,
            help_page: 1,
            inspector_lines: 0,
            help_lines: 0,
            divider_width: 48,
            overlay: None,
            context_selection: 0,
            context_anchor: (0, 0),
            hits: Vec::new(),
            hover: None,
            pressed: None,
            drag: None,
            last_click: None,
            ack_in_flight: None,
            prompt_in_flight: false,
            toast: None,
            endpoint_line: None,
            last_area: Rect::default(),
        }
    }

    pub(crate) fn set_managed(&mut self) {
        self.managed = true;
    }

    pub fn apply_snapshot(&mut self, snapshot: Value) {
        let next_epoch = snapshot
            .get("epoch")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let epoch_changed = !self.epoch.is_empty() && self.epoch != next_epoch;
        let previous_ids = self.filtered_ids();
        let previous_position = self
            .selected_id
            .as_ref()
            .and_then(|id| previous_ids.iter().position(|candidate| candidate == id))
            .unwrap_or(0);
        let mut rows = snapshot
            .get("observations")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Observation::parse)
            .collect::<Vec<_>>();
        if !self.managed {
            rows.sort_by(|left, right| {
                right
                    .pending
                    .cmp(&left.pending)
                    .then_with(|| left.id.cmp(&right.id))
            });
        }
        self.epoch = next_epoch;
        self.revision = snapshot
            .get("revision")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        self.rows = rows;
        // Inspect responses are supplemental to one complete projection. A new
        // projection invalidates them even when the observation ID survives.
        self.details.clear();

        let current_ids = self.filtered_ids();
        if current_ids != previous_ids {
            self.drag = None;
        }
        if !self
            .selected_id
            .as_ref()
            .is_some_and(|id| current_ids.contains(id))
        {
            self.selected_id = current_ids
                .get(previous_position.min(current_ids.len().saturating_sub(1)))
                .cloned();
        }
        if epoch_changed {
            self.cancel_interactions();
            self.overlay = None;
            self.ack_in_flight = None;
            self.prompt_in_flight = false;
            self.details.clear();
            self.toast = None;
        } else if let Some(pressed) = &self.pressed
            && let Some((id, revision)) = &pressed.identity
            && !self
                .rows
                .iter()
                .any(|row| row.id == *id && row.attention_revision == *revision)
        {
            self.pressed = None;
            self.last_click = None;
        }
        self.reveal_selection();
        self.dirty = true;
    }

    pub fn set_connection(&mut self, connected: bool, message: Option<String>) {
        self.connected = connected;
        self.connection_message = message;
        if !connected {
            self.cancel_interactions();
            self.overlay = None;
        }
        self.dirty = true;
    }

    pub fn set_native_available(&mut self, available: bool) {
        self.native_available = available;
        self.dirty = true;
    }

    pub fn set_details(&mut self, id: &str, full_observation: Value) {
        if self.rows.iter().any(|row| row.id == id) {
            self.details.insert(id.to_owned(), full_observation);
            self.details_open = true;
            self.inspector_scroll = 0;
            self.focus = Focus::Inspector;
            self.dirty = true;
        }
    }

    /// Select an agent by identity, clearing a filter or search that hides it.
    pub(crate) fn select_id(&mut self, id: &str) -> bool {
        if !self.rows.iter().any(|row| row.id == id) {
            return false;
        }
        if !self.filtered_ids().iter().any(|candidate| candidate == id) {
            self.filter = Filter::All;
            self.search.clear();
            self.search_cursor = 0;
            self.search_view_start = 0;
        }
        self.selected_id = Some(id.to_owned());
        self.reveal_selection();
        self.dirty = true;
        true
    }

    pub fn notify(&mut self, message: String) {
        self.toast = Some(message);
        self.dirty = true;
    }

    pub fn finish_action(&mut self) {
        self.ack_in_flight = None;
        self.prompt_in_flight = false;
        self.dirty = true;
    }

    pub(crate) fn start_prompt(&mut self) {
        self.prompt_in_flight = true;
        self.dirty = true;
    }

    pub(crate) fn selected(&self) -> Option<&Observation> {
        let id = self.selected_id.as_ref()?;
        self.rows.iter().find(|row| &row.id == id)
    }

    pub(crate) fn filtered_ids(&self) -> Vec<String> {
        self.rows
            .iter()
            .filter(|row| self.matches(row))
            .map(|row| row.id.clone())
            .collect()
    }

    pub(crate) fn filtered_rows(&self) -> Vec<&Observation> {
        self.rows.iter().filter(|row| self.matches(row)).collect()
    }

    fn matches(&self, row: &Observation) -> bool {
        let filter_matches = match self.filter {
            Filter::All => true,
            Filter::Attention => row.pending && row.attention_available,
            Filter::Working => matches!(row.activity.as_str(), "working" | "retrying"),
            Filter::Unavailable => !self.connected || row.is_unavailable(),
        };
        if !filter_matches {
            return false;
        }
        let query = self.search.to_lowercase();
        query.is_empty()
            || row.id.to_lowercase().contains(&query)
            || row.source_id.to_lowercase().contains(&query)
            || row.session_id.to_lowercase().contains(&query)
            || row
                .pane_id
                .as_deref()
                .unwrap_or_default()
                .to_lowercase()
                .contains(&query)
    }

    pub(crate) fn action_enabled(&self, action: Action) -> bool {
        match action {
            Action::MarkSeen => self.selected().is_some_and(|row| {
                self.connected
                    && row.pending
                    && row.attention_available
                    && (!self.managed || row.core_freshness == "fresh")
                    && !row.attention_revision.is_empty()
                    && self.ack_in_flight.is_none()
                    && !self.epoch.is_empty()
            }),
            Action::GoToPane => self.selected().is_some_and(|row| {
                self.connected
                    && self.native_available
                    && row.binding != "invalidated"
                    && row.core_process == "running"
                    && row.core_freshness == "fresh"
                    && row.pane_id.is_some()
                    && row.pty_generation.is_some()
                    && !self.epoch.is_empty()
            }),
            Action::Details => self.selected_id.is_some() && !self.epoch.is_empty(),
            Action::CopyId => {
                self.selected_id.is_some()
                    && self.connected
                    && self.native_available
                    && !self.epoch.is_empty()
            }
            Action::Expand => self.compact && self.native_available,
            Action::Retry => !self.connected,
            Action::NewAgent => self.managed && self.connected,
            Action::RenameAgent | Action::ReadScreen | Action::CloseAgent => {
                self.managed
                    && self.connected
                    && self
                        .selected()
                        .is_some_and(|row| row.core_freshness == "fresh")
            }
            Action::ResumeAgent => {
                self.managed
                    && self.connected
                    && self.selected().is_some_and(|row| {
                        row.core_freshness == "fresh"
                            && !row.session_id.is_empty()
                            && row.session_id != "unverified"
                            && row.binding != "managed_conflict"
                    })
            }
            Action::PrepareDraft | Action::InterruptAgent => {
                self.managed
                    && self.connected
                    && self.selected().is_some_and(|row| {
                        row.core_process == "running" && row.core_freshness == "fresh"
                    })
            }
            Action::SendPrompt => {
                self.managed
                    && self.connected
                    && !self.prompt_in_flight
                    && self.selected().is_some_and(|row| {
                        row.core_process == "running"
                            && row.core_freshness == "fresh"
                            && row.activity == "idle"
                    })
            }
        }
    }

    pub(crate) fn effect_for(&mut self, action: Action) -> Option<Effect> {
        if !self.action_enabled(action) {
            return None;
        }
        let effect = match action {
            Action::MarkSeen => {
                let row = self.selected()?.clone();
                self.ack_in_flight = Some((row.id.clone(), row.attention_revision.clone()));
                Effect::Ack {
                    id: row.id,
                    epoch: self.epoch.clone(),
                    revision: row.attention_revision,
                }
            }
            Action::GoToPane => {
                let row = self.selected()?;
                Effect::Navigate {
                    id: row.id.clone(),
                    pane_id: row.pane_id.clone()?,
                    pty_generation: row.pty_generation.clone()?,
                    epoch: self.epoch.clone(),
                }
            }
            Action::Details => {
                self.details_open = true;
                self.focus = Focus::Inspector;
                Effect::Inspect {
                    id: self.selected_id.clone()?,
                    epoch: self.epoch.clone(),
                }
            }
            Action::CopyId => Effect::CopyId {
                value: self.selected_id.clone()?,
                epoch: self.epoch.clone(),
            },
            Action::Expand => Effect::Expand,
            Action::Retry => Effect::Retry,
            Action::NewAgent => Effect::NewAgent,
            Action::RenameAgent => Effect::RenameAgent,
            Action::ResumeAgent => Effect::ResumeAgent,
            Action::PrepareDraft => Effect::PrepareDraft,
            Action::SendPrompt => Effect::SendPrompt,
            Action::InterruptAgent => Effect::InterruptAgent,
            Action::ReadScreen => Effect::ReadScreen,
            Action::CloseAgent => Effect::CloseAgent,
        };
        self.toast = None;
        self.dirty = true;
        Some(effect)
    }

    pub(crate) fn cancel_interactions(&mut self) {
        self.pressed = None;
        self.drag = None;
        self.last_click = None;
        self.hover = None;
    }

    pub(crate) fn clamp_selection_and_scroll(&mut self) {
        let ids = self.filtered_ids();
        if !self.selected_id.as_ref().is_some_and(|id| ids.contains(id)) {
            self.selected_id = ids.first().cloned();
        }
        let max = ids.len().saturating_sub(self.list_page.max(1));
        self.list_scroll = self.list_scroll.min(max);
        self.inspector_scroll = self.inspector_scroll.min(
            self.inspector_lines
                .saturating_sub(self.inspector_page.max(1)),
        );
        self.help_scroll = self
            .help_scroll
            .min(self.help_lines.saturating_sub(self.help_page.max(1)));
    }

    pub(crate) fn reveal_selection(&mut self) {
        self.clamp_selection_and_scroll();
        let ids = self.filtered_ids();
        let selected = self
            .selected_id
            .as_ref()
            .and_then(|id| ids.iter().position(|candidate| candidate == id))
            .unwrap_or(0);
        if selected < self.list_scroll {
            self.list_scroll = selected;
        } else if selected >= self.list_scroll + self.list_page.max(1) {
            self.list_scroll = selected + 1 - self.list_page.max(1);
        }
    }
}

#[cfg(test)]
mod managed_tests {
    use super::*;
    use serde_json::json;

    fn row(id: &str, pending: bool) -> Value {
        json!({
            "id":id,"source_id":"codex","session_id":"unverified","pane_id":"%1",
            "native":{"exists":true,"activity":"idle","attention":if pending {"needs_input"} else {"none"},
                "permission_count":0,"question_count":0,"pending_count":u8::from(pending),"freshness":"fresh","observed_at_ms":0},
            "core":{"process":"running","pty_generation":"1","freshness":"fresh"},
            "binding":"explicit_unverified","frontend_verified":false,
            "attention":{"revision":id,"acknowledged":false,"pending":pending,"available":pending}
        })
    }

    #[test]
    fn managed_snapshot_preserves_saved_view_order() {
        let mut app = App::new(false, Language::English, Theme::Dark);
        app.set_managed();
        app.apply_snapshot(json!({
            "epoch":"boot","revision":"1",
            "observations":[row("alpha-idle", false), row("zulu-blocked", true)]
        }));
        assert_eq!(
            app.rows
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            ["alpha-idle", "zulu-blocked"]
        );
    }
}
