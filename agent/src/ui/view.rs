use super::i18n::{
    HelpItem, Text, action_label, activity_label, attention_label, compact_action_label,
    compact_attention_label, compact_filter_label, evidence_label, filter_label, help_items,
    language_name, process_label, theme_name, tr,
};
use super::model::{
    Action, App, Filter, Focus, HitRegion, HitTarget, Language, Observation, Overlay, Theme,
};
use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Copy)]
pub(super) struct Palette {
    pub(super) canvas: Color,
    pub(super) panel: Color,
    pub(super) raised: Color,
    pub(super) text: Color,
    pub(super) muted: Color,
    pub(super) accent: Color,
    pub(super) accent_text: Color,
    pub(super) warning: Color,
    pub(super) danger: Color,
    pub(super) success: Color,
    pub(super) track: Color,
}

impl Palette {
    pub(super) fn for_theme(theme: Theme) -> Self {
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

    pub(super) fn tone(self, tone: Tone) -> Color {
        match tone {
            Tone::Normal => self.text,
            Tone::Muted => self.muted,
            Tone::Good => self.success,
            Tone::Warn => self.warning,
            Tone::Bad => self.danger,
        }
    }
}

/// Semantic text roles. Color only reinforces a label that is already visible.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Tone {
    Normal,
    Muted,
    Good,
    Warn,
    Bad,
}

/// Identity cells a one-line list row keeps before attention moves to a
/// second line.
const MIN_ID_COLUMN: usize = 12;

/// A labelled control. The key hint is drawn in the accent role so keyboard
/// routes stay visible next to every mouse target.
#[derive(Clone)]
struct Control {
    key: String,
    label: String,
    quiet: bool,
    key_right: bool,
}

impl Control {
    fn new(key: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            label: label.into(),
            quiet: false,
            key_right: false,
        }
    }

    fn quiet(mut self, quiet: bool) -> Self {
        self.quiet = quiet;
        self
    }

    fn width(&self) -> u16 {
        let key = cell_width(&self.key);
        let label = cell_width(&self.label);
        let gap = usize::from(key > 0 && label > 0);
        (key + gap + label + 2) as u16
    }
}

/// Picks the most descriptive label set whose controls fit on one line.
fn fit_level(levels: &[Vec<Control>], gap: u16, width: u16) -> usize {
    levels
        .iter()
        .position(|controls| row_width(controls, gap) <= width)
        .unwrap_or(levels.len().saturating_sub(1))
}

fn row_width(controls: &[Control], gap: u16) -> u16 {
    controls
        .iter()
        .map(Control::width)
        .sum::<u16>()
        .saturating_add(gap.saturating_mul(controls.len().saturating_sub(1) as u16))
}

/// One logical inspector entry before wrapping to the current width.
#[derive(Clone, Debug, PartialEq)]
enum Entry {
    Banner(String),
    Title(String),
    Heading(String),
    Field(&'static str, String, Tone),
    Note(String),
    Placeholder(String),
    Raw(String),
    Gap,
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
        // Chrome text lines up with the text inside bordered regions.
        let gutter = u16::from(!compact_narrow);
        let header_height = if dense {
            1
        } else if compact_narrow || area.height >= 28 {
            3
        } else {
            1
        };
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
        // The inbox view replaces filters, search, list and actions: they all
        // act on agents, which this view does not select.
        if self.managed && self.inbox.open {
            let body_height = area
                .height
                .saturating_sub(header_height + footer_height)
                .max(1);
            let header = Rect::new(area.x, area.y, area.width, header_height);
            let body = Rect::new(area.x, area.y + header_height, area.width, body_height);
            let footer_y = body.bottom();
            let footer = Rect::new(
                area.x,
                footer_y,
                area.width,
                footer_height.min(area.bottom().saturating_sub(footer_y)),
            );
            self.draw_header(frame, header, palette, dense, gutter);
            self.draw_inbox(frame, body, palette);
            self.draw_footer(frame, footer, palette, gutter);
            self.draw_overlay(frame, area, palette);
            self.dirty = false;
            return;
        }
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

        self.draw_header(frame, header, palette, dense, gutter);
        self.draw_filters(frame, filters, palette, dense, gutter);
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
                self.draw_divider(frame, divider, palette);
            }
            self.draw_inspector(frame, inspector, palette);
        } else if self.details_open {
            self.draw_inspector(frame, body, palette);
        } else {
            self.draw_list(frame, body, palette, self.compact || area.width < 60);
        }

        self.draw_actions(frame, actions, palette, dense, wide, gutter);
        self.draw_footer(frame, footer, palette, gutter);
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
        let inner = if inner.width > 2 {
            inset_x(inner, 1)
        } else {
            inner
        };
        if inner.width == 0 || inner.height == 0 {
            return;
        }
        // Bottom-up: Close, current size, minimum size, then the message.
        let mut bottom = inner.bottom();
        let close = Control::new("q", tr(self.language, Text::Close));
        let close_width = close.width().min(inner.width);
        bottom -= 1;
        self.draw_control(
            frame,
            Rect::new(
                inner.right().saturating_sub(close_width),
                bottom,
                close_width,
                1,
            ),
            &close,
            HitTarget::Close,
            palette,
            false,
        );
        let current = format!(
            "{} {} x {}",
            tr(self.language, Text::CurrentSize),
            area.width,
            area.height
        );
        for (text, color) in [
            (tr(self.language, Text::MinimumSize), palette.muted),
            (current.as_str(), palette.text),
        ] {
            if bottom <= inner.y + 1 {
                break;
            }
            bottom -= 1;
            frame.render_widget(
                Paragraph::new(clip(text, inner.width as usize)).style(Style::default().fg(color)),
                Rect::new(inner.x, bottom, inner.width, 1),
            );
        }
        let message = Rect::new(
            inner.x,
            inner.y,
            inner.width,
            bottom.saturating_sub(inner.y),
        );
        if message.height > 0 {
            let lines = wrap_rows(
                tr(self.language, Text::TerminalTooSmall),
                message.width as usize,
                message.height as usize,
            );
            frame.render_widget(
                Paragraph::new(lines.into_iter().map(Line::from).collect::<Vec<_>>()).style(
                    Style::default()
                        .fg(palette.warning)
                        .add_modifier(Modifier::BOLD),
                ),
                message,
            );
        }
    }

    fn connection_label(&self, short: bool, palette: Palette) -> (&'static str, Color) {
        if self.managed && !short {
            return (
                tr(
                    self.language,
                    if self.connected {
                        Text::NativeConnected
                    } else {
                        Text::NativeServerUnavailable
                    },
                ),
                if self.connected {
                    palette.success
                } else {
                    palette.warning
                },
            );
        }
        let key = match (self.connected, self.rows.is_empty(), short) {
            (true, _, false) => Text::Connected,
            (true, _, true) => Text::ShortConnected,
            (false, true, false) => Text::Connecting,
            (false, true, true) => Text::ShortConnecting,
            (false, false, false) => Text::Disconnected,
            (false, false, true) => Text::ShortDisconnected,
        };
        let color = if self.connected {
            palette.success
        } else {
            palette.warning
        };
        (tr(self.language, key), color)
    }

    /// Header controls in focus-index order: Close, Help, Theme, Language.
    fn header_levels(&self) -> Vec<Vec<Control>> {
        let language = language_name(self.language);
        let theme = theme_name(self.language, self.theme);
        let close = tr(self.language, Text::Close);
        let help = tr(self.language, Text::Help);
        vec![
            vec![
                Control::new("q", close),
                Control::new("?", help),
                Control::new("t", theme),
                Control::new("l", language),
            ],
            vec![
                Control::new("q", ""),
                Control::new("?", ""),
                Control::new("t", theme),
                Control::new("l", language),
            ],
            vec![
                Control::new("q", ""),
                Control::new("?", ""),
                Control::new("t", ""),
                Control::new("l", language),
            ],
            vec![
                Control::new("q", ""),
                Control::new("?", ""),
                Control::new("t", ""),
                Control::new("l", ""),
            ],
        ]
    }

    fn draw_header(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        palette: Palette,
        dense: bool,
        gutter: u16,
    ) {
        frame.render_widget(
            Block::default().style(Style::default().bg(palette.panel)),
            area,
        );
        let product = Span::styled(
            tr(self.language, Text::Product),
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        );
        let levels = self.header_levels();
        self.header_controls = 0;
        if self.compact && area.width < 60 && !dense {
            // Row 0: identity and connection. Row 1: controls. Row 2 separates
            // the header controls from the filter controls below.
            let (status, color) = self.connection_label(true, palette);
            let status_width = cell_width(status).min(area.width as usize) as u16;
            let title_width = area.width.saturating_sub(status_width + 1);
            let title = clip_line(
                vec![
                    product,
                    Span::styled(
                        format!(" {}", tr(self.language, Text::Agents)),
                        Style::default().fg(palette.text),
                    ),
                    self.inbox_span(palette),
                ],
                title_width as usize,
            );
            frame.render_widget(
                Paragraph::new(title),
                Rect::new(area.x, area.y, title_width, 1),
            );
            frame.render_widget(
                Paragraph::new(status).style(Style::default().fg(color)),
                Rect::new(
                    area.right().saturating_sub(status_width),
                    area.y,
                    status_width,
                    1,
                ),
            );
            let level = fit_level(&levels, 1, area.width);
            let controls_y = area.y + 1.min(area.height.saturating_sub(1));
            let mut x = area.x;
            // Drawn left to right, so walk the focus order backwards.
            for (focus_index, control) in levels[level].iter().enumerate().rev() {
                let width = control.width();
                if x + width > area.right() {
                    break;
                }
                let target = header_target(focus_index);
                self.draw_control(
                    frame,
                    Rect::new(x, controls_y, width, 1),
                    control,
                    target,
                    palette,
                    self.focus == Focus::Header && self.header_focus == focus_index,
                );
                self.header_controls += 1;
                x += width + 1;
            }
            return;
        }
        let line = inset_x(
            if area.height >= 3 {
                inset_y(area, 1)
            } else {
                Rect::new(area.x, area.y, area.width, 1)
            },
            gutter,
        );
        let view = if self.compact && self.last_area.width < 100 {
            tr(self.language, Text::Sidebar)
        } else {
            tr(
                self.language,
                if self.managed {
                    Text::ManagedDesk
                } else {
                    Text::Desk
                },
            )
        };
        let (connection, connection_color) = self.connection_label(false, palette);
        let (short, _) = self.connection_label(true, palette);
        let view = Span::styled(format!("  {view}"), Style::default().fg(palette.text));
        let status = |text: String| Span::styled(text, Style::default().fg(connection_color));
        let inbox = self.inbox_span(palette);
        let titles = [
            vec![
                product.clone(),
                view.clone(),
                inbox.clone(),
                status(format!("  {connection}")),
            ],
            vec![
                product.clone(),
                view,
                inbox.clone(),
                status(format!("  {short}")),
            ],
            vec![product, inbox, status(format!("  {short}"))],
            vec![status(short.to_owned())],
        ];
        // Title and control labels shorten in turns until both fit.
        let (title, level) = [(0, 0), (0, 1), (1, 1), (1, 2), (2, 2), (2, 3), (3, 3)]
            .into_iter()
            .find(|&(title, level)| {
                spans_width(&titles[title]) + 2 + row_width(&levels[level], 1) as usize
                    <= line.width as usize
            })
            .unwrap_or((3, 3));
        let title = titles[title].clone();
        let mut right = line.right();
        for (index, control) in levels[level].iter().enumerate() {
            let needed = control.width();
            if right < line.x + needed {
                break;
            }
            right -= needed;
            let target = header_target(index);
            self.draw_control(
                frame,
                Rect::new(right, line.y, needed, 1),
                control,
                target,
                palette,
                self.focus == Focus::Header && self.header_focus == index,
            );
            self.header_controls += 1;
            right = right.saturating_sub(1);
        }
        let title_room = right.saturating_sub(line.x).saturating_sub(1) as usize;
        let title = if spans_width(&title) > title_room {
            let mut title = clip_line(title, title_room.saturating_sub(1)).spans;
            title.push(Span::styled("…", Style::default().fg(connection_color)));
            Line::from(title)
        } else {
            Line::from(title)
        };
        frame.render_widget(
            Paragraph::new(title),
            Rect::new(line.x, line.y, title_room as u16, 1),
        );
    }

    fn filter_counts(&self) -> [usize; 4] {
        [
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
        ]
    }

    fn draw_filters(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        palette: Palette,
        dense: bool,
        gutter: u16,
    ) {
        frame.render_widget(
            Block::default().style(Style::default().bg(palette.canvas)),
            area,
        );
        let counts = self.filter_counts();
        let labels: [fn(Language, Filter) -> &'static str; 2] =
            [filter_label, compact_filter_label];
        let levels = labels
            .into_iter()
            .map(|label| {
                Filter::ALL
                    .into_iter()
                    .enumerate()
                    .map(|(index, filter)| {
                        Control::new(
                            "",
                            format!("{} {}", label(self.language, filter), counts[index]),
                        )
                        .quiet(counts[index] == 0)
                    })
                    .collect::<Vec<_>>()
            })
            .chain(std::iter::once(
                counts
                    .iter()
                    .map(|count| Control::new("", count.to_string()).quiet(*count == 0))
                    .collect(),
            ))
            .collect::<Vec<_>>();

        if self.compact && area.width < 60 && !dense {
            // Two labelled columns across two rows.
            let width = area.width.saturating_sub(1) / 2;
            let level = levels
                .iter()
                .position(|controls| controls.iter().all(|control| control.width() <= width))
                .unwrap_or(levels.len() - 1);
            for (index, filter) in Filter::ALL.into_iter().enumerate() {
                let column = (index % 2) as u16;
                let row = (index / 2) as u16;
                let rect = Rect::new(area.x + column * (width + 1), area.y + row, width, 1);
                self.draw_control(
                    frame,
                    rect,
                    &levels[level][index],
                    HitTarget::Filter(filter),
                    palette,
                    self.filter == filter,
                );
            }
            return;
        }
        // A one-row header leaves no gap above the filters, so the spare row
        // moves above them.
        let row = if area.height > 1 && self.last_area.height < 28 {
            area.y + 1
        } else {
            area.y
        };
        let line = inset_x(Rect::new(area.x, row, area.width, 1), gutter);
        let level = fit_level(&levels, 1, line.width);
        let mut x = line.x;
        for (index, filter) in Filter::ALL.into_iter().enumerate() {
            let control = &levels[level][index];
            let width = control.width();
            if x + width > line.right() {
                break;
            }
            self.draw_control(
                frame,
                Rect::new(x, line.y, width, 1),
                control,
                HitTarget::Filter(filter),
                palette,
                self.filter == filter,
            );
            x = x.saturating_add(width + 1);
        }
    }

    fn draw_search(&mut self, frame: &mut Frame, area: Rect, palette: Palette, dense: bool) {
        let focused = self.focus == Focus::Search;
        let block = if dense {
            Block::default().style(Style::default().bg(palette.panel))
        } else {
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(if focused {
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
        let clear =
            (!self.search.is_empty()).then(|| Control::new("", tr(self.language, Text::Clear)));
        let clear_width = clear
            .as_ref()
            .map_or(0, |control| control.width().min(inner.width));
        let field_width = inner.width.saturating_sub(clear_width);
        let field = Rect::new(inner.x, inner.y, field_width, 1);
        let cursor_x = if self.search.is_empty() {
            self.search_view_start = 0;
            let placeholder = vec![
                Span::styled(
                    "/ ",
                    Style::default()
                        .fg(if focused {
                            palette.muted
                        } else {
                            palette.accent
                        })
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    tr(self.language, Text::Search),
                    Style::default().fg(palette.muted),
                ),
            ];
            frame.render_widget(
                Paragraph::new(clip_line(placeholder, field.width as usize)),
                field,
            );
            0
        } else {
            let (shown, cursor_x, start) = search_window(
                &self.search,
                self.search_cursor,
                field_width.saturating_sub(1) as usize,
            );
            self.search_view_start = start;
            frame.render_widget(
                Paragraph::new(clip(&shown, field.width as usize))
                    .style(Style::default().fg(palette.text)),
                field,
            );
            cursor_x
        };
        self.push_hit(field, HitTarget::Search, 1);
        if let Some(control) = clear
            && clear_width > 0
        {
            let rect = Rect::new(field.right(), inner.y, clear_width, 1);
            self.draw_control(
                frame,
                rect,
                &control,
                HitTarget::ClearSearch,
                palette,
                false,
            );
        }
        if focused && self.overlay.is_none() && field.width > 0 {
            let x = field
                .x
                .saturating_add(cursor_x.min(field.width.saturating_sub(1) as usize) as u16);
            frame.set_cursor_position((x, field.y));
        }
    }

    fn region_block(&self, title: Text, focused: bool, palette: Palette) -> Block<'static> {
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" {} ", tr(self.language, title)))
            .title_style(if focused {
                Style::default()
                    .fg(palette.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(palette.muted)
            })
            .border_style(Style::default().fg(if focused {
                palette.accent
            } else {
                palette.track
            }))
            .style(Style::default().bg(palette.panel))
    }

    /// Activity column: a disconnected observer or unavailable source never
    /// shows a current activity claim.
    fn row_state(&self, row: &Observation) -> (&'static str, Tone) {
        if !self.connected {
            (tr(self.language, Text::LastKnown), Tone::Bad)
        } else if row.is_unavailable() {
            (tr(self.language, Text::StateUnavailable), Tone::Bad)
        } else {
            let tone = match row.activity.as_str() {
                "working" => Tone::Good,
                "retrying" => Tone::Warn,
                _ => Tone::Muted,
            };
            (activity_label(self.language, &row.activity), tone)
        }
    }

    /// Attention column: named kind, count and seen state, each in words.
    fn row_attention(&self, row: &Observation, verbose: bool) -> Vec<(String, Tone, bool)> {
        let kind = compact_attention_label(self.language, &row.attention_kind);
        if !row.pending {
            return vec![(kind.to_owned(), Tone::Muted, false)];
        }
        let current = self.connected && row.attention_available;
        let (kind_tone, seen_tone, bold) = match (current, row.acknowledged) {
            (false, _) => (Tone::Muted, Tone::Muted, false),
            (true, false) => (Tone::Warn, Tone::Warn, true),
            (true, true) => (Tone::Normal, Tone::Muted, false),
        };
        vec![
            (
                if verbose {
                    format!(
                        "{kind} {} {}",
                        row.attention_total(),
                        tr(self.language, Text::Pending)
                    )
                } else {
                    format!("{kind} {}", row.attention_total())
                },
                kind_tone,
                false,
            ),
            (
                format!(
                    "  {}",
                    tr(
                        self.language,
                        if row.acknowledged {
                            Text::Seen
                        } else {
                            Text::Unseen
                        }
                    )
                ),
                seen_tone,
                bold,
            ),
        ]
    }

    fn draw_list(&mut self, frame: &mut Frame, area: Rect, palette: Palette, two_line: bool) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let block = self.region_block(Text::Agents, self.focus == Focus::List, palette);
        let inner = inset(area, 1);
        frame.render_widget(block, area);
        if inner.width == 0 || inner.height == 0 {
            self.list_page = 1;
            return;
        }
        let rows = self
            .filtered_rows()
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        // Columns are sized from every filtered row, not just the visible
        // page, so they stay put while the list scrolls.
        let state_width = rows
            .iter()
            .map(|row| cell_width(self.row_state(row).0))
            .max()
            .unwrap_or(0);
        let id_width = rows
            .iter()
            .map(|row| cell_width(&row.id))
            .max()
            .unwrap_or(0);
        let attention_width = |verbose: bool| {
            rows.iter()
                .map(|row| {
                    self.row_attention(row, verbose)
                        .iter()
                        .map(|(text, _, _)| cell_width(text))
                        .sum::<usize>()
                })
                .max()
                .unwrap_or(0)
        };
        let (short_attention, long_attention) = (attention_width(false), attention_width(true));
        // One line keeps the whole attention column and at least a short
        // identity prefix; otherwise attention moves to a second line.
        let one_line_scroll = rows.len() > inner.height as usize;
        let two_line = two_line
            || 2 + id_width.min(MIN_ID_COLUMN) + 2 + state_width + 2 + short_attention
                > inner.width.saturating_sub(u16::from(one_line_scroll)) as usize;
        let row_height = if two_line { 2 } else { 1 };
        let previous_page = self.list_page;
        self.list_page = (inner.height / row_height).max(1) as usize;
        if self.list_page == previous_page {
            self.clamp_selection_and_scroll();
        } else {
            self.reveal_selection();
        }
        if rows.is_empty() {
            let (message, hint) = self.empty_list_text();
            let width = inner.width.saturating_sub(2) as usize;
            let height = inner.height as usize;
            let mut lines = wrap_rows(message, width, height);
            let hint = if hint.is_empty() {
                Vec::new()
            } else {
                wrap(hint, width)
            };
            // The hint is dropped before the message is cut.
            if lines.len() + hint.len() <= height {
                lines.extend(hint);
            }
            let top = (height / 3).min(height - lines.len()) as u16;
            frame.render_widget(
                Paragraph::new(lines.into_iter().map(Line::from).collect::<Vec<_>>())
                    .style(Style::default().fg(palette.muted))
                    .alignment(Alignment::Center),
                inset_x(
                    Rect::new(inner.x, inner.y + top, inner.width, inner.height - top),
                    1,
                ),
            );
            return;
        }
        let has_scroll = rows.len() > self.list_page;
        let content_width = inner.width.saturating_sub(u16::from(has_scroll)) as usize;
        // Keep "pending" only while every row's attention fits beside the
        // identity; the kind, count and seen state always stay.
        let verbose = if two_line {
            long_attention + 2 <= content_width
        } else {
            2 + id_width + 2 + state_width + 2 + long_attention <= content_width
        };
        let attention_width = if verbose {
            long_attention
        } else {
            short_attention
        };
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
            let rect = Rect::new(inner.x, y, content_width as u16, height);
            let selected = self.selected_id.as_deref() == Some(observation.id.as_str());
            let target = HitTarget::Row {
                id: observation.id.clone(),
                revision: observation.attention_revision.clone(),
            };
            let hovered = self.hover.as_ref() == Some(&target);
            frame.render_widget(
                Block::default().style(Style::default().bg(if selected {
                    palette.raised
                } else {
                    palette.panel
                })),
                rect,
            );
            let marker = Span::styled(
                if selected || hovered { "› " } else { "  " },
                Style::default()
                    .fg(if selected {
                        palette.accent
                    } else {
                        palette.muted
                    })
                    .add_modifier(Modifier::BOLD),
            );
            let id_style = Style::default().fg(palette.text).add_modifier(if selected {
                Modifier::BOLD
            } else {
                Modifier::empty()
            });
            let (state, state_tone) = self.row_state(observation);
            let state_style = Style::default().fg(palette.tone(state_tone));
            let attention = self
                .row_attention(observation, verbose)
                .into_iter()
                .map(|(text, tone, bold)| {
                    Span::styled(
                        text,
                        Style::default()
                            .fg(palette.tone(tone))
                            .add_modifier(if bold {
                                Modifier::BOLD
                            } else {
                                Modifier::empty()
                            }),
                    )
                })
                .collect::<Vec<_>>();
            if two_line {
                // Identity left, state right; attention on its own line.
                let room = content_width.saturating_sub(2 + 2 + cell_width(state) + 1);
                let id = ellipsize(&observation.id, room);
                let pad = content_width.saturating_sub(2 + cell_width(&id) + cell_width(state) + 1);
                let first = vec![
                    marker,
                    Span::styled(id, id_style),
                    Span::raw(" ".repeat(pad)),
                    Span::styled(state, state_style),
                ];
                frame.render_widget(
                    Paragraph::new(clip_line(first, content_width)),
                    Rect::new(rect.x, rect.y, rect.width, 1),
                );
                if height > 1 {
                    // The indent goes before any attention text is cut.
                    let indent = if spans_width(&attention) + 2 <= content_width {
                        "  "
                    } else {
                        ""
                    };
                    let mut second = vec![Span::raw(indent)];
                    second.extend(attention);
                    frame.render_widget(
                        Paragraph::new(clip_line(second, content_width)),
                        Rect::new(rect.x, rect.y + 1, rect.width, 1),
                    );
                }
            } else {
                // Marker, identity, activity and attention in aligned columns.
                let room = content_width.saturating_sub(2 + 2 + state_width + 2);
                let id_column = id_width.min(room.saturating_sub(attention_width));
                let line = vec![
                    marker,
                    Span::styled(
                        pad(&ellipsize(&observation.id, id_column), id_column),
                        id_style,
                    ),
                    Span::raw("  "),
                    Span::styled(pad(state, state_width), state_style),
                    Span::raw("  "),
                ]
                .into_iter()
                .chain(attention)
                .collect();
                frame.render_widget(
                    Paragraph::new(clip_line(line, content_width)),
                    Rect::new(rect.x, rect.y, rect.width, 1),
                );
            }
            self.push_hit(rect, target, 1);
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

    /// Explains why the list is empty without implying a completed state.
    fn empty_list_text(&self) -> (&'static str, &'static str) {
        if self.rows.is_empty() {
            if self.connected {
                if self.managed {
                    (
                        tr(self.language, Text::NoManagedAgents),
                        tr(self.language, Text::NoManagedAgentsHint),
                    )
                } else {
                    (
                        tr(self.language, Text::NoAgentsObserved),
                        tr(self.language, Text::NoAgentsHint),
                    )
                }
            } else {
                (
                    tr(
                        self.language,
                        if self.managed {
                            Text::ConnectingNative
                        } else {
                            Text::Connecting
                        },
                    ),
                    "",
                )
            }
        } else if !self.search.is_empty() {
            (
                tr(self.language, Text::NoRows),
                tr(self.language, Text::NoRowsHint),
            )
        } else if self.filter == Filter::Attention
            && self.connected
            && !self.rows.iter().any(Observation::is_unavailable)
        {
            (
                tr(self.language, Text::NoPending),
                tr(self.language, Text::ChooseAllHint),
            )
        } else {
            (
                tr(self.language, Text::NoRows),
                tr(self.language, Text::ChooseAllHint),
            )
        }
    }

    fn draw_divider(&mut self, frame: &mut Frame, divider: Rect, palette: Palette) {
        let dragging = self.drag.is_some();
        let hovered = self.hover == Some(HitTarget::Divider);
        frame.render_widget(
            Block::default().style(Style::default().bg(if dragging {
                palette.accent
            } else {
                palette.canvas
            })),
            divider,
        );
        if !dragging {
            let grip = divider.height.min(3);
            let top = divider.y + divider.height.saturating_sub(grip) / 2;
            frame.render_widget(
                Paragraph::new(vec![Line::from("┃"); grip as usize]).style(
                    Style::default()
                        .fg(if hovered {
                            palette.accent
                        } else {
                            palette.muted
                        })
                        .add_modifier(Modifier::BOLD),
                ),
                Rect::new(divider.x, top, 1, grip),
            );
        }
        self.push_hit(divider, HitTarget::Divider, 1);
    }

    fn draw_inspector(&mut self, frame: &mut Frame, area: Rect, palette: Palette) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let block = self.region_block(Text::Inspector, self.focus == Focus::Inspector, palette);
        let inner = inset(area, 1);
        frame.render_widget(block, area);
        if inner.width == 0 || inner.height == 0 {
            self.inspector_page = 1;
            return;
        }
        let page = inner.height.max(1) as usize;
        let padded = |scroll: bool| inset_x(inner, 1).width.saturating_sub(u16::from(scroll));
        // Wrapping depends on whether a scrollbar takes a column.
        let mut lines = self.inspector_view(padded(false) as usize, palette);
        let has_scroll = lines.len() > page;
        if has_scroll {
            lines = self.inspector_view(padded(true) as usize, palette);
        }
        self.inspector_lines = lines.len();
        self.inspector_page = page;
        self.clamp_selection_and_scroll();
        let content = Rect::new(
            inner.x + 1.min(inner.width),
            inner.y,
            padded(has_scroll),
            inner.height,
        );
        let visible = lines
            .into_iter()
            .skip(self.inspector_scroll)
            .take(self.inspector_page)
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(visible), content);
        self.push_hit(inner, HitTarget::InspectorBody, 1);
        if has_scroll {
            self.draw_scrollbar(
                frame,
                Rect::new(inner.right() - 1, inner.y, 1, inner.height),
                self.inspector_scroll,
                self.inspector_page,
                self.inspector_lines,
                HitTarget::InspectorTrack,
                HitTarget::InspectorThumb,
                palette,
                2,
            );
        }
    }

    fn freshness(&self, value: &str) -> (String, Tone) {
        if !self.connected {
            return (tr(self.language, Text::LastKnown).to_owned(), Tone::Warn);
        }
        let tone = match value {
            "fresh" => Tone::Good,
            "syncing" | "connecting" => Tone::Warn,
            _ => Tone::Bad,
        };
        (evidence_label(self.language, value).to_owned(), tone)
    }

    fn inspector_entries(&self) -> Vec<Entry> {
        let language = self.language;
        let Some(row) = self.selected() else {
            return vec![Entry::Placeholder(
                tr(language, Text::NoSelection).to_owned(),
            )];
        };
        let mut entries = Vec::new();
        if !self.connected {
            entries.push(Entry::Banner(tr(language, Text::StaleEvidence).to_owned()));
            entries.push(Entry::Gap);
        }
        entries.push(Entry::Title(row.id.clone()));
        let activity_tone = if !self.connected || row.is_unavailable() {
            Tone::Muted
        } else {
            match row.activity.as_str() {
                "working" => Tone::Good,
                "retrying" => Tone::Warn,
                "idle" => Tone::Normal,
                _ => Tone::Muted,
            }
        };
        entries.push(Entry::Field(
            tr(language, Text::Activity),
            activity_label(language, &row.activity).to_owned(),
            activity_tone,
        ));
        if let Some(last) = &row.last_activity {
            entries.push(Entry::Field(
                tr(language, Text::LastActivity),
                activity_label(language, last).to_owned(),
                Tone::Normal,
            ));
        }
        let kind = attention_label(language, &row.attention_kind);
        entries.push(if row.pending {
            let tone = if !self.connected || row.acknowledged {
                Tone::Normal
            } else {
                Tone::Warn
            };
            Entry::Field(
                tr(language, Text::Attention),
                format!(
                    "{kind} / {}",
                    tr(
                        language,
                        if row.acknowledged {
                            Text::Seen
                        } else {
                            Text::Unseen
                        }
                    )
                ),
                tone,
            )
        } else {
            Entry::Field(tr(language, Text::Attention), kind.to_owned(), Tone::Muted)
        });
        entries.push(Entry::Field(
            tr(language, Text::Approval),
            row.permission_count.to_string(),
            Tone::Normal,
        ));
        entries.push(Entry::Field(
            tr(language, Text::Question),
            row.question_count.to_string(),
            Tone::Normal,
        ));
        if row.pending_count > 0 {
            entries.push(Entry::Field(
                tr(language, Text::NeedsInput),
                row.pending_count.to_string(),
                Tone::Warn,
            ));
        }

        entries.push(Entry::Gap);
        entries.push(Entry::Heading(tr(language, Text::NativeSection).to_owned()));
        entries.push(Entry::Field(
            tr(language, Text::Source),
            row.source_id.clone(),
            Tone::Normal,
        ));
        entries.push(Entry::Field(
            tr(language, Text::Session),
            row.session_id.clone(),
            Tone::Normal,
        ));
        entries.push(Entry::Field(
            tr(language, Text::Pane),
            row.pane_id.clone().unwrap_or_else(|| "--".to_owned()),
            Tone::Normal,
        ));
        let (native, native_tone) = self.freshness(&row.native_freshness);
        entries.push(Entry::Field(
            tr(language, Text::Freshness),
            native,
            native_tone,
        ));
        entries.push(Entry::Field(
            tr(language, Text::Observed),
            utc_timestamp(row.observed_at_ms),
            Tone::Normal,
        ));
        let (binding, binding_tone) = match row.binding.as_str() {
            "explicit_unverified" => (Text::ConfiguredUnverified, Tone::Warn),
            "invalidated" => (Text::BindingInvalidated, Tone::Bad),
            "managed_reported" => (Text::BindingReported, Tone::Normal),
            "managed_requested" => (Text::BindingRequested, Tone::Warn),
            "managed_conflict" => (Text::BindingConflict, Tone::Bad),
            "managed_none" => (Text::BindingNone, Tone::Muted),
            _ => (Text::StateUnavailable, Tone::Bad),
        };
        entries.push(Entry::Field(
            tr(language, Text::Binding),
            tr(language, binding).to_owned(),
            binding_tone,
        ));
        entries.push(Entry::Field(
            tr(language, Text::FrontendVerified),
            tr(
                language,
                if row.frontend_verified {
                    Text::Verified
                } else {
                    Text::NotVerified
                },
            )
            .to_owned(),
            Tone::Normal,
        ));

        entries.push(Entry::Gap);
        entries.push(Entry::Heading(tr(language, Text::CoreSection).to_owned()));
        let process = process_label(language, &row.core_process);
        entries.push(if self.connected {
            Entry::Field(
                tr(language, Text::Process),
                process.to_owned(),
                match row.core_process.as_str() {
                    "running" => Tone::Good,
                    "exited" | "removed" => Tone::Bad,
                    _ => Tone::Muted,
                },
            )
        } else {
            Entry::Field(
                tr(language, Text::Process),
                format!("{process} ({})", tr(language, Text::LastKnown)),
                Tone::Warn,
            )
        });
        let (core, core_tone) = self.freshness(&row.core_freshness);
        entries.push(Entry::Field(tr(language, Text::Freshness), core, core_tone));
        entries.push(Entry::Field(
            tr(language, Text::PtyGeneration),
            row.pty_generation
                .clone()
                .unwrap_or_else(|| "--".to_owned()),
            Tone::Normal,
        ));
        let note = if !self.connected {
            Some(Text::NavigationUnavailable)
        } else if !self.native_available {
            Some(Text::NativeUnavailable)
        } else if !self.action_enabled(Action::GoToPane) {
            Some(Text::NavigationUnavailable)
        } else {
            None
        };
        if let Some(note) = note {
            entries.push(Entry::Gap);
            entries.push(Entry::Note(tr(language, note).to_owned()));
        }
        if let Some(details) = self.details.get(&row.id) {
            entries.push(Entry::Gap);
            entries.push(Entry::Heading(
                tr(language, Text::RawDetails).to_uppercase(),
            ));
            let mut raw = Vec::new();
            flatten_details(details, "", 0, &mut raw);
            entries.extend(raw.into_iter().map(Entry::Raw));
        }
        entries
    }

    /// Wraps inspector entries to `width`. Values wrap under their own
    /// column, so a narrow panel never drops the end of an evidence label.
    fn inspector_view(&self, width: usize, palette: Palette) -> Vec<Line<'static>> {
        let entries = self.inspector_entries();
        let label_width = entries
            .iter()
            .filter_map(|entry| match entry {
                Entry::Field(label, _, _) => Some(cell_width(label)),
                _ => None,
            })
            .max()
            .unwrap_or(0)
            + 2;
        // Stack labels above values when two columns would squeeze values.
        let stacked = width < label_width + 20;
        let muted = Style::default().fg(palette.muted);
        let mut lines = Vec::new();
        for entry in entries {
            match entry {
                Entry::Gap => lines.push(Line::default()),
                Entry::Banner(text) => lines.extend(wrap(&text, width).into_iter().map(|line| {
                    Line::styled(
                        line,
                        Style::default()
                            .fg(palette.warning)
                            .add_modifier(Modifier::BOLD),
                    )
                })),
                Entry::Title(text) => lines.extend(wrap(&text, width).into_iter().map(|line| {
                    Line::styled(
                        line,
                        Style::default()
                            .fg(palette.text)
                            .add_modifier(Modifier::BOLD),
                    )
                })),
                Entry::Heading(text) => lines.extend(
                    wrap(&text, width)
                        .into_iter()
                        .map(|line| Line::styled(line, muted.add_modifier(Modifier::BOLD))),
                ),
                Entry::Note(text) => lines.extend(
                    wrap(&text, width)
                        .into_iter()
                        .map(|line| Line::styled(line, Style::default().fg(palette.warning))),
                ),
                Entry::Placeholder(text) => lines.extend(
                    wrap(&text, width)
                        .into_iter()
                        .map(|line| Line::styled(line, muted)),
                ),
                Entry::Raw(text) => lines.extend(
                    wrap(&text, width)
                        .into_iter()
                        .map(|line| Line::styled(line, Style::default().fg(palette.text))),
                ),
                Entry::Field(label, value, tone) => {
                    let value_style = Style::default().fg(palette.tone(tone));
                    if stacked {
                        lines.push(Line::styled(clip(label, width), muted));
                        lines.extend(wrap(&value, width.saturating_sub(2)).into_iter().map(
                            |line| {
                                Line::from(vec![Span::raw("  "), Span::styled(line, value_style)])
                            },
                        ));
                    } else {
                        let values = wrap(&value, width.saturating_sub(label_width));
                        for (index, line) in values.into_iter().enumerate() {
                            let lead = if index == 0 {
                                Span::styled(pad(label, label_width), muted)
                            } else {
                                Span::raw(" ".repeat(label_width))
                            };
                            lines.push(Line::from(vec![lead, Span::styled(line, value_style)]));
                        }
                    }
                }
            }
        }
        lines
    }

    fn action_levels(&self, actions: &[Action], expanded: bool) -> Vec<Vec<Control>> {
        let waiting = self.ack_in_flight.is_some();
        let control = |action: Action, compact: bool, key_only: bool| {
            let is_waiting = action == Action::MarkSeen && waiting;
            let is_expanded = expanded && action == Action::Expand;
            let label = if key_only {
                ""
            } else if compact {
                compact_action_label(self.language, action, is_waiting, is_expanded)
            } else {
                action_label(self.language, action, is_waiting, is_expanded)
            };
            Control::new(action_shortcut(action), label)
        };
        [(false, false), (true, false), (false, true)]
            .into_iter()
            .map(|(compact, key_only)| {
                actions
                    .iter()
                    .map(|action| control(*action, compact, key_only))
                    .collect()
            })
            .collect()
    }

    fn draw_actions(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        palette: Palette,
        dense: bool,
        expanded: bool,
        gutter: u16,
    ) {
        frame.render_widget(
            Block::default().style(Style::default().bg(palette.panel)),
            area,
        );
        let actions = self.visible_actions();
        let levels = self.action_levels(&actions, expanded);
        self.action_controls = 0;
        self.action_focus = self.action_focus.min(actions.len().saturating_sub(1));
        if self.compact && self.last_area.width < 60 && !dense {
            let width = area.width.saturating_sub(1) / 2;
            let level = levels
                .iter()
                .position(|controls| controls.iter().all(|control| control.width() <= width))
                .unwrap_or(levels.len() - 1);
            for (index, action) in actions.into_iter().enumerate() {
                let column = (index % 2) as u16;
                let row = (index / 2) as u16;
                if row >= area.height {
                    break;
                }
                let rect = Rect::new(area.x + column * (width + 1), area.y + row, width, 1);
                self.draw_control(
                    frame,
                    rect,
                    &levels[level][index],
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
        let line = inset_x(Rect::new(area.x, area.y, area.width, 1), gutter);
        let level = fit_level(&levels, 1, line.width);
        let mut x = line.x;
        for (index, action) in actions.into_iter().enumerate() {
            let control = &levels[level][index];
            let width = control.width();
            if x + width > line.right() {
                break;
            }
            self.draw_control(
                frame,
                Rect::new(x, line.y, width, 1),
                control,
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

    fn draw_footer(&mut self, frame: &mut Frame, area: Rect, palette: Palette, gutter: u16) {
        if area.height == 0 {
            return;
        }
        frame.render_widget(
            Block::default().style(Style::default().bg(palette.canvas)),
            area,
        );
        let line = inset_x(area, gutter);
        let width = line.width as usize;
        let color = if self.toast.is_some() {
            palette.accent
        } else if self.connected {
            palette.muted
        } else {
            palette.warning
        };
        // A message may take every footer row except the server status
        // line's; a hint shortens to one row.
        let rows = area.height - u16::from(area.height > 1 && self.endpoint_line.is_some());
        let lines = match self.toast.as_deref().or(self.connection_message.as_deref()) {
            Some(message) => wrap_rows(message, width, rows as usize),
            None => {
                let hints = self.current_hint();
                let first = usize::from(self.compact && self.last_area.width < 60);
                let hint = hints[first..]
                    .iter()
                    .find(|hint| cell_width(hint) <= width)
                    .unwrap_or(&hints[2]);
                vec![ellipsize(hint, width)]
            }
        };
        for (index, text) in lines.iter().enumerate() {
            frame.render_widget(
                Paragraph::new(text.as_str()).style(Style::default().fg(color)),
                Rect::new(line.x, line.y + index as u16, line.width, 1),
            );
        }
        if area.height as usize <= lines.len() {
            return;
        }
        let y = area.bottom() - 1;
        if let Some((note, offline)) = &self.endpoint_line {
            frame.render_widget(
                Paragraph::new(ellipsize(note, area.width as usize)).style(
                    Style::default().bg(palette.panel).fg(if *offline {
                        palette.warning
                    } else {
                        palette.muted
                    }),
                ),
                Rect::new(area.x, y, area.width, 1),
            );
        } else if self.compact {
            frame.render_widget(
                Paragraph::new(ellipsize(
                    tr(self.language, Text::SharedSidebarShort),
                    width,
                ))
                .style(Style::default().fg(palette.muted)),
                Rect::new(line.x, y, line.width, 1),
            );
        }
    }

    /// The hint for the hovered target or focus: full, short, then minimal.
    /// "  Inbox N" in the header while the inbox is on, warning-coloured
    /// when something is unseen.
    fn inbox_span(&self, palette: Palette) -> Span<'static> {
        match self.inbox_label() {
            Some(label) => {
                let color = if self.inbox.view.unseen > 0 || self.inbox.mismatch {
                    palette.warning
                } else {
                    palette.muted
                };
                Span::styled(format!("  {label}"), Style::default().fg(color))
            }
            None => Span::raw(""),
        }
    }

    fn current_hint(&self) -> [&'static str; 3] {
        if self.managed && self.inbox.open {
            let hint = self.inbox_hint();
            return [hint, hint, tr(self.language, Text::HelpHintShort)];
        }
        let hint = |full, short| {
            [
                tr(self.language, full),
                tr(self.language, short),
                tr(self.language, Text::HelpHintShort),
            ]
        };
        match &self.hover {
            Some(HitTarget::Action(Action::MarkSeen)) if !self.action_enabled(Action::MarkSeen) => {
                return hint(Text::AckUnavailable, Text::AckUnavailableShort);
            }
            Some(HitTarget::Action(Action::GoToPane)) if !self.action_enabled(Action::GoToPane) => {
                return hint(
                    Text::NavigationUnavailable,
                    Text::NavigationUnavailableShort,
                );
            }
            Some(HitTarget::Divider) => return hint(Text::DividerHint, Text::DividerHintShort),
            Some(HitTarget::Search | HitTarget::ClearSearch) => {
                return hint(Text::SearchHint, Text::SearchHintShort);
            }
            Some(HitTarget::Row { .. }) => return hint(Text::ListHint, Text::ListHintShort),
            _ => {}
        }
        match self.focus {
            Focus::Filters => hint(Text::FilterHint, Text::FilterHintShort),
            Focus::Search => hint(Text::SearchHint, Text::SearchHintShort),
            Focus::List => hint(Text::ListHint, Text::ListHintShort),
            Focus::Inspector => hint(Text::InspectorHint, Text::InspectorHintShort),
            Focus::Actions => hint(Text::ActionHint, Text::ActionHintShort),
            Focus::Header => hint(Text::ClipboardHint, Text::ClipboardHintShort),
        }
    }

    fn draw_overlay(&mut self, frame: &mut Frame, area: Rect, palette: Palette) {
        match self.overlay {
            Some(Overlay::Help) => self.draw_help(frame, area, palette),
            Some(Overlay::Context) => self.draw_context(frame, area, palette),
            None => {}
        }
    }

    fn help_text(&self, width: usize, palette: Palette) -> Vec<Line<'static>> {
        let items = help_items(self.language, self.managed);
        let key_width = items
            .iter()
            .filter_map(|item| match item {
                HelpItem::Key(key, _) => Some(cell_width(key)),
                _ => None,
            })
            .max()
            .unwrap_or(0)
            + 2;
        let stacked = width < key_width + 20;
        let key_style = Style::default()
            .fg(palette.accent)
            .add_modifier(Modifier::BOLD);
        let mut lines = Vec::new();
        for item in items {
            match item {
                HelpItem::Gap => lines.push(Line::default()),
                HelpItem::Group(name) => lines.extend(wrap(name, width).into_iter().map(|line| {
                    Line::styled(
                        line,
                        Style::default()
                            .fg(palette.muted)
                            .add_modifier(Modifier::BOLD),
                    )
                })),
                HelpItem::Note(text) => lines.extend(
                    wrap(text, width)
                        .into_iter()
                        .map(|line| Line::styled(line, Style::default().fg(palette.text))),
                ),
                HelpItem::Key(key, text) if stacked => {
                    lines.extend(
                        wrap(key, width)
                            .into_iter()
                            .map(|line| Line::styled(line, key_style)),
                    );
                    lines.extend(wrap(text, width.saturating_sub(2)).into_iter().map(|line| {
                        Line::from(vec![
                            Span::raw("  "),
                            Span::styled(line, Style::default().fg(palette.text)),
                        ])
                    }));
                }
                HelpItem::Key(key, text) => {
                    for (index, line) in wrap(text, width.saturating_sub(key_width))
                        .into_iter()
                        .enumerate()
                    {
                        let lead = if index == 0 {
                            Span::styled(pad(key, key_width), key_style)
                        } else {
                            Span::raw(" ".repeat(key_width))
                        };
                        lines.push(Line::from(vec![
                            lead,
                            Span::styled(line, Style::default().fg(palette.text)),
                        ]));
                    }
                }
            }
        }
        lines
    }

    fn draw_help(&mut self, frame: &mut Frame, area: Rect, palette: Palette) {
        let width = area.width.saturating_sub(4).clamp(24, 86).min(area.width);
        let text_width = |scroll: bool| width.saturating_sub(4 + u16::from(scroll)) as usize;
        let max_height = area.height.saturating_sub(2).max(7).min(area.height);
        let mut lines = self.help_text(text_width(false), palette);
        let page = max_height.saturating_sub(2).max(1) as usize;
        let has_scroll = lines.len() > page;
        if has_scroll {
            lines = self.help_text(text_width(true), palette);
        }
        let height = (lines.len() as u16 + 2).clamp(7, max_height);
        let modal = centered(area, width, height);
        frame.render_widget(Clear, modal);
        // The close control sits in the top border so every row holds help;
        // the title and the control label shorten to share that border.
        let full_close = Control::new("Esc", tr(self.language, Text::Close));
        let border = modal.width.saturating_sub(4) as usize;
        let (title, close) = [
            (Text::HelpTitle, full_close.clone()),
            (Text::Help, full_close),
            (Text::Help, Control::new("Esc", "")),
        ]
        .into_iter()
        .map(|(title, close)| (format!(" {} ", tr(self.language, title)), close))
        .find(|(title, close)| cell_width(title) + 1 + close.width() as usize <= border)
        .unwrap_or_else(|| (String::new(), Control::new("Esc", "")));
        let block = Block::default()
            .borders(Borders::ALL)
            .title(title)
            .title_style(
                Style::default()
                    .fg(palette.accent)
                    .add_modifier(Modifier::BOLD),
            )
            .border_style(Style::default().fg(palette.accent))
            .style(Style::default().bg(palette.panel).fg(palette.text));
        let inner = inset(modal, 1);
        frame.render_widget(block, modal);
        self.push_hit(modal, HitTarget::OverlaySurface, 100);
        let close_width = close.width().min(modal.width.saturating_sub(2));
        let close_rect = Rect::new(
            modal.right().saturating_sub(close_width + 2),
            modal.y,
            close_width,
            1,
        );
        self.draw_control(
            frame,
            close_rect,
            &close,
            HitTarget::DismissOverlay,
            palette,
            false,
        );
        if let Some(hit) = self.hits.last_mut() {
            hit.z = 102;
        }
        self.help_lines = lines.len();
        self.help_page = inner.height.max(1) as usize;
        self.clamp_selection_and_scroll();
        let text_area = Rect::new(
            inner.x + 1.min(inner.width),
            inner.y,
            text_width(has_scroll) as u16,
            inner.height,
        );
        let visible = lines
            .into_iter()
            .skip(self.help_scroll)
            .take(self.help_page)
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(visible), text_area);
        self.push_hit(inner, HitTarget::HelpBody, 101);
        if has_scroll {
            self.draw_scrollbar(
                frame,
                Rect::new(inner.right() - 1, inner.y, 1, inner.height),
                self.help_scroll,
                self.help_page,
                self.help_lines,
                HitTarget::HelpTrack,
                HitTarget::HelpThumb,
                palette,
                102,
            );
        }
    }

    fn draw_context(&mut self, frame: &mut Frame, area: Rect, palette: Palette) {
        let actions = self.context_actions();
        let waiting = self.ack_in_flight.is_some();
        let items = actions
            .iter()
            .map(|action| {
                let mut control = Control::new(
                    action_shortcut(*action),
                    action_label(
                        self.language,
                        *action,
                        *action == Action::MarkSeen && waiting,
                        false,
                    ),
                );
                control.key_right = true;
                control
            })
            .collect::<Vec<_>>();
        let title = format!(" {} ", tr(self.language, Text::ContextMenu));
        let width = items
            .iter()
            .map(|control| control.width() + 3)
            .max()
            .unwrap_or(0)
            .max(cell_width(&title) as u16 + 4)
            .min(area.width.saturating_sub(2));
        let height = (items.len() as u16 + 2).min(area.height);
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
                .title(title)
                .title_style(
                    Style::default()
                        .fg(palette.accent)
                        .add_modifier(Modifier::BOLD),
                )
                .border_style(Style::default().fg(palette.accent))
                .style(Style::default().bg(palette.panel)),
            menu,
        );
        self.push_hit(menu, HitTarget::OverlaySurface, 100);
        let inner = inset(menu, 1);
        self.context_selection = self.context_selection.min(actions.len().saturating_sub(1));
        // A menu taller than the screen scrolls to keep the selection shown.
        let first = (self.context_selection + 1).saturating_sub(inner.height as usize);
        let hidden_below = actions.len() > first + inner.height as usize;
        for (shown, y) in [
            (first > 0, menu.y),
            (hidden_below, menu.bottom().saturating_sub(1)),
        ] {
            if shown && menu.width > 3 {
                frame.render_widget(
                    Paragraph::new(if y == menu.y { "↑" } else { "↓" })
                        .style(Style::default().fg(palette.accent).bg(palette.panel)),
                    Rect::new(menu.right() - 2, y, 1, 1),
                );
            }
        }
        for (row, (index, (action, control))) in actions
            .into_iter()
            .zip(items)
            .enumerate()
            .skip(first)
            .take(inner.height as usize)
            .enumerate()
        {
            let rect = Rect::new(inner.x, inner.y + row as u16, inner.width, 1);
            self.draw_control(
                frame,
                rect,
                &control,
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
        control: &Control,
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
        let (fg, key_fg, bg) = if !enabled {
            (palette.muted, palette.muted, palette.panel)
        } else if pressed {
            (palette.accent_text, palette.accent_text, palette.warning)
        } else if selected || hovered {
            (palette.accent_text, palette.accent_text, palette.accent)
        } else if control.key_right {
            (palette.text, palette.muted, palette.panel)
        } else {
            (
                if control.quiet {
                    palette.muted
                } else {
                    palette.text
                },
                palette.accent,
                palette.raised,
            )
        };
        let base = Style::default().fg(fg).bg(bg).add_modifier(if selected {
            Modifier::BOLD
        } else {
            Modifier::empty()
        });
        let key = Span::styled(
            control.key.clone(),
            base.fg(key_fg).add_modifier(if enabled {
                Modifier::BOLD
            } else {
                Modifier::empty()
            }),
        );
        let spans = if control.key_right {
            let inner = (rect.width as usize).saturating_sub(2);
            let key_width = cell_width(&control.key);
            let label = clip(&control.label, inner.saturating_sub(key_width + 1));
            let gap = inner.saturating_sub(cell_width(&label) + key_width);
            vec![
                Span::raw(" "),
                Span::styled(label, base),
                Span::raw(" ".repeat(gap)),
                key,
                Span::raw(" "),
            ]
        } else {
            let gap = if control.key.is_empty() || control.label.is_empty() {
                ""
            } else {
                " "
            };
            vec![
                Span::raw(" "),
                key,
                Span::raw(gap),
                Span::styled(control.label.clone(), base),
                Span::raw(" "),
            ]
        };
        frame.render_widget(
            Paragraph::new(clip_line(spans, rect.width as usize)).style(base),
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

fn header_target(focus_index: usize) -> HitTarget {
    match focus_index {
        0 => HitTarget::Close,
        1 => HitTarget::Help,
        2 => HitTarget::Theme,
        _ => HitTarget::Language,
    }
}

fn spans_width(spans: &[Span]) -> usize {
    spans.iter().map(|span| cell_width(&span.content)).sum()
}

/// Clips styled spans to `width` terminal cells.
pub(super) fn clip_line(spans: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let mut used = 0;
    let mut output = Vec::new();
    for span in spans {
        if used >= width {
            break;
        }
        let text = clip(&span.content, width - used);
        used += cell_width(&text);
        output.push(Span::styled(text, span.style));
    }
    Line::from(output)
}

/// Clips to `width` cells and pads with spaces to exactly that width.
pub(super) fn pad(text: &str, width: usize) -> String {
    let mut output = clip(text, width);
    let used = cell_width(&output);
    output.push_str(&" ".repeat(width.saturating_sub(used)));
    output
}

/// Word-wraps by rendered cell width; words wider than a line break by grapheme.
pub(super) fn wrap(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }
    let text = sanitize(text);
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut used = 0;
    for word in text.split(' ') {
        let word_width = cell_width(word);
        let space = usize::from(!line.is_empty());
        if used + space + word_width <= width {
            if space == 1 {
                line.push(' ');
            }
            line.push_str(word);
            used += space + word_width;
            continue;
        }
        if !line.is_empty() {
            lines.push(std::mem::take(&mut line));
            used = 0;
        }
        if word_width <= width {
            line.push_str(word);
            used = word_width;
            continue;
        }
        for grapheme in word.graphemes(true) {
            let next = cell_width(grapheme);
            if used + next > width && !line.is_empty() {
                lines.push(std::mem::take(&mut line));
                used = 0;
            }
            line.push_str(grapheme);
            used += next;
        }
    }
    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }
    lines
}

pub(super) fn inset_x(rect: Rect, amount: u16) -> Rect {
    Rect::new(
        rect.x.saturating_add(amount),
        rect.y,
        rect.width.saturating_sub(amount.saturating_mul(2)),
        rect.height,
    )
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

pub(super) fn action_shortcut(action: Action) -> &'static str {
    match action {
        Action::MarkSeen => "a",
        Action::GoToPane => "g",
        Action::Details => "Enter",
        Action::CopyId => "c",
        Action::Expand => "z",
        Action::Retry => "r",
        Action::NewAgent => "n",
        Action::RenameAgent => "r",
        Action::ResumeAgent => "s",
        Action::PrepareDraft => "d",
        Action::SendPrompt => "p",
        Action::InterruptAgent => "x",
        Action::ReadScreen => "v",
        Action::CloseAgent => "X",
        Action::Answer => "y",
        Action::Inbox => "i",
        Action::NextUnseen => "u",
    }
}

pub(super) fn utc_timestamp(milliseconds: u64) -> String {
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

pub(super) fn clip(text: &str, width: usize) -> String {
    fit(text, width).0
}

/// Clips to `width` cells and marks a cut with an ellipsis.
pub(super) fn ellipsize(text: &str, width: usize) -> String {
    let (output, cut) = fit(text, width);
    if !cut || width == 0 {
        return output;
    }
    let (mut output, _) = fit(text, width - 1);
    output.push('…');
    output
}

/// Word-wraps into at most `rows` lines; a cut ends with an ellipsis.
pub(super) fn wrap_rows(text: &str, width: usize, rows: usize) -> Vec<String> {
    let mut lines = wrap(text, width);
    if lines.len() > rows {
        let rest = lines.split_off(rows).join(" ");
        if let Some(last) = lines.pop() {
            lines.push(ellipsize(&format!("{last} {rest}"), width));
        }
    }
    lines
}

/// The longest prefix of `text` within `width` cells, and whether it was cut.
fn fit(text: &str, width: usize) -> (String, bool) {
    if width == 0 {
        return (String::new(), !text.is_empty());
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
            return (output, true);
        }
        output.push_str(safe);
        used += next;
    }
    (output, false)
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

pub(super) fn cell_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

pub(super) fn inset(rect: Rect, amount: u16) -> Rect {
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
    fn the_inbox_view_replaces_the_agent_controls() {
        use crate::managed::inbox::{InboxItem, InboxView};
        let width = 110;
        let height = 24;
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new(false, Language::English, Theme::Dark);
        app.managed = true;
        app.apply_snapshot(snapshot());
        app.set_connection(true, None);
        app.set_native_available(true);
        app.inbox.apply(InboxView {
            enabled: true,
            unseen: 1,
            events: vec![InboxItem {
                id: 7,
                seq: 40,
                source: "hook:claude".into(),
                pane: "%3".into(),
                run: "r".into(),
                revision: None,
                kind: "approval_requested".into(),
                summary: Some(serde_json::json!({"tool": "Bash", "command": "cargo test"})),
                observed_ms: 0,
                read: false,
                resolution: None,
            }],
        });
        app.inbox
            .set_names(vec![("%3".into(), "r".into(), "builder".into())]);
        app.toggle_inbox();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let text: String = (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    + "\n"
            })
            .collect();
        for wanted in [
            "Inbox 1",
            "Approval",
            "builder",
            "hook:claude",
            "Bash: cargo test",
            "00:00:00",
        ] {
            assert!(text.contains(wanted), "{wanted} missing in\n{text}");
        }
        assert!(
            app.hits
                .iter()
                .any(|hit| hit.target == HitTarget::InboxRow(7))
        );
        assert!(
            !app.hits
                .iter()
                .any(|hit| matches!(hit.target, HitTarget::Filter(_) | HitTarget::Action(_)))
        );
        // The same inbox again changes nothing to draw.
        assert!(!app.dirty);
        assert!(!app.inbox.apply(app.inbox.view.clone()));
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
            "Attention",
            "Working",
            "Unavailable",
            "Mark seen",
            "Go to pane",
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
        let entries = app.inspector_entries();
        assert!(entries.contains(&Entry::Field("Attention", "None".into(), Tone::Muted)));
        let text = format!("{entries:?}");
        assert!(!text.contains("Unseen"));
        assert!(!text.contains("Seen"));
    }

    #[test]
    fn empty_attention_view_claims_nothing_without_current_evidence() {
        let mut value = snapshot();
        value["observations"][0]["attention"]["pending"] = json!(false);
        let mut app = App::new(false, Language::English, Theme::Dark);
        app.apply_snapshot(value.clone());
        app.filter = Filter::Attention;
        app.set_connection(true, None);
        assert_eq!(app.empty_list_text().0, "No pending requests.");
        app.set_connection(false, None);
        assert_eq!(app.empty_list_text().0, "No agents match this view.");
        value["observations"][0]["native"]["freshness"] = json!("stale");
        app.apply_snapshot(value);
        app.set_connection(true, None);
        assert_eq!(app.empty_list_text().0, "No agents match this view.");
        app.filter = Filter::All;
        app.clamp_selection_and_scroll();
        let entries = app.inspector_entries();
        assert!(entries.contains(&Entry::Field("Activity", "Working".into(), Tone::Muted)));
    }

    #[test]
    fn tiny_view_prefers_minimum_size_over_current_size() {
        let backend = TestBackend::new(24, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new(false, Language::English, Theme::Dark);
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let text = (0..5)
            .flat_map(|y| (0..24).map(move |x| (x, y)))
            .map(|cell| buffer.cell(cell).unwrap().symbol().to_owned())
            .collect::<String>();
        assert!(text.contains("Minimum"), "{text}");
        let narrow = draw_at(4, 9, false, Language::English);
        assert!(narrow.hits.iter().any(|hit| hit.target == HitTarget::Close));
    }

    fn screen(width: u16, height: u16, compact: bool) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new(compact, Language::English, Theme::Dark);
        app.apply_snapshot(snapshot());
        app.set_connection(true, None);
        app.set_native_available(true);
        app.details_open = true;
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer.cell((x, y)).map(|cell| cell.symbol()).unwrap_or(" "))
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn narrow_inspector_wraps_instead_of_dropping_evidence() {
        let text = screen(34, 40, true);
        let joined = text
            .lines()
            .map(|line| line.trim_matches(|c: char| c == '│' || c.is_whitespace()))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(joined.contains("foreground agent not verified"), "{text}");
    }

    #[test]
    fn wrap_respects_cell_width_and_breaks_long_words() {
        assert_eq!(wrap("alpha beta gamma", 10), vec!["alpha beta", "gamma"]);
        assert_eq!(wrap("한글한글한글", 4), vec!["한글", "한글", "한글"]);
        assert_eq!(wrap("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        assert!(wrap("anything", 0).is_empty());
    }

    #[test]
    fn list_columns_align_identity_across_activity_labels() {
        let mut value = snapshot();
        let mut idle = value["observations"][0].clone();
        idle["id"] = json!("zz_idle");
        idle["native"]["activity"] = json!("idle");
        idle["attention"]["pending"] = json!(false);
        value["observations"].as_array_mut().unwrap().push(idle);
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new(false, Language::English, Theme::Dark);
        app.apply_snapshot(value);
        app.set_connection(true, None);
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let column = |needle: &str| {
            (0..24).find_map(|y| {
                let line = (0..80)
                    .map(|x| buffer.cell((x, y)).unwrap().symbol().to_owned())
                    .collect::<String>();
                line.find(needle).map(|byte| line[..byte].chars().count())
            })
        };
        let working = column("Working  ").unwrap();
        let idle = column("Idle").unwrap();
        assert_eq!(working, idle, "activity column must align");
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
