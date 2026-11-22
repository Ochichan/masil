//! Command palette for the native management desk. Agents, the panes of
//! this server and desk actions share one list ranked by `finder`. It owns
//! keyboard and mouse input while open.

use super::{
    model::{Action, Language, Theme},
    view::{cell_width, ellipsize},
};
use crate::finder;
use crate::managed::find::PaneTarget;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};
use std::time::{Duration, Instant};
use unicode_segmentation::UnicodeSegmentation;

const QUERY_GRAPHEMES: usize = 128;
const QUERY_BYTES: usize = 1_024;
const PRESS_EXPIRY: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Kind {
    Agent,
    Action,
    Pane,
}

#[derive(Clone, Debug)]
pub(super) enum Target {
    Agent(String),
    Pane(Box<PaneTarget>),
    Action(Action),
}

#[derive(Clone, Debug)]
pub(super) struct Item {
    pub kind: Kind,
    pub label: String,
    pub detail: String,
    pub hint: String,
    pub target: Target,
    pub enabled: bool,
    /// The pane an agent entry already stands for.
    pub pane: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Hit {
    Item(usize),
    Close,
    Surface,
}

pub(super) enum PaletteResult {
    None,
    Close,
    Activate(Target),
}

pub(super) struct Palette {
    language: Language,
    /// The agent that agent actions apply to.
    context: Option<String>,
    query: String,
    /// Grapheme distance from the end of `query` (0 = caret at end).
    cursor_from_end: usize,
    items: Vec<Item>,
    /// Indices into `items`, ranked for the current query.
    shown: Vec<usize>,
    selected: usize,
    scroll: usize,
    page: usize,
    panes: PaneState,
    hits: Vec<(Rect, Hit)>,
    modal: Rect,
    pressed: Option<(Option<Hit>, Instant)>,
    hover: Option<Hit>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PaneState {
    Loading,
    Loaded { truncated: bool },
    Failed,
}

fn word(language: Language, english: &'static str, korean: &'static str) -> &'static str {
    match language {
        Language::English => english,
        Language::Korean => korean,
    }
}

impl Palette {
    pub(super) fn new(language: Language, items: Vec<Item>, context: Option<String>) -> Self {
        let mut palette = Self {
            language,
            context,
            query: String::new(),
            cursor_from_end: 0,
            items,
            shown: Vec::new(),
            selected: 0,
            scroll: 0,
            page: 1,
            panes: PaneState::Loading,
            hits: Vec::new(),
            modal: Rect::default(),
            pressed: None,
            hover: None,
        };
        palette.rank();
        palette
    }

    /// Add the server's panes once they arrive; panes that host a listed
    /// agent are left to the agent entry.
    pub(super) fn set_panes(&mut self, result: Result<(Vec<PaneTarget>, bool), String>) {
        // A palette reopened before an earlier query returned gets two
        // answers; the first one fills it.
        if self.panes != PaneState::Loading {
            return;
        }
        let selected = self.shown.get(self.selected).copied();
        match result {
            Ok((panes, truncated)) => {
                let hosted: std::collections::HashSet<String> = self
                    .items
                    .iter()
                    .filter_map(|item| item.pane.clone())
                    .collect();
                self.items.extend(
                    panes
                        .into_iter()
                        .filter(|pane| !hosted.contains(&pane.pane_id))
                        .map(|pane| Item {
                            kind: Kind::Pane,
                            label: pane.label(),
                            detail: pane.detail(),
                            hint: pane.pane_id.clone(),
                            enabled: !pane.dead,
                            pane: None,
                            target: Target::Pane(Box::new(pane)),
                        }),
                );
                self.panes = PaneState::Loaded { truncated };
            }
            Err(_) => self.panes = PaneState::Failed,
        }
        self.rank();
        // Keep the highlighted entry when the list grows under it.
        if let Some(position) = selected.and_then(|item| self.shown.iter().position(|&i| i == item))
        {
            self.selected = position;
        }
    }

    fn rank(&mut self) {
        let query = self.query.trim();
        let mut ranked: Vec<(u32, usize)> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| {
                finder::score(query, &[&item.label, &item.detail]).map(|score| (score, index))
            })
            .collect();
        ranked.sort_by(|a, b| {
            let (left, right) = (&self.items[a.1], &self.items[b.1]);
            b.0.cmp(&a.0)
                .then(right.enabled.cmp(&left.enabled))
                .then(left.kind.cmp(&right.kind))
                .then(a.1.cmp(&b.1))
        });
        self.shown = ranked.into_iter().map(|(_, index)| index).collect();
        self.selected = 0;
        self.scroll = 0;
    }

    fn cursor_grapheme(&self) -> usize {
        let count = self.query.graphemes(true).count();
        count.saturating_sub(self.cursor_from_end.min(count))
    }

    fn grapheme_byte_offset(&self, grapheme_index: usize) -> usize {
        self.query
            .grapheme_indices(true)
            .nth(grapheme_index)
            .map_or(self.query.len(), |(offset, _)| offset)
    }

    fn insert(&mut self, text: &str) {
        let before = self.query.len();
        let mut cursor = self.cursor_grapheme();
        for grapheme in text.graphemes(true) {
            if grapheme.chars().any(char::is_control)
                || self.query.graphemes(true).count() >= QUERY_GRAPHEMES
                || self.query.len() + grapheme.len() > QUERY_BYTES
            {
                continue;
            }
            let at = self.grapheme_byte_offset(cursor);
            self.query.insert_str(at, grapheme);
            cursor += 1;
        }
        if self.query.len() != before {
            let count = self.query.graphemes(true).count();
            self.cursor_from_end = count.saturating_sub(cursor);
            self.rank();
        }
    }

    fn move_selection(&mut self, delta: isize) {
        if self.shown.is_empty() {
            return;
        }
        let last = self.shown.len() - 1;
        self.selected = self.selected.saturating_add_signed(delta).min(last);
        self.reveal();
    }

    fn reveal(&mut self) {
        let page = self.page.max(1);
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + page {
            self.scroll = self.selected + 1 - page;
        }
    }

    fn activate(&self, position: usize) -> PaletteResult {
        match self.shown.get(position).map(|&index| &self.items[index]) {
            Some(item) if item.enabled => PaletteResult::Activate(item.target.clone()),
            _ => PaletteResult::None,
        }
    }

    pub(super) fn hover(&self) -> Option<Hit> {
        self.hover
    }

    pub(super) fn handle(&mut self, event: Event, now: Instant) -> PaletteResult {
        match event {
            Event::Key(key) => self.key(key),
            Event::Paste(text) => {
                self.insert(&text);
                PaletteResult::None
            }
            Event::Resize(_, _) => {
                self.hits.clear();
                self.pressed = None;
                self.hover = None;
                PaletteResult::None
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
                match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left) => self.pressed = Some((target, now)),
                    MouseEventKind::Up(MouseButton::Left) => {
                        let pressed = self.pressed.take();
                        if let Some((hit, at)) = pressed
                            && hit == target
                            && now.duration_since(at) <= PRESS_EXPIRY
                        {
                            return match target {
                                Some(Hit::Item(position)) => {
                                    self.selected = position;
                                    self.activate(position)
                                }
                                Some(Hit::Close) | None => PaletteResult::Close,
                                Some(Hit::Surface) => PaletteResult::None,
                            };
                        }
                    }
                    MouseEventKind::Moved | MouseEventKind::Drag(_) => {
                        self.hover = match target {
                            Some(Hit::Item(_) | Hit::Close) => target,
                            _ => None,
                        };
                    }
                    MouseEventKind::ScrollDown if target.is_some() => self.move_selection(3),
                    MouseEventKind::ScrollUp if target.is_some() => self.move_selection(-3),
                    _ => {}
                }
                PaletteResult::None
            }
            _ => PaletteResult::None,
        }
    }

    fn key(&mut self, key: KeyEvent) -> PaletteResult {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => PaletteResult::Close,
            KeyCode::Enter => self.activate(self.selected),
            KeyCode::Up => {
                self.move_selection(-1);
                PaletteResult::None
            }
            KeyCode::Char('p') if control => {
                self.move_selection(-1);
                PaletteResult::None
            }
            KeyCode::Down | KeyCode::Tab => {
                self.move_selection(1);
                PaletteResult::None
            }
            KeyCode::Char('n') if control => {
                self.move_selection(1);
                PaletteResult::None
            }
            KeyCode::BackTab => {
                self.move_selection(-1);
                PaletteResult::None
            }
            KeyCode::PageUp => {
                self.move_selection(-(self.page.max(1) as isize));
                PaletteResult::None
            }
            KeyCode::PageDown => {
                self.move_selection(self.page.max(1) as isize);
                PaletteResult::None
            }
            KeyCode::Home => {
                self.selected = 0;
                self.reveal();
                PaletteResult::None
            }
            KeyCode::End => {
                self.selected = self.shown.len().saturating_sub(1);
                self.reveal();
                PaletteResult::None
            }
            KeyCode::Left => {
                let count = self.query.graphemes(true).count();
                self.cursor_from_end = (self.cursor_from_end + 1).min(count);
                PaletteResult::None
            }
            KeyCode::Right => {
                self.cursor_from_end = self.cursor_from_end.saturating_sub(1);
                PaletteResult::None
            }
            KeyCode::Char('a') if control => {
                self.cursor_from_end = self.query.graphemes(true).count();
                PaletteResult::None
            }
            KeyCode::Char('e') if control => {
                self.cursor_from_end = 0;
                PaletteResult::None
            }
            KeyCode::Backspace => {
                let cursor = self.cursor_grapheme();
                if cursor > 0 {
                    let start = self.grapheme_byte_offset(cursor - 1);
                    let end = self.grapheme_byte_offset(cursor);
                    self.query.replace_range(start..end, "");
                    self.rank();
                }
                PaletteResult::None
            }
            KeyCode::Delete => {
                let cursor = self.cursor_grapheme();
                let count = self.query.graphemes(true).count();
                if cursor < count {
                    let start = self.grapheme_byte_offset(cursor);
                    let end = self.grapheme_byte_offset(cursor + 1);
                    self.query.replace_range(start..end, "");
                    self.cursor_from_end = self.cursor_from_end.saturating_sub(1);
                    self.rank();
                }
                PaletteResult::None
            }
            KeyCode::Char('w') if control => {
                let cursor = self.cursor_grapheme();
                if cursor > 0 {
                    let graphemes: Vec<&str> = self.query.graphemes(true).collect();
                    let mut target = cursor;
                    while target > 0 && graphemes[target - 1].chars().all(char::is_whitespace) {
                        target -= 1;
                    }
                    while target > 0 && !graphemes[target - 1].chars().all(char::is_whitespace) {
                        target -= 1;
                    }
                    let start = self.grapheme_byte_offset(target);
                    let end = self.grapheme_byte_offset(cursor);
                    self.query.replace_range(start..end, "");
                    self.rank();
                }
                PaletteResult::None
            }
            KeyCode::Char('u') if control => {
                if !self.query.is_empty() {
                    self.query.clear();
                    self.cursor_from_end = 0;
                    self.rank();
                }
                PaletteResult::None
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
                PaletteResult::None
            }
            _ => PaletteResult::None,
        }
    }

    fn kind_label(&self, kind: Kind) -> &'static str {
        match kind {
            Kind::Agent => word(self.language, "Agent", "에이전트"),
            Kind::Pane => word(self.language, "Pane", "pane"),
            Kind::Action => word(self.language, "Action", "작업"),
        }
    }

    pub(super) fn draw(&mut self, frame: &mut Frame, theme: Theme) {
        let area = frame.area();
        self.hits.clear();
        let width = area.width.saturating_sub(4).clamp(24, 96).min(area.width);
        // Sized to every candidate, not the current matches, so typing does
        // not resize the box; it grows once when the panes arrive.
        let wanted = self.items.len().max(1) as u16 + 4;
        let height = wanted
            .min(26)
            .min(area.height.saturating_sub(2))
            .max(7)
            .min(area.height);
        if width < 20 || height < 5 {
            return;
        }
        let modal = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        self.modal = modal;
        let palette = super::view::Palette::for_theme(theme);
        let (panel, text, _, accent, accent_text) = super::managed::dialog_colors(theme);
        let muted = palette.muted_style();
        frame.render_widget(Clear, modal);
        let inner = Rect::new(
            modal.x + 2,
            modal.y + 1,
            modal.width.saturating_sub(4),
            modal.height.saturating_sub(2),
        );
        let text_width = inner.width as usize;
        // The modal surface consumes clicks that hit no entry; a click with
        // no region at all is outside and closes the palette.
        self.hits.push((modal, Hit::Surface));
        let title = [
            word(self.language, " Command palette ", " 명령 팔레트 "),
            word(self.language, " Palette ", " 팔레트 "),
        ]
        .into_iter()
        .find(|title| cell_width(title) + 8 <= modal.width as usize)
        .unwrap_or("");
        frame.render_widget(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(Style::default().fg(accent))
                .style(Style::default().bg(panel).fg(text)),
            modal,
        );
        let close_label = " Esc ";
        let close = Rect::new(modal.right().saturating_sub(7), modal.y, 5, 1);
        let close_pressed = self.pressed.is_some_and(|(hit, _)| hit == Some(Hit::Close));
        let close_hovered = self.hover == Some(Hit::Close);
        let close_style = if close_pressed {
            Style::default()
                .fg(accent_text)
                .bg(palette.warning)
                .add_modifier(Modifier::BOLD)
        } else if close_hovered {
            Style::default()
                .fg(accent_text)
                .bg(accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(accent).bg(panel)
        };
        frame.render_widget(Paragraph::new(close_label).style(close_style), close);
        self.hits.push((close, Hit::Close));

        // Query line: keep the caret visible whether appending at the end or
        // editing in the middle. The marker takes two cells and the caret one.
        let count = format!(" {}", self.shown.len());
        let field_width = text_width.saturating_sub(cell_width(&count));
        let query_width = field_width.saturating_sub(3);
        let cursor = self.cursor_grapheme();
        let query_spans = if self.query.is_empty() {
            if 2 < field_width {
                frame.set_cursor_position((inner.x + 2, inner.y));
            }
            vec![Span::styled(
                ellipsize(
                    word(
                        self.language,
                        "Type to find agents, panes and actions",
                        "에이전트, pane, 작업 찾기",
                    ),
                    field_width.saturating_sub(2),
                ),
                muted,
            )]
        } else if self.cursor_from_end == 0 {
            let visible = super::managed::tail(&self.query, query_width);
            let caret = 2 + cell_width(&visible);
            if caret < field_width {
                frame.set_cursor_position((inner.x + caret as u16, inner.y));
            }
            vec![Span::styled(
                format!("{visible}▏"),
                Style::default().fg(text),
            )]
        } else {
            let (visible, cursor_x, start) =
                super::view::search_window(&self.query, cursor, query_width);
            let rel = cursor.saturating_sub(start);
            let split_byte = visible
                .grapheme_indices(true)
                .nth(rel)
                .map_or(visible.len(), |(offset, _)| offset);
            let caret = 2 + cursor_x;
            if caret < field_width {
                frame.set_cursor_position((inner.x + caret as u16, inner.y));
            }
            vec![
                Span::styled(visible[..split_byte].to_owned(), Style::default().fg(text)),
                Span::styled("▏", Style::default().fg(accent)),
                Span::styled(visible[split_byte..].to_owned(), Style::default().fg(text)),
            ]
        };
        let mut query_line = vec![Span::styled(
            "› ",
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        )];
        query_line.extend(query_spans);
        frame.render_widget(
            Paragraph::new(Line::from(query_line)),
            Rect::new(inner.x, inner.y, field_width as u16, 1),
        );
        frame.render_widget(
            Paragraph::new(count.clone()).style(muted),
            Rect::new(
                inner.right().saturating_sub(cell_width(&count) as u16),
                inner.y,
                cell_width(&count) as u16,
                1,
            ),
        );

        let panes = match self.panes {
            PaneState::Loading => Some(word(
                self.language,
                "Loading panes…",
                "pane 목록 불러오는 중…",
            )),
            PaneState::Failed => Some(word(
                self.language,
                "Pane list unavailable",
                "pane 목록을 불러올 수 없음",
            )),
            PaneState::Loaded { truncated: true } => Some(word(
                self.language,
                "Only the first 2048 panes are listed",
                "처음 2048개 pane만 표시",
            )),
            PaneState::Loaded { .. } => None,
        };
        let target = self.context.as_ref().map(|name| match self.language {
            Language::English => format!("Actions apply to {name}"),
            Language::Korean => format!("작업 대상: {name}"),
        });
        let status = match (panes, target) {
            (Some(panes), Some(target)) => Some(format!("{target} · {panes}")),
            (Some(panes), None) => Some(panes.to_owned()),
            (None, target) => target,
        };
        let list_top = inner.y + 1;
        let list_height = inner
            .height
            .saturating_sub(1 + u16::from(status.is_some()))
            .max(1);
        self.page = list_height as usize;
        if self.selected >= self.shown.len() {
            self.selected = self.shown.len().saturating_sub(1);
        }
        self.reveal();
        if self.shown.is_empty() {
            frame.render_widget(
                Paragraph::new(ellipsize(
                    word(self.language, "No match", "일치하는 항목 없음"),
                    text_width,
                ))
                .style(muted),
                Rect::new(inner.x, list_top, inner.width, 1),
            );
        }
        let has_scroll = self.shown.len() > list_height as usize && text_width > 6;
        let row_text_width = text_width.saturating_sub(usize::from(has_scroll));
        let kind_width = [Kind::Agent, Kind::Pane, Kind::Action]
            .into_iter()
            .map(|kind| cell_width(self.kind_label(kind)))
            .max()
            .unwrap_or(0)
            + 1;
        for (row, position) in (self.scroll..self.shown.len())
            .take(list_height as usize)
            .enumerate()
        {
            let item = &self.items[self.shown[position]];
            let y = list_top + row as u16;
            let row_width = modal.width.saturating_sub(2 + u16::from(has_scroll));
            let rect = Rect::new(modal.x + 1, y, row_width, 1);
            let selected = position == self.selected;
            let hovered = self.hover == Some(Hit::Item(position));
            // A selected entry that cannot run keeps its muted text on the
            // panel. The marker, the underline and the hint show that it is
            // selected and that Enter does nothing now.
            let active = selected && item.enabled;
            let unavailable = selected && !item.enabled;
            let plain = |color| Style::default().fg(color);
            let bg = if active {
                accent
            } else if hovered && item.enabled {
                palette.raised_fill()
            } else {
                panel
            };
            let label_style = if active {
                plain(accent_text)
            } else if item.enabled {
                plain(text)
            } else {
                muted
            };
            let marker = if selected {
                "›"
            } else if hovered {
                "·"
            } else {
                " "
            };
            let hint = if unavailable {
                word(self.language, "unavailable", "지금은 불가")
            } else {
                item.hint.as_str()
            };
            let hint_width = cell_width(hint).min(row_text_width / 3);
            let body_width = row_text_width.saturating_sub(kind_width + hint_width + 1);
            let label = ellipsize(&item.label, body_width);
            let detail_width = body_width.saturating_sub(cell_width(&label) + 2);
            let detail = if detail_width > 3 {
                format!("  {}", ellipsize(&item.detail, detail_width))
            } else {
                String::new()
            };
            let used = kind_width + cell_width(&label) + cell_width(&detail);
            let gap = row_text_width.saturating_sub(used + hint_width);
            let secondary = if active { plain(accent_text) } else { muted };
            let edge = plain(if active { accent_text } else { accent });
            let base_label_style = label_style.add_modifier(if active {
                Modifier::BOLD
            } else if unavailable {
                Modifier::UNDERLINED
            } else {
                Modifier::empty()
            });
            let match_style = if active {
                plain(accent_text).add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
            } else if item.enabled {
                plain(accent).add_modifier(Modifier::BOLD)
            } else {
                base_label_style
            };
            let mut spans = vec![
                Span::styled(format!("{marker} "), edge),
                Span::styled(
                    {
                        let kind = self.kind_label(item.kind);
                        format!(
                            "{kind}{}",
                            " ".repeat(kind_width.saturating_sub(cell_width(kind)))
                        )
                    },
                    secondary,
                ),
            ];
            let mask = finder::match_mask(&self.query, &label);
            if mask.iter().any(|&matched| matched) {
                let mut run = String::new();
                let mut current = false;
                for (ch, matched) in label.chars().zip(mask) {
                    if !run.is_empty() && matched != current {
                        spans.push(Span::styled(
                            std::mem::take(&mut run),
                            if current {
                                match_style
                            } else {
                                base_label_style
                            },
                        ));
                    }
                    current = matched;
                    run.push(ch);
                }
                if !run.is_empty() {
                    spans.push(Span::styled(
                        run,
                        if current {
                            match_style
                        } else {
                            base_label_style
                        },
                    ));
                }
            } else {
                spans.push(Span::styled(label, base_label_style));
            }
            spans.push(Span::styled(detail, secondary));
            spans.push(Span::raw(" ".repeat(gap)));
            spans.push(Span::styled(
                ellipsize(hint, hint_width),
                if unavailable { muted } else { edge },
            ));
            frame.render_widget(
                Paragraph::new(Line::from(spans)).style(Style::default().bg(bg)),
                rect,
            );
            if !active && hovered && item.enabled {
                palette.raise(frame, rect);
            }
            self.hits.push((rect, Hit::Item(position)));
        }
        if has_scroll {
            let track = Rect::new(modal.right().saturating_sub(2), list_top, 1, list_height);
            frame.render_widget(
                Block::default().style(Style::default().bg(palette.track)),
                track,
            );
            let total = self.shown.len();
            let page = list_height as usize;
            let thumb_height = ((list_height as usize * page) / total)
                .max(1)
                .min(list_height as usize);
            let max_offset = total.saturating_sub(page).max(1);
            let travel = list_height as usize - thumb_height;
            let thumb_y = track.y + (travel * self.scroll.min(max_offset) / max_offset) as u16;
            frame.render_widget(
                Block::default().style(Style::default().bg(accent)),
                Rect::new(track.x, thumb_y, 1, thumb_height as u16),
            );
        }
        // Hidden entries are marked in the border.
        for (hidden, y) in [
            (self.scroll > 0, modal.y),
            (
                self.scroll + (list_height as usize) < self.shown.len(),
                modal.bottom().saturating_sub(1),
            ),
        ] {
            if hidden && modal.width > 10 {
                frame.render_widget(
                    Paragraph::new(if y == modal.y { "↑" } else { "↓" })
                        .style(Style::default().fg(accent).bg(panel)),
                    Rect::new(modal.right().saturating_sub(9), y, 1, 1),
                );
            }
        }
        if let Some(status) = status {
            frame.render_widget(
                Paragraph::new(ellipsize(&status, text_width)).style(muted),
                Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyEventState, MouseEvent};

    fn item(kind: Kind, label: &str, enabled: bool) -> Item {
        Item {
            kind,
            label: label.into(),
            detail: String::new(),
            hint: String::new(),
            target: Target::Action(Action::NewAgent),
            enabled,
            pane: None,
        }
    }

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        })
    }

    #[test]
    fn typing_ranks_matches_and_keeps_every_word() {
        let mut palette = Palette::new(
            Language::English,
            vec![
                item(Kind::Action, "New agent", true),
                item(Kind::Agent, "api-review", true),
                item(Kind::Agent, "builder", true),
            ],
            None,
        );
        assert_eq!(palette.shown.len(), 3);
        for character in "api".chars() {
            palette.handle(key(KeyCode::Char(character)), Instant::now());
        }
        assert_eq!(palette.shown, vec![1]);
        palette.handle(key(KeyCode::Backspace), Instant::now());
        palette.handle(key(KeyCode::Backspace), Instant::now());
        palette.handle(key(KeyCode::Backspace), Instant::now());
        assert_eq!(palette.shown.len(), 3);
    }

    #[test]
    fn disabled_entries_stay_listed_but_never_activate() {
        let mut palette = Palette::new(
            Language::Korean,
            vec![item(Kind::Action, "Close pane", false)],
            Some("builder".into()),
        );
        assert!(matches!(
            palette.handle(key(KeyCode::Enter), Instant::now()),
            PaletteResult::None
        ));
        assert!(matches!(
            palette.handle(key(KeyCode::Esc), Instant::now()),
            PaletteResult::Close
        ));
    }

    #[test]
    fn a_click_activates_only_when_press_and_release_share_the_target() {
        let mut palette = Palette::new(
            Language::English,
            vec![item(Kind::Action, "New agent", true)],
            None,
        );
        palette.hits = vec![
            (Rect::new(0, 0, 40, 10), Hit::Surface),
            (Rect::new(1, 2, 38, 1), Hit::Item(0)),
        ];
        // Row 5 is the modal surface: consumed, nothing happens.
        let mouse = |kind, row| {
            Event::Mouse(MouseEvent {
                kind,
                column: 5,
                row,
                modifiers: KeyModifiers::NONE,
            })
        };
        let now = Instant::now();
        palette.handle(mouse(MouseEventKind::Down(MouseButton::Left), 2), now);
        assert!(matches!(
            palette.handle(mouse(MouseEventKind::Up(MouseButton::Left), 5), now),
            PaletteResult::None
        ));
        palette.handle(mouse(MouseEventKind::Down(MouseButton::Left), 2), now);
        assert!(matches!(
            palette.handle(mouse(MouseEventKind::Up(MouseButton::Left), 2), now),
            PaletteResult::Activate(_)
        ));
        // Outside every region: closes.
        palette.handle(mouse(MouseEventKind::Down(MouseButton::Left), 20), now);
        assert!(matches!(
            palette.handle(mouse(MouseEventKind::Up(MouseButton::Left), 20), now),
            PaletteResult::Close
        ));
    }

    fn drawn(palette: &mut Palette, width: u16, height: u16) -> (Vec<String>, (u16, u16)) {
        use ratatui::{Terminal, backend::TestBackend};
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| palette.draw(frame, Theme::Dark))
            .unwrap();
        let cursor = terminal.get_cursor_position().unwrap();
        let buffer = terminal.backend().buffer();
        let rows = (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect();
        (rows, (cursor.x, cursor.y))
    }

    #[test]
    fn a_selected_entry_that_cannot_run_says_so() {
        let mut blocked = item(Kind::Action, "Close pane", false);
        blocked.hint = "X".into();
        for (language, wanted) in [
            (Language::English, "unavailable"),
            (Language::Korean, "지금은불가"),
        ] {
            let mut palette = Palette::new(language, vec![blocked.clone()], None);
            let (rows, _) = drawn(&mut palette, 60, 12);
            let row = rows
                .iter()
                .find(|row| row.contains("Close pane"))
                .expect("the entry is listed");
            assert!(row.contains('›'), "{row}");
            // A wide character leaves a blank cell after it.
            assert!(row.replace(' ', "").contains(wanted), "{row}");
        }
        // An entry that can run keeps its own key in the hint column.
        let mut ready = item(Kind::Action, "New agent", true);
        ready.hint = "n".into();
        let mut palette = Palette::new(Language::English, vec![ready], None);
        let (rows, _) = drawn(&mut palette, 60, 12);
        assert!(!rows.iter().any(|row| row.contains("unavailable")));
    }

    #[test]
    fn a_long_query_keeps_its_caret_and_the_terminal_cursor_on_it() {
        let mut palette = Palette::new(Language::English, Vec::new(), None);
        for query in ["ab", &"abcdefghij".repeat(12)] {
            palette.query = query.to_owned();
            let (rows, (x, y)) = drawn(&mut palette, 40, 12);
            let line = rows[y as usize].chars().collect::<Vec<_>>();
            assert_eq!(line[x as usize], '▏', "{}", rows[y as usize]);
            assert_eq!(line[x as usize - 1], query.chars().last().unwrap());
        }
        assert!(
            drawn(&mut palette, 40, 12)
                .0
                .iter()
                .any(|row| row.contains('…'))
        );
        // An empty query puts the cursor where typing starts.
        palette.query.clear();
        let (rows, (x, y)) = drawn(&mut palette, 40, 12);
        let line = rows[y as usize].chars().collect::<Vec<_>>();
        assert_eq!(line[x as usize - 2], '›', "{}", rows[y as usize]);
    }

    #[test]
    fn the_query_is_bounded_and_drops_control_characters() {
        let mut palette = Palette::new(Language::English, Vec::new(), None);
        palette.handle(Event::Paste("a\u{1b}b".into()), Instant::now());
        assert_eq!(palette.query, "ab");
        palette.handle(Event::Paste("가".repeat(400)), Instant::now());
        assert_eq!(palette.query.graphemes(true).count(), QUERY_GRAPHEMES);
        assert!(palette.query.len() <= QUERY_BYTES);
    }

    #[test]
    fn inline_cursor_editing_inserts_deletes_and_kills_words() {
        let mut palette = Palette::new(Language::English, Vec::new(), None);
        let now = Instant::now();
        palette.handle(Event::Paste("alpha gamma".into()), now);
        // Move left past "gamma" (5 chars) and insert "beta ".
        for _ in 0..5 {
            palette.handle(key(KeyCode::Left), now);
        }
        palette.handle(Event::Paste("beta ".into()), now);
        assert_eq!(palette.query, "alpha beta gamma");
        // Ctrl-W deletes "beta ".
        palette.handle(
            Event::Key(KeyEvent {
                code: KeyCode::Char('w'),
                modifiers: KeyModifiers::CONTROL,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            }),
            now,
        );
        assert_eq!(palette.query, "alpha gamma");
        // Delete removes 'g'.
        palette.handle(key(KeyCode::Delete), now);
        assert_eq!(palette.query, "alpha amma");
    }
}
