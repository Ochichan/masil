//! Interactive Ratatui overlay rendering any Masil UI Spec v1 document
//! (workspace panels and slash-command UI outputs) inside the native TUI.

use super::{
    model::{Language, Theme},
    view::{Palette, Tone, ellipsize},
};
use crate::plugin::ui_spec::{UiBlock, UiDocument, UiTone};
use crossterm::event::{Event, KeyCode, MouseEventKind};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};
use serde_json::{Value, json};

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
        panel_id: String,
        action: String,
        payload: Option<Value>,
    },
}

pub(super) struct PluginViewOverlay {
    pub language: Language,
    pub plugin_id: String,
    pub panel_id: Option<String>,
    pub title: String,
    pub agent: Option<Value>,
    pub doc: Option<UiDocument>,
    pub loading: bool,
    pub error: Option<String>,
    pub scroll: usize,
    pub selected_action: usize,
    actions: Vec<ActionBinding>,
    page: usize,
}

impl PluginViewOverlay {
    pub(super) fn loading(
        language: Language,
        plugin_id: String,
        panel_id: Option<String>,
        title: String,
        agent: Option<Value>,
    ) -> Self {
        Self {
            language,
            plugin_id,
            panel_id,
            title,
            agent,
            doc: None,
            loading: true,
            error: None,
            scroll: 0,
            selected_action: 0,
            actions: Vec::new(),
            page: 12,
        }
    }

    pub(super) fn with_document(
        language: Language,
        plugin_id: String,
        panel_id: Option<String>,
        title: String,
        agent: Option<Value>,
        doc: UiDocument,
    ) -> Self {
        let mut view = Self::loading(language, plugin_id, panel_id, title, agent);
        view.set_document(Ok(doc));
        view
    }

    pub(super) fn set_document(&mut self, result: Result<UiDocument, String>) {
        self.loading = false;
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
                KeyCode::Char('r') if self.panel_id.is_some() => {
                    self.loading = true;
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
                    if let (Some(panel_id), Some(binding)) =
                        (&self.panel_id, self.actions.get(self.selected_action))
                    {
                        self.loading = true;
                        PluginViewOutcome::Invoke {
                            plugin_id: self.plugin_id.clone(),
                            panel_id: panel_id.clone(),
                            action: binding.action.clone(),
                            payload: binding.payload.clone(),
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
                _ => PluginViewOutcome::None,
            },
            _ => PluginViewOutcome::None,
        }
    }

    pub(super) fn draw(&mut self, frame: &mut Frame, theme: Theme) {
        let area = frame.area();
        let width = area.width.saturating_sub(6).clamp(36, 108).min(area.width);
        let height = area.height.saturating_sub(4).clamp(10, 38).min(area.height);
        if width < 24 || height < 8 {
            return;
        }
        let modal = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
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
        self.page = inner.height.max(1) as usize;

        let mut lines: Vec<Line<'static>> = Vec::new();
        let content_width = inner.width as usize;

        if self.loading {
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
                );
            }
        }

        let max_scroll = lines.len().saturating_sub(inner.height as usize);
        self.scroll = self.scroll.min(max_scroll);
        let visible: Vec<Line<'static>> = lines
            .into_iter()
            .skip(self.scroll)
            .take(inner.height as usize)
            .collect();
        frame.render_widget(Paragraph::new(visible), inner);

        let hint = match (
            self.language,
            self.panel_id.is_some(),
            !self.actions.is_empty(),
        ) {
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
                    let focused = *action_idx == selected_action;
                    *action_idx += 1;
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
            let focused = *action_idx == selected_action;
            *action_idx += 1;
            let st = button_style(palette, style, focused);
            lines.push(Line::styled(format!(" [ {label} ] "), st));
            lines.push(Line::raw(""));
        }
        UiBlock::ButtonRow { buttons } => {
            let mut spans = Vec::new();
            for (idx, b) in buttons.iter().enumerate() {
                if idx > 0 {
                    spans.push(Span::raw("  "));
                }
                let focused = *action_idx == selected_action;
                *action_idx += 1;
                spans.push(Span::styled(
                    format!(" [ {} ] ", b.label),
                    button_style(palette, &b.style, focused),
                ));
            }
            lines.push(Line::from(spans));
            lines.push(Line::raw(""));
        }
        UiBlock::Toggle { label, value, .. } => {
            let focused = *action_idx == selected_action;
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
            lines.push(Line::styled(format!(" {state} {label} "), st));
        }
        UiBlock::Select {
            label,
            value,
            options,
            ..
        } => {
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
            lines.push(Line::from(vec![
                Span::styled(format!("{label}: "), palette.muted_style()),
                Span::styled(opts, Style::default().fg(palette.text)),
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
