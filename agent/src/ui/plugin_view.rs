//! Interactive Ratatui overlay rendering any Masil UI Spec v1 document
//! (workspace panels and slash-command UI outputs) inside the native TUI.

use super::{
    model::{Language, Theme},
    view::{Palette, Tone, ellipsize},
};
use crate::plugin::ui_spec::{UiBlock, UiDocument, UiTone};
use crossterm::event::{Event, KeyCode, KeyModifiers, MouseButton, MouseEventKind};
use futures_util::StreamExt;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
struct ActionHit {
    line: usize,
    col_start: usize,
    col_end: usize,
    action_idx: usize,
}

#[derive(Clone, Debug)]
pub(super) struct ActionBinding {
    pub action: String,
    pub payload: Option<Value>,
}

pub(super) enum PluginViewOutcome {
    None,
    Close,
    Refresh,
    Invoke {
        plugin_id: String,
        panel_id: Option<String>,
        command_id: Option<String>,
        command_args: String,
        action: String,
        payload: Option<Value>,
    },
}

pub(super) struct PluginViewOverlay {
    pub language: Language,
    pub plugin_id: String,
    pub panel_id: Option<String>,
    pub command_id: Option<String>,
    pub command_args: String,
    pub refresh_secs: Option<u64>,
    pub last_refresh: Instant,
    pub title: String,
    pub agent: Option<Value>,
    pub doc: Option<UiDocument>,
    pub loading: bool,
    pub error: Option<String>,
    pub scroll: usize,
    pub selected_action: usize,
    pub fullscreen: bool,
    actions: Vec<ActionBinding>,
    action_hits: Vec<ActionHit>,
    inner_rect: Rect,
    page: usize,
}

impl PluginViewOverlay {
    pub(super) fn loading(
        language: Language,
        plugin_id: String,
        panel_id: Option<String>,
        refresh_secs: Option<u64>,
        title: String,
        agent: Option<Value>,
    ) -> Self {
        Self {
            language,
            plugin_id,
            panel_id,
            command_id: None,
            command_args: String::new(),
            refresh_secs,
            last_refresh: Instant::now(),
            title,
            agent,
            doc: None,
            loading: true,
            error: None,
            scroll: 0,
            selected_action: 0,
            fullscreen: false,
            actions: Vec::new(),
            action_hits: Vec::new(),
            inner_rect: Rect::default(),
            page: 12,
        }
    }

    pub(super) fn with_command_document(
        language: Language,
        plugin_id: String,
        command_id: String,
        command_args: String,
        title: String,
        agent: Option<Value>,
        doc: UiDocument,
    ) -> Self {
        let mut view = Self::loading(language, plugin_id, None, None, title, agent);
        view.command_id = Some(command_id);
        view.command_args = command_args;
        view.set_document(Ok(doc));
        view
    }

    pub(super) fn should_auto_refresh(&self, now: Instant) -> bool {
        if self.loading || self.panel_id.is_none() {
            return false;
        }
        let Some(secs) = self.refresh_secs.filter(|&s| s > 0) else {
            return false;
        };
        now.duration_since(self.last_refresh) >= Duration::from_secs(secs.max(3))
    }

    pub(super) fn mark_refresh_started(&mut self, now: Instant) {
        self.loading = true;
        self.last_refresh = now;
    }

    pub(super) fn set_document(&mut self, result: Result<UiDocument, String>) {
        self.loading = false;
        self.last_refresh = Instant::now();
        match result {
            Ok(doc) => {
                if let Some(t) = &doc.title
                    && !t.trim().is_empty()
                {
                    self.title = t.clone();
                }
                self.actions = collect_actions(&doc);
                if self.selected_action >= self.actions.len() {
                    self.selected_action = 0;
                }
                self.doc = Some(doc);
                self.error = None;
            }
            Err(err) => {
                self.error = Some(err);
            }
        }
    }

    pub(super) fn handle(&mut self, event: Event) -> PluginViewOutcome {
        match event {
            Event::Key(key) => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => PluginViewOutcome::Close,
                KeyCode::Char('r') if self.panel_id.is_some() || self.command_id.is_some() => {
                    self.mark_refresh_started(Instant::now());
                    PluginViewOutcome::Refresh
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.scroll = self.scroll.saturating_add(1);
                    PluginViewOutcome::None
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.scroll = self.scroll.saturating_sub(1);
                    PluginViewOutcome::None
                }
                KeyCode::PageDown => {
                    self.scroll = self.scroll.saturating_add(self.page.max(1));
                    PluginViewOutcome::None
                }
                KeyCode::PageUp => {
                    self.scroll = self.scroll.saturating_sub(self.page.max(1));
                    PluginViewOutcome::None
                }
                KeyCode::Tab | KeyCode::Right => {
                    if !self.actions.is_empty() {
                        self.selected_action = (self.selected_action + 1) % self.actions.len();
                    }
                    PluginViewOutcome::None
                }
                KeyCode::BackTab | KeyCode::Left => {
                    if !self.actions.is_empty() {
                        self.selected_action = self
                            .selected_action
                            .checked_sub(1)
                            .unwrap_or(self.actions.len() - 1);
                    }
                    PluginViewOutcome::None
                }
                KeyCode::Enter => {
                    if (self.panel_id.is_some() || self.command_id.is_some())
                        && let Some(binding) = self.actions.get(self.selected_action).cloned()
                    {
                        self.mark_refresh_started(Instant::now());
                        PluginViewOutcome::Invoke {
                            plugin_id: self.plugin_id.clone(),
                            panel_id: self.panel_id.clone(),
                            command_id: self.command_id.clone(),
                            command_args: self.command_args.clone(),
                            action: binding.action,
                            payload: binding.payload,
                        }
                    } else {
                        PluginViewOutcome::None
                    }
                }
                _ => PluginViewOutcome::None,
            },
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollDown => {
                    self.scroll = self.scroll.saturating_add(3);
                    PluginViewOutcome::None
                }
                MouseEventKind::ScrollUp => {
                    self.scroll = self.scroll.saturating_sub(3);
                    PluginViewOutcome::None
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    if mouse.column >= self.inner_rect.x
                        && mouse.column < self.inner_rect.right()
                        && mouse.row >= self.inner_rect.y
                        && mouse.row < self.inner_rect.bottom()
                    {
                        let clicked_line = self.scroll + (mouse.row - self.inner_rect.y) as usize;
                        let clicked_col = (mouse.column - self.inner_rect.x) as usize;
                        if let Some(hit) = self.action_hits.iter().find(|h| {
                            h.line == clicked_line
                                && clicked_col >= h.col_start
                                && clicked_col < h.col_end
                        }) {
                            self.selected_action = hit.action_idx;
                            if (self.panel_id.is_some() || self.command_id.is_some())
                                && let Some(binding) =
                                    self.actions.get(self.selected_action).cloned()
                            {
                                self.mark_refresh_started(Instant::now());
                                return PluginViewOutcome::Invoke {
                                    plugin_id: self.plugin_id.clone(),
                                    panel_id: self.panel_id.clone(),
                                    command_id: self.command_id.clone(),
                                    command_args: self.command_args.clone(),
                                    action: binding.action,
                                    payload: binding.payload,
                                };
                            }
                        }
                    }
                    PluginViewOutcome::None
                }
                _ => PluginViewOutcome::None,
            },
            _ => PluginViewOutcome::None,
        }
    }

    pub(super) fn draw(&mut self, frame: &mut Frame, theme: Theme) {
        let area = frame.area();
        let modal = if self.fullscreen {
            if area.width < 16 || area.height < 6 {
                return;
            }
            area
        } else {
            let width = area.width.saturating_sub(6).clamp(36, 108).min(area.width);
            let height = area.height.saturating_sub(4).clamp(10, 38).min(area.height);
            if width < 24 || height < 8 {
                return;
            }
            Rect::new(
                area.x + (area.width - width) / 2,
                area.y + (area.height - height) / 2,
                width,
                height,
            )
        };
        let width = modal.width;
        let palette = Palette::for_theme(theme);
        frame.render_widget(Clear, modal);

        let header_title = format!(" {} [{}] ", self.title, self.plugin_id);
        frame.render_widget(
            Block::default()
                .borders(Borders::ALL)
                .title(ellipsize(&header_title, width.saturating_sub(4) as usize))
                .border_style(Style::default().fg(palette.accent))
                .style(Style::default().bg(palette.panel).fg(palette.text)),
            modal,
        );

        let inner = Rect::new(
            modal.x + 2,
            modal.y + 1,
            modal.width.saturating_sub(4),
            modal.height.saturating_sub(3),
        );
        let footer = Rect::new(
            modal.x + 2,
            modal.bottom().saturating_sub(2),
            modal.width.saturating_sub(4),
            1,
        );
        self.inner_rect = inner;
        self.page = inner.height.max(1) as usize;

        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut hits: Vec<ActionHit> = Vec::new();
        let content_width = inner.width as usize;

        if self.loading && self.doc.is_none() {
            let msg = match self.language {
                Language::English => "Loading plugin view...",
                Language::Korean => "플러그인 화면 불러오는 중...",
            };
            lines.push(Line::styled(msg, palette.muted_style()));
        }
        if let Some(err) = &self.error {
            lines.push(Line::styled(
                format!("Error: {err}"),
                Style::default()
                    .fg(palette.danger)
                    .add_modifier(Modifier::BOLD),
            ));
            lines.push(Line::raw(""));
        }
        if let Some(doc) = &self.doc {
            if let Some(sub) = &doc.subtitle
                && !sub.trim().is_empty()
            {
                lines.push(Line::styled(sub.clone(), palette.muted_style()));
                lines.push(Line::raw(""));
            }
            let mut action_idx = 0usize;
            for block in &doc.blocks {
                render_block(
                    block,
                    content_width,
                    palette,
                    self.selected_action,
                    &mut action_idx,
                    &mut lines,
                    &mut hits,
                );
            }
        }
        self.action_hits = hits;

        let max_scroll = lines.len().saturating_sub(inner.height as usize);
        self.scroll = self.scroll.min(max_scroll);
        let visible: Vec<Line<'static>> = lines
            .into_iter()
            .skip(self.scroll)
            .take(inner.height as usize)
            .collect();
        frame.render_widget(Paragraph::new(visible), inner);

        let interactive = self.panel_id.is_some() || self.command_id.is_some();
        let hint = match (self.language, interactive, !self.actions.is_empty()) {
            (Language::Korean, true, true) => {
                "↑↓ 스크롤 · Tab 동작 선택 · Enter 실행 · r 새로고침 · Esc 닫기"
            }
            (Language::Korean, true, false) => "↑↓ 스크롤 · r 새로고침 · Esc 닫기",
            (Language::Korean, false, _) => "↑↓ 스크롤 · Esc 닫기",
            (Language::English, true, true) => {
                "↑↓ Scroll · Tab Focus action · Enter Run · r Refresh · Esc Close"
            }
            (Language::English, true, false) => "↑↓ Scroll · r Refresh · Esc Close",
            (Language::English, false, _) => "↑↓ Scroll · Esc Close",
        };
        frame.render_widget(
            Paragraph::new(ellipsize(hint, footer.width as usize)).style(palette.muted_style()),
            footer,
        );
    }
}

fn collect_actions(doc: &UiDocument) -> Vec<ActionBinding> {
    let mut out = Vec::new();
    for block in &doc.blocks {
        match block {
            UiBlock::Button {
                action, payload, ..
            } => out.push(ActionBinding {
                action: action.clone(),
                payload: payload.clone(),
            }),
            UiBlock::ButtonRow { buttons } => {
                for b in buttons {
                    out.push(ActionBinding {
                        action: b.action.clone(),
                        payload: b.payload.clone(),
                    });
                }
            }
            UiBlock::Toggle { value, action, .. } => out.push(ActionBinding {
                action: action.clone(),
                payload: Some(json!({ "value": !*value })),
            }),
            UiBlock::Select {
                value,
                options,
                action,
                ..
            } if !options.is_empty() => {
                let current_idx = options.iter().position(|o| o.value == *value).unwrap_or(0);
                let next_val = &options[(current_idx + 1) % options.len()].value;
                out.push(ActionBinding {
                    action: action.clone(),
                    payload: Some(json!({ "value": next_val })),
                });
            }
            UiBlock::List { items, .. } => {
                for item in items {
                    if let Some(action) = &item.action {
                        out.push(ActionBinding {
                            action: action.clone(),
                            payload: item.payload.clone(),
                        });
                    }
                }
            }
            _ => {}
        }
    }
    out
}

fn tone_style(palette: Palette, tone: UiTone) -> Style {
    match tone {
        UiTone::Normal => Style::default().fg(palette.text),
        UiTone::Muted => palette.muted_style(),
        UiTone::Accent => Style::default().fg(palette.accent),
        UiTone::Success => palette.tone_style(Tone::Good),
        UiTone::Warning => palette.tone_style(Tone::Warn),
        UiTone::Danger => palette.tone_style(Tone::Bad),
    }
}

fn sparkline_string(values: &[f64]) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let min = values.iter().copied().fold(f64::INFINITY, f64::min);
    let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let span = (max - min).max(f64::EPSILON);
    values
        .iter()
        .map(|v| {
            let norm = ((*v - min) / span).clamp(0.0, 1.0);
            let idx = ((norm * 7.0).round() as usize).min(7);
            BARS[idx]
        })
        .collect()
}

fn render_block(
    block: &UiBlock,
    width: usize,
    palette: Palette,
    selected_action: usize,
    action_idx: &mut usize,
    lines: &mut Vec<Line<'static>>,
    hits: &mut Vec<ActionHit>,
) {
    match block {
        UiBlock::Header {
            title,
            subtitle,
            badge,
            tone,
        } => {
            let mut spans = vec![Span::styled(
                title.clone(),
                Style::default()
                    .fg(palette.text)
                    .add_modifier(Modifier::BOLD),
            )];
            if let Some(b) = badge {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(
                    format!("[{b}]"),
                    tone_style(palette, *tone).add_modifier(Modifier::BOLD),
                ));
            }
            lines.push(Line::from(spans));
            if let Some(sub) = subtitle {
                lines.push(Line::styled(sub.clone(), palette.muted_style()));
            }
            lines.push(Line::raw(""));
        }
        UiBlock::Text { text, tone, .. } => {
            for l in text.lines() {
                lines.push(Line::styled(l.to_owned(), tone_style(palette, *tone)));
            }
            lines.push(Line::raw(""));
        }
        UiBlock::Markdown { markdown } => {
            for l in markdown.lines() {
                if let Some(h) = l.strip_prefix("# ") {
                    lines.push(Line::styled(
                        h.to_owned(),
                        Style::default()
                            .fg(palette.accent)
                            .add_modifier(Modifier::BOLD),
                    ));
                } else if let Some(h) = l.strip_prefix("## ") {
                    lines.push(Line::styled(
                        h.to_owned(),
                        Style::default()
                            .fg(palette.text)
                            .add_modifier(Modifier::BOLD),
                    ));
                } else {
                    lines.push(Line::styled(
                        l.to_owned(),
                        Style::default().fg(palette.text),
                    ));
                }
            }
            lines.push(Line::raw(""));
        }
        UiBlock::MetricRow { items } => {
            let mut spans = Vec::new();
            for (idx, item) in items.iter().enumerate() {
                if idx > 0 {
                    spans.push(Span::styled("   |   ", palette.muted_style()));
                }
                spans.push(Span::styled(
                    format!("{}: ", item.label),
                    palette.muted_style(),
                ));
                spans.push(Span::styled(
                    item.value.clone(),
                    tone_style(palette, item.tone).add_modifier(Modifier::BOLD),
                ));
                if let Some(delta) = &item.delta {
                    spans.push(Span::styled(format!(" ({delta})"), palette.muted_style()));
                }
            }
            lines.push(Line::from(spans));
            lines.push(Line::raw(""));
        }
        UiBlock::Badge { label, tone } => {
            lines.push(Line::styled(
                format!("[{label}]"),
                tone_style(palette, *tone).add_modifier(Modifier::BOLD),
            ));
        }
        UiBlock::KeyValue { rows } => {
            for row in rows {
                lines.push(Line::from(vec![
                    Span::styled(format!("{:<16} ", row.key), palette.muted_style()),
                    Span::styled(row.value.clone(), tone_style(palette, row.tone)),
                ]));
            }
            lines.push(Line::raw(""));
        }
        UiBlock::List { title, items } => {
            if let Some(t) = title {
                lines.push(Line::styled(
                    t.clone(),
                    palette.muted_style().add_modifier(Modifier::BOLD),
                ));
            }
            for item in items {
                let mut marker = "· ".to_owned();
                let mut base = tone_style(palette, item.tone);
                if item.action.is_some() {
                    let cur_idx = *action_idx;
                    let focused = cur_idx == selected_action;
                    *action_idx += 1;
                    hits.push(ActionHit {
                        line: lines.len(),
                        col_start: 0,
                        col_end: width.max(1),
                        action_idx: cur_idx,
                    });
                    if focused {
                        marker = "› ".into();
                        base = Style::default()
                            .fg(palette.accent_text)
                            .bg(palette.accent)
                            .add_modifier(Modifier::BOLD);
                    }
                }
                let mut spans = vec![
                    Span::styled(marker, base),
                    Span::styled(item.title.clone(), base),
                ];
                if let Some(sub) = &item.subtitle {
                    spans.push(Span::styled(format!("  {sub}"), palette.muted_style()));
                }
                if let Some(b) = &item.badge {
                    spans.push(Span::styled(
                        format!("  [{b}]"),
                        tone_style(palette, item.tone),
                    ));
                }
                lines.push(Line::from(spans));
            }
            lines.push(Line::raw(""));
        }
        UiBlock::Table { columns, rows } => {
            let col_w = (width / columns.len().max(1)).clamp(8, 28);
            let header = columns
                .iter()
                .map(|c| format!("{:<w$}", ellipsize(c, col_w.saturating_sub(1)), w = col_w))
                .collect::<String>();
            lines.push(Line::styled(
                header,
                palette.muted_style().add_modifier(Modifier::BOLD),
            ));
            lines.push(Line::styled(
                "─".repeat(width.min(columns.len() * col_w)),
                Style::default().fg(palette.track),
            ));
            for row in rows {
                let line = row
                    .iter()
                    .map(|c| format!("{:<w$}", ellipsize(c, col_w.saturating_sub(1)), w = col_w))
                    .collect::<String>();
                lines.push(Line::styled(line, Style::default().fg(palette.text)));
            }
            lines.push(Line::raw(""));
        }
        UiBlock::Code { language, code, .. } => {
            if let Some(lang) = language {
                lines.push(Line::styled(format!("```{lang}"), palette.muted_style()));
            }
            for l in code.lines() {
                lines.push(Line::styled(
                    format!("  {l}"),
                    Style::default().fg(palette.text),
                ));
            }
            lines.push(Line::raw(""));
        }
        UiBlock::Diff { text } => {
            for l in text.lines() {
                let st = if l.starts_with('+') && !l.starts_with("+++") {
                    Style::default().fg(palette.success)
                } else if l.starts_with('-') && !l.starts_with("---") {
                    Style::default().fg(palette.danger)
                } else if l.starts_with("@@") {
                    Style::default().fg(palette.accent)
                } else {
                    palette.muted_style()
                };
                lines.push(Line::styled(l.to_owned(), st));
            }
            lines.push(Line::raw(""));
        }
        UiBlock::Progress {
            label,
            value,
            max,
            tone,
        } => {
            let pct = (value / max).clamp(0.0, 1.0);
            let bar_w = 24usize;
            let filled = (pct * bar_w as f64).round() as usize;
            let bar = format!(
                "{}{}",
                "█".repeat(filled),
                "░".repeat(bar_w.saturating_sub(filled))
            );
            lines.push(Line::from(vec![
                Span::styled(format!("{label} "), palette.muted_style()),
                Span::styled(bar, tone_style(palette, *tone)),
                Span::styled(
                    format!(" {:>3.0}%", pct * 100.0),
                    Style::default().fg(palette.text),
                ),
            ]));
            lines.push(Line::raw(""));
        }
        UiBlock::Sparkline {
            label,
            values,
            unit,
            tone,
        } => {
            let spark = sparkline_string(values);
            let last = values
                .last()
                .map(|v| format!(" {v:.1}{}", unit.as_deref().unwrap_or("")))
                .unwrap_or_default();
            let mut spans = Vec::new();
            if let Some(lbl) = label {
                spans.push(Span::styled(format!("{lbl}: "), palette.muted_style()));
            }
            spans.push(Span::styled(spark, tone_style(palette, *tone)));
            spans.push(Span::styled(last, Style::default().fg(palette.text)));
            lines.push(Line::from(spans));
            lines.push(Line::raw(""));
        }
        UiBlock::Divider => {
            lines.push(Line::styled(
                "─".repeat(width.min(80)),
                Style::default().fg(palette.track),
            ));
        }
        UiBlock::Callout { title, text, tone } => {
            let bar_style = tone_style(palette, *tone).add_modifier(Modifier::BOLD);
            if let Some(t) = title {
                lines.push(Line::from(vec![
                    Span::styled("▎ ", bar_style),
                    Span::styled(t.clone(), bar_style),
                ]));
            }
            for l in text.lines() {
                lines.push(Line::from(vec![
                    Span::styled("▎ ", bar_style),
                    Span::styled(l.to_owned(), Style::default().fg(palette.text)),
                ]));
            }
            lines.push(Line::raw(""));
        }
        UiBlock::Button { label, style, .. } => {
            let cur_idx = *action_idx;
            let focused = cur_idx == selected_action;
            *action_idx += 1;
            let st = button_style(palette, style, focused);
            let txt = format!(" [ {label} ] ");
            let w = super::view::cell_width(&txt);
            hits.push(ActionHit {
                line: lines.len(),
                col_start: 0,
                col_end: w,
                action_idx: cur_idx,
            });
            lines.push(Line::styled(txt, st));
            lines.push(Line::raw(""));
        }
        UiBlock::ButtonRow { buttons } => {
            let mut spans = Vec::new();
            let mut col = 0usize;
            for (idx, b) in buttons.iter().enumerate() {
                if idx > 0 {
                    spans.push(Span::raw("  "));
                    col += 2;
                }
                let cur_idx = *action_idx;
                let focused = cur_idx == selected_action;
                *action_idx += 1;
                let txt = format!(" [ {} ] ", b.label);
                let w = super::view::cell_width(&txt);
                hits.push(ActionHit {
                    line: lines.len(),
                    col_start: col,
                    col_end: col + w,
                    action_idx: cur_idx,
                });
                col += w;
                spans.push(Span::styled(txt, button_style(palette, &b.style, focused)));
            }
            lines.push(Line::from(spans));
            lines.push(Line::raw(""));
        }
        UiBlock::Toggle { label, value, .. } => {
            let cur_idx = *action_idx;
            let focused = cur_idx == selected_action;
            *action_idx += 1;
            let st = if focused {
                Style::default()
                    .fg(palette.accent_text)
                    .bg(palette.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(palette.text)
            };
            let state = if *value { "[x]" } else { "[ ]" };
            let txt = format!(" {state} {label} ");
            hits.push(ActionHit {
                line: lines.len(),
                col_start: 0,
                col_end: super::view::cell_width(&txt).max(1),
                action_idx: cur_idx,
            });
            lines.push(Line::styled(txt, st));
        }
        UiBlock::Select {
            label,
            value,
            options,
            ..
        } => {
            let focused = if !options.is_empty() {
                let cur_idx = *action_idx;
                let f = cur_idx == selected_action;
                *action_idx += 1;
                hits.push(ActionHit {
                    line: lines.len(),
                    col_start: 0,
                    col_end: width.max(1),
                    action_idx: cur_idx,
                });
                f
            } else {
                false
            };
            let opts = options
                .iter()
                .map(|o| {
                    if o.value == *value {
                        format!("[{}]", o.label)
                    } else {
                        o.label.clone()
                    }
                })
                .collect::<Vec<_>>()
                .join("  ");
            let val_style = if focused {
                Style::default()
                    .fg(palette.accent_text)
                    .bg(palette.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(palette.text)
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{label}: "), palette.muted_style()),
                Span::styled(opts, val_style),
            ]));
        }
        UiBlock::Form {
            submit_label,
            fields,
            ..
        } => {
            let names = fields
                .iter()
                .map(|f| f.label.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            lines.push(Line::styled(
                format!("Form ({names}) -> [{submit_label}] (editable in Mobile app)"),
                palette.muted_style(),
            ));
        }
        UiBlock::Empty { title, body } => {
            lines.push(Line::styled(
                title.clone(),
                Style::default()
                    .fg(palette.text)
                    .add_modifier(Modifier::BOLD),
            ));
            if let Some(b) = body {
                lines.push(Line::styled(b.clone(), palette.muted_style()));
            }
        }
    }
}

fn button_style(palette: Palette, kind: &str, focused: bool) -> Style {
    if focused {
        return Style::default()
            .fg(palette.accent_text)
            .bg(palette.accent)
            .add_modifier(Modifier::BOLD);
    }
    match kind {
        "primary" => Style::default()
            .fg(palette.accent)
            .add_modifier(Modifier::BOLD),
        "danger" => Style::default().fg(palette.danger),
        _ => Style::default().fg(palette.text),
    }
}

fn resolve_socket_for_standalone(explicit: Option<&str>) -> Option<PathBuf> {
    if let Some(s) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        return Some(PathBuf::from(s));
    }
    crate::api::current_socket().or_else(|| {
        crate::remote::discovery::discover_masil_servers()
            .into_iter()
            .next()
            .map(|s| PathBuf::from(s.socket))
    })
}

fn read_server_option(socket: &Path, opt: &str) -> Option<String> {
    let masil = crate::native_ui::native_executable().ok()?;
    let out = std::process::Command::new(masil)
        .arg("-S")
        .arg(socket)
        .args(["show-options", "-gqv", opt])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let val = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (!val.is_empty()).then_some(val)
}

fn detect_live_agent_context(socket: Option<&Path>) -> Option<Value> {
    let sock = socket?;
    let sock_str = sock.to_string_lossy().into_owned();
    let self_pane = std::env::var("TMUX_PANE").ok();

    if let Ok(val) =
        crate::remote::discovery::run_self_agent_json(Some(&sock_str), &["list".into()])
        && let Some(arr) = val.get("agents").and_then(Value::as_array)
        && !arr.is_empty()
    {
        let rank = |a: &Value| -> u8 {
            let pane = a
                .get("pane_id")
                .or_else(|| a.get("pane"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if self_pane.as_deref() == Some(pane) {
                return 9;
            }
            match a.get("state").and_then(Value::as_str).unwrap_or("") {
                "blocked" => 0,
                "working" | "retrying" => 1,
                "idle" => 2,
                _ => 3,
            }
        };
        if let Some(best) = arr.iter().min_by_key(|a| rank(a)) {
            let mut obj = best.clone();
            if let Some(m) = obj.as_object_mut() {
                if let Some(pane) = m
                    .get("pane_id")
                    .or_else(|| m.get("pane"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                {
                    m.insert("pane".into(), Value::String(pane.clone()));
                    m.insert("pane_id".into(), Value::String(pane));
                }
                m.insert("server_socket".into(), Value::String(sock_str));
            }
            return Some(obj);
        }
    }

    // Fallback to the active non-self pane in this masil server (e.g. a normal shell).
    let masil = crate::native_ui::native_executable().ok()?;
    let out = std::process::Command::new(masil)
        .arg("-S")
        .arg(sock)
        .args([
            "list-panes",
            "-a",
            "-F",
            "#{pane_id}\t#{pane_active}\t#{pane_current_path}\t#{pane_title}",
        ])
        .output()
        .ok()?;
    if out.status.success() {
        let stdout = String::from_utf8_lossy(&out.stdout);
        let mut fallback: Option<Value> = None;
        for line in stdout.lines() {
            let mut parts = line.splitn(4, '\t');
            let pane = parts.next().unwrap_or("").trim();
            let active = parts.next().unwrap_or("").trim() == "1";
            let cwd = parts.next().unwrap_or("").trim();
            let title = parts.next().unwrap_or("").trim();
            if pane.is_empty() || self_pane.as_deref() == Some(pane) {
                continue;
            }
            let row = json!({
                "name": if title.is_empty() { pane } else { title },
                "provider": "shell",
                "state": "unknown",
                "cwd": cwd,
                "pane": pane,
                "pane_id": pane,
                "server_socket": sock_str,
            });
            if active {
                return Some(row);
            }
            if fallback.is_none() {
                fallback = Some(row);
            }
        }
        if fallback.is_some() {
            return fallback;
        }
    }
    None
}

fn load_standalone_prefs(
    socket: Option<&Path>,
    catalog: Option<&crate::plugin::manifest::PluginCatalog>,
) -> (Language, Theme) {
    let prefs = super::preferences::Preferences::load();
    let language = socket
        .and_then(|s| read_server_option(s, "@masil-lang"))
        .map(|v| {
            if v == "ko" {
                Language::Korean
            } else {
                Language::English
            }
        })
        .unwrap_or(prefs.language);
    let mut theme = socket
        .and_then(|s| read_server_option(s, "@masil-theme"))
        .map(|v| match v.as_str() {
            "light" => Theme::Light,
            "terminal" | "tmux" => Theme::Terminal,
            _ => Theme::Dark,
        })
        .unwrap_or(prefs.theme);
    if let Some(t) = catalog.and_then(|c| c.active_resolved_theme()) {
        super::view::set_plugin_palette(Some(&t));
        theme = if t.appearance == "light" {
            Theme::Light
        } else {
            Theme::Dark
        };
    } else {
        super::view::set_plugin_palette(None);
    }
    (language, theme)
}

/// Runs a standalone live Ratatui viewer for a plugin panel inside any `masil`
/// floating pane or split pane (`masil-agent plugin open <plugin> [panel]`).
pub(crate) fn run_standalone_panel(
    plugin_id: &str,
    panel_id: Option<&str>,
    socket_arg: Option<&str>,
) -> Result<i32, String> {
    let socket = resolve_socket_for_standalone(socket_arg);
    let catalog = crate::plugin::manifest::load_catalog()?;
    let plugin = catalog
        .find_plugin(plugin_id)
        .cloned()
        .ok_or_else(|| format!("plugin '{plugin_id}' not found"))?;
    if !plugin.is_active(catalog.enabled) {
        return Err(format!("plugin '{plugin_id}' is disabled or has errors"));
    }
    let manifest = plugin
        .manifest
        .as_ref()
        .ok_or_else(|| format!("plugin '{plugin_id}' has no manifest"))?;
    let panel = match panel_id {
        Some(pid) => manifest
            .panels
            .iter()
            .find(|p| p.id == pid)
            .ok_or_else(|| format!("plugin '{plugin_id}' has no panel '{pid}'"))?,
        None => manifest
            .panels
            .first()
            .ok_or_else(|| format!("plugin '{plugin_id}' defines no panels"))?,
    };
    let panel_id = panel.id.clone();
    let panel_title = panel.title.clone();
    let refresh_secs = panel.refresh_secs;
    let (language, mut theme) = load_standalone_prefs(socket.as_deref(), Some(&catalog));

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;

    rt.block_on(async move {
        let mut terminal = super::terminal::Session::open()?;
        let mut events = crossterm::event::EventStream::new();
        let mut tick = tokio::time::interval(Duration::from_millis(500));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let initial_agent = detect_live_agent_context(socket.as_deref());
        let mut view = PluginViewOverlay::loading(
            language,
            plugin_id.to_owned(),
            Some(panel_id.clone()),
            refresh_secs,
            panel_title,
            initial_agent.clone(),
        );
        view.fullscreen = true;
        let mut fp = crate::plugin::manifest::catalog_fingerprint();
        let mut dirty = true;

        let spawn_fetch = |pid: String,
                           pan_id: String,
                           sock: Option<PathBuf>,
                           action: Option<String>,
                           payload: Option<Value>| {
            tokio::spawn(async move {
                let agent = tokio::task::spawn_blocking({
                    let s = sock.clone();
                    move || detect_live_agent_context(s.as_deref())
                })
                .await
                .ok()
                .flatten();
                let loaded = tokio::task::spawn_blocking(move || {
                    let cat = crate::plugin::manifest::load_catalog()?;
                    cat.find_plugin(&pid)
                        .cloned()
                        .ok_or_else(|| format!("plugin '{pid}' not found"))
                })
                .await
                .map_err(|e| e.to_string())??;
                crate::plugin::runner::invoke_panel(
                    &loaded,
                    &pan_id,
                    agent,
                    action,
                    payload,
                    serde_json::Map::new(),
                    sock,
                )
                .await
            })
        };

        let mut inflight: Option<tokio::task::JoinHandle<Result<UiDocument, String>>> =
            Some(spawn_fetch(
                plugin_id.to_owned(),
                panel_id.clone(),
                socket.clone(),
                None,
                None,
            ));

        loop {
            if terminal.panicked() {
                return Err("plugin view panicked".into());
            }
            if dirty {
                terminal
                    .draw(|frame| view.draw(frame, theme))
                    .map_err(|e| e.to_string())?;
                dirty = false;
            }
            tokio::select! {
                done = async {
                    match inflight.as_mut() {
                        Some(h) => h.await,
                        None => std::future::pending().await,
                    }
                } => {
                    inflight = None;
                    let res = match done {
                        Ok(r) => r,
                        Err(e) => Err(e.to_string()),
                    };
                    view.set_document(res);
                    dirty = true;
                }
                _ = tick.tick() => {
                    let next_fp = crate::plugin::manifest::catalog_fingerprint();
                    if next_fp != fp {
                        fp = next_fp;
                        let cat = crate::plugin::manifest::load_catalog().ok();
                        let (_, next_theme) = load_standalone_prefs(socket.as_deref(), cat.as_ref());
                        theme = next_theme;
                        if let Some(c) = &cat
                            && let Some(p) = c.find_plugin(plugin_id)
                            && let Some(m) = &p.manifest
                            && let Some(pd) = m.panels.iter().find(|pd| pd.id == panel_id)
                        {
                            view.refresh_secs = pd.refresh_secs;
                        }
                        if inflight.is_none() {
                            view.mark_refresh_started(Instant::now());
                            inflight = Some(spawn_fetch(
                                plugin_id.to_owned(),
                                panel_id.clone(),
                                socket.clone(),
                                None,
                                None,
                            ));
                        }
                        dirty = true;
                    } else if inflight.is_none() && view.should_auto_refresh(Instant::now()) {
                        view.mark_refresh_started(Instant::now());
                        inflight = Some(spawn_fetch(
                            plugin_id.to_owned(),
                            panel_id.clone(),
                            socket.clone(),
                            None,
                            None,
                        ));
                    }
                }
                ev = events.next() => match ev {
                    Some(Ok(Event::Key(k)))
                        if k.code == KeyCode::Char('c')
                            && k.modifiers.contains(KeyModifiers::CONTROL) =>
                    {
                        break;
                    }
                    Some(Ok(event)) => {
                        match view.handle(event) {
                            PluginViewOutcome::None => dirty = true,
                            PluginViewOutcome::Close => break,
                            PluginViewOutcome::Refresh => {
                                if inflight.is_none() {
                                    inflight = Some(spawn_fetch(
                                        plugin_id.to_owned(),
                                        panel_id.clone(),
                                        socket.clone(),
                                        None,
                                        None,
                                    ));
                                }
                                dirty = true;
                            }
                            PluginViewOutcome::Invoke {
                                plugin_id: pid,
                                panel_id: Some(pan_id),
                                action,
                                payload,
                                ..
                            } => {
                                if let Some(prev) = inflight.take() {
                                    prev.abort();
                                }
                                inflight = Some(spawn_fetch(
                                    pid,
                                    pan_id,
                                    socket.clone(),
                                    Some(action),
                                    payload,
                                ));
                                dirty = true;
                            }
                            PluginViewOutcome::Invoke { .. } => {}
                        }
                    }
                    Some(Err(_)) | None => break,
                }
            }
        }
        if let Some(h) = inflight {
            h.abort();
        }
        Ok(0)
    })
}

/// Runs a plugin slash command inside a `masil` floating popup, displaying any
/// returned UI document interactively (or a toast message via `display-message`).
pub(crate) fn run_standalone_command(
    plugin_id: &str,
    command_id: &str,
    args_text: &str,
    socket_arg: Option<&str>,
) -> Result<i32, String> {
    let socket = resolve_socket_for_standalone(socket_arg);
    let catalog = crate::plugin::manifest::load_catalog()?;
    let plugin = catalog
        .find_plugin(plugin_id)
        .cloned()
        .ok_or_else(|| format!("plugin '{plugin_id}' not found"))?;
    if !plugin.is_active(catalog.enabled) {
        return Err(format!("plugin '{plugin_id}' is disabled or has errors"));
    }
    let cmd_title = plugin
        .manifest
        .as_ref()
        .and_then(|m| {
            m.commands
                .iter()
                .find(|c| c.id == command_id || c.slash.trim_start_matches('/') == command_id)
                .map(|c| c.title.clone())
        })
        .unwrap_or_else(|| command_id.to_owned());
    let (language, theme) = load_standalone_prefs(socket.as_deref(), Some(&catalog));

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;

    rt.block_on(async move {
        let agent = detect_live_agent_context(socket.as_deref());
        let first = crate::plugin::runner::invoke_command_full(
            &plugin,
            command_id,
            args_text.to_owned(),
            agent.clone(),
            None,
            None,
            serde_json::Map::new(),
            socket.clone(),
            true,
        )
        .await?;

        if let Some(toast) = &first.toast
            && let Some(sock) = &socket
            && let Ok(masil) = crate::native_ui::native_executable()
        {
            let _ = std::process::Command::new(masil)
                .arg("-S")
                .arg(sock)
                .args(["display-message", toast])
                .output();
        }
        let Some(doc) = first.ui else {
            return Ok(0);
        };

        let mut terminal = super::terminal::Session::open()?;
        let mut events = crossterm::event::EventStream::new();
        let mut view = PluginViewOverlay::with_command_document(
            language,
            plugin_id.to_owned(),
            command_id.to_owned(),
            args_text.to_owned(),
            cmd_title,
            agent.clone(),
            doc,
        );
        view.fullscreen = true;

        loop {
            if terminal.panicked() {
                return Err("plugin command view panicked".into());
            }
            terminal
                .draw(|frame| view.draw(frame, theme))
                .map_err(|e| e.to_string())?;
            match events.next().await {
                Some(Ok(Event::Key(k)))
                    if k.code == KeyCode::Char('c')
                        && k.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    break;
                }
                Some(Ok(event)) => match view.handle(event) {
                    PluginViewOutcome::None => {}
                    PluginViewOutcome::Close => break,
                    PluginViewOutcome::Refresh => {
                        let out = crate::plugin::runner::invoke_command_full(
                            &plugin,
                            command_id,
                            args_text.to_owned(),
                            agent.clone(),
                            None,
                            None,
                            serde_json::Map::new(),
                            socket.clone(),
                            true,
                        )
                        .await;
                        match out {
                            Ok(o) => {
                                if let Some(d) = o.ui {
                                    view.set_document(Ok(d));
                                } else {
                                    view.loading = false;
                                }
                            }
                            Err(e) => view.set_document(Err(e)),
                        }
                    }
                    PluginViewOutcome::Invoke {
                        action, payload, ..
                    } => {
                        let out = crate::plugin::runner::invoke_command_full(
                            &plugin,
                            command_id,
                            args_text.to_owned(),
                            agent.clone(),
                            Some(action),
                            payload,
                            serde_json::Map::new(),
                            socket.clone(),
                            true,
                        )
                        .await;
                        match out {
                            Ok(o) => {
                                if let Some(d) = o.ui {
                                    view.set_document(Ok(d));
                                } else {
                                    view.loading = false;
                                }
                            }
                            Err(e) => view.set_document(Err(e)),
                        }
                    }
                },
                Some(Err(_)) | None => break,
            }
        }
        Ok(0)
    })
}
