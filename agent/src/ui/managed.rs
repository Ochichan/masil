//! Native agent-management desk. The native server is queried only while this view is open.

use super::{
    model::{App, Effect, Language, Theme},
    preferences, terminal,
    view::{cell_width, ellipsize, wrap_rows},
};
use crate::managed::{
    Agent, Manager,
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
                "Updates rmux metadata for this pane.",
                "이 창의 rmux 메타데이터를 변경합니다.",
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

fn dialog_colors(theme: Theme) -> (Color, Color, Color, Color, Color) {
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
    Details { id: String, value: Value },
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
                        if let Some(active) = &mut dialog {
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
                        } else if matches!(&event, Event::Key(key) if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)) {
                            if let Some(agent) = selected_agent(&app, &agents).filter(|agent| !agent.stale) {
                                dialog = Some(Dialog::confirm(app.language, DialogKind::Interrupt(agent.clone()), word(app.language, "Interrupt agent?", "에이전트를 중단할까요?"), word(app.language, "Delivers C-c to the verified foreground process. Provider handling is not asserted.", "검증된 포그라운드 프로세스에 C-c를 전달합니다. 제공자의 처리 여부는 확인되지 않습니다."), word(app.language, "Interrupt", "중단")));
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
                                    let next_endpoint_line = endpoint_status_line(&endpoints, app.language);
                                    if app.endpoint_line.as_ref() != Some(&next_endpoint_line) {
                                        app.endpoint_line = Some(next_endpoint_line);
                                        app.dirty = true;
                                    }
                                    if identity_changed {
                                        dialog = None;
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
                        Ok(Work::Action(message)) if message.target.as_ref().is_some_and(|target| target.same_run(&agents)) || (message.target.is_none() && message.boot == current_epoch) => match message.result {
                            Ok(ActionResult::Receipt(receipt)) => { app.finish_action(); app.notify(receipt); },
                            Ok(ActionResult::Details { id, value }) => app.set_details(&id, value),
                            Err(error) => { app.finish_action(); app.notify(format!("Action failed: {error}")); },
                        },
                        Ok(Work::Action(_)) => {},
                        Err(error) => { poll_pending = false; app.finish_action(); app.notify(format!("Action failed: {error}")); }
                    }
                    Vec::new()
                },
                _ = terminate.recv() => break,
                _ = interrupt.recv() => break,
                _ = hangup.recv() => break,
            };

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
                dispatch_effect(effect, &mut dialog, &mut app, &agents, fleet.clone(), &mut tasks, owned_pane.as_deref(), &current_epoch);
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
                        "Delivers C-c to the verified foreground process. Provider handling is not asserted.",
                        "검증된 포그라운드 프로세스에 C-c를 전달합니다. 제공자의 처리 여부는 확인되지 않습니다.",
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
                tasks.spawn(async move {
                    let result = fleet.acknowledge(&agent).await.map(|()| {
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
        Effect::Retry => {}
        Effect::CopyId { .. } | Effect::Preferences { .. } | Effect::Quit => {}
    }
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
                let result = fleet.draft(&agent, &text).await.map(|_| ActionResult::Receipt(message(language, "Draft prepared in tmux buffer rmux-agent-draft; it was not pasted or submitted".into(), "tmux 버퍼 rmux-agent-draft에 초안을 준비했습니다. 붙여넣거나 전송하지 않았습니다".into())));
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
                let result = fleet.keys(&agent, &["C-c".into()]).await.map(|_| {
                    ActionResult::Receipt(message(
                        language,
                        "C-c delivered to the selected pane; provider handling is unverified"
                            .into(),
                        "선택한 창에 C-c를 전달했습니다. 제공자의 처리 여부는 확인되지 않습니다"
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
