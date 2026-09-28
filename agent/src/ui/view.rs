use super::i18n::{
    Text, action_label, activity_label, attention_label, compact_action_label,
    compact_attention_label, compact_filter_label, evidence_label, filter_label, help_lines,
    language_name, process_label, theme_name, tr,
};
use super::model::{Action, App, Filter, Focus, HitRegion, HitTarget, Language, Overlay, Theme};
use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Copy)]
struct Palette {
    canvas: Color,
    panel: Color,
    raised: Color,
    text: Color,
    muted: Color,
    accent: Color,
    accent_text: Color,
    warning: Color,
    danger: Color,
    success: Color,
    track: Color,
}

impl Palette {
    fn for_theme(theme: Theme) -> Self {
        match theme {
            Theme::Dark => Self {
                canvas: Color::Rgb(12, 16, 22),
                panel: Color::Rgb(20, 26, 34),
                raised: Color::Rgb(31, 39, 49),
                text: Color::Rgb(230, 235, 241),
                muted: Color::Rgb(155, 168, 182),
                accent: Color::Rgb(91, 181, 194),
                accent_text: Color::Rgb(5, 19, 22),
                warning: Color::Rgb(235, 184, 92),
                danger: Color::Rgb(232, 111, 117),
                success: Color::Rgb(116, 196, 138),
                track: Color::Rgb(48, 58, 69),
            },
            Theme::Light => Self {
                canvas: Color::Rgb(238, 242, 244),
                panel: Color::Rgb(250, 251, 251),
                raised: Color::Rgb(220, 227, 230),
                text: Color::Rgb(25, 34, 41),
                muted: Color::Rgb(84, 100, 111),
                accent: Color::Rgb(0, 112, 127),
                accent_text: Color::White,
                warning: Color::Rgb(144, 84, 0),
                danger: Color::Rgb(170, 42, 52),
                success: Color::Rgb(22, 112, 54),
                track: Color::Rgb(190, 202, 207),
            },
            Theme::Terminal => Self {
                canvas: Color::Reset,
                panel: Color::Reset,
                raised: Color::DarkGray,
                text: Color::Reset,
                muted: Color::Gray,
                accent: Color::Cyan,
                accent_text: Color::Black,
                warning: Color::Yellow,
                danger: Color::Red,
                success: Color::Green,
                track: Color::DarkGray,
            },
        }
    }
}

impl App {
    pub fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        self.last_area = area;
        self.hits.clear();
        let palette = Palette::for_theme(self.theme);
        frame.render_widget(
            Block::default().style(Style::default().bg(palette.canvas)),
            area,
        );
        if area.width < 28 || area.height < 9 {
            self.draw_tiny(frame, area, palette);
            self.dirty = false;
            return;
        }

        let dense = area.height < 18;
        let compact_narrow = self.compact && area.width < 60;
        let header_height = if dense { 1 } else { 3 };
        let filters_height = if dense { 1 } else { 2 };
        let search_height = if dense { 1 } else { 3 };
        let actions_height = if dense {
            1
        } else if compact_narrow {
            self.visible_actions().len().div_ceil(2) as u16
        } else {
            2
        };
        let footer_height = if dense { 1 } else { 2 };
        let fixed = header_height + filters_height + search_height + actions_height + footer_height;
        let body_height = area.height.saturating_sub(fixed).max(1);
        let mut y = area.y;
        let header = Rect::new(area.x, y, area.width, header_height);
        y += header_height;
        let filters = Rect::new(area.x, y, area.width, filters_height);
        y += filters_height;
        let search = Rect::new(area.x, y, area.width, search_height);
        y += search_height;
        let body = Rect::new(area.x, y, area.width, body_height);
        y += body_height;
        let actions = Rect::new(area.x, y, area.width, actions_height);
        y += actions_height;
        let footer = Rect::new(area.x, y, area.width, footer_height.min(area.bottom() - y));

        self.draw_header(frame, header, palette, dense);
        self.draw_filters(frame, filters, palette, dense);
        self.draw_search(frame, search, palette, dense);

        let wide = area.width >= 100 && area.height >= 24;
        if wide {
            let list_width = self
                .divider_width
                .clamp(34, area.width.saturating_sub(34).max(34));
            let list = Rect::new(body.x, body.y, list_width.min(body.width), body.height);
            let divider = Rect::new(list.right(), body.y, 1.min(body.width), body.height);
            let inspector = Rect::new(
                divider.right(),
                body.y,
                body.right().saturating_sub(divider.right()),
                body.height,
            );
            self.draw_list(frame, list, palette, false);
            if divider.width > 0 && inspector.width > 0 {
                frame.render_widget(
                    Block::default().style(Style::default().bg(if self.drag.is_some() {
                        palette.accent
                    } else {
                        palette.track
                    })),
                    divider,
                );
                self.push_hit(divider, HitTarget::Divider, 1);
            }
            self.draw_inspector(frame, inspector, palette);
        } else if self.details_open {
            self.draw_inspector(frame, body, palette);
        } else {
            self.draw_list(frame, body, palette, self.compact || area.width < 60);
        }

        self.draw_actions(frame, actions, palette, dense, wide);
        self.draw_footer(frame, footer, palette);
        self.draw_overlay(frame, area, palette);
        self.dirty = false;
    }

    fn draw_tiny(&mut self, frame: &mut Frame, area: Rect, palette: Palette) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(palette.warning))
            .style(Style::default().bg(palette.panel).fg(palette.text));
        frame.render_widget(block, area);
        let inner = inset(area, 1);
        if inner.height > 0 {
            frame.render_widget(
                Paragraph::new(tr(self.language, Text::TerminalTooSmall))
                    .style(
                        Style::default()
                            .fg(palette.warning)
                            .add_modifier(Modifier::BOLD),
                    )
                    .wrap(Wrap { trim: true }),
                Rect::new(
                    inner.x,
                    inner.y,
                    inner.width,
                    inner.height.saturating_sub(2),
                ),
            );
        }
        if inner.height >= 2 {
            frame.render_widget(
                Paragraph::new(tr(self.language, Text::MinimumSize))
                    .style(Style::default().fg(palette.muted)),
                Rect::new(inner.x, inner.bottom().saturating_sub(2), inner.width, 1),
            );
        }
        if inner.height >= 1 {
            let label = format!("[q {}]", tr(self.language, Text::Close));
            let width = cell_width(&label).min(inner.width as usize) as u16;
            let close = Rect::new(
                inner.right().saturating_sub(width),
                inner.bottom() - 1,
                width,
                1,
            );
            frame.render_widget(
                Paragraph::new(clip(&label, width as usize)).style(
                    Style::default()
                        .fg(palette.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                close,
            );
            self.push_hit(close, HitTarget::Close, 1);
        }
    }

    fn draw_header(&mut self, frame: &mut Frame, area: Rect, palette: Palette, dense: bool) {
        frame.render_widget(
            Block::default().style(Style::default().bg(palette.panel)),
            area,
        );
        if self.compact && area.width < 60 && !dense {
            let status = if self.connected {
                tr(self.language, Text::ShortConnected)
            } else if self.rows.is_empty() {
                tr(self.language, Text::ShortConnecting)
            } else {
                tr(self.language, Text::ShortDisconnected)
            };
            let status_width = cell_width(status).min(area.width as usize) as u16;
            let title_width = area.width.saturating_sub(status_width + 1);
            frame.render_widget(
                Paragraph::new(clip(tr(self.language, Text::Agents), title_width as usize)).style(
                    Style::default()
                        .fg(palette.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Rect::new(area.x, area.y, title_width, 1),
            );
            frame.render_widget(
                Paragraph::new(status).style(Style::default().fg(if self.connected {
                    palette.success
                } else {
                    palette.warning
                })),
                Rect::new(
                    area.right().saturating_sub(status_width),
                    area.y,
                    status_width,
                    1,
                ),
            );
            let controls = [
                (
                    HitTarget::Language,
                    format!(
                        "l {}",
                        if self.language == Language::English {
                            "EN"
                        } else {
                            "KO"
                        }
                    ),
                    3,
                ),
                (
                    HitTarget::Theme,
                    format!(
                        "t {}",
                        match self.theme {
                            Theme::Dark => "D",
                            Theme::Light => "L",
                            Theme::Terminal => "T",
                        }
                    ),
                    2,
                ),
                (HitTarget::Help, "?".to_owned(), 1),
                (HitTarget::Close, "q".to_owned(), 0),
            ];
            self.header_controls = 0;
            let mut x = area.x;
            let controls_y = area.bottom().saturating_sub(1);
            for (target, label, focus_index) in controls {
                let width = (cell_width(&label) + 2) as u16;
                if x + width > area.right() {
                    break;
                }
                self.draw_control(
                    frame,
                    Rect::new(x, controls_y, width, 1),
                    &label,
                    target,
                    palette,
                    self.focus == Focus::Header && self.header_focus == focus_index,
                );
                self.header_controls += 1;
                x += width + 1;
            }
            return;
        }
        let line = if dense { area } else { inset_y(area, 1) };
        let view = if self.compact && self.last_area.width < 100 {
            tr(self.language, Text::Sidebar)
        } else {
            tr(self.language, Text::Desk)
        };
        let connection = if self.connected {
            tr(self.language, Text::Connected)
        } else if self.rows.is_empty() {
            tr(self.language, Text::Connecting)
        } else {
            tr(self.language, Text::Disconnected)
        };
        let connection_color = if self.connected {
            palette.success
        } else {
            palette.warning
        };
        let title = Line::from(vec![
            Span::styled(
                tr(self.language, Text::Product),
                Style::default()
                    .fg(palette.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("  {view}"), Style::default().fg(palette.text)),
            Span::styled(
                format!("  {connection}"),
                Style::default().fg(connection_color),
            ),
        ]);
        frame.render_widget(Paragraph::new(title), line);

        let compact_labels = line.width < 76;
        let mut right = line.right();
        let controls = [
            (
                HitTarget::Close,
                if compact_labels {
                    "q".to_owned()
                } else {
                    format!("q {}", tr(self.language, Text::Close))
                },
            ),
            (
                HitTarget::Help,
                if compact_labels {
                    "?".to_owned()
                } else {
                    format!("? {}", tr(self.language, Text::Help))
                },
            ),
            (
                HitTarget::Theme,
                if compact_labels {
                    "t".to_owned()
                } else {
                    format!("t {}", theme_name(self.language, self.theme))
                },
            ),
            (
                HitTarget::Language,
                if compact_labels {
                    language_name(self.language).to_owned()
                } else {
                    format!("l {}", language_name(self.language))
                },
            ),
        ];
        self.header_controls = 0;
        for (index, (target, label)) in controls.into_iter().enumerate() {
            let needed = (cell_width(&label) + 2) as u16;
            if right <= line.x + needed {
                break;
            }
            right -= needed;
            let rect = Rect::new(right, line.y, needed, 1);
            self.draw_control(
                frame,
                rect,
                &label,
                target.clone(),
                palette,
                self.focus == Focus::Header && self.header_focus == index,
            );
            self.header_controls += 1;
            right = right.saturating_sub(1);
        }
    }

    fn draw_filters(&mut self, frame: &mut Frame, area: Rect, palette: Palette, dense: bool) {
        frame.render_widget(
            Block::default().style(Style::default().bg(palette.canvas)),
            area,
        );
        let line = if dense {
            area
        } else {
            Rect::new(area.x, area.y, area.width, 1)
        };
        let counts = [
            self.rows.len(),
            self.rows
                .iter()
                .filter(|row| row.pending && row.attention_available)
                .count(),
            self.rows
                .iter()
                .filter(|row| matches!(row.activity.as_str(), "working" | "retrying"))
                .count(),
            if self.connected {
                self.rows.iter().filter(|row| row.is_unavailable()).count()
            } else {
                self.rows.len()
            },
        ];
        if self.compact && area.width < 60 {
            for (index, filter) in Filter::ALL.into_iter().enumerate() {
                let (column, row) = if dense {
                    (index as u16, 0)
                } else {
                    ((index % 2) as u16, (index / 2) as u16)
                };
                let columns = if dense { 4 } else { 2 };
                let gap = columns - 1;
                let width = area.width.saturating_sub(gap) / columns;
                let x = area.x + column * (width + 1);
                let rect = Rect::new(x, area.y + row, width, 1);
                let label = format!(
                    "{} {}",
                    compact_filter_label(self.language, filter),
                    counts[index]
                );
                self.draw_control(
                    frame,
                    rect,
                    &clip(&label, rect.width.saturating_sub(2) as usize),
                    HitTarget::Filter(filter),
                    palette,
                    self.filter == filter,
                );
            }
            return;
        }
        let mut x = line.x;
        for (index, filter) in Filter::ALL.into_iter().enumerate() {
            let long = format!("{} {}", filter_label(self.language, filter), counts[index]);
            let short = format!("{}", counts[index]);
            let label = if x + (cell_width(&long) as u16 + 3) <= line.right() {
                long
            } else {
                short
            };
            let width = (cell_width(&label) + 2) as u16;
            if x + width > line.right() {
                break;
            }
            let rect = Rect::new(x, line.y, width, 1);
            self.draw_control(
                frame,
                rect,
                &label,
                HitTarget::Filter(filter),
                palette,
                self.filter == filter,
            );
            x = x.saturating_add(width + 1);
        }
    }

    fn draw_search(&mut self, frame: &mut Frame, area: Rect, palette: Palette, dense: bool) {
        let block = if dense {
            Block::default().style(Style::default().bg(palette.panel))
        } else {
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(if self.focus == Focus::Search {
                    palette.accent
                } else {
                    palette.track
                }))
                .style(Style::default().bg(palette.panel))
        };
        let inner = if dense { area } else { inset(area, 1) };
        frame.render_widget(block, area);
        if inner.width == 0 || inner.height == 0 {
            return;
        }
        let clear_label = if self.search.is_empty() {
            String::new()
        } else {
            format!(" {} ", tr(self.language, Text::Clear))
        };
        let clear_width = cell_width(&clear_label).min(inner.width as usize) as u16;
        let field_width = inner.width.saturating_sub(clear_width);
        let placeholder = format!("/ {}", tr(self.language, Text::Search));
        let (shown, cursor_x, start) = if self.search.is_empty() {
            (placeholder.clone(), 0, 0)
        } else {
            search_window(
                &self.search,
                self.search_cursor,
                field_width.saturating_sub(1) as usize,
            )
        };
        self.search_view_start = start;
        let style = if self.search.is_empty() {
            Style::default().fg(palette.muted)
        } else {
            Style::default().fg(palette.text)
        };
        let field = Rect::new(inner.x, inner.y, field_width, 1);
        frame.render_widget(
            Paragraph::new(clip(&shown, field.width as usize)).style(style),
            field,
        );
        self.push_hit(field, HitTarget::Search, 1);
        if clear_width > 0 {
            let clear = Rect::new(field.right(), inner.y, clear_width, 1);
            self.draw_control(
                frame,
                clear,
                clear_label.trim(),
                HitTarget::ClearSearch,
                palette,
                false,
            );
        }
        if self.focus == Focus::Search && self.overlay.is_none() && field.width > 0 {
            let x = field
                .x
                .saturating_add(cursor_x.min(field.width.saturating_sub(1) as usize) as u16);
            frame.set_cursor_position((x, field.y));
        }
    }

    fn draw_list(&mut self, frame: &mut Frame, area: Rect, palette: Palette, two_line: bool) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" {} ", tr(self.language, Text::Agents)))
            .border_style(Style::default().fg(if self.focus == Focus::List {
                palette.accent
            } else {
                palette.track
            }))
            .style(Style::default().bg(palette.panel));
        let inner = inset(area, 1);
        frame.render_widget(block, area);
        if inner.width == 0 || inner.height == 0 {
            self.list_page = 1;
            return;
        }
        let row_height = if two_line { 2 } else { 1 };
        let previous_page = self.list_page;
        self.list_page = (inner.height / row_height).max(1) as usize;
        if self.list_page == previous_page {
            self.clamp_selection_and_scroll();
        } else {
            self.reveal_selection();
        }
        let rows = self
            .filtered_rows()
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        if rows.is_empty() {
            let text = format!(
                "{}\n{}",
                tr(self.language, Text::NoRows),
                tr(self.language, Text::NoRowsHint)
            );
            frame.render_widget(
                Paragraph::new(text)
                    .style(Style::default().fg(palette.muted))
                    .alignment(Alignment::Center)
                    .wrap(Wrap { trim: true }),
                inner,
            );
            return;
        }
        let has_scroll = rows.len() > self.list_page;
        let content_width = inner.width.saturating_sub(u16::from(has_scroll));
        for (visible, observation) in rows
            .iter()
            .skip(self.list_scroll)
            .take(self.list_page)
            .enumerate()
        {
            let y = inner.y + visible as u16 * row_height;
            let height = row_height.min(inner.bottom().saturating_sub(y));
            if height == 0 {
                continue;
            }
            let rect = Rect::new(inner.x, y, content_width, height);
            let selected = self.selected_id.as_deref() == Some(observation.id.as_str());
            let background = if selected {
                palette.raised
            } else {
                palette.panel
            };
            frame.render_widget(
                Block::default().style(Style::default().bg(background)),
                rect,
            );
            let unavailable = !self.connected || observation.is_unavailable();
            let state_color = if unavailable {
                palette.danger
            } else if observation.pending && !observation.acknowledged {
                palette.warning
            } else if matches!(observation.activity.as_str(), "working" | "retrying") {
                palette.success
            } else {
                palette.muted
            };
            let state = if unavailable {
                tr(self.language, Text::StateUnavailable)
            } else {
                activity_label(self.language, &observation.activity)
            };
            let attention = if observation.pending {
                format!(
                    "{} {} / {}",
                    observation.attention_total(),
                    tr(self.language, Text::Pending),
                    if observation.acknowledged {
                        tr(self.language, Text::Seen)
                    } else {
                        tr(self.language, Text::Unseen)
                    }
                )
            } else {
                attention_label(self.language, &observation.attention_kind).to_owned()
            };
            if two_line {
                let activity = if self.connected {
                    activity_label(self.language, &observation.activity)
                } else {
                    tr(self.language, Text::LastKnown)
                };
                let id_width = rect.width.saturating_sub(cell_width(activity) as u16 + 3);
                let first = Line::from(vec![
                    Span::styled(
                        format!(" {}", clip(&observation.id, id_width as usize)),
                        Style::default().fg(palette.text).add_modifier(if selected {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                    ),
                    Span::styled(
                        format!("  {activity}"),
                        Style::default()
                            .fg(state_color)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]);
                frame.render_widget(
                    Paragraph::new(first),
                    Rect::new(rect.x, rect.y, rect.width, 1),
                );
                if height > 1 {
                    let compact_attention = if observation.pending {
                        format!(
                            "  {}  {} {}  {}",
                            compact_attention_label(self.language, &observation.attention_kind),
                            observation.attention_total(),
                            tr(self.language, Text::Pending),
                            if observation.acknowledged {
                                tr(self.language, Text::Seen)
                            } else {
                                tr(self.language, Text::Unseen)
                            }
                        )
                    } else {
                        format!(
                            "  {}",
                            compact_attention_label(self.language, &observation.attention_kind)
                        )
                    };
                    frame.render_widget(
                        Paragraph::new(clip(&compact_attention, rect.width as usize)).style(
                            Style::default().fg(if observation.pending {
                                palette.warning
                            } else {
                                palette.muted
                            }),
                        ),
                        Rect::new(rect.x, rect.y + 1, rect.width, 1),
                    );
                }
            } else {
                let first = Line::from(vec![
                    Span::styled(
                        format!(" {} ", clip(state, 18)),
                        Style::default()
                            .fg(state_color)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        clip(&observation.id, content_width.saturating_sub(20) as usize),
                        Style::default().fg(palette.text).add_modifier(if selected {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                    ),
                    Span::styled(format!("  {attention}"), Style::default().fg(state_color)),
                ]);
                frame.render_widget(
                    Paragraph::new(first),
                    Rect::new(rect.x, rect.y, rect.width, 1),
                );
            }
            self.push_hit(
                rect,
                HitTarget::Row {
                    id: observation.id.clone(),
                    revision: observation.attention_revision.clone(),
                },
                1,
            );
        }
        if has_scroll {
            self.draw_scrollbar(
                frame,
                Rect::new(inner.right() - 1, inner.y, 1, inner.height),
                self.list_scroll,
                self.list_page,
                rows.len(),
                HitTarget::ListTrack,
                HitTarget::ListThumb,
                palette,
                2,
            );
        }
    }

    fn draw_inspector(&mut self, frame: &mut Frame, area: Rect, palette: Palette) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" {} ", tr(self.language, Text::Inspector)))
            .title_style(Style::default().fg(palette.muted))
            .border_style(Style::default().fg(if self.focus == Focus::Inspector {
                palette.accent
            } else {
                palette.track
            }))
            .style(Style::default().bg(palette.panel));
        let inner = inset(area, 1);
        frame.render_widget(block, area);
        let lines = self.inspector_text();
        self.inspector_lines = lines.len();
        self.inspector_page = inner.height.max(1) as usize;
        self.clamp_selection_and_scroll();
        if inner.width == 0 || inner.height == 0 {
            return;
        }
        let has_scroll = lines.len() > self.inspector_page;
        let content = Rect::new(
            inner.x,
            inner.y,
            inner.width.saturating_sub(u16::from(has_scroll)),
            inner.height,
        );
        let visible = lines
            .iter()
            .skip(self.inspector_scroll)
            .take(self.inspector_page)
            .map(|line| Line::from(clip(line, content.width as usize)))
            .collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(visible)
                .style(Style::default().fg(palette.text))
                .wrap(Wrap { trim: false }),
            content,
        );
        self.push_hit(content, HitTarget::InspectorBody, 1);
        if has_scroll {
            self.draw_scrollbar(
                frame,
                Rect::new(inner.right() - 1, inner.y, 1, inner.height),
                self.inspector_scroll,
                self.inspector_page,
                lines.len(),
                HitTarget::InspectorTrack,
                HitTarget::InspectorThumb,
                palette,
                2,
            );
        }
    }

    fn inspector_text(&self) -> Vec<String> {
        let Some(row) = self.selected() else {
            return vec![tr(self.language, Text::NoSelection).to_owned()];
        };
        let mut lines = Vec::new();
        if !self.connected {
            lines.push(tr(self.language, Text::StaleEvidence).to_owned());
            lines.push(String::new());
        }
        lines.push(row.id.clone());
        lines.push(format!(
            "{}: {}",
            tr(self.language, Text::Activity),
            activity_label(self.language, &row.activity)
        ));
        if let Some(last) = &row.last_activity {
            lines.push(format!(
                "{}: {}",
                tr(self.language, Text::LastActivity),
                activity_label(self.language, last)
            ));
        }
        lines.push(if row.pending {
            format!(
                "{}: {} / {}",
                tr(self.language, Text::Attention),
                attention_label(self.language, &row.attention_kind),
                if row.acknowledged {
                    tr(self.language, Text::Seen)
                } else {
                    tr(self.language, Text::Unseen)
                }
            )
        } else {
            format!(
                "{}: {}",
                tr(self.language, Text::Attention),
                attention_label(self.language, &row.attention_kind)
            )
        });
        lines.push(format!(
            "{}: {} / {}: {}",
            tr(self.language, Text::Approval),
            row.permission_count,
            tr(self.language, Text::Question),
            row.question_count
        ));
        lines.push(String::new());
        lines.push(format!(
            "{}: {}",
            tr(self.language, Text::Source),
            row.source_id
        ));
        lines.push(format!(
            "{}: {}",
            tr(self.language, Text::Session),
            row.session_id
        ));
        lines.push(format!(
            "{}: {}",
            tr(self.language, Text::Pane),
            row.pane_id.as_deref().unwrap_or("--")
        ));
        lines.push(format!(
            "{}: {}",
            tr(self.language, Text::Freshness),
            if self.connected {
                evidence_label(self.language, &row.native_freshness)
            } else {
                tr(self.language, Text::LastKnown)
            }
        ));
        lines.push(format!(
            "{}: {}",
            tr(self.language, Text::Observed),
            utc_timestamp(row.observed_at_ms)
        ));
        lines.push(format!(
            "{}: {}",
            tr(self.language, Text::Binding),
            match row.binding.as_str() {
                "explicit_unverified" => tr(self.language, Text::ConfiguredUnverified),
                "invalidated" => tr(self.language, Text::BindingInvalidated),
                _ => tr(self.language, Text::StateUnavailable),
            }
        ));
        lines.push(format!(
            "{}: {}",
            tr(self.language, Text::FrontendVerified),
            if row.frontend_verified {
                tr(self.language, Text::Yes)
            } else {
                tr(self.language, Text::No)
            }
        ));
        lines.push(String::new());
        let process = process_label(self.language, &row.core_process);
        lines.push(if self.connected {
            format!(
                "{} / {}: {}",
                tr(self.language, Text::Core),
                tr(self.language, Text::Process),
                process
            )
        } else {
            format!(
                "{} / {}: {} ({})",
                tr(self.language, Text::Core),
                tr(self.language, Text::Process),
                process,
                tr(self.language, Text::LastKnown)
            )
        });
        lines.push(format!(
            "{}: {}",
            tr(self.language, Text::CoreEvidence),
            if self.connected {
                evidence_label(self.language, &row.core_freshness)
            } else {
                tr(self.language, Text::LastKnown)
            }
        ));
        lines.push(format!(
            "{}: {}",
            tr(self.language, Text::PtyGeneration),
            row.pty_generation.as_deref().unwrap_or("--")
        ));
        if !self.connected {
            lines.push(String::new());
            lines.push(tr(self.language, Text::NavigationUnavailable).to_owned());
        } else if !self.native_available {
            lines.push(String::new());
            lines.push(tr(self.language, Text::NativeUnavailable).to_owned());
        } else if !self.action_enabled(Action::GoToPane) {
            lines.push(String::new());
            lines.push(tr(self.language, Text::NavigationUnavailable).to_owned());
        }
        if let Some(details) = self.details.get(&row.id) {
            lines.push(String::new());
            lines.push(tr(self.language, Text::RawDetails).to_uppercase());
            flatten_details(details, "", 0, &mut lines);
        }
        lines
    }

    fn draw_actions(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        palette: Palette,
        dense: bool,
        expanded: bool,
    ) {
        frame.render_widget(
            Block::default().style(Style::default().bg(palette.panel)),
            area,
        );
        let line = if dense {
            area
        } else {
            Rect::new(area.x, area.y, area.width, 1)
        };
        let actions = self.visible_actions();
        self.action_controls = 0;
        self.action_focus = self.action_focus.min(actions.len().saturating_sub(1));
        if self.compact && self.last_area.width < 60 && !dense {
            let width = area.width.saturating_sub(1) / 2;
            for (index, action) in actions.into_iter().enumerate() {
                let column = (index % 2) as u16;
                let row = (index / 2) as u16;
                if row >= area.height {
                    break;
                }
                let rect = Rect::new(area.x + column * (width + 1), area.y + row, width, 1);
                let waiting = action == Action::MarkSeen && self.ack_in_flight.is_some();
                let label = format!(
                    "{} {}",
                    action_shortcut(action),
                    compact_action_label(
                        self.language,
                        action,
                        waiting,
                        expanded && action == Action::Expand
                    )
                );
                self.draw_control(
                    frame,
                    rect,
                    &clip(&label, rect.width.saturating_sub(2) as usize),
                    HitTarget::Action(action),
                    palette,
                    self.focus == Focus::Actions && self.action_focus == index,
                );
                self.action_controls += 1;
            }
            self.action_focus = self
                .action_focus
                .min(self.action_controls.saturating_sub(1));
            return;
        }
        let full_width = actions
            .iter()
            .map(|action| {
                let shortcut = action_shortcut(*action);
                let waiting = *action == Action::MarkSeen && self.ack_in_flight.is_some();
                cell_width(&format!(
                    "{shortcut} {}",
                    action_label(
                        self.language,
                        *action,
                        waiting,
                        expanded && *action == Action::Expand
                    )
                )) + 2
            })
            .sum::<usize>()
            + actions.len().saturating_sub(1);
        let use_full_labels = full_width <= line.width as usize;
        let mut x = line.x;
        for (index, action) in actions.into_iter().enumerate() {
            let shortcut = action_shortcut(action);
            let waiting = action == Action::MarkSeen && self.ack_in_flight.is_some();
            let full = format!(
                "{shortcut} {}",
                action_label(
                    self.language,
                    action,
                    waiting,
                    expanded && action == Action::Expand
                )
            );
            let label = if use_full_labels {
                full
            } else {
                shortcut.to_owned()
            };
            let width = (cell_width(&label) + 2) as u16;
            if x + width > line.right() {
                break;
            }
            let rect = Rect::new(x, line.y, width, 1);
            self.draw_control(
                frame,
                rect,
                &label,
                HitTarget::Action(action),
                palette,
                self.focus == Focus::Actions && self.action_focus == index,
            );
            self.action_controls += 1;
            x += width + 1;
        }
        self.action_focus = self
            .action_focus
            .min(self.action_controls.saturating_sub(1));
    }

    fn draw_footer(&mut self, frame: &mut Frame, area: Rect, palette: Palette) {
        if area.height == 0 {
            return;
        }
        frame.render_widget(
            Block::default().style(Style::default().bg(palette.canvas)),
            area,
        );
        let status = self
            .toast
            .as_deref()
            .or(self.connection_message.as_deref())
            .map(str::to_owned)
            .unwrap_or_else(|| {
                if self.compact && self.last_area.width < 60 {
                    match self.language {
                        Language::English => "j/k Move  Enter Details  ? Help".to_owned(),
                        Language::Korean => "j/k 이동  Enter 상세  ? 도움말".to_owned(),
                    }
                } else {
                    self.current_hint()
                }
            });
        let color = if self.toast.is_some() {
            palette.accent
        } else if self.connected {
            palette.muted
        } else {
            palette.warning
        };
        frame.render_widget(
            Paragraph::new(clip(&status, area.width as usize)).style(Style::default().fg(color)),
            Rect::new(area.x, area.y, area.width, 1),
        );
        if area.height > 1 && self.compact {
            frame.render_widget(
                Paragraph::new(clip(
                    tr(self.language, Text::SharedSidebarShort),
                    area.width as usize,
                ))
                .style(Style::default().fg(palette.muted)),
                Rect::new(area.x, area.y + 1, area.width, 1),
            );
        }
    }

    fn current_hint(&self) -> String {
        if let Some(target) = &self.hover {
            return match target {
                HitTarget::Action(Action::MarkSeen) if !self.action_enabled(Action::MarkSeen) => {
                    tr(self.language, Text::AckUnavailable).to_owned()
                }
                HitTarget::Action(Action::GoToPane) if !self.action_enabled(Action::GoToPane) => {
                    tr(self.language, Text::NavigationUnavailable).to_owned()
                }
                HitTarget::Divider => tr(self.language, Text::DividerHint).to_owned(),
                HitTarget::Search | HitTarget::ClearSearch => {
                    tr(self.language, Text::SearchHint).to_owned()
                }
                HitTarget::Row { .. } => tr(self.language, Text::ListHint).to_owned(),
                _ => self.focus_hint().to_owned(),
            };
        }
        self.focus_hint().to_owned()
    }

    fn focus_hint(&self) -> &'static str {
        match self.focus {
            Focus::Filters => tr(self.language, Text::FilterHint),
            Focus::Search => tr(self.language, Text::SearchHint),
            Focus::List => tr(self.language, Text::ListHint),
            Focus::Inspector => tr(self.language, Text::InspectorHint),
            Focus::Actions => tr(self.language, Text::ActionHint),
            Focus::Header => tr(self.language, Text::ClipboardHint),
        }
    }

    fn draw_overlay(&mut self, frame: &mut Frame, area: Rect, palette: Palette) {
        match self.overlay {
            Some(Overlay::Help) => self.draw_help(frame, area, palette),
            Some(Overlay::Context) => self.draw_context(frame, area, palette),
            None => {}
        }
    }

    fn draw_help(&mut self, frame: &mut Frame, area: Rect, palette: Palette) {
        let width = area.width.saturating_sub(4).clamp(24, 86);
        let height = area.height.saturating_sub(2).clamp(7, 34);
        let modal = centered(area, width, height);
        frame.render_widget(Clear, modal);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" {} ", tr(self.language, Text::HelpTitle)))
            .border_style(Style::default().fg(palette.accent))
            .style(Style::default().bg(palette.panel).fg(palette.text));
        let inner = inset(modal, 1);
        frame.render_widget(block, modal);
        self.push_hit(modal, HitTarget::OverlaySurface, 100);
        let close_label = format!("Esc {}", tr(self.language, Text::Close));
        let close_width = (cell_width(&close_label) + 2).min(inner.width as usize) as u16;
        let close = Rect::new(
            inner.right().saturating_sub(close_width),
            inner.y,
            close_width,
            1,
        );
        self.draw_control(
            frame,
            close,
            &close_label,
            HitTarget::DismissOverlay,
            palette,
            false,
        );
        if let Some(hit) = self.hits.last_mut() {
            hit.z = 102;
        }
        let content = Rect::new(
            inner.x,
            inner.y + 1,
            inner.width,
            inner.height.saturating_sub(1),
        );
        let lines = help_lines(self.language);
        self.help_lines = lines.len();
        self.help_page = content.height.max(1) as usize;
        self.clamp_selection_and_scroll();
        let has_scroll = lines.len() > self.help_page;
        let text_area = Rect::new(
            content.x,
            content.y,
            content.width.saturating_sub(u16::from(has_scroll)),
            content.height,
        );
        let visible = lines
            .iter()
            .skip(self.help_scroll)
            .take(self.help_page)
            .map(|line| Line::from(clip(line, text_area.width as usize)))
            .collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(visible).wrap(Wrap { trim: false }),
            text_area,
        );
        self.push_hit(text_area, HitTarget::HelpBody, 101);
        if has_scroll {
            self.draw_scrollbar(
                frame,
                Rect::new(content.right() - 1, content.y, 1, content.height),
                self.help_scroll,
                self.help_page,
                lines.len(),
                HitTarget::HelpTrack,
                HitTarget::HelpThumb,
                palette,
                102,
            );
        }
    }

    fn draw_context(&mut self, frame: &mut Frame, area: Rect, palette: Palette) {
        let actions = self.context_actions();
        let width = if self.language == Language::Korean {
            30
        } else {
            26
        }
        .min(area.width - 2);
        let height = (actions.len() as u16 + 2).min(area.height);
        let x = self
            .context_anchor
            .0
            .min(area.right().saturating_sub(width));
        let y = self
            .context_anchor
            .1
            .min(area.bottom().saturating_sub(height));
        let menu = Rect::new(x, y, width, height);
        frame.render_widget(Clear, menu);
        frame.render_widget(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" {} ", tr(self.language, Text::ContextMenu)))
                .border_style(Style::default().fg(palette.accent))
                .style(Style::default().bg(palette.panel)),
            menu,
        );
        self.push_hit(menu, HitTarget::OverlaySurface, 100);
        let inner = inset(menu, 1);
        self.context_selection = self.context_selection.min(actions.len().saturating_sub(1));
        for (index, action) in actions.into_iter().enumerate().take(inner.height as usize) {
            let rect = Rect::new(inner.x, inner.y + index as u16, inner.width, 1);
            let waiting = action == Action::MarkSeen && self.ack_in_flight.is_some();
            let label = action_label(self.language, action, waiting, false);
            self.draw_control(
                frame,
                rect,
                label,
                HitTarget::ContextItem(action),
                palette,
                self.context_selection == index,
            );
            if let Some(hit) = self.hits.last_mut() {
                hit.z = 101;
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_scrollbar(
        &mut self,
        frame: &mut Frame,
        track: Rect,
        offset: usize,
        page: usize,
        total: usize,
        track_target: HitTarget,
        thumb_target: HitTarget,
        palette: Palette,
        z: u8,
    ) {
        if track.width == 0 || track.height == 0 || total <= page {
            return;
        }
        frame.render_widget(
            Block::default().style(Style::default().bg(palette.track)),
            track,
        );
        self.push_hit(track, track_target, z);
        let thumb_height = ((track.height as usize * page) / total)
            .max(1)
            .min(track.height as usize);
        let max_offset = total.saturating_sub(page).max(1);
        let travel = track.height as usize - thumb_height;
        let thumb_y = track.y + (travel * offset.min(max_offset) / max_offset) as u16;
        let thumb = Rect::new(track.x, thumb_y, 1, thumb_height as u16);
        frame.render_widget(
            Block::default().style(Style::default().bg(palette.accent)),
            thumb,
        );
        self.push_hit(thumb, thumb_target, z + 1);
    }

    fn draw_control(
        &mut self,
        frame: &mut Frame,
        rect: Rect,
        label: &str,
        target: HitTarget,
        palette: Palette,
        selected: bool,
    ) {
        if rect.width == 0 || rect.height == 0 {
            return;
        }
        let hovered = self.hover.as_ref() == Some(&target);
        let pressed = self
            .pressed
            .as_ref()
            .is_some_and(|pressed| pressed.target == target);
        let enabled = match target {
            HitTarget::Action(action) | HitTarget::ContextItem(action) => {
                self.action_enabled(action)
            }
            _ => true,
        };
        let (fg, bg) = if !enabled {
            (palette.muted, palette.panel)
        } else if pressed {
            (palette.accent_text, palette.warning)
        } else if selected || hovered {
            (palette.accent_text, palette.accent)
        } else {
            (palette.text, palette.raised)
        };
        let text = format!(" {} ", label);
        frame.render_widget(
            Paragraph::new(clip(&text, rect.width as usize)).style(
                Style::default().fg(fg).bg(bg).add_modifier(if selected {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
            ),
            rect,
        );
        self.push_hit(rect, target, 1);
    }

    fn push_hit(&mut self, rect: Rect, target: HitTarget, z: u8) {
        if let Some(rect) = intersection(rect, self.last_area)
            && rect.width > 0
            && rect.height > 0
        {
            self.hits.push(HitRegion { rect, target, z });
        }
    }
}

fn flatten_details(value: &serde_json::Value, prefix: &str, depth: usize, lines: &mut Vec<String>) {
    if depth > 3 || lines.len() > 120 {
        return;
    }
    match value {
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                let name = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                match value {
                    serde_json::Value::Object(_) | serde_json::Value::Array(_) => {
                        flatten_details(value, &name, depth + 1, lines)
                    }
                    _ => lines.push(format!("{name}: {}", plain_json(value))),
                }
            }
        }
        serde_json::Value::Array(values) => {
            for (index, value) in values.iter().enumerate().take(64) {
                let name = format!("{prefix}[{index}]");
                match value {
                    serde_json::Value::Object(_) | serde_json::Value::Array(_) => {
                        flatten_details(value, &name, depth + 1, lines)
                    }
                    _ => lines.push(format!("{name}: {}", plain_json(value))),
                }
            }
        }
        _ => lines.push(format!("{prefix}: {}", plain_json(value))),
    }
}

fn plain_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => sanitize(text),
        other => other.to_string(),
    }
}

fn action_shortcut(action: Action) -> &'static str {
    match action {
        Action::MarkSeen => "a",
        Action::GoToPane => "g",
        Action::Details => "Enter",
        Action::CopyId => "c",
        Action::Expand => "z",
        Action::Retry => "r",
    }
}

fn utc_timestamp(milliseconds: u64) -> String {
    let seconds = milliseconds / 1_000;
    let millis = milliseconds % 1_000;
    let days = (seconds / 86_400) as i64;
    let day_seconds = seconds % 86_400;
    let hour = day_seconds / 3_600;
    let minute = day_seconds % 3_600 / 60;
    let second = day_seconds % 60;

    // Proleptic Gregorian conversion from Unix days. Keeping it local avoids
    // a time dependency and, unlike an age label, needs no idle redraw clock.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}.{millis:03} UTC")
}

fn search_window(text: &str, cursor: usize, width: usize) -> (String, usize, usize) {
    if width == 0 {
        return (String::new(), 0, cursor);
    }
    let graphemes = text.graphemes(true).collect::<Vec<_>>();
    let cursor = cursor.min(graphemes.len());
    let mut start = cursor;
    let mut used = 0;
    while start > 0 {
        let next = UnicodeWidthStr::width(graphemes[start - 1]);
        if used + next > width.saturating_sub(1) {
            break;
        }
        start -= 1;
        used += next;
    }
    let cursor_x = graphemes[start..cursor]
        .iter()
        .map(|grapheme| UnicodeWidthStr::width(*grapheme))
        .sum();
    let mut shown = String::new();
    let mut cells = 0;
    for grapheme in graphemes.iter().skip(start) {
        let next = UnicodeWidthStr::width(*grapheme);
        if cells + next > width {
            break;
        }
        shown.push_str(grapheme);
        cells += next;
    }
    (shown, cursor_x, start)
}

fn clip(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let mut output = String::new();
    let mut used = 0;
    for grapheme in text.graphemes(true) {
        let safe = if grapheme.chars().any(char::is_control) {
            " "
        } else {
            grapheme
        };
        let next = UnicodeWidthStr::width(safe);
        if used + next > width {
            break;
        }
        output.push_str(safe);
        used += next;
    }
    output
}

fn sanitize(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

fn cell_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

fn inset(rect: Rect, amount: u16) -> Rect {
    Rect::new(
        rect.x.saturating_add(amount),
        rect.y.saturating_add(amount),
        rect.width.saturating_sub(amount.saturating_mul(2)),
        rect.height.saturating_sub(amount.saturating_mul(2)),
    )
}

fn inset_y(rect: Rect, amount: u16) -> Rect {
    Rect::new(
        rect.x,
        rect.y.saturating_add(amount),
        rect.width,
        rect.height.saturating_sub(amount.saturating_mul(2)).max(1),
    )
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width.min(area.width),
        height.min(area.height),
    )
}

fn intersection(left: Rect, right: Rect) -> Option<Rect> {
    let x = left.x.max(right.x);
    let y = left.y.max(right.y);
    let right_edge = left.right().min(right.right());
    let bottom = left.bottom().min(right.bottom());
    (right_edge > x && bottom > y).then(|| Rect::new(x, y, right_edge - x, bottom - y))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::{App, Language, Theme};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use serde_json::json;

    fn snapshot() -> serde_json::Value {
        json!({
            "epoch": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "revision": "1",
            "observations": [{
                "id": "build_agent", "source_id": "local", "session_id": "세션_one", "pane_id": "%2",
                "native": {"exists": true, "activity": "working", "last_activity": null,
                    "attention": "approval", "permission_count": 1, "question_count": 0,
                    "freshness": "fresh", "observed_at_ms": 42},
                "core": {"process": "running", "pty_generation": "9", "freshness": "fresh"},
                "binding": "explicit_unverified", "frontend_verified": false,
                "attention": {"revision": "3", "acknowledged": false, "pending": true, "available": true}
            }]
        })
    }

    fn draw_at(width: u16, height: u16, compact: bool, language: Language) -> App {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new(compact, language, Theme::Dark);
        app.apply_snapshot(snapshot());
        app.set_connection(true, None);
        app.set_native_available(true);
        terminal.draw(|frame| app.draw(frame)).unwrap();
        app
    }

    #[test]
    fn responsive_views_only_register_visible_geometry() {
        for (width, height, compact) in [(120, 34, false), (80, 24, false), (34, 18, true)] {
            let app = draw_at(width, height, compact, Language::English);
            assert!(!app.hits.is_empty());
            assert!(app.hits.iter().all(|hit| {
                hit.rect.width > 0
                    && hit.rect.height > 0
                    && hit.rect.right() <= width
                    && hit.rect.bottom() <= height
            }));
            assert!(
                app.hits
                    .iter()
                    .any(|hit| matches!(hit.target, HitTarget::Row { .. }))
            );
        }
    }

    #[test]
    fn tiny_view_keeps_a_close_target() {
        let app = draw_at(20, 6, false, Language::English);
        assert_eq!(app.hits.len(), 1);
        assert_eq!(app.hits[0].target, HitTarget::Close);
    }

    #[test]
    fn korean_sidebar_renders_and_keeps_actions_reachable() {
        let app = draw_at(34, 18, true, Language::Korean);
        assert!(
            app.hits
                .iter()
                .any(|hit| matches!(hit.target, HitTarget::Action(Action::MarkSeen)))
        );
        assert!(
            app.hits
                .iter()
                .any(|hit| matches!(hit.target, HitTarget::Action(Action::Expand)))
        );
    }

    #[test]
    fn compact_filters_and_actions_keep_names_and_hit_targets() {
        let backend = TestBackend::new(34, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new(true, Language::English, Theme::Dark);
        app.apply_snapshot(snapshot());
        app.set_connection(true, None);
        app.set_native_available(true);
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let mut screen = String::new();
        for y in 0..20 {
            for x in 0..34 {
                screen.push_str(buffer.cell((x, y)).map(|cell| cell.symbol()).unwrap_or(" "));
            }
            screen.push('\n');
        }
        for label in [
            "All",
            "Wait",
            "Work",
            "Unavailable",
            "Mark seen",
            "Pane",
            "Details",
            "Expand",
        ] {
            assert!(
                screen.contains(label),
                "missing compact label {label:?}\n{screen}"
            );
        }
        assert_eq!(
            app.hits
                .iter()
                .filter(|hit| matches!(hit.target, HitTarget::Filter(_)))
                .count(),
            4
        );
        assert_eq!(
            app.hits
                .iter()
                .filter(|hit| matches!(hit.target, HitTarget::Action(_)))
                .count(),
            4
        );
    }

    #[test]
    fn no_pending_attention_is_neither_seen_nor_unseen() {
        let mut value = snapshot();
        value["observations"][0]["native"]["attention"] = json!("none");
        value["observations"][0]["native"]["permission_count"] = json!(0);
        value["observations"][0]["attention"]["pending"] = json!(false);
        let mut app = App::new(false, Language::English, Theme::Dark);
        app.apply_snapshot(value);
        let text = app.inspector_text().join("\n");
        assert!(text.contains("Attention: None"));
        assert!(!text.contains("Unseen"));
        assert!(!text.contains("Seen"));
    }

    #[test]
    fn every_small_and_breakpoint_geometry_is_panic_free() {
        for width in 1..=105 {
            for height in 1..=25 {
                let backend = TestBackend::new(width, height);
                let mut terminal = Terminal::new(backend).unwrap();
                let mut app = App::new(width < 50, Language::English, Theme::Terminal);
                app.apply_snapshot(snapshot());
                terminal.draw(|frame| app.draw(frame)).unwrap();
                assert!(app.hits.iter().all(|hit| {
                    hit.rect.width > 0
                        && hit.rect.height > 0
                        && hit.rect.right() <= width
                        && hit.rect.bottom() <= height
                }));
            }
        }
    }
}
