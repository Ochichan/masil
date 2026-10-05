//! The coordinator's view of agent panes between ticks. It does what
//! `agent list` does whenever a pane changes, so attention events reach the
//! inbox while no command or management window runs, and it resolves the
//! events of runs whose pane is gone.
//!
//! A quiet tick starts no process on macOS: output to a pane updates its
//! tty's mtime, so the tick compares those. A `list-panes` runs when one
//! changed and every few ticks for new panes; the full pass (detection,
//! capture, tracked writes) runs only when that listing, a report (poke) or
//! time can have changed a state. Linux updates a tty's times at most every
//! 8 s, so there every tick lists the panes.

use super::operations::{STORE_NEWER, Store};
use super::{Agent, Manager};
use crate::bridge::{self, Event, PaneReason};
use crate::observation::now_ms;
use serde::Serialize;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Notify, mpsc};

/// Times in quiet ticks, so a test's short tick shortens them all. At the
/// default 2 s tick: stand back 60 s, a full pass at least every 60 s, busy
/// for 30 s after a change, a run ended after 30 s missing, the store
/// reopened every 30 s, the panes listed at least every 10 s.
#[derive(Clone, Copy, Debug)]
struct Timing {
    /// How long the coordinator stands back from a pane whose unchanged
    /// screen another writer judged differently.
    stand_back: u64,
    /// A full pass at least this often, as a safety net. The one change
    /// without output, a report expiring, gets its own pass.
    pass_every: u64,
    /// Busy for this long after any change: ticks run twice as often.
    hot: u64,
    /// A run missing from successful looks for this long has ended.
    ended_after: u64,
    /// The store connection is dropped this often, so a replaced store is
    /// noticed.
    reopen: u64,
    /// The panes are listed at least this often, for panes that are new and
    /// so not yet watched through their tty.
    list_every: u64,
}

impl Timing {
    fn new(tick: Duration) -> Self {
        let tick = tick.as_millis() as u64;
        Self {
            stand_back: 30 * tick,
            pass_every: 30 * tick,
            hot: 15 * tick,
            ended_after: 15 * tick,
            reopen: 15 * tick,
            list_every: 5 * tick,
        }
    }
}

/// One line per pane. Output counts only for panes that hold an agent or
/// did (META or TRACKED), so a busy shell or `tail -f` does not wake the
/// watch; a new agent shows up as a new command or foreground group.
const SIGNATURE: &str = "#{pane_id} #{masil_core_boot_id} #{pane_dead} #{masil_pty_generation} #{masil_foreground_pgid} #{pane_current_command} #{?#{||:#{@masil-managed-agent},#{@masil-managed-observation}},#{pane_output_generation} #{pane_tty},- -}";
/// Whether output shows in a tty's mtime promptly enough to skip listing.
const TTY_TIMES: bool = cfg!(target_os = "macos");
/// Core dirty notifications are leading-edge; one pass per 300 ms keeps
/// their bounded-latency target while coalescing a busy pane's burst.
const BRIDGE_EVENT_PASS_MS: u64 = 300;

fn tty_mtime(path: &str) -> Option<i128> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::metadata(path).ok()?;
    Some(i128::from(metadata.mtime()) * 1_000_000_000 + i128::from(metadata.mtime_nsec()))
}

#[derive(Clone, Debug, Default, PartialEq)]
struct PaneMemo {
    output: u64,
    tracked: String,
    /// Since when this process stands back from the pane.
    since: Option<u64>,
}

/// State a long-lived Manager keeps across passes.
pub(crate) struct Resident {
    timing: Timing,
    panes: Mutex<HashMap<String, PaneMemo>>,
    /// The operation store, opened once and reused.
    pub(super) store: Arc<Mutex<Option<Store>>>,
    /// An inbox write needs the coordinator's badge, extension, and route
    /// follow-up even when it happened outside the current screen pass.
    inbox_dirty: AtomicBool,
    stood_back: AtomicUsize,
    /// The pane and time of the last stand-back, for `status`.
    last_stood_back: Mutex<Option<(String, u64)>>,
}

impl Resident {
    fn new(tick: Duration) -> Self {
        Self {
            timing: Timing::new(tick),
            panes: Mutex::new(HashMap::new()),
            store: Arc::new(Mutex::new(None)),
            inbox_dirty: AtomicBool::new(false),
            stood_back: AtomicUsize::new(0),
            last_stood_back: Mutex::new(None),
        }
    }

    /// Whether this process may write the pane's tracked state. It may not
    /// while another writer changed TRACKED under the same output, which
    /// means the two judge one screen differently (different manifests).
    pub(super) fn may_write(&self, pane: &str, output: &str, tracked: &str, now: u64) -> bool {
        let Ok(output) = output.parse::<u64>() else {
            return true;
        };
        let Ok(mut panes) = self.panes.lock() else {
            return true;
        };
        let (allowed, stood_back) = decide(
            panes.get_mut(pane),
            output,
            tracked,
            now,
            self.timing.stand_back,
        );
        if stood_back {
            self.stood_back.fetch_add(1, Ordering::Relaxed);
            if let Ok(mut last) = self.last_stood_back.lock() {
                *last = Some((pane.to_owned(), now));
            }
        }
        allowed
    }

    /// Records what each agent pane holds after a pass.
    pub(super) fn remember(&self, agents: &[Agent], full: bool) {
        let Ok(mut panes) = self.panes.lock() else {
            return;
        };
        for agent in agents {
            let memo = panes.entry(agent.pane_id.clone()).or_default();
            if memo.output != agent.output_generation {
                memo.since = None;
            }
            memo.output = agent.output_generation;
            memo.tracked.clone_from(&agent.tracked_encoded);
        }
        if full {
            let present: HashSet<&str> =
                agents.iter().map(|agent| agent.pane_id.as_str()).collect();
            panes.retain(|pane, _| present.contains(pane.as_str()));
        }
    }

    /// Drops the volatile memo for a bridge-notified PTY replacement or pane
    /// removal. The following native pass remains the authority for the new
    /// binding and tracked state.
    pub(super) fn forget(&self, pane: &str) {
        if let Ok(mut panes) = self.panes.lock() {
            panes.remove(pane);
        }
    }

    pub(super) fn mark_inbox_dirty(&self) {
        self.inbox_dirty.store(true, Ordering::SeqCst);
    }

    fn take_inbox_dirty(&self) -> bool {
        self.inbox_dirty.swap(false, Ordering::SeqCst)
    }
}

/// `(allowed, newly stood back)` for one pane.
fn decide(
    memo: Option<&mut PaneMemo>,
    output: u64,
    tracked: &str,
    now: u64,
    stand_back: u64,
) -> (bool, bool) {
    let Some(memo) = memo.filter(|memo| memo.output == output) else {
        return (true, false);
    };
    if let Some(since) = memo.since {
        if now.saturating_sub(since) < stand_back {
            return (false, false);
        }
        // The period is over: judge again, so a writer that still disagrees
        // starts another one. At most one write per period goes through.
        memo.since = None;
    }
    if memo.tracked != tracked {
        memo.since = Some(now);
        return (false, true);
    }
    (true, false)
}

/// What the watch reports through the coordinator's `status`.
#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct WatchReport {
    pub ticks: u64,
    pub passes: u64,
    pub last_tick_ms: u64,
    pub last_pass_ms: u64,
    pub stood_back: usize,
    /// The pane and time of the last stand-back.
    pub last_stood_back: Option<(String, u64)>,
    pub ended_runs: u64,
    /// `--answers` runs being observed.
    pub observed: usize,
    pub last_error: Option<String>,
    pub last_error_ms: u64,
}

/// Signals between the coordinator and its watch.
#[derive(Default)]
pub(crate) struct WatchControl {
    /// A report or inbox write happened (poke), or a reload was asked for.
    pub wake: Notify,
    /// Re-read detection manifests before the next pass.
    pub reload: AtomicBool,
    /// A report or inbox write happened: look at the panes.
    pub poked: AtomicBool,
    /// Events were read: count again for the badge.
    pub recount: AtomicBool,
    /// The coordinator is ending: finish the current pass and return.
    pub stopping: AtomicBool,
    /// `agent notify test` waits here for the routes' results.
    pub test: Mutex<Option<tokio::sync::oneshot::Sender<serde_json::Value>>>,
    pub report: Mutex<WatchReport>,
    /// The badge was counted after a pass or a poke: inbox events may have
    /// been written (resident extensions read them then).
    pub inbox_changed: Arc<Notify>,
}

/// Consume both inbox-change sources every iteration. These must be separate
/// swaps so a pending recount cannot leave a resident-store write behind.
fn take_inbox_changes(control: &WatchControl, resident: Option<&Resident>) -> bool {
    let recounted = control.recount.swap(false, Ordering::SeqCst);
    let resident_dirty = resident.is_some_and(Resident::take_inbox_dirty);
    recounted || resident_dirty
}

/// A bounded reconnect schedule for bridge setup failures. The resident
/// never reconnects a watch in place: after loss it returns to the native
/// polling path until this timer permits a fresh, fenced subscription.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BridgeRetry {
    next_at: u64,
    delay: u64,
}

impl Default for BridgeRetry {
    fn default() -> Self {
        Self {
            next_at: 0,
            delay: 1_000,
        }
    }
}

impl BridgeRetry {
    fn due(self, now: u64) -> bool {
        now >= self.next_at
    }

    fn failed(&mut self, now: u64) {
        self.next_at = now.saturating_add(self.delay);
        self.delay = self.delay.saturating_mul(2).min(30_000);
    }

    fn unavailable(&mut self, now: u64, recheck_after: u64) {
        self.next_at = now.saturating_add(recheck_after);
        self.delay = 1_000;
    }

    fn succeeded(&mut self) {
        *self = Self::default();
    }
}

enum BridgeCommand {
    Resubscribe(HashSet<String>),
}

enum BridgeNotice {
    Ready { scope: HashSet<String> },
    Event(Event),
    Failed(String),
}

/// The resident-side state of the bridge. It owns only observation tasks;
/// all state changes still go through `Manager::list` and its guards.
#[derive(Default)]
struct BridgeState {
    task: Option<tokio::task::JoinHandle<()>>,
    command: Option<mpsc::UnboundedSender<BridgeCommand>>,
    scope: HashSet<String>,
    scope_known: bool,
    core_boot_id: Option<String>,
    bridge_off_boot_id: Option<String>,
    healthy: bool,
    healthy_since: Option<u64>,
    changed: bool,
    needs_full_pass: bool,
    dirty_panes: HashSet<String>,
    last_event_pass: Option<u64>,
    /// The last event pass failed: wait for the ordinary tick instead of the
    /// event-spacing clamp, while fresh notices stay limited by last_event_pass.
    event_failed: bool,
    lifecycle_changed: bool,
    retry: BridgeRetry,
}

impl BridgeState {
    fn can_start(&self, now: u64) -> bool {
        self.scope_known
            && !self.healthy
            && self.task.is_none()
            && !self.bridge_is_off()
            && self.retry.due(now)
    }

    fn bridge_is_off(&self) -> bool {
        self.core_boot_id
            .as_ref()
            .is_some_and(|boot| self.bridge_off_boot_id.as_ref() == Some(boot))
    }

    fn observe_boot(&mut self, boot: Option<String>) {
        if self.core_boot_id == boot {
            return;
        }
        self.core_boot_id = boot;
        self.bridge_off_boot_id = None;
        self.retry.next_at = 0;
        self.healthy = false;
        self.healthy_since = None;
        self.stop_task();
    }

    fn start(
        &mut self,
        endpoint: bridge::Endpoint,
        notices: mpsc::UnboundedSender<BridgeNotice>,
        control: Arc<WatchControl>,
    ) {
        self.stop_task();
        let (command, commands) = mpsc::unbounded_channel();
        self.command = Some(command);
        let scope = self.scope.clone();
        self.task = Some(tokio::spawn(bridge_supervisor(
            endpoint, scope, commands, notices, control,
        )));
    }

    fn set_scope(&mut self, scope: HashSet<String>) {
        let changed = self.scope != scope;
        self.scope_known = true;
        if !changed {
            return;
        }
        self.scope = scope.clone();
        self.healthy = false;
        self.healthy_since = None;
        if self
            .command
            .as_ref()
            .is_some_and(|command| command.send(BridgeCommand::Resubscribe(scope)).is_err())
        {
            self.stop_task();
        }
    }

    fn failed(&mut self, now: u64) {
        self.stop_task();
        self.healthy = false;
        self.healthy_since = None;
        self.retry.failed(now);
    }

    fn disabled(&mut self, now: u64, recheck_after: u64) {
        self.stop_task();
        self.healthy = false;
        self.healthy_since = None;
        self.bridge_off_boot_id.clone_from(&self.core_boot_id);
        self.retry.unavailable(now, recheck_after);
    }

    fn ready(&mut self, now: u64) {
        self.healthy = true;
        self.healthy_since = Some(now);
        // A Ready may have been drained before an unrelated select branch
        // continued, so it needs the same persistent pass marker as an event.
        self.changed = true;
        self.needs_full_pass = true;
    }

    fn reset_retry_after_stable_watch(&mut self, now: u64, pass_every: u64) {
        if self
            .healthy_since
            .is_some_and(|since| now.saturating_sub(since) >= pass_every)
        {
            self.retry.succeeded();
            self.healthy_since = None;
        }
    }

    fn note_event(&mut self, event: &Event) {
        self.changed = true;
        if let Event::Pane {
            pane_id,
            reason: PaneReason::ScreenDirty,
        } = event
            && self.scope.contains(pane_id)
            && !self.needs_full_pass
        {
            self.dirty_panes.insert(pane_id.clone());
        } else {
            self.needs_full_pass = true;
        }
    }

    fn signature_changed(&mut self) {
        self.changed = true;
        self.needs_full_pass = true;
    }

    fn event_pass_due(&self, now: u64) -> bool {
        self.changed
            && self
                .last_event_pass
                .is_none_or(|last| now.saturating_sub(last) >= BRIDGE_EVENT_PASS_MS)
    }

    fn targeted_dirty_pane(&self) -> Option<String> {
        if !self.changed || self.needs_full_pass || self.dirty_panes.len() != 1 {
            return None;
        }
        self.dirty_panes.iter().next().cloned()
    }

    fn pass_started(&mut self, now: u64) {
        if self.changed {
            self.last_event_pass = Some(now);
        }
        self.event_failed = false;
    }

    /// Keep the dirty marker for a later normal tick or fresh notice, but do
    /// not use this failed pass to schedule another short event retry.
    fn pass_failed(&mut self) {
        self.event_failed = true;
    }

    fn pass_succeeded(&mut self) {
        self.event_failed = false;
        self.changed = false;
        self.needs_full_pass = false;
        self.dirty_panes.clear();
    }

    fn stop_task(&mut self) {
        self.command = None;
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl Drop for BridgeState {
    fn drop(&mut self) {
        self.stop_task();
    }
}

/// The persistent change marker survives a select branch that handles an
/// observer or notifier update before the next pass decision.
fn healthy_pass_needed(
    changed: bool,
    event_due: bool,
    forced: bool,
    expired: bool,
    safety_due: bool,
    sweep_due: bool,
) -> bool {
    (changed && event_due) || forced || expired || safety_due || sweep_due
}

/// The bridge's leading-edge dirty signal is rate-limited by the preceding
/// event pass. Do not let the ordinary quiet tick turn that short spacing into
/// a one-to-two-second delay.
fn bridge_wait_interval(
    interval: Duration,
    healthy: bool,
    changed: bool,
    failed: bool,
    last_event_pass: Option<u64>,
    now: u64,
) -> Duration {
    if !healthy || !changed || failed {
        return interval;
    }
    let Some(last_event_pass) = last_event_pass else {
        return interval;
    };
    let remaining = BRIDGE_EVENT_PASS_MS.saturating_sub(now.saturating_sub(last_event_pass));
    interval.min(Duration::from_millis(remaining))
}

/// State which makes the historical fixed tick necessary even while the
/// bridge is connected. A busy or hot pass deliberately keeps its old polling
/// cadence; this only stretches genuinely quiet healthy time.
#[derive(Clone, Copy)]
struct HealthyBridgeWaitState {
    healthy: bool,
    busy: bool,
    hot_until: u64,
    retry: bool,
    inbox_dirty: bool,
    changed: bool,
    event_failed: bool,
    last_event_pass: Option<u64>,
    /// Upper bound for one healthy idle sleep.
    longest_wait: Duration,
}

/// Absolute millisecond deadlines for work that otherwise used to wait for
/// the next quiet tick. `None` means that optional work is not scheduled.
#[derive(Clone, Copy)]
struct HealthyBridgeDeadlines {
    periodic: u64,
    reopen: u64,
    list: u64,
    pass: u64,
    sweep: u64,
    report_expiry: Option<u64>,
    badge_retry: Option<u64>,
    report_retry: Option<u64>,
    report_accept: Option<u64>,
    prompt_expiry: Option<u64>,
    stable_watch: Option<u64>,
}

impl HealthyBridgeDeadlines {
    fn earliest(self) -> Option<u64> {
        [
            Some(self.periodic),
            Some(self.reopen),
            Some(self.list),
            Some(self.pass),
            Some(self.sweep),
            self.report_expiry,
            self.badge_retry,
            self.report_retry,
            self.report_accept,
            self.prompt_expiry,
            self.stable_watch,
        ]
        .into_iter()
        .flatten()
        .min()
    }
}

/// Chooses the next wait without turning a healthy, quiet coordinator back
/// into a fixed-tick poller. This runs after the current iteration's work, so
/// an ordinary deadline still in the past means its next time is unknown
/// (usually a failed attempt); use the historical tick instead of spinning.
fn healthy_bridge_wait_interval(
    interval: Duration,
    now: u64,
    state: HealthyBridgeWaitState,
    deadlines: HealthyBridgeDeadlines,
) -> Duration {
    let fallback = bridge_wait_interval(
        interval,
        state.healthy,
        state.changed,
        state.event_failed,
        state.last_event_pass,
        now,
    );
    if !state.healthy
        || state.busy
        || now < state.hot_until
        || state.retry
        || state.inbox_dirty
    {
        return fallback;
    }

    let event_at = if state.changed {
        if state.event_failed {
            return fallback;
        }
        let Some(last_event_pass) = state.last_event_pass else {
            return fallback;
        };
        Some(last_event_pass.saturating_add(BRIDGE_EVENT_PASS_MS))
    } else {
        None
    };
    // A coalesced bridge event is actionable now, unlike an ordinary timer
    // which has just failed to advance and must retain its tick retry.
    if event_at.is_some_and(|at| at <= now) {
        return Duration::ZERO;
    }

    let deadline = match (deadlines.earliest(), event_at) {
        (Some(left), Some(right)) => left.min(right),
        (Some(deadline), None) | (None, Some(deadline)) => deadline,
        // All permanent coordinator duties above are known. Keep the fixed
        // tick if that invariant ever changes rather than sleeping forever.
        (None, None) => return fallback,
    };
    if deadline <= now {
        return fallback;
    }
    // A wall clock stepped back would push every deadline away; never sleep
    // longer than the signature audit interval.
    Duration::from_millis(deadline - now).min(state.longest_wait)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PostPassInboxActions {
    count: bool,
    signal: bool,
    notify: bool,
}

/// A successful screen pass is the only producer that does not poke the
/// resident after changing its own inbox. Keep its three follow-up actions
/// together so a new attention event cannot leave a stale badge or extension.
fn post_pass_inbox_actions(pass_succeeded: bool, inbox_effects: bool) -> PostPassInboxActions {
    let changed = pass_succeeded && inbox_effects;
    PostPassInboxActions {
        count: changed,
        signal: changed,
        notify: changed,
    }
}

fn bridge_notice(
    notices: &mpsc::UnboundedSender<BridgeNotice>,
    control: &WatchControl,
    notice: BridgeNotice,
) -> bool {
    let sent = notices.send(notice).is_ok();
    if sent {
        // The resident's wake path debounces (50 ms with a healthy bridge).
        control.wake.notify_one();
    }
    sent
}

async fn bridge_watches(
    endpoint: &bridge::Endpoint,
    scope: &HashSet<String>,
) -> Result<Vec<bridge::Watch>, bridge::Error> {
    let lifecycle = bridge::Client::connect(endpoint, "resident-lifecycle-hello")
        .await?
        .watch("resident-lifecycle-watch", Vec::new(), true)
        .await?;
    let mut watches = vec![lifecycle];
    let mut panes: Vec<_> = scope.iter().cloned().collect();
    panes.sort_unstable();
    for (index, shard) in panes.chunks(bridge::MAX_WATCH_PANES).enumerate() {
        let hello = format!("resident-scope-{index}-hello");
        let watch = format!("resident-scope-{index}-watch");
        watches.push(
            bridge::Client::connect(endpoint, &hello)
                .await?
                .watch(&watch, shard.to_vec(), false)
                .await?,
        );
    }
    Ok(watches)
}

enum BridgeSupervisorOutcome {
    Resubscribe(HashSet<String>),
    Failed(bridge::Error),
    Stopped,
}

/// Keeps all watch sockets for a resident together. Any stream loss invalidates
/// the complete bridge projection: continuing with only lifecycle or only a
/// subset of panes would hide a missed dirty/lifecycle transition.
async fn bridge_supervisor(
    endpoint: bridge::Endpoint,
    mut scope: HashSet<String>,
    mut commands: mpsc::UnboundedReceiver<BridgeCommand>,
    notices: mpsc::UnboundedSender<BridgeNotice>,
    control: Arc<WatchControl>,
) {
    loop {
        let watches = match bridge_watches(&endpoint, &scope).await {
            Ok(watches) => watches,
            Err(error) => {
                bridge_notice(&notices, &control, BridgeNotice::Failed(error.to_string()));
                return;
            }
        };
        if !bridge_notice(
            &notices,
            &control,
            BridgeNotice::Ready {
                scope: scope.clone(),
            },
        ) {
            return;
        }
        let (events, mut received) = mpsc::unbounded_channel();
        let mut tasks = tokio::task::JoinSet::new();
        for mut watch in watches {
            let events = events.clone();
            tasks.spawn(async move {
                loop {
                    match watch.next().await {
                        Ok(event) => {
                            if events.send(Ok(event)).is_err() {
                                return;
                            }
                        }
                        Err(error) => {
                            let _ = events.send(Err(error));
                            return;
                        }
                    }
                }
            });
        }
        drop(events);
        let outcome = loop {
            tokio::select! {
                command = commands.recv() => match command {
                    Some(BridgeCommand::Resubscribe(scope)) => {
                        break BridgeSupervisorOutcome::Resubscribe(scope);
                    }
                    None => break BridgeSupervisorOutcome::Stopped,
                },
                event = received.recv() => match event {
                    Some(Ok(event)) => {
                        if matches!(event, Event::ServerExiting) {
                            break BridgeSupervisorOutcome::Failed(bridge::Error::Lost("server exiting".into()));
                        }
                        if !bridge_notice(&notices, &control, BridgeNotice::Event(event)) {
                            break BridgeSupervisorOutcome::Stopped;
                        }
                    }
                    Some(Err(error)) => break BridgeSupervisorOutcome::Failed(error),
                    None => break BridgeSupervisorOutcome::Failed(bridge::Error::Lost("all bridge watch tasks ended".into())),
                },
                joined = tasks.join_next() => match joined {
                    Some(Ok(())) => break BridgeSupervisorOutcome::Failed(bridge::Error::Lost("bridge watch task ended".into())),
                    Some(Err(error)) => break BridgeSupervisorOutcome::Failed(bridge::Error::Lost(format!("bridge watch task failed: {error}"))),
                    None => break BridgeSupervisorOutcome::Failed(bridge::Error::Lost("all bridge watch tasks ended".into())),
                },
            }
        };
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        match outcome {
            BridgeSupervisorOutcome::Resubscribe(next) => scope = next,
            BridgeSupervisorOutcome::Failed(error) => {
                bridge_notice(&notices, &control, BridgeNotice::Failed(error.to_string()));
                return;
            }
            BridgeSupervisorOutcome::Stopped => return,
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct BridgeEventAction {
    lifecycle_changed: bool,
    invalidate_pane: Option<String>,
}

/// The bridge only says that a native object changed. It does not mutate an
/// agent projection; this describes which cache reset and pass are needed.
fn bridge_event_action(event: &Event) -> BridgeEventAction {
    match event {
        Event::Pane { pane_id, reason } => BridgeEventAction {
            lifecycle_changed: matches!(reason, PaneReason::Created | PaneReason::Removed),
            invalidate_pane: matches!(reason, PaneReason::PtyChanged | PaneReason::Removed)
                .then(|| pane_id.clone()),
        },
        Event::WindowRemoved { .. } | Event::SessionRemoved { .. } => BridgeEventAction {
            lifecycle_changed: true,
            invalidate_pane: None,
        },
        Event::ServerExiting => BridgeEventAction::default(),
    }
}

impl Manager {
    /// The coordinator's Manager: it keeps per-pane memory and one store
    /// connection, and never pokes itself. `tick` is the quiet watch tick.
    pub(crate) fn resident(socket: std::path::PathBuf, tick: Duration) -> Result<Self, String> {
        let mut manager = Self::new(socket, None)?;
        manager.resident = Some(Resident::new(tick));
        Ok(manager)
    }

    pub(super) fn is_resident(&self) -> bool {
        self.resident.is_some()
    }

    /// Runs `work` on the resident store, opening it first if needed. None
    /// when the inbox is off or the store is missing.
    pub(super) async fn with_resident_store<T: Send + 'static>(
        &self,
        work: impl FnOnce(&mut Store) -> Result<T, String> + Send + 'static,
    ) -> Result<Option<T>, String> {
        let Some(resident) = &self.resident else {
            return Ok(None);
        };
        let cached = resident.store.clone();
        let open = cached.lock().map(|store| store.is_some()).unwrap_or(false);
        let instance = if open {
            None
        } else {
            match self.inbox_instance().await {
                Some(instance) => Some(instance),
                None => return Ok(None),
            }
        };
        let socket = self.native.socket.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = cached.lock().map_err(|_| "resident store lock poisoned")?;
            if guard.is_none() {
                let Some(instance) = instance else {
                    return Ok(None);
                };
                *guard = Store::open_existing(&socket, &instance)?;
            }
            let Some(store) = guard.as_mut() else {
                return Ok(None);
            };
            let result = work(store);
            if result.is_err() {
                // Reopen next time; a replaced or broken file is not reused.
                *guard = None;
            }
            result.map(Some)
        })
        .await
        .map_err(|error| error.to_string())?
    }
}

/// Watches the agent panes of the coordinator's server until `stopping` is
/// set or `stop` fires. `base` is the quiet tick; ticks run twice as often
/// while busy.
pub(crate) async fn watch(
    mut manager: Manager,
    control: Arc<WatchControl>,
    base: Duration,
    log: impl Fn(&str),
    stop: Arc<Notify>,
) {
    let timing = Timing::new(base);
    let mut errors = ErrorLog::default();
    let mut previous = String::new();
    let mut previous_panes = HashSet::new();
    let mut last_pass = 0_u64;
    let mut last_list = 0_u64;
    let mut ttys: HashMap<String, Option<i128>> = HashMap::new();
    let mut held = false;
    let mut retry = false;
    let mut expiry: Option<u64> = None;
    let mut last_sweep = 0_u64;
    let mut last_reopen = now_ms();
    let mut hot_until = 0;
    let mut busy = false;
    let mut absent: HashMap<String, u64> = HashMap::new();
    let mut stood_back = 0;
    let mut badge = Badge::read(&manager).await;
    let mut first = true;
    // `--answers` OpenCode runs: their tasks read the servers, this loop
    // writes what they find (observe.rs).
    let (updates, mut received) = tokio::sync::mpsc::unbounded_channel();
    let mut observers = super::observe::Observers::new(base);
    let mut reports = Reports::default();
    let (bridge_notices, mut bridge_events) = mpsc::unbounded_channel();
    let mut bridge = BridgeState::default();
    let observer = super::Observer::current();
    let mut notifier = crate::notify::Notifier::new(manager.native.clone());
    let mut next_tick: Option<tokio::time::Instant> = None;
    loop {
        let interval = if busy || now_ms() < hot_until {
            (base / 2).max(Duration::from_millis(100))
        } else {
            base
        };
        // A badge value held back by the once-a-second limit is written as
        // soon as the second is over.
        let interval = badge
            .retry_in(now_ms())
            .map_or(interval, |retry| interval.min(retry));
        let interval = reports
            .retry_in(now_ms())
            .map_or(interval, |retry| interval.min(retry));
        let mut force_pass = first || retry;
        if process_bridge_notices(
            &mut bridge,
            &mut bridge_events,
            BridgeNoticeContext {
                manager: &manager,
                observers: &mut observers,
                reports: &mut reports,
                errors: &mut errors,
                log: &log,
                control: &control,
            },
            now_ms(),
        ) && !bridge.healthy
        {
            force_pass = true;
        }
        let now = now_ms();
        let interval = healthy_bridge_wait_interval(
            interval,
            now,
            HealthyBridgeWaitState {
                healthy: bridge.healthy,
                busy,
                hot_until,
                retry,
                inbox_dirty: manager
                    .resident
                    .as_ref()
                    .is_some_and(|resident| resident.inbox_dirty.load(Ordering::SeqCst)),
                changed: bridge.changed,
                event_failed: bridge.event_failed,
                last_event_pass: bridge.last_event_pass,
                longest_wait: Duration::from_millis(timing.list_every),
            },
            HealthyBridgeDeadlines {
                // The periodic badge reread also reloads notification
                // settings and claims any queued notification events.
                periodic: badge.reread_at.saturating_add(timing.ended_after),
                reopen: last_reopen.saturating_add(timing.reopen),
                list: last_list.saturating_add(timing.list_every),
                pass: last_pass.saturating_add(timing.pass_every),
                sweep: last_sweep.saturating_add(timing.ended_after),
                report_expiry: expiry,
                badge_retry: badge.retry_at(),
                report_retry: reports.retry_at(),
                report_accept: reports.accept_at(),
                prompt_expiry: reports.prompt_expiry_at(),
                stable_watch: bridge
                    .healthy_since
                    .map(|since| since.saturating_add(timing.pass_every)),
            },
        );
        if !first {
            // Updates are handled between ticks without moving the next one.
            let candidate = tokio::time::Instant::now() + interval;
            let deadline =
                next_tick.map_or(candidate, |at: tokio::time::Instant| at.min(candidate));
            next_tick = Some(deadline);
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => {}
                _ = control.wake.notified() => {
                    // Several pokes in a burst make one pass. A healthy bridge
                    // already coalesces screen changes in the core and event
                    // passes keep their 300 ms spacing, so a short wait
                    // is enough there (B-13: an active pane within 500 ms).
                    let debounce = if bridge.healthy { 50 } else { 250 };
                    tokio::time::sleep(Duration::from_millis(debounce)).await;
                }
                Some(update) = received.recv() => {
                    if let Some(observer) = observer {
                        reports.apply(&manager, update, observer).await;
                    }
                    continue;
                }
                Some(outcome) = notifier.outcomes.recv() => {
                    if let Err(error) = record_outcome(&manager, outcome).await {
                        errors.note(&error, &log, &control);
                    }
                    continue;
                }
            }
            next_tick = None;
            if process_bridge_notices(
                &mut bridge,
                &mut bridge_events,
                BridgeNoticeContext {
                    manager: &manager,
                    observers: &mut observers,
                    reports: &mut reports,
                    errors: &mut errors,
                    log: &log,
                    control: &control,
                },
                now_ms(),
            ) && !bridge.healthy
            {
                force_pass = true;
            }
        }
        first = false;
        if control.stopping.load(Ordering::SeqCst) {
            bridge.stop_task();
            observers.stop();
            badge.clear(&manager).await;
            while let Ok(outcome) = notifier.outcomes.try_recv() {
                let _ = record_outcome(&manager, outcome).await;
            }
            return;
        }
        let test = control.test.lock().ok().and_then(|mut slot| slot.take());
        if let Some(reply) = test {
            notify_test(&manager, &mut notifier, reply).await;
        }
        if let Some(observer) = observer {
            reports.flush(&manager, observer).await;
        }
        let poked = control.poked.swap(false, Ordering::SeqCst);
        let inbox_changed = take_inbox_changes(&control, manager.resident.as_ref());
        force_pass |= poked;
        // `agent prompt` pokes once it has delivered.
        reports.accept(&manager, poked || inbox_changed).await;
        if control.reload.swap(false, Ordering::SeqCst) {
            if let Err(error) = manager.reload() {
                errors.note(&error, &log, &control);
            }
            notifier.reload();
            force_pass = true;
        }
        let started = now_ms();
        // Its own clock: passes count often while agents work, and the
        // shown value must still be read back now and then.
        let periodic = started.saturating_sub(badge.reread_at) >= timing.ended_after;
        if periodic {
            badge.reread_at = started;
            badge.reread(&manager).await;
            // An edited notify.toml applies within this period.
            notifier.recheck();
            if let Err(error) = notify(&manager, &mut notifier).await {
                errors.note(&error, &log, &control);
            }
        }
        if periodic
            || poked
            || inbox_changed
            || badge.retry_in(started).is_some_and(|retry| retry.is_zero())
        {
            badge.count(&manager, started).await;
            if poked || inbox_changed {
                control.inbox_changed.notify_one();
                if !periodic
                    && let Err(error) = notify(&manager, &mut notifier).await
                {
                    errors.note(&error, &log, &control);
                }
            }
        }
        if started.saturating_sub(last_reopen) >= timing.reopen {
            if let Some(resident) = &manager.resident
                && let Ok(mut store) = resident.store.lock()
            {
                *store = None;
            }
            last_reopen = started;
        }
        bridge.reset_retry_after_stable_watch(started, timing.pass_every);
        if bridge.can_start(started) {
            match manager.bridge_endpoint().await {
                Ok(Some(endpoint)) => {
                    bridge.start(endpoint, bridge_notices.clone(), control.clone());
                }
                Ok(None) => bridge.disabled(started, timing.pass_every),
                Err(error) => {
                    bridge.failed(started);
                    errors.note(&format!("bridge endpoint: {error}"), &log, &control);
                }
            }
        }
        let expired = expiry.is_some_and(|at| started >= at);
        // The bridge carries pane output and lifecycle changes, but the
        // lightweight signature still catches a new foreground agent in an
        // unscoped pane. It is 5x less frequent than the old Linux path.
        let bridge_healthy = bridge.healthy;
        let signature_changed = if bridge_healthy
            && started.saturating_sub(last_list) >= timing.list_every
        {
            match manager
                .command(&["list-panes", "-a", "-F", SIGNATURE])
                .await
            {
                Ok(signature) => {
                    last_list = started;
                    let changed = signature != previous;
                    previous = signature;
                    changed
                }
                Err(error) => {
                    errors.note(&error, &log, &control);
                    false
                }
            }
        } else {
            false
        };
        if signature_changed {
            bridge.signature_changed();
        }
        let bridge_event_due = bridge.event_pass_due(started);
        let bridge_safety_due =
            bridge_healthy && started.saturating_sub(last_pass) >= timing.pass_every;
        let bridge_sweep_due =
            bridge_healthy && started.saturating_sub(last_sweep) >= timing.ended_after;
        let (changed, manageable, fallback_panes) = if bridge_healthy {
            if !healthy_pass_needed(
                bridge.changed,
                bridge_event_due,
                force_pass,
                expired,
                bridge_safety_due,
                bridge_sweep_due,
            ) {
                if let Ok(mut report) = control.report.lock() {
                    report.ticks += 1;
                    report.last_tick_ms = started;
                }
                continue;
            }
            (bridge.changed, true, None)
        } else {
            // Read before listing, so output between the two is a change at
            // the next tick rather than lost. This is the pre-bridge polling
            // behavior and remains the complete fallback after stream loss.
            let stamps: HashMap<String, Option<i128>> = ttys
                .keys()
                .map(|tty| (tty.clone(), tty_mtime(tty)))
                .collect();
            let quiet = TTY_TIMES && stamps == ttys;
            let due = !quiet
                || force_pass
                || expired
                || started.saturating_sub(last_list) >= timing.list_every
                || started.saturating_sub(last_pass) >= timing.pass_every
                || started.saturating_sub(last_sweep) >= timing.ended_after;
            if !due {
                if let Ok(mut report) = control.report.lock() {
                    report.ticks += 1;
                    report.last_tick_ms = started;
                }
                continue;
            }
            let signature = match manager
                .command(&["list-panes", "-a", "-F", SIGNATURE])
                .await
            {
                Ok(signature) => signature,
                Err(error) => {
                    errors.note(&error, &log, &control);
                    continue;
                }
            };
            last_list = started;
            let changed = signature != previous;
            let listed: HashMap<String, Option<i128>> = signature
                .lines()
                .filter_map(|line| line.rsplit(' ').next())
                .filter(|tty| tty.starts_with('/'))
                .map(|tty| {
                    let stamp = stamps.get(tty).copied().unwrap_or_else(|| tty_mtime(tty));
                    (tty.to_owned(), stamp)
                })
                .collect();
            // A tty written but not yet read by the server lists the old
            // output generation: keep the old times once, so the next tick
            // looks again.
            if !changed
                && !held
                && listed
                    .iter()
                    .any(|(tty, stamp)| ttys.get(tty).is_some_and(|old| old != stamp))
            {
                held = true;
                ttys = listed
                    .into_iter()
                    .map(|(tty, stamp)| {
                        let old = ttys.get(&tty).copied().unwrap_or(stamp);
                        (tty, old)
                    })
                    .collect();
            } else {
                held = false;
                ttys = listed;
            }
            // Over the limit `collect` refuses; looking again every tick
            // would only start processes.
            let manageable = signature.lines().count() <= super::MAX_PANES;
            if !manageable {
                errors.note(
                    "too_many_panes: agent management supports at most 64 panes per server",
                    &log,
                    &control,
                );
            }
            let panes = signature
                .lines()
                .filter_map(|line| line.split(' ').next())
                .map(str::to_owned)
                .collect();
            (changed, manageable, Some((signature, panes)))
        };
        let targeted_pane = if bridge_healthy
            && bridge_event_due
            && !force_pass
            && !expired
            && !bridge_safety_due
            && !bridge_sweep_due
        {
            bridge.targeted_dirty_pane()
        } else {
            None
        };
        let pass = if bridge_healthy {
            healthy_pass_needed(
                bridge.changed,
                bridge_event_due,
                force_pass,
                expired,
                bridge_safety_due,
                bridge_sweep_due,
            )
        } else {
            manageable
                && (changed
                    || force_pass
                    || expired
                    || started.saturating_sub(last_pass) >= timing.pass_every)
        };
        let mut pass_succeeded = false;
        let mut pass_inbox_effects = false;
        if pass {
            if bridge_healthy && bridge.changed {
                bridge.pass_started(started);
            }
            if let Some(pane) = targeted_pane {
                match manager.resident_collect_pane(&pane).await {
                    Ok((agents, inbox_effects)) => {
                        reports.replace_pane(&pane, &agents);
                        observers
                            .sync_pane(&manager, &pane, &agents, &updates)
                            .await;
                        retry = false;
                        if let Ok(mut report) = control.report.lock() {
                            report.observed = observers.len();
                            report.passes += 1;
                            report.last_pass_ms = now_ms();
                        }
                        pass_inbox_effects = inbox_effects;
                        pass_succeeded = true;
                    }
                    Err(error) => {
                        errors.note(&error, &log, &control);
                        retry = !retry;
                    }
                }
            } else {
                match manager.resident_list().await {
                    Ok((agents, bridge_scope, core_boot_id, inbox_effects)) => {
                        retry = false;
                        reports.panes = agents
                            .iter()
                            .filter(|agent| super::observe::observable(agent))
                            .map(|agent| (agent.run.clone(), agent.pane_id.clone()))
                            .collect();
                        reports.agents = agents
                            .iter()
                            .filter(|agent| super::observe::observable(agent))
                            .map(|agent| (agent.run.clone(), agent.clone()))
                            .collect();
                        observers.sync(&manager, &agents, &updates).await;
                        if let Ok(mut report) = control.report.lock() {
                            report.observed = observers.len();
                        }
                        // A blocked agent may wait for hours and its event is
                        // already written; only work changes on its own.
                        busy = agents.iter().any(|agent| agent.state == "working");
                        notifier.names = agents
                            .iter()
                            .map(|agent| (agent.run.clone(), agent.name.clone()))
                            .collect();
                        expiry = report_expiry(&agents, now_ms());
                        bridge.observe_boot(core_boot_id);
                        bridge.set_scope(bridge_scope);
                        if let Ok(mut report) = control.report.lock() {
                            report.passes += 1;
                            report.last_pass_ms = now_ms();
                        }
                        pass_inbox_effects = inbox_effects;
                        pass_succeeded = true;
                    }
                    Err(error) => {
                        errors.note(&error, &log, &control);
                        // Once more at the next tick, then wait for the next
                        // change: the change that called this pass is not lost,
                        // and a lasting failure does not start a process a tick.
                        retry = !retry;
                    }
                }
            }
            last_pass = started;
            if changed {
                hot_until = now_ms() + timing.hot;
            }
        }
        if bridge_healthy && pass {
            if pass_succeeded {
                bridge.pass_succeeded();
            } else if bridge.changed {
                bridge.pass_failed();
            }
        }
        let inbox_actions = post_pass_inbox_actions(pass_succeeded, pass_inbox_effects);
        if inbox_actions.count {
            // The pass's own inbox writes also marked the resident dirty; the
            // follow-up below covers them, so the next loop need not repeat
            // it. Nothing else writes the inbox on this task in between.
            if let Some(resident) = &manager.resident {
                resident.take_inbox_dirty();
            }
            badge.count(&manager, now_ms()).await;
        }
        if inbox_actions.signal {
            control.inbox_changed.notify_one();
        }
        if inbox_actions.notify
            && let Err(error) = notify(&manager, &mut notifier).await
        {
            errors.note(&error, &log, &control);
        }
        // The first successful pass is what discovers the managed pane scope.
        // Start its lifecycle stream immediately instead of waiting a quiet
        // tick before the initial bridge handshake.
        let bridge_now = now_ms();
        if bridge.can_start(bridge_now) {
            match manager.bridge_endpoint().await {
                Ok(Some(endpoint)) => {
                    bridge.start(endpoint, bridge_notices.clone(), control.clone());
                }
                Ok(None) => bridge.disabled(bridge_now, timing.pass_every),
                Err(error) => {
                    bridge.failed(bridge_now);
                    errors.note(&format!("bridge endpoint: {error}"), &log, &control);
                }
            }
        }
        let sweep_due = if bridge_healthy {
            bridge_sweep_due || (pass_succeeded && bridge.lifecycle_changed)
        } else {
            fallback_panes.as_ref().is_some_and(|(_, panes)| {
                manageable
                    && (panes != &previous_panes
                        || started.saturating_sub(last_sweep) >= timing.ended_after)
            })
        };
        if sweep_due {
            last_sweep = started;
            // A failed sweep follows the existing timed retry cadence instead
            // of turning one lifecycle event into a list-panes loop.
            if bridge_healthy {
                bridge.lifecycle_changed = false;
            }
            match sweep(&manager, &mut absent, started, timing.ended_after).await {
                Ok(ended) => {
                    if let Ok(mut report) = control.report.lock() {
                        report.ended_runs += ended as u64;
                    }
                }
                Err(error) => {
                    if error.starts_with(STORE_NEWER) {
                        log(&format!("{error}; a newer masil-agent must take over"));
                        stop.notify_one();
                        return;
                    }
                    errors.note(&error, &log, &control);
                }
            }
        }
        if let Some((signature, panes)) = fallback_panes {
            previous = signature;
            previous_panes = panes;
        }
        let (count, last) = manager.resident.as_ref().map_or((0, None), |resident| {
            (
                resident.stood_back.load(Ordering::Relaxed),
                resident
                    .last_stood_back
                    .lock()
                    .ok()
                    .and_then(|last| last.clone()),
            )
        });
        if count > stood_back
            && let Some((pane, _)) = &last
        {
            log(&format!(
                "watch: stood back from {pane}: another process judged its screen differently"
            ));
        }
        stood_back = count;
        if let Ok(mut report) = control.report.lock() {
            report.ticks += 1;
            report.last_tick_ms = started;
            report.stood_back = count;
            report.last_stood_back = last;
        }
    }
}

/// Reports from `--answers` OpenCode servers, written at most once a second
/// per run since every option write redraws every client. A newer state
/// replaces one still waiting.
#[derive(Default)]
struct Reports {
    /// Observed runs and their panes, from the last pass.
    panes: HashMap<String, String>,
    /// The observed agents as the last pass read them. A write against an
    /// older copy is refused by its guard and tried again after the next pass.
    agents: HashMap<String, Agent>,
    waiting: HashMap<String, Wanted>,
    written: HashMap<String, u64>,
    /// Writes refused in a row; a run is given up after a few, until its
    /// next state.
    failures: HashMap<String, u32>,
    /// User messages the servers recorded, kept while a delivered prompt
    /// may still be matched to them, with when they arrived.
    prompted: HashMap<String, Vec<(super::observe::Prompted, u64)>>,
    /// A message came since the last match.
    recheck: bool,
    matched_at: u64,
}

const REPORT_TRIES: u32 = 5;
/// How long a user message waits for the prompt masil delivered.
const PROMPT_KEEP_MS: u64 = 120_000;

#[derive(Clone, Copy)]
enum Wanted {
    State(super::observe::ApiState),
    /// The connection is lost: the screen decides again.
    Clear,
}

impl Reports {
    async fn apply(
        &mut self,
        manager: &Manager,
        update: super::observe::Update,
        observer: super::Observer,
    ) {
        use super::observe::Update;
        match update {
            Update::State { run, state } => {
                self.failures.remove(&run);
                self.waiting.insert(run, Wanted::State(state));
            }
            Update::Effects(effects) => {
                manager.inbox_apply(effects).await;
            }
            // One event per loss: a later loss in the same run is new.
            Update::Lost { run, since } => {
                if let Some(pane) = self.panes.get(&run) {
                    manager
                        .inbox_apply(vec![super::inbox::Effect::Event {
                            source: "api:opencode".into(),
                            source_ref: format!("observation:{run}:{since}"),
                            provider: "opencode".into(),
                            pane: pane.clone(),
                            run: run.clone(),
                            revision: None,
                            kind: "observation_lost",
                            native_ref: None,
                            summary: None,
                        }])
                        .await;
                }
                self.failures.remove(&run);
                self.waiting.insert(run, Wanted::Clear);
            }
            Update::Prompted { run, message } => {
                // A reconnect's catch-up can report a message again.
                let messages = self.prompted.entry(run).or_default();
                if !messages
                    .iter()
                    .any(|(kept, _)| kept.message == message.message)
                {
                    messages.push((message, now_ms()));
                    self.recheck = true;
                }
            }
            Update::Restored { run } => {
                manager
                    .inbox_apply(vec![super::inbox::Effect::ResolveRun {
                        run,
                        kinds: &["observation_lost"],
                        resolution: "restored",
                    }])
                    .await;
            }
        }
        self.flush(manager, observer).await;
    }

    /// Matches the kept user messages with delivered prompts: a match is
    /// `native_accepted`, with the message as its turn. At most twice a
    /// second; a message that matches nothing waits for the next delivery.
    async fn accept(&mut self, manager: &Manager, woken: bool) {
        let now = now_ms();
        let panes = &self.panes;
        self.prompted.retain(|run, messages| {
            messages.retain(|(_, at)| now < at + PROMPT_KEEP_MS);
            !messages.is_empty() && panes.contains_key(run)
        });
        if self.prompted.is_empty() || !(self.recheck || woken) || now < self.matched_at + 500 {
            return;
        }
        self.recheck = false;
        self.matched_at = now;
        for (run, messages) in &mut self.prompted {
            let reports: Vec<_> = messages
                .iter()
                .map(|(message, _)| super::prompt::Reported {
                    digest: message.digest.clone(),
                    created_ms: Some(message.created_ms),
                    evidence: json!({"turn": message.message, "session": message.session}),
                })
                .collect();
            let Ok(results) = manager.accept_prompts(run, &reports, "provider_api").await else {
                continue;
            };
            let mut results = results.into_iter();
            messages.retain(|_| {
                !matches!(results.next(), Some(super::prompt::Acceptance::Accepted(_)))
            });
        }
    }

    fn retry_at(&self) -> Option<u64> {
        self.waiting
            .keys()
            .map(|run| {
                self.written
                    .get(run)
                    .copied()
                    .unwrap_or(0)
                    .saturating_add(1_000)
            })
            .min()
    }

    fn retry_in(&self, now: u64) -> Option<Duration> {
        self.retry_at()
            .map(|at| Duration::from_millis(at.saturating_sub(now)))
    }

    /// A prompted update asks `accept` to run again once its rate limit ends.
    fn accept_at(&self) -> Option<u64> {
        (!self.prompted.is_empty() && self.recheck).then(|| self.matched_at.saturating_add(500))
    }

    /// Prompt messages are normally dropped by the next tick. When quiet
    /// healthy bridge mode sleeps longer, preserve that expiry explicitly.
    fn prompt_expiry_at(&self) -> Option<u64> {
        self.prompted
            .values()
            .flat_map(|messages| messages.iter())
            .map(|(_, at)| at.saturating_add(PROMPT_KEEP_MS))
            .min()
    }

    /// Drops reports tied to an old PTY before the next native pass decides
    /// whether that pane still holds the same managed run.
    fn invalidate_pane(&mut self, pane: &str) {
        let runs: Vec<String> = self
            .panes
            .iter()
            .filter(|(_, candidate)| candidate.as_str() == pane)
            .map(|(run, _)| run.clone())
            .collect();
        for run in runs {
            self.panes.remove(&run);
            self.agents.remove(&run);
            self.waiting.remove(&run);
            self.written.remove(&run);
            self.failures.remove(&run);
            self.prompted.remove(&run);
        }
    }

    /// Replace only one pane's observable runs after a targeted bridge pass.
    fn replace_pane(&mut self, pane: &str, agents: &[Agent]) {
        self.panes.retain(|_, candidate| candidate != pane);
        self.agents.retain(|_, agent| agent.pane_id != pane);
        for agent in agents.iter().filter(|agent| super::observe::observable(agent)) {
            self.panes.insert(agent.run.clone(), agent.pane_id.clone());
            self.agents.insert(agent.run.clone(), agent.clone());
        }
    }

    async fn flush(&mut self, manager: &Manager, observer: super::Observer) {
        let now = now_ms();
        self.waiting.retain(|run, _| self.panes.contains_key(run));
        let due: Vec<String> = self
            .waiting
            .keys()
            .filter(|run| self.written.get(*run).is_none_or(|at| now >= at + 1_000))
            .cloned()
            .collect();
        for run in due {
            let Some(wanted) = self.waiting.remove(&run) else {
                continue;
            };
            let Some(agent) = self.agents.get_mut(&run) else {
                continue;
            };
            // A closing report holds only while the screen stays as it is
            // now, so it needs the current output generation.
            if matches!(wanted, Wanted::State(state) if !state.live)
                && let Ok(fresh) = manager.get(&agent.pane_id).await
                && fresh.run == run
            {
                *agent = fresh;
            }
            let (state, generation) = match wanted {
                Wanted::State(state) => (state, Some(agent.output_generation)),
                Wanted::Clear => (
                    super::observe::ApiState {
                        state: "unknown",
                        permissions: 0,
                        questions: 0,
                        live: false,
                    },
                    None,
                ),
            };
            match manager.report_api(agent, state, observer, generation).await {
                Ok(written) => {
                    self.failures.remove(&run);
                    if written {
                        self.written.insert(run, now_ms());
                    }
                }
                // Kept for the next second unless a newer state came
                // meanwhile: a write that lost to a hook's is tried again,
                // against the pane read again.
                Err(_) => {
                    if let Ok(fresh) = manager.get(&agent.pane_id).await
                        && fresh.run == run
                    {
                        *agent = fresh;
                    }
                    let failures = self.failures.entry(run.clone()).or_default();
                    *failures += 1;
                    if *failures < REPORT_TRIES {
                        self.waiting.entry(run.clone()).or_insert(wanted);
                    }
                    self.written.insert(run, now_ms());
                }
            }
        }
        self.written.retain(|run, _| self.panes.contains_key(run));
        self.failures.retain(|run, _| self.panes.contains_key(run));
    }
}

/// Applies notices that the bridge reader queued before it woke the resident.
/// The debounce happens in the caller's normal `control.wake` branch;
/// this function performs no I/O and so can drain a burst atomically.
struct BridgeNoticeContext<'a, F> {
    manager: &'a Manager,
    observers: &'a mut super::observe::Observers,
    reports: &'a mut Reports,
    errors: &'a mut ErrorLog,
    log: &'a F,
    control: &'a WatchControl,
}

fn process_bridge_notices<F: Fn(&str)>(
    bridge: &mut BridgeState,
    notices: &mut mpsc::UnboundedReceiver<BridgeNotice>,
    context: BridgeNoticeContext<'_, F>,
    now: u64,
) -> bool {
    let mut woken = false;
    let mut invalidated = HashSet::new();
    while let Ok(notice) = notices.try_recv() {
        match notice {
            BridgeNotice::Ready { scope } if scope == bridge.scope => {
                bridge.ready(now);
                woken = true;
            }
            // An old task can report ready just as a scope change asks it to
            // resubscribe. It is not a healthy projection for the new scope.
            BridgeNotice::Ready { .. } => {}
            BridgeNotice::Event(event) => {
                let action = bridge_event_action(&event);
                bridge.note_event(&event);
                bridge.lifecycle_changed |= action.lifecycle_changed;
                if let Some(pane) = action.invalidate_pane {
                    invalidated.insert(pane);
                }
                woken = true;
            }
            BridgeNotice::Failed(error) => {
                bridge.failed(now);
                context
                    .errors
                    .note(&format!("bridge: {error}"), context.log, context.control);
                woken = true;
            }
        }
    }
    for pane in invalidated {
        context.manager.invalidate_pane_observation(&pane);
        context.observers.invalidate_pane(&pane);
        context.reports.invalidate_pane(&pane);
    }
    woken
}

/// Claims the events recorded since the last claim and hands them to the
/// notification routes (P7a). Costs one read while nothing is new.
async fn notify(manager: &Manager, notifier: &mut crate::notify::Notifier) -> Result<(), String> {
    let Some(settings) = notifier.settings()? else {
        return Ok(());
    };
    let (kinds, routes) = (settings.events.clone(), settings.routes());
    let now = now_ms();
    let stale_before = now.saturating_sub(crate::notify::STALE_MS);
    let claimed = manager
        .with_resident_store(move |store| {
            let claim = store.notify_claim(&kinds, &routes, stale_before, now)?;
            // Claimed is committed: without the kept environment the
            // routes still run, with less.
            let env = if claim.events.is_empty() && claim.stale == 0 {
                Vec::new()
            } else {
                store.notify_env().unwrap_or_default()
            };
            Ok((claim, env))
        })
        .await;
    if let Some((claim, kept)) = claimed?
        && (!claim.events.is_empty() || claim.stale > 0)
    {
        let env = notifier.environment(kept).await;
        let skipped = notifier.send(claim.events, claim.stale, settings, env);
        if !skipped.is_empty() {
            record_outcome(manager, (skipped, "skipped_backlog".into())).await?;
        }
    }
    Ok(())
}

/// A test through every route, in the environment real notifications use;
/// the reply comes when all are done. Never blocks the watch.
async fn notify_test(
    manager: &Manager,
    notifier: &mut crate::notify::Notifier,
    reply: tokio::sync::oneshot::Sender<serde_json::Value>,
) {
    let settings = match crate::notify::Settings::load() {
        Ok(settings) => std::sync::Arc::new(settings),
        Err(error) => {
            let _ = reply.send(serde_json::json!({"error": error}));
            return;
        }
    };
    let kept = manager
        .with_resident_store(|store| store.notify_env())
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    let env = notifier.environment(kept).await;
    notifier.test(settings, env, reply);
}

async fn record_outcome(
    manager: &Manager,
    (rows, outcome): crate::notify::Outcome,
) -> Result<(), String> {
    manager
        .with_resident_store(move |store| store.notify_outcome(&rows, &outcome))
        .await
        .map(drop)
}

/// The status-line badge: the unseen count in a server option, written
/// only when it changes and at most once a second, since every option write
/// redraws every client.
struct Badge {
    /// The value the server shows; None when unknown.
    shown: Option<i64>,
    /// A value waiting for the once-a-second limit.
    pending: Option<i64>,
    last_write: u64,
    counted: u64,
    reread_at: u64,
}

const BADGE_GAP_MS: u64 = 1_000;

impl Badge {
    async fn read(manager: &Manager) -> Self {
        let shown = manager
            .command(&["show-options", "-gqv", crate::coordinator::BADGE_OPTION])
            .await
            .ok()
            .map(|text| text.trim().parse::<i64>().unwrap_or(0));
        Self {
            shown,
            pending: None,
            last_write: 0,
            counted: 0,
            reread_at: 0,
        }
    }

    fn retry_at(&self) -> Option<u64> {
        self.pending
            .map(|_| self.last_write.saturating_add(BADGE_GAP_MS))
    }

    fn retry_in(&self, now: u64) -> Option<Duration> {
        self.retry_at()
            .map(|at| Duration::from_millis(at.saturating_sub(now)))
    }

    async fn count(&mut self, manager: &Manager, now: u64) {
        self.counted = now;
        let Ok(count) = manager
            .with_resident_store(|store| store.inbox_unseen_count())
            .await
        else {
            // Wait a gap before trying again, never in a tight loop.
            self.last_write = now;
            return;
        };
        self.update(manager, count.unwrap_or(0), now).await;
    }

    /// Reads the shown value again: another process (`inbox status`, a
    /// stale coordinator) may have changed it.
    async fn reread(&mut self, manager: &Manager) {
        if let Ok(text) = manager
            .command(&["show-options", "-gqv", crate::coordinator::BADGE_OPTION])
            .await
        {
            self.shown = Some(text.trim().parse::<i64>().unwrap_or(0));
        }
    }

    async fn update(&mut self, manager: &Manager, count: i64, now: u64) {
        if self.shown == Some(count) {
            self.pending = None;
            return;
        }
        if now.saturating_sub(self.last_write) < BADGE_GAP_MS {
            self.pending = Some(count);
            return;
        }
        let value = count.to_string();
        let args: &[&str] = if count == 0 {
            &["set-option", "-gqu", crate::coordinator::BADGE_OPTION]
        } else {
            &[
                "set-option",
                "-gq",
                crate::coordinator::BADGE_OPTION,
                &value,
            ]
        };
        // A failed write is retried after a gap, never in a tight loop.
        self.last_write = now;
        if manager.command(args).await.is_ok() {
            self.shown = Some(count);
            self.pending = None;
        } else {
            self.pending = Some(count);
        }
    }

    async fn clear(&mut self, manager: &Manager) {
        if self.shown != Some(0) {
            let _ = manager
                .command(&["set-option", "-gqu", crate::coordinator::BADGE_OPTION])
                .await;
        }
    }
}

/// When the earliest report that still overrides a screen stops doing so.
/// `collect` lets a report win for REPORT_FRESH_MS without any output; a
/// held turn ends on a quiet screen, which no output announces, so it is
/// looked at again every TURN_QUIET_SECONDS.
fn report_expiry(agents: &[Agent], now: u64) -> Option<u64> {
    agents
        .iter()
        .filter_map(|agent| {
            let report = agent.metadata.as_ref()?.report.as_ref()?;
            let held = agent
                .run_evidence
                .as_ref()
                .and_then(|evidence| evidence.report.as_ref())
                .is_some_and(|source| source.sequence == report.sequence && source.hold.is_some());
            Some(if held {
                now.saturating_add(super::TURN_QUIET_SECONDS * 1000 + 1)
                    .min(report.at.saturating_add(super::TURN_HOLD_MAX_MS + 1))
            } else {
                report.at.saturating_add(super::REPORT_FRESH_MS + 1)
            })
        })
        .filter(|at| *at > now)
        .min()
}

/// Resolves the open events of runs that every successful look has missed
/// for `ended_after`. Returns how many runs ended.
async fn sweep(
    manager: &Manager,
    absent: &mut HashMap<String, u64>,
    seen_at: u64,
    ended_after: u64,
) -> Result<usize, String> {
    let (live, _) = manager.live_runs().await?;
    // The secret of an `--answers` run that ended goes with it; the last one
    // gone turns the `answers` feature off. A failure here does not hold up
    // the inbox's ended runs; the next sweep tries again.
    let _ = manager.prune_answer_secrets(&live).await;
    super::tokens::prune_ended(&manager.native.socket, &live);
    let Some(open) = manager
        .with_resident_store(|store| store.open_runs())
        .await?
    else {
        absent.clear();
        return Ok(0);
    };
    let ended = ended_runs(absent, &live, &open, seen_at, ended_after);
    if ended.is_empty() {
        return Ok(0);
    }
    let count = ended.len();
    manager
        .with_resident_store(move |store| {
            let now = now_ms();
            for (run, since) in ended {
                store.resolve_ended_run(&run, since, now)?;
            }
            Ok(())
        })
        .await?;
    Ok(count)
}

/// Updates the missing-since times and returns the runs to resolve, each with
/// the time it was first missed; events seen after that are kept.
fn ended_runs(
    absent: &mut HashMap<String, u64>,
    live: &HashSet<String>,
    open: &[String],
    now: u64,
    ended_after: u64,
) -> Vec<(String, u64)> {
    absent.retain(|run, _| open.contains(run) && !live.contains(run));
    for run in open.iter().filter(|run| !live.contains(*run)) {
        absent.entry(run.clone()).or_insert(now);
    }
    let ended: Vec<(String, u64)> = absent
        .iter()
        .filter(|(_, since)| now.saturating_sub(**since) >= ended_after)
        .map(|(run, since)| (run.clone(), *since))
        .collect();
    for (run, _) in &ended {
        absent.remove(run);
    }
    ended
}

/// One log line per error code per minute.
#[derive(Default)]
struct ErrorLog {
    last: HashMap<String, u64>,
}

impl ErrorLog {
    fn note(&mut self, error: &str, log: &impl Fn(&str), control: &WatchControl) {
        let now = now_ms();
        let code = error.split(':').next().unwrap_or(error).to_owned();
        if let Ok(mut report) = control.report.lock() {
            report.last_error = Some(error.chars().take(512).collect());
            report.last_error_ms = now;
        }
        let due = self
            .last
            .get(&code)
            .is_none_or(|at| now.saturating_sub(*at) >= 60_000);
        if due {
            self.last.insert(code, now);
            log(&format!("watch: {error}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STAND_BACK: u64 = 60_000;
    const ENDED_AFTER: u64 = 30_000;

    fn healthy_idle_state() -> HealthyBridgeWaitState {
        HealthyBridgeWaitState {
            healthy: true,
            busy: false,
            hot_until: 0,
            retry: false,
            inbox_dirty: false,
            changed: false,
            event_failed: false,
            last_event_pass: None,
            longest_wait: Duration::from_secs(86_400),
        }
    }

    fn healthy_idle_deadlines(at: u64) -> HealthyBridgeDeadlines {
        HealthyBridgeDeadlines {
            periodic: at,
            reopen: at,
            list: at,
            pass: at,
            sweep: at,
            report_expiry: Some(at),
            badge_retry: Some(at),
            report_retry: Some(at),
            report_accept: Some(at),
            prompt_expiry: Some(at),
            stable_watch: Some(at),
        }
    }

    fn assert_healthy_idle_wait(now: u64, at: u64, deadlines: HealthyBridgeDeadlines) {
        assert_eq!(
            healthy_bridge_wait_interval(
                Duration::from_secs(2),
                now,
                healthy_idle_state(),
                deadlines,
            ),
            Duration::from_millis(at - now)
        );
    }

    #[test]
    fn timings_scale_with_the_tick() {
        let timing = Timing::new(Duration::from_secs(2));
        assert_eq!(
            (timing.stand_back, timing.pass_every, timing.ended_after),
            (60_000, 60_000, 30_000)
        );
        assert_eq!(Timing::new(Duration::from_millis(200)).ended_after, 3_000);
    }

    #[test]
    fn the_coordinator_stands_back_where_another_writer_disagrees() {
        let mut memo = PaneMemo {
            output: 4,
            tracked: "ours".into(),
            since: None,
        };
        // Same output and the value it left: it may write.
        assert_eq!(
            decide(Some(&mut memo), 4, "ours", 100, STAND_BACK),
            (true, false)
        );
        // Another writer changed it under the same output: stand back.
        assert_eq!(
            decide(Some(&mut memo), 4, "theirs", 200, STAND_BACK),
            (false, true)
        );
        assert_eq!(
            decide(
                Some(&mut memo),
                4,
                "theirs",
                200 + STAND_BACK - 1,
                STAND_BACK
            ),
            (false, false)
        );
        // Each pass remembers the value it found, here the other writer's.
        memo.tracked = "theirs".into();
        // Expired with nothing new from the other writer: one write goes.
        assert_eq!(
            decide(Some(&mut memo), 4, "theirs", 200 + STAND_BACK, STAND_BACK),
            (true, false)
        );
        // That write is remembered; the other writer answers it: stand back
        // again, so the two never trade writes faster than once a period.
        memo.tracked = "ours-again".into();
        assert_eq!(
            decide(Some(&mut memo), 4, "theirs", 300 + STAND_BACK, STAND_BACK),
            (false, true)
        );
        // New output: judge afresh.
        assert_eq!(
            decide(Some(&mut memo), 5, "theirs", 300, STAND_BACK),
            (true, false)
        );
        assert_eq!(decide(None, 5, "x", 300, STAND_BACK), (true, false));
    }

    #[test]
    fn a_run_ends_only_after_it_stays_missing() {
        let mut absent = HashMap::new();
        let live: HashSet<String> = ["r1".to_owned()].into();
        let open = ["r1".to_owned(), "r2".to_owned()];
        assert!(ended_runs(&mut absent, &live, &open, 1_000, ENDED_AFTER).is_empty());
        assert_eq!(absent.get("r2"), Some(&1_000));
        // Back again: forgotten.
        let both: HashSet<String> = ["r1".to_owned(), "r2".to_owned()].into();
        assert!(ended_runs(&mut absent, &both, &open, 2_000, ENDED_AFTER).is_empty());
        assert!(absent.is_empty());
        assert!(ended_runs(&mut absent, &live, &open, 3_000, ENDED_AFTER).is_empty());
        assert!(
            ended_runs(
                &mut absent,
                &live,
                &open,
                3_000 + ENDED_AFTER - 1,
                ENDED_AFTER
            )
            .is_empty()
        );
        assert_eq!(
            ended_runs(&mut absent, &live, &open, 3_000 + ENDED_AFTER, ENDED_AFTER),
            [("r2".to_owned(), 3_000)]
        );
        assert!(absent.is_empty());
    }

    #[test]
    fn bridge_retry_backs_off_then_resets_for_a_fresh_watch() {
        let mut retry = BridgeRetry::default();
        assert!(retry.due(0));
        retry.failed(10);
        assert_eq!((retry.next_at, retry.delay), (1_010, 2_000));
        assert!(!retry.due(1_009));
        assert!(retry.due(1_010));
        for _ in 0..8 {
            retry.failed(retry.next_at);
        }
        assert_eq!(retry.delay, 30_000);
        retry.unavailable(100, 60_000);
        assert_eq!((retry.next_at, retry.delay), (60_100, 1_000));
        retry.succeeded();
        assert_eq!(retry, BridgeRetry::default());
    }

    #[test]
    fn bridge_ready_keeps_a_persistent_wake_until_a_pass_runs() {
        let mut bridge = BridgeState::default();
        bridge.ready(10);
        assert!(healthy_pass_needed(
            bridge.changed,
            bridge.event_pass_due(10),
            false,
            false,
            false,
            false,
        ));
        bridge.pass_started(10);
        bridge.pass_succeeded();
        assert!(!bridge.changed);
    }

    #[test]
    fn bridge_event_passes_coalesce_for_half_a_second() {
        let mut bridge = BridgeState::default();
        bridge.ready(0);
        assert!(bridge.event_pass_due(0));
        bridge.pass_started(0);
        bridge.pass_succeeded();
        bridge.scope.insert("%4".into());
        bridge.note_event(&Event::Pane {
            pane_id: "%4".into(),
            reason: PaneReason::ScreenDirty,
        });
        assert!(!bridge.event_pass_due(BRIDGE_EVENT_PASS_MS - 1));
        assert!(bridge.event_pass_due(BRIDGE_EVENT_PASS_MS));
    }

    #[test]
    fn bridge_targets_exactly_one_dirty_pane_and_uses_full_for_many() {
        let mut bridge = BridgeState::default();
        bridge.changed = true;
        bridge.dirty_panes.insert("%4".into());
        assert_eq!(bridge.targeted_dirty_pane(), Some("%4".into()));
        bridge.dirty_panes.insert("%5".into());
        assert_eq!(bridge.targeted_dirty_pane(), None);
        bridge.dirty_panes.remove("%5");
        bridge.needs_full_pass = true;
        assert_eq!(bridge.targeted_dirty_pane(), None);
    }

    #[test]
    fn bridge_wait_is_clamped_to_the_remaining_event_interval() {
        let tick = Duration::from_secs(2);
        assert_eq!(
            bridge_wait_interval(tick, true, true, false, Some(1_000), 1_100),
            Duration::from_millis(200)
        );
        assert_eq!(
            bridge_wait_interval(tick, true, true, false, Some(1_000), 1_500),
            Duration::ZERO
        );
        assert_eq!(
            bridge_wait_interval(tick, false, true, false, Some(1_000), 1_100),
            tick
        );
    }

    #[test]
    fn healthy_idle_deadline_tracks_every_periodic_duty() {
        let now = 10_000;
        let due = now + 250;
        let far = now + 100_000;
        let base = healthy_idle_deadlines(far);
        for deadlines in [
            HealthyBridgeDeadlines {
                periodic: due,
                ..base
            },
            HealthyBridgeDeadlines {
                reopen: due,
                ..base
            },
            HealthyBridgeDeadlines { list: due, ..base },
            HealthyBridgeDeadlines { pass: due, ..base },
            HealthyBridgeDeadlines { sweep: due, ..base },
            HealthyBridgeDeadlines {
                report_expiry: Some(due),
                ..base
            },
            HealthyBridgeDeadlines {
                badge_retry: Some(due),
                ..base
            },
            HealthyBridgeDeadlines {
                report_retry: Some(due),
                ..base
            },
            HealthyBridgeDeadlines {
                report_accept: Some(due),
                ..base
            },
            HealthyBridgeDeadlines {
                prompt_expiry: Some(due),
                ..base
            },
            HealthyBridgeDeadlines {
                stable_watch: Some(due),
                ..base
            },
        ] {
            assert_healthy_idle_wait(now, due, deadlines);
        }
    }

    #[test]
    fn healthy_idle_deadline_skips_quiet_ticks_until_the_signature_audit() {
        let now = 10_000;
        let deadlines = HealthyBridgeDeadlines {
            periodic: now + 30_000,
            reopen: now + 30_000,
            list: now + 10_000,
            pass: now + 60_000,
            sweep: now + 30_000,
            report_expiry: None,
            badge_retry: None,
            report_retry: None,
            report_accept: None,
            prompt_expiry: None,
            stable_watch: None,
        };
        assert_healthy_idle_wait(now, now + 10_000, deadlines);
    }

    #[test]
    fn healthy_idle_deadline_retains_fixed_tick_for_active_or_overdue_work() {
        let now = 10_000;
        let tick = Duration::from_secs(2);
        let deadlines = healthy_idle_deadlines(now + 100_000);
        let mut state = healthy_idle_state();
        state.healthy = false;
        assert_eq!(
            healthy_bridge_wait_interval(tick, now, state, deadlines),
            tick
        );
        state = healthy_idle_state();
        state.busy = true;
        assert_eq!(
            healthy_bridge_wait_interval(tick, now, state, deadlines),
            tick
        );
        state = healthy_idle_state();
        state.hot_until = now + 1;
        assert_eq!(
            healthy_bridge_wait_interval(tick, now, state, deadlines),
            tick
        );
        state = healthy_idle_state();
        state.retry = true;
        assert_eq!(
            healthy_bridge_wait_interval(tick, now, state, deadlines),
            tick
        );
        state = healthy_idle_state();
        state.inbox_dirty = true;
        assert_eq!(
            healthy_bridge_wait_interval(tick, now, state, deadlines),
            tick
        );
        state = healthy_idle_state();
        state.changed = true;
        state.event_failed = true;
        state.last_event_pass = Some(now);
        assert_eq!(
            healthy_bridge_wait_interval(tick, now, state, deadlines),
            tick
        );
        let overdue = HealthyBridgeDeadlines {
            periodic: now,
            ..deadlines
        };
        assert_eq!(
            healthy_bridge_wait_interval(tick, now, healthy_idle_state(), overdue),
            tick
        );
    }

    #[test]
    fn healthy_idle_deadline_wakes_for_a_coalesced_bridge_event() {
        let now = 10_000;
        let state = HealthyBridgeWaitState {
            changed: true,
            last_event_pass: Some(now),
            ..healthy_idle_state()
        };
        let deadlines = healthy_idle_deadlines(now + 100_000);
        assert_eq!(
            healthy_bridge_wait_interval(Duration::from_secs(2), now, state, deadlines),
            Duration::from_millis(BRIDGE_EVENT_PASS_MS)
        );
        assert_eq!(
            healthy_bridge_wait_interval(
                Duration::from_secs(2),
                now + BRIDGE_EVENT_PASS_MS,
                state,
                deadlines,
            ),
            Duration::ZERO
        );
    }

    #[test]
    fn failed_bridge_event_pass_waits_for_the_normal_tick() {
        let tick = Duration::from_secs(2);
        let mut bridge = BridgeState::default();
        bridge.ready(1_000);
        bridge.pass_started(1_000);
        bridge.pass_failed();
        // Fresh notices after a failure are still limited to 2 Hz.
        assert!(!bridge.event_pass_due(1_100));
        assert!(bridge.event_pass_due(1_000 + BRIDGE_EVENT_PASS_MS));
        // The failure itself does not schedule a short event retry.
        assert_eq!(
            bridge_wait_interval(
                tick,
                bridge.healthy,
                bridge.changed,
                bridge.event_failed,
                bridge.last_event_pass,
                1_100,
            ),
            tick
        );
    }

    #[test]
    fn resident_inbox_dirty_is_consumed_once_even_with_a_pending_recount() {
        let control = WatchControl::default();
        let resident = Resident::new(Duration::from_secs(1));
        resident.mark_inbox_dirty();
        control.recount.store(true, Ordering::SeqCst);
        assert!(take_inbox_changes(&control, Some(&resident)));
        assert!(!take_inbox_changes(&control, Some(&resident)));

        resident.mark_inbox_dirty();
        assert!(take_inbox_changes(&control, Some(&resident)));
        assert!(!take_inbox_changes(&control, Some(&resident)));
    }

    #[test]
    fn successful_inbox_effects_drive_all_post_pass_follow_up() {
        let none = PostPassInboxActions {
            count: false,
            signal: false,
            notify: false,
        };
        assert_eq!(post_pass_inbox_actions(false, true), none);
        assert_eq!(post_pass_inbox_actions(true, false), none);
        assert_eq!(
            post_pass_inbox_actions(true, true),
            PostPassInboxActions {
                count: true,
                signal: true,
                notify: true,
            }
        );
    }

    #[test]
    fn bridge_backoff_resets_only_after_a_stable_watch() {
        let mut bridge = BridgeState::default();
        bridge.retry.failed(10);
        assert_eq!(bridge.retry.delay, 2_000);
        bridge.ready(20);
        bridge.reset_retry_after_stable_watch(79, 60);
        assert_eq!(bridge.retry.delay, 2_000);
        bridge.reset_retry_after_stable_watch(80, 60);
        assert_eq!(bridge.retry, BridgeRetry::default());
    }

    #[test]
    fn bridge_events_only_invalidate_replaced_or_removed_panes() {
        let pty = bridge_event_action(&Event::Pane {
            pane_id: "%4".into(),
            reason: PaneReason::PtyChanged,
        });
        assert_eq!(
            pty,
            BridgeEventAction {
                lifecycle_changed: false,
                invalidate_pane: Some("%4".into()),
            }
        );
        let removed = bridge_event_action(&Event::Pane {
            pane_id: "%4".into(),
            reason: PaneReason::Removed,
        });
        assert_eq!(
            removed,
            BridgeEventAction {
                lifecycle_changed: true,
                invalidate_pane: Some("%4".into()),
            }
        );
        assert_eq!(
            bridge_event_action(&Event::Pane {
                pane_id: "%4".into(),
                reason: PaneReason::ScreenDirty,
            }),
            BridgeEventAction::default()
        );
        assert!(
            bridge_event_action(&Event::WindowRemoved {
                window_id: "@2".into(),
            })
            .lifecycle_changed
        );
    }
}
