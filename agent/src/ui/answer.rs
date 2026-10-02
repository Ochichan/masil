//! The dialog that answers an OpenCode agent's pending permissions and
//! questions through masil, for agents started with `--answers`.
//!
//! It shows what the provider listed when it opened. The answer itself is
//! checked again against the provider's pending list, so a request answered
//! in the pane meanwhile comes back as expired.

use super::{
    managed::dialog_colors,
    model::{Language, Theme},
    view::{cell_width, ellipsize, wrap},
};
use crate::managed::{Agent, answer::Reply};
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

const LIST_ROWS: usize = 5;
const TEXT_LIMIT: usize = 4096;
const DIFF_LINES: usize = 8;
/// Metadata a person reads to decide, in this order.
const METADATA: [&str; 8] = [
    "description",
    "command",
    "filePath",
    "filepath",
    "path",
    "url",
    "query",
    "pattern",
];

pub(super) struct AnswerDialog {
    language: Language,
    pub(super) agent: Agent,
    requests: Vec<Value>,
    selected: usize,
    stage: Stage,
    notice: Option<&'static str>,
    hits: Vec<(Rect, Hit)>,
    pressed: Option<Hit>,
}

enum Stage {
    List,
    /// Questions of one request, asked in turn.
    Question {
        index: usize,
        cursor: usize,
        answers: Vec<String>,
        text: String,
    },
    Reject,
    /// Confirming an `always` allow.
    Always,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Hit {
    Request(usize),
    Choice(usize),
    Key(KeyCode),
}

pub(super) enum Outcome {
    None,
    Cancel,
    Submit { request: String, reply: Reply },
}

#[derive(Clone, Copy)]
enum Tone {
    Text,
    Muted,
    Selected,
}

struct Row {
    text: String,
    tone: Tone,
    hit: Option<Hit>,
}

impl Row {
    fn new(text: String, tone: Tone) -> Self {
        Self {
            text,
            tone,
            hit: None,
        }
    }
}

impl AnswerDialog {
    pub(super) fn new(language: Language, agent: Agent, requests: Vec<Value>) -> Self {
        Self {
            language,
            agent,
            requests,
            selected: 0,
            stage: Stage::List,
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

    fn current(&self) -> Option<&Value> {
        self.requests.get(self.selected)
    }

    fn is_permission(&self) -> bool {
        self.current()
            .is_some_and(|request| request["kind"] == "permission")
    }

    /// Other permissions of the selected request's session.
    fn session_peers(&self) -> usize {
        let Some(request) = self.current() else {
            return 0;
        };
        self.requests
            .iter()
            .filter(|other| {
                other["kind"] == "permission"
                    && other["session_id"] == request["session_id"]
                    && other["id"] != request["id"]
            })
            .count()
    }

    fn questions(&self) -> Vec<Value> {
        self.current()
            .and_then(|request| request["questions"].as_array().cloned())
            .unwrap_or_default()
    }

    fn submit(&self, reply: Reply) -> Outcome {
        match self.current().and_then(|request| request["id"].as_str()) {
            Some(id) => Outcome::Submit {
                request: id.to_owned(),
                reply,
            },
            None => Outcome::Cancel,
        }
    }

    pub(super) fn handle(&mut self, event: Event) -> Outcome {
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => self.key(key),
            Event::Paste(pasted) => {
                if let Stage::Question { text, .. } = &mut self.stage {
                    push_text(text, &pasted);
                }
                Outcome::None
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
                    return Outcome::None;
                }
                if !matches!(mouse.kind, MouseEventKind::Up(MouseButton::Left))
                    || self.pressed.take() != target
                {
                    return Outcome::None;
                }
                match target {
                    Some(Hit::Request(index)) if matches!(self.stage, Stage::List) => {
                        self.selected = index;
                        self.notice = None;
                    }
                    Some(Hit::Choice(index)) => {
                        if let Stage::Question { cursor, .. } = &mut self.stage {
                            *cursor = index;
                        }
                    }
                    Some(Hit::Key(code)) => {
                        return self.key(KeyEvent::new(code, KeyModifiers::NONE));
                    }
                    _ => {}
                }
                Outcome::None
            }
            _ => Outcome::None,
        }
    }

    fn key(&mut self, key: KeyEvent) -> Outcome {
        if key.modifiers.intersects(
            KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER | KeyModifiers::META,
        ) {
            if key.code == KeyCode::Char('u')
                && key.modifiers.contains(KeyModifiers::CONTROL)
                && let Stage::Question { text, .. } = &mut self.stage
            {
                text.clear();
            }
            return Outcome::None;
        }
        self.notice = None;
        match std::mem::replace(&mut self.stage, Stage::List) {
            Stage::List => self.list_key(key.code),
            Stage::Always => match key.code {
                KeyCode::Esc => Outcome::None,
                KeyCode::Enter => self.submit(Reply::Always),
                _ => {
                    self.stage = Stage::Always;
                    Outcome::None
                }
            },
            Stage::Reject => match key.code {
                KeyCode::Esc => Outcome::None,
                KeyCode::Enter => self.submit(if self.is_permission() {
                    Reply::Reject { message: None }
                } else {
                    Reply::RejectQuestion
                }),
                _ => {
                    self.stage = Stage::Reject;
                    Outcome::None
                }
            },
            Stage::Question {
                index,
                cursor,
                answers,
                text,
            } => self.question_key(key.code, index, cursor, answers, text),
        }
    }

    fn list_key(&mut self, code: KeyCode) -> Outcome {
        let question = !self.is_permission();
        match code {
            KeyCode::Esc | KeyCode::Char('q') => return Outcome::Cancel,
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(self.requests.len().saturating_sub(1));
            }
            KeyCode::Char('1') if !question => return self.submit(Reply::Once),
            KeyCode::Char('1') | KeyCode::Enter if question => self.begin_questions(),
            KeyCode::Enter => {
                self.notice = Some(self.word(
                    "Press 1 to allow once, 2 to always allow or 3 to reject.",
                    "1은 한 번 허용, 2는 항상 허용, 3은 거절입니다.",
                ));
            }
            // What `always` would also allow among the session's other
            // waiting permissions cannot be known; with none, it may go.
            KeyCode::Char('2') if !question => {
                if self.session_peers() > 0 {
                    self.notice = Some(self.word(
                        "Another permission of this session waits: answer it first, or answer in the pane.",
                        "같은 session에 다른 권한 요청이 있습니다. 그것을 먼저 처리하거나 창에서 답하세요.",
                    ));
                } else {
                    self.stage = Stage::Always;
                }
            }
            KeyCode::Char('3') if self.current().is_some() => self.stage = Stage::Reject,
            _ => {}
        }
        Outcome::None
    }

    fn begin_questions(&mut self) {
        let questions = self.questions();
        let answerable = !questions.is_empty()
            && questions.iter().all(|question| {
                question["multiple"] != true && (custom(question) || !choices(question).is_empty())
            });
        if answerable {
            self.stage = Stage::Question {
                index: 0,
                cursor: 0,
                answers: Vec::new(),
                text: String::new(),
            };
        } else {
            self.notice = Some(self.word(
                "This question takes several choices; answer it in the pane.",
                "여러 개를 고르는 질문이라 창에서 답합니다.",
            ));
        }
    }

    fn question_key(
        &mut self,
        code: KeyCode,
        index: usize,
        mut cursor: usize,
        mut answers: Vec<String>,
        mut text: String,
    ) -> Outcome {
        let questions = self.questions();
        let Some(question) = questions.get(index) else {
            return Outcome::None;
        };
        let options = choices(question);
        let entries = options.len() + usize::from(custom(question));
        let typing = cursor == options.len();
        let mut index = index;
        match code {
            KeyCode::Esc => return Outcome::None,
            KeyCode::Up => cursor = cursor.saturating_sub(1),
            KeyCode::Down => cursor = (cursor + 1).min(entries.saturating_sub(1)),
            KeyCode::Char('k') if !typing => cursor = cursor.saturating_sub(1),
            KeyCode::Char('j') if !typing => cursor = (cursor + 1).min(entries.saturating_sub(1)),
            KeyCode::Char(digit @ '1'..='9') if !typing => {
                let choice = digit as usize - '1' as usize;
                if choice < entries {
                    cursor = choice;
                }
            }
            KeyCode::Backspace if typing => {
                if let Some((end, _)) = text.grapheme_indices(true).next_back() {
                    text.truncate(end);
                }
            }
            KeyCode::Char(character) if typing => push_text(&mut text, &character.to_string()),
            KeyCode::Enter => {
                let answer = match options.get(cursor) {
                    Some((label, _)) => label.clone(),
                    None => text.trim().to_owned(),
                };
                if answer.is_empty() {
                    self.notice = Some(self.word("Type an answer first.", "먼저 답을 입력하세요."));
                } else {
                    answers.push(answer);
                    if answers.len() == questions.len() {
                        return self.submit(Reply::Answers(answers));
                    }
                    index += 1;
                    cursor = 0;
                    text.clear();
                }
            }
            _ => {}
        }
        self.stage = Stage::Question {
            index,
            cursor,
            answers,
            text,
        };
        Outcome::None
    }

    /// One line per request: its kind and what it asks.
    fn summary(&self, request: &Value) -> String {
        if request["kind"] == "permission" {
            let detail = request["patterns"]
                .as_array()
                .and_then(|patterns| patterns.first())
                .or_else(|| {
                    METADATA
                        .iter()
                        .map(|key| &request["metadata"][key])
                        .find(|value| value.is_string())
                })
                .map(plain)
                .unwrap_or_default();
            format!(
                "{} {} {detail}",
                self.word("Permission", "권한"),
                plain(&request["permission"])
            )
        } else {
            let question = &request["questions"][0];
            let header = if question["header"].as_str().is_some_and(|h| !h.is_empty()) {
                &question["header"]
            } else {
                &question["question"]
            };
            format!("{} {}", self.word("Question", "질문"), plain(header))
        }
    }

    /// The rows under the request list, and the row to keep in view.
    fn detail(&self, width: usize) -> (Vec<Row>, usize) {
        let mut rows = Vec::new();
        let push = |rows: &mut Vec<Row>, text: &str, tone: Tone| {
            rows.extend(
                wrap(text, width)
                    .into_iter()
                    .map(|line| Row::new(line, tone)),
            );
        };
        let Some(request) = self.current() else {
            return (rows, 0);
        };
        let mut focus = 0;
        match &self.stage {
            Stage::List if request["kind"] == "permission" => {
                let patterns = request["patterns"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(plain)
                    .collect::<Vec<_>>();
                let permission = plain(&request["permission"]);
                if patterns.is_empty() {
                    push(&mut rows, &permission, Tone::Text);
                } else {
                    push(
                        &mut rows,
                        &format!("{permission}: {}", patterns.join(", ")),
                        Tone::Text,
                    );
                }
                for key in METADATA {
                    if let Some(value) = request["metadata"][key].as_str() {
                        push(&mut rows, &format!("{key}: {value}"), Tone::Muted);
                    }
                }
                if let Some(diff) = request["metadata"]["diff"].as_str() {
                    for line in diff.lines().take(DIFF_LINES) {
                        rows.push(Row::new(ellipsize(line, width), Tone::Muted));
                    }
                }
                self.origin(&mut rows, request, width);
            }
            Stage::List => {
                for question in request["questions"].as_array().into_iter().flatten() {
                    push(&mut rows, &plain(&question["question"]), Tone::Text);
                    for (number, (label, description)) in choices(question).iter().enumerate() {
                        push(
                            &mut rows,
                            &format!("  {}. {label}  {description}", number + 1),
                            Tone::Muted,
                        );
                    }
                }
                self.origin(&mut rows, request, width);
            }
            Stage::Question {
                index,
                cursor,
                text,
                ..
            } => {
                let questions = self.questions();
                let Some(question) = questions.get(*index) else {
                    return (rows, 0);
                };
                push(
                    &mut rows,
                    &format!(
                        "{} {}/{} · {}",
                        self.word("Question", "질문"),
                        index + 1,
                        questions.len(),
                        plain(&question["header"])
                    ),
                    Tone::Muted,
                );
                push(&mut rows, &plain(&question["question"]), Tone::Text);
                let options = choices(question);
                for (number, (label, description)) in options.iter().enumerate() {
                    if number == *cursor {
                        focus = rows.len();
                    }
                    let mark = if number == *cursor { "›" } else { " " };
                    let line = if description.is_empty() {
                        format!("{mark} {}. {label}", number + 1)
                    } else {
                        format!("{mark} {}. {label}  {description}", number + 1)
                    };
                    rows.push(Row {
                        text: ellipsize(&line, width),
                        tone: if number == *cursor {
                            Tone::Selected
                        } else {
                            Tone::Text
                        },
                        hit: Some(Hit::Choice(number)),
                    });
                }
                if custom(question) {
                    let selected = *cursor == options.len();
                    if selected {
                        focus = rows.len();
                    }
                    let label = self.word("Your answer: ", "직접 입력: ");
                    let room = width.saturating_sub(cell_width(label) + 3);
                    let shown = if selected {
                        format!("{}▏", tail(text, room))
                    } else {
                        tail(text, room)
                    };
                    rows.push(Row {
                        text: ellipsize(
                            &format!("{} {label}{shown}", if selected { "›" } else { " " }),
                            width,
                        ),
                        tone: if selected { Tone::Selected } else { Tone::Text },
                        hit: Some(Hit::Choice(options.len())),
                    });
                }
            }
            Stage::Always => {
                push(
                    &mut rows,
                    &format!(
                        "{} {}",
                        self.word("Always allow:", "항상 허용:"),
                        self.summary(request)
                    ),
                    Tone::Text,
                );
                let patterns = request["always"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(plain)
                    .collect::<Vec<_>>();
                if !patterns.is_empty() {
                    push(
                        &mut rows,
                        &format!(
                            "{} {}",
                            self.word("Also allows:", "함께 허용:"),
                            patterns.join(", ")
                        ),
                        Tone::Text,
                    );
                }
                push(
                    &mut rows,
                    self.word(
                        "While this OpenCode runs, its other sessions' matching requests are allowed without asking.",
                        "이 OpenCode가 떠 있는 동안 다른 session의 같은 요청도 묻지 않고 허용됩니다.",
                    ),
                    Tone::Muted,
                );
            }
            Stage::Reject if request["kind"] == "permission" => {
                push(
                    &mut rows,
                    &format!(
                        "{} {}",
                        self.word("Reject:", "거절:"),
                        self.summary(request)
                    ),
                    Tone::Text,
                );
                // OpenCode rejects the session's other permissions with it;
                // the count comes first so a short window still shows it.
                let others = self
                    .requests
                    .iter()
                    .filter(|other| {
                        other["kind"] == "permission"
                            && other["session_id"] == request["session_id"]
                            && other["id"] != request["id"]
                    })
                    .collect::<Vec<_>>();
                if !others.is_empty() {
                    let count = others.len();
                    push(
                        &mut rows,
                        &match self.language {
                            Language::English => format!(
                                "OpenCode also rejects {count} more request(s) of this session:"
                            ),
                            Language::Korean => {
                                format!("같은 session의 요청 {count}개도 함께 거절됩니다:")
                            }
                        },
                        Tone::Text,
                    );
                    for other in others {
                        rows.push(Row::new(
                            ellipsize(&format!("  {}", self.summary(other)), width),
                            Tone::Muted,
                        ));
                    }
                }
                push(
                    &mut rows,
                    self.word(
                        "OpenCode tells the agent you rejected it.",
                        "OpenCode가 agent에게 거절했다고 알립니다.",
                    ),
                    Tone::Muted,
                );
            }
            Stage::Reject => {
                push(
                    &mut rows,
                    &format!(
                        "{} {}",
                        self.word("Dismiss:", "닫기:"),
                        self.summary(request)
                    ),
                    Tone::Text,
                );
                push(
                    &mut rows,
                    self.word(
                        "OpenCode tells the agent you dismissed the question.",
                        "OpenCode가 agent에게 질문을 닫았다고 알립니다.",
                    ),
                    Tone::Muted,
                );
            }
        }
        (rows, focus)
    }

    /// Requests from a session the agent started, such as a subagent's.
    fn origin(&self, rows: &mut Vec<Row>, request: &Value, width: usize) {
        if request["root"] == false {
            rows.extend(
                wrap(
                    self.word(
                        "From a subagent session.",
                        "하위 agent session의 요청입니다.",
                    ),
                    width,
                )
                .into_iter()
                .map(|line| Row::new(line, Tone::Muted)),
            );
        }
    }

    /// Buttons, the main one last.
    fn buttons(&self) -> Vec<(String, KeyCode)> {
        let label = |key: &str, text: &str| format!(" {key} {text} ");
        match &self.stage {
            Stage::List if self.is_permission() => vec![
                (label("Esc", self.word("Cancel", "취소")), KeyCode::Esc),
                (label("3", self.word("Reject", "거절")), KeyCode::Char('3')),
                (label("2", self.word("Always", "항상")), KeyCode::Char('2')),
                (
                    label("1", self.word("Allow once", "한 번 허용")),
                    KeyCode::Char('1'),
                ),
            ],
            Stage::List => vec![
                (label("Esc", self.word("Cancel", "취소")), KeyCode::Esc),
                (label("3", self.word("Dismiss", "닫기")), KeyCode::Char('3')),
                (
                    label("Enter", self.word("Answer", "답하기")),
                    KeyCode::Enter,
                ),
            ],
            Stage::Question { index, .. } => vec![
                (label("Esc", self.word("Back", "뒤로")), KeyCode::Esc),
                (
                    label(
                        "Enter",
                        if index + 1 == self.questions().len() {
                            self.word("Send", "보내기")
                        } else {
                            self.word("Next", "다음")
                        },
                    ),
                    KeyCode::Enter,
                ),
            ],
            Stage::Always => vec![
                (label("Esc", self.word("Back", "뒤로")), KeyCode::Esc),
                (
                    label("Enter", self.word("Always allow", "항상 허용")),
                    KeyCode::Enter,
                ),
            ],
            Stage::Reject => vec![
                (label("Esc", self.word("Back", "뒤로")), KeyCode::Esc),
                (
                    label(
                        "Enter",
                        if self.is_permission() {
                            self.word("Reject", "거절")
                        } else {
                            self.word("Dismiss", "닫기")
                        },
                    ),
                    KeyCode::Enter,
                ),
            ],
        }
    }

    pub(super) fn draw(&mut self, frame: &mut Frame, theme: Theme) {
        let area = frame.area();
        let width = area.width.saturating_sub(4).clamp(1, 76).min(area.width);
        let text_width = width.saturating_sub(4) as usize;
        let room = area.height.saturating_sub(2).max(6).min(area.height);
        // Borders, a gap above the buttons and the buttons.
        let space = room.saturating_sub(4) as usize;
        let (detail, focus) = self.detail(text_width);
        let notice = self
            .notice
            .map(|notice| wrap(notice, text_width))
            .unwrap_or_default();
        // The list is shown while choosing; a question or a confirmation
        // names its request and takes every row.
        let listing = matches!(self.stage, Stage::List);
        let list = if listing {
            self.requests
                .len()
                .min(LIST_ROWS)
                .min(space.saturating_sub(2).max(1))
        } else {
            0
        };
        let gap = usize::from(listing);
        let below = space.saturating_sub(list + gap);
        let notice_rows = notice.len().min(below);
        let detail_rows = detail.len().min(below - notice_rows);
        let height = (list + gap + detail_rows + notice_rows + 4).min(room as usize) as u16;
        let modal = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        );
        let (panel, text, muted, accent, accent_text) = dialog_colors(theme);
        let style = |tone: Tone| match tone {
            Tone::Text => Style::default().fg(text).bg(panel),
            Tone::Muted => Style::default().fg(muted).bg(panel),
            Tone::Selected => Style::default().fg(accent_text).bg(accent),
        };
        frame.render_widget(Clear, modal);
        let title = format!(
            " {} · {} ",
            self.word("Answer requests", "요청에 답하기"),
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
        self.hits.clear();
        let inner = Rect::new(
            modal.x + 2,
            modal.y + 1,
            modal.width.saturating_sub(4),
            modal.height.saturating_sub(2),
        );
        let mut y = inner.y;
        let bottom = modal.bottom().saturating_sub(2);
        let first = (self.selected + 1).saturating_sub(list);
        for (index, request) in self.requests.iter().enumerate().skip(first).take(list) {
            if y >= bottom {
                break;
            }
            let selected = index == self.selected;
            let line = format!(
                "{} {}",
                if selected { "›" } else { " " },
                self.summary(request)
            );
            let tone = if selected {
                Tone::Selected
            } else {
                Tone::Muted
            };
            let rect = Rect::new(inner.x, y, inner.width, 1);
            frame.render_widget(
                Paragraph::new(ellipsize(&line, text_width)).style(style(tone)),
                rect,
            );
            self.hits.push((rect, Hit::Request(index)));
            y += 1;
        }
        y += gap as u16;
        let skip = (focus + 1).saturating_sub(detail_rows);
        for row in detail.into_iter().skip(skip).take(detail_rows) {
            if y >= bottom {
                break;
            }
            let rect = Rect::new(inner.x, y, inner.width, 1);
            frame.render_widget(Paragraph::new(row.text).style(style(row.tone)), rect);
            if let Some(hit) = row.hit {
                self.hits.push((rect, hit));
            }
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
        // Buttons from the right; those that do not fit keep their keys.
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
            let button_style = if index == last {
                Style::default()
                    .bg(accent)
                    .fg(accent_text)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(text).bg(panel)
            };
            frame.render_widget(Paragraph::new(label).style(button_style), rect);
            self.hits.push((rect, Hit::Key(code)));
            right = rect.x.saturating_sub(1);
        }
    }
}

/// The options of one question: label and description.
fn choices(question: &Value) -> Vec<(String, String)> {
    question["options"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|option| {
            Some((
                option["label"].as_str()?.to_owned(),
                plain(&option["description"]),
            ))
        })
        .collect()
}

/// OpenCode lets a person type an answer unless the question says no.
fn custom(question: &Value) -> bool {
    question["custom"] != false
}

/// A string field as display text, without control characters.
fn plain(value: &Value) -> String {
    value
        .as_str()
        .unwrap_or_default()
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

fn push_text(text: &mut String, pasted: &str) {
    for character in pasted.chars().filter(|character| !character.is_control()) {
        if text.len() + character.len_utf8() > TEXT_LIMIT {
            break;
        }
        text.push(character);
    }
}

/// The end of `text` within `width` cells.
fn tail(text: &str, width: usize) -> String {
    let mut kept = Vec::new();
    let mut used = 0;
    for grapheme in text.graphemes(true).rev() {
        let next = cell_width(grapheme);
        if used + next > width {
            break;
        }
        kept.push(grapheme);
        used += next;
    }
    kept.reverse();
    kept.concat()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn agent() -> Agent {
        serde_json::from_value(json!({
            "id": "local:%1", "name": "oc", "provider": "opencode", "pane_id": "%1",
            "window_id": "@1", "workspace": "w", "cwd": "/", "boot": "b", "generation": "1",
            "run": "r", "process": "running", "state": "blocked", "session_id": null,
            "evidence": {}, "seen": false, "revision": "1", "returned_idle": false
        }))
        .expect("agent")
    }

    fn key(dialog: &mut AnswerDialog, code: KeyCode) -> Outcome {
        dialog.handle(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    fn permissions() -> Vec<Value> {
        vec![
            json!({"id": "per_1", "kind": "permission", "session_id": "s", "permission": "bash", "patterns": ["ls"]}),
            json!({"id": "per_2", "kind": "permission", "session_id": "s", "permission": "edit", "patterns": []}),
        ]
    }

    #[test]
    fn a_permission_is_allowed_once_with_one_key() {
        let mut dialog = AnswerDialog::new(Language::English, agent(), permissions());
        key(&mut dialog, KeyCode::Down);
        match key(&mut dialog, KeyCode::Char('1')) {
            Outcome::Submit {
                request,
                reply: Reply::Once,
            } => assert_eq!(request, "per_2"),
            _ => panic!("expected once"),
        }
    }

    #[test]
    fn always_is_confirmed_and_refused_while_the_session_has_another_request() {
        let mut dialog = AnswerDialog::new(Language::English, agent(), permissions());
        assert!(matches!(
            key(&mut dialog, KeyCode::Char('2')),
            Outcome::None
        ));
        assert!(matches!(dialog.stage, Stage::List));
        assert!(dialog.notice.is_some());
        let alone = vec![
            json!({"id": "per_1", "kind": "permission", "session_id": "s",
            "permission": "bash", "patterns": ["ls"], "always": ["ls *"]}),
        ];
        let mut dialog = AnswerDialog::new(Language::English, agent(), alone);
        key(&mut dialog, KeyCode::Char('2'));
        assert!(matches!(dialog.stage, Stage::Always));
        let (rows, _) = dialog.detail(80);
        assert!(rows.iter().any(|row| row.text.contains("ls *")));
        assert!(matches!(
            key(&mut dialog, KeyCode::Enter),
            Outcome::Submit {
                reply: Reply::Always,
                ..
            }
        ));
    }

    #[test]
    fn reject_asks_again_and_names_what_goes_with_it() {
        let mut dialog = AnswerDialog::new(Language::English, agent(), permissions());
        assert!(matches!(
            key(&mut dialog, KeyCode::Char('3')),
            Outcome::None
        ));
        let (rows, _) = dialog.detail(60);
        assert!(rows.iter().any(|row| row.text.contains("Permission edit")));
        assert!(matches!(key(&mut dialog, KeyCode::Esc), Outcome::None));
        assert!(matches!(dialog.stage, Stage::List));
        key(&mut dialog, KeyCode::Char('3'));
        assert!(matches!(
            key(&mut dialog, KeyCode::Enter),
            Outcome::Submit {
                reply: Reply::Reject { message: None },
                ..
            }
        ));
    }

    #[test]
    fn questions_are_asked_in_turn_with_choices_or_typed_text() {
        let request = json!({"id": "que_1", "kind": "question", "session_id": "s", "questions": [
            {"question": "Which?", "header": "Pick", "custom": false,
             "options": [{"label": "red", "description": ""}, {"label": "blue", "description": ""}]},
            {"question": "Why?", "header": "Reason", "options": []}
        ]});
        let mut dialog = AnswerDialog::new(Language::English, agent(), vec![request]);
        key(&mut dialog, KeyCode::Enter);
        key(&mut dialog, KeyCode::Char('2'));
        key(&mut dialog, KeyCode::Enter);
        // The second question has no options: typing goes to the answer,
        // and an empty answer is not sent.
        assert!(matches!(key(&mut dialog, KeyCode::Enter), Outcome::None));
        assert!(dialog.notice.is_some());
        for character in "j2k".chars() {
            key(&mut dialog, KeyCode::Char(character));
        }
        match key(&mut dialog, KeyCode::Enter) {
            Outcome::Submit {
                reply: Reply::Answers(answers),
                ..
            } => assert_eq!(answers, ["blue", "j2k"]),
            _ => panic!("expected answers"),
        }
    }

    #[test]
    fn a_question_with_several_choices_stays_in_the_pane() {
        let request = json!({"id": "que_1", "kind": "question", "questions": [
            {"question": "Which?", "multiple": true, "options": [{"label": "a"}]}]});
        let mut dialog = AnswerDialog::new(Language::English, agent(), vec![request]);
        key(&mut dialog, KeyCode::Enter);
        assert!(matches!(dialog.stage, Stage::List));
        assert!(dialog.notice.is_some());
    }

    #[test]
    fn provider_text_is_drawn_without_control_characters() {
        let request = json!({"id": "per_1", "kind": "permission", "session_id": "s",
            "permission": "bash\u{1b}[2J", "patterns": ["rm\u{1b}]52;c;x\u{7}"]});
        let dialog = AnswerDialog::new(Language::English, agent(), vec![request.clone()]);
        assert!(!dialog.summary(&request).chars().any(char::is_control));
        let (rows, _) = dialog.detail(60);
        assert!(
            rows.iter()
                .all(|row| !row.text.chars().any(char::is_control))
        );
    }
}
