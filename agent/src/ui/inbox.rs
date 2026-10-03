//! The management window's inbox view: this server's unseen attention
//! events, focus on an event's pane, and marking events read. It is a body
//! mode, not an overlay, so it stays open while polls change the selected
//! agent.

use super::model::{App, Effect, HitRegion, HitTarget, Language, Overlay};
use super::view::{Palette, clip_line, inset_x, pad, utc_timestamp};
use crate::managed::inbox::{InboxItem, InboxView};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;
use ratatui::widgets::{Block, Paragraph};
use std::time::{Duration, Instant};

/// How long the second `A` that confirms "mark all read" may wait.
const CONFIRM: Duration = Duration::from_secs(3);

#[derive(Debug, Default)]
pub(crate) struct InboxState {
    pub(crate) open: bool,
    /// Recent events of every kind, not only unseen ones.
    pub(crate) all: bool,
    pub(crate) view: InboxView,
    /// This window's state directory is not the server's: the inbox it
    /// could read is not the one the server writes.
    pub(crate) mismatch: bool,
    /// The selected event, kept by ID across refreshes.
    pub(crate) selected: Option<i64>,
    pub(crate) scroll: usize,
    pub(crate) page: usize,
    /// Until when a second `A` confirms, and the fence the first one took.
    pub(crate) confirm: Option<(Instant, i64)>,
    /// Read the inbox again without waiting for the next poll.
    pub(crate) refresh: bool,
    /// (pane, run, name) of local agents, to name the event rows.
    pub(crate) names: Vec<(String, String, String)>,
}

impl InboxState {
    /// Takes a newly read inbox. Returns whether anything shown changed.
    pub(crate) fn apply(&mut self, view: InboxView) -> bool {
        if view == self.view {
            return false;
        }
        let position = self.selected_index();
        self.view = view;
        // Keep the selection on the same event; when it left the list, take
        // the event now in its place.
        if self.selected_index().is_none() {
            self.selected = position
                .map(|index| index.min(self.view.events.len().saturating_sub(1)))
                .and_then(|index| self.view.events.get(index))
                .or_else(|| self.view.events.first())
                .map(|event| event.id);
        }
        true
    }

    /// Takes the local agents' names. Returns whether they changed.
    pub(crate) fn set_names(&mut self, names: Vec<(String, String, String)>) -> bool {
        if names == self.names {
            return false;
        }
        self.names = names;
        true
    }

    fn selected_index(&self) -> Option<usize> {
        let id = self.selected?;
        self.view.events.iter().position(|event| event.id == id)
    }

    pub(crate) fn selected_item(&self) -> Option<&InboxItem> {
        self.selected_index()
            .and_then(|index| self.view.events.get(index))
    }

    fn move_selection(&mut self, delta: isize) {
        let count = self.view.events.len();
        if count == 0 {
            return;
        }
        let current = self.selected_index().unwrap_or(0) as isize;
        let index = (current + delta).clamp(0, count as isize - 1) as usize;
        self.selected = Some(self.view.events[index].id);
    }

    fn select_position(&mut self, index: usize) {
        if let Some(event) = self
            .view
            .events
            .get(index.min(self.view.events.len().saturating_sub(1)))
        {
            self.selected = Some(event.id);
        }
    }

    /// The oldest event that is neither read nor resolved, preferring one
    /// whose agent still runs: a closed pane's event cannot be focused.
    pub(crate) fn oldest_unseen(&self) -> Option<&InboxItem> {
        let live = |event: &&InboxItem| {
            self.names
                .iter()
                .any(|(pane, run, _)| *pane == event.pane && *run == event.run)
        };
        let mut unseen = self.view.events.iter().filter(|event| waiting(event));
        unseen.clone().find(live).or_else(|| unseen.next())
    }

    /// The newest sequence this window displayed.
    pub(crate) fn newest_seq(&self) -> Option<i64> {
        self.view.events.iter().map(|event| event.seq).max()
    }

    /// The newest sequence displayed for a run, if any.
    pub(crate) fn newest_seq_of(&self, run: &str) -> Option<i64> {
        self.view
            .events
            .iter()
            .filter(|event| event.run == run)
            .map(|event| event.seq)
            .max()
    }
}

fn waiting(event: &InboxItem) -> bool {
    !event.read && event.resolution.is_none()
}

fn word(language: Language, english: &'static str, korean: &'static str) -> &'static str {
    match language {
        Language::English => english,
        Language::Korean => korean,
    }
}

pub(crate) fn kind_label(language: Language, kind: &str) -> &'static str {
    match kind {
        "approval_requested" => word(language, "Approval", "승인 필요"),
        "question_asked" => word(language, "Question", "질문"),
        "blocked" => word(language, "Needs input", "입력 필요"),
        "turn_completed" => word(language, "Turn done", "턴 끝"),
        "returned_idle" => word(language, "Back to idle", "idle 복귀"),
        "operation_unknown" => word(language, "Outcome unknown", "결과 모름"),
        "error" => word(language, "Error", "오류"),
        "observation_lost" => word(language, "Not observed", "관찰 끊김"),
        "missed_schedule" => word(language, "Schedule missed", "예약 놓침"),
        _ => word(language, "Event", "사건"),
    }
}

fn resolution_label(language: Language, resolution: &str) -> &'static str {
    match resolution {
        "replied" => word(language, "answered", "응답함"),
        "answered" => word(language, "answered in masil", "masil에서 응답함"),
        "restored" => word(language, "observed again", "다시 관찰함"),
        "superseded" => word(language, "answered with another", "함께 처리됨"),
        "rejected" => word(language, "rejected", "거절함"),
        "decided" => word(language, "decided", "결정됨"),
        "left_blocked" => word(language, "no longer waiting", "대기 끝"),
        "turn_completed" | "next_turn" => word(language, "turn moved on", "턴 진행"),
        "covered" => word(language, "covered by a hook", "hook이 대신함"),
        "run_ended" => word(language, "run ended", "run 끝"),
        "settled" => word(language, "settled", "정함"),
        _ => word(language, "resolved", "해결됨"),
    }
}

/// Tool, command, path or permission: what the summary allowlist kept.
fn summary_text(event: &InboxItem) -> String {
    let Some(summary) = &event.summary else {
        return String::new();
    };
    let field = |name: &str| summary.get(name).and_then(|value| value.as_str());
    let detail = field("command")
        .or_else(|| field("file_path"))
        .or_else(|| field("url"))
        .or_else(|| field("permission"))
        .or_else(|| field("action"))
        .or_else(|| field("schedule"));
    match (field("tool"), detail) {
        (Some(tool), Some(detail)) => format!("{tool}: {detail}"),
        (Some(tool), None) => tool.to_owned(),
        (None, Some(detail)) => detail.to_owned(),
        (None, None) => String::new(),
    }
}

impl App {
    /// "Inbox N" for the header, while the inbox is on.
    pub(crate) fn inbox_label(&self) -> Option<String> {
        if !self.managed {
            return None;
        }
        if self.inbox.mismatch {
            return Some(
                word(
                    self.language,
                    "Inbox: other state dir",
                    "인박스: 다른 state 위치",
                )
                .to_owned(),
            );
        }
        self.inbox.view.enabled.then(|| {
            format!(
                "{} {}",
                word(self.language, "Inbox", "인박스"),
                self.inbox.view.unseen
            )
        })
    }

    pub(crate) fn toggle_inbox(&mut self) {
        self.inbox.open = !self.inbox.open;
        self.inbox.confirm = None;
        if self.inbox.open {
            self.inbox.refresh = true;
            if self.inbox.selected_index().is_none() {
                self.inbox.select_position(0);
            }
        }
        self.dirty = true;
    }

    /// Focus the oldest unseen event's pane, from any view. Reads nothing.
    pub(crate) fn next_unseen(&mut self) -> Vec<Effect> {
        match self.inbox.oldest_unseen() {
            Some(event) => {
                let effect = Effect::InboxFocus {
                    pane: event.pane.clone(),
                    run: event.run.clone(),
                };
                self.inbox.selected = Some(event.id);
                self.dirty = true;
                vec![effect]
            }
            None => {
                self.toast = Some(
                    word(self.language, "No unseen events", "안 읽은 사건이 없습니다").to_owned(),
                );
                self.dirty = true;
                Vec::new()
            }
        }
    }

    pub(crate) fn handle_inbox_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let confirming = self
            .inbox
            .confirm
            .filter(|(until, _)| Instant::now() < *until)
            .map(|(_, through)| through);
        if key.code != KeyCode::Char('A') && self.inbox.confirm.take().is_some() {
            self.toast = None;
            self.dirty = true;
        }
        let page = self.inbox.page.max(1) as isize;
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.inbox.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.inbox.move_selection(1),
            KeyCode::PageUp => self.inbox.move_selection(-page),
            KeyCode::PageDown => self.inbox.move_selection(page),
            KeyCode::Home => self.inbox.select_position(0),
            KeyCode::End => self
                .inbox
                .select_position(self.inbox.view.events.len().saturating_sub(1)),
            KeyCode::Enter | KeyCode::Char('g') => {
                return self
                    .inbox
                    .selected_item()
                    .map(|event| Effect::InboxFocus {
                        pane: event.pane.clone(),
                        run: event.run.clone(),
                    })
                    .into_iter()
                    .collect();
            }
            KeyCode::Char('y') => {
                return self
                    .inbox
                    .selected_item()
                    .map(|event| Effect::InboxAnswer {
                        pane: event.pane.clone(),
                        run: event.run.clone(),
                    })
                    .into_iter()
                    .collect();
            }
            KeyCode::Char('a') => {
                return self
                    .inbox
                    .selected_item()
                    .filter(|event| !event.read)
                    .map(|event| Effect::InboxRead { id: event.id })
                    .into_iter()
                    .collect();
            }
            KeyCode::Char('A') => {
                // The fence is what the first press saw; events that arrive
                // before the second stay unread.
                if let Some(through) = confirming {
                    self.inbox.confirm = None;
                    self.toast = None;
                    self.dirty = true;
                    return vec![Effect::InboxReadAll { through }];
                }
                let Some(through) = self.inbox.newest_seq() else {
                    return Vec::new();
                };
                self.inbox.confirm = Some((Instant::now() + CONFIRM, through));
                self.toast = Some(
                    word(
                        self.language,
                        "Press A again to mark every listed event read",
                        "A를 한 번 더 누르면 보이는 사건을 모두 읽음으로 바꿉니다",
                    )
                    .to_owned(),
                );
            }
            KeyCode::Char('o') => {
                self.inbox.all = !self.inbox.all;
                self.inbox.refresh = true;
            }
            KeyCode::Char('u') => return self.next_unseen(),
            KeyCode::Char('i') | KeyCode::Esc => self.toggle_inbox(),
            KeyCode::Char('?') => {
                self.overlay = Some(Overlay::Help);
                self.help_scroll = 0;
            }
            KeyCode::Char('q') => return vec![Effect::Quit],
            KeyCode::Char('l') => return self.change_language(),
            KeyCode::Char('t') => return self.change_theme(),
            // Agent actions would act on a selection this view hides.
            _ => return Vec::new(),
        }
        self.dirty = true;
        Vec::new()
    }

    /// Clicking an event selects it; clicking it again focuses its pane.
    /// The mouse wheel moves the inbox selection.
    pub(crate) fn scroll_inbox(&mut self, delta: isize) {
        self.inbox.move_selection(delta);
        self.dirty = true;
    }

    pub(crate) fn click_inbox_row(&mut self, id: i64, repeat: bool) -> Vec<Effect> {
        self.dirty = true;
        if repeat && self.inbox.selected == Some(id) {
            return self
                .inbox
                .selected_item()
                .map(|event| Effect::InboxFocus {
                    pane: event.pane.clone(),
                    run: event.run.clone(),
                })
                .into_iter()
                .collect();
        }
        self.inbox.selected = Some(id);
        Vec::new()
    }

    pub(super) fn draw_inbox(&mut self, frame: &mut Frame, area: Rect, palette: Palette) {
        frame.render_widget(
            Block::default().style(Style::default().bg(palette.canvas)),
            area,
        );
        let narrow = area.width < 60;
        let inner = inset_x(area, u16::from(!narrow));
        if inner.height == 0 {
            return;
        }
        let scope = word(
            self.language,
            if self.inbox.all {
                "Recent events on this server"
            } else {
                "Unseen events on this server"
            },
            if self.inbox.all {
                "이 서버의 최근 사건"
            } else {
                "이 서버의 안 읽은 사건"
            },
        );
        frame.render_widget(
            Paragraph::new(clip_line(
                vec![Span::styled(
                    scope.to_owned(),
                    Style::default().fg(palette.muted),
                )],
                inner.width as usize,
            )),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );
        let list = Rect::new(
            inner.x,
            inner.y + 1,
            inner.width,
            inner.height.saturating_sub(1),
        );
        let message = if self.inbox.mismatch {
            Some(word(
                self.language,
                "This window uses another state directory than the server; run it with the server's XDG_STATE_HOME and HOME.",
                "이 창은 서버와 다른 state 위치를 씁니다. 서버의 XDG_STATE_HOME과 HOME으로 여세요.",
            ))
        } else if !self.inbox.view.enabled {
            Some(word(
                self.language,
                "The inbox is off. Turn it on with: masil-agent agent inbox enable",
                "인박스가 꺼져 있습니다. 켜기: masil-agent agent inbox enable",
            ))
        } else if self.inbox.view.events.is_empty() {
            Some(word(
                self.language,
                "Nothing to see.",
                "볼 사건이 없습니다.",
            ))
        } else {
            None
        };
        if let Some(message) = message {
            // Wrapped: on a narrow screen the command is on the next lines,
            // not cut off.
            frame.render_widget(
                Paragraph::new(Span::styled(
                    message.to_owned(),
                    Style::default().fg(palette.text),
                ))
                .wrap(ratatui::widgets::Wrap { trim: true }),
                list,
            );
            return;
        }
        let row_height: u16 = if narrow { 2 } else { 1 };
        let mut hits = Vec::new();
        let page = (list.height / row_height).max(1) as usize;
        self.inbox.page = page;
        let selected = self.inbox.selected_index().unwrap_or(0);
        if selected < self.inbox.scroll {
            self.inbox.scroll = selected;
        } else if selected >= self.inbox.scroll + page {
            self.inbox.scroll = selected + 1 - page;
        }
        let names = &self.inbox.names;
        let language = self.language;
        for (offset, event) in self
            .inbox
            .view
            .events
            .iter()
            .skip(self.inbox.scroll)
            .take(page)
            .enumerate()
        {
            let y = list.y + offset as u16 * row_height;
            let rect = Rect::new(list.x, y, list.width, row_height);
            let is_selected = self.inbox.selected == Some(event.id);
            let base = if is_selected {
                Style::default().bg(palette.raised).fg(palette.text)
            } else {
                Style::default().fg(palette.text)
            };
            let dim = event.read || event.resolution.is_some();
            let text_style = if dim { base.fg(palette.muted) } else { base };
            let mark = if waiting(event) { "● " } else { "  " };
            let time = utc_timestamp(event.observed_ms)
                .get(11..19)
                .unwrap_or("")
                .to_owned();
            let kind = kind_label(language, &event.kind);
            let agent = names
                .iter()
                .find(|(pane, run, _)| *pane == event.pane && *run == event.run)
                .map_or(event.pane.as_str(), |(_, _, name)| name.as_str())
                .to_owned();
            let mut detail = summary_text(event);
            if let Some(resolution) = &event.resolution {
                let label = resolution_label(language, resolution);
                detail = if detail.is_empty() {
                    format!("({label})")
                } else {
                    format!("{detail} ({label})")
                };
            }
            let kind_style = if dim {
                text_style
            } else {
                text_style.fg(palette.warning).add_modifier(Modifier::BOLD)
            };
            let lines = if narrow {
                vec![
                    clip_line(
                        vec![
                            Span::styled(mark.to_owned(), kind_style),
                            Span::styled(format!("{kind}  "), kind_style),
                            Span::styled(agent, text_style),
                        ],
                        rect.width as usize,
                    ),
                    clip_line(
                        vec![Span::styled(
                            format!("  {}  {detail}", event.source),
                            text_style.fg(palette.muted),
                        )],
                        rect.width as usize,
                    ),
                ]
            } else {
                let kind_width = 16.min(rect.width as usize);
                vec![clip_line(
                    vec![
                        Span::styled(mark.to_owned(), kind_style),
                        Span::styled(format!("{time}  "), text_style.fg(palette.muted)),
                        Span::styled(pad(kind, kind_width), kind_style),
                        Span::styled(format!(" {} ", pad(&agent, 16)), text_style),
                        Span::styled(
                            format!("{} ", pad(&event.source, 14)),
                            text_style.fg(palette.muted),
                        ),
                        Span::styled(detail, text_style),
                    ],
                    rect.width as usize,
                )]
            };
            frame.render_widget(Paragraph::new(lines).style(base), rect);
            hits.push(HitRegion {
                rect,
                target: HitTarget::InboxRow(event.id),
                z: 0,
            });
        }
        self.hits.extend(hits);
    }

    /// The key line for the inbox view.
    pub(crate) fn inbox_hint(&self) -> &'static str {
        word(
            self.language,
            "Enter/g focus · y answer · a read · A all read · o all/unseen · u next unseen · i close",
            "Enter/g 이동 · y 응답 · a 읽음 · A 모두 읽음 · o 전체/안 읽음 · u 다음 · i 닫기",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: i64, seq: i64, run: &str, kind: &str) -> InboxItem {
        InboxItem {
            id,
            seq,
            source: "screen".into(),
            pane: "%1".into(),
            run: run.into(),
            revision: Some(seq),
            kind: kind.into(),
            summary: None,
            observed_ms: 1_000,
            read: false,
            resolution: None,
        }
    }

    fn view(events: Vec<InboxItem>) -> InboxView {
        InboxView {
            enabled: true,
            unseen: events.len() as i64,
            events,
        }
    }

    #[test]
    fn the_same_inbox_changes_nothing_and_selection_follows_the_event() {
        let mut state = InboxState::default();
        assert!(state.apply(view(vec![
            item(1, 10, "r", "blocked"),
            item(2, 11, "r", "blocked")
        ])));
        state.selected = Some(2);
        assert!(!state.apply(view(vec![
            item(1, 10, "r", "blocked"),
            item(2, 11, "r", "blocked")
        ])));
        // Event 1 resolved and left: the selection stays on event 2.
        assert!(state.apply(view(vec![
            item(2, 11, "r", "blocked"),
            item(3, 12, "r", "blocked")
        ])));
        assert_eq!(state.selected, Some(2));
        // Event 2 left: the event now in its place is selected.
        assert!(state.apply(view(vec![item(3, 12, "r", "blocked")])));
        assert_eq!(state.selected, Some(3));
    }

    #[test]
    fn run_reads_cover_only_what_was_shown() {
        let mut state = InboxState::default();
        let mut operation = item(4, 14, "r", "operation_unknown");
        operation.source = "operation".into();
        state.apply(view(vec![
            item(1, 10, "r", "blocked"),
            item(2, 11, "other", "blocked"),
            operation,
        ]));
        assert_eq!(state.newest_seq_of("r"), Some(14));
        assert_eq!(state.newest_seq(), Some(14));
        state.apply(view(vec![
            item(1, 10, "r", "blocked"),
            item(5, 15, "r", "approval_requested"),
        ]));
        assert_eq!(state.oldest_unseen().map(|event| event.id), Some(1));
        // An event of an agent still running comes before a closed pane's.
        let mut closed = item(0, 9, "gone", "blocked");
        closed.pane = "%9".into();
        state.apply(view(vec![closed, item(1, 10, "r", "blocked")]));
        assert_eq!(state.oldest_unseen().map(|event| event.id), Some(0));
        state.set_names(vec![("%1".into(), "r".into(), "builder".into())]);
        assert_eq!(state.oldest_unseen().map(|event| event.id), Some(1));
    }

    #[test]
    fn keys_in_the_inbox_act_on_events_and_ignore_agent_actions() {
        use crossterm::event::{KeyEvent, KeyModifiers};
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let mut app = App::new(false, Language::English, super::super::model::Theme::Dark);
        app.managed = true;
        app.inbox.apply(view(vec![
            item(1, 10, "r", "blocked"),
            item(2, 11, "r", "blocked"),
        ]));
        app.toggle_inbox();
        assert_eq!(app.inbox.selected, Some(1));
        app.handle_inbox_key(key(KeyCode::Down));
        assert_eq!(app.inbox.selected, Some(2));
        assert!(matches!(
            app.handle_inbox_key(key(KeyCode::Char('a'))).as_slice(),
            [Effect::InboxRead { id: 2 }]
        ));
        assert!(matches!(
            app.handle_inbox_key(key(KeyCode::Enter)).as_slice(),
            [Effect::InboxFocus { pane, run }] if pane == "%1" && run == "r"
        ));
        // A once asks, twice confirms, with the fence of the first press.
        assert!(app.handle_inbox_key(key(KeyCode::Char('A'))).is_empty());
        app.inbox.apply(view(vec![
            item(1, 10, "r", "blocked"),
            item(2, 11, "r", "blocked"),
            item(3, 12, "r", "blocked"),
        ]));
        assert!(matches!(
            app.handle_inbox_key(key(KeyCode::Char('A'))).as_slice(),
            [Effect::InboxReadAll { through: 11 }]
        ));
        assert!(app.handle_inbox_key(key(KeyCode::Char('X'))).is_empty());
        assert!(app.handle_inbox_key(key(KeyCode::Char('p'))).is_empty());
        app.handle_inbox_key(key(KeyCode::Esc));
        assert!(!app.inbox.open);
    }
}
