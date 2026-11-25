//! Native agent-management desk. The native server is queried only while this view is open.

use super::{
    answer::{AnswerDialog, Outcome},
    i18n::action_label,
    model::{Action, App, Effect, Language, Observation, Overlay, Theme},
    palette::{self, Palette, PaletteResult},
    plugin_view::{PluginViewOutcome, PluginViewOverlay},
    preferences,
    queue::{QueueOutcome, QueueWindow},
    terminal,
    view::{cell_width, ellipsize, menu_shortcut, wrap_rows},
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
    CreatePluginWithAi,
}

impl DialogKind {
    /// The agent a dialog acts on. The start dialog has none.
    fn agent(&self) -> Option<&Agent> {
        match self {
            Self::Start { .. } | Self::CreatePluginWithAi => None,
            Self::Resume(agent)
            | Self::Rename(agent)
            | Self::Draft(agent)
            | Self::Prompt(agent)
            | Self::Interrupt(agent)
            | Self::Close(agent) => Some(agent),
        }
    }
}

struct Field {
    label: &'static str,
    value: String,
    limit: usize,
    /// Grapheme index of the editing cursor within `value`.
    cursor: usize,
    /// First visible grapheme index when horizontally windowed.
    view_start: usize,
}

impl Field {
    fn new(label: &'static str, value: String, limit: usize) -> Self {
        let cursor = value.graphemes(true).count();
        Self {
            label,
            value,
            limit,
            cursor,
            view_start: 0,
        }
    }

    fn grapheme_count(&self) -> usize {
        self.value.graphemes(true).count()
    }

    fn clamp_cursor(&mut self) {
        self.cursor = self.cursor.min(self.grapheme_count());
    }

    fn byte_offset(&self, grapheme_index: usize) -> usize {
        self.value
            .grapheme_indices(true)
            .nth(grapheme_index)
            .map_or(self.value.len(), |(offset, _)| offset)
    }

    fn move_cursor_to_column(&mut self, relative_col: usize) {
        let mut cells = 0usize;
        let mut index = self.view_start;
        for grapheme in self.value.graphemes(true).skip(self.view_start) {
            let width = cell_width(grapheme);
            if cells + width > relative_col {
                break;
            }
            cells += width;
            index += 1;
        }
        self.cursor = index.min(self.grapheme_count());
    }
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
    title: String,
    note: String,
    /// What the last input or submit did not take. Drawn above the note.
    notice: Option<String>,
    submit: &'static str,
    fields: Vec<Field>,
    focus: usize,
    hits: Vec<(Rect, DialogHit)>,
    hover: Option<DialogHit>,
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
            fields.push(Field::new(
                word(language, "Server", "서버"),
                default_endpoint.clone(),
                64,
            ));
        }
        let local_cwd = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .to_string_lossy()
            .into_owned();
        fields.extend([
            Field::new(word(language, "Name", "이름"), String::new(), 32),
            Field::new(
                word(language, "Provider", "제공자"),
                crate::providers::all()
                    .first()
                    .map_or("codex", |p| p.id)
                    .into(),
                64,
            ),
            Field::new(
                word(language, "Working directory", "작업 디렉터리"),
                if default_endpoint == "local" {
                    local_cwd
                } else {
                    String::new()
                },
                4096,
            ),
        ]);
        Self {
            language,
            kind: DialogKind::Start {
                default_endpoint,
                endpoint_field,
                endpoint_keys: endpoints.iter().cloned().collect(),
            },
            title: word(language, "New native agent", "새 네이티브 에이전트").to_owned(),
            note: format!(
                "{} {}",
                word(
                    language,
                    "Starts a new pane. Idle is not treated as task completion.",
                    "새 pane을 시작합니다. 대기 상태를 작업 완료로 취급하지 않습니다.",
                ),
                name_rule(language)
            ),
            notice: None,
            submit: word(language, "Start", "시작"),
            fields,
            focus: 0,
            hits: Vec::new(),
            hover: None,
            pressed: None,
        }
    }

    fn resume(agent: &Agent, language: Language) -> Self {
        Self {
            language,
            kind: DialogKind::Resume(agent.clone()),
            title: word(language, "Resume native session", "네이티브 세션 재개").to_owned(),
            note: format!(
                "{} {}",
                word(
                    language,
                    "Creates a new pane from the reported session reference; acceptance remains unverified.",
                    "보고된 세션 참조로 새 pane을 만듭니다. 세션 수락 여부는 확인되지 않습니다.",
                ),
                name_rule(language)
            ),
            notice: None,
            submit: word(language, "Resume", "재개"),
            fields: vec![Field::new(
                word(language, "New name", "새 이름"),
                format!("{}-resume", agent.name.chars().take(25).collect::<String>()),
                32,
            )],
            focus: 0,
            hits: Vec::new(),
            hover: None,
            pressed: None,
        }
    }

    fn rename(agent: &Agent, language: Language) -> Self {
        Self {
            language,
            kind: DialogKind::Rename(agent.clone()),
            title: message(
                language,
                format!("Rename {}", subject(agent)),
                format!("이름 변경: {}", subject(agent)),
            ),
            note: format!(
                "{} {}",
                word(
                    language,
                    "Updates masil metadata for this pane.",
                    "이 pane의 masil 메타데이터를 변경합니다.",
                ),
                name_rule(language)
            ),
            notice: None,
            submit: word(language, "Rename", "이름 변경"),
            fields: vec![Field::new(
                word(language, "Name", "이름"),
                agent.name.clone(),
                32,
            )],
            focus: 0,
            hits: Vec::new(),
            hover: None,
            pressed: None,
        }
    }

    fn draft(agent: &Agent, language: Language) -> Self {
        Self {
            language,
            kind: DialogKind::Draft(agent.clone()),
            title: word(language, "Prepare draft", "초안 준비").to_owned(),
            note: word(
                language,
                "Copies text to a tmux buffer only. It does not paste or submit it.",
                "텍스트를 tmux 버퍼에만 복사합니다. 붙여넣거나 전송하지 않습니다.",
            )
            .to_owned(),
            notice: None,
            submit: word(language, "Prepare", "준비"),
            fields: vec![Field::new(
                word(language, "Draft text", "초안 텍스트"),
                String::new(),
                32_768,
            )],
            focus: 0,
            hits: Vec::new(),
            hover: None,
            pressed: None,
        }
    }

    fn prompt(agent: &Agent, language: Language) -> Self {
        Self {
            language,
            kind: DialogKind::Prompt(agent.clone()),
            title: message(
                language,
                format!("Send prompt to {}", subject(agent)),
                format!("프롬프트 전송: {}", subject(agent)),
            ),
            note: word(
                language,
                "Sends this text with Enter to the verified idle agent. Delivery does not prove provider acceptance or task success.",
                "검증된 대기 에이전트에 이 텍스트와 Enter를 전달합니다. 전달은 제공자 수락이나 작업 성공을 뜻하지 않습니다.",
            )
            .to_owned(),
            notice: None,
            submit: word(language, "Send", "전송"),
            fields: vec![Field::new(
                word(language, "Prompt text", "프롬프트 텍스트"),
                String::new(),
                32_768,
            )],
            focus: 0,
            hits: Vec::new(),
            hover: None,
            pressed: None,
        }
    }

    /// Confirms sending the provider's interrupt key to this agent.
    fn interrupt(agent: &Agent, language: Language) -> Self {
        Self::confirm(
            language,
            DialogKind::Interrupt(agent.clone()),
            message(
                language,
                format!("Interrupt {}?", subject(agent)),
                format!("{} 중단할까요?", subject(agent)),
            ),
            word(
                language,
                "Sends the provider's interrupt key (Esc for Claude and Codex) while its screen shows a turn. Whether the provider stopped is not asserted.",
                "화면에 turn이 보일 때 provider의 끊기 key(Claude, Codex는 Esc)를 보냅니다. provider가 멈췄는지는 확인하지 않습니다.",
            ),
            word(language, "Interrupt", "중단"),
        )
    }

    /// Confirms killing this agent's pane.
    fn close(agent: &Agent, language: Language) -> Self {
        Self::confirm(
            language,
            DialogKind::Close(agent.clone()),
            message(
                language,
                format!("Close the pane of {}?", subject(agent)),
                format!("{} pane을 닫을까요?", subject(agent)),
            ),
            word(
                language,
                "This kills the selected native pane and its running process.",
                "선택한 네이티브 pane과 실행 중인 프로세스를 종료합니다.",
            ),
            word(language, "Close pane", "pane 닫기"),
        )
    }

    fn create_plugin_with_ai(language: Language) -> Self {
        Self {
            language,
            kind: DialogKind::CreatePluginWithAi,
            title: word(
                language,
                "Create Masil plugin with AI",
                "AI로 Masil 플러그인 만들기",
            )
            .to_owned(),
            note: word(
                language,
                "Launches agy, claude, or codex with the official masil-plugin skill to build and hot-reload your plugin.",
                "공식 masil-plugin 스킬과 함께 agy, claude, codex 세션을 실행해 플러그인을 만들고 실시간 반영합니다.",
            )
            .to_owned(),
            notice: None,
            submit: word(language, "Launch AI creator", "AI 제작 시작"),
            fields: vec![
                Field::new(
                    word(language, "Provider (agy/claude/codex)", "도구 (agy/claude/codex)"),
                    "agy".into(),
                    32,
                ),
                Field::new(
                    word(language, "Plugin goal", "만들 플러그인 설명"),
                    String::new(),
                    512,
                ),
            ],
            focus: 1,
            hits: Vec::new(),
            hover: None,
            pressed: None,
        }
    }

    fn confirm(
        language: Language,
        kind: DialogKind,
        title: String,
        note: &'static str,
        submit: &'static str,
    ) -> Self {
        Self {
            language,
            kind,
            title,
            note: note.to_owned(),
            notice: None,
            submit,
            fields: Vec::new(),
            focus: 0,
            hits: Vec::new(),
            hover: None,
            pressed: None,
        }
    }

    fn hover(&self) -> Option<DialogHit> {
        self.hover
    }

    /// The field and the reason this dialog cannot be submitted as it
    /// stands. Checked while the dialog is open, so typed text is kept.
    fn refusal(&self) -> Option<(usize, String)> {
        let value = |index: usize| {
            self.fields
                .get(index)
                .map_or("", |field| field.value.as_str())
        };
        let name = |index: usize| {
            (!name_allowed(value(index))).then(|| (index, name_rule(self.language).to_owned()))
        };
        match &self.kind {
            DialogKind::Start {
                default_endpoint,
                endpoint_field,
                endpoint_keys,
            } => {
                let endpoint = if *endpoint_field {
                    value(0)
                } else {
                    default_endpoint.as_str()
                };
                if !endpoint_keys.contains_key(endpoint) {
                    let mut known = endpoint_keys.keys().map(String::as_str).collect::<Vec<_>>();
                    known.sort_unstable();
                    let known = known.join(", ");
                    return Some((
                        0,
                        message(
                            self.language,
                            format!(
                                "Select a server that was available when this dialog opened: {known}"
                            ),
                            format!(
                                "이 대화 상자를 열 때 사용 가능했던 서버를 선택하세요: {known}"
                            ),
                        ),
                    ));
                }
                name(usize::from(*endpoint_field))
            }
            DialogKind::Resume(_) | DialogKind::Rename(_) => name(0),
            DialogKind::CreatePluginWithAi => {
                let provider = value(0).trim().to_ascii_lowercase();
                if !matches!(
                    provider.as_str(),
                    "agy" | "antigravity" | "claude" | "codex"
                ) {
                    return Some((
                        0,
                        word(
                            self.language,
                            "Choose agy, claude, or codex",
                            "agy, claude, codex 중 하나를 입력하세요",
                        )
                        .to_owned(),
                    ));
                }
                if value(1).trim().is_empty() {
                    return Some((
                        1,
                        word(
                            self.language,
                            "Describe what the plugin should do",
                            "플러그인 기능을 입력하세요",
                        )
                        .to_owned(),
                    ));
                }
                None
            }
            _ => None,
        }
    }

    fn handle(&mut self, event: Event) -> DialogResult {
        // A notice is about the last key or paste only.
        if matches!(event, Event::Key(_) | Event::Paste(_)) {
            self.notice = None;
        }
        match event {
            Event::Key(key) => self.key(key),
            Event::Paste(text) => {
                self.insert(&text);
                DialogResult::None
            }
            Event::Mouse(mouse) => {
                let hit = self
                    .hits
                    .iter()
                    .rev()
                    .find(|(rect, _)| contains(*rect, mouse.column, mouse.row))
                    .copied();
                let target = hit.map(|(_, hit)| hit);
                match mouse.kind {
                    MouseEventKind::Moved | MouseEventKind::Drag(_) => self.hover = target,
                    MouseEventKind::Down(MouseButton::Left) => self.pressed = target,
                    MouseEventKind::Up(MouseButton::Left) => {
                        let pressed = self.pressed.take();
                        if pressed == target {
                            match hit {
                                Some((rect, DialogHit::Field(index))) => {
                                    self.focus = index;
                                    if let Some(field) = self.fields.get_mut(index) {
                                        let rel = mouse.column.saturating_sub(rect.x) as usize;
                                        field.move_cursor_to_column(rel);
                                    }
                                }
                                Some((_, DialogHit::Cancel)) => return DialogResult::Cancel,
                                Some((_, DialogHit::Submit)) => return DialogResult::Submit,
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
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
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
            KeyCode::Left => {
                if let Some(field) = self.fields.get_mut(self.focus) {
                    field.clamp_cursor();
                    field.cursor = field.cursor.saturating_sub(1);
                }
                DialogResult::None
            }
            KeyCode::Right => {
                if let Some(field) = self.fields.get_mut(self.focus) {
                    field.cursor = (field.cursor + 1).min(field.grapheme_count());
                }
                DialogResult::None
            }
            KeyCode::Home => {
                if let Some(field) = self.fields.get_mut(self.focus) {
                    field.cursor = 0;
                }
                DialogResult::None
            }
            KeyCode::End => {
                if let Some(field) = self.fields.get_mut(self.focus) {
                    field.cursor = field.grapheme_count();
                }
                DialogResult::None
            }
            KeyCode::Char('a') if control => {
                if let Some(field) = self.fields.get_mut(self.focus) {
                    field.cursor = 0;
                }
                DialogResult::None
            }
            KeyCode::Char('e') if control => {
                if let Some(field) = self.fields.get_mut(self.focus) {
                    field.cursor = field.grapheme_count();
                }
                DialogResult::None
            }
            KeyCode::Backspace => {
                if let Some(field) = self.fields.get_mut(self.focus) {
                    field.clamp_cursor();
                    if field.cursor > 0 {
                        let start = field.byte_offset(field.cursor - 1);
                        let end = field.byte_offset(field.cursor);
                        field.value.replace_range(start..end, "");
                        field.cursor -= 1;
                    }
                }
                DialogResult::None
            }
            KeyCode::Delete => {
                if let Some(field) = self.fields.get_mut(self.focus) {
                    field.clamp_cursor();
                    if field.cursor < field.grapheme_count() {
                        let start = field.byte_offset(field.cursor);
                        let end = field.byte_offset(field.cursor + 1);
                        field.value.replace_range(start..end, "");
                    }
                }
                DialogResult::None
            }
            KeyCode::Char('w') if control => {
                if let Some(field) = self.fields.get_mut(self.focus) {
                    field.clamp_cursor();
                    if field.cursor > 0 {
                        let graphemes: Vec<&str> = field.value.graphemes(true).collect();
                        let mut target = field.cursor;
                        while target > 0 && graphemes[target - 1].chars().all(char::is_whitespace) {
                            target -= 1;
                        }
                        while target > 0 && !graphemes[target - 1].chars().all(char::is_whitespace)
                        {
                            target -= 1;
                        }
                        let start = field.byte_offset(target);
                        let end = field.byte_offset(field.cursor);
                        field.value.replace_range(start..end, "");
                        field.cursor = target;
                    }
                }
                DialogResult::None
            }
            KeyCode::Char('u') if control => {
                if let Some(field) = self.fields.get_mut(self.focus) {
                    field.value.clear();
                    field.cursor = 0;
                    field.view_start = 0;
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
        field.clamp_cursor();
        // A field is one line: a line break or tab between words becomes
        // one space, so pasted lines do not run together. One at either end
        // of the text is dropped, so a copied path or name gains no space.
        let text = text.trim_matches(['\r', '\n', '\t']);
        let mut cut = false;
        let mut clean = String::new();
        let mut characters = text.chars().peekable();
        while let Some(character) = characters.next() {
            let character = match character {
                '\r' => {
                    characters.next_if_eq(&'\n');
                    ' '
                }
                '\n' | '\t' => ' ',
                other if other.is_control() => continue,
                other => other,
            };
            if field.value.len() + clean.len() + character.len_utf8() > field.limit {
                cut = true;
                break;
            }
            clean.push(character);
        }
        if !clean.is_empty() {
            let at = field.byte_offset(field.cursor);
            let added = clean.graphemes(true).count();
            field.value.insert_str(at, &clean);
            field.cursor += added;
        }
        if cut {
            let limit = field.limit;
            self.notice = Some(message(
                self.language,
                format!("This field holds {limit} bytes; the rest was not inserted."),
                format!("이 칸은 {limit}바이트까지입니다. 나머지는 넣지 않았습니다."),
            ));
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
        // The notice comes first, so a short screen cuts the note instead.
        let rows = space.saturating_sub(shown * 2) as usize;
        let notice = self
            .notice
            .as_deref()
            .map(|notice| wrap_rows(notice, text_width, rows))
            .unwrap_or_default();
        let note = wrap_rows(&self.note, text_width, rows - notice.len());
        let height = (shown * 2 + (notice.len() + note.len()) as u16 + 4).min(room);
        let modal = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        );
        let (panel, text, _, accent, accent_text) = dialog_colors(theme);
        let palette = super::view::Palette::for_theme(theme);
        let muted = palette.muted_style();
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
        let focus = self.focus;
        for (index, field) in self
            .fields
            .iter_mut()
            .enumerate()
            .skip(first)
            .take(shown as usize)
        {
            frame.render_widget(
                Paragraph::new(ellipsize(field.label, text_width)).style(muted),
                Rect::new(inner.x, y, inner.width, 1),
            );
            let selected = focus == index;
            field.clamp_cursor();
            let rect = Rect::new(inner.x, y + 1, inner.width, 1);
            let value = if field.value.is_empty() {
                field.view_start = 0;
                if selected && rect.width > 0 {
                    frame.set_cursor_position((rect.x, rect.y));
                }
                String::new()
            } else if selected {
                let count = field.grapheme_count();
                if field.cursor == count {
                    let shown = tail(&field.value, text_width.saturating_sub(1));
                    let shown_graphemes = shown.graphemes(true).count();
                    let hidden_ellipsis =
                        usize::from(shown.starts_with('…') && !field.value.starts_with('…'));
                    field.view_start =
                        count.saturating_sub(shown_graphemes.saturating_sub(hidden_ellipsis));
                    if rect.width > 0 {
                        let column = (cell_width(&shown) as u16).min(rect.width - 1);
                        frame.set_cursor_position((rect.x + column, rect.y));
                    }
                    shown
                } else {
                    let (shown, cursor_x, start) =
                        super::view::search_window(&field.value, field.cursor, text_width);
                    field.view_start = start;
                    if rect.width > 0 {
                        let column = (cursor_x as u16).min(rect.width - 1);
                        frame.set_cursor_position((rect.x + column, rect.y));
                    }
                    shown
                }
            } else {
                field.view_start = 0;
                tail(&field.value, text_width)
            };
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
            let warning = Style::default().fg(palette.warning);
            frame.render_widget(
                Paragraph::new(
                    notice
                        .into_iter()
                        .map(|line| Line::styled(line, warning))
                        .chain(note.into_iter().map(|line| Line::styled(line, muted)))
                        .take(note_room as usize)
                        .collect::<Vec<_>>(),
                ),
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
        let submit_style = if self.pressed == Some(DialogHit::Submit) {
            Style::default()
                .bg(palette.warning)
                .fg(accent_text)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .bg(accent)
                .fg(accent_text)
                .add_modifier(Modifier::BOLD)
        };
        frame.render_widget(Paragraph::new(submit_label).style(submit_style), submit);
        if cancel.width > 0 {
            let (cancel_style, raised) = if self.pressed == Some(DialogHit::Cancel) {
                (
                    Style::default()
                        .bg(palette.warning)
                        .fg(accent_text)
                        .add_modifier(Modifier::BOLD),
                    false,
                )
            } else if self.hover == Some(DialogHit::Cancel) {
                (
                    Style::default()
                        .bg(accent)
                        .fg(accent_text)
                        .add_modifier(Modifier::BOLD),
                    false,
                )
            } else {
                (Style::default().fg(text).bg(palette.raised_fill()), true)
            };
            frame.render_widget(Paragraph::new(cancel_label).style(cancel_style), cancel);
            if raised {
                palette.raise(frame, cancel);
            }
            self.hits.push((cancel, DialogHit::Cancel));
        }
        self.hits.push((submit, DialogHit::Submit));
    }
}

/// The end of `text` within `width` cells, marked with a leading ellipsis
/// when the start is hidden.
pub(super) fn tail(text: &str, width: usize) -> String {
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

/// The agent a dialog title names: its listed name and its pane.
fn subject(agent: &Agent) -> String {
    format!(
        "{} ({})",
        crate::managed::find::agent_label(agent),
        agent.pane_id
    )
}

/// The rule `managed::valid_name` applies when an agent is started, resumed
/// or renamed. That function is private to its module, so the desk repeats
/// the rule to refuse a name while its dialog is still open.
fn name_allowed(name: &str) -> bool {
    name.len() <= 32
        && name.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
}

fn name_rule(language: Language) -> &'static str {
    word(
        language,
        "A name begins with a lowercase letter and uses 1 to 32 lowercase letters, digits, '-' or '_'.",
        "이름은 영문 소문자로 시작하고 영문 소문자, 숫자, '-', '_'를 1~32자까지 씁니다.",
    )
}

/// An error for the footer. An unknown outcome is not a failure that is
/// safe to send again (CONTEXT.md), so it keeps its own wording, and the
/// `operation resolve` command comes first where the text names one.
fn action_error(language: Language, error: &str) -> String {
    let text = crate::managed::failure::human(error);
    if !outcome_is_unknown(error) {
        return message(
            language,
            format!("Action failed: {text}"),
            format!("작업 실패: {text}"),
        );
    }
    match operation_key(text) {
        Some(key) => format!("{} {text}", unknown_outcome(language, key)),
        None => message(
            language,
            format!("Outcome unknown: {text}"),
            format!("결과를 모릅니다: {text}"),
        ),
    }
}

/// Whether an error's code is in the unknown-outcome class, which holds
/// more codes than `outcome_unknown` (`interrupt_partial`, `api_timeout`).
fn outcome_is_unknown(error: &str) -> bool {
    use crate::managed::failure::{Class, classify};
    classify(error).0 == Class::Unknown
}

/// What to do about an operation whose outcome is unknown.
fn unknown_outcome(language: Language, key: &str) -> String {
    message(
        language,
        format!("Outcome unknown: check the pane, then `agent operation resolve {key}`."),
        format!("결과를 모릅니다: pane을 본 뒤 `agent operation resolve {key}`로 정하세요."),
    )
}

/// The operation key in an error that names its `operation resolve` command.
fn operation_key(text: &str) -> Option<&str> {
    let (_, rest) = text.split_once("operation resolve ")?;
    let key = rest
        .split(|character: char| character.is_whitespace() || character == '`')
        .next()?;
    (!key.is_empty()).then_some(key)
}

/// A receipt or error for a run that is no longer listed names its agent.
fn for_unlisted(language: Language, name: Option<&str>, text: String) -> String {
    match name {
        Some(name) => message(
            language,
            format!("{name} (that run is no longer listed): {text}"),
            format!("{name} (그 실행은 목록에 없음): {text}"),
        ),
        None => text,
    }
}

/// The receipt for a prompt, worded for the stage its operation reached.
fn prompt_receipt(language: Language, value: &Value) -> String {
    let key = value["operation_key"].as_str().unwrap_or_default();
    match value["stage"].as_str().unwrap_or_default() {
        "native_accepted" => message(
            language,
            "Prompt and Enter delivered; the provider reported that it accepted the prompt".into(),
            "프롬프트와 Enter를 전달했습니다. 제공자가 프롬프트를 수락했다고 보고했습니다".into(),
        ),
        "delivered" | "user_confirmed_delivered" => message(
            language,
            "Prompt and Enter delivered; provider acceptance is unverified".into(),
            "프롬프트와 Enter를 전달했습니다. 제공자 수락 여부는 확인되지 않습니다".into(),
        ),
        "not_delivered" => message(
            language,
            format!("Prompt not delivered: operation {key} is recorded as not delivered"),
            format!(
                "프롬프트를 전달하지 않았습니다: 작업 {key}는 전달되지 않은 것으로 기록되어 있습니다"
            ),
        ),
        _ => unknown_outcome(language, key),
    }
}

/// Panel, text, muted, accent and accent text. The terminal theme's muted
/// entry is a fixed grey; `view::Palette::muted_style` dims the host's own
/// foreground instead and stays readable on a light host.
pub(super) fn dialog_colors(theme: Theme) -> (Color, Color, Color, Color, Color) {
    let p = super::view::Palette::for_theme(theme);
    let panel = if p.host { Color::Reset } else { p.raised };
    (panel, p.text, p.muted, p.accent, p.accent_text)
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
    /// A plugin panel render completed.
    PluginPanel(Result<crate::plugin::ui_spec::UiDocument, String>),
    /// A plugin slash command completed.
    PluginCommand {
        plugin_id: String,
        title: String,
        agent: Option<Value>,
        result: Result<crate::plugin::runner::CommandOutcome, String>,
    },
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
    /// The agent's listed name, for a result that arrives after it left.
    name: String,
    /// Whether the list held the agent when the action began. An agent the
    /// saved view hides was never listed, so its absence says nothing.
    listed: bool,
    /// The Mark seen press this action answers, as `App::ack_in_flight`
    /// holds it.
    ack: Option<(String, String)>,
    /// The prompt this action delivers, as `App::start_prompt` counted it.
    prompt: Option<u64>,
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
            name: crate::managed::find::agent_label(agent),
            listed: true,
            ack: None,
            prompt: None,
        }
    }

    /// Ends the action this result belongs to. A result for a run that is no
    /// longer current clears only the flag its own action set, so it cannot
    /// end a newer action that is still in flight.
    fn end_action(target: Option<&Self>, current: bool, app: &mut App) {
        if current {
            app.finish_action();
            return;
        }
        let Some(target) = target else { return };
        if target.ack.is_some() && app.ack_in_flight == target.ack {
            app.ack_in_flight = None;
        }
        if app.prompt_in_flight && target.prompt == Some(app.prompt_serial) {
            app.prompt_in_flight = false;
        }
        app.dirty = true;
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

/// Whether the list still holds this run of the agent, as fresh as it was.
fn listed(agent: &Agent, agents: &HashMap<String, Agent>) -> bool {
    let identity = agent_identity(agent);
    agents
        .values()
        .any(|other| other.pane_id == agent.pane_id && agent_identity(other) == identity)
}

/// Whether the agent of an open dialog is gone. For an agent that was never
/// listed, only another run at its pane shows that it left.
fn left(agent: &Agent, was_listed: bool, agents: &HashMap<String, Agent>) -> bool {
    if was_listed {
        return !listed(agent, agents);
    }
    let identity = agent_identity(agent);
    agents.values().any(|other| {
        other.endpoint_key == agent.endpoint_key
            && other.pane_id == agent.pane_id
            && agent_identity(other) != identity
    })
}

/// Whether ui.json exists where `Preferences::load` looks for it.
fn preferences_file_exists() -> bool {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|path| PathBuf::from(path).join(".config")))
        .filter(|path| path.is_absolute())
        .is_some_and(|path| path.join("masil/ui.json").exists())
}

/// A global option of the server, or None when it is unset or unreadable.
async fn server_option(manager: &Manager, option: &str) -> Option<String> {
    let output = manager
        .native
        .tmux(["show-options", "-gqv", option].map(Into::into), None)
        .await
        .ok()?;
    let value = String::from_utf8(output.stdout).ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

/// The theme for a value of `@masil-theme`. Its `tmux` choice keeps the
/// server's own colours, which the terminal theme follows.
fn server_theme(value: &str) -> Option<Theme> {
    match value {
        "tmux" => Some(Theme::Terminal),
        other => other.parse().ok(),
    }
}

/// Language and theme for a new desk: a flag first, then ui.json, then the
/// `@masil-lang` and `@masil-theme` the settings screen sets on the server,
/// then the defaults.
async fn initial_look(
    language: Option<Language>,
    theme: Option<Theme>,
    preferences: &preferences::Preferences,
    manager: &Manager,
) -> (Language, Theme) {
    let stored = preferences.warning.is_none() && preferences_file_exists();
    if stored || (language.is_some() && theme.is_some()) {
        return (
            language.unwrap_or(preferences.language),
            theme.unwrap_or(preferences.theme),
        );
    }
    let (server_language, server_look) = tokio::join!(
        server_option(manager, "@masil-lang"),
        server_option(manager, "@masil-theme")
    );
    (
        language
            .or_else(|| server_language.and_then(|value| value.parse().ok()))
            .unwrap_or(preferences.language),
        theme
            .or_else(|| server_look.as_deref().and_then(server_theme))
            .unwrap_or(preferences.theme),
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
    let manager = Manager::new(socket.clone(), options.client)?;
    let (language, theme) =
        initial_look(options.language, options.theme, &preferences, &manager).await;
    let mut app = App::new(options.compact, language, theme);
    app.set_managed();
    app.set_native_available(true);
    if let Some(warning) = &preferences.warning {
        app.notify(warning.clone());
    }
    // This client's own list view and desk state (D1).
    let (mut client_view, local_view, notice) =
        super::client_view::ClientView::open(&manager, app.language, options.compact).await;
    if let Some(notice) = notice
        && app.toast.is_none()
    {
        app.notify(notice);
    }
    client_view.restore_filter(&mut app);
    let fleet = Arc::new(Fleet::new(manager)?.with_local_view(local_view));
    let mut fleet_updates = fleet.take_updates();
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
    let mut last_projection: Option<Vec<ManagedProjectionRow>> = None;
    let mut revision = 0_u64;
    let mut tasks = JoinSet::<Work>::new();
    let mut poll_pending = false;
    let mut poll = tokio::time::interval(std::time::Duration::from_secs(1));
    poll.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut dialog: Option<Dialog> = None;
    let mut answering: Option<AnswerDialog> = None;
    // Whether the list has held the answer dialog's agent since it opened.
    let mut answering_listed = false;
    let mut queueing: Option<QueueWindow> = None;
    let mut changing: Option<super::changes::ChangesWindow> = None;
    let mut palette: Option<Palette> = None;
    let mut plugin_view: Option<PluginViewOverlay> = None;
    let mut plugin_fp = crate::plugin::manifest::catalog_fingerprint();
    let mut plugin_catalog = crate::plugin::manifest::load_catalog().ok();
    if let Some(cat) = &plugin_catalog {
        super::view::set_plugin_palette(cat.active_resolved_theme().as_ref());
    }

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
            // The panic hook gave the terminal back: a task panicked.
            if terminal.panicked() {
                return Err("a background task panicked; the desk closed".into());
            }
            if app.dirty {
                // The search cursor stays out from under a window drawn above the desk.
                app.modal_open = dialog.is_some()
                    || answering.is_some()
                    || queueing.is_some()
                    || changing.is_some()
                    || palette.is_some()
                    || plugin_view.is_some();
                terminal.draw(|frame| {
                    app.draw(frame);
                    if let Some(dialog) = &mut dialog { dialog.draw(frame, app.theme); }
                    if let Some(answering) = &mut answering { answering.draw(frame, app.theme); }
                    if let Some(queueing) = &mut queueing { queueing.draw(frame, app.theme); }
                    if let Some(changing) = &mut changing { changing.draw(frame, app.theme); }
                    if let Some(plugin_view) = &mut plugin_view { plugin_view.draw(frame, app.theme); }
                    if let Some(palette) = &mut palette { palette.draw(frame, app.theme); }
                }).map_err(|error| error.to_string())?;
                app.dirty = false;
            }
            let mut poll_result: Option<Result<FleetSnapshot, String>> = None;
            let effects = tokio::select! {
                _ = poll.tick(), if !poll_pending => {
                    let next_fp = crate::plugin::manifest::catalog_fingerprint();
                    if next_fp != plugin_fp {
                        plugin_fp = next_fp;
                        plugin_catalog = crate::plugin::manifest::load_catalog().ok();
                        super::view::set_plugin_palette(
                            plugin_catalog
                                .as_ref()
                                .and_then(|c| c.active_resolved_theme())
                                .as_ref(),
                        );
                        app.inspector_cache = None;
                        app.dirty = true;
                    }
                    poll_pending = true;
                    let fleet = fleet.clone();
                    tasks.spawn(async move {
                        Work::Poll(fleet.poll().await)
                    });
                    Vec::new()
                }
                Some(done) = async {
                    match &mut fleet_updates {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    // Deliver completed endpoint polls to the UI immediately instead of
                    // waiting for the next 1 s interval tick.
                    let mut latest = fleet.finish_poll(done);
                    if let Some(rx) = &mut fleet_updates {
                        while let Ok(more) = rx.try_recv() {
                            latest = fleet.finish_poll(more);
                        }
                    }
                    poll_result = Some(latest);
                    Vec::new()
                }
                event = events.next() => match event {
                    Some(Ok(event)) => {
                        let mut effects = Vec::new();
                        client_view.input(&app, &event);
                        let is_mouse_move = matches!(
                            &event,
                            Event::Mouse(mouse) if mouse.kind == MouseEventKind::Moved
                        );
                        if let Some(active) = &mut palette {
                            if matches!(event, Event::Resize(_, _)) {
                                app.handle(event.clone(), Instant::now());
                            }
                            let prev_hover = active.hover();
                            let outcome = active.handle(event, Instant::now());
                            if !is_mouse_move
                                || active.hover() != prev_hover
                                || !matches!(outcome, PaletteResult::None)
                            {
                                app.dirty = true;
                            }
                            match outcome {
                                PaletteResult::None => {}
                                PaletteResult::Close => palette = None,
                                PaletteResult::Activate(target) => {
                                    palette = None;
                                    effects.extend(activate_palette(
                                        target,
                                        &mut app,
                                        &agents,
                                        fleet.clone(),
                                        &mut tasks,
                                        owned_pane.as_deref(),
                                        &current_epoch,
                                        &mut dialog,
                                        &mut plugin_view,
                                        &mut plugin_catalog,
                                        &mut plugin_fp,
                                    ));
                                }
                            }
                        } else if let Some(active) = &mut plugin_view {
                            match active.handle(event) {
                                PluginViewOutcome::None => app.dirty = true,
                                PluginViewOutcome::Close => {
                                    plugin_view = None;
                                    app.dirty = true;
                                }
                                PluginViewOutcome::Refresh => {
                                    if let Some(panel_id) = active.panel_id.clone() {
                                        spawn_plugin_panel(
                                            &mut tasks,
                                            active.plugin_id.clone(),
                                            panel_id,
                                            active.agent.clone(),
                                            None,
                                            None,
                                            Some(fleet.local.native.socket.clone()),
                                        );
                                    }
                                    app.dirty = true;
                                }
                                PluginViewOutcome::Invoke {
                                    plugin_id,
                                    panel_id,
                                    action,
                                    payload,
                                } => {
                                    spawn_plugin_panel(
                                        &mut tasks,
                                        plugin_id,
                                        panel_id,
                                        active.agent.clone(),
                                        Some(action),
                                        payload,
                                        Some(fleet.local.native.socket.clone()),
                                    );
                                    app.dirty = true;
                                }
                            }
                        } else if matches!(&event, Event::Key(key) if key.code == KeyCode::Char(':') && key.modifiers.difference(KeyModifiers::SHIFT).is_empty())
                            && dialog.is_none()
                            && answering.is_none()
                            && queueing.is_none()
                            && changing.is_none()
                            && plugin_view.is_none()
                            && app.overlay.is_none()
                            && app.focus != super::model::Focus::Search
                        {
                            let context = selected_agent(&app, &agents).map(crate::managed::find::agent_label);
                            palette = Some(Palette::new(
                                app.language,
                                palette_items(&app, &agents, plugin_catalog.as_ref()),
                                context,
                            ));
                            let fleet = fleet.clone();
                            tasks.spawn(async move { Work::Panes(fleet.local.pane_targets().await) });
                            app.dirty = true;
                        } else if let Some(active) = &mut changing {
                            use super::changes::ChangesOutcome;
                            let prev_hover = active.hover();
                            let outcome = active.handle(event);
                            if !is_mouse_move || active.hover() != prev_hover || !matches!(outcome, ChangesOutcome::None) {
                                app.dirty = true;
                            }
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
                        } else if let Some(active) = &mut queueing {
                            let prev_hover = active.hover();
                            let outcome = active.handle(event);
                            if !is_mouse_move || active.hover() != prev_hover || !matches!(outcome, QueueOutcome::None) {
                                app.dirty = true;
                            }
                            match outcome {
                                QueueOutcome::None => {}
                                QueueOutcome::Close => queueing = None,
                                QueueOutcome::Op(op) => {
                                    let agent = active.agent.clone();
                                    queue_task(&mut tasks, fleet.clone(), agent, Some(op), None);
                                }
                            }
                        } else if let Some(active) = &mut answering {
                            let prev_hover = active.hover();
                            let outcome = active.handle(event);
                            if !is_mouse_move || active.hover() != prev_hover || !matches!(outcome, Outcome::None) {
                                app.dirty = true;
                            }
                            match outcome {
                                Outcome::None => {}
                                Outcome::Cancel => answering = None,
                                Outcome::Submit { request, reply } => {
                                    let agent = answering.take().expect("active answer dialog").agent;
                                    submit_answer(agent, answering_listed, request, reply, fleet.clone(), &mut tasks, app.language, &current_epoch);
                                }
                            }
                        } else if let Some(active) = &mut dialog {
                            let prev_hover = active.hover();
                            let outcome = active.handle(event);
                            if !is_mouse_move || active.hover() != prev_hover || !matches!(outcome, DialogResult::None) {
                                app.dirty = true;
                            }
                            match outcome {
                                DialogResult::None => {}
                                DialogResult::Cancel => dialog = None,
                                DialogResult::Submit => {
                                    // Refused while open: the typed fields stay.
                                    if let Some((field, reason)) = active.refusal() {
                                        active.focus = field;
                                        active.notice = Some(reason);
                                    } else {
                                        let submitted = dialog.take().expect("active dialog");
                                        submit_dialog(submitted, fleet.clone(), &mut tasks, &mut app, &current_epoch);
                                    }
                                }
                            }
                        } else if !app.inbox.open && matches!(&event, Event::Key(key) if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)) {
                            if let Some(agent) = selected_agent(&app, &agents).filter(|agent| !agent.stale) {
                                dialog = Some(Dialog::interrupt(agent, app.language));
                                app.dirty = true;
                            }
                        } else {
                            effects.extend(app.handle(event, Instant::now()));
                        }
                        effects
                    },
                    Some(Err(error)) => return Err(error.to_string()),
                    None => break,
                },
                Some(completed) = tasks.join_next(), if !tasks.is_empty() => {
                    match completed {
                        Ok(Work::Poll(result)) => {
                            poll_pending = false;
                            poll_result = Some(result);
                        }
                        Ok(Work::Inbox(result)) => {
                            inbox_pending = false;
                            if let Ok(view) = result && app.inbox.apply(view) {
                                app.dirty = true;
                            }
                        }
                        Ok(Work::Action(message)) => {
                            let current = match &message.target {
                                Some(target) => target.same_run(&agents),
                                None => message.boot == current_epoch,
                            };
                            // A result for a run that left the list, or from before the
                            // server changed, still ends its own action and is shown with
                            // the agent's name. Only the views it would open are dropped.
                            let target = message.target.as_ref();
                            let unlisted = target.filter(|target| !current && target.listed).map(|target| target.name.as_str());
                            match message.result {
                            Ok(ActionResult::Receipt(receipt)) => { TargetIdentity::end_action(target, current, &mut app); app.notify(for_unlisted(app.language, unlisted, receipt)); app.inbox.refresh = true; },
                            Ok(ActionResult::Details { .. } | ActionResult::Requests { .. }) if !current => {},
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
                                    answering_listed = listed(&opened.agent, &agents);
                                    answering = Some(opened);
                                }
                            }
                            Err(error) => { TargetIdentity::end_action(target, current, &mut app); app.notify(for_unlisted(app.language, unlisted, action_error(app.language, &error))); app.inbox.refresh = true; },
                            }
                        },
                        Ok(Work::Pager(Err(error))) => {
                            let text = crate::managed::failure::human(&error);
                            let notice = self::message(app.language, format!("Pager: {text}"), format!("페이저: {text}"));
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
                                    Err(error) => app.notify(action_error(app.language, &error)),
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
                                    (_, Some(window)) => window.finished(kind, result.as_ref().map(|_| ()).map_err(String::as_str), queue_done(app.language, kind, &result)),
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
                                    app.notify(action_error(app.language, &error));
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
                        Ok(Work::PluginPanel(result)) => {
                            if let Some(view) = &mut plugin_view {
                                view.set_document(result);
                                app.dirty = true;
                            }
                        }
                        Ok(Work::PluginCommand {
                            plugin_id,
                            title,
                            agent,
                            result,
                        }) => {
                            match result {
                                Ok(out) => {
                                    if let Some(toast) = out.toast {
                                        app.notify(toast);
                                    }
                                    if let Some(doc) = out.ui {
                                        plugin_view = Some(PluginViewOverlay::with_document(
                                            app.language,
                                            plugin_id,
                                            None,
                                            title,
                                            agent,
                                            doc,
                                        ));
                                    } else if let Some(draft) = out.draft
                                        && let Some(selected) = selected_agent(&app, &agents)
                                    {
                                        let mut d = Dialog::prompt(selected, app.language);
                                        if let Some(f) = d.fields.first_mut() {
                                            f.value = draft;
                                            f.cursor = f.grapheme_count();
                                        }
                                        dialog = Some(d);
                                    }
                                }
                                Err(err) => app.notify(format!("Plugin error: {err}")),
                            }
                            app.dirty = true;
                        }
                        Err(error) => { poll_pending = false; app.finish_action(); app.notify(self::message(app.language, format!("Action failed: {error}"), format!("작업 실패: {error}"))); }
                    }
                    Vec::new()
                },
                _ = terminate.recv() => break,
                _ = interrupt.recv() => break,
                _ = hangup.recv() => break,
            };

            if let Some(result) = poll_result {
                match result {
                    Ok(FleetSnapshot { epoch, agents: list, endpoints }) => {
                        let previous_selected_identity = selected_identity(&app, &agents);
                        let local_ready = endpoints.iter().any(|status| status.id == "local" && status.connected);
                        let connecting = endpoints.iter().any(|status| status.error.as_deref() == Some("connecting"));
                        // Before the guards below: they and the epoch must see
                        // the restored agent.
                        client_view.restore_selection(&mut app, |id| list.iter().any(|agent| agent.id == id), local_ready, connecting);
                        let stable_projection = build_managed_projection(&list);
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
                        // A dialog about an agent closes when that run of it is
                        // no longer listed, whichever row is selected. The start
                        // dialog is about no agent and stays.
                        let mut dropped = None;
                        if let Some(agent) = dialog.as_ref().and_then(|dialog| dialog.kind.agent())
                            && !listed(agent, &agents)
                        {
                            dropped = Some(crate::managed::find::agent_label(agent));
                            dialog = None;
                        }
                        if let Some(active) = &answering {
                            // An agent the saved view hides can be answered from
                            // the inbox without ever being listed.
                            answering_listed |= listed(&active.agent, &agents);
                            if left(&active.agent, answering_listed, &agents) {
                                dropped = Some(crate::managed::find::agent_label(&active.agent));
                                answering = None;
                            }
                        }
                        // The queue window stays: it shows the agent's new run
                        // and keeps typed text from reaching the desk's keys.
                        // A first run after an empty selection is a new run too.
                        if identity_changed && let Some(window) = &queueing {
                            let agent = window.agent.clone();
                            queue_task(&mut tasks, fleet.clone(), agent, None, None);
                        }
                        // The first agent of an empty or new desk replaces no
                        // identity: notices and open windows stay as they are.
                        if identity_changed && previous_selected_identity.is_some() {
                            palette = None;
                            app.cancel_interactions();
                            // Help is not about the agent and stays open.
                            if app.overlay == Some(Overlay::Context) {
                                app.overlay = None;
                            }
                            app.ack_in_flight = None;
                            app.prompt_in_flight = false;
                            app.toast = None;
                            app.dirty = true;
                        }
                        if let Some(name) = dropped {
                            app.notify(message(
                                app.language,
                                format!("{name}: its run changed or it left the list, so its dialog was closed"),
                                format!("{name}: 실행이 바뀌었거나 목록에서 빠져 대화 상자를 닫았습니다"),
                            ));
                        }
                        if epoch != current_epoch || last_projection.as_ref() != Some(&stable_projection) {
                            revision = revision.wrapping_add(1);
                            // The selection's identity is not a daemon epoch: the block
                            // above made the resets a new identity needs, so the
                            // snapshot must not read a moved selection as a restart.
                            app.epoch.clone_from(&app_epoch);
                            let observations = stable_projection
                                .iter()
                                .map(|row| row.observation.clone())
                                .collect();
                            app.apply_observations(app_epoch, revision.to_string(), observations);
                            last_projection = Some(stable_projection);
                        }
                        client_view.rows_shown(&mut app, local_ready);
                        current_epoch = epoch;
                        if !app.connected || app.connection_message.is_some() { app.set_connection(true, None); }
                    }
                    Err(error) => {
                        let message = self::message(
                            app.language,
                            format!("Native inventory unavailable: {error}"),
                            format!("네이티브 목록을 읽을 수 없습니다: {error}"),
                        );
                        if app.connected || app.connection_message.as_deref() != Some(&message) { app.set_connection(false, Some(message)); }
                    }
                }
            }

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
                    app.notify(message(app.language, "An action is pending; try again shortly".into(), "작업을 처리하고 있습니다. 잠시 후 다시 시도하세요".into()));
                    continue;
                }
                dispatch_effect(effect, &mut dialog, &mut app, &agents, fleet.clone(), &mut tasks, owned_pane.as_deref(), &current_epoch, last_poll_ms);
                app.dirty = true;
            }
            client_view.tick(&mut app, Instant::now());
        }
        Ok(0)
    }.await;
    tasks.abort_all();
    drop(events);
    drop(terminal);
    // After the terminal is given back: a busy store must not hold the
    // desk on screen.
    client_view.finish(&app).await;
    result
}

/// Direct managed-agent projection row without intermediate JSON DOM allocation.
#[derive(PartialEq, Eq)]
struct ManagedProjectionRow {
    observation: Observation,
    run: String,
    endpoint_key: String,
}

fn build_managed_projection(agents: &[Agent]) -> Vec<ManagedProjectionRow> {
    agents
        .iter()
        .map(|agent| ManagedProjectionRow {
            observation: Observation::from_agent(agent),
            run: agent.run.clone(),
            endpoint_key: agent.endpoint_key.clone(),
        })
        .collect()
}

/// Agents in desk order, then the desk actions for the current selection.
/// Panes of this server are added once the background query returns.
fn palette_items(
    app: &App,
    agents: &HashMap<String, Agent>,
    catalog: Option<&crate::plugin::manifest::PluginCatalog>,
) -> Vec<palette::Item> {
    // The inbox view selects events, not agents: only its own actions.
    if app.managed && app.inbox.open {
        return [Action::Inbox, Action::NextUnseen]
            .into_iter()
            .map(|action| palette::Item {
                kind: palette::Kind::Action,
                label: action_label(app.language, action, false, false).to_owned(),
                detail: String::new(),
                hint: menu_shortcut(action).to_owned(),
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
                hint: menu_shortcut(action).to_owned(),
                target: palette::Target::Action(action),
                enabled: app.action_enabled(action),
                pane: None,
            }),
    );
    if let Some(cat) = catalog {
        for plugin in cat.active_plugins() {
            let Some(m) = &plugin.manifest else {
                continue;
            };
            for panel in &m.panels {
                items.push(palette::Item {
                    kind: palette::Kind::Panel,
                    label: panel.title.clone(),
                    detail: format!("plugin: {}", plugin.id),
                    hint: "panel".into(),
                    target: palette::Target::PluginPanel {
                        plugin_id: plugin.id.clone(),
                        panel_id: panel.id.clone(),
                        title: panel.title.clone(),
                    },
                    enabled: true,
                    pane: None,
                });
            }
            for cmd in &m.commands {
                let slash = format!("/{}", cmd.slash.trim_start_matches('/'));
                items.push(palette::Item {
                    kind: palette::Kind::Plugin,
                    label: format!("{slash} {}", cmd.title),
                    detail: cmd.description.clone(),
                    hint: plugin.id.clone(),
                    target: palette::Target::PluginCommand {
                        plugin_id: plugin.id.clone(),
                        command_id: cmd.id.clone(),
                        title: cmd.title.clone(),
                    },
                    enabled: true,
                    pane: None,
                });
            }
        }
        for t in cat.all_themes() {
            let active = cat.active_theme.as_deref() == Some(t.id.as_str());
            items.push(palette::Item {
                kind: palette::Kind::Theme,
                label: format!("Theme: {}", t.name),
                detail: format!("{} · {}", t.appearance, t.plugin_id),
                hint: if active {
                    "active".into()
                } else {
                    t.id.clone()
                },
                target: palette::Target::PluginTheme {
                    theme_id: Some(t.id),
                    name: t.name,
                },
                enabled: true,
                pane: None,
            });
        }
        if cat.active_theme.is_some() {
            items.push(palette::Item {
                kind: palette::Kind::Theme,
                label: word(app.language, "Theme: Reset default", "테마: 기본값 복원").into(),
                detail: String::new(),
                hint: "reset".into(),
                target: palette::Target::PluginTheme {
                    theme_id: None,
                    name: "default".into(),
                },
                enabled: true,
                pane: None,
            });
        }
    }
    items.push(palette::Item {
        kind: palette::Kind::Plugin,
        label: word(
            app.language,
            "Create Masil plugin with AI",
            "AI로 Masil 플러그인 만들기",
        )
        .into(),
        detail: word(
            app.language,
            "Launch agy, claude, or codex in Creator Mode",
            "agy, claude, codex로 즉석 플러그인 제작",
        )
        .into(),
        hint: "creator".into(),
        target: palette::Target::CreatePluginWithAi,
        enabled: true,
        pane: None,
    });
    items.push(palette::Item {
        kind: palette::Kind::Plugin,
        label: word(app.language, "Reload plugins", "플러그인 다시 불러오기").into(),
        detail: String::new(),
        hint: "reload".into(),
        target: palette::Target::ReloadPlugins,
        enabled: true,
        pane: None,
    });
    items
}

fn agent_to_plugin_json(agent: &Agent, socket: &std::path::Path) -> Value {
    json!({
        "name": agent.name,
        "provider": agent.provider,
        "state": agent.state,
        "cwd": agent.cwd,
        "pane": agent.pane_id,
        "pane_id": agent.pane_id,
        "server_socket": socket.to_string_lossy(),
        "session_id": agent.session_id,
    })
}

fn spawn_plugin_panel(
    tasks: &mut JoinSet<Work>,
    plugin_id: String,
    panel_id: String,
    agent: Option<Value>,
    action: Option<String>,
    payload: Option<Value>,
    socket: Option<PathBuf>,
) {
    tasks.spawn(async move {
        let loaded = tokio::task::spawn_blocking(move || {
            let cat = crate::plugin::manifest::load_catalog()?;
            let p = cat
                .find_plugin(&plugin_id)
                .cloned()
                .ok_or_else(|| format!("plugin '{plugin_id}' not found"))?;
            Ok::<_, String>(p)
        })
        .await;
        let res = match loaded {
            Ok(Ok(plugin)) => {
                crate::plugin::runner::invoke_panel(
                    &plugin,
                    &panel_id,
                    agent,
                    action,
                    payload,
                    serde_json::Map::new(),
                    socket,
                )
                .await
            }
            Ok(Err(e)) => Err(e),
            Err(e) => Err(e.to_string()),
        };
        Work::PluginPanel(res)
    });
}

/// Run a palette entry through the same guarded paths as keys and clicks.
#[allow(clippy::too_many_arguments)]
fn activate_palette(
    target: palette::Target,
    app: &mut App,
    agents: &HashMap<String, Agent>,
    fleet: Arc<Fleet>,
    tasks: &mut JoinSet<Work>,
    owned_pane: Option<&str>,
    expected_boot: &str,
    dialog: &mut Option<Dialog>,
    plugin_view: &mut Option<PluginViewOverlay>,
    plugin_catalog: &mut Option<crate::plugin::manifest::PluginCatalog>,
    plugin_fp: &mut String,
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
                        format!("{id} 선택. 지금은 pane으로 이동할 수 없습니다"),
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
                            format!("pane {} 선택 완료", pane.pane_id),
                        ))
                    });
                action_message(boot, None, result)
            });
            Vec::new()
        }
        palette::Target::Action(action) => app.effect_for(action).into_iter().collect(),
        palette::Target::PluginPanel {
            plugin_id,
            panel_id,
            title,
        } => {
            let agent_json = selected_agent(app, agents)
                .map(|a| agent_to_plugin_json(a, &fleet.local.native.socket));
            *plugin_view = Some(PluginViewOverlay::loading(
                app.language,
                plugin_id.clone(),
                Some(panel_id.clone()),
                title,
                agent_json.clone(),
            ));
            spawn_plugin_panel(
                tasks,
                plugin_id,
                panel_id,
                agent_json,
                None,
                None,
                Some(fleet.local.native.socket.clone()),
            );
            Vec::new()
        }
        palette::Target::PluginCommand {
            plugin_id,
            command_id,
            title,
        } => {
            let agent_json = selected_agent(app, agents)
                .map(|a| agent_to_plugin_json(a, &fleet.local.native.socket));
            let sock = Some(fleet.local.native.socket.clone());
            tasks.spawn(async move {
                let pid = plugin_id.clone();
                let loaded = tokio::task::spawn_blocking(move || {
                    let cat = crate::plugin::manifest::load_catalog()?;
                    cat.find_plugin(&pid)
                        .cloned()
                        .ok_or_else(|| format!("plugin '{pid}' not found"))
                })
                .await;
                let result = match loaded {
                    Ok(Ok(plugin)) => {
                        crate::plugin::runner::invoke_command(
                            &plugin,
                            &command_id,
                            String::new(),
                            agent_json.clone(),
                            sock,
                        )
                        .await
                    }
                    Ok(Err(e)) => Err(e),
                    Err(e) => Err(e.to_string()),
                };
                Work::PluginCommand {
                    plugin_id,
                    title,
                    agent: agent_json,
                    result,
                }
            });
            Vec::new()
        }
        palette::Target::PluginTheme { theme_id, name } => {
            match crate::plugin::set_active_theme(theme_id.as_deref()) {
                Ok(_) => {
                    *plugin_fp = crate::plugin::manifest::catalog_fingerprint();
                    *plugin_catalog = crate::plugin::manifest::load_catalog().ok();
                    super::view::set_plugin_palette(
                        plugin_catalog
                            .as_ref()
                            .and_then(|c| c.active_resolved_theme())
                            .as_ref(),
                    );
                    app.inspector_cache = None;
                    app.notify(message(
                        language,
                        format!("Theme set to {name}"),
                        format!("테마 적용: {name}"),
                    ));
                }
                Err(e) => app.notify(format!("Theme error: {e}")),
            }
            Vec::new()
        }
        palette::Target::ReloadPlugins => {
            if let Ok(mut cfg) = crate::plugin::manifest::PluginsConfig::load() {
                cfg.bump_reload();
                let _ = cfg.save();
            }
            *plugin_fp = crate::plugin::manifest::catalog_fingerprint();
            *plugin_catalog = crate::plugin::manifest::load_catalog().ok();
            super::view::set_plugin_palette(
                plugin_catalog
                    .as_ref()
                    .and_then(|c| c.active_resolved_theme())
                    .as_ref(),
            );
            app.inspector_cache = None;
            let count = plugin_catalog
                .as_ref()
                .map(|c| c.active_plugins().count())
                .unwrap_or(0);
            app.notify(message(
                language,
                format!("Reloaded plugins ({count} active)"),
                format!("플러그인 새로고침 완료 ({count}개 활성)"),
            ));
            Vec::new()
        }
        palette::Target::CreatePluginWithAi => {
            *dialog = Some(Dialog::create_plugin_with_ai(app.language));
            Vec::new()
        }
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
                Err(error) => app.notify(message(
                    language,
                    format!("Could not open start dialog: {error}"),
                    format!("시작 대화 상자를 열 수 없습니다: {error}"),
                )),
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
                *dialog = Some(Dialog::interrupt(agent, app.language));
            }
        }
        Effect::CloseAgent => {
            if let Some(agent) = selected_agent(app, agents).filter(|agent| !agent.stale) {
                *dialog = Some(Dialog::close(agent, app.language));
            }
        }
        Effect::Ack { id, revision, .. } => {
            if let Some(agent) = agents.get(&id).filter(|agent| !agent.stale).cloned() {
                let boot = expected_boot.to_owned();
                let mut target = TargetIdentity::for_agent(&agent);
                target.ack = Some((id.clone(), revision));
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
                        format!("pane {} 선택 완료", agent.pane_id),
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
                            format!("pane {} 선택 완료", agent.pane_id),
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
                        "결과를 모릅니다. pane을 본 뒤 `agent operation resolve {key}`로 정하세요"
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
    // An unknown outcome keeps its name: the plain text reads as a failure.
    let text = crate::managed::failure::human(error);
    if outcome_is_unknown(error) {
        message(
            language,
            format!("Outcome unknown: {text}"),
            format!("결과를 모릅니다: {text}"),
        )
    } else {
        text.to_owned()
    }
}

/// The answer dialog for these requests, or a note when there is nothing
/// masil can answer.
fn open_answers(agent: Agent, value: Value, app: &mut App) -> Option<AnswerDialog> {
    let language = app.language;
    if !crate::managed::answer::is_channel(value["channel"].as_str().unwrap_or("")) {
        let reason = value["reason"].as_str().unwrap_or("no answer channel");
        app.notify(message(
            language,
            format!("{}: {reason}. Press g to go to its pane.", agent.name),
            format!("{}: pane에서 답하세요 (g로 이동). {reason}", agent.name),
        ));
        return None;
    }
    let requests = value["requests"].as_array().cloned().unwrap_or_default();
    if requests.is_empty() {
        app.notify(message(
            language,
            format!("{} has no pending request", agent.name),
            format!("{}에 열린 요청이 없습니다", agent.name),
        ));
        return None;
    }
    Some(AnswerDialog::new(language, agent, requests))
}

#[allow(clippy::too_many_arguments)]
fn submit_answer(
    agent: Agent,
    listed: bool,
    request: String,
    reply: crate::managed::answer::Reply,
    fleet: Arc<Fleet>,
    tasks: &mut JoinSet<Work>,
    language: super::model::Language,
    current_epoch: &str,
) {
    let epoch = current_epoch.to_owned();
    let mut target = TargetIdentity::for_agent(&agent);
    target.listed = listed;
    let label = crate::managed::answer::profile(&agent.provider).map_or_else(
        || agent.provider.clone(),
        |profile| profile.label.to_owned(),
    );
    tasks.spawn(async move {
        let result = fleet.answer(&agent, &request, reply).await.map(|value| {
            let accepted = value["provider_accepted"] == true;
            let also = value["also_answered"].as_array().map_or(0, Vec::len);
            let stage = value["stage"].as_str().unwrap_or("unknown");
            let mut english = if accepted {
                format!("Answered {request}; {label} accepted it")
            } else {
                format!("Answered {request}: {stage}")
            };
            let mut korean = if accepted {
                format!("{request}에 답했습니다. {label}가 받았습니다")
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
            let mut target = TargetIdentity::for_agent(&agent);
            app.start_prompt();
            target.prompt = Some(app.prompt_serial);
            tasks.spawn(async move {
                let result = fleet
                    .prompt(&agent, &text)
                    .await
                    .map(|value| ActionResult::Receipt(prompt_receipt(language, &value)));
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
                        "선택한 pane에 끊기 key를 보냈습니다. 제공자가 멈췄는지는 확인되지 않습니다"
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
                        format!("pane {} 닫기 완료", agent.pane_id),
                    ))
                });
                action_message(epoch, Some(target), result)
            });
        }
        DialogKind::CreatePluginWithAi => {
            let epoch = current_epoch.to_owned();
            let raw_provider = values
                .first()
                .map(String::as_str)
                .unwrap_or("agy")
                .trim()
                .to_ascii_lowercase();
            let provider = match raw_provider.as_str() {
                "antigravity" => "agy".to_owned(),
                other => other.to_owned(),
            };
            let goal = values
                .get(1)
                .map(String::as_str)
                .unwrap_or("")
                .trim()
                .to_owned();
            tasks.spawn(async move {
                let res = async {
                    let plugins_root = crate::plugin::manifest::plugins_dir()?;
                    let _ = std::fs::create_dir_all(&plugins_root);
                    let target_skill = provider.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        crate::plugin::skill::install_target(&target_skill, false)
                    })
                    .await;
                    let slug: String = goal
                        .chars()
                        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
                        .take(12)
                        .collect::<String>()
                        .to_ascii_lowercase();
                    let agent_name = if slug.is_empty() {
                        format!("plugin-ai-{}", crate::remote::config::now_ms() % 1000)
                    } else {
                        format!("plugin-{slug}")
                    };
                    let prompt =
                        crate::plugin::skill::build_creator_prompt(&goal, None, &plugins_root);
                    let sock = fleet.local.native.socket.clone();
                    let cwd = plugins_root.to_string_lossy().into_owned();
                    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
                    let out = tokio::process::Command::new(&exe)
                        .args([
                            "agent",
                            "--socket",
                            &sock.to_string_lossy(),
                            "start",
                            &agent_name,
                            &provider,
                            "--cwd",
                            &cwd,
                        ])
                        .output()
                        .await
                        .map_err(|e| e.to_string())?;
                    if !out.status.success() {
                        return Err(String::from_utf8_lossy(&out.stderr).trim().to_owned());
                    }
                    let name_for_prompt = agent_name.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_millis(2400)).await;
                        let _ = tokio::process::Command::new(&exe)
                            .args([
                                "agent",
                                "--socket",
                                &sock.to_string_lossy(),
                                "prompt",
                                &name_for_prompt,
                                &prompt,
                            ])
                            .output()
                            .await;
                    });
                    Ok(ActionResult::Receipt(message(
                        language,
                        format!("Started AI plugin creator '{agent_name}' ({provider})"),
                        format!("AI 플러그인 제작 세션 '{agent_name}' ({provider}) 시작됨"),
                    )))
                }
                .await;
                action_message(epoch, None, res)
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn agent(name: &str, state: &str) -> Agent {
        serde_json::from_value(json!({
            "id": name, "name": name, "provider": "codex", "pane_id": "%7",
            "window_id": "@1", "workspace": "main", "cwd": "/tmp", "boot": "b",
            "generation": "1", "run": "r1", "process": "running", "state": state,
            "evidence": {"state": state, "source": "screen"},
            "seen": false, "revision": "1", "returned_idle": false,
        }))
        .unwrap()
    }

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    /// The drawn rows and where the draw left the terminal cursor.
    fn screen(dialog: &mut Dialog, width: u16, height: u16) -> (String, (u16, u16)) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| dialog.draw(frame, Theme::Dark))
            .unwrap();
        let cursor = terminal.get_cursor_position().unwrap();
        let buffer = terminal.backend().buffer();
        let text = (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        (text, (cursor.x, cursor.y))
    }

    #[test]
    fn a_pasted_line_break_or_tab_becomes_one_space() {
        let mut dialog = Dialog::prompt(&agent("builder", "idle"), Language::English);
        dialog.handle(Event::Paste("fix the bug in\nmain.rs".into()));
        assert_eq!(dialog.fields[0].value, "fix the bug in main.rs");
        dialog.fields[0].value.clear();
        dialog.handle(Event::Paste("a\r\nb\rc\td\u{1b}e".into()));
        assert_eq!(dialog.fields[0].value, "a b c de");
        assert!(dialog.notice.is_none());
    }

    #[test]
    fn a_pasted_line_break_at_either_end_adds_no_space() {
        let mut dialog = Dialog::rename(&agent("builder", "idle"), Language::English);
        dialog.fields[0].value.clear();
        dialog.handle(Event::Paste("reviewer\n".into()));
        assert_eq!(dialog.fields[0].value, "reviewer");
        assert!(dialog.refusal().is_none());
        let mut dialog = Dialog::prompt(&agent("builder", "idle"), Language::English);
        dialog.handle(Event::Paste("\r\n\t/tmp/work dir\r\n".into()));
        assert_eq!(dialog.fields[0].value, "/tmp/work dir");
        dialog.handle(Event::Paste("\n".into()));
        assert_eq!(dialog.fields[0].value, "/tmp/work dir");
    }

    #[test]
    fn an_insert_cut_at_the_field_limit_says_so_until_the_next_key() {
        let mut dialog = Dialog::rename(&agent("builder", "idle"), Language::English);
        dialog.fields[0].value.clear();
        dialog.handle(Event::Paste("a".repeat(40)));
        assert_eq!(dialog.fields[0].value.len(), 32);
        let notice = dialog.notice.clone().expect("a notice for the cut");
        assert!(notice.contains("32 bytes"), "{notice}");
        let (text, _) = screen(&mut dialog, 80, 24);
        assert!(text.contains("32 bytes"), "{text}");
        dialog.handle(key(KeyCode::Backspace));
        assert!(dialog.notice.is_none());
    }

    #[test]
    fn a_refused_name_or_server_keeps_the_dialog_and_names_the_rule() {
        let endpoints = [
            ("local".to_owned(), "k1".to_owned()),
            ("build".to_owned(), "k2".to_owned()),
        ];
        let mut dialog = Dialog::start(Language::English, &endpoints, None);
        assert!(dialog.note.contains("lowercase letter"));
        // Server, name, provider, working directory.
        dialog.fields[0].value = "buld".into();
        dialog.fields[1].value = "reviewer".into();
        let (field, reason) = dialog.refusal().expect("an unknown server is refused");
        assert_eq!(field, 0);
        assert!(reason.contains("build, local"), "{reason}");
        dialog.fields[0].value = "build".into();
        assert!(dialog.refusal().is_none());
        for name in ["Reviewer", "", "9lives", "re viewer", "한글"] {
            dialog.fields[1].value = name.into();
            assert_eq!(
                dialog.refusal().map(|(field, _)| field),
                Some(1),
                "{name:?}"
            );
        }
        for name in ["reviewer", "r", "api-review_2"] {
            assert!(name_allowed(name), "{name}");
        }
        assert!(!name_allowed(&"a".repeat(33)));

        let mut rename = Dialog::rename(&agent("builder", "idle"), Language::Korean);
        rename.fields[0].value = "Builder".into();
        assert!(rename.refusal().is_some());
        // Text and confirmation dialogs have nothing to refuse.
        assert!(
            Dialog::draft(&agent("builder", "idle"), Language::English)
                .refusal()
                .is_none()
        );
    }

    #[test]
    fn agent_dialogs_name_the_agent_and_its_pane() {
        let builder = agent("builder", "idle");
        for dialog in [
            Dialog::prompt(&builder, Language::English),
            Dialog::rename(&builder, Language::English),
            Dialog::interrupt(&builder, Language::English),
            Dialog::close(&builder, Language::English),
            Dialog::close(&builder, Language::Korean),
        ] {
            assert!(dialog.title.contains("builder (%7)"), "{}", dialog.title);
            assert!(dialog.kind.agent().is_some());
        }
        assert!(
            Dialog::start(Language::English, &[], None)
                .kind
                .agent()
                .is_none()
        );
    }

    #[test]
    fn the_focused_field_places_the_terminal_cursor_after_its_text() {
        let mut dialog = Dialog::prompt(&agent("builder", "idle"), Language::English);
        dialog.handle(Event::Paste("한글 ab".into()));
        let (text, (x, y)) = screen(&mut dialog, 80, 24);
        let line = text.lines().nth(y as usize).unwrap();
        // Two wide characters, a space and two letters end seven cells in.
        let start = line.chars().take_while(|c| *c != '한').count() as u16;
        assert_eq!(x, start + 7, "{line}");
        // A value wider than the field keeps a free cell for the cursor.
        dialog.handle(Event::Paste("x".repeat(200)));
        let (text, (x, y)) = screen(&mut dialog, 80, 24);
        let line = text
            .lines()
            .nth(y as usize)
            .unwrap()
            .chars()
            .collect::<Vec<_>>();
        assert_eq!(line[x as usize], ' ', "the cursor covers no character");
        assert_eq!(line[x as usize - 1], 'x');
    }

    #[test]
    fn a_dialog_closes_for_its_own_agent_only() {
        let builder = agent("builder", "idle");
        let mut other = agent("reviewer", "idle");
        other.pane_id = "%8".into();
        other.run = "r2".into();
        let mut agents = HashMap::from([
            (builder.id.clone(), builder.clone()),
            (other.id.clone(), other.clone()),
        ]);
        assert!(listed(&builder, &agents));
        // Another agent's run changing does not concern this one.
        agents.get_mut("reviewer").unwrap().run = "r3".into();
        assert!(listed(&builder, &agents));
        assert!(!listed(&other, &agents));
        agents.get_mut("builder").unwrap().stale = true;
        assert!(!listed(&builder, &agents));
        agents.remove("builder");
        assert!(!listed(&builder, &agents));
    }

    #[test]
    fn a_dialog_for_a_hidden_agent_stays_until_its_pane_runs_another() {
        let hidden = agent("builder", "blocked");
        let mut other = agent("reviewer", "idle");
        other.pane_id = "%8".into();
        let mut agents = HashMap::from([(other.id.clone(), other)]);
        // The saved view hides the agent: it was never listed.
        assert!(!listed(&hidden, &agents));
        assert!(!left(&hidden, false, &agents));
        // A listed agent that drops out of the list did leave.
        assert!(left(&hidden, true, &agents));
        // The view shows it again, with the same run and then a new one.
        agents.insert(hidden.id.clone(), hidden.clone());
        assert!(!left(&hidden, false, &agents));
        agents.get_mut("builder").unwrap().run = "r2".into();
        assert!(left(&hidden, false, &agents));
    }

    #[test]
    fn a_late_result_ends_only_its_own_action() {
        let mut app = App::new(false, Language::English, Theme::Dark);
        let older = agent("builder", "idle");
        // A prompt to an old run, then a prompt to the run that replaced it.
        app.start_prompt();
        let mut first = TargetIdentity::for_agent(&older);
        first.prompt = Some(app.prompt_serial);
        app.prompt_in_flight = false;
        app.start_prompt();
        TargetIdentity::end_action(Some(&first), false, &mut app);
        assert!(app.prompt_in_flight, "the newer prompt is still in flight");
        let mut second = TargetIdentity::for_agent(&older);
        second.prompt = Some(app.prompt_serial);
        TargetIdentity::end_action(Some(&second), false, &mut app);
        assert!(!app.prompt_in_flight);

        // Mark seen: only the press the result answers is cleared.
        app.ack_in_flight = Some(("builder".into(), "2".into()));
        let mut ack = TargetIdentity::for_agent(&older);
        ack.ack = Some(("builder".into(), "1".into()));
        TargetIdentity::end_action(Some(&ack), false, &mut app);
        assert!(app.ack_in_flight.is_some());
        // A late rename result holds no flag at all.
        TargetIdentity::end_action(Some(&TargetIdentity::for_agent(&older)), false, &mut app);
        assert!(app.ack_in_flight.is_some());
        ack.ack = Some(("builder".into(), "2".into()));
        TargetIdentity::end_action(Some(&ack), false, &mut app);
        assert!(app.ack_in_flight.is_none());

        // A result for the current run ends the action as before.
        app.start_prompt();
        TargetIdentity::end_action(Some(&TargetIdentity::for_agent(&older)), true, &mut app);
        assert!(!app.prompt_in_flight);
    }

    #[test]
    fn an_unknown_outcome_is_never_worded_as_a_failure() {
        let failed = action_error(Language::English, "target_absent: agent target not found");
        assert_eq!(failed, "Action failed: agent target not found");
        let unknown = action_error(
            Language::English,
            "outcome_unknown: server_unreachable: native command timed out",
        );
        assert_eq!(unknown, "Outcome unknown: native command timed out");
        assert!(
            action_error(Language::Korean, "outcome_unknown: timed out")
                .starts_with("결과를 모릅니다:")
        );
        // The resolve command leads, so a cut footer keeps it.
        let keyed = action_error(
            Language::English,
            "outcome_unknown: prompt operation 1 pasted its text and masil sent Enter, but its result is unknown (x); look at the pane, then run `masil-agent agent operation resolve run:r1/prompt/1 --as delivered|not-delivered`",
        );
        assert!(
            keyed.starts_with(
                "Outcome unknown: check the pane, then `agent operation resolve run:r1/prompt/1`."
            ),
            "{keyed}"
        );
        assert!(!keyed.contains("Action failed"));
        assert_eq!(operation_key("no command here"), None);
        // Every code of the unknown class reads the same way.
        let partial = action_error(
            Language::English,
            "interrupt_partial: one key of two reached the pane",
        );
        assert_eq!(partial, "Outcome unknown: one key of two reached the pane");
        let sent = queue_done(
            Language::English,
            "sent",
            &Err("api_timeout: no reply in time".into()),
        );
        assert_eq!(sent, "Outcome unknown: no reply in time");
        let refused = queue_done(
            Language::English,
            "sent",
            &Err("busy: agent is working".into()),
        );
        assert!(!refused.starts_with("Outcome unknown"), "{refused}");
    }

    #[test]
    fn the_prompt_receipt_follows_the_returned_stage() {
        let receipt = |stage: &str| {
            prompt_receipt(
                Language::English,
                &json!({"stage": stage, "operation_key": "run:r1/prompt/1"}),
            )
        };
        assert_eq!(
            receipt("delivered"),
            "Prompt and Enter delivered; provider acceptance is unverified"
        );
        assert!(receipt("native_accepted").contains("provider reported"));
        assert!(receipt("not_delivered").starts_with("Prompt not delivered"));
        for stage in ["pending", "", "something_new"] {
            let text = receipt(stage);
            assert!(text.starts_with("Outcome unknown"), "{stage}: {text}");
            assert!(text.contains("operation resolve run:r1/prompt/1"), "{text}");
            assert!(!text.contains("delivered;"), "{text}");
        }
        // A reply without a stage claims no delivery either.
        assert!(prompt_receipt(Language::Korean, &json!({})).starts_with("결과를 모릅니다"));
    }

    #[test]
    fn a_result_for_an_unlisted_run_names_its_agent() {
        assert_eq!(
            for_unlisted(Language::English, Some("builder"), "Closed pane %7".into()),
            "builder (that run is no longer listed): Closed pane %7"
        );
        assert_eq!(
            for_unlisted(Language::English, None, "Closed pane %7".into()),
            "Closed pane %7"
        );
        let target = TargetIdentity::for_agent(&agent("builder", "idle"));
        assert_eq!(target.name, "builder");
        assert!(!target.same_run(&HashMap::new()));
    }

    #[test]
    fn a_blocked_agent_projects_as_waiting_for_input() {
        use crate::ui::i18n::activity_label;
        for (state, activity, label) in [
            ("blocked", "blocked", "Waiting for input"),
            ("idle", "idle", "Idle"),
            ("working", "working", "Working"),
            ("exited", "unknown", "Unknown"),
        ] {
            let projection = agent("builder", state).projection();
            assert_eq!(projection["native"]["activity"], activity, "{state}");
            assert_eq!(activity_label(Language::English, activity), label);
        }
        assert_eq!(activity_label(Language::Korean, "blocked"), "입력 대기");
    }

    #[test]
    fn direct_observation_matches_json_projection_across_states() {
        for (state, returned_idle, stale, permissions, questions, columns, endpoint_label) in [
            ("idle", false, false, 0, 0, false, ""),
            ("working", false, false, 0, 0, true, ""),
            ("blocked", false, false, 2, 0, false, "edge"),
            ("blocked", false, false, 0, 1, false, ""),
            ("blocked", false, false, 0, 0, false, ""),
            ("idle", true, false, 0, 0, false, ""),
            ("exited", false, true, 0, 0, false, "remote"),
        ] {
            let mut item = agent("builder", state);
            item.returned_idle = returned_idle;
            item.stale = stale;
            item.permission_count = permissions;
            item.question_count = questions;
            item.columns = columns;
            item.endpoint_label = endpoint_label.into();
            let mut via_json = Observation::parse(&item.projection()).unwrap();
            via_json.observed_at_ms = 0;
            let direct = Observation::from_agent(&item);
            assert_eq!(direct, via_json, "state={state} stale={stale}");
        }
    }

    #[test]
    fn dialog_inline_cursor_supports_navigation_insertion_and_word_kill() {
        let mut dialog = Dialog::rename(&agent("alpha-gamma", "idle"), Language::English);
        // Move cursor left past "gamma" (5 chars) and insert "beta-".
        for _ in 0..5 {
            dialog.handle(Event::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)));
        }
        dialog.handle(Event::Paste("beta-".into()));
        assert_eq!(dialog.fields[0].value, "alpha-beta-gamma");
        // Home + Delete removes 'a'.
        dialog.handle(Event::Key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE)));
        dialog.handle(Event::Key(KeyEvent::new(
            KeyCode::Delete,
            KeyModifiers::NONE,
        )));
        assert_eq!(dialog.fields[0].value, "lpha-beta-gamma");
        // End + Backspace removes 'a'.
        dialog.handle(Event::Key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE)));
        dialog.handle(Event::Key(KeyEvent::new(
            KeyCode::Backspace,
            KeyModifiers::NONE,
        )));
        assert_eq!(dialog.fields[0].value, "lpha-beta-gamm");
    }

    #[test]
    fn the_server_theme_names_map_to_desk_themes() {
        assert_eq!(server_theme("light"), Some(Theme::Light));
        assert_eq!(server_theme("tmux"), Some(Theme::Terminal));
        assert_eq!(server_theme("terminal"), Some(Theme::Terminal));
        assert_eq!(server_theme("dark"), Some(Theme::Dark));
        assert_eq!(server_theme("solarized"), None);
    }

    #[test]
    #[ignore]
    fn bench_managed_projection_50_agents() {
        let list: Vec<Agent> = (0..50)
            .map(|index| {
                let mut a = agent(
                    &format!("agent-{index:02}"),
                    if index % 4 == 0 { "blocked" } else { "working" },
                );
                a.pane_id = format!("%{index}");
                a.run = format!("run-{index}");
                a
            })
            .collect();
        let mut app = App::new(false, Language::English, Theme::Dark);
        app.set_managed();
        let iters = 1000usize;
        let mut last_projection: Option<Vec<ManagedProjectionRow>> = None;
        let start = std::time::Instant::now();
        for index in 0..iters {
            let stable_projection = build_managed_projection(&list);
            if index == 0 || last_projection.as_ref() != Some(&stable_projection) {
                let observations = stable_projection
                    .iter()
                    .map(|row| row.observation.clone())
                    .collect();
                app.apply_observations("e1".into(), index.to_string(), observations);
                last_projection = Some(stable_projection);
            }
        }
        let elapsed = start.elapsed();
        eprintln!(
            "BENCH_MANAGED_PROJ_50_UNCHANGED: total_ms={:.2} us_per_poll={:.2}",
            elapsed.as_secs_f64() * 1000.0,
            elapsed.as_secs_f64() * 1_000_000.0 / iters as f64
        );
    }
}
