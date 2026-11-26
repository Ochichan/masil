//! `masil-agent settings`: a terminal settings screen for the masil UI layer.
//! It runs in a masil popup, saves choices to settings.conf and applies them
//! to the running server at once.

pub(crate) mod catalog;
pub(crate) mod server;
pub(crate) mod store;

use super::model::{Language, Theme};
use super::terminal::Session;
use super::view::{
    Palette, Tone, cell_width, clip, clip_line, ellipsize, inset, inset_x, pad, wrap, wrap_rows,
};
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
use std::time::{Duration, Instant};

/// How long after an Enter or click on Reset the next one does not count
/// yet. It is longer than the delay before a held key starts to repeat, and
/// every repeat starts it again, so a held key or a double click cannot
/// confirm.
const RESET_PAUSE: Duration = Duration::from_secs(1);
/// How long the Reset question waits for its answer.
const RESET_EXPIRY: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Eq, PartialEq)]
enum Hit {
    Group(usize),
    Row(usize),
    Value(usize, String),
    Step(usize, i64),
    /// The Reset row's button.
    Reset(usize),
    PluginTheme(usize, String),
    PluginOpenPanel(usize, String, String, String),
    PluginToggle(usize, String, bool),
    PluginReload(usize),
    ScrollTrack,
    ScrollThumb,
    Close,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Focus {
    Groups,
    Rows,
}

/// A row of the selected group: a catalog setting, plugin control, or layer action.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Row {
    Setting(&'static Setting),
    Layer,
    Reset,
    PluginTheme,
    PluginPanels,
    PluginItem(usize),
    PluginReload,
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
    scroll_max: usize,
    scroll_page: usize,
    scroll_track: Rect,
    dragging_scroll: bool,
    layer_on: bool,
    values: HashMap<&'static str, String>,
    /// Effective values that differ from the saved choice, per key.
    configured: HashMap<&'static str, String>,
    plugin_catalog: Option<crate::plugin::manifest::PluginCatalog>,
    /// When Reset was last asked for: an Enter or click on it between
    /// `RESET_PAUSE` and `RESET_EXPIRY` later runs it. Any other key, click,
    /// wheel step, resize or focus change takes the question back.
    reset_armed: Option<Instant>,
    receipt: Option<(Tone, String)>,
    hits: Vec<(Rect, Hit)>,
    hover: Option<Hit>,
    pressed: Option<Hit>,
    dirty: bool,
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
            scroll_max: 0,
            scroll_page: 1,
            scroll_track: Rect::default(),
            dragging_scroll: false,
            layer_on: true,
            values: HashMap::new(),
            configured: HashMap::new(),
            plugin_catalog: None,
            reset_armed: None,
            receipt: None,
            hits: Vec::new(),
            hover: None,
            pressed: None,
            dirty: true,
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

    /// Reads choices and their effective values from the server, all in
    /// one client call.
    fn reload(&mut self) {
        let mut options = vec![(UI_KEY, Scope::Session)];
        for setting in SETTINGS {
            options.push((setting.key, Scope::Session));
            options.extend(setting.readback);
        }
        let mut values = self.server.globals(&options).into_iter();
        let mut next = || values.next().flatten();
        self.layer_on = next().as_deref() != Some("off");
        self.configured.clear();
        for setting in SETTINGS {
            let chosen = next();
            // The option a choice drives counts while the layer is on, and
            // always when the choice is that option itself.
            let effective = setting
                .readback
                .and_then(|(option, _)| next().filter(|_| self.layer_on || option == setting.key));
            let (value, configured) = shown(setting, chosen, effective);
            if let Some(configured) = configured {
                self.configured.insert(setting.key, configured);
            }
            self.values.insert(setting.key, value);
        }
        self.language = match self.values.get("@masil-lang").map(String::as_str) {
            Some("ko") => Language::Korean,
            _ => Language::English,
        };
        self.theme = match self.values.get("@masil-theme").map(String::as_str) {
            Some("light") => Theme::Light,
            Some("terminal" | "tmux") => Theme::Terminal,
            _ => Theme::Dark,
        };
        self.plugin_catalog = crate::plugin::manifest::load_catalog().ok();
        if let Some(t) = self
            .plugin_catalog
            .as_ref()
            .and_then(|c| c.active_resolved_theme())
        {
            super::view::set_plugin_palette(Some(&t));
            self.theme = if t.appearance == "light" {
                Theme::Light
            } else {
                Theme::Dark
            };
        } else {
            super::view::set_plugin_palette(None);
        }
    }

    fn rows(&self) -> Vec<Row> {
        let group = Group::ALL[self.group];
        if group == Group::Layer {
            return vec![Row::Layer, Row::Reset];
        }
        if group == Group::Plugins {
            let mut out = Vec::new();
            let has_themes = self
                .plugin_catalog
                .as_ref()
                .is_some_and(|c| !c.all_themes().is_empty() || c.active_resolved_theme().is_some());
            if has_themes {
                out.push(Row::PluginTheme);
            }
            let has_panels = self.plugin_catalog.as_ref().is_some_and(|c| {
                c.active_plugins()
                    .any(|p| p.manifest.as_ref().is_some_and(|m| !m.panels.is_empty()))
            });
            if has_panels {
                out.push(Row::PluginPanels);
            }
            if let Some(cat) = &self.plugin_catalog {
                for idx in 0..cat.plugins.len() {
                    out.push(Row::PluginItem(idx));
                }
            }
            out.push(Row::PluginReload);
            return out;
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
                        format!("Applied {what} until masil stops; not saved: {error}")
                    }
                    Language::Korean => {
                        format!("{what}: masil을 끌 때까지 적용됩니다. 저장 실패: {error}")
                    }
                },
            ),
            Outcome::SavedWhileOff => (
                Tone::Warn,
                match language {
                    Language::English => {
                        format!("Saved {what}. The masil UI is off, so the screen is unchanged.")
                    }
                    Language::Korean => {
                        format!("{what} 저장함. masil UI가 꺼져 있어 화면은 그대로입니다.")
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
        if setting.key == "@masil-theme" {
            let next_plugin_theme = match value.as_str() {
                "tmux" => None,
                other => Some(other),
            };
            let _ = crate::plugin::set_active_theme(next_plugin_theme);
        }
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

    fn choose_plugin_theme(&mut self, theme_id: &str) {
        let arg = (!theme_id.is_empty()).then_some(theme_id);
        match crate::plugin::set_active_theme(arg) {
            Ok(_) => {
                self.reload();
                let label = if theme_id.is_empty() {
                    text(self.language, "Default", "기본값").to_owned()
                } else {
                    self.plugin_catalog
                        .as_ref()
                        .and_then(|c| c.all_themes().into_iter().find(|t| t.id == theme_id))
                        .map(|t| t.name)
                        .unwrap_or_else(|| theme_id.to_owned())
                };
                self.receipt = Some((
                    Tone::Good,
                    format!(
                        "{} {} → {label}",
                        text(self.language, "Applied:", "적용됨:"),
                        text(self.language, "Plugin theme", "플러그인 테마")
                    ),
                ));
            }
            Err(err) => {
                self.receipt = Some((Tone::Bad, err));
            }
        }
    }

    fn toggle_plugin_enabled(&mut self, plugin_id: &str, enabled: bool) {
        let res = crate::plugin::manifest::PluginsConfig::load().and_then(|mut cfg| {
            cfg.set_plugin_enabled(plugin_id, enabled);
            cfg.save()?;
            if let Ok(cat) = crate::plugin::manifest::load_catalog() {
                crate::plugin::sync_theme_to_masil_servers(&cat);
            }
            Ok(())
        });
        match res {
            Ok(()) => {
                self.reload();
                let state = if enabled {
                    text(self.language, "On", "켜짐")
                } else {
                    text(self.language, "Off", "꺼짐")
                };
                self.receipt = Some((
                    Tone::Good,
                    format!(
                        "{} {plugin_id} → {state}",
                        text(self.language, "Applied:", "적용됨:")
                    ),
                ));
            }
            Err(err) => self.receipt = Some((Tone::Bad, err)),
        }
    }

    fn open_plugin_panel(&mut self, plugin_id: &str, panel_id: &str, title: &str) {
        let Ok(exe) = std::env::current_exe() else {
            self.receipt = Some((Tone::Bad, "cannot locate masil-agent".into()));
            return;
        };
        let q = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
        let sock = self.server.socket_path().to_string_lossy().into_owned();
        let shell_cmd = format!(
            "{} plugin open {} {} --socket {}",
            q(&exe.to_string_lossy()),
            q(plugin_id),
            q(panel_id),
            q(&sock)
        );
        match self.server.run(&[
            "new-pane", "-x", "52%", "-y", "58%", "-T", title, &shell_cmd,
        ]) {
            Ok(_) => self.quit = true,
            Err(err) => self.receipt = Some((Tone::Bad, err)),
        }
    }

    fn reload_plugins(&mut self) {
        let res = crate::plugin::manifest::PluginsConfig::load().and_then(|mut cfg| {
            cfg.bump_reload();
            cfg.save()
        });
        match res {
            Ok(()) => {
                self.reload();
                if let Some(cat) = &self.plugin_catalog {
                    crate::plugin::sync_theme_to_masil_servers(cat);
                }
                let count = self
                    .plugin_catalog
                    .as_ref()
                    .map(|c| c.active_plugins().count())
                    .unwrap_or(0);
                self.receipt = Some((
                    Tone::Good,
                    match self.language {
                        Language::English => format!("Reloaded plugins ({count} active)"),
                        Language::Korean => format!("플러그인 다시 불러오기 완료 ({count}개 활성)"),
                    },
                ));
            }
            Err(err) => self.receipt = Some((Tone::Bad, err)),
        }
    }

    fn set_layer(&mut self, on: bool) {
        let outcome = server::set_layer(&self.server, self.path.as_deref(), on);
        self.reload();
        let what = if on {
            text(self.language, "masil UI on", "masil UI 켜짐").to_owned()
        } else {
            text(
                self.language,
                "masil UI off; tmux defaults and your tmux.conf are back",
                "masil UI 꺼짐. tmux 기본값과 tmux.conf를 다시 적용했습니다",
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
            "the settings on this screen are back to their defaults",
            "이 화면의 설정을 기본값으로 되돌렸습니다",
        )
        .to_owned();
        self.receipt(outcome, what);
    }

    /// The first Enter or click on Reset asks, and the row says what will
    /// happen; one that answers in time runs it.
    fn confirm_reset(&mut self, asked: Option<Instant>, now: Instant) {
        match reset_press(asked, now) {
            None => self.reset(),
            asked => self.reset_armed = asked,
        }
    }

    /// How long the Reset question still waits, when it is open.
    fn reset_wait(&self, now: Instant) -> Option<Duration> {
        self.reset_armed
            .map(|asked| (asked + RESET_EXPIRY).saturating_duration_since(now))
    }

    /// One terminal event at `now`.
    fn event(&mut self, event: Event, now: Instant) {
        match event {
            Event::Key(key) => {
                if key.kind != KeyEventKind::Release {
                    self.key(key, now);
                    self.dirty = true;
                }
            }
            Event::Mouse(mouse) => self.mouse(mouse, now),
            Event::Resize(..) => {
                self.hover = None;
                self.pressed = None;
                self.reset_armed = None;
                self.dirty = true;
            }
            // A focus change or a paste is no answer to the Reset question.
            _ => {
                if self.reset_armed.take().is_some() {
                    self.dirty = true;
                }
            }
        }
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
            Row::PluginTheme => {
                let mut ids = vec![String::new()];
                if let Some(cat) = &self.plugin_catalog {
                    for t in cat.all_themes() {
                        ids.push(t.id);
                    }
                }
                let current = self
                    .plugin_catalog
                    .as_ref()
                    .and_then(|c| c.active_resolved_theme())
                    .map(|t| t.id)
                    .unwrap_or_default();
                let idx = ids.iter().position(|id| *id == current).unwrap_or(0) as i64;
                let next = (idx + delta).clamp(0, ids.len() as i64 - 1) as usize;
                if ids[next] != current {
                    let chosen = ids[next].clone();
                    self.choose_plugin_theme(&chosen);
                }
            }
            Row::PluginItem(idx) => {
                if let Some(p) = self
                    .plugin_catalog
                    .as_ref()
                    .and_then(|c| c.plugins.get(idx))
                    .cloned()
                    && (delta < 0) != p.enabled
                {
                    self.toggle_plugin_enabled(&p.id, delta < 0);
                }
            }
            Row::Reset | Row::PluginPanels | Row::PluginReload => {}
        }
    }

    fn activate(&mut self, hit: Hit, now: Instant) {
        let rows = self.rows();
        let armed = self.reset_armed.take();
        match hit {
            Hit::Group(index) => {
                if index != self.group {
                    self.group = index;
                    self.row = 0;
                    self.scroll = 0;
                }
                self.focus = Focus::Groups;
            }
            // A row's label only takes the focus.
            Hit::Row(index) => {
                self.row = index.min(rows.len().saturating_sub(1));
                self.focus = Focus::Rows;
            }
            Hit::Reset(index) => {
                self.row = index.min(rows.len().saturating_sub(1));
                self.focus = Focus::Rows;
                if rows.get(index) == Some(&Row::Reset) {
                    self.confirm_reset(armed, now);
                }
            }
            Hit::PluginTheme(index, theme_id) => {
                self.row = index.min(rows.len().saturating_sub(1));
                self.focus = Focus::Rows;
                self.choose_plugin_theme(&theme_id);
            }
            Hit::PluginOpenPanel(index, plugin_id, panel_id, title) => {
                self.row = index.min(rows.len().saturating_sub(1));
                self.focus = Focus::Rows;
                self.open_plugin_panel(&plugin_id, &panel_id, &title);
            }
            Hit::PluginToggle(index, plugin_id, enabled) => {
                self.row = index.min(rows.len().saturating_sub(1));
                self.focus = Focus::Rows;
                self.toggle_plugin_enabled(&plugin_id, enabled);
            }
            Hit::PluginReload(index) => {
                self.row = index.min(rows.len().saturating_sub(1));
                self.focus = Focus::Rows;
                self.reload_plugins();
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
            Hit::ScrollTrack | Hit::ScrollThumb => {}
            Hit::Close => self.quit = true,
        }
    }

    fn key(&mut self, key: KeyEvent, now: Instant) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        let rows = self.rows();
        let armed = self.reset_armed.take();
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
            KeyCode::Home if self.focus == Focus::Rows => {
                self.row = 0;
                self.scroll = 0;
            }
            KeyCode::End if self.focus == Focus::Rows => {
                self.row = rows.len().saturating_sub(1);
                self.scroll = self.scroll_max;
            }
            KeyCode::PageUp if self.focus == Focus::Rows => {
                self.scroll = self.scroll.saturating_sub(self.scroll_page.max(1));
                self.row = self.row.saturating_sub(3);
            }
            KeyCode::PageDown if self.focus == Focus::Rows => {
                self.scroll = (self.scroll + self.scroll_page.max(1)).min(self.scroll_max);
                self.row = (self.row + 3).min(rows.len().saturating_sub(1));
            }
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
                Focus::Rows => match rows.get(self.row).copied() {
                    Some(Row::Reset) => self.confirm_reset(armed, now),
                    Some(Row::Layer) => self.set_layer(!self.layer_on),
                    Some(Row::PluginReload) => self.reload_plugins(),
                    Some(Row::PluginItem(idx)) => {
                        if let Some(p) = self
                            .plugin_catalog
                            .as_ref()
                            .and_then(|c| c.plugins.get(idx))
                            .cloned()
                        {
                            self.toggle_plugin_enabled(&p.id, !p.enabled);
                        }
                    }
                    Some(Row::PluginPanels) => {
                        let first_panel =
                            self.plugin_catalog.as_ref().and_then(|c| {
                                c.active_plugins().find_map(|p| {
                                    p.manifest.as_ref()?.panels.first().map(|pan| {
                                        (p.id.clone(), pan.id.clone(), pan.title.clone())
                                    })
                                })
                            });
                        if let Some((pid, pan_id, title)) = first_panel {
                            self.open_plugin_panel(&pid, &pan_id, &title);
                        }
                    }
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

    fn scroll_to_row(&mut self, row: u16) {
        if self.scroll_track.height <= 1 || self.scroll_max == 0 {
            return;
        }
        let relative = row
            .saturating_sub(self.scroll_track.y)
            .min(self.scroll_track.height - 1) as usize;
        let next = (relative * self.scroll_max) / (self.scroll_track.height as usize - 1);
        if self.scroll != next {
            self.scroll = next;
            self.dirty = true;
        }
    }

    fn mouse(&mut self, mouse: MouseEvent, now: Instant) {
        match mouse.kind {
            MouseEventKind::Moved => {
                self.dragging_scroll = false;
                let next = self.hit_at(mouse.column, mouse.row);
                if self.hover != next {
                    self.hover = next;
                    self.dirty = true;
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if self.dragging_scroll {
                    self.scroll_to_row(mouse.row);
                } else {
                    let next = self.hit_at(mouse.column, mouse.row);
                    if self.hover != next {
                        self.hover = next;
                        self.dirty = true;
                    }
                }
            }
            MouseEventKind::Drag(_) => {
                let next = self.hit_at(mouse.column, mouse.row);
                if self.hover != next {
                    self.hover = next;
                    self.dirty = true;
                }
            }
            // Only a press and release on the Reset button keep its
            // question open; any other press, a release elsewhere and the
            // wheel take it back.
            MouseEventKind::Down(MouseButton::Left) => {
                let next = self.hit_at(mouse.column, mouse.row);
                if matches!(next, Some(Hit::ScrollTrack | Hit::ScrollThumb)) {
                    self.dragging_scroll = true;
                    self.scroll_to_row(mouse.row);
                    self.reset_armed = None;
                    self.dirty = true;
                    return;
                }
                if self.pressed != next {
                    self.pressed = next;
                    self.dirty = true;
                }
                if !matches!(self.pressed, Some(Hit::Reset(_))) && self.reset_armed.take().is_some()
                {
                    self.dirty = true;
                }
            }
            MouseEventKind::Down(_) => {
                self.dragging_scroll = false;
                if self.reset_armed.take().is_some() {
                    self.dirty = true;
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if self.dragging_scroll {
                    self.dragging_scroll = false;
                    self.dirty = true;
                    return;
                }
                let released = self.hit_at(mouse.column, mouse.row);
                match self.pressed.take() {
                    Some(hit) if released.as_ref() == Some(&hit) => {
                        self.activate(hit, now);
                        self.dirty = true;
                    }
                    Some(_) => {
                        self.reset_armed = None;
                        self.dirty = true;
                    }
                    None => {
                        if self.reset_armed.take().is_some() {
                            self.dirty = true;
                        }
                    }
                }
            }
            MouseEventKind::ScrollDown => {
                self.scroll = self.scroll.saturating_add(2);
                self.reset_armed = None;
                self.dirty = true;
            }
            MouseEventKind::ScrollUp => {
                self.scroll = self.scroll.saturating_sub(2);
                self.reset_armed = None;
                self.dirty = true;
            }
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
        let (style, raised) = if pressed {
            (
                Style::default()
                    .fg(palette.accent_text)
                    .bg(palette.warning)
                    .add_modifier(Modifier::BOLD),
                false,
            )
        } else if selected {
            (
                Style::default()
                    .fg(palette.accent_text)
                    .bg(palette.accent)
                    .add_modifier(Modifier::BOLD),
                false,
            )
        } else if hovered {
            (
                Style::default()
                    .fg(palette.accent)
                    .bg(palette.raised_fill()),
                true,
            )
        } else {
            (
                Style::default().fg(palette.text).bg(palette.raised_fill()),
                true,
            )
        };
        if y >= clip_to.y && y < clip_to.bottom() && x < clip_to.right() {
            let visible = width.min(clip_to.right() - x);
            let rect = Rect::new(x, y, visible, 1);
            frame.render_widget(
                Paragraph::new(clip(&text, visible as usize)).style(style),
                rect,
            );
            if raised {
                palette.raise(frame, rect);
            }
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
                Paragraph::new(
                    wrap_rows(message, area.width as usize, area.height as usize).join("\n"),
                )
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
                " masil ",
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
        // A long receipt takes the hint row too.
        let receipt = self
            .receipt
            .as_ref()
            .map(|(tone, message)| (*tone, wrap_rows(message, line.width as usize, 2)))
            .unwrap_or((Tone::Muted, Vec::new()));
        for (index, message) in receipt.1.iter().enumerate() {
            frame.render_widget(
                Paragraph::new(message.as_str())
                    .style(Style::default().fg(palette.tone(receipt.0))),
                Rect::new(line.x, line.y + index as u16, line.width, 1),
            );
        }
        let full = text(
            language,
            "Tab sections  ↑↓/PgUp/PgDn/Home/End move  ←→ change  Enter select  q close",
            "Tab 항목  ↑↓/PgUp/PgDn/Home/End 이동  ←→ 값 변경  Enter 선택  q 닫기",
        );
        let medium = text(
            language,
            "Tab sections  ↑↓ move  ←→ change  Enter select  q close",
            "Tab 항목  ↑↓ 이동  ←→ 값 변경  Enter 선택  q 닫기",
        );
        let hint = if cell_width(full) <= line.width as usize {
            full
        } else if cell_width(medium) <= line.width as usize {
            medium
        } else {
            text(
                language,
                "Tab  ↑↓  ←→  Enter  q close",
                "Tab  ↑↓  ←→  Enter  q 닫기",
            )
        };
        if receipt.1.len() < 2 {
            frame.render_widget(
                Paragraph::new(ellipsize(hint, line.width as usize)).style(palette.muted_style()),
                Rect::new(line.x, line.y + 1, line.width, 1),
            );
        }

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
                palette.muted_style()
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
                    .bg(palette.raised_fill())
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
                        if selected {
                            Style::default().fg(palette.accent)
                        } else {
                            palette.muted_style()
                        }
                        .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(group.label(self.language)),
                ]))
                .style(style),
                rect,
            );
            if selected {
                palette.raise(frame, rect);
            }
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
            self.scroll_max = 0;
            self.scroll_track = Rect::default();
            return;
        }
        let rows = self.rows();
        self.row = self.row.min(rows.len().saturating_sub(1));

        let layout = |width: u16| {
            let mut blocks = Vec::new();
            for row in &rows {
                let mut notes: Vec<(Tone, String)> = Vec::new();
                let help: String = match row {
                    Row::Setting(setting) => {
                        if let Some(effective) = self.configured.get(setting.key) {
                            notes.push((
                                Tone::Warn,
                                match language {
                                    Language::English => format!(
                                        "Your tmux configuration sets {effective}; choosing here overrides it until masil restarts."
                                    ),
                                    Language::Korean => format!(
                                        "tmux 설정 파일이 {effective}로 정합니다. 여기서 고르면 masil을 다시 시작할 때까지 덮어씁니다."
                                    ),
                                },
                            ));
                        }
                        setting.help(language).to_owned()
                    }
                    Row::Layer => text(
                        language,
                        "Off restores tmux defaults, removes masil buttons and re-reads your tmux.conf.",
                        "끄면 tmux 기본값으로 돌아가고 masil 버튼을 없애며 tmux.conf를 다시 읽습니다.",
                    )
                    .to_owned(),
                    Row::Reset => {
                        if self.reset_armed.is_some() {
                            notes.push((
                                Tone::Warn,
                                text(
                                    language,
                                    "After one second, Enter or a click on Reset now runs it. Any other key or click cancels, and so do 5 seconds without one.",
                                    "1초 뒤부터 Enter나 '지금 초기화' 클릭으로 실행합니다. 다른 키나 클릭은 취소하고, 5초 동안 누르지 않아도 취소됩니다.",
                                )
                                .to_owned(),
                            ));
                        }
                        text(
                            language,
                            "Puts every setting on this screen back to its default. Other @masil lines in settings.conf stay.",
                            "이 화면의 설정을 모두 기본값으로 되돌립니다. settings.conf의 다른 @masil 줄은 그대로 둡니다.",
                        )
                        .to_owned()
                    }
                    Row::PluginTheme => text(
                        language,
                        "Applies an installed plugin theme live across the masil bar, borders, TUI and Mobile.",
                        "설치된 플러그인 테마를 masil 사이드바, 경계선, TUI, 모바일 앱에 즉시 적용합니다.",
                    )
                    .to_owned(),
                    Row::PluginPanels => text(
                        language,
                        "Click any panel to open it in a live floating pane inside masil.",
                        "패널을 클릭하면 masil 안에 실시간 플로팅 창으로 바로 엽니다.",
                    )
                    .to_owned(),
                    Row::PluginItem(idx) => {
                        if let Some(p) = self
                            .plugin_catalog
                            .as_ref()
                            .and_then(|c| c.plugins.get(*idx))
                        {
                            if !p.errors.is_empty() {
                                notes.push((Tone::Bad, p.errors.join(" · ")));
                            }
                            let desc = p
                                .manifest
                                .as_ref()
                                .map(|m| m.description.as_str())
                                .filter(|s| !s.is_empty())
                                .unwrap_or(p.id.as_str());
                            let mut parts = Vec::new();
                            if !p.themes.is_empty() {
                                parts.push(format!(
                                    "{} {}",
                                    text(language, "themes", "테마"),
                                    p.themes.len()
                                ));
                            }
                            if let Some(m) = &p.manifest {
                                if !m.panels.is_empty() {
                                    parts.push(format!(
                                        "{} {}",
                                        text(language, "panels", "패널"),
                                        m.panels.len()
                                    ));
                                }
                                if !m.commands.is_empty() {
                                    parts.push(format!(
                                        "{} {}",
                                        text(language, "cmds", "커맨드"),
                                        m.commands.len()
                                    ));
                                }
                                if !m.hooks.is_empty() {
                                    parts.push(format!(
                                        "{} {}",
                                        text(language, "hooks", "훅"),
                                        m.hooks.len()
                                    ));
                                }
                                if !m.presets.is_empty() {
                                    parts.push(format!(
                                        "{} {}",
                                        text(language, "presets", "프리셋"),
                                        m.presets.len()
                                    ));
                                }
                            }
                            if parts.is_empty() {
                                desc.to_owned()
                            } else {
                                format!("{desc} ({})", parts.join(" · "))
                            }
                        } else {
                            String::new()
                        }
                    }
                    Row::PluginReload => text(
                        language,
                        "Reloads manifests, scripts, themes and panels from ~/.config/masil/plugins.",
                        "~/.config/masil/plugins의 매니페스트, 스크립트, 테마, 패널을 다시 불러옵니다.",
                    )
                    .to_owned(),
                };
                let mut lines = wrap(&help, width as usize)
                    .into_iter()
                    .map(|line| (Tone::Muted, line))
                    .collect::<Vec<_>>();
                for (tone, note) in notes {
                    lines.extend(
                        wrap(&note, width as usize)
                            .into_iter()
                            .map(|line| (tone, line)),
                    );
                }
                blocks.push(lines);
            }
            let heights = blocks
                .iter()
                .zip(&rows)
                .map(|(lines, row)| 1 + self.control_lines(*row, width) + lines.len() as u16 + 1)
                .collect::<Vec<_>>();
            let total: u16 = heights.iter().sum();
            (blocks, heights, total)
        };
        let (mut blocks, mut heights, mut total) = layout(inner.width);
        let has_scroll = total > inner.height && inner.width > 4;
        let content_inner = if has_scroll {
            let narrowed = Rect::new(inner.x, inner.y, inner.width - 2, inner.height);
            (blocks, heights, total) = layout(narrowed.width);
            narrowed
        } else {
            inner
        };
        let top: u16 = heights[..self.row].iter().sum();
        let max_scroll = total.saturating_sub(content_inner.height) as usize;
        self.scroll_max = max_scroll;
        self.scroll_page = content_inner.height.max(1) as usize;
        if focused && !self.dragging_scroll {
            // Keep the focused row visible.
            let bottom = top + heights[self.row];
            if (top as usize) < self.scroll {
                self.scroll = top as usize;
            } else if bottom as usize > self.scroll + content_inner.height as usize {
                self.scroll = (bottom - content_inner.height.min(bottom)) as usize;
            }
        }
        self.scroll = self.scroll.min(max_scroll);

        let mut y = content_inner.y as i32 - self.scroll as i32;
        for (index, row) in rows.iter().enumerate() {
            let height = heights[index] as i32;
            if y + height <= content_inner.y as i32 || y >= content_inner.bottom() as i32 {
                y += height;
                continue;
            }
            let selected = focused && index == self.row;
            let label: String = match row {
                Row::Setting(setting) => setting.label(language).to_owned(),
                Row::Layer => text(language, "masil UI layer", "masil UI 레이어").to_owned(),
                Row::Reset => text(language, "Reset", "초기화").to_owned(),
                Row::PluginTheme => text(language, "Plugin theme", "플러그인 테마").to_owned(),
                Row::PluginPanels => {
                    text(language, "Open plugin panel", "플러그인 패널 열기").to_owned()
                }
                Row::PluginItem(idx) => self
                    .plugin_catalog
                    .as_ref()
                    .and_then(|c| c.plugins.get(*idx))
                    .map(|p| {
                        let name = p
                            .manifest
                            .as_ref()
                            .map(|m| m.name.as_str())
                            .filter(|s| !s.is_empty())
                            .unwrap_or(p.id.as_str());
                        let status = if !p.enabled {
                            text(language, "off", "꺼짐")
                        } else if !p.errors.is_empty() {
                            text(language, "error", "오류")
                        } else {
                            text(language, "running", "실행 중")
                        };
                        format!("{name} [{status}]")
                    })
                    .unwrap_or_default(),
                Row::PluginReload => text(language, "Reload", "다시 불러오기").to_owned(),
            };
            if y >= content_inner.y as i32 {
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
                    Rect::new(content_inner.x, y as u16, content_inner.width, 1),
                );
                self.push_hit(
                    Rect::new(content_inner.x, y as u16, content_inner.width, 1),
                    content_inner,
                    Hit::Row(index),
                );
            }
            self.draw_controls(frame, *row, index, content_inner, y + 1, palette);
            let help_y = y + 1 + self.control_lines(*row, content_inner.width) as i32;
            for (offset, (tone, line)) in blocks[index].iter().enumerate() {
                let line_y = help_y + offset as i32;
                if line_y >= content_inner.y as i32 && line_y < content_inner.bottom() as i32 {
                    frame.render_widget(
                        Paragraph::new(pad(line, content_inner.width as usize))
                            .style(palette.tone_style(*tone)),
                        Rect::new(content_inner.x, line_y as u16, content_inner.width, 1),
                    );
                }
            }
            y += height;
        }
        if has_scroll {
            let track = Rect::new(inner.right() - 1, inner.y, 1, inner.height);
            self.scroll_track = track;
            frame.render_widget(
                Block::default().style(Style::default().bg(palette.track)),
                track,
            );
            self.push_hit(track, inner, Hit::ScrollTrack);
            let page = content_inner.height as usize;
            let total_rows = total as usize;
            let thumb_height = ((track.height as usize * page) / total_rows.max(1))
                .max(1)
                .min(track.height as usize);
            let travel = track.height as usize - thumb_height;
            let thumb_y =
                track.y + (travel * self.scroll.min(max_scroll) / max_scroll.max(1)) as u16;
            let thumb = Rect::new(track.x, thumb_y, 1, thumb_height as u16);
            frame.render_widget(
                Block::default().style(Style::default().bg(palette.accent)),
                thumb,
            );
            self.push_hit(thumb, inner, Hit::ScrollThumb);
        } else {
            self.scroll_track = Rect::default();
        }
    }

    /// The chips of a row's choices; a number row has its own controls.
    fn row_chips(&self, row: Row, index: usize) -> Vec<(String, Hit, bool)> {
        let language = self.language;
        match row {
            Row::Setting(setting) => {
                let Kind::Choices(choices) = setting.kind else {
                    return Vec::new();
                };
                let current = self
                    .values
                    .get(setting.key)
                    .map_or(setting.default, String::as_str);
                choices
                    .iter()
                    .map(|choice| {
                        (
                            choice.label(language).to_owned(),
                            Hit::Value(index, choice.value.to_owned()),
                            choice.value == current,
                        )
                    })
                    .collect()
            }
            Row::Layer => [("on", "On", "켜기"), ("off", "Off", "끄기")]
                .into_iter()
                .map(|(value, en, ko)| {
                    (
                        text(language, en, ko).to_owned(),
                        Hit::Value(index, value.to_owned()),
                        (value == "on") == self.layer_on,
                    )
                })
                .collect(),
            Row::Reset => vec![(
                if self.reset_armed.is_some() {
                    text(language, "Reset now", "지금 초기화").to_owned()
                } else {
                    text(
                        language,
                        "Reset the settings on this screen",
                        "이 화면의 설정 초기화",
                    )
                    .to_owned()
                },
                Hit::Reset(index),
                false,
            )],
            Row::PluginTheme => {
                let active_id = self
                    .plugin_catalog
                    .as_ref()
                    .and_then(|c| c.active_resolved_theme())
                    .map(|t| t.id);
                let mut chips = vec![(
                    text(language, "Default", "기본값").to_owned(),
                    Hit::PluginTheme(index, String::new()),
                    active_id.is_none(),
                )];
                if let Some(cat) = &self.plugin_catalog {
                    for t in cat.all_themes() {
                        let sel = active_id.as_deref() == Some(t.id.as_str());
                        chips.push((t.name, Hit::PluginTheme(index, t.id), sel));
                    }
                }
                chips
            }
            Row::PluginPanels => {
                let mut chips = Vec::new();
                if let Some(cat) = &self.plugin_catalog {
                    for p in cat.active_plugins() {
                        let Some(m) = &p.manifest else { continue };
                        for panel in &m.panels {
                            chips.push((
                                panel.title.clone(),
                                Hit::PluginOpenPanel(
                                    index,
                                    p.id.clone(),
                                    panel.id.clone(),
                                    panel.title.clone(),
                                ),
                                false,
                            ));
                        }
                    }
                }
                chips
            }
            Row::PluginItem(idx) => {
                let Some(p) = self
                    .plugin_catalog
                    .as_ref()
                    .and_then(|c| c.plugins.get(idx))
                else {
                    return Vec::new();
                };
                vec![
                    (
                        text(language, "On", "켜기").to_owned(),
                        Hit::PluginToggle(index, p.id.clone(), true),
                        p.enabled,
                    ),
                    (
                        text(language, "Off", "끄기").to_owned(),
                        Hit::PluginToggle(index, p.id.clone(), false),
                        !p.enabled,
                    ),
                ]
            }
            Row::PluginReload => vec![(
                text(language, "Reload plugins", "플러그인 다시 불러오기").to_owned(),
                Hit::PluginReload(index),
                false,
            )],
        }
    }

    /// Lines a row's controls take once its chips wrap to `width`.
    fn control_lines(&self, row: Row, width: u16) -> u16 {
        let chips = self.row_chips(row, 0);
        if chips.is_empty() {
            return 1;
        }
        flow(&chip_widths(&chips), width).1
    }

    fn draw_controls(
        &mut self,
        frame: &mut Frame,
        row: Row,
        index: usize,
        inner: Rect,
        y: i32,
        palette: Palette,
    ) {
        let visible = |y: i32| y >= inner.y as i32 && y < inner.bottom() as i32;
        if let Row::Setting(setting) = row
            && let Kind::Number { .. } = setting.kind
        {
            if !visible(y) {
                return;
            }
            let y = y as u16;
            let current = self
                .values
                .get(setting.key)
                .cloned()
                .unwrap_or_else(|| setting.default.to_owned());
            let mut x = inner.x;
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
            return;
        }
        // Chips that do not fit move to the next line.
        let chips = self.row_chips(row, index);
        let (places, _) = flow(&chip_widths(&chips), inner.width);
        for ((label, hit, selected), (x, line)) in chips.into_iter().zip(places) {
            let chip_y = y + line as i32;
            if visible(chip_y) {
                self.chip(
                    frame,
                    inner.x + x,
                    chip_y as u16,
                    &label,
                    hit,
                    selected,
                    inner,
                    palette,
                );
            }
        }
    }
}

/// What a row shows: the value in effect and, when it is not what the saved
/// choice asks for, that value for the row's note. `chosen` is the saved
/// `@masil-*` option and `effective` the tmux option it drives, both as the
/// server reports them. A value outside the catalog is shown as it is, a
/// hand-edited choice included: only an effect that differs from the choice
/// was set by something else.
fn shown(
    setting: &Setting,
    chosen: Option<String>,
    effective: Option<String>,
) -> (String, Option<String>) {
    let choice = chosen.unwrap_or_else(|| setting.default.to_owned());
    match effective {
        Some(effective) if effective != choice => (effective.clone(), Some(effective)),
        _ => (choice, None),
    }
}

/// What an Enter or click on Reset does at `now`, given when it was last
/// asked for: None runs the reset, else the question is open from the
/// returned instant. A press sooner than `RESET_PAUSE` after the last one
/// (a held key, a double click) or after `RESET_EXPIRY` asks again.
fn reset_press(asked: Option<Instant>, now: Instant) -> Option<Instant> {
    match asked.map(|asked| now.saturating_duration_since(asked)) {
        Some(waited) if waited >= RESET_PAUSE && waited < RESET_EXPIRY => None,
        _ => Some(now),
    }
}

/// Each --set pair with its catalog entry. An unknown key is an error.
fn known_sets(sets: &[(String, String)]) -> Result<Vec<(&'static Setting, &str)>, String> {
    sets.iter()
        .map(|(key, value)| {
            catalog::setting(key)
                .map(|setting| (setting, value.as_str()))
                .ok_or_else(|| format!("unknown setting {key}"))
        })
        .collect()
}

fn chip_widths(chips: &[(String, Hit, bool)]) -> Vec<u16> {
    chips
        .iter()
        .map(|(label, _, _)| cell_width(label) as u16 + 2)
        .collect()
}

/// Places items of `widths` left to right with one-cell gaps, wrapping
/// within `width`. Returns each item's column and line, and the line count.
fn flow(widths: &[u16], width: u16) -> (Vec<(u16, u16)>, u16) {
    let mut places = Vec::new();
    let (mut x, mut line) = (0u16, 0u16);
    for &item in widths {
        if x > 0 && x.saturating_add(item) > width {
            x = 0;
            line += 1;
        }
        places.push((x, line));
        x = x.saturating_add(item + 1);
    }
    (places, line + 1)
}

fn usage() -> String {
    "usage: masil-agent settings [--socket MASIL_SOCKET] [--group NAME] [--set KEY VALUE]... [--layer on|off] [--reset] [--get] | --reload-config".into()
}

/// Runs the settings screen, or applies choices without a screen when any
/// of --set, --layer, --reset or --get is given. --reload-config re-reads
/// the user's tmux configuration files in the core's startup order and
/// prints only errors, for the masil menu's run-shell.
pub(crate) fn run(args: &[String]) -> Result<i32, String> {
    let mut socket = None;
    let mut initial_group: Option<Group> = None;
    let mut sets = Vec::new();
    let mut layer = None;
    let mut reset = false;
    let mut get = false;
    let mut reload = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--socket" => {
                socket = Some(args.get(index + 1).ok_or_else(usage)?.clone());
                index += 2;
            }
            "--group" => {
                let raw = args
                    .get(index + 1)
                    .ok_or_else(usage)?
                    .trim()
                    .to_ascii_lowercase();
                initial_group = Some(match raw.as_str() {
                    "bar" | "status" => Group::Bar,
                    "panes" => Group::Panes,
                    "input" => Group::Input,
                    "look" | "appearance" => Group::Look,
                    "plugins" | "plugin" => Group::Plugins,
                    "layer" | "ui" => Group::Layer,
                    _ => return Err(usage()),
                });
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
            "--reload-config" => {
                reload = true;
                index += 1;
            }
            _ => return Err(usage()),
        }
    }
    if reload && (reset || layer.is_some() || !sets.is_empty() || get) {
        return Err(usage());
    }
    // Every --set is checked before anything is applied, a reset included.
    let sets = known_sets(&sets)?;
    let mut refused = false;
    for (setting, value) in &sets {
        if !setting.accepts(value) {
            refused = true;
            let outcome = Outcome::Failed(format!("unsupported value {value}"));
            println!("{}={value}: {outcome:?}", setting.key);
        }
    }
    if refused {
        return Ok(1);
    }
    let server = Server::locate(socket.as_deref())?;
    if reload {
        return Ok(match server.reload_user_config() {
            Ok(()) => 0,
            Err(error) => {
                println!("{error}");
                1
            }
        });
    }
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
        for (setting, value) in sets {
            report(
                format!("{}={value}", setting.key),
                server::apply(&server, path.as_deref(), setting, value),
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
    if let Some(g) = initial_group
        && let Some(pos) = Group::ALL.iter().position(|x| *x == g)
    {
        app.group = pos;
    }
    let mut session = Session::open()?;
    while !app.quit {
        if app.dirty {
            session
                .draw(|frame| app.draw(frame))
                .map_err(|error| error.to_string())?;
            app.dirty = false;
        }
        // An open Reset question ends on its own, and the row shows that.
        if let Some(wait) = app.reset_wait(Instant::now())
            && !event::poll(wait).map_err(|error| error.to_string())?
        {
            app.reset_armed = None;
            app.dirty = true;
            continue;
        }
        let first = event::read().map_err(|error| error.to_string())?;
        app.event(first, Instant::now());
        // Coalesce burst mouse moves/scrolls before drawing the next frame.
        for _ in 0..63 {
            if app.quit || !event::poll(Duration::ZERO).map_err(|error| error.to_string())? {
                break;
            }
            let next = event::read().map_err(|error| error.to_string())?;
            app.event(next, Instant::now());
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
                group == Group::Layer
                    || group == Group::Plugins
                    || SETTINGS.iter().any(|setting| setting.group == group),
                "{group:?} is empty"
            );
            assert!(!group.label(Language::Korean).is_empty());
        }
        for setting in SETTINGS {
            assert!(setting.key.starts_with("@masil-"));
            assert!(!setting.help(Language::Korean).is_empty());
        }
    }

    #[test]
    fn a_value_outside_the_catalog_is_shown_as_it_is() {
        let some = |text: &str| Some(text.to_owned());
        // The sidebar's collapse button sets a width the catalog has no
        // step for: the row shows it, and nothing else set it.
        let width = catalog::setting("@masil-status-width").unwrap();
        assert_eq!(shown(width, some("8"), some("8")), ("8".to_owned(), None));
        assert_eq!(
            shown(width, some("30"), some("30")),
            ("30".to_owned(), None)
        );
        assert_eq!(shown(width, None, None), ("24".to_owned(), None));
        // A choice edited by hand in settings.conf drives the option: the
        // row shows it and blames nothing else.
        let history = catalog::setting("@masil-history").unwrap();
        assert_eq!(shown(history, some("7"), some("7")), ("7".to_owned(), None));
        assert_eq!(shown(history, some("7"), None), ("7".to_owned(), None));
        // tmux.conf sets a history limit that is not one of the choices:
        // the row shows it and notes where it comes from.
        assert_eq!(
            shown(history, None, some("100000")),
            ("100000".to_owned(), some("100000"))
        );
        assert_eq!(
            shown(history, some("50000"), some("2000")),
            ("2000".to_owned(), some("2000"))
        );
        // The effect matches the choice, or there is none to read.
        assert_eq!(
            shown(history, some("50000"), some("50000")),
            ("50000".to_owned(), None)
        );
    }

    #[test]
    fn every_set_is_checked_before_anything_is_applied() {
        let pair = |key: &str, value: &str| (key.to_owned(), value.to_owned());
        assert_eq!(
            known_sets(&[pair("@masil-theme", "light"), pair("BAD", "x")]).unwrap_err(),
            "unknown setting BAD"
        );
        // The link setting is one the screen and --set can change.
        let sets = [pair("@masil-open-links", "on"), pair("@masil-theme", "up")];
        let known = known_sets(&sets).unwrap();
        assert_eq!(known[0].0.sections, ["keys"]);
        assert!(known[0].0.accepts(known[0].1) && !known[1].0.accepts(known[1].1));
        assert!(!known[0].0.accepts("yes"));
    }

    /// A screen over a server that does not exist; nothing is read from it.
    fn screen() -> App {
        App {
            server: Server::locate(Some("/nonexistent/masil-settings-test.sock")).unwrap(),
            path: None,
            language: Language::English,
            theme: Theme::Dark,
            group: Group::ALL.len() - 1,
            row: 0,
            focus: Focus::Rows,
            scroll: 0,
            scroll_max: 0,
            scroll_page: 1,
            scroll_track: Rect::default(),
            dragging_scroll: false,
            layer_on: true,
            values: HashMap::new(),
            configured: HashMap::new(),
            plugin_catalog: None,
            reset_armed: None,
            receipt: None,
            hits: Vec::new(),
            hover: None,
            pressed: None,
            dirty: true,
            quit: false,
        }
    }

    fn enter() -> KeyEvent {
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
    }

    fn drawn(app: &mut App, width: u16, height: u16) -> String {
        use ratatui::{Terminal, backend::TestBackend};
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    + "\n"
            })
            .collect()
    }

    #[test]
    fn the_reset_row_says_what_a_second_press_will_do() {
        let mut app = screen();
        let text = drawn(&mut app, 100, 30);
        assert!(
            text.contains(" Reset the settings on this screen "),
            "{text}"
        );
        assert!(text.contains("in settings.conf stay."), "{text}");
        // The label and the button are separate targets.
        assert!(app.hits.iter().any(|(_, hit)| *hit == Hit::Row(1)));
        assert!(app.hits.iter().any(|(_, hit)| *hit == Hit::Reset(1)));
        app.activate(Hit::Reset(1), Instant::now());
        let text = drawn(&mut app, 100, 30);
        assert!(text.contains(" Reset now "), "{text}");
        assert!(
            text.contains("After one second, Enter or a click"),
            "{text}"
        );
        app.language = Language::Korean;
        assert_eq!(app.row_chips(Row::Reset, 1)[0].0, "지금 초기화");
        assert!(app.reset_armed.is_some());
    }

    #[test]
    fn a_second_press_on_reset_counts_only_after_the_pause_and_before_the_expiry() {
        let start = Instant::now();
        let at = |milliseconds: u64| start + Duration::from_millis(milliseconds);
        // The first press asks.
        assert_eq!(reset_press(None, at(0)), Some(at(0)));
        // A second one at 500 ms does not run it; one at 1.1 s does.
        assert_eq!(reset_press(Some(at(0)), at(500)), Some(at(500)));
        assert_eq!(reset_press(Some(at(0)), at(1_100)), None);
        // A held key repeats from about 500 ms on, every 30 ms: each repeat
        // starts the pause again, so none of them runs it.
        let mut asked = reset_press(None, at(0));
        for repeat in 0..200 {
            asked = reset_press(asked, at(500 + repeat * 30));
            assert!(asked.is_some(), "repeat {repeat} ran the reset");
        }
        // After the expiry a press asks again.
        assert_eq!(reset_press(Some(at(0)), at(4_900)), None);
        assert_eq!(reset_press(Some(at(0)), at(5_000)), Some(at(5_000)));
    }

    #[test]
    fn reset_asks_first_and_its_label_only_takes_the_focus() {
        let start = Instant::now();
        let at = |milliseconds: u64| start + Duration::from_millis(milliseconds);
        let mut app = screen();
        assert_eq!(app.rows(), [Row::Layer, Row::Reset]);
        // A click on the row's label focuses it and asks nothing.
        app.activate(Hit::Row(1), at(0));
        assert_eq!((app.row, app.reset_armed), (1, None));
        // The first Enter asks. One 500 ms later, as from a held key,
        // neither runs the reset nor takes the question back.
        app.key(enter(), at(0));
        assert_eq!(app.reset_armed, Some(at(0)));
        app.key(enter(), at(500));
        assert_eq!(app.reset_armed, Some(at(500)));
        assert!(app.receipt.is_none());
        assert_eq!(app.reset_wait(at(1_500)), Some(Duration::from_secs(4)));
        let chips = app.row_chips(Row::Reset, 1);
        assert_eq!(
            (chips[0].0.as_str(), &chips[0].1),
            ("Reset now", &Hit::Reset(1))
        );
        // Any other key takes it back, and so does a click elsewhere.
        app.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), at(600));
        assert_eq!((app.row, app.reset_armed), (0, None));
        app.activate(Hit::Reset(1), at(700));
        assert!(app.reset_armed.is_some());
        app.activate(Hit::Row(1), at(800));
        assert!(app.reset_armed.is_none() && app.receipt.is_none());
        assert_eq!(
            app.row_chips(Row::Reset, 1)[0].0,
            "Reset the settings on this screen"
        );
    }

    #[test]
    fn input_that_is_not_an_answer_takes_the_reset_question_back() {
        let start = Instant::now();
        let mouse = |kind, column, row| {
            Event::Mouse(MouseEvent {
                kind,
                column,
                row,
                modifiers: KeyModifiers::NONE,
            })
        };
        let mut app = screen();
        drawn(&mut app, 100, 30);
        let (button, _) = app
            .hits
            .iter()
            .find(|(_, hit)| *hit == Hit::Reset(1))
            .cloned()
            .expect("the Reset button");
        let left = MouseButton::Left;
        let events = [
            // A press on blank space, and one with another button.
            mouse(MouseEventKind::Down(left), 99, 29),
            mouse(MouseEventKind::Down(MouseButton::Right), button.x, button.y),
            mouse(MouseEventKind::ScrollDown, button.x, button.y),
            mouse(MouseEventKind::ScrollUp, button.x, button.y),
            Event::Resize(90, 30),
            Event::FocusLost,
            Event::FocusGained,
        ];
        for event in events {
            app.reset_armed = Some(start);
            app.event(event.clone(), start);
            assert_eq!(app.reset_armed, None, "{event:?} kept the question");
        }
        // A press on the button released somewhere else.
        app.reset_armed = Some(start);
        app.event(mouse(MouseEventKind::Down(left), button.x, button.y), start);
        assert_eq!(app.reset_armed, Some(start));
        app.event(mouse(MouseEventKind::Up(left), 99, 29), start);
        assert_eq!(app.reset_armed, None);
        // Moving the pointer and releasing a key leave it open.
        app.reset_armed = Some(start);
        app.event(mouse(MouseEventKind::Moved, 3, 3), start);
        let mut release = enter();
        release.kind = KeyEventKind::Release;
        app.event(Event::Key(release), start);
        assert_eq!(app.reset_armed, Some(start));
        // A click on the button itself asks, and nothing ran.
        app.reset_armed = None;
        app.event(mouse(MouseEventKind::Down(left), button.x, button.y), start);
        app.event(mouse(MouseEventKind::Up(left), button.x, button.y), start);
        assert_eq!(app.reset_armed, Some(start));
        assert!(app.receipt.is_none());
    }

    #[test]
    fn overflow_shows_a_clickable_scrollbar_and_terminal_theme_raises_chips() {
        let mut app = screen();
        app.group = 0;
        app.theme = Theme::Terminal;
        // Short viewport forces overflow on the first section.
        let _ = drawn(&mut app, 76, 12);
        assert!(
            app.hits.iter().any(|(_, hit)| *hit == Hit::ScrollTrack),
            "expected scrollbar track when rows overflow"
        );
        assert!(
            app.hits.iter().any(|(_, hit)| *hit == Hit::ScrollThumb),
            "expected scrollbar thumb when rows overflow"
        );
        assert!(app.scroll_max > 0);
        // Clicking the bottom of the track scrolls to the bottom.
        let bottom_y = app.scroll_track.bottom() - 1;
        app.scroll_to_row(bottom_y);
        assert_eq!(app.scroll, app.scroll_max);
    }
}
