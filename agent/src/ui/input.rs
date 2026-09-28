use super::model::{
    Action, App, ClickRecord, Drag, Effect, Filter, Focus, HitRegion, HitTarget, Overlay, Pressed,
};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use std::time::{Duration, Instant};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const DOUBLE_CLICK: Duration = Duration::from_millis(500);
const PRESS_EXPIRY: Duration = Duration::from_secs(2);
const SEARCH_LIMIT: usize = 128;
const SEARCH_BYTE_LIMIT: usize = 1_024;

impl App {
    pub fn handle(&mut self, event: Event, now: Instant) -> Vec<Effect> {
        let effects = match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => self.handle_key(key),
            Event::Mouse(mouse) => self.handle_mouse(mouse, now),
            Event::Paste(text) => {
                if self.focus == Focus::Search && self.overlay.is_none() {
                    self.insert_search(&text);
                }
                Vec::new()
            }
            Event::Resize(_, _) => {
                self.hits.clear();
                self.cancel_interactions();
                self.dirty = true;
                Vec::new()
            }
            Event::FocusLost => {
                self.cancel_interactions();
                self.dirty = true;
                Vec::new()
            }
            Event::FocusGained => {
                self.dirty = true;
                Vec::new()
            }
            _ => Vec::new(),
        };
        self.clamp_selection_and_scroll();
        effects
    }

    fn handle_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        if (self.focus != Focus::Search || self.overlay.is_some())
            && key.modifiers.intersects(
                KeyModifiers::CONTROL
                    | KeyModifiers::ALT
                    | KeyModifiers::SUPER
                    | KeyModifiers::HYPER
                    | KeyModifiers::META,
            )
        {
            return Vec::new();
        }
        if let Some(overlay) = self.overlay {
            return self.handle_overlay_key(overlay, key);
        }
        if self.focus == Focus::Search {
            return self.handle_search_key(key);
        }
        if key.code == KeyCode::F(10) && key.modifiers.contains(KeyModifiers::SHIFT) {
            self.open_context_for_selection();
            return Vec::new();
        }
        match key.code {
            KeyCode::Char('?') => {
                self.overlay = Some(Overlay::Help);
                self.help_scroll = 0;
                self.dirty = true;
                Vec::new()
            }
            KeyCode::Char('/') => {
                self.focus = Focus::Search;
                self.search_cursor = grapheme_count(&self.search);
                self.dirty = true;
                Vec::new()
            }
            KeyCode::Char('q') => vec![Effect::Quit],
            KeyCode::Char('l') => self.change_language(),
            KeyCode::Char('t') => self.change_theme(),
            KeyCode::Char('a') => self.one_effect(Action::MarkSeen),
            KeyCode::Char('g') => self.one_effect(Action::GoToPane),
            KeyCode::Char('c') => self.one_effect(Action::CopyId),
            KeyCode::Char('z') => self.one_effect(Action::Expand),
            KeyCode::Char('r') => self.one_effect(Action::Retry),
            KeyCode::Esc => {
                if self.details_open {
                    self.details_open = false;
                    self.focus = Focus::List;
                    self.dirty = true;
                    Vec::new()
                } else {
                    vec![Effect::Quit]
                }
            }
            KeyCode::Tab => {
                self.cycle_focus(!key.modifiers.contains(KeyModifiers::SHIFT));
                Vec::new()
            }
            KeyCode::BackTab => {
                self.cycle_focus(false);
                Vec::new()
            }
            KeyCode::Up | KeyCode::Char('k') if self.focus == Focus::List => {
                self.move_selection(-1);
                Vec::new()
            }
            KeyCode::Down | KeyCode::Char('j') if self.focus == Focus::List => {
                self.move_selection(1);
                Vec::new()
            }
            KeyCode::Home if self.focus == Focus::List => {
                self.select_position(0);
                Vec::new()
            }
            KeyCode::End if self.focus == Focus::List => {
                self.select_position(self.filtered_ids().len().saturating_sub(1));
                Vec::new()
            }
            KeyCode::PageUp if self.focus == Focus::List => {
                self.move_selection(-(self.list_page.max(1) as isize));
                Vec::new()
            }
            KeyCode::PageDown if self.focus == Focus::List => {
                self.move_selection(self.list_page.max(1) as isize);
                Vec::new()
            }
            KeyCode::Up | KeyCode::Char('k') if self.focus == Focus::Inspector => {
                self.scroll_inspector(-1);
                Vec::new()
            }
            KeyCode::Down | KeyCode::Char('j') if self.focus == Focus::Inspector => {
                self.scroll_inspector(1);
                Vec::new()
            }
            KeyCode::PageUp if self.focus == Focus::Inspector => {
                self.scroll_inspector(-(self.inspector_page.max(1) as isize));
                Vec::new()
            }
            KeyCode::PageDown if self.focus == Focus::Inspector => {
                self.scroll_inspector(self.inspector_page.max(1) as isize);
                Vec::new()
            }
            KeyCode::Left if self.focus == Focus::Filters => {
                self.move_filter(-1);
                Vec::new()
            }
            KeyCode::Right if self.focus == Focus::Filters => {
                self.move_filter(1);
                Vec::new()
            }
            KeyCode::Left if self.focus == Focus::Actions => {
                self.move_action(-1);
                Vec::new()
            }
            KeyCode::Right if self.focus == Focus::Actions => {
                self.move_action(1);
                Vec::new()
            }
            KeyCode::Left if self.focus == Focus::Header => {
                self.header_focus =
                    (self.header_focus + 1).min(self.header_controls.saturating_sub(1));
                self.dirty = true;
                Vec::new()
            }
            KeyCode::Right if self.focus == Focus::Header => {
                self.header_focus = self.header_focus.saturating_sub(1);
                self.dirty = true;
                Vec::new()
            }
            KeyCode::Enter => match self.focus {
                Focus::List | Focus::Inspector => self.one_effect(Action::Details),
                Focus::Filters => {
                    self.apply_filter(Filter::ALL[self.filter_focus.min(Filter::ALL.len() - 1)]);
                    Vec::new()
                }
                Focus::Actions => {
                    if self.action_focus >= self.action_controls {
                        return Vec::new();
                    }
                    let actions = self.visible_actions();
                    actions
                        .get(self.action_focus)
                        .copied()
                        .map(|action| self.one_effect(action))
                        .unwrap_or_default()
                }
                Focus::Header => self.activate_header_focus(),
                _ => Vec::new(),
            },
            _ => Vec::new(),
        }
    }

    fn handle_overlay_key(&mut self, overlay: Overlay, key: KeyEvent) -> Vec<Effect> {
        match overlay {
            Overlay::Help => match key.code {
                KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') => {
                    self.overlay = None;
                    self.dirty = true;
                    Vec::new()
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.scroll_help(-1);
                    Vec::new()
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.scroll_help(1);
                    Vec::new()
                }
                KeyCode::PageUp => {
                    self.scroll_help(-(self.help_page.max(1) as isize));
                    Vec::new()
                }
                KeyCode::PageDown => {
                    self.scroll_help(self.help_page.max(1) as isize);
                    Vec::new()
                }
                KeyCode::Home => {
                    self.help_scroll = 0;
                    self.dirty = true;
                    Vec::new()
                }
                KeyCode::End => {
                    self.help_scroll = self.help_lines.saturating_sub(self.help_page.max(1));
                    self.dirty = true;
                    Vec::new()
                }
                _ => Vec::new(),
            },
            Overlay::Context => {
                let actions = self.context_actions();
                match key.code {
                    KeyCode::Esc | KeyCode::F(10) => {
                        self.overlay = None;
                        self.dirty = true;
                        Vec::new()
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.context_selection = self.context_selection.saturating_sub(1);
                        self.dirty = true;
                        Vec::new()
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        self.context_selection =
                            (self.context_selection + 1).min(actions.len().saturating_sub(1));
                        self.dirty = true;
                        Vec::new()
                    }
                    KeyCode::Enter => {
                        self.overlay = None;
                        actions
                            .get(self.context_selection)
                            .copied()
                            .map(|action| self.one_effect(action))
                            .unwrap_or_default()
                    }
                    KeyCode::Char('a') => {
                        self.overlay = None;
                        self.one_effect(Action::MarkSeen)
                    }
                    KeyCode::Char('g') => {
                        self.overlay = None;
                        self.one_effect(Action::GoToPane)
                    }
                    _ => Vec::new(),
                }
            }
        }
    }

    fn handle_search_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        match key.code {
            KeyCode::Esc | KeyCode::Enter => {
                self.focus = Focus::List;
                self.dirty = true;
            }
            KeyCode::Left => {
                self.search_cursor = self.search_cursor.saturating_sub(1);
                self.dirty = true;
            }
            KeyCode::Right => {
                self.search_cursor = (self.search_cursor + 1).min(grapheme_count(&self.search));
                self.dirty = true;
            }
            KeyCode::Home => {
                self.search_cursor = 0;
                self.dirty = true;
            }
            KeyCode::End => {
                self.search_cursor = grapheme_count(&self.search);
                self.dirty = true;
            }
            KeyCode::Backspace => {
                if self.search_cursor > 0 {
                    remove_grapheme(&mut self.search, self.search_cursor - 1);
                    self.search_cursor -= 1;
                    self.search_changed();
                }
            }
            KeyCode::Delete => {
                if self.search_cursor < grapheme_count(&self.search) {
                    remove_grapheme(&mut self.search, self.search_cursor);
                    self.search_changed();
                }
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.search.clear();
                self.search_cursor = 0;
                self.search_view_start = 0;
                self.search_changed();
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.insert_search(&character.to_string());
            }
            _ => {}
        }
        Vec::new()
    }

    fn handle_mouse(&mut self, mouse: MouseEvent, now: Instant) -> Vec<Effect> {
        match mouse.kind {
            MouseEventKind::Moved | MouseEventKind::Drag(MouseButton::Left) => {
                if self.drag.is_some() {
                    self.update_drag(mouse.column, mouse.row);
                } else {
                    let next = self
                        .hit_at(mouse.column, mouse.row)
                        .map(|hit| hit.target.clone());
                    if self.hover != next {
                        self.hover = next;
                        self.dirty = true;
                    }
                }
                Vec::new()
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.left_press(mouse.column, mouse.row, now)
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.left_release(mouse.column, mouse.row, now)
            }
            MouseEventKind::Down(MouseButton::Right) => {
                self.right_press(mouse.column, mouse.row);
                Vec::new()
            }
            MouseEventKind::Down(_) => {
                self.cancel_interactions();
                self.dirty = true;
                Vec::new()
            }
            MouseEventKind::ScrollUp => {
                self.wheel(mouse.column, mouse.row, -3);
                Vec::new()
            }
            MouseEventKind::ScrollDown => {
                self.wheel(mouse.column, mouse.row, 3);
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn left_press(&mut self, column: u16, row: u16, now: Instant) -> Vec<Effect> {
        let hit = self.hit_at(column, row).cloned();
        if self.overlay.is_some()
            && !hit
                .as_ref()
                .is_some_and(|hit| is_overlay_target(&hit.target))
        {
            self.overlay = None;
            self.cancel_interactions();
            self.dirty = true;
            return Vec::new();
        }
        let Some(hit) = hit else {
            self.cancel_interactions();
            return Vec::new();
        };
        let related_double = matches!(&hit.target, HitTarget::Row { id, .. }
            if self.last_click.as_ref().is_some_and(|click| click.id == *id));
        self.pressed = None;
        self.drag = None;
        if !related_double {
            self.last_click = None;
        }
        match &hit.target {
            HitTarget::Row { id, .. } => {
                self.selected_id = Some(id.clone());
                self.focus = Focus::List;
                self.dirty = true;
                let double = self.last_click.as_ref().is_some_and(|click| {
                    click.id == *id
                        && now.saturating_duration_since(click.at) <= DOUBLE_CLICK
                        && click.column.abs_diff(column) <= 1
                        && click.row.abs_diff(row) <= 1
                });
                self.last_click = if double {
                    None
                } else {
                    Some(ClickRecord {
                        id: id.clone(),
                        column,
                        row,
                        at: now,
                    })
                };
                if double {
                    return self.one_effect(Action::GoToPane);
                }
                self.pressed = Some(Pressed {
                    target: hit.target,
                    at: now,
                    identity: self.selected_identity_revision(),
                });
            }
            HitTarget::Search => {
                self.focus = Focus::Search;
                self.search_cursor = self.search_cursor_for_cell(column, hit.rect);
                self.pressed = Some(Pressed {
                    target: hit.target,
                    at: now,
                    identity: None,
                });
                self.dirty = true;
            }
            HitTarget::Divider => {
                self.drag = Some(Drag::Divider {
                    start_x: column,
                    start_width: self.divider_width,
                });
                self.dirty = true;
            }
            HitTarget::ListThumb => self.start_thumb_drag(HitTarget::ListThumb, row, hit.rect),
            HitTarget::InspectorThumb => {
                self.start_thumb_drag(HitTarget::InspectorThumb, row, hit.rect)
            }
            HitTarget::HelpThumb => self.start_thumb_drag(HitTarget::HelpThumb, row, hit.rect),
            HitTarget::ListTrack => self.page_track(HitTarget::ListTrack, row),
            HitTarget::InspectorTrack => self.page_track(HitTarget::InspectorTrack, row),
            HitTarget::HelpTrack => self.page_track(HitTarget::HelpTrack, row),
            HitTarget::InspectorBody => {
                self.focus = Focus::Inspector;
                self.dirty = true;
            }
            HitTarget::HelpBody | HitTarget::OverlaySurface => {}
            _ => {
                let identity =
                    if matches!(hit.target, HitTarget::Action(_) | HitTarget::ContextItem(_)) {
                        self.selected_identity_revision()
                    } else {
                        None
                    };
                self.pressed = Some(Pressed {
                    target: hit.target,
                    at: now,
                    identity,
                });
                self.dirty = true;
            }
        }
        Vec::new()
    }

    fn left_release(&mut self, column: u16, row: u16, now: Instant) -> Vec<Effect> {
        if self.drag.take().is_some() {
            self.pressed = None;
            self.dirty = true;
            return Vec::new();
        }
        let Some(pressed) = self.pressed.take() else {
            return Vec::new();
        };
        if now.saturating_duration_since(pressed.at) > PRESS_EXPIRY {
            self.dirty = true;
            return Vec::new();
        }
        if matches!(
            pressed.target,
            HitTarget::Action(_) | HitTarget::ContextItem(_)
        ) && pressed.identity != self.selected_identity_revision()
        {
            self.dirty = true;
            return Vec::new();
        }
        let same = self
            .hit_at(column, row)
            .is_some_and(|hit| hit.target == pressed.target);
        if !same {
            self.dirty = true;
            return Vec::new();
        }
        self.activate_target(pressed.target)
    }

    fn right_press(&mut self, column: u16, row: u16) {
        self.cancel_interactions();
        let target = self.hit_at(column, row).map(|hit| hit.target.clone());
        if self.overlay.is_some() {
            if !target.as_ref().is_some_and(is_overlay_target) {
                self.overlay = None;
                self.dirty = true;
            }
            return;
        }
        if let Some(HitTarget::Row { id, .. }) = target {
            self.selected_id = Some(id);
            self.context_anchor = (column, row);
            self.context_selection = 0;
            self.overlay = Some(Overlay::Context);
            self.focus = Focus::List;
            self.dirty = true;
        }
    }

    fn activate_target(&mut self, target: HitTarget) -> Vec<Effect> {
        match target {
            HitTarget::Language => self.change_language(),
            HitTarget::Theme => self.change_theme(),
            HitTarget::Help => {
                self.overlay = Some(Overlay::Help);
                self.help_scroll = 0;
                self.dirty = true;
                Vec::new()
            }
            HitTarget::Close => vec![Effect::Quit],
            HitTarget::Filter(filter) => {
                self.apply_filter(filter);
                Vec::new()
            }
            HitTarget::ClearSearch => {
                self.search.clear();
                self.search_cursor = 0;
                self.search_view_start = 0;
                self.search_changed();
                Vec::new()
            }
            HitTarget::Action(action) => self.one_effect(action),
            HitTarget::ContextItem(action) => {
                self.overlay = None;
                self.one_effect(action)
            }
            HitTarget::DismissOverlay => {
                self.overlay = None;
                self.dirty = true;
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn wheel(&mut self, column: u16, row: u16, amount: isize) {
        let target = self.hit_at(column, row).map(|hit| hit.target.clone());
        if let Some(overlay) = self.overlay {
            if overlay == Overlay::Help {
                self.scroll_help(amount);
            }
            return;
        }
        match target {
            Some(HitTarget::Row { .. } | HitTarget::ListTrack | HitTarget::ListThumb) => {
                self.scroll_list(amount)
            }
            Some(
                HitTarget::InspectorBody | HitTarget::InspectorTrack | HitTarget::InspectorThumb,
            ) => self.scroll_inspector(amount),
            _ => {}
        }
    }

    fn update_drag(&mut self, column: u16, row: u16) {
        let Some(drag) = self.drag.clone() else {
            return;
        };
        match drag {
            Drag::Divider {
                start_x,
                start_width,
            } => {
                let delta = i32::from(column) - i32::from(start_x);
                let maximum = self.last_area.width.saturating_sub(35).max(34);
                self.divider_width =
                    (i32::from(start_width) + delta).clamp(34, i32::from(maximum)) as u16;
                self.dirty = true;
            }
            Drag::ListThumb { grab } => {
                self.drag_scroll(row.saturating_sub(grab), HitTarget::ListTrack)
            }
            Drag::InspectorThumb { grab } => {
                self.drag_scroll(row.saturating_sub(grab), HitTarget::InspectorTrack)
            }
            Drag::HelpThumb { grab } => {
                self.drag_scroll(row.saturating_sub(grab), HitTarget::HelpTrack)
            }
        }
    }

    fn start_thumb_drag(&mut self, target: HitTarget, row: u16, rect: Rect) {
        let grab = row.saturating_sub(rect.y);
        self.drag = Some(match target {
            HitTarget::ListThumb => Drag::ListThumb { grab },
            HitTarget::InspectorThumb => Drag::InspectorThumb { grab },
            _ => Drag::HelpThumb { grab },
        });
        self.dirty = true;
    }

    fn drag_scroll(&mut self, thumb_top: u16, track_target: HitTarget) {
        let Some(track) = self
            .hits
            .iter()
            .find(|hit| hit.target == track_target)
            .map(|hit| hit.rect)
        else {
            return;
        };
        let (total, page) = match track_target {
            HitTarget::ListTrack => (self.filtered_ids().len(), self.list_page),
            HitTarget::InspectorTrack => (self.inspector_lines, self.inspector_page),
            _ => (self.help_lines, self.help_page),
        };
        let thumb_height =
            (track.height as usize * page / total.max(1)).clamp(1, track.height as usize);
        let travel = (track.height as usize).saturating_sub(thumb_height);
        let position = (thumb_top.saturating_sub(track.y) as usize).min(travel);
        let track_span = travel.max(1);
        match track_target {
            HitTarget::ListTrack => {
                let max = self
                    .filtered_ids()
                    .len()
                    .saturating_sub(self.list_page.max(1));
                self.list_scroll = position.saturating_mul(max) / track_span;
            }
            HitTarget::InspectorTrack => {
                let max = self
                    .inspector_lines
                    .saturating_sub(self.inspector_page.max(1));
                self.inspector_scroll = position.saturating_mul(max) / track_span;
            }
            _ => {
                let max = self.help_lines.saturating_sub(self.help_page.max(1));
                self.help_scroll = position.saturating_mul(max) / track_span;
            }
        }
        self.dirty = true;
    }

    fn page_track(&mut self, target: HitTarget, row: u16) {
        let thumb_target = match target {
            HitTarget::ListTrack => HitTarget::ListThumb,
            HitTarget::InspectorTrack => HitTarget::InspectorThumb,
            _ => HitTarget::HelpThumb,
        };
        let thumb = self
            .hits
            .iter()
            .find(|hit| hit.target == thumb_target)
            .map(|hit| hit.rect);
        let direction = if thumb.is_some_and(|rect| row < rect.y) {
            -1
        } else {
            1
        };
        match target {
            HitTarget::ListTrack => self.scroll_list(direction * self.list_page.max(1) as isize),
            HitTarget::InspectorTrack => {
                self.scroll_inspector(direction * self.inspector_page.max(1) as isize)
            }
            _ => self.scroll_help(direction * self.help_page.max(1) as isize),
        }
    }

    fn hit_at(&self, column: u16, row: u16) -> Option<&HitRegion> {
        self.hits
            .iter()
            .filter(|hit| contains(hit.rect, column, row))
            .max_by_key(|hit| hit.z)
    }

    fn search_cursor_for_cell(&self, column: u16, rect: Rect) -> usize {
        let wanted = column.saturating_sub(rect.x) as usize;
        let mut width = 0;
        for (index, grapheme) in self
            .search
            .graphemes(true)
            .skip(self.search_view_start)
            .enumerate()
        {
            let next = width + UnicodeWidthStr::width(grapheme);
            if wanted < next {
                return (self.search_view_start + index).min(grapheme_count(&self.search));
            }
            width = next;
        }
        grapheme_count(&self.search)
    }

    fn move_selection(&mut self, delta: isize) {
        let ids = self.filtered_ids();
        if ids.is_empty() {
            return;
        }
        let current = self
            .selected_id
            .as_ref()
            .and_then(|id| ids.iter().position(|candidate| candidate == id))
            .unwrap_or(0);
        let next = current
            .saturating_add_signed(delta)
            .min(ids.len().saturating_sub(1));
        self.selected_id = ids.get(next).cloned();
        self.reveal_selection();
        self.dirty = true;
    }

    fn select_position(&mut self, position: usize) {
        self.selected_id = self.filtered_ids().get(position).cloned();
        self.reveal_selection();
        self.dirty = true;
    }

    fn scroll_list(&mut self, amount: isize) {
        let max = self
            .filtered_ids()
            .len()
            .saturating_sub(self.list_page.max(1));
        self.list_scroll = self.list_scroll.saturating_add_signed(amount).min(max);
        self.dirty = true;
    }

    fn scroll_inspector(&mut self, amount: isize) {
        let max = self
            .inspector_lines
            .saturating_sub(self.inspector_page.max(1));
        self.inspector_scroll = self.inspector_scroll.saturating_add_signed(amount).min(max);
        self.dirty = true;
    }

    fn scroll_help(&mut self, amount: isize) {
        let max = self.help_lines.saturating_sub(self.help_page.max(1));
        self.help_scroll = self.help_scroll.saturating_add_signed(amount).min(max);
        self.dirty = true;
    }

    fn move_filter(&mut self, delta: isize) {
        self.filter_focus = self
            .filter_focus
            .saturating_add_signed(delta)
            .min(Filter::ALL.len() - 1);
        self.apply_filter(Filter::ALL[self.filter_focus]);
    }

    fn move_action(&mut self, delta: isize) {
        let count = self.action_controls;
        if count > 0 {
            self.action_focus = self
                .action_focus
                .saturating_add_signed(delta)
                .min(count - 1);
            self.dirty = true;
        }
    }

    fn apply_filter(&mut self, filter: Filter) {
        self.filter = filter;
        self.filter_focus = Filter::ALL
            .iter()
            .position(|candidate| *candidate == filter)
            .unwrap_or_default();
        self.list_scroll = 0;
        self.reveal_selection();
        self.toast = None;
        self.dirty = true;
    }

    fn search_changed(&mut self) {
        self.list_scroll = 0;
        self.reveal_selection();
        self.toast = None;
        self.dirty = true;
    }

    fn insert_search(&mut self, text: &str) {
        let remaining_graphemes = SEARCH_LIMIT.saturating_sub(grapheme_count(&self.search));
        let mut remaining_bytes = SEARCH_BYTE_LIMIT.saturating_sub(self.search.len());
        let mut clean = String::new();
        for grapheme in text.graphemes(true).take(remaining_graphemes) {
            if grapheme.chars().any(char::is_control) {
                continue;
            }
            if grapheme.len() > remaining_bytes {
                break;
            }
            clean.push_str(grapheme);
            remaining_bytes -= grapheme.len();
        }
        if clean.is_empty() {
            return;
        }
        let byte = grapheme_byte(&self.search, self.search_cursor);
        self.search.insert_str(byte, &clean);
        self.search_cursor += grapheme_count(&clean);
        self.search_changed();
    }

    fn cycle_focus(&mut self, forward: bool) {
        let mut order = vec![Focus::Header, Focus::Filters, Focus::Search, Focus::List];
        if self.last_area.width >= 100 || self.details_open {
            order.push(Focus::Inspector);
        }
        order.push(Focus::Actions);
        let current = order
            .iter()
            .position(|focus| *focus == self.focus)
            .unwrap_or(0);
        let next = if forward {
            (current + 1) % order.len()
        } else {
            (current + order.len() - 1) % order.len()
        };
        self.focus = order[next];
        self.dirty = true;
    }

    fn change_language(&mut self) -> Vec<Effect> {
        self.language = self.language.next();
        self.toast = None;
        self.dirty = true;
        vec![Effect::Preferences {
            language: self.language,
            theme: self.theme,
        }]
    }

    fn change_theme(&mut self) -> Vec<Effect> {
        self.theme = self.theme.next();
        self.toast = None;
        self.dirty = true;
        vec![Effect::Preferences {
            language: self.language,
            theme: self.theme,
        }]
    }

    fn one_effect(&mut self, action: Action) -> Vec<Effect> {
        self.effect_for(action).into_iter().collect()
    }

    fn activate_header_focus(&mut self) -> Vec<Effect> {
        if self.header_focus >= self.header_controls {
            return Vec::new();
        }
        match self.header_focus {
            0 => vec![Effect::Quit],
            1 => {
                self.overlay = Some(Overlay::Help);
                self.help_scroll = 0;
                self.dirty = true;
                Vec::new()
            }
            2 => self.change_theme(),
            _ => self.change_language(),
        }
    }

    fn selected_identity_revision(&self) -> Option<(String, String)> {
        self.selected()
            .map(|row| (row.id.clone(), row.attention_revision.clone()))
    }

    pub(crate) fn visible_actions(&self) -> Vec<Action> {
        let mut actions = vec![Action::MarkSeen, Action::GoToPane, Action::Details];
        if self.compact && self.native_available {
            actions.push(Action::Expand);
        }
        if !self.connected {
            actions.push(Action::Retry);
        }
        actions
    }

    pub(crate) fn context_actions(&self) -> Vec<Action> {
        vec![
            Action::MarkSeen,
            Action::GoToPane,
            Action::Details,
            Action::CopyId,
        ]
    }

    fn open_context_for_selection(&mut self) {
        if self.selected_id.is_none() {
            return;
        }
        let anchor = self
            .hits
            .iter()
            .find(|hit| {
                matches!(&hit.target, HitTarget::Row { id, .. }
                    if self.selected_id.as_ref() == Some(id))
            })
            .map(|hit| (hit.rect.x + hit.rect.width / 2, hit.rect.y + 1))
            .unwrap_or((self.last_area.width / 2, self.last_area.height / 2));
        self.context_anchor = anchor;
        self.context_selection = 0;
        self.overlay = Some(Overlay::Context);
        self.last_click = None;
        self.dirty = true;
    }
}

fn is_overlay_target(target: &HitTarget) -> bool {
    matches!(
        target,
        HitTarget::OverlaySurface
            | HitTarget::ContextItem(_)
            | HitTarget::HelpTrack
            | HitTarget::HelpThumb
            | HitTarget::HelpBody
            | HitTarget::DismissOverlay
    )
}

fn contains(rect: Rect, column: u16, row: u16) -> bool {
    rect.width > 0
        && rect.height > 0
        && column >= rect.x
        && column < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height)
}

fn grapheme_count(text: &str) -> usize {
    text.graphemes(true).count()
}

fn grapheme_byte(text: &str, index: usize) -> usize {
    text.grapheme_indices(true)
        .nth(index)
        .map(|(byte, _)| byte)
        .unwrap_or(text.len())
}

fn remove_grapheme(text: &mut String, index: usize) {
    let start = grapheme_byte(text, index);
    let end = grapheme_byte(text, index + 1);
    text.replace_range(start..end, "");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::{Language, Theme};

    #[test]
    fn scrollbar_travel_accounts_for_thumb_height_and_cancels_on_resize() {
        let mut app = App::new(false, Language::English, Theme::Dark);
        let ids: Vec<_> = (0..20).map(|i| format!("row-{i:02}")).collect();
        app.apply_snapshot(snapshot(
            &ids.iter().map(String::as_str).collect::<Vec<_>>(),
        ));
        app.list_page = 10;
        app.hits = vec![
            HitRegion {
                rect: Rect::new(10, 5, 1, 10),
                target: HitTarget::ListTrack,
                z: 1,
            },
            HitRegion {
                rect: Rect::new(10, 5, 1, 5),
                target: HitTarget::ListThumb,
                z: 2,
            },
        ];
        app.left_press(10, 7, Instant::now());
        app.update_drag(40, 12); // Grabbed two cells into a five-cell thumb.
        assert_eq!(app.list_scroll, 10);
        app.update_drag(40, 0);
        assert_eq!(app.list_scroll, 0);
        app.handle(Event::Resize(80, 24), Instant::now());
        assert!(app.drag.is_none());
        assert!(app.hits.is_empty());
    }

    #[test]
    fn focus_loss_and_fresh_press_cancel_old_capture() {
        let mut app = App::new(false, Language::English, Theme::Dark);
        app.drag = Some(Drag::Divider {
            start_x: 40,
            start_width: 40,
        });
        app.handle(Event::FocusLost, Instant::now());
        assert!(app.drag.is_none());
        app.drag = Some(Drag::Divider {
            start_x: 40,
            start_width: 40,
        });
        app.left_press(0, 0, Instant::now());
        assert!(app.drag.is_none());
    }
    use crossterm::event::{KeyEvent, MouseEvent};
    use serde_json::json;

    fn snapshot(ids: &[&str]) -> serde_json::Value {
        json!({
            "epoch": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "revision": "1",
            "observations": ids.iter().enumerate().map(|(index, id)| json!({
                "id": id,
                "source_id": "local",
                "session_id": format!("session_{index}"),
                "pane_id": format!("%{index}"),
                "native": {"exists": true, "activity": "idle", "last_activity": null,
                    "attention": "question", "permission_count": 0, "question_count": 1,
                    "freshness": "fresh", "observed_at_ms": 1},
                "core": {"process": "running", "pty_generation": "7", "freshness": "fresh"},
                "binding": "explicit_unverified", "frontend_verified": false,
                "attention": {"revision": format!("r{index}"), "acknowledged": false,
                    "pending": true, "available": true}
            })).collect::<Vec<_>>()
        })
    }

    #[test]
    fn selection_survives_reordering_by_identity() {
        let mut app = App::new(false, Language::English, Theme::Dark);
        app.apply_snapshot(snapshot(&["alpha", "beta"]));
        app.selected_id = Some("beta".into());
        app.apply_snapshot(snapshot(&["beta", "alpha"]));
        assert_eq!(app.selected_id.as_deref(), Some("beta"));
    }

    #[test]
    fn unicode_search_edits_graphemes() {
        let mut app = App::new(false, Language::English, Theme::Dark);
        app.focus = Focus::Search;
        app.handle(Event::Paste("한글e\u{301}".into()), Instant::now());
        assert_eq!(app.search_cursor, 3);
        app.handle(
            Event::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)),
            Instant::now(),
        );
        assert_eq!(app.search, "한글");
    }

    #[test]
    fn oversized_single_grapheme_is_rejected_whole() {
        let mut app = App::new(false, Language::English, Theme::Dark);
        app.focus = Focus::Search;
        let oversized = format!("a{}", "\u{301}".repeat(2_000));
        app.handle(Event::Paste(oversized), Instant::now());
        assert!(app.search.is_empty());
        assert!(app.search.len() <= SEARCH_BYTE_LIMIT);
    }

    #[test]
    fn button_requires_release_on_same_live_target() {
        let mut app = App::new(false, Language::English, Theme::Dark);
        app.hits.push(HitRegion {
            rect: Rect::new(2, 2, 8, 1),
            target: HitTarget::Close,
            z: 1,
        });
        let now = Instant::now();
        assert!(
            app.handle(
                Event::Mouse(MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: 3,
                    row: 2,
                    modifiers: KeyModifiers::NONE,
                }),
                now,
            )
            .is_empty()
        );
        assert!(
            app.handle(
                Event::Mouse(MouseEvent {
                    kind: MouseEventKind::Up(MouseButton::Left),
                    column: 12,
                    row: 2,
                    modifiers: KeyModifiers::NONE,
                }),
                now,
            )
            .is_empty()
        );
    }

    #[test]
    fn ack_captures_displayed_epoch_and_revision() {
        let mut app = App::new(false, Language::English, Theme::Dark);
        app.apply_snapshot(snapshot(&["alpha"]));
        app.set_connection(true, None);
        for modifier in [
            KeyModifiers::CONTROL,
            KeyModifiers::ALT,
            KeyModifiers::SUPER,
        ] {
            assert!(
                app.handle(
                    Event::Key(KeyEvent::new(KeyCode::Char('a'), modifier)),
                    Instant::now(),
                )
                .is_empty()
            );
            assert!(app.ack_in_flight.is_none());
        }
        assert_eq!(
            app.handle(
                Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
                Instant::now(),
            ),
            vec![Effect::Ack {
                id: "alpha".into(),
                epoch: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                revision: "r0".into(),
            }]
        );
        assert!(app.ack_in_flight.is_some());
    }

    #[test]
    fn copy_shortcut_targets_tmux_buffer_effect() {
        let mut app = App::new(false, Language::English, Theme::Dark);
        app.apply_snapshot(snapshot(&["alpha"]));
        app.set_connection(true, None);
        app.set_native_available(true);
        assert_eq!(
            app.handle(
                Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)),
                Instant::now(),
            ),
            vec![Effect::CopyId {
                value: "alpha".into(),
                epoch: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            }]
        );
    }

    #[test]
    fn disabled_press_cannot_act_on_a_newly_arrived_selection() {
        let mut app = App::new(false, Language::English, Theme::Dark);
        app.hits = vec![HitRegion {
            rect: Rect::new(0, 0, 12, 1),
            target: HitTarget::Action(Action::MarkSeen),
            z: 1,
        }];
        let now = Instant::now();
        app.left_press(1, 0, now);
        app.apply_snapshot(snapshot(&["alpha"]));
        app.set_connection(true, None);
        assert!(app.left_release(1, 0, now).is_empty());
        assert!(app.ack_in_flight.is_none());
    }

    #[test]
    fn snapshot_revision_change_cancels_pressed_action() {
        let mut app = App::new(false, Language::English, Theme::Dark);
        app.apply_snapshot(snapshot(&["alpha"]));
        app.set_connection(true, None);
        app.hits.push(HitRegion {
            rect: Rect::new(1, 1, 10, 1),
            target: HitTarget::Action(Action::MarkSeen),
            z: 1,
        });
        let now = Instant::now();
        app.handle(
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 2,
                row: 1,
                modifiers: KeyModifiers::NONE,
            }),
            now,
        );
        let mut changed = snapshot(&["alpha"]);
        changed["observations"][0]["attention"]["revision"] = json!("new");
        app.apply_snapshot(changed);
        assert!(
            app.handle(
                Event::Mouse(MouseEvent {
                    kind: MouseEventKind::Up(MouseButton::Left),
                    column: 2,
                    row: 1,
                    modifiers: KeyModifiers::NONE,
                }),
                now + Duration::from_millis(10),
            )
            .is_empty()
        );
        assert!(app.ack_in_flight.is_none());
    }

    #[test]
    fn selection_change_between_action_press_and_release_cancels_activation() {
        let mut app = App::new(false, Language::English, Theme::Dark);
        app.apply_snapshot(snapshot(&["alpha", "beta"]));
        app.set_connection(true, None);
        app.hits.push(HitRegion {
            rect: Rect::new(1, 1, 10, 1),
            target: HitTarget::Action(Action::MarkSeen),
            z: 1,
        });
        let now = Instant::now();
        app.handle(
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 2,
                row: 1,
                modifiers: KeyModifiers::NONE,
            }),
            now,
        );
        app.handle(
            Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)),
            now + Duration::from_millis(5),
        );
        let effects = app.handle(
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                column: 2,
                row: 1,
                modifiers: KeyModifiers::NONE,
            }),
            now + Duration::from_millis(10),
        );
        assert!(effects.is_empty());
        assert_eq!(app.selected_id.as_deref(), Some("beta"));
        assert!(app.ack_in_flight.is_none());
    }

    #[test]
    fn filter_reselection_between_press_and_release_cancels_activation() {
        let mut app = App::new(false, Language::English, Theme::Dark);
        app.apply_snapshot(snapshot(&["alpha", "beta"]));
        app.set_connection(true, None);
        app.apply_filter(Filter::Attention);
        app.hits.push(HitRegion {
            rect: Rect::new(1, 1, 10, 1),
            target: HitTarget::Action(Action::MarkSeen),
            z: 1,
        });
        let now = Instant::now();
        app.handle(
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 2,
                row: 1,
                modifiers: KeyModifiers::NONE,
            }),
            now,
        );
        let mut changed = snapshot(&["alpha", "beta"]);
        changed["observations"][0]["attention"]["pending"] = json!(false);
        changed["observations"][0]["attention"]["acknowledged"] = json!(true);
        app.apply_snapshot(changed);
        assert_eq!(app.selected_id.as_deref(), Some("beta"));
        let effects = app.handle(
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                column: 2,
                row: 1,
                modifiers: KeyModifiers::NONE,
            }),
            now + Duration::from_millis(10),
        );
        assert!(effects.is_empty());
        assert!(app.ack_in_flight.is_none());
    }

    #[test]
    fn double_click_is_keyed_to_row_identity() {
        let mut app = App::new(false, Language::English, Theme::Dark);
        app.apply_snapshot(snapshot(&["alpha"]));
        app.set_connection(true, None);
        app.set_native_available(true);
        app.hits.push(HitRegion {
            rect: Rect::new(1, 1, 20, 2),
            target: HitTarget::Row {
                id: "alpha".into(),
                revision: "r0".into(),
            },
            z: 1,
        });
        let click = |kind| {
            Event::Mouse(MouseEvent {
                kind,
                column: 2,
                row: 1,
                modifiers: KeyModifiers::NONE,
            })
        };
        let now = Instant::now();
        assert!(
            app.handle(click(MouseEventKind::Down(MouseButton::Left)), now)
                .is_empty()
        );
        app.handle(
            click(MouseEventKind::Up(MouseButton::Left)),
            now + Duration::from_millis(20),
        );
        assert!(matches!(
            app.handle(
                click(MouseEventKind::Down(MouseButton::Left)),
                now + Duration::from_millis(100),
            )
            .as_slice(),
            [Effect::Navigate { id, .. }] if id == "alpha"
        ));
    }

    #[test]
    fn outside_overlay_click_closes_and_is_consumed() {
        let mut app = App::new(false, Language::English, Theme::Dark);
        app.overlay = Some(Overlay::Help);
        app.hits.push(HitRegion {
            rect: Rect::new(1, 1, 8, 1),
            target: HitTarget::Close,
            z: 1,
        });
        let effects = app.handle(
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 2,
                row: 1,
                modifiers: KeyModifiers::NONE,
            }),
            Instant::now(),
        );
        assert!(effects.is_empty());
        assert_eq!(app.overlay, None);
    }

    #[test]
    fn wheel_scroll_does_not_move_or_reveal_selection() {
        let mut app = App::new(false, Language::English, Theme::Dark);
        app.apply_snapshot(snapshot(&["a", "b", "c", "d", "e", "f"]));
        app.list_page = 2;
        app.hits.push(HitRegion {
            rect: Rect::new(0, 0, 20, 6),
            target: HitTarget::Row {
                id: "a".into(),
                revision: "r0".into(),
            },
            z: 1,
        });
        app.handle(
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 2,
                row: 2,
                modifiers: KeyModifiers::NONE,
            }),
            Instant::now(),
        );
        assert_eq!(app.selected_id.as_deref(), Some("a"));
        assert_eq!(app.list_scroll, 3);
    }

    #[test]
    fn disconnected_unavailable_filter_includes_retained_rows() {
        let mut app = App::new(false, Language::English, Theme::Dark);
        app.apply_snapshot(snapshot(&["alpha", "beta"]));
        app.set_connection(false, Some("offline".into()));
        app.apply_filter(Filter::Unavailable);
        assert_eq!(app.filtered_ids(), vec!["alpha", "beta"]);
    }

    #[test]
    fn inspector_body_owns_wheel_and_click_focus() {
        let mut app = App::new(false, Language::English, Theme::Dark);
        app.inspector_lines = 20;
        app.inspector_page = 4;
        app.focus = Focus::List;
        app.hits.push(HitRegion {
            rect: Rect::new(5, 5, 20, 8),
            target: HitTarget::InspectorBody,
            z: 1,
        });
        let now = Instant::now();
        app.handle(
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 6,
                row: 6,
                modifiers: KeyModifiers::NONE,
            }),
            now,
        );
        assert_eq!(app.inspector_scroll, 3);
        app.handle(
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 6,
                row: 6,
                modifiers: KeyModifiers::NONE,
            }),
            now,
        );
        assert_eq!(app.focus, Focus::Inspector);
    }
}
