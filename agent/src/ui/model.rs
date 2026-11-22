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
    /// Open the request dialog for this agent's pending requests.
    Answer {
        id: String,
    },
    /// Open this agent's prompt queue.
    Queue {
        id: String,
    },
    /// Open this agent's working tree changes.
    Changes {
        id: String,
    },
    /// Start or stop a dictation for this agent.
    Dictate {
        id: String,
    },
    Quit,
    Preferences {
        language: Language,
        theme: Theme,
    },
    /// Focus the pane of an inbox event's run, without reading it.
    InboxFocus {
        pane: String,
        run: String,
    },
    /// Mark one inbox event read.
    InboxRead {
        id: i64,
    },
    /// Mark every event through this sequence read.
    InboxReadAll {
        through: i64,
    },
    /// Open the request dialog for an inbox event's run.
    InboxAnswer {
        pane: String,
        run: String,
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
    /// Answer the agent's pending requests through masil.
    Answer,
    /// The agent's prompt queue.
    Queue,
    /// The agent's working tree changes and checkpoints.
    Changes,
    /// Start or stop dictating into the agent's queue (`agent dictate`).
    Dictate,
    /// Open or close the inbox view.
    Inbox,
    /// Focus the oldest unseen inbox event's pane.
    NextUnseen,
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
    InboxRow(i64),
    InboxTrack,
    InboxThumb,
}

#[derive(Clone, Debug)]
pub(crate) struct HitRegion {
    pub rect: Rect,
    pub target: HitTarget,
    pub z: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
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
    /// The agent's pane is in a column window (`layout columns`).
    pub columns: bool,
}

impl Observation {
    pub(crate) fn parse(value: &Value) -> Option<Self> {
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
            columns: value
                .get("columns")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }

    pub(crate) fn from_agent(agent: &crate::managed::Agent) -> Self {
        let blocked = agent.state == "blocked";
        let (permissions, questions) = if blocked {
            (agent.permission_count, agent.question_count)
        } else {
            (0, 0)
        };
        let attention_kind = match (blocked, permissions, questions) {
            (true, 1.., _) => "approval",
            (true, 0, 1..) => "question",
            (true, ..) => "needs_input",
            (false, ..) if agent.returned_idle => "returned_idle",
            _ => "none",
        };
        let pending_count = u64::from(
            (blocked && permissions == 0 && questions == 0) || (!blocked && agent.returned_idle),
        );
        let source_id = if agent.endpoint_label.is_empty() {
            agent.provider.clone()
        } else {
            format!("{} / {}", agent.endpoint_label, agent.provider)
        };
        let activity = if matches!(agent.state.as_str(), "idle" | "working" | "blocked") {
            agent.state.clone()
        } else {
            "unknown".to_owned()
        };
        let pending = blocked || agent.returned_idle;
        let freshness = if agent.stale { "stale" } else { "fresh" };
        let core_process = if agent.stale {
            "unknown".to_owned()
        } else {
            agent.process.clone()
        };
        Self {
            id: agent.id.clone(),
            source_id,
            session_id: agent
                .session_id
                .clone()
                .unwrap_or_else(|| "unverified".to_owned()),
            pane_id: Some(agent.pane_id.clone()),
            activity,
            last_activity: None,
            native_exists: Some(agent.process == "running"),
            native_freshness: freshness.to_owned(),
            observed_at_ms: 0,
            attention_kind: attention_kind.to_owned(),
            permission_count: u64::from(permissions),
            question_count: u64::from(questions),
            pending_count,
            attention_revision: agent.revision.clone(),
            acknowledged: agent.seen,
            pending,
            attention_available: pending,
            core_process,
            pty_generation: Some(agent.generation.clone()),
            core_freshness: freshness.to_owned(),
            binding: agent.binding_label().to_owned(),
            frontend_verified: false,
            columns: agent.columns,
        }
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
    InboxThumb { grab: u16 },
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
    /// A window of the managed desk is drawn above this view and owns the
    /// terminal cursor.
    pub(crate) modal_open: bool,
    pub(crate) context_selection: usize,
    pub(crate) context_anchor: (u16, u16),
    pub(crate) hits: Vec<HitRegion>,
    pub(crate) hover: Option<HitTarget>,
    pub(crate) pressed: Option<Pressed>,
    pub(crate) drag: Option<Drag>,
    pub(crate) last_click: Option<ClickRecord>,
    pub(crate) ack_in_flight: Option<(String, String)>,
    pub(crate) prompt_in_flight: bool,
    /// Counts started prompts: a late result ends only the prompt it sent.
    pub(crate) prompt_serial: u64,
    /// The latest `y` press: only its request list may open a dialog.
    pub(crate) answer_request: u64,
    /// The latest `e` press: only its list may open the queue window.
    pub(crate) queue_request: u64,
    /// Counts `f` presses: only the latest opens the changes window.
    pub(crate) changes_request: u64,
    pub(crate) toast: Option<String>,
    /// Server status line on the last footer row, and whether one is offline.
    pub(crate) endpoint_line: Option<(String, bool)>,
    pub(crate) last_area: Rect,
    /// The inbox view of the management window.
    pub(crate) inbox: super::inbox::InboxState,
    /// Monotonic counters so view caches invalidate without deep-comparing state.
    pub(crate) rows_version: u64,
    pub(crate) details_version: u64,
    pub(crate) inspector_cache: Option<super::view::InspectorCache>,
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
            modal_open: false,
            context_selection: 0,
            context_anchor: (0, 0),
            hits: Vec::new(),
            hover: None,
            pressed: None,
            drag: None,
            last_click: None,
            ack_in_flight: None,
            prompt_in_flight: false,
            prompt_serial: 0,
            answer_request: 0,
            queue_request: 0,
            changes_request: 0,
            toast: None,
            endpoint_line: None,
            last_area: Rect::default(),
            inbox: Default::default(),
            rows_version: 0,
            details_version: 0,
            inspector_cache: None,
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
        let revision = snapshot
            .get("revision")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let rows = snapshot
            .get("observations")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Observation::parse)
            .collect::<Vec<_>>();
        self.apply_observations(next_epoch, revision, rows);
    }

    pub(crate) fn apply_observations(
        &mut self,
        next_epoch: String,
        revision: String,
        mut rows: Vec<Observation>,
    ) {
        let epoch_changed = !self.epoch.is_empty() && self.epoch != next_epoch;
        let previous_ids = self.filtered_ids();
        let previous_position = self
            .selected_id
            .as_ref()
            .and_then(|id| previous_ids.iter().position(|candidate| candidate == id))
            .unwrap_or(0);
        if !self.managed {
            rows.sort_by(|left, right| {
                right
                    .pending
                    .cmp(&left.pending)
                    .then_with(|| left.id.cmp(&right.id))
            });
        }
        self.epoch = next_epoch;
        self.revision = revision;
        self.rows = rows;
        self.rows_version = self.rows_version.wrapping_add(1);
        // Inspect responses are supplemental to one complete projection. A new
        // projection invalidates them even when the observation ID survives.
        if !self.details.is_empty() {
            self.details.clear();
            self.details_version = self.details_version.wrapping_add(1);
        }

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
            // The menu was opened for the row that left; its items must not
            // act on the row that took its place.
            if self.overlay == Some(Overlay::Context) {
                self.overlay = None;
            }
        }
        if epoch_changed {
            self.cancel_interactions();
            self.overlay = None;
            self.ack_in_flight = None;
            self.prompt_in_flight = false;
            if !self.details.is_empty() {
                self.details.clear();
                self.details_version = self.details_version.wrapping_add(1);
            }
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
            self.details_version = self.details_version.wrapping_add(1);
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
        if !self.is_filtered_id(id) {
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
        self.prompt_serial = self.prompt_serial.wrapping_add(1);
        self.dirty = true;
    }

    pub(crate) fn selected(&self) -> Option<&Observation> {
        let id = self.selected_id.as_ref()?;
        self.rows.iter().find(|row| &row.id == id)
    }

    pub(crate) fn filtered_ids(&self) -> Vec<String> {
        let query = SearchQuery::new(&self.search);
        self.rows
            .iter()
            .filter(|row| self.matches_with(row, &query))
            .map(|row| row.id.clone())
            .collect()
    }

    pub(crate) fn filtered_indices(&self) -> Vec<usize> {
        let query = SearchQuery::new(&self.search);
        self.rows
            .iter()
            .enumerate()
            .filter_map(|(index, row)| self.matches_with(row, &query).then_some(index))
            .collect()
    }

    pub(crate) fn filtered_count(&self) -> usize {
        let query = SearchQuery::new(&self.search);
        self.rows
            .iter()
            .filter(|row| self.matches_with(row, &query))
            .count()
    }

    fn is_filtered_id(&self, id: &str) -> bool {
        let query = SearchQuery::new(&self.search);
        self.rows
            .iter()
            .any(|row| row.id == id && self.matches_with(row, &query))
    }

    fn matches_with(&self, row: &Observation, query: &SearchQuery<'_>) -> bool {
        let filter_matches = match self.filter {
            Filter::All => true,
            Filter::Attention => row.pending && row.attention_available,
            Filter::Working => matches!(row.activity.as_str(), "working" | "retrying"),
            Filter::Unavailable => !self.connected || row.is_unavailable(),
        };
        if !filter_matches {
            return false;
        }
        query.matches(&row.id)
            || query.matches(&row.source_id)
            || query.matches(&row.session_id)
            || query.matches(row.pane_id.as_deref().unwrap_or_default())
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
            // Any running agent: the dialog says why one without an answer
            // channel is answered in its pane.
            Action::PrepareDraft
            | Action::InterruptAgent
            | Action::Answer
            | Action::Queue
            | Action::Changes
            | Action::Dictate => {
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
            Action::Inbox => self.managed,
            Action::NextUnseen => self.managed && self.inbox.oldest_unseen().is_some(),
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
            Action::Answer => Effect::Answer {
                id: self.selected_id.clone()?,
            },
            Action::Queue => Effect::Queue {
                id: self.selected_id.clone()?,
            },
            Action::Changes => Effect::Changes {
                id: self.selected_id.clone()?,
            },
            Action::Dictate => Effect::Dictate {
                id: self.selected_id.clone()?,
            },
            Action::Inbox => {
                self.toggle_inbox();
                return None;
            }
            Action::NextUnseen => {
                let effect = self.next_unseen().into_iter().next();
                self.dirty = true;
                return effect;
            }
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
        let query = SearchQuery::new(&self.search);
        let mut count = 0usize;
        let mut first_index = None;
        let mut has_selected = false;
        for (index, row) in self.rows.iter().enumerate() {
            if !self.matches_with(row, &query) {
                continue;
            }
            if first_index.is_none() {
                first_index = Some(index);
            }
            if self.selected_id.as_deref() == Some(row.id.as_str()) {
                has_selected = true;
            }
            count += 1;
        }
        if !has_selected {
            self.selected_id = first_index.map(|index| self.rows[index].id.clone());
        }
        let max = count.saturating_sub(self.list_page.max(1));
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
        let query = SearchQuery::new(&self.search);
        let mut selected = 0usize;
        let mut position = 0usize;
        for row in &self.rows {
            if !self.matches_with(row, &query) {
                continue;
            }
            if self.selected_id.as_deref() == Some(row.id.as_str()) {
                selected = position;
                break;
            }
            position += 1;
        }
        if selected < self.list_scroll {
            self.list_scroll = selected;
        } else if selected >= self.list_scroll + self.list_page.max(1) {
            self.list_scroll = selected + 1 - self.list_page.max(1);
        }
    }
}

/// Pre-prepared case-insensitive substring matcher. ASCII queries compare
/// bytes in place with zero heap allocation per candidate row.
enum SearchQuery<'a> {
    Empty,
    Ascii(&'a [u8]),
    Unicode(String),
}

impl<'a> SearchQuery<'a> {
    fn new(search: &'a str) -> Self {
        if search.is_empty() {
            Self::Empty
        } else if search.is_ascii() {
            Self::Ascii(search.as_bytes())
        } else {
            Self::Unicode(search.to_lowercase())
        }
    }

    fn matches(&self, haystack: &str) -> bool {
        match self {
            Self::Empty => true,
            Self::Ascii(needle) => contains_ignore_ascii_case(haystack.as_bytes(), needle),
            Self::Unicode(needle) => haystack.to_lowercase().contains(needle.as_str()),
        }
    }
}

fn contains_ignore_ascii_case(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    if needle.len() > haystack.len() {
        return false;
    }
    let first = needle[0].to_ascii_lowercase();
    let last = haystack.len() - needle.len();
    for start in 0..=last {
        if haystack[start].to_ascii_lowercase() == first
            && haystack[start..start + needle.len()].eq_ignore_ascii_case(needle)
        {
            return true;
        }
    }
    false
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

    #[test]
    fn a_context_menu_closes_when_its_row_leaves() {
        let mut app = App::new(false, Language::English, Theme::Dark);
        let snapshot = |ids: &[&str]| {
            json!({
                "epoch":"boot","revision":"1",
                "observations":ids.iter().map(|id| row(id, true)).collect::<Vec<_>>()
            })
        };
        app.apply_snapshot(snapshot(&["alpha", "beta"]));
        app.selected_id = Some("alpha".into());
        app.overlay = Some(Overlay::Context);
        // The same rows again: the menu still belongs to its row.
        app.apply_snapshot(snapshot(&["alpha", "beta"]));
        assert_eq!(app.overlay, Some(Overlay::Context));
        app.apply_snapshot(snapshot(&["beta"]));
        assert_eq!(app.selected_id.as_deref(), Some("beta"));
        assert_eq!(app.overlay, None);
        // Help is about no row and stays.
        app.overlay = Some(Overlay::Help);
        app.apply_snapshot(snapshot(&["alpha"]));
        assert_eq!(app.overlay, Some(Overlay::Help));
    }

    #[test]
    fn only_a_changed_epoch_resets_receipts_and_actions_in_flight() {
        let mut app = App::new(false, Language::English, Theme::Dark);
        app.set_managed();
        let snapshot = |epoch: &str| json!({"epoch":epoch,"revision":"1","observations":[row("alpha", true), row("beta", true)]});
        app.apply_snapshot(snapshot("identity-of-alpha"));
        app.selected_id = Some("beta".into());
        app.notify("Marked beta as seen".into());
        app.ack_in_flight = Some(("beta".into(), "beta".into()));
        // The managed desk names the selection's identity as the epoch. It
        // sets the new one before the snapshot, so moving the selection is
        // not read as a restart.
        app.epoch = "identity-of-beta".into();
        app.apply_snapshot(snapshot("identity-of-beta"));
        assert_eq!(app.toast.as_deref(), Some("Marked beta as seen"));
        assert!(app.ack_in_flight.is_some());
        app.apply_snapshot(snapshot("another-daemon"));
        assert!(app.toast.is_none());
        assert!(app.ack_in_flight.is_none());
    }
}
