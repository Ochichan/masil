//! `rmux-agent settings`: a terminal settings screen for the rmux UI layer.
//! It runs in an rmux popup, saves choices to settings.conf and applies them
//! to the running server at once.

pub(crate) mod catalog;
pub(crate) mod server;
pub(crate) mod store;

use super::model::{Language, Theme};
use super::terminal::Session;
use super::view::{Palette, Tone, cell_width, clip, clip_line, inset, inset_x, pad, wrap};
use catalog::{Group, Kind, SETTINGS, Scope, Setting, UI_KEY};
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use server::{Outcome, Server};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Clone, Debug, Eq, PartialEq)]
enum Hit {
    Group(usize),
    Row(usize),
    Value(usize, String),
    Step(usize, i64),
    Close,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Focus {
    Groups,
    Rows,
}

/// A row of the selected group: a catalog setting or a layer action.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Row {
    Setting(&'static Setting),
    Layer,
    Reset,
}

struct App {
    server: Server,
    path: Option<PathBuf>,
    language: Language,
    theme: Theme,
    group: usize,
    row: usize,
    focus: Focus,
    scroll: usize,
    layer_on: bool,
    values: HashMap<&'static str, String>,
    /// Effective values that differ from the saved choice, per key.
    configured: HashMap<&'static str, String>,
    receipt: Option<(Tone, String)>,
    hits: Vec<(Rect, Hit)>,
    hover: Option<Hit>,
    pressed: Option<Hit>,
    quit: bool,
}

fn text(language: Language, en: &'static str, ko: &'static str) -> &'static str {
    match language {
        Language::English => en,
        Language::Korean => ko,
    }
}

impl App {
    fn new(server: Server) -> Self {
        let path = store::settings_path();
        let mut app = Self {
            server,
            path,
            language: Language::English,
            theme: Theme::Dark,
            group: 0,
            row: 0,
            focus: Focus::Rows,
            scroll: 0,
            layer_on: true,
            values: HashMap::new(),
            configured: HashMap::new(),
            receipt: None,
            hits: Vec::new(),
            hover: None,
            pressed: None,
            quit: false,
        };
        app.reload();
        if let Some(path) = &app.path
            && let Err(error) = store::load(path)
        {
            app.receipt = Some((Tone::Warn, error));
        }
        app
    }

    /// Reads choices and their effective values from the server.
    fn reload(&mut self) {
        self.layer_on = self.server.layer_on();
        self.configured.clear();
        for setting in SETTINGS {
            let chosen = self
                .server
                .global(setting.key, Scope::Session)
                .filter(|value| setting.accepts(value))
                .unwrap_or_else(|| setting.default.to_owned());
            let effective = setting
                .readback
                .filter(|_| self.layer_on)
                .and_then(|(option, scope)| self.server.global(option, scope))
                .filter(|value| setting.accepts(value));
            match effective {
                Some(effective) if effective != chosen => {
                    self.configured.insert(setting.key, effective.clone());
                    self.values.insert(setting.key, effective);
                }
                _ => {
                    self.values.insert(setting.key, chosen);
                }
            }
        }
        self.language = match self.values.get("@rmux-lang").map(String::as_str) {
            Some("ko") => Language::Korean,
            _ => Language::English,
        };
        self.theme = match self.values.get("@rmux-theme").map(String::as_str) {
            Some("light") => Theme::Light,
            Some("terminal" | "tmux") => Theme::Terminal,
            _ => Theme::Dark,
        };
    }

    fn rows(&self) -> Vec<Row> {
        let group = Group::ALL[self.group];
        if group == Group::Layer {
            return vec![Row::Layer, Row::Reset];
        }
        SETTINGS
            .iter()
            .filter(|setting| setting.group == group)
            .map(Row::Setting)
            .collect()
    }

    fn receipt(&mut self, outcome: Outcome, what: String) {
        let language = self.language;
        self.receipt = Some(match outcome {
            Outcome::Applied => (
                Tone::Good,
                format!("{} {what}", text(language, "Applied:", "적용됨:")),
            ),
            Outcome::Hidden(scope) => (
                Tone::Warn,
                match (language, scope) {
                    (Language::English, scope) => {
                        format!("Saved {what}, but this {scope} has its own value that hides it.")
                    }
                    (Language::Korean, "window") => {
                        format!("{what} 저장함. 이 창의 별도 값이 가립니다.")
                    }
                    (Language::Korean, _) => {
                        format!("{what} 저장함. 이 세션의 별도 값이 가립니다.")
                    }
                },
            ),
            Outcome::SessionOnly(error) => (
                Tone::Warn,
                match language {
                    Language::English => {
                        format!("Applied {what} until rmux stops; not saved: {error}")
                    }
                    Language::Korean => {
                        format!("{what}: rmux를 끌 때까지 적용됩니다. 저장 실패: {error}")
                    }
                },
            ),
            Outcome::SavedWhileOff => (
                Tone::Warn,
                match language {
                    Language::English => {
                        format!("Saved {what}. The rmux UI is off, so the screen is unchanged.")
                    }
                    Language::Korean => {
                        format!("{what} 저장함. rmux UI가 꺼져 있어 화면은 그대로입니다.")
                    }
                },
            ),
            Outcome::Failed(error) => (
                Tone::Bad,
                format!(
                    "{} {error}",
                    text(language, "Not applied:", "적용하지 못함:")
                ),
            ),
        });
    }

    fn choose(&mut self, setting: &'static Setting, value: String) {
        let outcome = server::apply(&self.server, self.path.as_deref(), setting, &value);
        // Describe the change in the language now in effect.
        self.reload();
        let label = match setting.kind {
            Kind::Choices(choices) => choices
                .iter()
                .find(|choice| choice.value == value)
                .map_or(value.clone(), |choice| {
                    choice.label(self.language).to_owned()
                }),
            Kind::Number { .. } => value.clone(),
        };
        let what = format!("{} → {label}", setting.label(self.language));
        self.receipt(outcome, what);
    }

    fn set_layer(&mut self, on: bool) {
        let outcome = server::set_layer(&self.server, self.path.as_deref(), on);
        self.reload();
        let what = if on {
            text(self.language, "rmux UI on", "rmux UI 켜짐").to_owned()
        } else {
            text(
                self.language,
                "rmux UI off; tmux defaults and your tmux.conf are back",
                "rmux UI 꺼짐. tmux 기본값과 tmux.conf를 다시 적용했습니다",
            )
            .to_owned()
        };
        self.receipt(outcome, what);
    }

    fn reset(&mut self) {
        let outcome = server::reset(&self.server, self.path.as_deref());
        self.reload();
        let what = text(
            self.language,
            "every rmux setting is back to its default",
            "모든 rmux 설정을 기본값으로 되돌렸습니다",
        )
        .to_owned();
        self.receipt(outcome, what);
    }

    /// Moves a row's value by one choice or step and applies it.
    fn shift(&mut self, row: Row, delta: i64) {
        match row {
            Row::Setting(setting) => {
                let current = self
                    .values
                    .get(setting.key)
                    .cloned()
                    .unwrap_or_else(|| setting.default.to_owned());
                let next = match setting.kind {
                    Kind::Choices(choices) => {
                        let index = choices
                            .iter()
                            .position(|choice| choice.value == current)
                            .unwrap_or(0) as i64;
                        let next = (index + delta).clamp(0, choices.len() as i64 - 1) as usize;
                        choices[next].value.to_owned()
                    }
                    Kind::Number { min, max, step } => {
                        let value = current.parse::<i64>().unwrap_or(i64::from(min));
                        (value + delta * i64::from(step))
                            .clamp(i64::from(min), i64::from(max))
                            .to_string()
                    }
                };
                if next != current {
                    self.choose(setting, next);
                }
            }
            Row::Layer => {
                if (delta < 0) != self.layer_on {
                    self.set_layer(delta < 0);
                }
            }
            Row::Reset => {}
        }
    }

    fn activate(&mut self, hit: Hit) {
        let rows = self.rows();
        match hit {
            Hit::Group(index) => {
                if index != self.group {
                    self.group = index;
                    self.row = 0;
                    self.scroll = 0;
                }
                self.focus = Focus::Groups;
            }
            Hit::Row(index) => {
                self.row = index.min(rows.len().saturating_sub(1));
                self.focus = Focus::Rows;
                if rows.get(index) == Some(&Row::Reset) {
                    self.reset();
                }
            }
            Hit::Value(index, value) => {
                self.row = index;
                self.focus = Focus::Rows;
                match rows.get(index) {
                    Some(Row::Setting(setting)) => {
                        if self.values.get(setting.key) != Some(&value) {
                            self.choose(setting, value);
                        }
                    }
                    Some(Row::Layer) => {
                        let on = value == "on";
                        if on != self.layer_on {
                            self.set_layer(on);
                        }
                    }
                    _ => {}
                }
            }
            Hit::Step(index, delta) => {
                self.row = index;
                self.focus = Focus::Rows;
                if let Some(row) = rows.get(index).copied() {
                    self.shift(row, delta);
                }
            }
            Hit::Close => self.quit = true,
        }
    }

    fn key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        let rows = self.rows();
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => self.quit = true,
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = match self.focus {
                    Focus::Groups => Focus::Rows,
                    Focus::Rows => Focus::Groups,
                }
            }
            KeyCode::Up | KeyCode::Char('k') => match self.focus {
                Focus::Groups => self.select_group(self.group.saturating_sub(1)),
                Focus::Rows => self.row = self.row.saturating_sub(1),
            },
            KeyCode::Down | KeyCode::Char('j') => match self.focus {
                Focus::Groups => self.select_group(self.group + 1),
                Focus::Rows => self.row = (self.row + 1).min(rows.len().saturating_sub(1)),
            },
            KeyCode::Left | KeyCode::Char('h') if self.focus == Focus::Rows => {
                if let Some(row) = rows.get(self.row).copied() {
                    self.shift(row, -1);
                }
            }
            KeyCode::Right | KeyCode::Char('l') if self.focus == Focus::Rows => {
                if let Some(row) = rows.get(self.row).copied() {
                    self.shift(row, 1);
                }
            }
            KeyCode::Enter | KeyCode::Char(' ') => match self.focus {
                Focus::Groups => self.focus = Focus::Rows,
                Focus::Rows => match rows.get(self.row) {
                    Some(Row::Reset) => self.reset(),
                    Some(Row::Layer) => self.set_layer(!self.layer_on),
                    _ => {}
                },
            },
            _ => {}
        }
    }

    fn select_group(&mut self, index: usize) {
        let index = index.min(Group::ALL.len() - 1);
        if index != self.group {
            self.group = index;
            self.row = 0;
            self.scroll = 0;
        }
    }

    fn hit_at(&self, column: u16, row: u16) -> Option<Hit> {
        self.hits
            .iter()
            .rev()
            .find(|(rect, _)| {
                column >= rect.x && column < rect.right() && row >= rect.y && row < rect.bottom()
            })
            .map(|(_, hit)| hit.clone())
    }

    fn mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::Moved | MouseEventKind::Drag(_) => {
                self.hover = self.hit_at(mouse.column, mouse.row);
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.pressed = self.hit_at(mouse.column, mouse.row);
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let released = self.hit_at(mouse.column, mouse.row);
                if let Some(hit) = self.pressed.take()
                    && released.as_ref() == Some(&hit)
                {
                    self.activate(hit);
                }
            }
            MouseEventKind::ScrollDown => self.scroll = self.scroll.saturating_add(2),
            MouseEventKind::ScrollUp => self.scroll = self.scroll.saturating_sub(2),
            _ => {}
        }
    }

    fn push_hit(&mut self, rect: Rect, clip: Rect, hit: Hit) {
        let x = rect.x.max(clip.x);
        let y = rect.y.max(clip.y);
        let right = rect.right().min(clip.right());
        let bottom = rect.bottom().min(clip.bottom());
        if right > x && bottom > y {
            self.hits
                .push((Rect::new(x, y, right - x, bottom - y), hit));
        }
    }

    /// Draws a chip; returns its width.
    #[allow(clippy::too_many_arguments)]
    fn chip(
        &mut self,
        frame: &mut Frame,
        x: u16,
        y: u16,
        label: &str,
        hit: Hit,
        selected: bool,
        clip_to: Rect,
        palette: Palette,
    ) -> u16 {
        let text = format!(" {label} ");
        let width = cell_width(&text) as u16;
        let hovered = self.hover.as_ref() == Some(&hit);
        let pressed = self.pressed.as_ref() == Some(&hit);
        let style = if pressed {
            Style::default().fg(palette.accent_text).bg(palette.warning)
        } else if selected {
            Style::default()
                .fg(palette.accent_text)
                .bg(palette.accent)
                .add_modifier(Modifier::BOLD)
        } else if hovered {
            Style::default().fg(palette.accent).bg(palette.raised)
        } else {
            Style::default().fg(palette.text).bg(palette.raised)
        };
        if y >= clip_to.y && y < clip_to.bottom() && x < clip_to.right() {
            let visible = width.min(clip_to.right() - x);
            frame.render_widget(
                Paragraph::new(clip(&text, visible as usize)).style(style),
                Rect::new(x, y, visible, 1),
            );
        }
        self.push_hit(Rect::new(x, y, width, 1), clip_to, hit);
        width
    }

    fn draw(&mut self, frame: &mut Frame) {
        self.hits.clear();
        let area = frame.area();
        let palette = Palette::for_theme(self.theme);
        frame.render_widget(
            Block::default().style(Style::default().bg(palette.canvas)),
            area,
        );
        if area.width < 30 || area.height < 10 {
            let message = text(
                self.language,
                "Terminal too small for settings. q closes.",
                "설정을 표시하기에 터미널이 작습니다. q로 닫습니다.",
            );
            frame.render_widget(
                Paragraph::new(wrap(message, area.width as usize).join("\n"))
                    .style(Style::default().fg(palette.warning)),
                area,
            );
            self.push_hit(area, area, Hit::Close);
            return;
        }
        let language = self.language;

        // Header.
        let header = Rect::new(area.x, area.y, area.width, 1);
        frame.render_widget(
            Block::default().style(Style::default().bg(palette.panel)),
            header,
        );
        let title = vec![
            Span::styled(
                " rmux ",
                Style::default()
                    .fg(palette.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                text(language, " Settings", " 설정"),
                Style::default().fg(palette.text),
            ),
        ];
        frame.render_widget(
            Paragraph::new(clip_line(title, area.width as usize)),
            header,
        );
        let close = format!("q {}", text(language, "Close", "닫기"));
        let close_x = area.right().saturating_sub(cell_width(&close) as u16 + 3);
        self.chip(
            frame,
            close_x,
            header.y,
            &close,
            Hit::Close,
            false,
            header,
            palette,
        );

        // Footer: receipt, then keyboard hints.
        let footer = Rect::new(area.x, area.bottom() - 2, area.width, 2);
        let line = inset_x(footer, 1);
        if let Some((tone, message)) = &self.receipt {
            frame.render_widget(
                Paragraph::new(clip(message, line.width as usize))
                    .style(Style::default().fg(palette.tone(*tone))),
                Rect::new(line.x, line.y, line.width, 1),
            );
        }
        let full = text(
            language,
            "Tab sections  ↑↓ move  ←→ change  Enter select  q close",
            "Tab 항목  ↑↓ 이동  ←→ 값 변경  Enter 선택  q 닫기",
        );
        let hint = if cell_width(full) <= line.width as usize {
            full
        } else {
            text(
                language,
                "Tab  ↑↓  ←→  Enter  q close",
                "Tab  ↑↓  ←→  Enter  q 닫기",
            )
        };
        frame.render_widget(
            Paragraph::new(clip(hint, line.width as usize))
                .style(Style::default().fg(palette.muted)),
            Rect::new(line.x, line.y + 1, line.width, 1),
        );

        // Body.
        let body = Rect::new(
            area.x,
            area.y + 2,
            area.width,
            area.height.saturating_sub(4),
        );
        let panel = if area.width >= 72 {
            let groups = Rect::new(body.x, body.y, 24, body.height);
            self.draw_groups(frame, groups, palette);
            Rect::new(
                groups.right() + 1,
                body.y,
                body.width - groups.width - 1,
                body.height,
            )
        } else {
            // Narrow: the groups become chips that wrap onto more rows.
            let mut x = body.x + 1;
            let mut y = body.y;
            for (index, group) in Group::ALL.iter().enumerate() {
                let width = cell_width(group.label(language)) as u16 + 2;
                if x > body.x + 1 && x + width > body.right() {
                    x = body.x + 1;
                    y += 1;
                }
                self.chip(
                    frame,
                    x,
                    y,
                    group.label(language),
                    Hit::Group(index),
                    index == self.group,
                    body,
                    palette,
                );
                x = x.saturating_add(width + 1);
            }
            let used = y - body.y + 2;
            Rect::new(
                body.x,
                body.y + used,
                body.width,
                body.height.saturating_sub(used),
            )
        };
        self.draw_rows(frame, panel, palette);
    }

    fn block(&self, title: &str, focused: bool, palette: Palette) -> Block<'static> {
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" {title} "))
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

    fn draw_groups(&mut self, frame: &mut Frame, area: Rect, palette: Palette) {
        let focused = self.focus == Focus::Groups;
        frame.render_widget(
            self.block(text(self.language, "Sections", "항목"), focused, palette),
            area,
        );
        let inner = inset(area, 1);
        for (index, group) in Group::ALL.iter().enumerate() {
            let y = inner.y + index as u16;
            if y >= inner.bottom() {
                break;
            }
            let rect = Rect::new(inner.x, y, inner.width, 1);
            let selected = index == self.group;
            let hovered = self.hover == Some(Hit::Group(index));
            let marker = if selected || hovered { "› " } else { "  " };
            let style = if selected {
                Style::default()
                    .fg(palette.text)
                    .bg(palette.raised)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(if hovered {
                    palette.accent
                } else {
                    palette.text
                })
            };
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(
                        marker,
                        Style::default()
                            .fg(if selected {
                                palette.accent
                            } else {
                                palette.muted
                            })
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(group.label(self.language)),
                ]))
                .style(style),
                rect,
            );
            self.push_hit(rect, inner, Hit::Group(index));
        }
    }

    fn draw_rows(&mut self, frame: &mut Frame, area: Rect, palette: Palette) {
        let language = self.language;
        let group = Group::ALL[self.group];
        let focused = self.focus == Focus::Rows;
        frame.render_widget(self.block(group.label(language), focused, palette), area);
        let inner = inset_x(inset(area, 1), 1);
        if inner.width == 0 || inner.height == 0 {
            return;
        }
        let rows = self.rows();
        self.row = self.row.min(rows.len().saturating_sub(1));

        // Each row: label, controls, wrapped help and notes, then a gap.
        let mut blocks = Vec::new();
        for row in &rows {
            let mut notes: Vec<(Tone, String)> = Vec::new();
            let help = match row {
                Row::Setting(setting) => {
                    if let Some(effective) = self.configured.get(setting.key) {
                        notes.push((
                            Tone::Warn,
                            match language {
                                Language::English => format!(
                                    "Your tmux configuration sets {effective}; choosing here overrides it until rmux restarts."
                                ),
                                Language::Korean => format!(
                                    "tmux 설정 파일이 {effective}로 정합니다. 여기서 고르면 rmux를 다시 시작할 때까지 덮어씁니다."
                                ),
                            },
                        ));
                    }
                    setting.help(language)
                }
                Row::Layer => text(
                    language,
                    "Off restores tmux defaults, removes rmux buttons and re-reads your tmux.conf.",
                    "끄면 tmux 기본값으로 돌아가고 rmux 버튼을 없애며 tmux.conf를 다시 읽습니다.",
                ),
                Row::Reset => text(
                    language,
                    "Removes saved choices; the rmux defaults apply at once.",
                    "저장한 선택을 지우고 rmux 기본값을 바로 적용합니다.",
                ),
            };
            let mut lines = wrap(help, inner.width as usize)
                .into_iter()
                .map(|line| (Tone::Muted, line))
                .collect::<Vec<_>>();
            for (tone, note) in notes {
                lines.extend(
                    wrap(&note, inner.width as usize)
                        .into_iter()
                        .map(|line| (tone, line)),
                );
            }
            blocks.push(lines);
        }
        let heights = blocks
            .iter()
            .map(|lines| 2 + lines.len() as u16 + 1)
            .collect::<Vec<_>>();
        let total: u16 = heights.iter().sum();
        let top: u16 = heights[..self.row].iter().sum();
        let max_scroll = total.saturating_sub(inner.height) as usize;
        if focused {
            // Keep the focused row visible.
            let bottom = top + heights[self.row];
            if (top as usize) < self.scroll {
                self.scroll = top as usize;
            } else if bottom as usize > self.scroll + inner.height as usize {
                self.scroll = (bottom - inner.height.min(bottom)) as usize;
            }
        }
        self.scroll = self.scroll.min(max_scroll);

        let mut y = inner.y as i32 - self.scroll as i32;
        for (index, row) in rows.iter().enumerate() {
            let height = heights[index] as i32;
            if y + height <= inner.y as i32 || y >= inner.bottom() as i32 {
                y += height;
                continue;
            }
            let selected = focused && index == self.row;
            let label = match row {
                Row::Setting(setting) => setting.label(language),
                Row::Layer => text(language, "rmux UI layer", "rmux UI 레이어"),
                Row::Reset => text(language, "Reset", "초기화"),
            };
            if y >= inner.y as i32 {
                let marker = if selected { "› " } else { "" };
                frame.render_widget(
                    Paragraph::new(Line::from(vec![
                        Span::styled(
                            marker,
                            Style::default()
                                .fg(palette.accent)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            label,
                            Style::default().fg(palette.text).add_modifier(if selected {
                                Modifier::BOLD
                            } else {
                                Modifier::empty()
                            }),
                        ),
                    ])),
                    Rect::new(inner.x, y as u16, inner.width, 1),
                );
                self.push_hit(
                    Rect::new(inner.x, y as u16, inner.width, 1),
                    inner,
                    Hit::Row(index),
                );
            }
            let controls_y = y + 1;
            if controls_y >= inner.y as i32 && controls_y < inner.bottom() as i32 {
                self.draw_controls(frame, *row, index, inner, controls_y as u16, palette);
            }
            for (offset, (tone, line)) in blocks[index].iter().enumerate() {
                let line_y = y + 2 + offset as i32;
                if line_y >= inner.y as i32 && line_y < inner.bottom() as i32 {
                    frame.render_widget(
                        Paragraph::new(pad(line, inner.width as usize))
                            .style(Style::default().fg(palette.tone(*tone))),
                        Rect::new(inner.x, line_y as u16, inner.width, 1),
                    );
                }
            }
            y += height;
        }
    }

    fn draw_controls(
        &mut self,
        frame: &mut Frame,
        row: Row,
        index: usize,
        inner: Rect,
        y: u16,
        palette: Palette,
    ) {
        let language = self.language;
        let mut x = inner.x;
        match row {
            Row::Setting(setting) => {
                let current = self
                    .values
                    .get(setting.key)
                    .cloned()
                    .unwrap_or_else(|| setting.default.to_owned());
                match setting.kind {
                    Kind::Choices(choices) => {
                        for choice in choices {
                            let width = self.chip(
                                frame,
                                x,
                                y,
                                choice.label(language),
                                Hit::Value(index, choice.value.to_owned()),
                                choice.value == current,
                                inner,
                                palette,
                            );
                            x = x.saturating_add(width + 1);
                        }
                    }
                    Kind::Number { .. } => {
                        let width = self.chip(
                            frame,
                            x,
                            y,
                            "-",
                            Hit::Step(index, -1),
                            false,
                            inner,
                            palette,
                        );
                        x = x.saturating_add(width + 1);
                        let value = format!("{current:>3}");
                        if x < inner.right() {
                            frame.render_widget(
                                Paragraph::new(clip(&value, (inner.right() - x) as usize)).style(
                                    Style::default()
                                        .fg(palette.text)
                                        .add_modifier(Modifier::BOLD),
                                ),
                                Rect::new(x, y, cell_width(&value) as u16, 1),
                            );
                        }
                        x = x.saturating_add(cell_width(&value) as u16 + 1);
                        self.chip(frame, x, y, "+", Hit::Step(index, 1), false, inner, palette);
                    }
                }
            }
            Row::Layer => {
                for (value, en, ko) in [("on", "On", "켜기"), ("off", "Off", "끄기")] {
                    let width = self.chip(
                        frame,
                        x,
                        y,
                        text(language, en, ko),
                        Hit::Value(index, value.to_owned()),
                        (value == "on") == self.layer_on,
                        inner,
                        palette,
                    );
                    x = x.saturating_add(width + 1);
                }
            }
            Row::Reset => {
                self.chip(
                    frame,
                    x,
                    y,
                    text(language, "Reset all rmux settings", "모든 rmux 설정 초기화"),
                    Hit::Row(index),
                    false,
                    inner,
                    palette,
                );
            }
        }
    }
}

fn usage() -> String {
    "usage: rmux-agent settings [--socket RMUX_SOCKET] [--set KEY VALUE]... [--layer on|off] [--reset] [--get]".into()
}

/// Runs the settings screen, or applies choices without a screen when any
/// of --set, --layer, --reset or --get is given.
pub(crate) fn run(args: &[String]) -> Result<i32, String> {
    let mut socket = None;
    let mut sets = Vec::new();
    let mut layer = None;
    let mut reset = false;
    let mut get = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--socket" => {
                socket = Some(args.get(index + 1).ok_or_else(usage)?.clone());
                index += 2;
            }
            "--set" => {
                let key = args.get(index + 1).ok_or_else(usage)?;
                let value = args.get(index + 2).ok_or_else(usage)?;
                sets.push((key.clone(), value.clone()));
                index += 3;
            }
            "--layer" => {
                layer = Some(match args.get(index + 1).map(String::as_str) {
                    Some("on") => true,
                    Some("off") => false,
                    _ => return Err(usage()),
                });
                index += 2;
            }
            "--reset" => {
                reset = true;
                index += 1;
            }
            "--get" => {
                get = true;
                index += 1;
            }
            _ => return Err(usage()),
        }
    }
    let server = Server::locate(socket.as_deref())?;
    let path = store::settings_path();
    if reset || layer.is_some() || !sets.is_empty() || get {
        let mut failed = false;
        let mut report = |name: String, outcome: Outcome| {
            failed |= matches!(outcome, Outcome::Failed(_));
            println!("{name}: {outcome:?}");
        };
        if reset {
            report("reset".into(), server::reset(&server, path.as_deref()));
        }
        if let Some(on) = layer {
            report(
                UI_KEY.into(),
                server::set_layer(&server, path.as_deref(), on),
            );
        }
        for (key, value) in sets {
            let setting = catalog::setting(&key).ok_or_else(|| format!("unknown setting {key}"))?;
            report(
                format!("{key}={value}"),
                server::apply(&server, path.as_deref(), setting, &value),
            );
        }
        if get {
            for setting in SETTINGS {
                let value = server
                    .global(setting.key, Scope::Session)
                    .unwrap_or_else(|| setting.default.to_owned());
                println!("{} {value}", setting.key);
            }
            println!("{UI_KEY} {}", if server.layer_on() { "on" } else { "off" });
        }
        return Ok(if failed { 1 } else { 0 });
    }

    let mut app = App::new(server);
    let mut session = Session::open()?;
    while !app.quit {
        session
            .terminal()
            .draw(|frame| app.draw(frame))
            .map_err(|error| error.to_string())?;
        match event::read().map_err(|error| error.to_string())? {
            Event::Key(key) => app.key(key),
            Event::Mouse(mouse) => app.mouse(mouse),
            Event::Resize(..) => {
                app.hover = None;
                app.pressed = None;
            }
            _ => {}
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_group_has_rows_and_every_setting_a_group() {
        for group in Group::ALL {
            assert!(
                group == Group::Layer || SETTINGS.iter().any(|setting| setting.group == group),
                "{group:?} is empty"
            );
            assert!(!group.label(Language::Korean).is_empty());
        }
        for setting in SETTINGS {
            assert!(setting.key.starts_with("@rmux-"));
            assert!(!setting.help(Language::Korean).is_empty());
        }
    }
}
