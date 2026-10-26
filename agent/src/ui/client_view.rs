//! The agent desk's state for one terminal client (D1, RL-12): its filter
//! tab, selected agent and list scroll, kept in the server's operation store
//! under the client's terminal profile, and the client's own saved list view.
//! A desk that cannot tell its client apart (several clients attached, or a
//! terminal without a stable identity) shows the server's view and writes
//! nothing. The compact sidebar keeps nothing either: a sidebar in each
//! window would share the full desk's row.

use super::model::{App, Filter, Language};
use crate::managed::Manager;
use crossterm::event::{Event, KeyEventKind, MouseEventKind};
use futures_util::FutureExt;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;

/// Selection and scroll are written once they have not changed this long.
const SETTLE: Duration = Duration::from_secs(2);
/// After a failed write, the next one waits this long.
const RETRY: Duration = Duration::from_secs(10);
const MAX_SELECTED: usize = 256;
const MAX_SCROLL: usize = 100_000;

/// What the desk keeps for a client.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct UiState {
    filter: Filter,
    selected: Option<String>,
    scroll: usize,
}

impl UiState {
    /// What a client row holds; anything missing or unknown is the default.
    fn parse(text: &str) -> Self {
        let value: Value = serde_json::from_str(text).unwrap_or(Value::Null);
        let filter = match value["filter"].as_str() {
            Some("attention") => Filter::Attention,
            Some("working") => Filter::Working,
            Some("unavailable") => Filter::Unavailable,
            _ => Filter::All,
        };
        let selected = value["selected"]
            .as_str()
            .filter(|id| valid_selection(id))
            .map(str::to_owned);
        let scroll = value["scroll"]
            .as_u64()
            .map_or(0, |scroll| scroll.min(MAX_SCROLL as u64) as usize);
        Self {
            filter,
            selected,
            scroll,
        }
    }

    fn encode(&self) -> String {
        let filter = match self.filter {
            Filter::All => "all",
            Filter::Attention => "attention",
            Filter::Working => "working",
            Filter::Unavailable => "unavailable",
        };
        json!({"filter": filter, "selected": self.selected, "scroll": self.scroll}).to_string()
    }

    fn of(app: &App) -> Self {
        Self {
            filter: app.filter,
            selected: app.selected_id.clone().filter(|id| valid_selection(id)),
            scroll: app.list_scroll.min(MAX_SCROLL),
        }
    }
}

fn valid_selection(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_SELECTED && !id.chars().any(char::is_control)
}

fn text(language: Language, english: &str, korean: &str) -> String {
    match language {
        Language::English => english,
        Language::Korean => korean,
    }
    .to_owned()
}

pub(super) struct ClientView {
    socket: PathBuf,
    /// None: this desk keeps nothing.
    profile: Option<String>,
    /// What the store holds (as loaded, or as last written).
    stored: UiState,
    /// The selection and scroll to put back once the first rows arrive.
    /// Any input from the user before that drops it: their choice wins.
    restore: Option<UiState>,
    /// The scroll to put back once the rows with the restored selection are
    /// shown.
    restored: Option<UiState>,
    /// Shown when the desk opens, and again once the first rows replace
    /// the toast the opening showed.
    notice: Option<String>,
    /// The state last seen.
    seen: UiState,
    /// Whether the user gave input since the last turn of the desk loop. A
    /// change of state in a turn with input is the user's; any other change
    /// (an agent that is gone, a list that grew) is the desk's own and is
    /// not written.
    touched: bool,
    /// When the user's last change was made, while it is not written yet.
    changed_at: Option<Instant>,
    writing: Option<(UiState, JoinHandle<Result<(), String>>)>,
    retry_at: Option<Instant>,
    warned: bool,
}

impl ClientView {
    /// Finds the desk's client and what is kept for it. Returns the state,
    /// the client's own saved list view for the fleet (None: the server's),
    /// and a notice for the footer when nothing is kept.
    pub(super) async fn open(
        manager: &Manager,
        language: Language,
        compact: bool,
    ) -> (Self, Option<String>, Option<String>) {
        let mut state = Self {
            socket: manager.native.socket.clone(),
            profile: None,
            stored: UiState::default(),
            restore: None,
            restored: None,
            notice: None,
            seen: UiState::default(),
            touched: false,
            changed_at: None,
            writing: None,
            retry_at: None,
            warned: false,
        };
        if compact {
            return (state, None, None);
        }
        let client = match manager.client_view().await {
            Ok(client) => client,
            Err(error) => {
                let notice = if error.contains("several clients") {
                    text(
                        language,
                        "Several clients are attached; this desk does not keep its view",
                        "client가 여럿이라 보기를 저장하지 않습니다",
                    )
                } else if error.starts_with("client_ambiguous") {
                    text(
                        language,
                        "No attached client; this desk does not keep its view",
                        "붙은 client가 없어 보기를 저장하지 않습니다",
                    )
                } else {
                    let error = crate::managed::failure::human(&error).to_owned();
                    text(
                        language,
                        &format!("The desk view is not kept: {error}"),
                        &format!("보기를 저장하지 않습니다: {error}"),
                    )
                };
                state.notice = Some(notice.clone());
                return (state, None, Some(notice));
            }
        };
        let Some(profile) = client.profile.clone() else {
            let notice = text(
                language,
                "This terminal gives no stable identity; set MASIL_CLIENT_KEY to keep the desk view",
                "터미널을 구별할 수 없어 보기를 저장하지 않습니다 (MASIL_CLIENT_KEY)",
            );
            state.notice = Some(notice.clone());
            return (state, None, Some(notice));
        };
        let (view, notice) = match client.check() {
            Ok(()) => (client.view_text().map(str::to_owned), None),
            Err(_) => (
                None,
                Some(text(
                    language,
                    "This client's saved view is damaged; showing the server view (agent view clear --client-view)",
                    "이 client의 저장 보기가 손상되어 서버 보기를 보여 줍니다 (agent view clear --client-view)",
                )),
            ),
        };
        state.profile = Some(profile);
        state.notice.clone_from(&notice);
        state.stored = client.ui().map(UiState::parse).unwrap_or_default();
        state.seen = state.stored.clone();
        state.restore = Some(state.stored.clone());
        (state, view, notice)
    }

    /// Puts back the filter tab before anything is drawn.
    pub(super) fn restore_filter(&self, app: &mut App) {
        if let Some(restore) = &self.restore {
            app.filter = restore.filter;
            app.filter_focus = Filter::ALL
                .iter()
                .position(|filter| *filter == restore.filter)
                .unwrap_or_default();
        }
    }

    /// Notes input from the user, before the desk handles it. What the
    /// input changes is measured from the state now, so nothing the desk
    /// changed before it (a fallback selection, a scroll the last draw
    /// moved) counts as the user's. Before the saved selection is put back,
    /// input drops it, so a restore that waits for an endpoint never
    /// overrides what the user chose meanwhile.
    pub(super) fn input(&mut self, app: &App, event: &Event) {
        let by_user = match event {
            Event::Key(key) => key.kind != KeyEventKind::Release,
            Event::Paste(_) => true,
            // A click acts on its release, and a drag may come as moves.
            Event::Mouse(mouse) => mouse.kind != MouseEventKind::Moved || app.drag.is_some(),
            _ => false,
        };
        if by_user {
            // A turn whose input was not noted yet keeps its starting point.
            if !self.touched {
                self.seen = UiState::of(app);
            }
            self.touched = true;
            self.restore = None;
        }
    }

    /// Puts back the saved selection before a snapshot is applied, so the
    /// desk's guards and epoch see the restored agent. It waits for this
    /// server's rows (`local_ready`); while an endpoint is still
    /// `connecting`, an endpoint agent not listed yet waits for it. An agent
    /// that is gone leaves the desk's own choice. `listed` tells whether an
    /// agent id is in the new list.
    pub(super) fn restore_selection(
        &mut self,
        app: &mut App,
        listed: impl Fn(&str) -> bool,
        local_ready: bool,
        connecting: bool,
    ) {
        if !local_ready {
            return;
        }
        let Some(restore) = &self.restore else {
            return;
        };
        match restore.selected.as_deref() {
            Some(id) if listed(id) => app.selected_id = Some(id.to_owned()),
            Some(id) if connecting && !id.starts_with("local::") => return,
            _ => {}
        }
        self.restored = self.restore.take();
    }

    /// After a snapshot is applied: puts back the scroll of a restore, and
    /// shows the opening notice again once this server's rows replaced the
    /// toast the opening showed.
    pub(super) fn rows_shown(&mut self, app: &mut App, local_ready: bool) {
        if !local_ready {
            return;
        }
        if let Some(notice) = self.notice.take()
            && app.toast.is_none()
        {
            app.notify(notice);
        }
        let Some(restored) = self.restored.take() else {
            return;
        };
        app.list_scroll = restored.scroll;
        app.reveal_selection();
        app.dirty = true;
        self.seen = UiState::of(app);
        self.touched = false;
        self.changed_at = None;
    }

    /// Writes the state when it is due: a new filter tab at once, a new
    /// selection or scroll once it has settled. Called after every turn of
    /// the desk loop, which turns at least once a second.
    pub(super) fn tick(&mut self, app: &mut App, now: Instant) {
        if self.profile.is_none() {
            return;
        }
        // Every turn, so a turn that writes nothing still settles what it saw.
        self.note(&UiState::of(app), now);
        if self.restore.is_some() {
            return;
        }
        if let Some((written, mut handle)) = self.writing.take() {
            if !handle.is_finished() {
                self.writing = Some((written, handle));
                return;
            }
            // A finished task can still poll as pending when the runtime's
            // budget for this turn is spent; keep it for the next turn.
            match (&mut handle).now_or_never() {
                Some(Ok(Ok(()))) => self.stored = written,
                Some(Ok(Err(error))) => self.failed(app, &error, now),
                Some(Err(error)) => self.failed(app, &error.to_string(), now),
                None => {
                    self.writing = Some((written, handle));
                    return;
                }
            }
        }
        if let Some(due) = self.due(UiState::of(app), now) {
            let socket = self.socket.clone();
            let profile = self.profile.clone().unwrap_or_default();
            let encoded = due.encode();
            let handle = tokio::task::spawn_blocking(move || {
                crate::managed::save_client_ui(&socket, &profile, &encoded)
            });
            self.writing = Some((due, handle));
        }
    }

    /// Ends a turn: a change made in a turn with input from the user is
    /// theirs; any other change is the desk's own and only becomes the
    /// state the next input is measured from.
    fn note(&mut self, current: &UiState, now: Instant) {
        if std::mem::take(&mut self.touched) && current != &self.seen {
            self.changed_at = Some(now);
        }
        self.seen = current.clone();
    }

    /// The state to write now, if any: the user's change, a new filter tab
    /// at once, a new selection or scroll once it has settled. Called with
    /// no write in flight.
    fn due(&mut self, current: UiState, now: Instant) -> Option<UiState> {
        let changed_at = self.changed_at?;
        if current == self.stored {
            self.changed_at = None;
            return None;
        }
        if self.retry_at.is_some_and(|at| now < at) {
            return None;
        }
        let settled = now.duration_since(changed_at) >= SETTLE;
        if current.filter != self.stored.filter || settled {
            self.changed_at = None;
            return Some(current);
        }
        None
    }

    fn failed(&mut self, app: &mut App, error: &str, now: Instant) {
        self.retry_at = Some(now + RETRY);
        // The user's change is still unsaved; it is written again once the
        // retry wait is over.
        self.changed_at = Some(now);
        if !self.warned {
            self.warned = true;
            let error = crate::managed::failure::human(error).to_owned();
            app.notify(text(
                app.language,
                &format!("Could not keep the desk view: {error}"),
                &format!("보기를 저장하지 못했습니다: {error}"),
            ));
        }
    }

    /// The last write, when the desk closes with a state the store lacks.
    pub(super) async fn finish(&mut self, app: &App) {
        if let Some((written, handle)) = self.writing.take() {
            match handle.await {
                Ok(Ok(())) => self.stored = written,
                // The change it carried is still unsaved.
                _ => self.changed_at = Some(Instant::now()),
            }
        }
        let Some(profile) = self.profile.clone() else {
            return;
        };
        if self.restore.is_some() {
            return;
        }
        let current = UiState::of(app);
        self.note(&current, Instant::now());
        if self.changed_at.is_none() || current == self.stored {
            return;
        }
        let socket = self.socket.clone();
        let encoded = current.encode();
        let _ = tokio::task::spawn_blocking(move || {
            crate::managed::save_client_ui(&socket, &profile, &encoded)
        })
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(filter: Filter, selected: Option<&str>, scroll: usize) -> UiState {
        UiState {
            filter,
            selected: selected.map(str::to_owned),
            scroll,
        }
    }

    fn view() -> ClientView {
        ClientView {
            socket: PathBuf::new(),
            profile: Some("0123456789abcdef".into()),
            stored: UiState::default(),
            restore: None,
            restored: None,
            notice: None,
            seen: UiState::default(),
            touched: false,
            changed_at: None,
            writing: None,
            retry_at: None,
            warned: false,
        }
    }

    impl ClientView {
        /// A turn of the desk loop with no write in flight.
        fn turn(&mut self, current: UiState, now: Instant) -> Option<UiState> {
            self.note(&current, now);
            self.due(current, now)
        }
    }

    fn app() -> App {
        App::new(false, Language::English, super::super::model::Theme::Dark)
    }

    fn key(code: char) -> Event {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        Event::Key(KeyEvent::new(KeyCode::Char(code), KeyModifiers::NONE))
    }

    #[test]
    fn a_row_is_read_leniently_and_written_back_the_same() {
        let kept = state(Filter::Working, Some("%3"), 7);
        assert_eq!(UiState::parse(&kept.encode()), kept);
        assert_eq!(UiState::parse("not json"), UiState::default());
        assert_eq!(
            UiState::parse(r#"{"filter":"later","selected":"a\nb","scroll":-1}"#),
            UiState::default()
        );
        assert_eq!(UiState::parse(r#"{"scroll":999999999}"#).scroll, MAX_SCROLL);
    }

    #[test]
    fn a_filter_is_written_at_once_and_a_selection_once_it_settles() {
        let mut view = view();
        let start = Instant::now();
        assert_eq!(view.turn(UiState::default(), start), None);
        let filtered = state(Filter::Attention, None, 0);
        view.touched = true;
        assert_eq!(view.turn(filtered.clone(), start), Some(filtered.clone()));
        view.stored = filtered;
        let selected = state(Filter::Attention, Some("%1"), 0);
        view.touched = true;
        assert_eq!(view.turn(selected.clone(), start), None);
        assert_eq!(view.turn(selected.clone(), start + SETTLE / 2), None);
        // Another move restarts the wait.
        let moved = state(Filter::Attention, Some("%2"), 1);
        view.touched = true;
        assert_eq!(view.turn(moved.clone(), start + SETTLE / 2), None);
        assert_eq!(view.turn(moved.clone(), start + SETTLE), None);
        assert_eq!(
            view.turn(moved.clone(), start + SETTLE / 2 + SETTLE),
            Some(moved.clone())
        );
        view.stored = moved.clone();
        assert_eq!(view.turn(moved, start + SETTLE * 4), None);
    }

    #[test]
    fn a_state_the_user_did_not_make_is_not_written() {
        let mut view = view();
        let start = Instant::now();
        view.stored = state(Filter::All, Some("%9"), 4);
        // After the restore the stored agent is gone and the desk chose %1.
        let chosen = state(Filter::All, Some("%1"), 0);
        view.seen = chosen.clone();
        assert_eq!(view.turn(chosen.clone(), start + SETTLE * 10), None);
        // The desk moves on its own (the selected agent exited, the list
        // grew): not written.
        let by_desk = state(Filter::All, Some("%3"), 2);
        assert_eq!(view.turn(by_desk.clone(), start), None);
        assert_eq!(view.turn(by_desk, start + SETTLE * 10), None);
        // A move by the user is written once it settles.
        let moved = state(Filter::All, Some("%2"), 0);
        view.touched = true;
        assert_eq!(view.turn(moved.clone(), start), None);
        assert_eq!(
            view.turn(moved.clone(), start + SETTLE),
            Some(moved.clone())
        );
        // Once it is written, a later move by the desk is not.
        view.stored = moved;
        let later = state(Filter::All, Some("%4"), 0);
        assert_eq!(view.turn(later.clone(), start + SETTLE * 3), None);
        assert_eq!(view.turn(later, start + SETTLE * 9), None);
    }

    #[test]
    fn input_drops_a_restore_that_still_waits() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent};
        let app = app();
        let mut view = view();
        view.restore = Some(state(Filter::All, Some("ep::delta"), 3));
        let mouse = |kind| {
            Event::Mouse(MouseEvent {
                kind,
                column: 1,
                row: 1,
                modifiers: KeyModifiers::NONE,
            })
        };
        view.input(&app, &mouse(MouseEventKind::Moved));
        assert!(view.restore.is_some() && !view.touched);
        view.input(&app, &key('j'));
        assert!(view.restore.is_none() && view.touched);
        // The release of a click is input: the desk acts on it.
        view.touched = false;
        view.input(&app, &mouse(MouseEventKind::Up(MouseButton::Left)));
        assert!(view.touched);
    }

    #[test]
    fn what_the_desk_chose_while_a_restore_waited_is_not_written() {
        let mut app = app();
        let mut view = view();
        let saved = state(Filter::All, Some("ep::delta"), 5);
        view.stored = saved.clone();
        view.seen = saved.clone();
        view.restore = Some(saved);
        let start = Instant::now();
        // The endpoint still connects; the desk shows its own first row.
        app.selected_id = Some("local::a".into());
        app.list_scroll = 0;
        view.tick(&mut app, start);
        // A key that changes nothing drops the restore.
        view.input(&app, &key('?'));
        view.tick(&mut app, start);
        view.tick(&mut app, start + SETTLE * 10);
        assert!(view.restore.is_none());
        assert!(view.changed_at.is_none() && view.writing.is_none());
        // Before any turn too: the opening state is not the user's either.
        let mut view = self::view();
        view.stored = state(Filter::All, Some("ep::delta"), 5);
        view.seen = view.stored.clone();
        view.restore = Some(view.stored.clone());
        view.input(&app, &key('q'));
        view.note(&UiState::of(&app), start);
        assert!(view.changed_at.is_none());
    }

    #[test]
    fn input_in_a_turn_with_a_write_in_flight_is_settled_in_that_turn() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _guard = runtime.enter();
        let mut app = app();
        let mut view = view();
        let start = Instant::now();
        let (release, held) = std::sync::mpsc::channel::<()>();
        let written = state(Filter::Attention, None, 0);
        let handle = tokio::task::spawn_blocking(move || {
            held.recv().ok();
            Ok(())
        });
        view.writing = Some((written.clone(), handle));
        app.filter = Filter::Attention;
        view.seen = written.clone();
        // A key that changes nothing while the write is in flight.
        view.input(&app, &key('?'));
        view.tick(&mut app, start);
        assert!(!view.touched && view.writing.is_some());
        release.send(()).unwrap();
        while !view.writing.as_ref().unwrap().1.is_finished() {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        // The desk then moves on its own: not the user's.
        app.selected_id = Some("%2".into());
        view.tick(&mut app, start + SETTLE * 10);
        assert_eq!(view.stored, written);
        assert!(view.changed_at.is_none() && view.writing.is_none());
    }

    #[test]
    fn a_restore_waits_only_for_an_endpoint_agent_not_listed_yet() {
        let mut app = App::new(false, Language::English, super::super::model::Theme::Dark);
        let mut view = view();
        view.restore = Some(state(Filter::All, Some("ep::delta"), 3));
        // This server's rows are not shown yet.
        view.restore_selection(&mut app, |_| false, false, true);
        assert!(view.restore.is_some());
        // The endpoint is still connecting.
        view.restore_selection(&mut app, |_| false, true, true);
        assert!(view.restore.is_some());
        // Listed now.
        view.restore_selection(&mut app, |id| id == "ep::delta", true, true);
        assert_eq!(app.selected_id.as_deref(), Some("ep::delta"));
        assert!(view.restore.is_none() && view.restored.is_some());
        // A local agent that is not listed is gone: no wait.
        let mut fresh = App::new(false, Language::English, super::super::model::Theme::Dark);
        view.restore = Some(state(Filter::All, Some("local::charlie"), 0));
        view.restore_selection(&mut fresh, |_| false, true, true);
        assert!(view.restore.is_none() && fresh.selected_id.is_none());
    }

    #[test]
    fn a_failed_write_waits_before_the_next() {
        let mut view = view();
        let start = Instant::now();
        view.retry_at = Some(start + RETRY);
        view.changed_at = Some(start);
        let filtered = state(Filter::Working, None, 0);
        assert_eq!(view.turn(filtered.clone(), start), None);
        assert_eq!(view.turn(filtered.clone(), start + RETRY), Some(filtered));
    }
}
