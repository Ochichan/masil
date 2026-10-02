//! The changes window (`f`): what changed in the selected agent's working
//! tree, one file's diff, its checkpoints, and restoring a file from one
//! (docs/checkpoints.md). Restoring is files only, previewed first, and
//! confirmed with Enter.

use super::{
    managed::dialog_colors,
    model::{Language, Theme},
    view::{cell_width, ellipsize},
};
use crate::managed::{Agent, changes::ChangesOp};
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

const LIST_ROWS: usize = 8;
/// The pause after a result before keys act again, so a key typed while
/// waiting never confirms what just appeared.
const QUIET: std::time::Duration = std::time::Duration::from_millis(400);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Files,
    Checkpoints,
}

#[derive(Clone)]
enum Mode {
    Browse,
    /// The files changed since checkpoint `id`.
    Since {
        id: u64,
    },
    /// A restore preview waiting for Enter: one file, or a renamed file's
    /// two names.
    Confirm {
        id: u64,
        paths: Vec<String>,
        token: String,
    },
    /// Choosing the agent that gets the review request.
    Handoff {
        selected: usize,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Hit {
    Row(usize),
    Key(KeyCode),
}

pub(super) enum ChangesOutcome {
    None,
    Close,
    Op(ChangesOp),
    /// Show this text in `$PAGER`.
    Pager(String),
}

pub(super) struct ChangesWindow {
    /// Which opening of the window this is: results of an earlier one are
    /// not its own.
    pub(super) generation: u64,
    language: Language,
    pub(super) agent: Agent,
    /// Agents that can get a review request.
    reviewers: Vec<String>,
    /// The run the window was opened for: restoring stops once the agent
    /// restarts.
    run: String,
    restarted: bool,
    tab: Tab,
    mode: Mode,
    files: Option<Value>,
    checkpoints: Option<Value>,
    since: Option<Value>,
    selected: [usize; 3],
    /// The diff on show, with the path it belongs to.
    diff: Option<(String, String, bool)>,
    scroll: usize,
    pending: bool,
    quiet_until: Option<std::time::Instant>,
    notice: Option<String>,
    hits: Vec<(Rect, Hit)>,
    pressed: Option<Hit>,
}

impl ChangesWindow {
    pub(super) fn new(
        generation: u64,
        language: Language,
        agent: Agent,
        reviewers: Vec<String>,
        files: Value,
    ) -> Self {
        let run = agent.run.clone();
        Self {
            generation,
            language,
            agent,
            reviewers,
            run,
            restarted: false,
            tab: Tab::Files,
            mode: Mode::Browse,
            files: Some(files),
            checkpoints: None,
            since: None,
            selected: [0; 3],
            diff: None,
            scroll: 0,
            pending: false,
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

    /// A message about this window's work that came from elsewhere (the
    /// pager), shown inside it rather than under it.
    pub(super) fn tell(&mut self, notice: String) {
        self.notice = Some(notice);
    }

    /// The selected agent's run now: a different one means it restarted.
    pub(super) fn seen_run(&mut self, run: &str) {
        if run != self.run {
            self.restarted = true;
        }
    }

    fn list_index(&self) -> usize {
        match (self.tab, &self.mode) {
            (_, Mode::Since { .. }) => 2,
            (Tab::Files, _) => 0,
            (Tab::Checkpoints, _) => 1,
        }
    }

    /// The rows of the list on show.
    fn rows(&self) -> Vec<&Value> {
        fn array<'a>(value: &'a Option<Value>, key: &str) -> Vec<&'a Value> {
            value
                .as_ref()
                .and_then(|value| {
                    let mut node = value;
                    for part in key.split('.') {
                        node = &node[part];
                    }
                    node.as_array()
                })
                .map(|rows| rows.iter().collect())
                .unwrap_or_default()
        }
        match (self.tab, &self.mode) {
            (_, Mode::Since { .. }) => array(&self.since, "since.files"),
            (Tab::Files, _) => array(&self.files, "files"),
            (Tab::Checkpoints, _) => array(&self.checkpoints, "checkpoints"),
        }
    }

    fn current(&self) -> Option<&Value> {
        self.rows()
            .into_iter()
            .nth(self.selected[self.list_index()])
    }

    fn current_path(&self) -> Option<String> {
        self.current()?["path"].as_str().map(str::to_owned)
    }

    /// A result arrived for `op`.
    pub(super) fn receive(&mut self, op: &ChangesOp, result: Result<Value, String>) {
        self.pending = false;
        // A button held down before this arrived is not pressed on what
        // it shows now.
        self.pressed = None;
        self.quiet_until = Some(std::time::Instant::now() + QUIET);
        let value = match result {
            Ok(value) => value,
            Err(error) => {
                if matches!(op, ChangesOp::Restore { token: Some(_), .. }) {
                    self.mode = Mode::Browse;
                }
                self.notice = Some(crate::managed::failure::human(&error).to_owned());
                return;
            }
        };
        match op {
            ChangesOp::List => {
                self.files = Some(value);
                self.clamp();
            }
            ChangesOp::Diff { path } => {
                self.diff = Some((
                    path.clone(),
                    value["diff"].as_str().unwrap_or_default().to_owned(),
                    value["truncated"].as_bool().unwrap_or(false),
                ));
                self.scroll = 0;
            }
            ChangesOp::Checkpoints => {
                self.checkpoints = Some(value);
                self.clamp();
            }
            ChangesOp::Make { .. } => {
                self.notice = Some(format!(
                    "{} {}",
                    self.word("Checkpoint made:", "checkpoint를 만들었습니다:"),
                    value["id"]
                ));
                self.checkpoints = None;
            }
            ChangesOp::Show { id, path: None } => {
                self.since = Some(value);
                self.mode = Mode::Since { id: *id };
                self.selected[2] = 0;
                self.diff = None;
            }
            ChangesOp::Show {
                path: Some(path), ..
            } => {
                self.diff = Some((
                    path.clone(),
                    value["diff"].as_str().unwrap_or_default().to_owned(),
                    value["truncated"].as_bool().unwrap_or(false),
                ));
                self.scroll = 0;
            }
            ChangesOp::Restore {
                id,
                paths,
                token: None,
                ..
            } => {
                let files = value["files"].as_array().cloned().unwrap_or_default();
                let file = files.first().cloned().unwrap_or_default();
                let diff: String = files
                    .iter()
                    .map(|file| file["diff"].as_str().unwrap_or_default())
                    .collect();
                self.diff = Some((
                    paths.join(" + "),
                    diff,
                    files
                        .iter()
                        .any(|file| file["truncated"].as_bool() == Some(true)),
                ));
                self.scroll = 0;
                self.mode = Mode::Confirm {
                    id: *id,
                    paths: paths.clone(),
                    token: value["token"].as_str().unwrap_or_default().to_owned(),
                };
                self.notice = Some(format!(
                    "{} {}",
                    match file["action"].as_str() {
                        _ if paths.len() > 1 => self.word(
                            "Restore puts the file back under its old name and removes the new one.",
                            "되돌리면 파일을 이전 이름으로 되돌리고 새 이름은 지웁니다.",
                        ),
                        Some("delete") =>
                            self.word("Restore deletes this file.", "되돌리면 이 파일을 지웁니다."),
                        Some("unchanged") => self.word(
                            "This file is already as saved.",
                            "이미 저장된 그대로입니다."
                        ),
                        _ => self.word(
                            "Restore writes the saved file back.",
                            "저장된 파일로 되돌립니다."
                        ),
                    },
                    self.word(
                        "Files only: the conversation is not rolled back.",
                        "파일만이며 대화는 되돌리지 않습니다."
                    )
                ));
            }
            ChangesOp::Restore { token: Some(_), .. } => {
                self.mode = Mode::Browse;
                self.tab = Tab::Files;
                self.since = None;
                self.diff = None;
                self.notice = Some(format!(
                    "{} {} {}",
                    match value["stage"].as_str() {
                        Some("restored") => self.word("Restored.", "되돌렸습니다."),
                        _ => self.word("Partly restored.", "일부만 되돌렸습니다."),
                    },
                    self.word(
                        "The files before are checkpoint",
                        "되돌리기 전 파일은 checkpoint"
                    ),
                    value["before"]
                ));
                self.checkpoints = None;
                self.files = None;
            }
            ChangesOp::Handoff { reviewer } => {
                self.mode = Mode::Browse;
                self.notice = Some(format!(
                    "{} {reviewer}. {}",
                    self.word(
                        "Review request queued for",
                        "검토 요청을 대기열에 넣었습니다:"
                    ),
                    self.word("Nothing was sent.", "보내지는 않았습니다.")
                ));
            }
        }
    }

    /// After a change, what needs reading again.
    pub(super) fn needs(&self) -> Option<ChangesOp> {
        if self.pending {
            return None;
        }
        match self.tab {
            Tab::Files if self.files.is_none() => Some(ChangesOp::List),
            Tab::Checkpoints if self.checkpoints.is_none() => Some(ChangesOp::Checkpoints),
            _ => None,
        }
    }

    fn clamp(&mut self) {
        let count = self.rows().len();
        let index = self.list_index();
        self.selected[index] = self.selected[index].min(count.saturating_sub(1));
    }

    fn op(&mut self, op: ChangesOp) -> ChangesOutcome {
        self.pending = true;
        ChangesOutcome::Op(op)
    }

    pub(super) fn handle(&mut self, event: Event) -> ChangesOutcome {
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => self.key(key),
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
                    return ChangesOutcome::None;
                }
                if !matches!(mouse.kind, MouseEventKind::Up(MouseButton::Left))
                    || self.pressed.take() != target
                {
                    return ChangesOutcome::None;
                }
                match target {
                    Some(Hit::Row(index)) => {
                        let list = self.list_index();
                        if let Mode::Handoff { selected } = &mut self.mode {
                            *selected = index;
                        } else {
                            self.selected[list] = index;
                        }
                        ChangesOutcome::None
                    }
                    Some(Hit::Key(code)) => self.key(KeyEvent::new(code, KeyModifiers::NONE)),
                    None => ChangesOutcome::None,
                }
            }
            _ => ChangesOutcome::None,
        }
    }

    fn key(&mut self, key: KeyEvent) -> ChangesOutcome {
        if key.code == KeyCode::Esc && matches!(self.mode, Mode::Browse) {
            return ChangesOutcome::Close;
        }
        let now = std::time::Instant::now();
        // Nothing acts while a result is on its way, nor just after one
        // arrived: a key pressed while waiting must not confirm it.
        if self.pending || self.quiet_until.is_some_and(|until| now < until) {
            if self.quiet_until.is_some_and(|until| now < until) {
                self.quiet_until = Some(now + QUIET);
            }
            return ChangesOutcome::None;
        }
        self.quiet_until = None;
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
        {
            return ChangesOutcome::None;
        }
        self.notice = None;
        match self.mode.clone() {
            Mode::Confirm { id, paths, token } => match key.code {
                KeyCode::Enter if !self.restarted => self.op(ChangesOp::Restore {
                    id,
                    paths,
                    token: Some(token),
                    run: Some(self.run.clone()),
                }),
                KeyCode::Esc => {
                    self.mode = Mode::Since { id };
                    self.diff = None;
                    ChangesOutcome::None
                }
                _ => ChangesOutcome::None,
            },
            Mode::Handoff { selected } => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    self.mode = Mode::Handoff {
                        selected: selected.saturating_sub(1),
                    };
                    ChangesOutcome::None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.mode = Mode::Handoff {
                        selected: (selected + 1).min(self.reviewers.len().saturating_sub(1)),
                    };
                    ChangesOutcome::None
                }
                KeyCode::Enter => match self.reviewers.get(selected).cloned() {
                    Some(reviewer) => self.op(ChangesOp::Handoff { reviewer }),
                    None => ChangesOutcome::None,
                },
                KeyCode::Esc => {
                    self.mode = Mode::Browse;
                    ChangesOutcome::None
                }
                _ => ChangesOutcome::None,
            },
            Mode::Since { id } => self.since_key(id, key),
            Mode::Browse => self.browse_key(key),
        }
    }

    fn move_selection(&mut self, code: KeyCode) -> bool {
        let count = self.rows().len();
        let index = self.list_index();
        match code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected[index] = self.selected[index].saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected[index] = (self.selected[index] + 1).min(count.saturating_sub(1));
            }
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::PageDown => self.scroll += 10,
            _ => return false,
        }
        true
    }

    fn browse_key(&mut self, key: KeyEvent) -> ChangesOutcome {
        if self.move_selection(key.code) {
            return ChangesOutcome::None;
        }
        match (self.tab, key.code) {
            (_, KeyCode::Char('q')) => ChangesOutcome::Close,
            (_, KeyCode::Tab) => {
                self.tab = match self.tab {
                    Tab::Files => Tab::Checkpoints,
                    Tab::Checkpoints => Tab::Files,
                };
                self.diff = None;
                match self.needs() {
                    Some(op) => self.op(op),
                    None => ChangesOutcome::None,
                }
            }
            (_, KeyCode::Char('g')) => {
                let op = match self.tab {
                    Tab::Files => ChangesOp::List,
                    Tab::Checkpoints => ChangesOp::Checkpoints,
                };
                self.op(op)
            }
            (_, KeyCode::Char('m')) => self.op(ChangesOp::Make {
                reason: "from the desk".into(),
            }),
            (_, KeyCode::Char('o')) => match &self.diff {
                Some((_, diff, _)) => ChangesOutcome::Pager(diff.clone()),
                None => {
                    self.notice = Some(
                        self.word(
                            "Open a diff with Enter first.",
                            "먼저 Enter로 diff를 여세요.",
                        )
                        .into(),
                    );
                    ChangesOutcome::None
                }
            },
            (Tab::Files, KeyCode::Char('h')) => {
                if self.reviewers.is_empty() {
                    self.notice = Some(
                        self.word("No other agent to ask.", "요청할 다른 agent가 없습니다.")
                            .into(),
                    );
                } else {
                    self.mode = Mode::Handoff { selected: 0 };
                }
                ChangesOutcome::None
            }
            (Tab::Files, KeyCode::Enter) => match self.current_path() {
                Some(path) if !path.ends_with('/') => self.op(ChangesOp::Diff { path }),
                _ => ChangesOutcome::None,
            },
            (Tab::Checkpoints, KeyCode::Enter) => {
                match self.current().and_then(|row| row["id"].as_u64()) {
                    Some(id) => self.op(ChangesOp::Show { id, path: None }),
                    None => ChangesOutcome::None,
                }
            }
            _ => ChangesOutcome::None,
        }
    }

    fn since_key(&mut self, id: u64, key: KeyEvent) -> ChangesOutcome {
        if self.move_selection(key.code) {
            return ChangesOutcome::None;
        }
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Browse;
                self.since = None;
                self.diff = None;
                ChangesOutcome::None
            }
            KeyCode::Enter => match self.current_path() {
                Some(path) => self.op(ChangesOp::Show {
                    id,
                    path: Some(path),
                }),
                None => ChangesOutcome::None,
            },
            KeyCode::Char('o') => match &self.diff {
                Some((_, diff, _)) => ChangesOutcome::Pager(diff.clone()),
                None => ChangesOutcome::None,
            },
            KeyCode::Char('r') => {
                if self.restarted {
                    self.notice = Some(
                        self.word(
                            "The agent restarted; reopen the window to restore.",
                            "agent가 다시 시작됐습니다. 창을 다시 열어 되돌리세요.",
                        )
                        .into(),
                    );
                    return ChangesOutcome::None;
                }
                match self.current_path() {
                    // A renamed file comes back under its old name.
                    Some(path) => {
                        let from = self
                            .current()
                            .and_then(|row| row["from"].as_str().map(str::to_owned));
                        self.op(ChangesOp::Restore {
                            id,
                            paths: from.into_iter().chain([path]).collect(),
                            token: None,
                            run: Some(self.run.clone()),
                        })
                    }
                    None => ChangesOutcome::None,
                }
            }
            _ => ChangesOutcome::None,
        }
    }

    fn row(&self, row: &Value) -> String {
        match (self.tab, &self.mode) {
            (Tab::Checkpoints, Mode::Browse) => format!(
                "#{:<4} {}{} {}",
                row["id"],
                if row["auto"].as_bool() == Some(true) {
                    "auto "
                } else {
                    ""
                },
                row["reason"].as_str().unwrap_or_default(),
                row["files"]
                    .as_u64()
                    .map(|files| format!("· {files}"))
                    .unwrap_or_default()
            ),
            _ => {
                let counts = if row["binary"].as_bool() == Some(true) {
                    "bin".to_owned()
                } else {
                    match (row["insertions"].as_u64(), row["deletions"].as_u64()) {
                        (Some(added), Some(deleted)) => format!("+{added} -{deleted}"),
                        _ => String::new(),
                    }
                };
                format!(
                    "{:<10} {:>11}  {}",
                    row["kind"].as_str().unwrap_or_default(),
                    counts,
                    plain(row["path"].as_str().unwrap_or_default())
                )
            }
        }
    }

    fn header(&self) -> String {
        match (&self.mode, self.tab) {
            (Mode::Since { id } | Mode::Confirm { id, .. }, _) => format!(
                "{} #{id} · {}",
                self.word("Changed since checkpoint", "checkpoint 이후 바뀐 파일"),
                self.word("r restore a file", "r 파일 되돌리기")
            ),
            (Mode::Handoff { .. }, _) => self
                .word(
                    "Ask which agent to review?",
                    "어느 agent에게 검토를 부탁할까요?",
                )
                .into(),
            (_, Tab::Files) => match &self.files {
                Some(files) => format!(
                    "{} · {} {}{}",
                    files["base"]["branch"].as_str().unwrap_or("(detached)"),
                    files["total"],
                    self.word("changed", "개 바뀜"),
                    if files["truncated"].as_bool() == Some(true) {
                        " (500+)"
                    } else {
                        ""
                    }
                ),
                None => self.word("Reading…", "읽는 중…").into(),
            },
            (_, Tab::Checkpoints) => match &self.checkpoints {
                Some(list) => format!(
                    "{} {}",
                    list["checkpoints"].as_array().map_or(0, Vec::len),
                    self.word(
                        "checkpoints · Enter shows changes since",
                        "개 checkpoint · Enter로 그 뒤 변경 보기"
                    )
                ),
                None => self.word("Reading…", "읽는 중…").into(),
            },
        }
    }

    fn buttons(&self) -> Vec<(String, KeyCode)> {
        let label = |key: &str, text: &str| format!(" {key} {text} ");
        match &self.mode {
            Mode::Confirm { .. } if self.restarted => {
                vec![(label("Esc", self.word("Back", "뒤로")), KeyCode::Esc)]
            }
            Mode::Confirm { .. } => vec![
                (label("Esc", self.word("Back", "뒤로")), KeyCode::Esc),
                (
                    label("Enter", self.word("Restore file", "파일 되돌리기")),
                    KeyCode::Enter,
                ),
            ],
            Mode::Handoff { .. } => vec![
                (label("Esc", self.word("Back", "뒤로")), KeyCode::Esc),
                (
                    label("Enter", self.word("Queue request", "요청 넣기")),
                    KeyCode::Enter,
                ),
            ],
            Mode::Since { .. } => vec![
                (label("Esc", self.word("Back", "뒤로")), KeyCode::Esc),
                (label("o", self.word("Pager", "pager")), KeyCode::Char('o')),
                (
                    label("r", self.word("Restore", "되돌리기")),
                    KeyCode::Char('r'),
                ),
                (label("Enter", self.word("Diff", "diff")), KeyCode::Enter),
            ],
            Mode::Browse => vec![
                (label("Esc", self.word("Close", "닫기")), KeyCode::Esc),
                (
                    label("Tab", self.word("Files/Checkpoints", "파일/checkpoint")),
                    KeyCode::Tab,
                ),
                (
                    label("m", self.word("Checkpoint", "checkpoint")),
                    KeyCode::Char('m'),
                ),
                (
                    label("h", self.word("Review", "검토 요청")),
                    KeyCode::Char('h'),
                ),
                (label("o", self.word("Pager", "pager")), KeyCode::Char('o')),
                (label("Enter", self.word("Open", "열기")), KeyCode::Enter),
            ],
        }
    }

    pub(super) fn draw(&mut self, frame: &mut Frame, theme: Theme) {
        let area = frame.area();
        let width = area.width.saturating_sub(4).clamp(1, 110).min(area.width);
        let height = area.height.saturating_sub(2).max(8).min(area.height);
        let modal = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        );
        let text_width = width.saturating_sub(4) as usize;
        let (panel, text, muted, accent, accent_text) = dialog_colors(theme);
        frame.render_widget(Clear, modal);
        let title = format!(" {} · {} ", self.word("Changes", "변경"), self.agent.name);
        frame.render_widget(
            Block::default()
                .borders(Borders::ALL)
                .title(ellipsize(&title, width.saturating_sub(2) as usize))
                .border_style(Style::default().fg(accent))
                .style(Style::default().bg(panel).fg(text)),
            modal,
        );
        let inner = Rect::new(
            modal.x + 2,
            modal.y + 1,
            modal.width.saturating_sub(4),
            modal.height.saturating_sub(2),
        );
        let mut hits = Vec::new();
        let mut y = inner.y;
        let line = |frame: &mut Frame, y: u16, content: &str, style: Style| {
            frame.render_widget(
                Paragraph::new(ellipsize(content, text_width)).style(style),
                Rect::new(inner.x, y, inner.width, 1),
            );
        };
        line(
            frame,
            y,
            &self.header(),
            Style::default().fg(muted).bg(panel),
        );
        y += 1;
        let bottom = modal.bottom().saturating_sub(3);
        // The list, or the agents to ask.
        let rows: Vec<String> = match &self.mode {
            Mode::Handoff { .. } => self.reviewers.clone(),
            _ => self.rows().into_iter().map(|row| self.row(row)).collect(),
        };
        let selected = match &self.mode {
            Mode::Handoff { selected } => *selected,
            _ => self.selected[self.list_index()],
        };
        let shown = rows
            .len()
            .clamp(1, LIST_ROWS)
            .min(bottom.saturating_sub(y) as usize);
        let first = (selected + 1).saturating_sub(shown);
        if rows.is_empty() {
            line(
                frame,
                y,
                self.word("Nothing here.", "없습니다."),
                Style::default().fg(muted).bg(panel),
            );
            y += 1;
        }
        for (index, row) in rows.iter().enumerate().skip(first).take(shown) {
            let style = if index == selected {
                Style::default().fg(accent_text).bg(accent)
            } else {
                Style::default().fg(text).bg(panel)
            };
            let rect = Rect::new(inner.x, y, inner.width, 1);
            frame.render_widget(
                Paragraph::new(ellipsize(row, text_width)).style(style),
                rect,
            );
            hits.push((rect, Hit::Row(index)));
            y += 1;
        }
        y += 1;
        // The diff, then the notice.
        let notice_rows = usize::from(self.notice.is_some());
        let room = (bottom.saturating_sub(y) as usize).saturating_sub(notice_rows);
        if let Some((path, diff, truncated)) = &self.diff {
            let lines: Vec<&str> = diff.lines().collect();
            let skip = self.scroll.min(lines.len().saturating_sub(1));
            if room > 0 {
                let mark = if *truncated {
                    self.word(" (cut at 256 KiB)", " (256 KiB에서 자름)")
                } else {
                    ""
                };
                line(
                    frame,
                    y,
                    &format!("{}{mark}", plain(path)),
                    Style::default().fg(muted).bg(panel),
                );
                y += 1;
            }
            for diff_line in lines.iter().skip(skip).take(room.saturating_sub(1)) {
                let color = if diff_line.starts_with('+') && !diff_line.starts_with("+++") {
                    accent
                } else if diff_line.starts_with('-') && !diff_line.starts_with("---") {
                    muted
                } else {
                    text
                };
                line(
                    frame,
                    y,
                    &plain(diff_line),
                    Style::default().fg(color).bg(panel),
                );
                y += 1;
            }
        }
        if let Some(notice) = &self.notice {
            line(
                frame,
                bottom.saturating_sub(1).max(y),
                notice,
                Style::default().fg(accent).bg(panel),
            );
        }
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
            hits.push((rect, Hit::Key(code)));
            right = rect.x.saturating_sub(1);
        }
        self.hits = hits;
    }
}

/// Text drawn as is: control characters become spaces, tabs four spaces.
fn plain(text: &str) -> String {
    text.replace('\t', "    ")
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn agent() -> Agent {
        serde_json::from_value(json!({
            "id": "a", "name": "builder", "provider": "codex", "pane_id": "%1", "window_id": "@1",
            "workspace": "main", "cwd": "/tmp", "boot": "b", "generation": "1", "run": "r1",
            "process": "running", "state": "idle", "evidence": {}, "seen": true, "revision": "1",
            "returned_idle": false
        }))
        .unwrap()
    }

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn settle(window: &mut ChangesWindow) {
        window.quiet_until = None;
    }

    fn files() -> Value {
        json!({"base": {"branch": "main"}, "total": 2, "truncated": false, "files": [
            {"path": "a.txt", "kind": "modified", "insertions": 1, "deletions": 0, "binary": false},
            {"path": "b.txt", "kind": "untracked", "insertions": 2, "deletions": 0, "binary": false}]})
    }

    #[test]
    fn a_restore_needs_a_preview_and_then_enter() {
        let mut window = ChangesWindow::new(1, Language::English, agent(), vec![], files());
        // To a checkpoint, its changes, then a file.
        assert!(matches!(
            window.handle(key(KeyCode::Tab)),
            ChangesOutcome::Op(ChangesOp::Checkpoints)
        ));
        window.receive(
            &ChangesOp::Checkpoints,
            Ok(json!({"checkpoints": [{"id": 3, "reason": "x"}]})),
        );
        settle(&mut window);
        assert!(matches!(
            window.handle(key(KeyCode::Enter)),
            ChangesOutcome::Op(ChangesOp::Show { id: 3, path: None })
        ));
        window.receive(
            &ChangesOp::Show { id: 3, path: None },
            Ok(json!({"since": {"files": [{"path": "a.txt", "kind": "modified"}]}})),
        );
        settle(&mut window);
        let preview = window.handle(key(KeyCode::Char('r')));
        let ChangesOutcome::Op(op @ ChangesOp::Restore { token: None, .. }) = preview else {
            panic!("expected a preview request");
        };
        window.receive(
            &op,
            Ok(json!({"token": "abc", "files": [{"action": "restore", "diff": "-x\n+y\n"}]})),
        );
        // A key pressed while the preview came is not a confirmation.
        assert!(matches!(
            window.handle(key(KeyCode::Enter)),
            ChangesOutcome::None
        ));
        settle(&mut window);
        match window.handle(key(KeyCode::Enter)) {
            ChangesOutcome::Op(ChangesOp::Restore {
                id: 3,
                paths,
                token: Some(token),
                ..
            }) => {
                assert_eq!((paths, token.as_str()), (vec!["a.txt".to_owned()], "abc"));
            }
            _ => panic!("expected the confirmed restore"),
        }
    }

    #[test]
    fn after_a_restart_nothing_is_restored() {
        let mut window = ChangesWindow::new(1, Language::English, agent(), vec![], files());
        window.mode = Mode::Confirm {
            id: 1,
            paths: vec!["a.txt".into()],
            token: "t".into(),
        };
        window.seen_run("r2");
        assert!(matches!(
            window.handle(key(KeyCode::Enter)),
            ChangesOutcome::None
        ));
    }

    #[test]
    fn a_handoff_names_the_chosen_agent() {
        let mut window = ChangesWindow::new(
            1,
            Language::English,
            agent(),
            vec!["rev".into(), "qa".into()],
            files(),
        );
        assert!(matches!(
            window.handle(key(KeyCode::Char('h'))),
            ChangesOutcome::None
        ));
        window.handle(key(KeyCode::Down));
        match window.handle(key(KeyCode::Enter)) {
            ChangesOutcome::Op(ChangesOp::Handoff { reviewer }) => assert_eq!(reviewer, "qa"),
            _ => panic!("expected a handoff"),
        }
    }
}
