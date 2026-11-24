//! Interactive `ratatui` TUI onboarding wizard (`masil-agent remote setup`) for:
//! 1. Provider hooks (opt-in marker-based install/remove for claude/codex/opencode/agy)
//! 2. Relay URL / Direct address / OS service (`launchd`/`systemd`) / Daemon control
//! 3. Live Unicode half-block QR pairing code & copyable `masil://pair?...` URI
//! 4. Paired mobile devices list & immediate revocation

use super::model::{Language, Theme};
use super::terminal::Session;
use super::view::Palette;
use crate::remote::config::{
    DeviceStore, HostIdentity, PairedDevice, RemoteConfig, render_terminal_qr,
    resolve_advertised_direct, write_qr_html_preview,
};
use crate::remote::hooks::{self, HookProviderStatus};
use crate::remote::{read_daemon_pid, service};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use std::process::{Command, Stdio};
use std::time::Duration;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Hooks,
    Network,
    Pair,
    Devices,
}

impl Tab {
    const ALL: [Self; 4] = [Self::Hooks, Self::Network, Self::Pair, Self::Devices];

    fn key(self) -> &'static str {
        match self {
            Self::Hooks => "1",
            Self::Network => "2",
            Self::Pair => "3",
            Self::Devices => "4",
        }
    }

    fn label(self, lang: Language) -> &'static str {
        match (self, lang) {
            (Self::Hooks, Language::English) => "Provider hooks",
            (Self::Hooks, Language::Korean) => "프로바이더 훅",
            (Self::Network, Language::English) => "Relay & daemon",
            (Self::Network, Language::Korean) => "릴레이·데몬",
            (Self::Pair, Language::English) => "QR pairing",
            (Self::Pair, Language::Korean) => "QR 페어링",
            (Self::Devices, Language::English) => "Paired devices",
            (Self::Devices, Language::Korean) => "연결 기기",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Hooks => Self::Network,
            Self::Network => Self::Pair,
            Self::Pair => Self::Devices,
            Self::Devices => Self::Hooks,
        }
    }

    fn prev(self) -> Self {
        match self {
            Self::Hooks => Self::Devices,
            Self::Network => Self::Hooks,
            Self::Pair => Self::Network,
            Self::Devices => Self::Pair,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EditField {
    RelayUrl,
    DirectAddr,
}

fn shorten_home_path(raw: &str) -> String {
    if let Some(home) = std::env::var_os("HOME").map(|h| h.to_string_lossy().into_owned())
        && let Some(rest) = raw.strip_prefix(&home)
    {
        return if rest.is_empty() {
            "~".to_owned()
        } else {
            format!("~{rest}")
        };
    }
    raw.to_owned()
}

fn pad_cells(text: &str, target: usize) -> String {
    let w = UnicodeWidthStr::width(text);
    if w >= target {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len() + (target - w));
    out.push_str(text);
    for _ in 0..(target - w) {
        out.push(' ');
    }
    out
}

fn fill_row_tail(spans: &mut Vec<Span<'static>>, width: usize, bg: Color) {
    let used: usize = spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum();
    if width > used {
        spans.push(Span::styled(
            " ".repeat(width - used),
            Style::default().bg(bg),
        ));
    }
}

fn keycap_spans(key: &str, label: &str, p: Palette, active: bool) -> Vec<Span<'static>> {
    if active {
        let style = Style::default()
            .bg(p.accent)
            .fg(p.accent_text)
            .add_modifier(Modifier::BOLD);
        vec![Span::styled(format!(" {key} {label} "), style)]
    } else {
        vec![
            Span::styled(
                format!(" {key}"),
                Style::default()
                    .bg(p.raised)
                    .fg(p.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" {label} "),
                Style::default().bg(p.raised).fg(p.text),
            ),
        ]
    }
}

struct App {
    lang: Language,
    theme: Theme,
    tab: Tab,
    config: RemoteConfig,
    identity: HostIdentity,
    hooks: Vec<HookProviderStatus>,
    devices: Vec<PairedDevice>,
    selected: usize,
    daemon_pid: Option<u32>,
    service_installed: bool,
    qr_lines: Vec<String>,
    pair_uri: Option<String>,
    qr_html_path: Option<std::path::PathBuf>,
    editing: Option<(EditField, String)>,
    status_msg: String,
    status_is_err: bool,
    quit: bool,
}

impl App {
    fn new(lang: Language, theme: Theme) -> Result<Self, String> {
        let config = RemoteConfig::load()?;
        let identity = HostIdentity::load_or_create()?;
        let mut app = Self {
            lang,
            theme,
            tab: Tab::Hooks,
            config,
            identity,
            hooks: Vec::new(),
            devices: Vec::new(),
            selected: 0,
            daemon_pid: None,
            service_installed: false,
            qr_lines: Vec::new(),
            pair_uri: None,
            qr_html_path: None,
            editing: None,
            status_msg: String::new(),
            status_is_err: false,
            quit: false,
        };
        app.refresh();
        let all_detected_hooks_ready = !app.hooks.is_empty()
            && app
                .hooks
                .iter()
                .filter(|h| h.cli_installed)
                .all(|h| h.hook_installed);
        if all_detected_hooks_ready && app.daemon_pid.is_some() {
            app.tab = Tab::Pair;
            app.regenerate_qr();
            app.status_msg.clear();
        }
        Ok(app)
    }

    fn t<'a>(&self, en: &'a str, ko: &'a str) -> &'a str {
        match self.lang {
            Language::English => en,
            Language::Korean => ko,
        }
    }

    fn set_ok(&mut self, msg: impl Into<String>) {
        self.status_is_err = false;
        self.status_msg = msg.into();
    }

    fn set_err(&mut self, msg: impl Into<String>) {
        self.status_is_err = true;
        self.status_msg = msg.into();
    }

    fn refresh(&mut self) {
        self.hooks = hooks::inspect_all();
        self.devices = DeviceStore::load().map(|d| d.devices).unwrap_or_default();
        self.daemon_pid = read_daemon_pid();
        self.service_installed = service::is_installed();
        let max_len = self.list_len();
        if max_len == 0 {
            self.selected = 0;
        } else if self.selected >= max_len {
            self.selected = max_len - 1;
        }
    }

    fn list_len(&self) -> usize {
        match self.tab {
            Tab::Hooks => self.hooks.len(),
            Tab::Network => 5,
            Tab::Pair => 1,
            Tab::Devices => self.devices.len(),
        }
    }

    fn regenerate_qr(&mut self) {
        if self.config.relay_url.is_none()
            && self.config.advertise_direct.is_none()
            && self.config.listen_addr.is_none()
        {
            self.config.listen_addr = Some("0.0.0.0:7767".into());
        }
        self.config.enabled = true;
        let _ = self.config.save();
        match DeviceStore::create_pairing_offer(&self.identity, &self.config) {
            Ok((offer, uri)) => match render_terminal_qr(&uri) {
                Ok(lines) => {
                    self.qr_lines = lines;
                    self.qr_html_path = write_qr_html_preview(&uri, &offer.host_name).ok();
                    self.pair_uri = Some(uri);
                    self.set_ok(
                        self.t(
                            "Generated fresh 5-minute pairing QR code. Press o for crisp browser QR.",
                            "5분간 유효한 새 페어링 QR 코드를 생성했습니다. o 키로 브라우저에서 선명한 QR을 열 수 있습니다.",
                        )
                        .to_owned(),
                    );
                }
                Err(e) => self.set_err(e),
            },
            Err(e) => self.set_err(e),
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        if let Some((field, mut buf)) = self.editing.take() {
            match key.code {
                KeyCode::Esc => {
                    self.set_ok(self.t("Edit cancelled.", "입력을 취소했습니다.").to_owned());
                }
                KeyCode::Enter => {
                    let trimmed = buf.trim().to_owned();
                    let val = (!trimmed.is_empty()).then_some(trimmed);
                    match field {
                        EditField::RelayUrl => self.config.relay_url = val,
                        EditField::DirectAddr => {
                            self.config.advertise_direct = val.clone();
                            if val.is_some()
                                && self
                                    .config
                                    .listen_addr
                                    .as_deref()
                                    .is_some_and(|s| s.starts_with("127.0.0.1:"))
                            {
                                let port = self
                                    .config
                                    .listen_addr
                                    .as_deref()
                                    .and_then(|s| s.rsplit_once(':'))
                                    .map(|(_, p)| p)
                                    .unwrap_or("7767");
                                self.config.listen_addr = Some(format!("0.0.0.0:{port}"));
                            }
                        }
                    }
                    self.config.enabled = true;
                    match self.config.save() {
                        Ok(()) => self.set_ok(
                            self.t(
                                "Saved ~/.config/masil/remote.toml",
                                "원격 설정을 저장했습니다.",
                            )
                            .to_owned(),
                        ),
                        Err(e) => self.set_err(e),
                    }
                }
                KeyCode::Backspace => {
                    buf.pop();
                    self.editing = Some((field, buf));
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    buf.push(c);
                    self.editing = Some((field, buf));
                }
                _ => {
                    self.editing = Some((field, buf));
                }
            }
            return;
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => self.quit = true,
            KeyCode::Char('l') | KeyCode::Char('L') => {
                self.lang = match self.lang {
                    Language::English => Language::Korean,
                    Language::Korean => Language::English,
                };
            }
            KeyCode::Char('t') | KeyCode::Char('T') => {
                self.theme = match self.theme {
                    Theme::Dark => Theme::Light,
                    Theme::Light => Theme::Terminal,
                    Theme::Terminal => Theme::Dark,
                };
            }
            KeyCode::Char('?') => {
                self.set_ok(
                    self.t(
                        "Keys: 1-4/Tab tabs · j/k move · Enter apply · o browser QR · r refresh · l lang · t theme · q close",
                        "단축키: 1-4/Tab 탭 전환 · j/k 이동 · Enter 실행 · o 브라우저 QR · r 새로고침 · l 언어 · t 테마 · q 닫기",
                    )
                    .to_owned(),
                );
            }
            KeyCode::Char('1') => self.switch_tab(Tab::Hooks),
            KeyCode::Char('2') => self.switch_tab(Tab::Network),
            KeyCode::Char('3') => self.switch_tab(Tab::Pair),
            KeyCode::Char('4') => self.switch_tab(Tab::Devices),
            KeyCode::Tab | KeyCode::Right => self.switch_tab(self.tab.next()),
            KeyCode::BackTab | KeyCode::Left => self.switch_tab(self.tab.prev()),
            KeyCode::Up | KeyCode::Char('k') => {
                if self.selected > 0 {
                    self.selected -= 1;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.selected + 1 < self.list_len() {
                    self.selected += 1;
                }
            }
            KeyCode::Char('r') | KeyCode::Char('R') => {
                self.refresh();
                if self.tab == Tab::Pair {
                    self.regenerate_qr();
                } else {
                    self.set_ok(
                        self.t("Refreshed status.", "상태를 새로고침했습니다.")
                            .to_owned(),
                    );
                }
            }
            KeyCode::Enter | KeyCode::Char(' ') => self.activate_selected(),
            KeyCode::Char('o') | KeyCode::Char('O') if self.tab == Tab::Pair => {
                if self.qr_html_path.is_none() {
                    self.regenerate_qr();
                }
                if let Some(ref path) = self.qr_html_path {
                    #[cfg(target_os = "macos")]
                    let res = Command::new("open").arg(path).spawn();
                    #[cfg(target_os = "linux")]
                    let res = Command::new("xdg-open").arg(path).spawn();
                    match res {
                        Ok(_) => self.set_ok(
                            self.t(
                                "Opened crisp SVG QR in browser.",
                                "브라우저에서 선명한 SVG QR 코드를 열었습니다.",
                            )
                            .to_owned(),
                        ),
                        Err(e) => self.set_err(format!("Failed opening browser QR: {e}")),
                    }
                }
            }
            KeyCode::Char('a') | KeyCode::Char('A') if self.tab == Tab::Hooks => {
                let mut count = 0usize;
                for h in self.hooks.clone() {
                    if h.cli_installed
                        && !h.hook_installed
                        && hooks::install_provider(&h.id).is_ok()
                    {
                        count += 1;
                    }
                }
                self.refresh();
                self.set_ok(match self.lang {
                    Language::English => {
                        format!("Installed hooks for {count} detected provider(s).")
                    }
                    Language::Korean => format!("감지된 프로바이더 {count}곳에 훅을 설치했습니다."),
                });
            }
            _ => {}
        }
    }

    fn switch_tab(&mut self, tab: Tab) {
        self.tab = tab;
        self.selected = 0;
        self.refresh();
        if tab == Tab::Pair && self.qr_lines.is_empty() {
            self.regenerate_qr();
        }
    }

    fn activate_selected(&mut self) {
        match self.tab {
            Tab::Hooks => {
                if let Some(h) = self.hooks.get(self.selected).cloned() {
                    let res = if h.hook_installed {
                        hooks::uninstall_provider(&h.id)
                    } else {
                        hooks::install_provider(&h.id)
                    };
                    match res {
                        Ok(msg) => {
                            self.refresh();
                            self.set_ok(msg);
                        }
                        Err(e) => self.set_err(e),
                    }
                }
            }
            Tab::Network => match self.selected {
                0 => {
                    let cur = self.config.relay_url.clone().unwrap_or_default();
                    self.editing = Some((EditField::RelayUrl, cur));
                }
                1 => {
                    let cur = self
                        .config
                        .advertise_direct
                        .clone()
                        .or_else(|| self.config.listen_addr.clone())
                        .unwrap_or_default();
                    self.editing = Some((EditField::DirectAddr, cur));
                }
                2 => {
                    self.config.push_enabled = !self.config.push_enabled;
                    let _ = self.config.save();
                    self.set_ok(if self.config.push_enabled {
                        self.t("Relay push bridge enabled.", "릴레이 푸시 알림을 켰습니다.")
                            .to_owned()
                    } else {
                        self.t(
                            "Relay push bridge disabled.",
                            "릴레이 푸시 알림을 껐습니다.",
                        )
                        .to_owned()
                    });
                }
                3 => {
                    let res = if self.service_installed {
                        service::uninstall()
                    } else {
                        service::install()
                    };
                    match res {
                        Ok(msg) => {
                            self.refresh();
                            self.set_ok(msg);
                        }
                        Err(e) => self.set_err(e),
                    }
                }
                4 => {
                    if let Some(pid) = self.daemon_pid {
                        // SAFETY: SIGTERM asks our own daemon PID to shut down gracefully.
                        unsafe {
                            libc::kill(pid as libc::pid_t, libc::SIGTERM);
                        }
                        self.refresh();
                        self.set_ok(match self.lang {
                            Language::English => {
                                format!("Sent SIGTERM to remote daemon (pid {pid}).")
                            }
                            Language::Korean => {
                                format!("원격 데몬(pid {pid}) 종료 신호를 보냈습니다.")
                            }
                        });
                    } else if let Ok(exe) = std::env::current_exe() {
                        if self
                            .config
                            .listen_addr
                            .as_deref()
                            .is_some_and(|s| s.starts_with("127.0.0.1:"))
                        {
                            let port = self
                                .config
                                .listen_addr
                                .as_deref()
                                .and_then(|s| s.rsplit_once(':'))
                                .map(|(_, p)| p)
                                .unwrap_or("7767");
                            self.config.listen_addr = Some(format!("0.0.0.0:{port}"));
                        }
                        self.config.enabled = true;
                        let _ = self.config.save();
                        match Command::new(exe)
                            .args(["remote", "daemon"])
                            .stdin(Stdio::null())
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .spawn()
                        {
                            Ok(child) => {
                                self.daemon_pid = Some(child.id());
                                self.set_ok(match self.lang {
                                    Language::English => {
                                        format!("Started remote daemon (pid {}).", child.id())
                                    }
                                    Language::Korean => {
                                        format!("원격 데몬을 시작했습니다 (pid {}).", child.id())
                                    }
                                });
                            }
                            Err(e) => self.set_err(format!("Failed starting daemon: {e}")),
                        }
                    }
                }
                _ => {}
            },
            Tab::Pair => self.regenerate_qr(),
            Tab::Devices => {
                if let Some(dev) = self.devices.get(self.selected).cloned() {
                    match DeviceStore::revoke(&dev.device_id) {
                        Ok(true) => {
                            self.refresh();
                            self.set_ok(match self.lang {
                                Language::English => {
                                    format!("Revoked device {} ({}).", dev.name, dev.device_id)
                                }
                                Language::Korean => format!(
                                    "기기 {} ({})의 접근 권한을 철회했습니다.",
                                    dev.name, dev.device_id
                                ),
                            });
                        }
                        Ok(false) => self.set_err(
                            self.t("Device was already removed.", "이미 제거된 기기입니다.")
                                .to_owned(),
                        ),
                        Err(e) => self.set_err(e),
                    }
                }
            }
        }
    }

    fn theme_label(&self) -> &'static str {
        match (self.theme, self.lang) {
            (Theme::Dark, Language::English) => "Dark",
            (Theme::Dark, Language::Korean) => "어둡게",
            (Theme::Light, Language::English) => "Light",
            (Theme::Light, Language::Korean) => "밝게",
            (Theme::Terminal, Language::English) => "Terminal",
            (Theme::Terminal, Language::Korean) => "터미널",
        }
    }

    fn render(&self, f: &mut Frame) {
        let p = Palette::for_theme(self.theme);
        let area = f.area();
        let bg = Style::default().bg(p.canvas).fg(p.text);
        f.render_widget(Block::default().style(bg), area);

        let dense = area.height < 18;
        let header_h = if dense { 1 } else { 3 };
        let tabs_h = if dense { 1 } else { 2 };
        let actions_h = if dense { 1 } else { 2 };
        let footer_h = if dense { 1 } else { 2 };

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(header_h),
                Constraint::Length(tabs_h),
                Constraint::Min(8),
                Constraint::Length(actions_h),
                Constraint::Length(footer_h),
            ])
            .split(area);

        self.render_header(f, chunks[0], p);
        self.render_tabs(f, chunks[1], p);
        self.render_body(f, chunks[2], p);
        self.render_actions(f, chunks[3], p);
        self.render_footer(f, chunks[4], p);
    }

    fn render_header(&self, f: &mut Frame, area: Rect, p: Palette) {
        f.render_widget(Block::default().style(Style::default().bg(p.panel)), area);
        let row_y = area.y + area.height / 2;
        let inner = Rect::new(area.x + 1, row_y, area.width.saturating_sub(2), 1);

        let lang_chip = keycap_spans("l", self.t("EN", "한국어"), p, false);
        let theme_chip = keycap_spans("t", self.theme_label(), p, false);
        let help_chip = keycap_spans("?", self.t("Help", "도움말"), p, false);
        let close_chip = keycap_spans("q", self.t("Close", "닫기"), p, false);

        let mut right_spans: Vec<Span<'static>> = Vec::new();
        right_spans.extend(lang_chip);
        right_spans.push(Span::styled(" ", Style::default().bg(p.panel)));
        right_spans.extend(theme_chip);
        right_spans.push(Span::styled(" ", Style::default().bg(p.panel)));
        right_spans.extend(help_chip);
        right_spans.push(Span::styled(" ", Style::default().bg(p.panel)));
        right_spans.extend(close_chip);
        let right_w: u16 = right_spans
            .iter()
            .map(|s| UnicodeWidthStr::width(s.content.as_ref()) as u16)
            .sum();

        let status_span = if let Some(pid) = self.daemon_pid {
            Span::styled(
                match self.lang {
                    Language::English => format!("Daemon running · pid {pid}"),
                    Language::Korean => format!("데몬 실행 중 · pid {pid}"),
                },
                Style::default().fg(p.success).bg(p.panel),
            )
        } else {
            Span::styled(
                self.t("Daemon stopped", "데몬 중지됨"),
                Style::default().fg(p.warning).bg(p.panel),
            )
        };

        let left_spans = vec![
            Span::styled(
                "masil",
                Style::default()
                    .fg(p.accent)
                    .bg(p.panel)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("  ", Style::default().bg(p.panel)),
            Span::styled(
                self.t("Remote setup", "원격 설정"),
                Style::default().fg(p.text).bg(p.panel),
            ),
            Span::styled("  ", Style::default().bg(p.panel)),
            status_span,
        ];

        let left_w = inner.width.saturating_sub(right_w + 2);
        f.render_widget(
            Paragraph::new(Line::from(left_spans)),
            Rect::new(inner.x, inner.y, left_w, 1),
        );
        if inner.width > right_w {
            f.render_widget(
                Paragraph::new(Line::from(right_spans)),
                Rect::new(inner.right().saturating_sub(right_w), inner.y, right_w, 1),
            );
        }
    }

    fn render_tabs(&self, f: &mut Frame, area: Rect, p: Palette) {
        let installed_hooks = self.hooks.iter().filter(|h| h.hook_installed).count();
        let total_hooks = self.hooks.len();
        let row = Rect::new(area.x + 1, area.y, area.width.saturating_sub(2), 1);
        let mut spans: Vec<Span<'static>> = Vec::new();
        for (idx, t) in Tab::ALL.iter().enumerate() {
            if idx > 0 {
                spans.push(Span::styled(" ", Style::default().bg(p.canvas)));
            }
            let active = *t == self.tab;
            let suffix = match t {
                Tab::Hooks => format!(" {installed_hooks}/{total_hooks}"),
                Tab::Devices => format!(" {}", self.devices.len()),
                _ => String::new(),
            };
            let label = format!("{}{}", t.label(self.lang), suffix);
            spans.extend(keycap_spans(t.key(), &label, p, active));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), row);
    }

    fn render_body(&self, f: &mut Frame, area: Rect, p: Palette) {
        if self.tab == Tab::Pair {
            self.render_pair_body(f, area, p);
            return;
        }

        let wide = area.width >= 92;
        if wide {
            let left_w = ((area.width as u32 * 58) / 100).clamp(48, 78) as u16;
            let right_w = area.width.saturating_sub(left_w + 1);
            let left = Rect::new(area.x, area.y, left_w, area.height);
            let right = Rect::new(left.right() + 1, area.y, right_w, area.height);
            self.render_active_step(f, left, p);
            self.render_summary_inspector(f, right, p);
        } else {
            self.render_active_step(f, area, p);
        }
    }

    fn render_active_step(&self, f: &mut Frame, area: Rect, p: Palette) {
        if self.tab == Tab::Hooks && area.height >= 16 {
            let table_h = (self.hooks.len() as u16 + 7).min(area.height.saturating_sub(6));
            let split = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(table_h), Constraint::Min(6)])
                .split(area);
            self.render_hooks_table_panel(f, split[0], p);
            self.render_hook_detail_panel(f, split[1], p);
            return;
        }
        let title = format!(" {} ", self.tab.label(self.lang));
        let block = Block::default()
            .title(Span::styled(
                title,
                Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(p.accent))
            .style(Style::default().bg(p.panel));
        let raw_inner = block.inner(area);
        f.render_widget(block, area);
        if raw_inner.width < 4 || raw_inner.height < 2 {
            return;
        }
        let inner = Rect::new(
            raw_inner.x + 1,
            raw_inner.y,
            raw_inner.width.saturating_sub(3),
            raw_inner.height,
        );
        let w = inner.width as usize;

        let mut lines: Vec<Line<'static>> = Vec::new();
        match self.tab {
            Tab::Hooks => {
                lines.push(Line::from(Span::styled(
                    self.t(
                        "Marker-based hooks link live panes to local transcripts cleanly.",
                        "마커 기반 훅이 라이브 세션과 로컬 대화 기록을 정확히 연결합니다.",
                    ),
                    Style::default().fg(p.muted),
                )));
                lines.push(Line::raw(""));
                // Table column header (3 crisp columns; full config path is shown in Selected below)
                let hdr_status = pad_cells(self.t("Status", "상태"), 12);
                let hdr_name = pad_cells(self.t("Provider", "프로바이더"), 24);
                lines.push(Line::from(Span::styled(
                    format!("  {hdr_status} {hdr_name} {}", self.t("CLI", "CLI")),
                    Style::default().fg(p.muted).add_modifier(Modifier::BOLD),
                )));
                lines.push(Line::from(Span::styled(
                    "─".repeat(w),
                    Style::default().fg(p.track),
                )));

                for (i, h) in self.hooks.iter().enumerate() {
                    let sel = i == self.selected;
                    let row_bg = if sel { p.raised } else { p.panel };
                    let marker = if sel { "› " } else { "  " };
                    let (status_txt, status_col) = if h.hook_installed {
                        (self.t("Installed", "설치됨"), p.success)
                    } else {
                        (self.t("Off", "미설치"), p.warning)
                    };
                    let cli_txt = if h.cli_installed {
                        self.t("Detected", "감지됨")
                    } else {
                        self.t("Missing", "미설치")
                    };
                    let name_style = if sel {
                        Style::default()
                            .fg(p.text)
                            .bg(row_bg)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(p.text).bg(row_bg)
                    };
                    let mut spans = vec![
                        Span::styled(marker, Style::default().fg(p.accent).bg(row_bg)),
                        Span::styled(
                            format!("{} ", pad_cells(status_txt, 12)),
                            Style::default().fg(status_col).bg(row_bg),
                        ),
                        Span::styled(format!("{} ", pad_cells(&h.label, 24)), name_style),
                        Span::styled(cli_txt, Style::default().fg(p.muted).bg(row_bg)),
                    ];
                    fill_row_tail(&mut spans, w, row_bg);
                    lines.push(Line::from(spans));
                }

                if let Some(h) = self.hooks.get(self.selected) {
                    lines.push(Line::raw(""));
                    lines.push(Line::from(Span::styled(
                        "─".repeat(w),
                        Style::default().fg(p.track),
                    )));
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!("{}  ", self.t("Selected", "선택 항목")),
                            Style::default().fg(p.muted).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            h.label.clone(),
                            Style::default().fg(p.text).add_modifier(Modifier::BOLD),
                        ),
                    ]));
                    lines.push(Line::from(vec![
                        Span::styled(
                            pad_cells(self.t("Config path", "설정 파일"), 14),
                            Style::default().fg(p.muted),
                        ),
                        Span::styled(
                            shorten_home_path(&h.config_path),
                            Style::default().fg(p.text),
                        ),
                    ]));
                    lines.push(Line::from(vec![
                        Span::styled(
                            pad_cells(self.t("Action", "동작"), 14),
                            Style::default().fg(p.muted),
                        ),
                        Span::styled(
                            if h.hook_installed {
                                self.t(
                                    "Enter removes masil hook (user hooks stay intact)",
                                    "Enter로 masil 훅 제거 (기존 사용자 훅 유지)",
                                )
                            } else {
                                self.t(
                                    "Enter installs masil marker hook safely",
                                    "Enter로 masil 마커 훅 안전 설치",
                                )
                            },
                            Style::default().fg(p.accent),
                        ),
                    ]));
                }
            }
            Tab::Network => {
                lines.push(Line::from(Span::styled(
                    self.t(
                        "Configure direct LAN/Tailscale access, optional E2EE relay, and host daemon.",
                        "직접 LAN/Tailscale 접속, 선택형 E2EE 릴레이 및 호스트 데몬을 관리합니다.",
                    ),
                    Style::default().fg(p.muted),
                )));
                lines.push(Line::raw(""));
                let rows = [
                    (
                        self.t("Relay URL", "릴레이 URL"),
                        self.config.relay_url.clone().unwrap_or_else(|| {
                            self.t("Direct LAN/Tailscale only", "직접 LAN/Tailscale 전용")
                                .to_owned()
                        }),
                        self.config.relay_url.is_some(),
                    ),
                    (
                        self.t("Direct address", "직접 접속 주소"),
                        resolve_advertised_direct(&self.config)
                            .or_else(|| self.config.listen_addr.clone())
                            .unwrap_or_else(|| self.t("Disabled", "비활성").to_owned()),
                        true,
                    ),
                    (
                        self.t("Push bridge", "푸시 알림 브리지"),
                        if self.config.push_enabled {
                            self.t("Enabled", "사용 중").to_owned()
                        } else {
                            self.t("Disabled", "꺼짐").to_owned()
                        },
                        self.config.push_enabled,
                    ),
                    (
                        self.t("Auto-start service", "자동 시작 서비스"),
                        if self.service_installed {
                            self.t("Installed", "설치됨").to_owned()
                        } else {
                            self.t("Not installed", "미설치").to_owned()
                        },
                        self.service_installed,
                    ),
                    (
                        self.t("Remote daemon", "원격 데몬"),
                        match self.daemon_pid {
                            Some(pid) => match self.lang {
                                Language::English => format!("Running (pid {pid})"),
                                Language::Korean => format!("실행 중 (pid {pid})"),
                            },
                            None => self.t("Stopped", "중지됨").to_owned(),
                        },
                        self.daemon_pid.is_some(),
                    ),
                ];
                for (i, (label, val, good)) in rows.iter().enumerate() {
                    let sel = i == self.selected;
                    let row_bg = if sel { p.raised } else { p.panel };
                    let marker = if sel { "› " } else { "  " };
                    let val_col = if *good { p.success } else { p.muted };
                    let mut spans = vec![
                        Span::styled(marker, Style::default().fg(p.accent).bg(row_bg)),
                        Span::styled(
                            pad_cells(label, 22),
                            Style::default().fg(p.muted).bg(row_bg),
                        ),
                        Span::styled(
                            val.clone(),
                            Style::default()
                                .fg(val_col)
                                .bg(row_bg)
                                .add_modifier(if sel {
                                    Modifier::BOLD
                                } else {
                                    Modifier::empty()
                                }),
                        ),
                    ];
                    fill_row_tail(&mut spans, w, row_bg);
                    lines.push(Line::from(spans));
                }
                if let Some((field, ref buf)) = self.editing {
                    lines.push(Line::raw(""));
                    let prompt = match field {
                        EditField::RelayUrl => self.t("Relay URL > ", "릴레이 URL > "),
                        EditField::DirectAddr => self.t("Direct Address > ", "직접 주소 > "),
                    };
                    lines.push(Line::from(vec![
                        Span::styled(
                            prompt,
                            Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(format!("{buf}_"), Style::default().fg(p.text)),
                    ]));
                }
            }
            Tab::Devices => {
                lines.push(Line::from(Span::styled(
                    self.t(
                        "Paired mobile devices. Press Enter to immediately revoke access.",
                        "연결된 모바일 기기 목록입니다. Enter를 누르면 즉시 접근 권한을 철회합니다.",
                    ),
                    Style::default().fg(p.muted),
                )));
                lines.push(Line::raw(""));
                if self.devices.is_empty() {
                    lines.push(Line::from(Span::styled(
                        self.t(
                            "No paired devices yet. Switch to tab 3 to scan a QR code.",
                            "연결된 기기가 없습니다. 3번 탭에서 QR 코드를 스캔해 연결하세요.",
                        ),
                        Style::default().fg(p.warning),
                    )));
                } else {
                    for (i, d) in self.devices.iter().enumerate() {
                        let sel = i == self.selected;
                        let row_bg = if sel { p.raised } else { p.panel };
                        let marker = if sel { "› " } else { "  " };
                        let push_txt = if d.push_token.is_some() {
                            self.t("Push on", "푸시 켜짐")
                        } else {
                            self.t("Push off", "푸시 꺼짐")
                        };
                        let mut spans = vec![
                            Span::styled(marker, Style::default().fg(p.accent).bg(row_bg)),
                            Span::styled(
                                format!("{} ", pad_cells(&d.name, 18)),
                                Style::default().fg(p.text).bg(row_bg).add_modifier(if sel {
                                    Modifier::BOLD
                                } else {
                                    Modifier::empty()
                                }),
                            ),
                            Span::styled(
                                format!("{} ", pad_cells(&d.platform, 10)),
                                Style::default().fg(p.accent).bg(row_bg),
                            ),
                            Span::styled(
                                format!("{} ", pad_cells(push_txt, 10)),
                                Style::default().fg(p.muted).bg(row_bg),
                            ),
                            Span::styled(
                                d.device_id.clone(),
                                Style::default().fg(p.muted).bg(row_bg),
                            ),
                        ];
                        fill_row_tail(&mut spans, w, row_bg);
                        lines.push(Line::from(spans));
                    }
                }
            }
            Tab::Pair => {}
        }

        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
    }

    fn render_hooks_table_panel(&self, f: &mut Frame, area: Rect, p: Palette) {
        let block = Block::default()
            .title(Span::styled(
                format!(" {} ", self.tab.label(self.lang)),
                Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(p.accent))
            .style(Style::default().bg(p.panel));
        let raw_inner = block.inner(area);
        f.render_widget(block, area);
        if raw_inner.width < 4 || raw_inner.height < 2 {
            return;
        }
        let inner = Rect::new(
            raw_inner.x + 1,
            raw_inner.y,
            raw_inner.width.saturating_sub(3),
            raw_inner.height,
        );
        let w = inner.width as usize;
        let hdr_status = pad_cells(self.t("Status", "상태"), 12);
        let hdr_name = pad_cells(self.t("Provider", "프로바이더"), 24);
        let mut lines: Vec<Line<'static>> = vec![
            Line::from(Span::styled(
                self.t(
                    "Marker-based hooks link live panes to local transcripts cleanly.",
                    "마커 기반 훅이 라이브 세션과 로컬 대화 기록을 정확히 연결합니다.",
                ),
                Style::default().fg(p.muted),
            )),
            Line::raw(""),
            Line::from(Span::styled(
                format!("  {hdr_status} {hdr_name} {}", self.t("CLI", "CLI")),
                Style::default().fg(p.muted).add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled("─".repeat(w), Style::default().fg(p.track))),
        ];
        for (i, h) in self.hooks.iter().enumerate() {
            let sel = i == self.selected;
            let row_bg = if sel { p.raised } else { p.panel };
            let marker = if sel { "› " } else { "  " };
            let (status_txt, status_col) = if h.hook_installed {
                (self.t("Installed", "설치됨"), p.success)
            } else {
                (self.t("Off", "미설치"), p.warning)
            };
            let cli_txt = if h.cli_installed {
                self.t("Detected", "감지됨")
            } else {
                self.t("Missing", "미설치")
            };
            let name_style = if sel {
                Style::default()
                    .fg(p.text)
                    .bg(row_bg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(p.text).bg(row_bg)
            };
            let mut spans = vec![
                Span::styled(marker, Style::default().fg(p.accent).bg(row_bg)),
                Span::styled(
                    format!("{} ", pad_cells(status_txt, 12)),
                    Style::default().fg(status_col).bg(row_bg),
                ),
                Span::styled(format!("{} ", pad_cells(&h.label, 24)), name_style),
                Span::styled(cli_txt, Style::default().fg(p.muted).bg(row_bg)),
            ];
            fill_row_tail(&mut spans, w, row_bg);
            lines.push(Line::from(spans));
        }
        f.render_widget(Paragraph::new(lines), inner);
    }

    fn render_hook_detail_panel(&self, f: &mut Frame, area: Rect, p: Palette) {
        let block = Block::default()
            .title(Span::styled(
                format!(" {} ", self.t("Selected hook", "선택 항목 상세")),
                Style::default().fg(p.muted),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(p.track))
            .style(Style::default().bg(p.panel));
        let raw_inner = block.inner(area);
        f.render_widget(block, area);
        if raw_inner.width < 4 || raw_inner.height < 2 {
            return;
        }
        let inner = Rect::new(
            raw_inner.x + 1,
            raw_inner.y,
            raw_inner.width.saturating_sub(2),
            raw_inner.height,
        );
        let Some(h) = self.hooks.get(self.selected) else {
            return;
        };
        let lines = vec![
            Line::from(Span::styled(
                h.label.clone(),
                Style::default().fg(p.text).add_modifier(Modifier::BOLD),
            )),
            Line::from(vec![
                Span::styled(
                    pad_cells(self.t("Config path", "설정 파일"), 14),
                    Style::default().fg(p.muted),
                ),
                Span::styled(
                    shorten_home_path(&h.config_path),
                    Style::default().fg(p.text),
                ),
            ]),
            Line::from(vec![
                Span::styled(
                    pad_cells(self.t("Action", "동작"), 14),
                    Style::default().fg(p.muted),
                ),
                Span::styled(
                    if h.hook_installed {
                        self.t(
                            "Enter removes masil hook (user hooks stay intact)",
                            "Enter로 masil 훅 제거 (기존 사용자 훅 유지)",
                        )
                    } else {
                        self.t(
                            "Enter installs masil marker hook safely",
                            "Enter로 masil 마커 훅 안전 설치",
                        )
                    },
                    Style::default().fg(p.accent),
                ),
            ]),
        ];
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
    }

    fn render_summary_inspector(&self, f: &mut Frame, area: Rect, p: Palette) {
        let block = Block::default()
            .title(Span::styled(
                format!(" {} ", self.t("Host summary", "호스트 상태 요약")),
                Style::default().fg(p.muted),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(p.track))
            .style(Style::default().bg(p.panel));
        let raw_inner = block.inner(area);
        f.render_widget(block, area);
        if raw_inner.width < 12 || raw_inner.height < 4 {
            return;
        }
        let top_pad = u16::from(raw_inner.height >= 24);
        let inner = Rect::new(
            raw_inner.x + 1,
            raw_inner.y + top_pad,
            raw_inner.width.saturating_sub(2),
            raw_inner.height.saturating_sub(top_pad),
        );
        let w = inner.width as usize;
        let lbl_w = 14usize;

        let installed = self.hooks.iter().filter(|h| h.hook_installed).count();
        let total = self.hooks.len();
        let direct = resolve_advertised_direct(&self.config)
            .or_else(|| self.config.listen_addr.clone())
            .unwrap_or_else(|| self.t("Unconfigured", "미설정").to_owned());

        let section = |title: &str| -> Line<'static> {
            let used = UnicodeWidthStr::width(title) + 1;
            let rule_len = w.saturating_sub(used);
            Line::from(vec![
                Span::styled(
                    title.to_owned(),
                    Style::default().fg(p.muted).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
                Span::styled("─".repeat(rule_len), Style::default().fg(p.track)),
            ])
        };

        let field = |label: &str, value: String, color: Color| -> Line<'static> {
            Line::from(vec![
                Span::styled(pad_cells(label, lbl_w), Style::default().fg(p.muted)),
                Span::styled(value, Style::default().fg(color)),
            ])
        };

        let mut lines: Vec<Line<'static>> = vec![
            Line::from(Span::styled(
                self.config.effective_host_name(),
                Style::default().fg(p.text).add_modifier(Modifier::BOLD),
            )),
            field(
                self.t("Host ID", "호스트 ID"),
                self.identity.host_id.clone(),
                p.muted,
            ),
            field(
                self.t("Daemon", "원격 데몬"),
                match self.daemon_pid {
                    Some(pid) => match self.lang {
                        Language::English => format!("Running (pid {pid})"),
                        Language::Korean => format!("실행 중 (pid {pid})"),
                    },
                    None => self.t("Stopped", "중지됨").to_owned(),
                },
                if self.daemon_pid.is_some() {
                    p.success
                } else {
                    p.warning
                },
            ),
            field(self.t("Direct", "직접 접속"), direct, p.success),
            field(
                self.t("Relay", "릴레이"),
                self.config
                    .relay_url
                    .clone()
                    .unwrap_or_else(|| self.t("Direct LAN only", "직접 LAN 전용").to_owned()),
                if self.config.relay_url.is_some() {
                    p.success
                } else {
                    p.muted
                },
            ),
            Line::raw(""),
            section(self.t("Readiness", "연결 준비 상태")),
            field(
                self.t("Marker hooks", "마커 훅"),
                match self.lang {
                    Language::English => format!("{installed} / {total} installed"),
                    Language::Korean => format!("{installed} / {total} 설치됨"),
                },
                if installed == total && total > 0 {
                    p.success
                } else {
                    p.warning
                },
            ),
            field(
                self.t("Paired", "연결 기기"),
                match self.lang {
                    Language::English => format!("{} device(s)", self.devices.len()),
                    Language::Korean => format!("{}대 연결됨", self.devices.len()),
                },
                if self.devices.is_empty() {
                    p.muted
                } else {
                    p.success
                },
            ),
            field(
                self.t("Auto-start", "자동 시작"),
                if self.service_installed {
                    self.t("Installed", "설치됨").to_owned()
                } else {
                    self.t("Manual", "수동 실행").to_owned()
                },
                if self.service_installed {
                    p.success
                } else {
                    p.muted
                },
            ),
            field(
                self.t("Encryption", "암호화"),
                "Noise NK · E2EE".to_owned(),
                p.accent,
            ),
        ];

        if let Some(dev) = self.devices.first() {
            lines.push(Line::raw(""));
            lines.push(section(self.t("Recent device", "최근 연결 기기")));
            lines.push(field(
                self.t("Name", "기기 이름"),
                format!("{} ({})", dev.name, dev.platform),
                p.text,
            ));
            lines.push(field(
                self.t("Device ID", "기기 ID"),
                dev.device_id.clone(),
                p.muted,
            ));
        }

        if self.tab == Tab::Pair {
            lines.push(Line::raw(""));
            lines.push(section(self.t("Pairing session", "페어링 세션")));
            lines.push(field(
                self.t("Token TTL", "유효 시간"),
                self.t("5 minutes (one-time)", "5분 (1회용 토큰)")
                    .to_owned(),
                p.text,
            ));
            lines.push(field(
                self.t("QR preview", "브라우저 QR"),
                self.t("Press o for SVG", "o 키로 선명한 SVG 열기")
                    .to_owned(),
                p.muted,
            ));
        }

        lines.push(Line::raw(""));
        lines.push(section(self.t("Next step", "다음 단계")));
        if self.tab == Tab::Pair {
            lines.push(Line::from(Span::styled(
                self.t(
                    "Scan the QR code in the masil mobile app",
                    "masil 모바일 앱에서 왼쪽 QR 코드를 스캔하세요",
                ),
                Style::default().fg(p.text).add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(Span::styled(
                self.t(
                    "o  Open crisp SVG QR in browser",
                    "o  브라우저에서 선명한 SVG QR 열기",
                ),
                Style::default().fg(p.muted),
            )));
            lines.push(Line::from(Span::styled(
                self.t(
                    "r  Generate a fresh 5-minute token",
                    "r  5분 유효 페어링 토큰 재발급",
                ),
                Style::default().fg(p.muted),
            )));
        } else {
            let guide = self.t(
                "Press 3 to open QR pairing, or press o there for a browser SVG QR.",
                "3번 탭에서 QR 페어링을 열거나 o 키로 브라우저 QR을 띄울 수 있습니다.",
            );
            for wrapped in super::view::wrap(guide, w.saturating_sub(1).max(8)) {
                lines.push(Line::from(Span::styled(
                    wrapped,
                    Style::default().fg(p.muted),
                )));
            }
        }

        f.render_widget(Paragraph::new(lines), inner);
    }

    fn render_pair_body(&self, f: &mut Frame, area: Rect, p: Palette) {
        let wide = area.width >= 96;
        if wide {
            let qr_cols = self
                .qr_lines
                .first()
                .map(|l| l.chars().count() as u16)
                .unwrap_or(49);
            // Symmetric 3-cell horizontal padding on each side of the QR inside the border
            // (+2 for borders = qr_cols + 8), guaranteeing exact left/right symmetry.
            let left_w = (qr_cols + 8).min(area.width.saturating_sub(38));
            let right_w = area.width.saturating_sub(left_w + 1);
            let left = Rect::new(area.x, area.y, left_w, area.height);
            let right = Rect::new(left.right() + 1, area.y, right_w, area.height);
            self.render_qr_panel(f, left, p);
            self.render_summary_inspector(f, right, p);
            return;
        }
        self.render_qr_panel(f, area, p);
    }

    fn render_qr_panel(&self, f: &mut Frame, area: Rect, p: Palette) {
        let block = Block::default()
            .title(Span::styled(
                format!(" {} ", self.t("QR pairing", "QR 페어링")),
                Style::default()
                    .fg(p.accent)
                    .bg(p.panel)
                    .add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(p.accent))
            .style(Style::default().bg(p.panel));
        let raw_inner = block.inner(area);
        f.render_widget(block, area);
        if raw_inner.width < 12 || raw_inner.height < 4 {
            return;
        }
        let qr_w = self
            .qr_lines
            .first()
            .map(|l| l.chars().count() as u16)
            .unwrap_or(49);
        let qr_h = self.qr_lines.len() as u16;
        let qr_x = raw_inner.x + raw_inner.width.saturating_sub(qr_w) / 2;
        let qr_y = raw_inner.y + raw_inner.height.saturating_sub(qr_h) / 2;
        let qr_style = Style::default().fg(Color::White).bg(Color::Black);
        let qr_render_lines: Vec<Line> = self
            .qr_lines
            .iter()
            .take(raw_inner.height as usize)
            .map(|q| Line::from(Span::styled(q.clone(), qr_style)))
            .collect();
        f.render_widget(
            Paragraph::new(qr_render_lines),
            Rect::new(
                qr_x,
                qr_y,
                qr_w.min(raw_inner.right().saturating_sub(qr_x)),
                qr_h.min(raw_inner.height),
            ),
        );
    }

    fn render_actions(&self, f: &mut Frame, area: Rect, p: Palette) {
        let row = Rect::new(area.x + 1, area.y, area.width.saturating_sub(2), 1);
        let pairs: Vec<(&str, &str)> = match self.tab {
            Tab::Hooks => vec![
                ("Enter", self.t("Toggle hook", "훅 토글")),
                ("a", self.t("Install all", "일괄 설치")),
                ("3", self.t("QR pairing", "QR 페어링")),
                ("r", self.t("Refresh", "새로고침")),
            ],
            Tab::Network => vec![
                ("Enter", self.t("Edit / Toggle", "편집 · 전환")),
                ("3", self.t("QR pairing", "QR 페어링")),
                ("r", self.t("Refresh", "새로고침")),
            ],
            Tab::Pair => vec![
                ("o", self.t("Browser SVG QR", "브라우저 QR")),
                ("r", self.t("Regenerate token", "토큰 재발급")),
                ("4", self.t("Paired devices", "연결 기기")),
            ],
            Tab::Devices => vec![
                ("Enter", self.t("Revoke access", "접근 철회")),
                ("3", self.t("Pair device", "새 기기 연결")),
                ("r", self.t("Refresh", "새로고침")),
            ],
        };
        let mut spans: Vec<Span<'static>> = Vec::new();
        for (idx, (k, lbl)) in pairs.into_iter().enumerate() {
            if idx > 0 {
                spans.push(Span::styled(" ", Style::default().bg(p.canvas)));
            }
            spans.extend(keycap_spans(k, lbl, p, false));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), row);
    }

    fn render_footer(&self, f: &mut Frame, area: Rect, p: Palette) {
        if area.height == 0 {
            return;
        }
        let msg_color = if self.status_is_err {
            p.danger
        } else {
            p.success
        };
        let hint = self.t(
            "Arrows or j/k move; 1-4 or Tab switch tabs; Enter applies the selected row.",
            "방향키 또는 j/k 이동 · 1-4/Tab 탭 전환 · Enter 선택 항목 실행",
        );
        let line1 = if self.status_msg.is_empty() {
            Span::styled(hint, Style::default().fg(p.muted))
        } else {
            Span::styled(self.status_msg.clone(), Style::default().fg(msg_color))
        };
        f.render_widget(
            Paragraph::new(Line::from(line1)),
            Rect::new(area.x + 1, area.y, area.width.saturating_sub(2), 1),
        );

        if area.height >= 2 {
            let y = area.bottom() - 1;
            let bar_style = Style::default().fg(p.muted).bg(p.canvas);
            let bar_text = self.t(
                "Config: ~/.config/masil/remote.toml  ·  Transport: Noise NK E2EE",
                "설정: ~/.config/masil/remote.toml  ·  전송: Noise NK 종단간 암호화",
            );
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(bar_text, bar_style))),
                Rect::new(area.x + 1, y, area.width.saturating_sub(2), 1),
            );
        }
    }
}

pub(crate) fn run(args: &[String]) -> Result<i32, String> {
    let mut lang = Language::Korean;
    let mut theme = Theme::Dark;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--lang" if i + 1 < args.len() => {
                lang = match args[i + 1].as_str() {
                    "en" => Language::English,
                    "ko" => Language::Korean,
                    other => return Err(format!("unsupported --lang: {other}")),
                };
                i += 2;
            }
            "--theme" if i + 1 < args.len() => {
                theme = match args[i + 1].as_str() {
                    "dark" => Theme::Dark,
                    "light" => Theme::Light,
                    "terminal" => Theme::Terminal,
                    other => return Err(format!("unsupported --theme: {other}")),
                };
                i += 2;
            }
            other => return Err(format!("unknown remote setup option: {other}")),
        }
    }

    let mut app = App::new(lang, theme)?;
    let mut session = Session::open()?;
    while !app.quit && !session.panicked() {
        session
            .draw(|f| app.render(f))
            .map_err(|e| format!("terminal draw failed: {e}"))?;
        if event::poll(Duration::from_millis(250)).map_err(|e| e.to_string())?
            && let Event::Key(key) = event::read().map_err(|e| e.to_string())?
        {
            app.handle_key(key);
        }
    }
    Ok(0)
}
