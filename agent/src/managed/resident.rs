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
use crate::observation::now_ms;
use serde::Serialize;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

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
const SIGNATURE: &str = "#{pane_id} #{pane_dead} #{masil_pty_generation} #{masil_foreground_pgid} #{pane_current_command} #{?#{||:#{@masil-managed-agent},#{@masil-managed-observation}},#{pane_output_generation} #{pane_tty},- -}";
/// Whether output shows in a tty's mtime promptly enough to skip listing.
const TTY_TIMES: bool = cfg!(target_os = "macos");

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
    let mut last_pass = 0;
    let mut last_list = 0;
    let mut ttys: HashMap<String, Option<i128>> = HashMap::new();
    let mut held = false;
    let mut retry = false;
    let mut expiry: Option<u64> = None;
    let mut last_sweep = 0;
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
        let mut woken = first || retry;
        if !first {
            // Updates are handled between ticks without moving the next one.
            let candidate = tokio::time::Instant::now() + interval;
            let deadline =
                next_tick.map_or(candidate, |at: tokio::time::Instant| at.min(candidate));
            next_tick = Some(deadline);
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => {}
                _ = control.wake.notified() => {
                    // Several pokes in a burst make one pass.
                    tokio::time::sleep(Duration::from_millis(250)).await;
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
        }
        first = false;
        if control.stopping.load(Ordering::SeqCst) {
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
        woken |= control.poked.swap(false, Ordering::SeqCst);
        // `agent prompt` pokes once it has delivered.
        reports.accept(&manager, woken).await;
        if control.reload.swap(false, Ordering::SeqCst) {
            if let Err(error) = manager.reload() {
                errors.note(&error, &log, &control);
            }
            notifier.reload();
            woken = true;
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
        if control.recount.swap(false, Ordering::SeqCst)
            || woken
            || periodic
            || badge.retry_in(started).is_some_and(|retry| retry.is_zero())
        {
            badge.count(&manager, started).await;
            control.inbox_changed.notify_one();
        }
        if started.saturating_sub(last_reopen) >= timing.reopen {
            if let Some(resident) = &manager.resident
                && let Ok(mut store) = resident.store.lock()
            {
                *store = None;
            }
            last_reopen = started;
        }
        let expired = expiry.is_some_and(|at| started >= at);
        // Read before listing, so output between the two is a change at the
        // next tick rather than lost.
        let stamps: HashMap<String, Option<i128>> = ttys
            .keys()
            .map(|tty| (tty.clone(), tty_mtime(tty)))
            .collect();
        let quiet = TTY_TIMES && stamps == ttys;
        let due = !quiet
            || woken
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
        // A tty written but not yet read by the server lists the old output
        // generation: keep the old times once, so the next tick looks again.
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
        // Over the limit `collect` refuses; looking again every tick would
        // only start processes.
        let manageable = signature.lines().count() <= super::MAX_PANES;
        if !manageable {
            errors.note(
                "too_many_panes: agent management supports at most 64 panes per server",
                &log,
                &control,
            );
        }
        let pass = manageable
            && (changed
                || woken
                || expired
                || started.saturating_sub(last_pass) >= timing.pass_every);
        if pass {
            match manager.list().await {
                Ok(agents) => {
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
                    if let Ok(mut report) = control.report.lock() {
                        report.passes += 1;
                        report.last_pass_ms = now_ms();
                    }
                }
                Err(error) => {
                    errors.note(&error, &log, &control);
                    // Once more at the next tick, then wait for the next
                    // change: the change that called this pass is not lost,
                    // and a lasting failure does not start a process a tick.
                    retry = !retry;
                }
            }
            last_pass = started;
            if changed {
                hot_until = now_ms() + timing.hot;
            }
            // A pass can record or resolve events.
            badge.count(&manager, now_ms()).await;
            control.inbox_changed.notify_one();
            if let Err(error) = notify(&manager, &mut notifier).await {
                errors.note(&error, &log, &control);
            }
        }
        let panes: HashSet<String> = signature
            .lines()
            .filter_map(|line| line.split(' ').next())
            .map(str::to_owned)
            .collect();
        if manageable
            && (panes != previous_panes || started.saturating_sub(last_sweep) >= timing.ended_after)
        {
            last_sweep = started;
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
        previous = signature;
        previous_panes = panes;
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
            Update::Effects(effects) => manager.inbox_apply(effects).await,
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

    fn retry_in(&self, now: u64) -> Option<Duration> {
        self.waiting
            .keys()
            .map(|run| {
                let at = self.written.get(run).map_or(0, |at| at + 1_000);
                Duration::from_millis(at.saturating_sub(now))
            })
            .min()
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

    fn retry_in(&self, now: u64) -> Option<Duration> {
        self.pending?;
        Some(Duration::from_millis(
            (self.last_write + BADGE_GAP_MS).saturating_sub(now),
        ))
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
}
