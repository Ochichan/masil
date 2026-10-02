//! The prompt queue window (`e`): an agent's queued prompts, and those of
//! its earlier runs. Nothing here sends on its own; `s` sends the selected
//! prompt through the durable prompt path (docs/prompt-queue.md).

use super::{
    managed::dialog_colors,
    model::{Language, Theme},
    view::{cell_width, ellipsize, wrap},
};
use crate::managed::{Agent, queue::QueueOp};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    widgets::{Block, Borders, Clear, Paragraph},
};
use serde_json::Value;
use unicode_segmentation::UnicodeSegmentation;

const BODY_LIMIT: usize = 32_768;
const PATH_LIMIT: usize = 4_096;
const LIST_ROWS: usize = 8;
/// The pause in typing after a save before list keys act again.
const QUIET: std::time::Duration = std::time::Duration::from_millis(500);

pub(super) struct QueueWindow {
    language: Language,
    pub(super) agent: Agent,
    /// The run the window was opened for; another one in a later view
    /// means the agent was restarted.
    run: String,
    view: Value,
    selected: usize,
    mode: Mode,
    /// An editor whose save is on its way: restored if the save fails.
    pending: Option<Mode>,
    /// After a save, keys are ignored until typing pauses: someone who
    /// typed on must not remove or send with the letters and Enter.
    quiet_until: Option<std::time::Instant>,
    notice: Option<String>,
    hits: Vec<(Rect, Hit)>,
    pressed: Option<Hit>,
}

#[derive(Clone)]
enum Mode {
    List,
    /// Writing a new prompt (`id` None) or changing one.
    Edit {
        id: Option<i64>,
        revision: i64,
        text: String,
    },
    Attach {
        id: i64,
        revision: i64,
        text: String,
    },
    Remove {
        id: i64,
        revision: i64,
    },
    Send {
        id: i64,
        revision: i64,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Hit {
    Row(usize),
    Key(KeyCode),
}

pub(super) enum QueueOutcome {
    None,
    Close,
    Op(QueueOp),
}

/// One listed item and whether it belongs to an earlier run.
struct Entry<'a> {
    item: &'a Value,
    held: bool,
}

impl QueueWindow {
    pub(super) fn new(language: Language, agent: Agent, view: Value) -> Self {
        let run = view["run"].as_str().unwrap_or(&agent.run).to_owned();
        Self {
            language,
            agent,
            run,
            view,
            selected: 0,
            mode: Mode::List,
            pending: None,
            quiet_until: None,
            notice: None,
            hits: Vec::new(),
            pressed: None,
        }
    }

    fn word(&self, english: &'static str, korean: &'static str) -> &'static str {
        match self.language {
            Language::English => english,
            Language::Korean => korean,
        }
    }

    pub(super) fn pane(&self) -> &str {
        &self.agent.pane_id
    }

    /// A newer list: the selection stays on the same item where it can.
    pub(super) fn refresh(&mut self, view: Value) {
        let id = self.current().and_then(|entry| entry.item["id"].as_i64());
        self.view = view;
        if let Some(index) = id.and_then(|id| {
            self.entries()
                .iter()
                .position(|entry| entry.item["id"].as_i64() == Some(id))
        }) {
            self.selected = index;
        }
        self.selected = self.selected.min(self.entries().len().saturating_sub(1));
    }

    /// The result of a change: shown here, whatever the agent runs now. A
    /// failed save gives the text back.
    pub(super) fn finished(&mut self, result: Result<(), &str>, notice: String) {
        let saved = self.pending.take();
        match (result, saved) {
            (Ok(()), Some(_)) => {
                self.quiet_until = Some(std::time::Instant::now() + QUIET);
            }
            // The item changed, was sent, or belongs to an earlier run: the
            // text can only go in as a new prompt.
            (Err(error), Some(Mode::Edit { text, .. }))
                if ["queue_stale", "queue_held", "queue_not_staged"]
                    .iter()
                    .any(|code| error.starts_with(code)) =>
            {
                self.mode = Mode::Edit {
                    id: None,
                    revision: 0,
                    text,
                };
                self.notice = Some(format!(
                    "{} {}",
                    crate::managed::failure::human(error),
                    self.word(
                        "Enter saves it as a new prompt.",
                        "Enter로 새 prompt로 저장합니다."
                    )
                ));
                return;
            }
            (Err(_), Some(pending @ Mode::Edit { .. })) => self.mode = pending,
            _ => {}
        }
        self.notice = Some(notice);
    }

    /// `Show` came back: open the editor on the whole body.
    pub(super) fn edit_body(&mut self, value: &Value) {
        let (Some(id), Some(body)) = (value["id"].as_i64(), value["body"].as_str()) else {
            return;
        };
        if matches!(self.mode, Mode::List)
            && self.current().and_then(|entry| entry.item["id"].as_i64()) == Some(id)
        {
            self.mode = Mode::Edit {
                id: Some(id),
                revision: value["revision"].as_i64().unwrap_or(0),
                text: body.to_owned(),
            };
        }
    }

    fn run_changed(&self) -> bool {
        self.view["run"].as_str().is_some_and(|run| run != self.run)
    }

    fn entries(&self) -> Vec<Entry<'_>> {
        let list = |key: &str, held: bool| {
            self.view[key]
                .as_array()
                .into_iter()
                .flatten()
                .map(move |item| Entry { item, held })
        };
        list("items", false).chain(list("held", true)).collect()
    }

    fn current(&self) -> Option<Entry<'_>> {
        self.entries().into_iter().nth(self.selected)
    }

    /// The selected item, if it is one of this run's prompts in one of
    /// `states`.
    fn of_run(&self, states: &[&str]) -> Option<(i64, i64)> {
        let entry = self.current()?;
        let state = entry.item["state"].as_str().unwrap_or_default();
        (!entry.held && states.contains(&state) && !self.run_changed()).then(|| {
            (
                entry.item["id"].as_i64().unwrap_or(0),
                entry.item["revision"].as_i64().unwrap_or(0),
            )
        })
    }

    fn staged(&self) -> Option<(i64, i64)> {
        self.of_run(&["staged"])
    }

    pub(super) fn handle(&mut self, event: Event) -> QueueOutcome {
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => self.key(key),
            Event::Paste(pasted) => {
                match &mut self.mode {
                    Mode::Edit { text, .. } => push(text, &pasted, BODY_LIMIT, true),
                    Mode::Attach { text, .. } => push(text, &pasted, PATH_LIMIT, false),
                    _ => {}
                }
                QueueOutcome::None
            }
            Event::Mouse(mouse) => {
                let target = self
                    .hits
                    .iter()
                    .rev()
                    .find(|(rect, _)| {
                        mouse.column >= rect.x
                            && mouse.column < rect.right()
                            && mouse.row >= rect.y
                            && mouse.row < rect.bottom()
                    })
                    .map(|(_, hit)| *hit);
                if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
                    self.pressed = target;
                    return QueueOutcome::None;
                }
                if !matches!(mouse.kind, MouseEventKind::Up(MouseButton::Left))
                    || self.pressed.take() != target
                {
                    return QueueOutcome::None;
                }
                match target {
                    Some(Hit::Row(index)) if matches!(self.mode, Mode::List) => {
                        self.selected = index;
                        QueueOutcome::None
                    }
                    Some(Hit::Key(code)) => self.key(KeyEvent::new(code, KeyModifiers::NONE)),
                    _ => QueueOutcome::None,
                }
            }
            _ => QueueOutcome::None,
        }
    }

    fn key(&mut self, key: KeyEvent) -> QueueOutcome {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        // Keys typed on after Enter must not act on the list meanwhile, nor
        // until typing pauses after the save; Esc still closes the window.
        let now = std::time::Instant::now();
        let typing_on = self.quiet_until.is_some_and(|until| now < until);
        if self.pending.is_some() || typing_on {
            if key.code == KeyCode::Esc {
                return QueueOutcome::Close;
            }
            if typing_on {
                self.quiet_until = Some(now + QUIET);
            }
            return QueueOutcome::None;
        }
        self.quiet_until = None;
        self.notice = None;
        match std::mem::replace(&mut self.mode, Mode::List) {
            Mode::List => self.list_key(key),
            Mode::Edit {
                id,
                revision,
                mut text,
            } => match key.code {
                KeyCode::Esc => QueueOutcome::None,
                KeyCode::Enter if !control => {
                    if text.trim().is_empty() {
                        self.notice = Some(
                            self.word("Write the prompt first.", "먼저 prompt를 쓰세요.")
                                .into(),
                        );
                        self.mode = Mode::Edit { id, revision, text };
                        return QueueOutcome::None;
                    }
                    self.pending = Some(Mode::Edit {
                        id,
                        revision,
                        text: text.clone(),
                    });
                    QueueOutcome::Op(match id {
                        None => QueueOp::Add { body: text },
                        Some(id) => QueueOp::Edit {
                            id,
                            body: text,
                            revision,
                        },
                    })
                }
                code => {
                    edit(&mut text, code, control, BODY_LIMIT, true);
                    self.mode = Mode::Edit { id, revision, text };
                    QueueOutcome::None
                }
            },
            Mode::Attach {
                id,
                revision,
                mut text,
            } => match key.code {
                KeyCode::Esc => QueueOutcome::None,
                KeyCode::Enter if !text.trim().is_empty() => {
                    self.pending = Some(Mode::Attach {
                        id,
                        revision,
                        text: text.clone(),
                    });
                    QueueOutcome::Op(QueueOp::Attach {
                        id,
                        path: text,
                        revision,
                    })
                }
                code => {
                    edit(&mut text, code, control, PATH_LIMIT, false);
                    self.mode = Mode::Attach { id, revision, text };
                    QueueOutcome::None
                }
            },
            Mode::Remove { id, revision } => match key.code {
                KeyCode::Enter => QueueOutcome::Op(QueueOp::Remove { id, revision }),
                KeyCode::Esc => QueueOutcome::None,
                _ => {
                    self.mode = Mode::Remove { id, revision };
                    QueueOutcome::None
                }
            },
            Mode::Send { id, revision } => match key.code {
                KeyCode::Enter => QueueOutcome::Op(QueueOp::Send { id, revision }),
                KeyCode::Esc => QueueOutcome::None,
                _ => {
                    self.mode = Mode::Send { id, revision };
                    QueueOutcome::None
                }
            },
        }
    }

    fn list_key(&mut self, key: KeyEvent) -> QueueOutcome {
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
        {
            return QueueOutcome::None;
        }
        let count = self.entries().len();
        let not_here = self.word(
            "Only a staged prompt of this run can change; press r to queue a copy.",
            "이 run의 대기 중인 prompt만 바꿀 수 있습니다. r로 복사해 넣으세요.",
        );
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return QueueOutcome::Close,
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(count.saturating_sub(1));
            }
            KeyCode::Char('n') if !self.run_changed() => {
                self.mode = Mode::Edit {
                    id: None,
                    revision: 0,
                    text: String::new(),
                };
            }
            KeyCode::Enter => match self.staged() {
                Some((id, _)) => return QueueOutcome::Op(QueueOp::Show { id }),
                None => self.notice = Some(not_here.into()),
            },
            KeyCode::Char('a') => match self.staged() {
                Some((id, revision)) => {
                    self.mode = Mode::Attach {
                        id,
                        revision,
                        text: String::new(),
                    };
                }
                None => self.notice = Some(not_here.into()),
            },
            // The store places by rank among the run's items: this run's
            // items come first in the list, so the rank is the row.
            KeyCode::Char(code @ ('K' | 'J')) => match self.staged() {
                Some((id, revision)) => {
                    let rank = self.selected as i64 + 1;
                    let position = if code == 'K' { rank - 1 } else { rank + 1 };
                    return QueueOutcome::Op(QueueOp::Move {
                        id,
                        position: position.max(1),
                        revision,
                    });
                }
                None => self.notice = Some(not_here.into()),
            },
            KeyCode::Char('x') => {
                if let Some(entry) = self.current() {
                    let id = entry.item["id"].as_i64().unwrap_or(0);
                    let revision = entry.item["revision"].as_i64().unwrap_or(0);
                    // Being sent or of unknown outcome: a person settles it first.
                    let state = entry.item["state"].as_str().unwrap_or_default();
                    let removable = entry.held || matches!(state, "staged" | "sent" | "not_sent");
                    if removable {
                        self.mode = Mode::Remove { id, revision };
                    } else {
                        self.notice = Some(not_here.into());
                    }
                }
            }
            // A cut-off send is sent again to settle it.
            KeyCode::Char('s') => match self.of_run(&["staged", "sending"]) {
                Some((id, revision)) => self.mode = Mode::Send { id, revision },
                None => self.notice = Some(not_here.into()),
            },
            // A copy only of what surely was or was not sent.
            KeyCode::Char('r') => {
                if let Some(entry) = self.current()
                    && (entry.held
                        || matches!(entry.item["state"].as_str(), Some("sent" | "not_sent")))
                    && !self.run_changed()
                {
                    let id = entry.item["id"].as_i64().unwrap_or(0);
                    return QueueOutcome::Op(QueueOp::AddFrom { id });
                }
            }
            _ => {}
        }
        QueueOutcome::None
    }

    fn state_label(&self, state: &str) -> &'static str {
        match state {
            "staged" => self.word("queued", "대기"),
            "sending" => self.word("sending", "보내는 중"),
            "sent" => self.word("sent", "보냄"),
            "unknown" => self.word("unknown", "결과 모름"),
            "not_sent" => self.word("not sent", "안 보냄"),
            _ => self.word("?", "?"),
        }
    }

    fn row(&self, entry: &Entry<'_>) -> String {
        let item = entry.item;
        let preview = item["preview"]
            .as_str()
            .map(|text| text.lines().next().unwrap_or_default().to_owned())
            .unwrap_or_else(|| self.word("(body cleared)", "(본문 지워짐)").to_owned());
        let attachments = item["attachments"].as_array().map_or(0, Vec::len);
        let clip = if attachments > 0 {
            format!(" +{attachments}")
        } else {
            String::new()
        };
        let place = if entry.held {
            self.word("earlier", "이전").to_owned()
        } else {
            item["position"].as_i64().unwrap_or(0).to_string()
        };
        format!(
            "{place:>3} {:<9} {}{clip}",
            self.state_label(item["state"].as_str().unwrap_or_default()),
            plain(&preview)
        )
    }

    fn detail(&self, width: usize) -> Vec<(String, bool)> {
        let mut rows = Vec::new();
        let mut push = |text: &str, muted: bool| {
            rows.extend(wrap(text, width).into_iter().map(|line| (line, muted)));
        };
        match &self.mode {
            Mode::Edit { text, id, .. } => {
                push(
                    if id.is_some() {
                        self.word("Edit the prompt:", "prompt 고치기:")
                    } else {
                        self.word("New prompt:", "새 prompt:")
                    },
                    true,
                );
                let lines: Vec<&str> = text.split('\n').collect();
                let count = lines.len();
                for (index, line) in lines.iter().enumerate() {
                    let cursor = if index + 1 == count { "▏" } else { "" };
                    for wrapped in wrap(&format!("{}{cursor}", plain(line)), width) {
                        rows.push((wrapped, false));
                    }
                }
                return rows;
            }
            Mode::Attach { text, .. } => {
                push(
                    self.word(
                        "Path to attach (drop a file or type it; relative to the agent's directory):",
                        "붙일 경로(파일을 끌어 놓거나 입력, 상대 경로는 agent 디렉터리 기준):",
                    ),
                    true,
                );
                push(&format!("{}▏", plain(text)), false);
                return rows;
            }
            Mode::Remove { .. } => {
                push(
                    self.word(
                        "Remove this prompt? Its text is deleted from the store.",
                        "이 prompt를 지울까요? 본문이 store에서 지워집니다.",
                    ),
                    false,
                );
                return rows;
            }
            Mode::Send { .. } => {
                push(
                    self.word(
                        "Send this prompt to the agent now? It must be idle.",
                        "이 prompt를 지금 agent에 보낼까요? agent가 대기 상태여야 합니다.",
                    ),
                    false,
                );
                return rows;
            }
            Mode::List => {}
        }
        if self.run_changed() {
            push(
                self.word(
                    "The agent was restarted: these prompts belong to its earlier run and are not sent.",
                    "agent가 다시 시작됐습니다. 이 prompt들은 이전 run의 것이라 보내지 않습니다.",
                ),
                false,
            );
        }
        if let Some(entry) = self.current() {
            let item = entry.item;
            if let Some(preview) = item["preview"].as_str() {
                for line in preview.lines().take(6) {
                    push(&plain(line), false);
                }
                if item["bytes"].as_u64().unwrap_or(0) as usize > preview.len() {
                    push("…", true);
                }
            }
            for attachment in item["attachments"].as_array().into_iter().flatten() {
                push(
                    &format!(
                        "{} {} ({})",
                        self.word("path:", "경로:"),
                        plain(attachment["path"].as_str().unwrap_or_default()),
                        self.word("checked", "확인됨")
                    ),
                    true,
                );
            }
            if let Some(note) = item["note"].as_str() {
                push(
                    &format!(
                        "{} {}",
                        self.word("Last try:", "마지막 시도:"),
                        plain(crate::managed::failure::human(note))
                    ),
                    true,
                );
            }
        } else {
            push(
                self.word(
                    "Nothing is queued. Press n to write a prompt.",
                    "대기 중인 prompt가 없습니다. n으로 새로 쓰세요.",
                ),
                true,
            );
        }
        rows
    }

    fn buttons(&self) -> Vec<(String, KeyCode)> {
        let label = |key: &str, text: &str| format!(" {key} {text} ");
        match &self.mode {
            Mode::Edit { .. } => vec![
                (label("Esc", self.word("Cancel", "취소")), KeyCode::Esc),
                (label("^J", self.word("New line", "줄 바꿈")), KeyCode::Null),
                (label("Enter", self.word("Save", "저장")), KeyCode::Enter),
            ],
            Mode::Attach { .. } => vec![
                (label("Esc", self.word("Cancel", "취소")), KeyCode::Esc),
                (
                    label("Enter", self.word("Attach", "붙이기")),
                    KeyCode::Enter,
                ),
            ],
            Mode::Remove { .. } => vec![
                (label("Esc", self.word("Back", "뒤로")), KeyCode::Esc),
                (
                    label("Enter", self.word("Remove", "지우기")),
                    KeyCode::Enter,
                ),
            ],
            Mode::Send { .. } => vec![
                (label("Esc", self.word("Back", "뒤로")), KeyCode::Esc),
                (label("Enter", self.word("Send", "보내기")), KeyCode::Enter),
            ],
            Mode::List => vec![
                (label("Esc", self.word("Close", "닫기")), KeyCode::Esc),
                (label("n", self.word("New", "새로")), KeyCode::Char('n')),
                (
                    label("x", self.word("Remove", "지우기")),
                    KeyCode::Char('x'),
                ),
                (label("s", self.word("Send", "보내기")), KeyCode::Char('s')),
            ],
        }
    }

    pub(super) fn draw(&mut self, frame: &mut Frame, theme: Theme) {
        let area = frame.area();
        let width = area.width.saturating_sub(4).clamp(1, 84).min(area.width);
        let text_width = width.saturating_sub(4) as usize;
        let room = area.height.saturating_sub(2).max(7).min(area.height);
        // Borders, a gap, the buttons and the keep-note.
        let space = room.saturating_sub(5) as usize;
        let entries = self.entries();
        let listing = matches!(self.mode, Mode::List);
        let list = if listing {
            entries
                .len()
                .clamp(1, LIST_ROWS)
                .min(space.saturating_sub(2).max(1))
        } else {
            0
        };
        let detail = self.detail(text_width);
        let notice = self
            .notice
            .as_deref()
            .map(|notice| wrap(notice, text_width))
            .unwrap_or_default();
        let gap = usize::from(listing);
        let below = space.saturating_sub(list + gap);
        let notice_rows = notice.len().min(below);
        let detail_rows = detail.len().min(below - notice_rows);
        let height = (list + gap + detail_rows + notice_rows + 5).min(room as usize) as u16;
        let modal = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        );
        let (panel, text, muted, accent, accent_text) = dialog_colors(theme);
        frame.render_widget(Clear, modal);
        let title = format!(
            " {} · {} ",
            self.word("Prompt queue", "prompt 대기열"),
            self.agent.name
        );
        frame.render_widget(
            Block::default()
                .borders(Borders::ALL)
                .title(ellipsize(&title, width.saturating_sub(2) as usize))
                .border_style(Style::default().fg(accent))
                .style(Style::default().bg(panel).fg(text)),
            modal,
        );
        let mut hits = Vec::new();
        let inner = Rect::new(
            modal.x + 2,
            modal.y + 1,
            modal.width.saturating_sub(4),
            modal.height.saturating_sub(2),
        );
        let mut y = inner.y;
        let bottom = modal.bottom().saturating_sub(3);
        if listing {
            let first = (self.selected + 1).saturating_sub(list);
            if entries.is_empty() {
                y += 1;
            }
            for (index, entry) in entries.iter().enumerate().skip(first).take(list) {
                if y >= bottom {
                    break;
                }
                let selected = index == self.selected;
                let style = if selected {
                    Style::default().fg(accent_text).bg(accent)
                } else if entry.held {
                    Style::default().fg(muted).bg(panel)
                } else {
                    Style::default().fg(text).bg(panel)
                };
                let rect = Rect::new(inner.x, y, inner.width, 1);
                frame.render_widget(
                    Paragraph::new(ellipsize(&self.row(entry), text_width)).style(style),
                    rect,
                );
                hits.push((rect, Hit::Row(index)));
                y += 1;
            }
            y += 1;
        }
        // The editor keeps its end in view.
        let skip = if listing {
            0
        } else {
            detail.len().saturating_sub(detail_rows)
        };
        for (line, quiet) in detail.into_iter().skip(skip).take(detail_rows) {
            if y >= bottom {
                break;
            }
            frame.render_widget(
                Paragraph::new(line).style(
                    Style::default()
                        .fg(if quiet { muted } else { text })
                        .bg(panel),
                ),
                Rect::new(inner.x, y, inner.width, 1),
            );
            y += 1;
        }
        for line in notice.into_iter().take(notice_rows) {
            if y >= bottom {
                break;
            }
            frame.render_widget(
                Paragraph::new(line).style(Style::default().fg(accent).bg(panel)),
                Rect::new(inner.x, y, inner.width, 1),
            );
            y += 1;
        }
        let keep = self.word(
            "Bodies stay in this machine's operation store until removed.",
            "본문은 지울 때까지 이 기기의 operation store에 남습니다.",
        );
        frame.render_widget(
            Paragraph::new(ellipsize(keep, text_width)).style(Style::default().fg(muted).bg(panel)),
            Rect::new(inner.x, modal.bottom().saturating_sub(3), inner.width, 1),
        );
        let button_y = modal.bottom().saturating_sub(2);
        let mut right = inner.right();
        let buttons = self.buttons();
        let last = buttons.len().saturating_sub(1);
        for (index, (label, code)) in buttons.into_iter().enumerate().rev() {
            let label_width = cell_width(&label) as u16;
            if right < inner.x + label_width {
                break;
            }
            let rect = Rect::new(right - label_width, button_y, label_width, 1);
            let style = if index == last {
                Style::default()
                    .bg(accent)
                    .fg(accent_text)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(text).bg(panel)
            };
            frame.render_widget(Paragraph::new(label).style(style), rect);
            if code != KeyCode::Null {
                hits.push((rect, Hit::Key(code)));
            }
            right = rect.x.saturating_sub(1);
        }
        self.hits = hits;
    }
}

/// Typing into a field. `lines` keeps line breaks (Ctrl-J adds one).
fn edit(text: &mut String, code: KeyCode, control: bool, limit: usize, lines: bool) {
    match code {
        KeyCode::Backspace => {
            if let Some((end, _)) = text.grapheme_indices(true).next_back() {
                text.truncate(end);
            }
        }
        KeyCode::Char('u') if control => text.clear(),
        KeyCode::Char('j') if control && lines => push(text, "\n", limit, true),
        KeyCode::Tab if lines => push(text, "\t", limit, true),
        KeyCode::Char(character) if !control => push(text, &character.to_string(), limit, lines),
        _ => {}
    }
}

/// Adds typed or pasted text: line breaks (CRLF and CR as LF) and tabs only
/// where `lines`, no other control characters, up to `limit` bytes.
fn push(text: &mut String, added: &str, limit: usize, lines: bool) {
    let added = added.replace("\r\n", "\n").replace('\r', "\n");
    for character in added.chars() {
        let kept = !character.is_control() || (lines && matches!(character, '\n' | '\t'));
        if !kept {
            continue;
        }
        if text.len() + character.len_utf8() > limit {
            break;
        }
        text.push(character);
    }
}

/// Text drawn as is: control characters become spaces.
fn plain(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn agent() -> Agent {
        serde_json::from_value(json!({
            "id": "local:%1", "name": "cx", "provider": "codex", "pane_id": "%1",
            "window_id": "@1", "workspace": "w", "cwd": "/", "boot": "b", "generation": "1",
            "run": "r1", "process": "running", "state": "idle", "session_id": null,
            "evidence": {}, "seen": false, "revision": "1", "returned_idle": false
        }))
        .expect("agent")
    }

    fn view() -> Value {
        json!({"run": "r1", "items": [
            {"id": 1, "position": 1, "state": "staged", "revision": 3, "preview": "first", "attachments": []},
            {"id": 2, "position": 2, "state": "sent", "revision": 2, "preview": "second", "attachments": []}
        ], "held": [
            {"id": 9, "position": 1, "state": "staged", "revision": 1, "preview": "old", "attachments": []}
        ]})
    }

    fn key(window: &mut QueueWindow, code: KeyCode, modifiers: KeyModifiers) -> QueueOutcome {
        window.handle(Event::Key(KeyEvent::new(code, modifiers)))
    }

    #[test]
    fn a_new_prompt_keeps_pasted_lines_and_ctrl_j_adds_one() {
        let mut window = QueueWindow::new(Language::English, agent(), view());
        key(&mut window, KeyCode::Char('n'), KeyModifiers::NONE);
        window.handle(Event::Paste("line one\r\nline\ttwo\u{1b}".into()));
        key(&mut window, KeyCode::Char('j'), KeyModifiers::CONTROL);
        key(&mut window, KeyCode::Char('x'), KeyModifiers::NONE);
        match key(&mut window, KeyCode::Enter, KeyModifiers::NONE) {
            QueueOutcome::Op(QueueOp::Add { body }) => assert_eq!(body, "line one\nline\ttwo\nx"),
            _ => panic!("expected add"),
        }
    }

    #[test]
    fn send_carries_the_shown_revision_and_only_staged_items_of_this_run_change() {
        let mut window = QueueWindow::new(Language::English, agent(), view());
        assert!(matches!(
            key(&mut window, KeyCode::Char('s'), KeyModifiers::NONE),
            QueueOutcome::None
        ));
        match key(&mut window, KeyCode::Enter, KeyModifiers::NONE) {
            QueueOutcome::Op(QueueOp::Send { id, revision }) => assert_eq!((id, revision), (1, 3)),
            _ => panic!("expected send"),
        }
        // A sent item: no send, but a copy can be queued.
        key(&mut window, KeyCode::Down, KeyModifiers::NONE);
        assert!(matches!(
            key(&mut window, KeyCode::Char('s'), KeyModifiers::NONE),
            QueueOutcome::None
        ));
        assert!(window.notice.is_some());
        assert!(matches!(
            key(&mut window, KeyCode::Char('r'), KeyModifiers::NONE),
            QueueOutcome::Op(QueueOp::AddFrom { id: 2 })
        ));
        // An earlier run's item is removed after a confirmation.
        key(&mut window, KeyCode::Down, KeyModifiers::NONE);
        assert!(matches!(
            key(&mut window, KeyCode::Char('x'), KeyModifiers::NONE),
            QueueOutcome::None
        ));
        assert!(matches!(
            key(&mut window, KeyCode::Enter, KeyModifiers::NONE),
            QueueOutcome::Op(QueueOp::Remove { id: 9, revision: 1 })
        ));
    }

    #[test]
    fn a_failed_save_gives_the_text_back_and_keys_wait_for_it() {
        let mut window = QueueWindow::new(Language::English, agent(), view());
        key(&mut window, KeyCode::Char('n'), KeyModifiers::NONE);
        window.handle(Event::Paste("kept text".into()));
        assert!(matches!(
            key(&mut window, KeyCode::Enter, KeyModifiers::NONE),
            QueueOutcome::Op(QueueOp::Add { .. })
        ));
        // Typing on before the result does nothing, `s` included.
        assert!(matches!(
            key(&mut window, KeyCode::Char('s'), KeyModifiers::NONE),
            QueueOutcome::None
        ));
        assert!(matches!(window.mode, Mode::List));
        window.finished(Err("queue_full: no room"), "queue_full".into());
        match &window.mode {
            Mode::Edit { text, .. } => assert_eq!(text, "kept text"),
            _ => panic!("expected the editor back"),
        }
        // A save that worked leaves the list.
        assert!(matches!(
            key(&mut window, KeyCode::Enter, KeyModifiers::NONE),
            QueueOutcome::Op(QueueOp::Add { .. })
        ));
        window.finished(Ok(()), "Queued".into());
        assert!(matches!(window.mode, Mode::List));
        assert!(window.pending.is_none());
        // Typing on right after the save: `x` and Enter do nothing.
        assert!(matches!(
            key(&mut window, KeyCode::Char('x'), KeyModifiers::NONE),
            QueueOutcome::None
        ));
        assert!(matches!(
            key(&mut window, KeyCode::Enter, KeyModifiers::NONE),
            QueueOutcome::None
        ));
        assert!(matches!(window.mode, Mode::List));
    }

    #[test]
    fn an_edit_of_an_item_that_moved_on_comes_back_as_a_new_prompt() {
        let mut window = QueueWindow::new(Language::English, agent(), view());
        window.edit_body(&json!({"id": 1, "revision": 3, "body": "my edit"}));
        assert!(matches!(
            key(&mut window, KeyCode::Enter, KeyModifiers::NONE),
            QueueOutcome::Op(QueueOp::Edit { id: 1, .. })
        ));
        window.finished(Err("queue_stale: changed"), "changed".into());
        match key(&mut window, KeyCode::Enter, KeyModifiers::NONE) {
            QueueOutcome::Op(QueueOp::Add { body }) => assert_eq!(body, "my edit"),
            _ => panic!("expected a new prompt"),
        }
    }

    #[test]
    fn after_a_restart_nothing_of_the_old_run_is_sent() {
        let mut window = QueueWindow::new(Language::English, agent(), view());
        let mut later = view();
        later["run"] = json!("r2");
        window.refresh(later);
        assert!(matches!(
            key(&mut window, KeyCode::Char('s'), KeyModifiers::NONE),
            QueueOutcome::None
        ));
        assert!(matches!(
            key(&mut window, KeyCode::Char('n'), KeyModifiers::NONE),
            QueueOutcome::None
        ));
        let rows = window.detail(80);
        assert!(rows.iter().any(|(row, _)| row.contains("restarted")));
    }
}
