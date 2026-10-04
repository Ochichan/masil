//! Native agent-management desk. The native server is queried only while this view is open.

use super::{
    answer::{AnswerDialog, Outcome},
    i18n::action_label,
    model::{Action, App, Effect, Language, Theme},
    palette::{self, Palette, PaletteResult},
    preferences,
    queue::{QueueOutcome, QueueWindow},
    terminal,
    view::{action_shortcut, cell_width, ellipsize, wrap_rows},
};
use crate::managed::{
    Agent, Manager, Through,
    fleet::{EndpointStatus, Fleet, FleetSnapshot},
};
use crossterm::event::{
    Event, EventStream, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind,
};
use futures_util::StreamExt;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Block, Borders, Clear, Paragraph},
};
use serde_json::{Value, json};
use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Instant};
use tokio::{task::JoinSet, time::MissedTickBehavior};
use unicode_segmentation::UnicodeSegmentation;

#[derive(Default)]
struct Options {
    client: Option<String>,
    target: Option<String>,
    compact: bool,
    language: Option<Language>,
    theme: Option<Theme>,
}

impl Options {
    fn parse(args: &[String], sidebar: bool) -> Result<Self, String> {
        let mut options = Self::default();
        let mut seen = std::collections::HashSet::new();
        let mut index = 0;
        while index < args.len() {
            let key = args[index].as_str();
            if !seen.insert(key) {
                return Err(format!("repeated managed UI option: {key}"));
            }
            if key == "--compact" && !sidebar {
                options.compact = true;
                index += 1;
                continue;
            }
            let value = args
                .get(index + 1)
                .ok_or_else(|| format!("missing value for {key}"))?;
            match key {
                "--client" => options.client = Some(value.clone()),
                "--target" if sidebar => options.target = Some(value.clone()),
                "--lang" => options.language = Some(value.parse()?),
                "--theme" => options.theme = Some(value.parse()?),
                _ => return Err(format!("unknown managed UI option: {key}")),
            }
            index += 2;
        }
        Ok(options)
    }
}

pub(super) fn run(socket: &str, args: &[String], sidebar: bool) -> Result<i32, String> {
    if socket.chars().any(char::is_control) {
        return Err("native socket path contains control characters".into());
    }
    let options = Options::parse(args, sidebar)?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?
        .block_on(async {
            if sidebar {
                open_sidebar(PathBuf::from(socket), options).await
            } else {
                desk(PathBuf::from(socket), options).await
            }
        })
}

async fn open_sidebar(socket: PathBuf, options: Options) -> Result<i32, String> {
    let manager = Manager::new(socket, options.client)?;
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let result = manager
        .native
        .managed_sidebar(
            &executable,
            options.target.as_deref(),
            options.language.map(Language::as_str),
            options.theme.map(Theme::as_str),
        )
        .await?;
    match result {
        crate::native_ui::SidebarOutcome::Created { pane_id, .. } => {
            println!("Created agent management sidebar {pane_id}. Shared native window layout.")
        }
        crate::native_ui::SidebarOutcome::Reused { pane_id, .. } => {
            println!("Agent management sidebar already open in {pane_id}.")
        }
    }
    Ok(0)
}

enum DialogKind {
    Start {
        default_endpoint: String,
        endpoint_field: bool,
        endpoint_keys: HashMap<String, String>,
    },
    Resume(Agent),
    Rename(Agent),
    Draft(Agent),
    Prompt(Agent),
    Interrupt(Agent),
    Close(Agent),
}

struct Field {
    label: &'static str,
    value: String,
    limit: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DialogHit {
    Field(usize),
    Cancel,
    Submit,
}

struct Dialog {
    language: Language,
    kind: DialogKind,
    title: &'static str,
    note: &'static str,
    submit: &'static str,
    fields: Vec<Field>,
    focus: usize,
    hits: Vec<(Rect, DialogHit)>,
    pressed: Option<DialogHit>,
}

enum DialogResult {
    None,
    Cancel,
    Submit,
}

impl Dialog {
    fn start(language: Language, endpoints: &[(String, String)], selected: Option<&Agent>) -> Self {
        let default_endpoint = selected
            .map(|agent| agent.endpoint_id.as_str())
            .filter(|endpoint| endpoints.iter().any(|(candidate, _)| candidate == endpoint))
            .or_else(|| endpoints.first().map(|(id, _)| id.as_str()))
            .unwrap_or("local")
            .to_owned();
        let endpoint_field = endpoints.len() > 1;
        let mut fields = Vec::new();
        if endpoint_field {
            fields.push(Field {
                label: word(language, "Server", "서버"),
                value: default_endpoint.clone(),
                limit: 64,
            });
        }
        let local_cwd = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .to_string_lossy()
            .into_owned();
        fields.extend([
            Field {
                label: word(language, "Name", "이름"),
                value: String::new(),
                limit: 32,
            },
            Field {
                label: word(language, "Provider", "제공자"),
                value: crate::providers::all()
                    .first()
                    .map_or("codex", |p| p.id)
                    .into(),
                limit: 64,
            },
            Field {
                label: word(language, "Working directory", "작업 디렉터리"),
                value: if default_endpoint == "local" {
                    local_cwd
                } else {
                    String::new()
                },
                limit: 4096,
            },
        ]);
        Self {
            language,
            kind: DialogKind::Start {
                default_endpoint,
                endpoint_field,
                endpoint_keys: endpoints.iter().cloned().collect(),
            },
            title: word(language, "New native agent", "새 네이티브 에이전트"),
            note: word(
                language,
                "Starts a new pane. Idle is not treated as task completion.",
                "새 창에서 시작합니다. 대기 상태를 작업 완료로 취급하지 않습니다.",
            ),
            submit: word(language, "Start", "시작"),
            fields,
            focus: 0,
            hits: Vec::new(),
            pressed: None,
        }
    }

    fn resume(agent: &Agent, language: Language) -> Self {
        Self {
            language,
            kind: DialogKind::Resume(agent.clone()),
            title: word(language, "Resume native session", "네이티브 세션 재개"),
            note: word(
                language,
                "Creates a new pane from the reported session reference; acceptance remains unverified.",
                "보고된 세션 참조로 새 창을 만듭니다. 세션 수락 여부는 확인되지 않습니다.",
            ),
            submit: word(language, "Resume", "재개"),
            fields: vec![Field {
                label: word(language, "New name", "새 이름"),
                value: format!("{}-resume", agent.name.chars().take(25).collect::<String>()),
                limit: 32,
            }],
            focus: 0,
            hits: Vec::new(),
            pressed: None,
        }
    }

    fn rename(agent: &Agent, language: Language) -> Self {
        Self {
            language,
            kind: DialogKind::Rename(agent.clone()),
            title: word(language, "Rename managed agent", "관리 에이전트 이름 변경"),
            note: word(
                language,
                "Updates masil metadata for this pane.",
                "이 창의 masil 메타데이터를 변경합니다.",
            ),
            submit: word(language, "Rename", "이름 변경"),
            fields: vec![Field {
                label: word(language, "Name", "이름"),
                value: agent.name.clone(),
                limit: 32,
            }],
            focus: 0,
            hits: Vec::new(),
            pressed: None,
        }
    }

    fn draft(agent: &Agent, language: Language) -> Self {
        Self {
            language,
            kind: DialogKind::Draft(agent.clone()),
            title: word(language, "Prepare draft", "초안 준비"),
            note: word(
                language,
                "Copies text to a tmux buffer only. It does not paste or submit it.",
                "텍스트를 tmux 버퍼에만 복사합니다. 붙여넣거나 전송하지 않습니다.",
            ),
            submit: word(language, "Prepare", "준비"),
            fields: vec![Field {
                label: word(language, "Draft text", "초안 텍스트"),
                value: String::new(),
                limit: 32_768,
            }],
            focus: 0,
            hits: Vec::new(),
            pressed: None,
        }
    }

    fn prompt(agent: &Agent, language: Language) -> Self {
        Self {
            language,
            kind: DialogKind::Prompt(agent.clone()),
            title: word(language, "Send prompt", "프롬프트 전송"),
            note: word(
                language,
                "Sends this text with Enter to the verified idle agent. Delivery does not prove provider acceptance or task success.",
                "검증된 대기 에이전트에 이 텍스트와 Enter를 전달합니다. 전달은 제공자 수락이나 작업 성공을 뜻하지 않습니다.",
            ),
            submit: word(language, "Send", "전송"),
            fields: vec![Field {
                label: word(language, "Prompt text", "프롬프트 텍스트"),
                value: String::new(),
                limit: 32_768,
            }],
            focus: 0,
            hits: Vec::new(),
            pressed: None,
        }
    }

    fn confirm(
        language: Language,
        kind: DialogKind,
        title: &'static str,
        note: &'static str,
        submit: &'static str,
    ) -> Self {
        Self {
            language,
            kind,
            title,
            note,
            submit,
            fields: Vec::new(),
            focus: 0,
            hits: Vec::new(),
            pressed: None,
        }
    }

    fn handle(&mut self, event: Event) -> DialogResult {
        match event {
            Event::Key(key) => self.key(key),
            Event::Paste(text) => {
                self.insert(&text);
                DialogResult::None
            }
            Event::Mouse(mouse) => {
                let target = self
                    .hits
                    .iter()
                    .rev()
                    .find(|(rect, _)| contains(*rect, mouse.column, mouse.row))
                    .map(|(_, hit)| *hit);
                match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left) => self.pressed = target,
                    MouseEventKind::Up(MouseButton::Left) => {
                        let pressed = self.pressed.take();
                        if pressed == target {
                            match target {
                                Some(DialogHit::Field(index)) => self.focus = index,
                                Some(DialogHit::Cancel) => return DialogResult::Cancel,
                                Some(DialogHit::Submit) => return DialogResult::Submit,
                                None => {}
                            }
                        }
                    }
                    _ => {}
                }
                DialogResult::None
            }
            _ => DialogResult::None,
        }
    }

    fn key(&mut self, key: KeyEvent) -> DialogResult {
        match key.code {
            KeyCode::Esc => DialogResult::Cancel,
            KeyCode::Enter => DialogResult::Submit,
            KeyCode::Tab => {
                if !self.fields.is_empty() {
                    self.focus = (self.focus + 1) % self.fields.len();
                }
                DialogResult::None
            }
            KeyCode::BackTab => {
                if !self.fields.is_empty() {
                    self.focus = (self.focus + self.fields.len() - 1) % self.fields.len();
                }
                DialogResult::None
            }
            KeyCode::Backspace => {
                if let Some(field) = self.fields.get_mut(self.focus)
                    && let Some((index, _)) = field.value.grapheme_indices(true).next_back()
                {
                    field.value.truncate(index);
                }
                DialogResult::None
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(field) = self.fields.get_mut(self.focus) {
                    field.value.clear();
                }
                DialogResult::None
            }
            KeyCode::Char(character)
                if !key.modifiers.intersects(
                    KeyModifiers::CONTROL
                        | KeyModifiers::ALT
                        | KeyModifiers::SUPER
                        | KeyModifiers::META,
                ) =>
            {
                self.insert(&character.to_string());
                DialogResult::None
            }
            _ => DialogResult::None,
        }
    }

    fn insert(&mut self, text: &str) {
        let Some(field) = self.fields.get_mut(self.focus) else {
            return;
        };
        for character in text.chars().filter(|character| !character.is_control()) {
            if field.value.len() + character.len_utf8() > field.limit {
                break;
            }
            field.value.push(character);
        }
    }

    fn draw(&mut self, frame: &mut Frame, theme: Theme) {
        let area = frame.area();
        let width = area.width.saturating_sub(4).clamp(1, 72).min(area.width);
        let text_width = width.saturating_sub(4) as usize;
        // Fields, the wrapped note, a gap and the buttons. A short screen
        // shows fewer fields around the focused one and shortens the note.
        let room = area.height.saturating_sub(2).max(5).min(area.height);
        let space = room.saturating_sub(4);
        let total = self.fields.len() as u16;
        let shown = total.min(space / 2);
        let first = (self.focus + 1).saturating_sub(shown as usize);
        let note = wrap_rows(
            self.note,
            text_width,
            space.saturating_sub(shown * 2) as usize,
        );
        let height = (shown * 2 + note.len() as u16 + 4).min(room);
        let modal = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        );
        let (panel, text, muted, accent, accent_text) = dialog_colors(theme);
        frame.render_widget(Clear, modal);
        frame.render_widget(
            Block::default()
                .borders(Borders::ALL)
                .title(ellipsize(
                    &format!(" {} ", self.title),
                    width.saturating_sub(2) as usize,
                ))
                .border_style(Style::default().fg(accent))
                .style(Style::default().bg(panel).fg(text)),
            modal,
        );
        // Hidden fields are marked in the border; Tab still reaches them.
        for (hidden, y) in [
            (first > 0, modal.y),
            (
                first + usize::from(shown) < self.fields.len(),
                modal.bottom().saturating_sub(1),
            ),
        ] {
            if hidden && modal.width > 3 {
                frame.render_widget(
                    Paragraph::new(if y == modal.y { "↑" } else { "↓" })
                        .style(Style::default().fg(accent).bg(panel)),
                    Rect::new(modal.right() - 2, y, 1, 1),
                );
            }
        }
        self.hits.clear();
        let inner = Rect::new(
            modal.x + 2,
            modal.y + 1,
            modal.width.saturating_sub(4),
            modal.height.saturating_sub(2),
        );
        let mut y = inner.y;
        for (index, field) in self
            .fields
            .iter()
            .enumerate()
            .skip(first)
            .take(shown as usize)
        {
            frame.render_widget(
                Paragraph::new(ellipsize(field.label, text_width))
                    .style(Style::default().fg(muted)),
                Rect::new(inner.x, y, inner.width, 1),
            );
            let selected = self.focus == index;
            // Typing appends, so a long value keeps its end in view.
            let value = if field.value.is_empty() {
                " ".to_owned()
            } else {
                tail(&field.value, text_width)
            };
            let rect = Rect::new(inner.x, y + 1, inner.width, 1);
            frame.render_widget(
                Paragraph::new(value).style(
                    Style::default()
                        .fg(if selected { accent_text } else { text })
                        .bg(if selected { accent } else { panel }),
                ),
                rect,
            );
            self.hits.push((rect, DialogHit::Field(index)));
            y += 2;
        }
        let note_room = inner.bottom().saturating_sub(y + 2);
        if note_room > 0 {
            frame.render_widget(
                Paragraph::new(
                    note.into_iter()
                        .take(note_room as usize)
                        .map(Line::from)
                        .collect::<Vec<_>>(),
                )
                .style(Style::default().fg(muted)),
                Rect::new(inner.x, y, inner.width, note_room),
            );
        }
        let button_y = modal.bottom().saturating_sub(2);
        let submit_label = format!(" {} ", self.submit);
        let submit_width = (cell_width(&submit_label) as u16).min(inner.width);
        // Cancel falls back to its key when both labels cannot fit.
        let cancel_label = [word(self.language, " Cancel ", " 취소 "), " Esc "]
            .into_iter()
            .find(|label| cell_width(label) as u16 + 1 + submit_width <= inner.width)
            .unwrap_or("");
        let cancel_width = cell_width(cancel_label) as u16;
        let submit = Rect::new(
            inner.right().saturating_sub(submit_width),
            button_y,
            submit_width,
            1,
        );
        let cancel = Rect::new(
            submit.x.saturating_sub(cancel_width + 1),
            button_y,
            cancel_width,
            1,
        );
        frame.render_widget(
            Paragraph::new(submit_label).style(
                Style::default()
                    .bg(accent)
                    .fg(accent_text)
                    .add_modifier(Modifier::BOLD),
            ),
            submit,
        );
        if cancel.width > 0 {
            frame.render_widget(
                Paragraph::new(cancel_label).style(Style::default().fg(text)),
                cancel,
            );
            self.hits.push((cancel, DialogHit::Cancel));
        }
        self.hits.push((submit, DialogHit::Submit));
    }
}

/// The end of `text` within `width` cells, marked with a leading ellipsis
/// when the start is hidden.
fn tail(text: &str, width: usize) -> String {
    if cell_width(text) <= width {
        return text.to_owned();
    }
    let mut kept = Vec::new();
    let mut used = 1;
    for grapheme in text.graphemes(true).rev() {
        let next = cell_width(grapheme);
        if used + next > width {
            break;
        }
        kept.push(grapheme);
        used += next;
    }
    kept.reverse();
    format!("…{}", kept.concat())
}

fn word(language: Language, english: &'static str, korean: &'static str) -> &'static str {
    match language {
        Language::English => english,
        Language::Korean => korean,
    }
}

fn message(language: Language, english: String, korean: String) -> String {
    match language {
        Language::English => english,
        Language::Korean => korean,
    }
}

pub(super) fn dialog_colors(theme: Theme) -> (Color, Color, Color, Color, Color) {
    match theme {
        Theme::Dark => (
            Color::Rgb(31, 39, 49),
            Color::Rgb(230, 235, 241),
            Color::Rgb(155, 168, 182),
            Color::Rgb(91, 181, 194),
            Color::Rgb(5, 19, 22),
        ),
        Theme::Light => (
            Color::Rgb(250, 251, 251),
            Color::Rgb(25, 34, 41),
            Color::Rgb(84, 100, 111),
            Color::Rgb(0, 112, 127),
            Color::White,
        ),
        Theme::Terminal => (
            Color::Reset,
            Color::Reset,
            Color::Gray,
            Color::Cyan,
            Color::Black,
        ),
    }
}

fn contains(rect: Rect, column: u16, row: u16) -> bool {
    column >= rect.x && column < rect.right() && row >= rect.y && row < rect.bottom()
}

fn endpoint_status_line(statuses: &[EndpointStatus], language: Language) -> (String, bool) {
    let offline = statuses.iter().any(|status| !status.connected);
    let entries = statuses
        .iter()
        .map(|status| {
            let label = status
                .label
                .chars()
                .filter(|character| !character.is_control())
                .collect::<String>();
            let source = if label == status.id {
                label
            } else {
                format!("{label} [{}]", status.id)
            };
            if status.connected {
                format!("{source} {}", word(language, "online", "온라인"))
            } else {
                let error = status
                    .error
                    .as_deref()
                    .map(crate::managed::failure::human)
                    .unwrap_or(word(language, "unavailable", "사용 불가"))
                    .chars()
                    .filter(|character| !character.is_control())
                    .take(80)
                    .collect::<String>();
                format!(
                    "{source} {}: {error}",
                    word(language, "offline", "오프라인")
                )
            }
        })
        .collect::<Vec<_>>()
        .join(" · ");
    (
        format!("{}: {entries}", word(language, "Servers", "서버")),
        offline,
    )
}

enum Work {
    Poll(Result<FleetSnapshot, String>),
    Action(ActionMessage),
    Panes(Result<(Vec<crate::managed::find::PaneTarget>, bool), String>),
    Inbox(Result<crate::managed::inbox::InboxView, String>),
    /// A queue window's list, after a change when there was one.
    Queue(QueueMessage),
    /// A changes window's result.
    Changes(ChangesMessage),
    /// A pager popup that could not open.
    Pager(Result<(), String>),
}

struct ChangesMessage {
    agent: Box<Agent>,
    /// The window opening that asked, if any.
    window: Option<u64>,
    /// The `f` press that asked to open the window.
    press: Option<u64>,
    op: crate::managed::changes::ChangesOp,
    result: Result<Value, String>,
}

struct QueueMessage {
    agent: Box<Agent>,
    /// The `e` press that asked to open the window.
    press: Option<u64>,
    done: Option<(&'static str, Result<Value, String>)>,
    view: Result<Value, String>,
}
struct ActionMessage {
    boot: String,
    target: Option<TargetIdentity>,
    result: Result<ActionResult, String>,
}
#[derive(Clone)]
struct TargetIdentity {
    endpoint_key: String,
    pane_id: String,
    boot: String,
    generation: String,
    run: String,
}
enum ActionResult {
    Receipt(String),
    Details {
        id: String,
        value: Value,
    },
    /// The agent's pending requests, to answer in a dialog: `press` is the
    /// `y` press it answers, and `event` the inbox event's pane and run when
    /// it was pressed there.
    Requests {
        agent: Box<Agent>,
        value: Value,
        press: u64,
        event: Option<(String, String)>,
    },
}

impl TargetIdentity {
    fn for_agent(agent: &Agent) -> Self {
        Self {
            endpoint_key: agent.endpoint_key.clone(),
            pane_id: agent.pane_id.clone(),
            boot: agent.boot.clone(),
            generation: agent.generation.clone(),
            run: agent.run.clone(),
        }
    }

    fn same_run(&self, agents: &HashMap<String, Agent>) -> bool {
        agents.values().any(|agent| {
            agent.endpoint_key == self.endpoint_key
                && agent.pane_id == self.pane_id
                && agent.boot == self.boot
                && agent.generation == self.generation
                && agent.run == self.run
        })
    }
}

fn selected_identity(app: &App, agents: &HashMap<String, Agent>) -> Option<String> {
    app.selected_id
        .as_ref()
        .and_then(|id| agents.get(id))
        .map(agent_identity)
}

fn agent_identity(agent: &Agent) -> String {
    format!(
        "{}:{}:{}:{}:{}",
        agent.endpoint_key, agent.boot, agent.generation, agent.run, agent.stale
    )
}

fn action_message(
    boot: String,
    target: Option<TargetIdentity>,
    result: Result<ActionResult, String>,
) -> Work {
    Work::Action(ActionMessage {
        boot,
        target,
        result,
    })
}

async fn desk(socket: PathBuf, options: Options) -> Result<i32, String> {
    let preferences = preferences::Preferences::load();
    let mut app = App::new(
        options.compact,
        options.language.unwrap_or(preferences.language),
        options.theme.unwrap_or(preferences.theme),
    );
    app.set_managed();
    app.set_native_available(true);
    if let Some(warning) = &preferences.warning {
        app.notify(warning.clone());
    }
    let fleet = Arc::new(Fleet::new(Manager::new(socket.clone(), options.client)?)?);
    // An inbox in another state directory is not the one the server writes.
    {
        let socket = socket.clone();
        app.inbox.mismatch = tokio::task::spawn_blocking(move || {
            crate::coordinator::check_state(&socket)
                .is_err_and(|error| error.starts_with("coordinator_state_mismatch"))
        })
        .await
        .unwrap_or(false);
    }
    let mut inbox_pending = false;
    let mut inbox_due = false;
    let mut inbox_checked: Option<Instant> = None;
    let mut last_poll_ms = 0u64;
    let mut current_epoch = String::new();
    let owned_pane = options
        .compact
        .then(|| std::env::var("TMUX_PANE").ok())
        .flatten();
    let mut agents = HashMap::<String, Agent>::new();
    let mut last_projection: Option<Vec<Value>> = None;
    let mut revision = 0_u64;
    let mut tasks = JoinSet::<Work>::new();
    let mut poll_pending = false;
    let mut poll = tokio::time::interval(std::time::Duration::from_secs(1));
    poll.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut dialog: Option<Dialog> = None;
    let mut answering: Option<AnswerDialog> = None;
    let mut queueing: Option<QueueWindow> = None;
    let mut changing: Option<super::changes::ChangesWindow> = None;
    let mut palette: Option<Palette> = None;

    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|error| error.to_string())?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .map_err(|error| error.to_string())?;
    let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        .map_err(|error| error.to_string())?;
    let mut terminal = terminal::Session::open()?;
    let mut events = EventStream::new();
    let result = async {
        loop {
            if app.dirty {
                terminal.terminal().draw(|frame| {
                    app.draw(frame);
                    if let Some(dialog) = &mut dialog { dialog.draw(frame, app.theme); }
                    if let Some(answering) = &mut answering { answering.draw(frame, app.theme); }
                    if let Some(queueing) = &mut queueing { queueing.draw(frame, app.theme); }
                    if let Some(changing) = &mut changing { changing.draw(frame, app.theme); }
                    if let Some(palette) = &mut palette { palette.draw(frame, app.theme); }
                }).map_err(|error| error.to_string())?;
                app.dirty = false;
            }
            let effects = tokio::select! {
                _ = poll.tick(), if !poll_pending => {
                    poll_pending = true;
                    let fleet = fleet.clone();
                    tasks.spawn(async move {
                        Work::Poll(fleet.poll().await)
                    });
                    Vec::new()
                }
                event = events.next() => match event {
                    Some(Ok(event)) => {
                        if let Some(active) = &mut palette {
                            if matches!(event, Event::Resize(_, _)) {
                                app.handle(event.clone(), Instant::now());
                            }
                            let outcome = active.handle(event, Instant::now());
                            app.dirty = true;
                            match outcome {
                                PaletteResult::None => Vec::new(),
                                PaletteResult::Close => { palette = None; Vec::new() },
                                PaletteResult::Activate(target) => {
                                    palette = None;
                                    activate_palette(target, &mut app, &agents, fleet.clone(), &mut tasks, owned_pane.as_deref(), &current_epoch)
                                }
                            }
                        } else if matches!(&event, Event::Key(key) if key.code == KeyCode::Char(':') && key.modifiers.difference(KeyModifiers::SHIFT).is_empty())
                            && dialog.is_none()
                            && answering.is_none()
                            && queueing.is_none()
                            && changing.is_none()
                            && app.overlay.is_none()
                            && app.focus != super::model::Focus::Search
                        {
                            let context = selected_agent(&app, &agents).map(crate::managed::find::agent_label);
                            palette = Some(Palette::new(app.language, palette_items(&app, &agents), context));
                            let fleet = fleet.clone();
                            tasks.spawn(async move { Work::Panes(fleet.local.pane_targets().await) });
                            app.dirty = true;
                            Vec::new()
                        } else if let Some(active) = &mut changing {
                            use super::changes::ChangesOutcome;
                            let outcome = active.handle(event);
                            app.dirty = true;
                            match outcome {
                                ChangesOutcome::None => {}
                                ChangesOutcome::Close => changing = None,
                                ChangesOutcome::Op(op) => {
                                    changes_task(&mut tasks, fleet.clone(), active.agent.clone(), op, None, Some(active.generation));
                                }
                                ChangesOutcome::Pager(text) => {
                                    let fleet = fleet.clone();
                                    tasks.spawn(async move { Work::Pager(fleet.local.open_pager(text).await) });
                                }
                            }
                            Vec::new()
                        } else if let Some(active) = &mut queueing {
                            let outcome = active.handle(event);
                            app.dirty = true;
                            match outcome {
                                QueueOutcome::None => {}
                                QueueOutcome::Close => queueing = None,
                                QueueOutcome::Op(op) => {
                                    let agent = active.agent.clone();
                                    queue_task(&mut tasks, fleet.clone(), agent, Some(op), None);
                                }
                            }
                            Vec::new()
                        } else if let Some(active) = &mut answering {
                            let outcome = active.handle(event);
                            app.dirty = true;
                            match outcome {
                                Outcome::None => {}
                                Outcome::Cancel => answering = None,
                                Outcome::Submit { request, reply } => {
                                    let agent = answering.take().expect("active answer dialog").agent;
                                    submit_answer(agent, request, reply, fleet.clone(), &mut tasks, app.language, &current_epoch);
                                }
                            }
                            Vec::new()
                        } else if let Some(active) = &mut dialog {
                            let outcome = active.handle(event);
                            app.dirty = true;
                            match outcome {
                                DialogResult::None => Vec::new(),
                                DialogResult::Cancel => { dialog = None; Vec::new() },
                                DialogResult::Submit => {
                                    let submitted = dialog.take().expect("active dialog");
                                    submit_dialog(submitted, fleet.clone(), &mut tasks, &mut app, &current_epoch);
                                    Vec::new()
                                }
                            }
                        } else if !app.inbox.open && matches!(&event, Event::Key(key) if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)) {
                            if let Some(agent) = selected_agent(&app, &agents).filter(|agent| !agent.stale) {
                                dialog = Some(Dialog::confirm(app.language, DialogKind::Interrupt(agent.clone()), word(app.language, "Interrupt agent?", "에이전트를 중단할까요?"), word(app.language, "Sends the provider's interrupt key (Esc for Claude and Codex) while its screen shows a turn. Whether the provider stopped is not asserted.", "화면에 turn이 보일 때 provider의 끊기 key(Claude, Codex는 Esc)를 보냅니다. provider가 멈췄는지는 확인하지 않습니다."), word(app.language, "Interrupt", "중단")));
                                app.dirty = true;
                            }
                            Vec::new()
                        } else { app.handle(event, Instant::now()) }
                    },
                    Some(Err(error)) => return Err(error.to_string()),
                    None => break,
                },
                Some(completed) = tasks.join_next(), if !tasks.is_empty() => {
                    match completed {
                        Ok(Work::Poll(result)) => {
                            poll_pending = false;
                            match result {
                                Ok(FleetSnapshot { epoch, agents: list, endpoints }) => {
                                    let previous_selected_identity = selected_identity(&app, &agents);
                                    let projection = list.iter().map(Agent::projection).collect::<Vec<_>>();
                                    let mut stable_projection = projection.clone();
                                    for (row, agent) in stable_projection.iter_mut().zip(&list) {
                                        row["native"]["observed_at_ms"] = json!(0);
                                        row["_run"] = json!(agent.run);
                                        row["_endpoint_key"] = json!(agent.endpoint_key);
                                    }
                                    let next_selected_identity = app
                                        .selected_id
                                        .as_ref()
                                        .and_then(|id| list.iter().find(|agent| &agent.id == id))
                                        .or_else(|| list.first())
                                        .map(agent_identity);
                                    let next_agents = list.into_iter().map(|agent| (agent.id.clone(), agent)).collect::<HashMap<_, _>>();
                                    let identity_changed = previous_selected_identity != next_selected_identity;
                                    let app_epoch = next_selected_identity
                                        .clone()
                                        .unwrap_or_else(|| "managed-fleet-empty".into());
                                    agents = next_agents;
                                    if let Some(window) = &mut changing
                                        && let Some(agent) = agents.values().find(|agent| agent.pane_id == window.pane())
                                    {
                                        window.seen_run(&agent.run);
                                    }
                                    last_poll_ms = crate::observation::now_ms();
                                    inbox_due = true;
                                    let mut names = agents
                                        .values()
                                        .filter(|agent| agent.endpoint_id.is_empty() || agent.endpoint_id == "local")
                                        .map(|agent| (agent.pane_id.clone(), agent.run.clone(), agent.name.clone()))
                                        .collect::<Vec<_>>();
                                    names.sort();
                                    if app.inbox.set_names(names) && app.inbox.open {
                                        app.dirty = true;
                                    }
                                    let next_endpoint_line = endpoint_status_line(&endpoints, app.language);
                                    if app.endpoint_line.as_ref() != Some(&next_endpoint_line) {
                                        app.endpoint_line = Some(next_endpoint_line);
                                        app.dirty = true;
                                    }
                                    if identity_changed {
                                        // The queue window stays: it shows the agent's new run
                                        // and keeps typed text from reaching the desk's keys.
                                        if let Some(window) = &queueing {
                                            let agent = window.agent.clone();
                                            queue_task(&mut tasks, fleet.clone(), agent, None, None);
                                        }
                                        dialog = None;
                                        answering = None;
                                        palette = None;
                                        app.cancel_interactions();
                                        app.overlay = None;
                                        app.ack_in_flight = None;
                                        app.prompt_in_flight = false;
                                        app.toast = None;
                                    }
                                    if epoch != current_epoch || last_projection.as_ref() != Some(&stable_projection) {
                                        revision = revision.wrapping_add(1);
                                        app.apply_snapshot(json!({"epoch":app_epoch,"revision":revision.to_string(),"observations":projection}));
                                        last_projection = Some(stable_projection);
                                    }
                                    current_epoch = epoch;
                                    if !app.connected || app.connection_message.is_some() { app.set_connection(true, None); }
                                }
                                Err(error) => {
                                    let message = format!("Native inventory unavailable: {error}");
                                    if app.connected || app.connection_message.as_deref() != Some(&message) { app.set_connection(false, Some(message)); }
                                }
                            }
                        }
                        Ok(Work::Inbox(result)) => {
                            inbox_pending = false;
                            if let Ok(view) = result && app.inbox.apply(view) {
                                app.dirty = true;
                            }
                        }
                        Ok(Work::Action(message)) if message.target.as_ref().is_some_and(|target| target.same_run(&agents)) || (message.target.is_none() && message.boot == current_epoch) => match message.result {
                            Ok(ActionResult::Receipt(receipt)) => { app.finish_action(); app.notify(receipt); app.inbox.refresh = true; },
                            Ok(ActionResult::Details { id, value }) => app.set_details(&id, value),
                            Ok(ActionResult::Requests { agent, value, press, event }) => {
                                app.dirty = true;
                                // Only the latest press opens, and only onto the view it was
                                // pressed in: a late list must not take a key meant elsewhere.
                                let same_view = if let Some((pane, run)) = &event {
                                    app.inbox.open
                                        && app.inbox.selected_item().is_some_and(|item| &item.pane == pane && &item.run == run)
                                } else {
                                    !app.inbox.open && app.selected_id.as_ref() == Some(&agent.id)
                                };
                                let free = answering.is_none()
                                    && queueing.is_none()
                                    && changing.is_none()
                                    && dialog.is_none()
                                    && palette.is_none()
                                    && app.overlay.is_none()
                                    && app.focus != super::model::Focus::Search;
                                if press != app.answer_request {
                                } else if !(same_view && free) {
                                    app.notify(self::message(
                                        app.language,
                                        "The view changed before the requests arrived; press y again".into(),
                                        "요청 목록이 오기 전에 화면이 바뀌었습니다. y를 다시 누르세요".into(),
                                    ));
                                } else if let Some(opened) = open_answers(*agent, value, &mut app) {
                                    answering = Some(opened);
                                }
                            }
                            Err(error) => { app.finish_action(); app.notify(format!("Action failed: {}", crate::managed::failure::human(&error))); app.inbox.refresh = true; },
                        },
                        Ok(Work::Action(_)) => {},
                        Ok(Work::Pager(Err(error))) => {
                            let notice = format!("Pager: {}", crate::managed::failure::human(&error));
                            match &mut changing {
                                Some(window) => window.tell(notice),
                                None => app.notify(notice),
                            }
                            app.dirty = true;
                        }
                        Ok(Work::Pager(Ok(()))) => {}
                        Ok(Work::Changes(message)) => {
                            app.dirty = true;
                            let ChangesMessage { agent, window: opening, press, op, result } = message;
                            if let Some(window) = changing.as_mut().filter(|window| opening == Some(window.generation)) {
                                window.receive(&op, result);
                                if let Some(next) = window.needs() {
                                    changes_task(&mut tasks, fleet.clone(), window.agent.clone(), next, None, Some(window.generation));
                                }
                            } else if press.is_some_and(|press| press == app.changes_request) {
                                match result {
                                    Ok(files) => {
                                        let free = answering.is_none()
                                            && queueing.is_none()
                                            && changing.is_none()
                                            && dialog.is_none()
                                            && palette.is_none()
                                            && app.overlay.is_none()
                                            && app.focus != super::model::Focus::Search
                                            && !app.inbox.open
                                            && app.selected_id.as_ref() == Some(&agent.id);
                                        if free {
                                            let reviewers = agents
                                                .values()
                                                .filter(|other| other.id != agent.id && !other.stale)
                                                .filter(|other| other.endpoint_id.is_empty() || other.endpoint_id == "local")
                                                .map(|other| other.name.clone())
                                                .collect::<std::collections::BTreeSet<_>>()
                                                .into_iter()
                                                .collect();
                                            changing = Some(super::changes::ChangesWindow::new(app.changes_request, app.language, *agent, reviewers, files));
                                        } else {
                                            app.notify(self::message(
                                                app.language,
                                                "The view changed before the changes arrived; press f again".into(),
                                                "변경 목록이 오기 전에 화면이 바뀌었습니다. f를 다시 누르세요".into(),
                                            ));
                                        }
                                    }
                                    Err(error) => app.notify(format!("Action failed: {}", crate::managed::failure::human(&error))),
                                }
                            }
                        }
                        Ok(Work::Queue(message)) => {
                            app.dirty = true;
                            let QueueMessage { agent, press, done, view } = message;
                            let window = queueing.as_mut().filter(|window| window.pane() == agent.pane_id);
                            // Shown in the window whatever the agent runs now.
                            if let Some((kind, result)) = done {
                                match (&result, window) {
                                    (Ok(value), Some(window)) if kind == "show" => window.edit_body(value),
                                    (_, Some(window)) => window.finished(result.as_ref().map(|_| ()).map_err(String::as_str), queue_done(app.language, kind, &result)),
                                    (_, None) => app.notify(queue_done(app.language, kind, &result)),
                                }
                            }
                            match view {
                                Ok(view) => {
                                    if let Some(window) = queueing.as_mut().filter(|window| window.pane() == agent.pane_id) {
                                        window.refresh(view);
                                    } else if press.is_some_and(|press| press == app.queue_request) {
                                        let free = answering.is_none()
                                            && queueing.is_none()
                                            && changing.is_none()
                                            && dialog.is_none()
                                            && palette.is_none()
                                            && app.overlay.is_none()
                                            && app.focus != super::model::Focus::Search
                                            && !app.inbox.open
                                            && app.selected_id.as_ref() == Some(&agent.id);
                                        if free {
                                            queueing = Some(QueueWindow::new(app.language, *agent, view));
                                        } else {
                                            app.notify(self::message(
                                                app.language,
                                                "The view changed before the queue arrived; press e again".into(),
                                                "대기열이 오기 전에 화면이 바뀌었습니다. e를 다시 누르세요".into(),
                                            ));
                                        }
                                    }
                                }
                                Err(error) if press.is_some() => {
                                    app.notify(format!("Action failed: {}", crate::managed::failure::human(&error)));
                                }
                                Err(_) => {}
                            }
                        }
                        Ok(Work::Panes(result)) => {
                            if let Some(active) = &mut palette {
                                active.set_panes(result);
                                app.dirty = true;
                            }
                        }
                        Err(error) => { poll_pending = false; app.finish_action(); app.notify(format!("Action failed: {error}")); }
                    }
                    Vec::new()
                },
                _ = terminate.recv() => break,
                _ = interrupt.recv() => break,
                _ = hangup.recv() => break,
            };

            // The inbox is read after each poll while it is on, and its switch
            // checked every 5 s while it is off; the store is not opened then.
            if !inbox_pending && !app.inbox.mismatch && (app.inbox.refresh || inbox_due) {
                let check = app.inbox.refresh
                    || app.inbox.view.enabled
                    || inbox_checked.is_none_or(|at| at.elapsed() >= std::time::Duration::from_secs(5));
                if check {
                    inbox_pending = true;
                    inbox_checked = Some(Instant::now());
                    let fleet = fleet.clone();
                    let all = app.inbox.all;
                    tasks.spawn(async move { Work::Inbox(fleet.inbox_view(all).await) });
                }
                app.inbox.refresh = false;
                inbox_due = false;
            }
            for effect in effects {
                if matches!(effect, Effect::Quit) { return Ok(0); }
                if let Effect::Preferences { language, theme } = effect {
                    if let Err(error) = preferences.save(language, theme) { app.notify(error); }
                    continue;
                }
                if tasks.len() >= 8 {
                    app.finish_action();
                    app.notify("An action is pending; try again shortly".into());
                    continue;
                }
                dispatch_effect(effect, &mut dialog, &mut app, &agents, fleet.clone(), &mut tasks, owned_pane.as_deref(), &current_epoch, last_poll_ms);
                app.dirty = true;
            }
        }
        Ok(0)
    }.await;
    tasks.abort_all();
    drop(events);
    drop(terminal);
    result
}

/// Agents in desk order, then the desk actions for the current selection.
/// Panes of this server are added once the background query returns.
fn palette_items(app: &App, agents: &HashMap<String, Agent>) -> Vec<palette::Item> {
    // The inbox view selects events, not agents: only its own actions.
    if app.managed && app.inbox.open {
        return [Action::Inbox, Action::NextUnseen]
            .into_iter()
            .map(|action| palette::Item {
                kind: palette::Kind::Action,
                label: action_label(app.language, action, false, false).to_owned(),
                detail: String::new(),
                hint: action_shortcut(action).to_owned(),
                target: palette::Target::Action(action),
                enabled: app.action_enabled(action),
                pane: None,
            })
            .collect();
    }
    let mut items: Vec<palette::Item> = app
        .rows
        .iter()
        .filter_map(|row| agents.get(&row.id))
        .map(|agent| palette::Item {
            kind: palette::Kind::Agent,
            label: crate::managed::find::agent_label(agent),
            detail: crate::managed::find::agent_detail(agent),
            hint: agent.state.clone(),
            target: palette::Target::Agent(agent.id.clone()),
            enabled: !agent.stale,
            pane: agent.endpoint_id.is_empty().then(|| agent.pane_id.clone()),
        })
        .collect();
    let inbox_actions = if app.managed {
        vec![Action::Inbox, Action::NextUnseen]
    } else {
        Vec::new()
    };
    items.extend(
        app.context_actions()
            .into_iter()
            .chain(inbox_actions)
            .map(|action| palette::Item {
                kind: palette::Kind::Action,
                label: action_label(app.language, action, app.ack_in_flight.is_some(), false)
                    .to_owned(),
                detail: String::new(),
                hint: action_shortcut(action).to_owned(),
                target: palette::Target::Action(action),
                enabled: app.action_enabled(action),
                pane: None,
            }),
    );
    items
}

/// Run a palette entry through the same guarded paths as keys and clicks.
fn activate_palette(
    target: palette::Target,
    app: &mut App,
    agents: &HashMap<String, Agent>,
    fleet: Arc<Fleet>,
    tasks: &mut JoinSet<Work>,
    owned_pane: Option<&str>,
    expected_boot: &str,
) -> Vec<Effect> {
    let language = app.language;
    match target {
        palette::Target::Agent(id) => {
            if !agents.contains_key(&id) || !app.select_id(&id) {
                app.notify(message(
                    language,
                    format!("{id} is no longer listed"),
                    format!("{id}은(는) 더 이상 목록에 없습니다"),
                ));
                return Vec::new();
            }
            match app.effect_for(Action::GoToPane) {
                Some(effect) => vec![effect],
                None => {
                    app.notify(message(
                        language,
                        format!("Selected {id}; its pane cannot be focused now"),
                        format!("{id} 선택. 지금은 창으로 이동할 수 없습니다"),
                    ));
                    Vec::new()
                }
            }
        }
        palette::Target::Pane(pane) => {
            let origin = owned_pane.map(str::to_owned);
            let boot = expected_boot.to_owned();
            tasks.spawn(async move {
                let socket = fleet.local.native.socket.clone();
                let result = fleet
                    .local
                    .native
                    .navigate(
                        &pane.boot,
                        &pane.pane_id,
                        &pane.generation,
                        origin.as_deref(),
                        &socket,
                    )
                    .await
                    .map(|_| {
                        ActionResult::Receipt(message(
                            language,
                            format!("Selected pane {}", pane.pane_id),
                            format!("창 {} 선택 완료", pane.pane_id),
                        ))
                    });
                action_message(boot, None, result)
            });
            Vec::new()
        }
        palette::Target::Action(action) => app.effect_for(action).into_iter().collect(),
    }
}

fn selected_agent<'a>(app: &App, agents: &'a HashMap<String, Agent>) -> Option<&'a Agent> {
    app.selected_id.as_ref().and_then(|id| agents.get(id))
}

#[allow(clippy::too_many_arguments)]
fn dispatch_effect(
    effect: Effect,
    dialog: &mut Option<Dialog>,
    app: &mut App,
    agents: &HashMap<String, Agent>,
    fleet: Arc<Fleet>,
    tasks: &mut JoinSet<Work>,
    owned_pane: Option<&str>,
    expected_boot: &str,
    last_poll_ms: u64,
) {
    let language = app.language;
    match effect {
        Effect::NewAgent => {
            let endpoints = fleet.endpoint_ids().and_then(|ids| {
                ids.into_iter()
                    .map(|id| fleet.endpoint_key(&id).map(|key| (id, key)))
                    .collect::<Result<Vec<_>, _>>()
            });
            match endpoints {
                Ok(endpoints) => {
                    *dialog = Some(Dialog::start(
                        app.language,
                        &endpoints,
                        selected_agent(app, agents),
                    ));
                }
                Err(error) => app.notify(format!("Could not open start dialog: {error}")),
            }
        }
        Effect::RenameAgent => {
            if let Some(agent) = selected_agent(app, agents).filter(|agent| !agent.stale) {
                *dialog = Some(Dialog::rename(agent, app.language));
            }
        }
        Effect::ResumeAgent => {
            if let Some(agent) = selected_agent(app, agents).filter(|agent| !agent.stale) {
                *dialog = Some(Dialog::resume(agent, app.language));
            }
        }
        Effect::PrepareDraft => {
            if let Some(agent) = selected_agent(app, agents).filter(|agent| !agent.stale) {
                *dialog = Some(Dialog::draft(agent, app.language));
            }
        }
        Effect::SendPrompt => {
            if let Some(agent) = selected_agent(app, agents).filter(|agent| !agent.stale) {
                *dialog = Some(Dialog::prompt(agent, app.language));
            }
        }
        Effect::InterruptAgent => {
            if let Some(agent) = selected_agent(app, agents).filter(|agent| !agent.stale) {
                *dialog = Some(Dialog::confirm(
                    app.language,
                    DialogKind::Interrupt(agent.clone()),
                    word(app.language, "Interrupt agent?", "에이전트를 중단할까요?"),
                    word(
                        app.language,
                        "Sends the provider's interrupt key (Esc for Claude and Codex) while its screen shows a turn. Whether the provider stopped is not asserted.",
                        "화면에 turn이 보일 때 provider의 끊기 key(Claude, Codex는 Esc)를 보냅니다. provider가 멈췄는지는 확인하지 않습니다.",
                    ),
                    word(app.language, "Interrupt", "중단"),
                ));
            }
        }
        Effect::CloseAgent => {
            if let Some(agent) = selected_agent(app, agents).filter(|agent| !agent.stale) {
                *dialog = Some(Dialog::confirm(
                    app.language,
                    DialogKind::Close(agent.clone()),
                    word(app.language, "Close agent pane?", "에이전트 창을 닫을까요?"),
                    word(
                        app.language,
                        "This kills the selected native pane and its running process.",
                        "선택한 네이티브 창과 실행 중인 프로세스를 종료합니다.",
                    ),
                    word(app.language, "Close pane", "창 닫기"),
                ));
            }
        }
        Effect::Ack { id, .. } => {
            if let Some(agent) = agents.get(&id).filter(|agent| !agent.stale).cloned() {
                let boot = expected_boot.to_owned();
                let target = TargetIdentity::for_agent(&agent);
                // Read inbox events only as far as this window showed them.
                let through = if app.inbox.view.enabled {
                    Through::Seq(app.inbox.newest_seq_of(&agent.run).unwrap_or(0))
                } else {
                    Through::Before(last_poll_ms)
                };
                tasks.spawn(async move {
                    let result = fleet.acknowledge(&agent, through).await.map(|()| {
                        ActionResult::Receipt(message(
                            language,
                            format!("Marked {id} as seen"),
                            format!("{id} 확인 표시 완료"),
                        ))
                    });
                    action_message(boot, Some(target), result)
                });
            }
        }
        Effect::InboxFocus { pane, run } => {
            // An endpoint's merged event names `endpoint::%pane`.
            let (endpoint, pane) = match pane.split_once("::") {
                Some((endpoint, pane)) => (Some(endpoint.to_owned()), pane.to_owned()),
                None => (None, pane),
            };
            let owner = |agent: &&Agent| match &endpoint {
                Some(endpoint) => agent.endpoint_id == *endpoint && !agent.stale,
                None => {
                    (agent.endpoint_id.is_empty() || agent.endpoint_id == "local") && !agent.stale
                }
            };
            let listed = agents
                .values()
                .filter(owner)
                .find(|agent| agent.pane_id == pane && agent.run == run)
                .cloned();
            let remote = endpoint.is_some();
            let origin = owned_pane.map(str::to_owned);
            let boot = expected_boot.to_owned();
            tasks.spawn(async move {
                let result = async {
                    // An agent the saved view hides is still a local agent.
                    let agent = match listed {
                        Some(agent) => agent,
                        None if remote => {
                            return Err(
                                "target_absent: the endpoint's agent is not listed now".to_owned()
                            );
                        }
                        None => fleet
                            .local_agents()
                            .await?
                            .into_iter()
                            .find(|agent| agent.pane_id == pane && agent.run == run)
                            .ok_or_else(|| {
                                "target_absent: the event's pane is closed or runs another agent now"
                                    .to_owned()
                            })?,
                    };
                    fleet.focus_from(&agent, origin.as_deref()).await?;
                    Ok(ActionResult::Receipt(message(
                        language,
                        format!("Selected pane {}", agent.pane_id),
                        format!("창 {} 선택 완료", agent.pane_id),
                    )))
                }
                .await;
                action_message(boot, None, result)
            });
        }
        Effect::InboxRead { id } => {
            let Some(event) = app
                .inbox
                .view
                .events
                .iter()
                .find(|event| event.id == id)
                .cloned()
            else {
                return;
            };
            // The listed copy of the agent: its guard refuses a revision
            // newer than the one this window showed.
            let listed = agents
                .values()
                .find(|agent| {
                    (agent.endpoint_id.is_empty() || agent.endpoint_id == "local")
                        && agent.pane_id == event.pane
                        && agent.run == event.run
                })
                .cloned();
            let boot = expected_boot.to_owned();
            tasks.spawn(async move {
                let result = async {
                    fleet.inbox_mark_read(vec![id], None).await?;
                    // The agent first: listing it can record a newer event,
                    // which the store read below then sees.
                    let agent =
                        match listed {
                            Some(agent) => Some(agent),
                            None => fleet.local_agents().await?.into_iter().find(|agent| {
                                agent.pane_id == event.pane && agent.run == event.run
                            }),
                        };
                    // The row is seen once its run has no unseen event left
                    // in the store. A full list may hide older ones: then
                    // the row stays as it is.
                    let unseen = fleet.inbox_view(false).await?;
                    let waiting = unseen.events.len() >= 100
                        || unseen.events.iter().any(|other| {
                            other.run == event.run && other.source != "operation" && !other.read
                        });
                    let mut changed = false;
                    if !waiting
                        && let Some(agent) = agent
                        && (agent.state == "blocked" || agent.returned_idle)
                        && !agent.seen
                        && let Err(error) = fleet.acknowledge(&agent, Through::Seq(event.seq)).await
                    {
                        // The row changed since this window showed it: the
                        // event is read, the newer attention stays unseen.
                        if !error.starts_with("identity_mismatch") {
                            return Err(error);
                        }
                        changed = true;
                    }
                    Ok(ActionResult::Receipt(if changed {
                        message(
                            language,
                            "Marked the event read; the agent changed, so its row stays unseen"
                                .to_owned(),
                            "사건을 읽음으로 표시했습니다. agent가 바뀌어 행은 그대로 둡니다"
                                .to_owned(),
                        )
                    } else {
                        message(
                            language,
                            "Marked the event read".to_owned(),
                            "사건을 읽음으로 표시했습니다".to_owned(),
                        )
                    }))
                }
                .await;
                action_message(boot, None, result)
            });
        }
        Effect::InboxReadAll { through } => {
            // Only runs this window showed an event of, up to the fence.
            let shown = app
                .inbox
                .view
                .events
                .iter()
                .filter(|event| event.seq <= through && event.source != "operation")
                .map(|event| event.run.clone())
                .collect::<std::collections::HashSet<_>>();
            let boot = expected_boot.to_owned();
            tasks.spawn(async move {
                let result = async {
                    fleet.inbox_mark_read(Vec::new(), Some(through)).await?;
                    // The agents first: listing them can record newer
                    // events, which the store read below then sees. Their
                    // copies guard the writes, so a later change is refused.
                    let agents = fleet.local_agents().await?;
                    // Runs with an unseen event after the fence keep their
                    // rows unseen, whether or not this window showed it.
                    let waiting = fleet
                        .inbox_view(false)
                        .await?
                        .events
                        .into_iter()
                        .filter(|event| event.source != "operation" && !event.read)
                        .map(|event| event.run)
                        .collect::<std::collections::HashSet<_>>();
                    let unseen = agents
                        .into_iter()
                        .filter(|agent| {
                            (agent.state == "blocked" || agent.returned_idle)
                                && !agent.seen
                                && shown.contains(&agent.run)
                                && !waiting.contains(&agent.run)
                        })
                        .collect::<Vec<_>>();
                    fleet.acknowledge_many(&unseen).await?;
                    Ok(ActionResult::Receipt(message(
                        language,
                        "Marked every listed event read".to_owned(),
                        "보이는 사건을 모두 읽음으로 표시했습니다".to_owned(),
                    )))
                }
                .await;
                action_message(boot, None, result)
            });
        }
        Effect::Navigate { id, .. } => {
            if let Some(agent) = agents.get(&id).filter(|agent| !agent.stale).cloned() {
                let origin = owned_pane.map(str::to_owned);
                let boot = expected_boot.to_owned();
                let target = TargetIdentity::for_agent(&agent);
                tasks.spawn(async move {
                    let result = fleet.focus_from(&agent, origin.as_deref()).await.map(|()| {
                        ActionResult::Receipt(message(
                            language,
                            format!("Selected pane {}", agent.pane_id),
                            format!("창 {} 선택 완료", agent.pane_id),
                        ))
                    });
                    action_message(boot, Some(target), result)
                });
            }
        }
        Effect::Inspect { id, .. } => {
            if let Some(agent) = agents.get(&id).filter(|agent| !agent.stale) {
                let value = serde_json::to_value(agent)
                    .unwrap_or_else(|_| json!({"evidence":agent.evidence}));
                let boot = expected_boot.to_owned();
                let target = TargetIdentity::for_agent(agent);
                tasks.spawn(async move {
                    action_message(boot, Some(target), Ok(ActionResult::Details { id, value }))
                });
            }
        }
        Effect::ReadScreen => {
            if let Some(agent) = selected_agent(app, agents)
                .filter(|agent| !agent.stale)
                .cloned()
            {
                let boot = expected_boot.to_owned();
                let target = TargetIdentity::for_agent(&agent);
                tasks.spawn(async move {
                    let result = fleet.read(&agent, true).await.map(|screen| ActionResult::Details {
                        id: agent.id, value: json!({"evidence":agent.evidence,"screen":{"source":"recent","text":screen}}),
                    });
                    action_message(boot, Some(target), result)
                });
            }
        }
        Effect::Expand => {
            if let Some(pane) = owned_pane.map(str::to_owned) {
                let boot = expected_boot.to_owned();
                tasks.spawn(async move {
                    let result = async {
                        let local_boot = fleet.local.boot().await?;
                        let socket = fleet.local.native.socket.clone();
                        fleet
                            .local
                            .native
                            .toggle_zoom(&local_boot, &pane, &socket)
                            .await?;
                        Ok(ActionResult::Receipt(message(
                            language,
                            "Toggled native sidebar zoom".into(),
                            "네이티브 사이드패널 확대 상태를 전환했습니다".into(),
                        )))
                    }
                    .await;
                    action_message(boot, None, result)
                });
            }
        }
        Effect::Changes { id } => {
            if let Some(agent) = agents.get(&id).filter(|agent| !agent.stale).cloned() {
                app.changes_request += 1;
                changes_task(
                    tasks,
                    fleet,
                    agent,
                    crate::managed::changes::ChangesOp::List,
                    Some(app.changes_request),
                    None,
                );
            }
        }
        Effect::Dictate { id } => {
            if let Some(agent) = agents.get(&id).filter(|agent| !agent.stale).cloned() {
                if !(agent.endpoint_id.is_empty() || agent.endpoint_id == "local") {
                    let boot = expected_boot.to_owned();
                    let target = TargetIdentity::for_agent(&agent);
                    tasks.spawn(async move {
                        action_message(
                            boot,
                            Some(target),
                            Err(
                                "remote_unsupported: dictation goes to agents of this server"
                                    .into(),
                            ),
                        )
                    });
                    return;
                }
                let socket = fleet.local.native.socket.clone();
                let boot = expected_boot.to_owned();
                let target = TargetIdentity::for_agent(&agent);
                tasks.spawn(async move {
                    let result = dictate(&socket, &agent.pane_id).map(|()| {
                        ActionResult::Receipt(message(
                            language,
                            format!("Dictation for {}: started or stopped (m)", agent.name),
                            format!("{} 받아쓰기: 시작 또는 멈춤 (m)", agent.name),
                        ))
                    });
                    action_message(boot, Some(target), result)
                });
            }
        }
        Effect::Queue { id } => {
            if let Some(agent) = agents.get(&id).filter(|agent| !agent.stale).cloned() {
                app.queue_request += 1;
                queue_task(tasks, fleet, agent, None, Some(app.queue_request));
            }
        }
        Effect::Answer { id } => {
            if let Some(agent) = agents.get(&id).filter(|agent| !agent.stale).cloned() {
                let boot = expected_boot.to_owned();
                let target = TargetIdentity::for_agent(&agent);
                app.answer_request += 1;
                let press = app.answer_request;
                tasks.spawn(async move {
                    let result = fleet
                        .requests(&agent)
                        .await
                        .map(|value| ActionResult::Requests {
                            agent: Box::new(agent),
                            value,
                            press,
                            event: None,
                        });
                    action_message(boot, Some(target), result)
                });
            }
        }
        Effect::InboxAnswer { pane, run } => {
            let listed = agents
                .values()
                .find(|agent| {
                    (agent.endpoint_id.is_empty() || agent.endpoint_id == "local")
                        && !agent.stale
                        && agent.pane_id == pane
                        && agent.run == run
                })
                .cloned();
            let boot = expected_boot.to_owned();
            app.answer_request += 1;
            let press = app.answer_request;
            tasks.spawn(async move {
                let result = async {
                    // An agent the saved view hides is still a local agent.
                    let agent = match listed {
                        Some(agent) => agent,
                        None => fleet
                            .local_agents()
                            .await?
                            .into_iter()
                            .find(|agent| agent.pane_id == pane && agent.run == run)
                            .ok_or_else(|| {
                                "target_absent: the event's pane is closed or runs another agent now"
                                    .to_owned()
                            })?,
                    };
                    let value = fleet.requests(&agent).await?;
                    Ok(ActionResult::Requests {
                        agent: Box::new(agent),
                        value,
                        press,
                        event: Some((pane, run)),
                    })
                }
                .await;
                action_message(boot, None, result)
            });
        }
        Effect::Retry => {}
        Effect::CopyId { .. } | Effect::Preferences { .. } | Effect::Quit => {}
    }
}

/// `agent dictate --toggle PANE` apart from the desk: its own session, no
/// terminal, so it ends its dictation however the desk ends; a thread
/// reaps it. It finds the asking client from this pane.
fn dictate(socket: &std::path::Path, pane: &str) -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let mut command = std::process::Command::new(executable);
    command
        .arg("agent")
        .arg("--socket")
        .arg(socket)
        .args(["dictate", "--toggle", pane])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // SAFETY: setsid is async-signal-safe.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().map_err(|error| error.to_string())?;
    std::thread::spawn(move || child.wait());
    Ok(())
}

/// One queue change (when given), then the agent's queue again.
fn changes_task(
    tasks: &mut JoinSet<Work>,
    fleet: Arc<Fleet>,
    agent: Agent,
    op: crate::managed::changes::ChangesOp,
    press: Option<u64>,
    window: Option<u64>,
) {
    tasks.spawn(async move {
        let result = fleet.changes_op(&agent, op.clone()).await;
        Work::Changes(ChangesMessage {
            agent: Box::new(agent),
            window,
            press,
            op,
            result,
        })
    });
}

fn queue_task(
    tasks: &mut JoinSet<Work>,
    fleet: Arc<Fleet>,
    agent: Agent,
    op: Option<crate::managed::queue::QueueOp>,
    press: Option<u64>,
) {
    use crate::managed::queue::QueueOp;
    tasks.spawn(async move {
        let done = match op {
            Some(op) => {
                let kind = match &op {
                    QueueOp::Add { .. } | QueueOp::AddFrom { .. } => "added",
                    QueueOp::Show { .. } => "show",
                    QueueOp::Edit { .. } => "saved",
                    QueueOp::Attach { .. } => "attached",
                    QueueOp::Move { .. } => "moved",
                    QueueOp::Remove { .. } => "removed",
                    QueueOp::Send { .. } => "sent",
                };
                Some((kind, fleet.queue_op(&agent, op).await))
            }
            None => None,
        };
        let view = fleet.queue_view(&agent).await;
        Work::Queue(QueueMessage {
            agent: Box::new(agent),
            press,
            done,
            view,
        })
    });
}

/// What a queue change did, in a line.
fn queue_done(
    language: super::model::Language,
    kind: &str,
    result: &Result<Value, String>,
) -> String {
    let error = match result {
        Ok(value) if kind == "sent" => {
            let stage = value["stage"].as_str().unwrap_or("delivered");
            let key = value["operation_key"].as_str().unwrap_or_default();
            return match stage {
                "delivered" | "native_accepted" | "user_confirmed_delivered" => message(
                    language,
                    format!("Sent: {stage}; provider acceptance shows in its receipt"),
                    format!("보냄: {stage}. provider 수락은 receipt에 보입니다"),
                ),
                _ => message(
                    language,
                    format!(
                        "Outcome unknown; check the pane, then `agent operation resolve {key}`"
                    ),
                    format!(
                        "결과를 모릅니다. 창을 본 뒤 `agent operation resolve {key}`로 정하세요"
                    ),
                ),
            };
        }
        Ok(_) => {
            return match (language, kind) {
                (super::model::Language::Korean, "added") => "넣었습니다".into(),
                (super::model::Language::Korean, "saved") => "저장했습니다".into(),
                (super::model::Language::Korean, "attached") => "경로를 확인해 붙였습니다".into(),
                (super::model::Language::Korean, "moved") => "옮겼습니다".into(),
                (super::model::Language::Korean, "removed") => "지웠습니다".into(),
                (_, "added") => "Queued".into(),
                (_, "saved") => "Saved".into(),
                (_, "attached") => "Path checked and attached".into(),
                (_, "moved") => "Moved".into(),
                (_, "removed") => "Removed".into(),
                _ => String::new(),
            };
        }
        Err(error) => error,
    };
    crate::managed::failure::human(error).to_owned()
}

/// The answer dialog for these requests, or a note when there is nothing
/// masil can answer.
fn open_answers(agent: Agent, value: Value, app: &mut App) -> Option<AnswerDialog> {
    let language = app.language;
    if value["channel"] != crate::managed::answer::CHANNEL {
        let reason = value["reason"].as_str().unwrap_or("no answer channel");
        app.notify(message(
            language,
            format!("{}: {reason}. Press g to go to its pane.", agent.name),
            format!("{}: 창에서 답하세요 (g로 이동). {reason}", agent.name),
        ));
        return None;
    }
    let requests = value["requests"].as_array().cloned().unwrap_or_default();
    if requests.is_empty() {
        app.notify(message(
            language,
            format!("{} has no pending request", agent.name),
            format!("{}에 대기 중인 요청이 없습니다", agent.name),
        ));
        return None;
    }
    Some(AnswerDialog::new(language, agent, requests))
}

fn submit_answer(
    agent: Agent,
    request: String,
    reply: crate::managed::answer::Reply,
    fleet: Arc<Fleet>,
    tasks: &mut JoinSet<Work>,
    language: super::model::Language,
    current_epoch: &str,
) {
    let epoch = current_epoch.to_owned();
    let target = TargetIdentity::for_agent(&agent);
    tasks.spawn(async move {
        let result = fleet.answer(&agent, &request, reply).await.map(|value| {
            let accepted = value["provider_accepted"] == true;
            let also = value["also_answered"].as_array().map_or(0, Vec::len);
            let stage = value["stage"].as_str().unwrap_or("unknown");
            let mut english = if accepted {
                format!("Answered {request}; OpenCode accepted it")
            } else {
                format!("Answered {request}: {stage}")
            };
            let mut korean = if accepted {
                format!("{request}에 답했습니다. OpenCode가 받았습니다")
            } else {
                format!("{request}에 답했습니다: {stage}")
            };
            if also > 0 {
                english.push_str(&format!(
                    "; it also closed {also} other request(s) of the session"
                ));
                korean.push_str(&format!(". 같은 session의 요청 {also}개도 함께 닫혔습니다"));
            }
            ActionResult::Receipt(message(language, english, korean))
        });
        action_message(epoch, Some(target), result)
    });
}

fn submit_dialog(
    dialog: Dialog,
    fleet: Arc<Fleet>,
    tasks: &mut JoinSet<Work>,
    app: &mut App,
    current_epoch: &str,
) {
    let language = app.language;
    let values = dialog
        .fields
        .into_iter()
        .map(|field| field.value)
        .collect::<Vec<_>>();
    match dialog.kind {
        DialogKind::Start {
            default_endpoint,
            endpoint_field,
            endpoint_keys,
        } => {
            if values.len() != 3 + usize::from(endpoint_field) {
                return;
            }
            let offset = usize::from(endpoint_field);
            let endpoint = if endpoint_field {
                values[0].clone()
            } else {
                default_endpoint
            };
            let Some(endpoint_key) = endpoint_keys.get(&endpoint).cloned() else {
                app.notify(message(
                    language,
                    "Select a server that was available when this dialog opened".into(),
                    "이 대화 상자를 열 때 사용 가능했던 서버를 선택하세요".into(),
                ));
                return;
            };
            let name = values[offset].clone();
            let provider = values[offset + 1].clone();
            let mut cwd = PathBuf::from(&values[offset + 2]);
            if endpoint != "local"
                && std::env::current_dir()
                    .is_ok_and(|local| local.to_string_lossy() == values[offset + 2])
            {
                cwd.clear();
            }
            let epoch = current_epoch.to_owned();
            tasks.spawn(async move {
                let result = fleet
                    .start_at(
                        &endpoint,
                        &endpoint_key,
                        &name,
                        &provider,
                        &cwd,
                        &[],
                        None,
                        None,
                    )
                    .await
                    .map(|value| {
                        let pane = value["pane_id"].as_str().unwrap_or("unknown pane");
                        ActionResult::Receipt(message(
                            language,
                            format!("Started {name} in {pane}; provider acceptance is unverified"),
                            format!("{pane}에서 {name} 시작; 제공자 수락 여부는 확인되지 않음"),
                        ))
                    });
                action_message(epoch, None, result)
            });
        }
        DialogKind::Resume(agent) => {
            if agent.session_id.is_none() {
                app.notify(message(
                    language,
                    "No native session reference was reported".into(),
                    "네이티브 세션 참조가 보고되지 않았습니다".into(),
                ));
                return;
            }
            let name = values.first().cloned().unwrap_or_default();
            let epoch = current_epoch.to_owned();
            let target = TargetIdentity::for_agent(&agent);
            tasks.spawn(async move {
                let result = fleet.resume(&agent, &name).await.map(|value| {
                    let pane = value["pane_id"].as_str().unwrap_or("unknown pane");
                    ActionResult::Receipt(message(language, format!("Resume process started in {pane}; native session acceptance is unverified"), format!("{pane}에서 재개 프로세스 시작; 네이티브 세션 수락 여부는 확인되지 않음")))
                });
                action_message(epoch, Some(target), result)
            });
        }
        DialogKind::Rename(agent) => {
            let name = values.first().cloned().unwrap_or_default();
            let epoch = current_epoch.to_owned();
            let target = TargetIdentity::for_agent(&agent);
            tasks.spawn(async move {
                let result = fleet.rename(&agent, &name).await.map(|()| {
                    ActionResult::Receipt(message(
                        language,
                        format!("Renamed agent to {name}"),
                        format!("에이전트 이름을 {name}(으)로 변경했습니다"),
                    ))
                });
                action_message(epoch, Some(target), result)
            });
        }
        DialogKind::Draft(agent) => {
            let text = values.first().cloned().unwrap_or_default();
            let epoch = current_epoch.to_owned();
            let target = TargetIdentity::for_agent(&agent);
            tasks.spawn(async move {
                let result = fleet.draft(&agent, &text).await.map(|_| ActionResult::Receipt(message(language, "Draft prepared in tmux buffer masil-agent-draft; it was not pasted or submitted".into(), "tmux 버퍼 masil-agent-draft에 초안을 준비했습니다. 붙여넣거나 전송하지 않았습니다".into())));
                action_message(epoch, Some(target), result)
            });
        }
        DialogKind::Prompt(agent) => {
            let text = values.first().cloned().unwrap_or_default();
            let epoch = current_epoch.to_owned();
            let target = TargetIdentity::for_agent(&agent);
            app.start_prompt();
            tasks.spawn(async move {
                let result = fleet.prompt(&agent, &text).await.map(|_| {
                    ActionResult::Receipt(message(
                        language,
                        "Prompt and Enter delivered; provider acceptance is unverified".into(),
                        "프롬프트와 Enter를 전달했습니다. 제공자 수락 여부는 확인되지 않습니다"
                            .into(),
                    ))
                });
                action_message(epoch, Some(target), result)
            });
        }
        DialogKind::Interrupt(agent) => {
            let epoch = current_epoch.to_owned();
            let target = TargetIdentity::for_agent(&agent);
            tasks.spawn(async move {
                let result = fleet.interrupt(&agent).await.map(|_| {
                    ActionResult::Receipt(message(
                        language,
                        "The interrupt key reached the selected pane; whether the provider stopped is unverified"
                            .into(),
                        "선택한 창에 끊기 key를 보냈습니다. 제공자가 멈췄는지는 확인되지 않습니다"
                            .into(),
                    ))
                });
                action_message(epoch, Some(target), result)
            });
        }
        DialogKind::Close(agent) => {
            let epoch = current_epoch.to_owned();
            let target = TargetIdentity::for_agent(&agent);
            tasks.spawn(async move {
                let result = fleet.close(&agent).await.map(|()| {
                    ActionResult::Receipt(message(
                        language,
                        format!("Closed pane {}", agent.pane_id),
                        format!("창 {} 닫기 완료", agent.pane_id),
                    ))
                });
                action_message(epoch, Some(target), result)
            });
        }
    }
}
