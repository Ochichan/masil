//! `agent list` and `agent get` answered by the coordinator.
//!
//! A command that starts for one query pays for spawning `masil list-panes`
//! and for parsing the inventory. The coordinator already holds a bridge
//! connection to the core, so it keeps a read-only snapshot of the agents and
//! checks it with one `format_digest` request, which spawns nothing. The
//! snapshot answers while the core's view of the panes is unchanged and the
//! snapshot is younger than `AGE_BOUND`; otherwise it is collected again.
//!
//! The reply is the text the local command would print, so the CLI writes it
//! unchanged. Anything the snapshot cannot answer is a refusal, and the CLI
//! then runs the local path, which also produces the authoritative errors.

use super::core_action::{CoordinatorActions, FormatDigest};
use super::{Agent, Manager, view};
use crate::detection::Engine;
use crate::observation::now_ms;
use serde::Serialize;
use serde_json::{Value, json};
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// How old a snapshot may be and still answer when the pane fields that
/// change only with the screen (output generation, last output time) and the
/// time-driven judgements may have moved.
pub(crate) const AGE_BOUND: Duration = Duration::from_secs(1);
/// A cached screen older than this is captured again on a recompute.
pub(crate) const SCREEN_MAX_AGE: Duration = Duration::from_secs(5);
/// Queries that may be in flight at once; more are refused as busy.
const MAX_IN_FLIGHT: usize = 4;
/// A reply carries at most this much text.
pub(crate) const MAX_ANSWER: usize = 1024 * 1024;
/// The control-socket frame a query reply may use: the text escaped, and
/// the envelope around it.
pub(crate) const FRAME: usize = MAX_ANSWER + 8 * 1024;
/// A new `get` or `list` of the same override files is not asked for a
/// reload again until this long after a request.
const RELOAD_RETRY: Duration = Duration::from_secs(2);

/// The inventory format without the output generation and the last output
/// time. Everything in it changes only when a pane's identity, its options or
/// its title changes, so a snapshot is checked against it on every query.
const CONTROL_FORMAT: &str = "#{q:pane_id}\t#{q:window_id}\t#{q:session_name}\t#{q:pane_pid}\t#{q:pane_dead}\t#{q:masil_core_boot_id}\t#{q:masil_pty_generation}\t#{q:pane_current_command}\t#{q:pane_current_path}\t#{q:pane_title}\t#{q:pane_tty}\t#{q:@masil-managed-agent}\t#{q:@masil-managed-observation}\t#{q:masil_foreground_pgid}\t#{q:masil_osc_progress}\t#{q:@masil-agent-run-evidence}\t#{q:@masil-agent-epoch}";
/// The global the `list` view is read from.
const VIEW_FORMAT: &str = "#{@masil-agent-view}";
/// The inventory fields `CONTROL_FORMAT` leaves out (see `FORMAT`).
const ACTIVITY_FIELDS: [usize; 2] = [14, 18];
const FIELDS: usize = 19;

/// What a query asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    List,
    Get,
}

struct Request {
    kind: Kind,
    target: Option<String>,
    state_dir: PathBuf,
    config_dir: Option<PathBuf>,
    fingerprint: String,
    exe: Value,
}

/// Why a query was not answered. The CLI runs the local path for any of
/// them. `reload` asks the coordinator to reload its detection overrides.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Refusal {
    pub(crate) code: &'static str,
    pub(crate) message: String,
    pub(crate) reload: bool,
}

impl Refusal {
    fn new(message: impl Into<String>) -> Self {
        Self {
            code: "query_refused",
            message: message.into(),
            reload: false,
        }
    }
}

fn parse(params: &Value) -> Result<Request, Refusal> {
    let text = |key: &str| {
        params[key]
            .as_str()
            .ok_or_else(|| Refusal::new(format!("{key} is missing")))
    };
    let kind = match text("kind")? {
        "list" => Kind::List,
        "get" => Kind::Get,
        other => return Err(Refusal::new(format!("unknown query kind {other}"))),
    };
    let target = params["target"].as_str().map(str::to_owned);
    match (kind, &target) {
        (Kind::Get, None) => return Err(Refusal::new("get needs a target")),
        (Kind::List, Some(_)) => return Err(Refusal::new("list takes no target")),
        _ => {}
    }
    Ok(Request {
        kind,
        target,
        state_dir: PathBuf::from(text("state_dir")?),
        config_dir: params["config_dir"].as_str().map(PathBuf::from),
        fingerprint: text("config_fingerprint")?.to_owned(),
        exe: params["exe_identity"].clone(),
    })
}

/// The control-socket parameters of a query, for the CLI.
pub(crate) fn request_params(kind: &str, target: Option<&str>) -> Option<Value> {
    let state = super::state_base().ok()?;
    let state = state.canonicalize().unwrap_or(state);
    let mut params = json!({
        "kind": kind,
        "state_dir": state,
        "config_dir": crate::detection::override_directory(),
        "config_fingerprint": crate::detection::override_fingerprint(),
        "exe_identity": crate::coordinator::exe_identity(),
    });
    if let Some(target) = target {
        params["target"] = json!(target);
    }
    Some(params)
}

/// Exactly what `agent list` prints for these agents.
pub(crate) fn render_list(agents: &[Agent]) -> Result<String, String> {
    #[derive(Serialize)]
    struct AgentList<'a> {
        agents: &'a [Agent],
    }
    render(&AgentList { agents })
}

/// Exactly what `agent get` prints for this agent.
pub(crate) fn render_get(agent: &Agent) -> Result<String, String> {
    render(&json!(agent))
}

fn render<T: Serialize + ?Sized>(value: &T) -> Result<String, String> {
    let mut text = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    text.push('\n');
    Ok(text)
}

fn sha256_hex(text: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// What `list-panes -a -F CONTROL_FORMAT` prints, from the text
/// `list-panes -a -F FORMAT` printed. None when the text is not a whole
/// inventory. A backslash escapes the next character, as tmux's `q:` does.
fn control_text(inventory: &str) -> Option<String> {
    let mut out = String::with_capacity(inventory.len());
    let mut row: Vec<&str> = Vec::with_capacity(FIELDS);
    let mut start = 0;
    let mut escaped = false;
    for (index, ch) in inventory.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '\t' | '\n' => {
                row.push(&inventory[start..index]);
                start = index + 1;
                if ch == '\n' {
                    if row.len() != FIELDS {
                        return None;
                    }
                    let kept: Vec<&str> = row
                        .iter()
                        .enumerate()
                        .filter(|(field, _)| !ACTIVITY_FIELDS.contains(field))
                        .map(|(_, text)| *text)
                        .collect();
                    out.push_str(&kept.join("\t"));
                    out.push('\n');
                    row.clear();
                }
            }
            _ => {}
        }
    }
    (!escaped && start == inventory.len() && row.is_empty()).then_some(out)
}

/// A read-only look at every agent, and what it was checked against.
struct Snapshot {
    /// SHA-256 of the inventory's control fields, as the core computes it.
    digest: String,
    /// The `@masil-agent-view` value the `list` text was made with.
    view_text: String,
    epoch: u64,
    valid_until: Instant,
    agents: Vec<Agent>,
    /// The `list` text, or why the saved view cannot be applied.
    list: Result<Arc<str>, String>,
}

impl Snapshot {
    /// Whether this snapshot still answers. The control digest is exact; the
    /// activity fields and the time-driven judgements are bounded by
    /// `valid_until`.
    fn valid(&self, digest: &str, view_text: &str, epoch: u64, now: Instant) -> bool {
        self.epoch == epoch
            && self.digest == digest
            && self.view_text == view_text
            && now < self.valid_until
    }
}

/// The earliest wall-clock time after `now` at which an agent's judgement
/// changes with time alone: a report going stale, a held turn's quiet check
/// or cap, a resume deadline.
fn earliest_transition_ms(agents: &[Agent], now: u64) -> Option<u64> {
    let mut earliest: Option<u64> = None;
    let mut note = |at: u64| {
        if at > now {
            earliest = Some(earliest.map_or(at, |current| current.min(at)));
        }
    };
    for agent in agents {
        if let Some(report) = agent.metadata.as_ref().and_then(|m| m.report.as_ref()) {
            let source = agent
                .run_evidence
                .as_ref()
                .and_then(|evidence| evidence.report.as_ref())
                .filter(|source| source.sequence == report.sequence);
            match source {
                // Decided by a process that is alive or not, not by a clock.
                Some(source) if source.source == super::REPORT_SOURCE_API => {}
                Some(source) if source.hold.is_some() => {
                    note(report.at.saturating_add(super::TURN_QUIET_SECONDS * 1000));
                    note(report.at.saturating_add(super::TURN_HOLD_MAX_MS + 1));
                }
                _ => note(report.at.saturating_add(super::REPORT_FRESH_MS + 1)),
            }
        }
        if let Some(deadline) = agent.binding.resume_deadline_ms {
            note(deadline);
        }
    }
    earliest
}

/// When a snapshot taken at `taken` (wall clock `wall_ms`) stops answering.
fn valid_until(taken: Instant, wall_ms: u64, agents: &[Agent]) -> Instant {
    let bound = earliest_transition_ms(agents, wall_ms).map_or(AGE_BOUND, |at| {
        Duration::from_millis(at - wall_ms).min(AGE_BOUND)
    });
    taken + bound
}

#[derive(Default)]
struct Stats {
    served: AtomicU64,
    snapshot_hits: AtomicU64,
    recomputes: AtomicU64,
    refused: AtomicU64,
    busy: AtomicU64,
}

struct Inner {
    /// Bumped by each reload; a snapshot is stored only under the epoch it
    /// was collected in.
    epoch: u64,
    manager: Option<Arc<Manager>>,
    /// The override fingerprint of the loaded engine.
    fingerprint: Option<String>,
    /// The fingerprint of an override set that failed to load.
    failed: Option<String>,
    snapshot: Option<Arc<Snapshot>>,
    reload_asked: Option<Instant>,
    /// The requested fingerprint that last triggered a reload. It is not
    /// asked for again until the loaded fingerprint changes, so a caller
    /// whose files never match the coordinator's does not cause a reload
    /// every `RELOAD_RETRY`.
    reload_for: Option<String>,
}

struct InFlight<'a>(&'a AtomicUsize);

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The coordinator's query service: its own Manager and detection cache,
/// separate from the screen watch's, and the snapshot.
pub(crate) struct Service {
    socket: PathBuf,
    state_dir: PathBuf,
    exe: Value,
    config_dir: Option<PathBuf>,
    log: Arc<dyn Fn(&str) + Send + Sync>,
    inner: std::sync::Mutex<Inner>,
    /// One recompute at a time; the others wait for its snapshot.
    flight: tokio::sync::Mutex<()>,
    /// Queries take their digest one at a time, so concurrent queries wait
    /// for each other and never for an action.
    digest_gate: tokio::sync::Mutex<()>,
    in_flight: AtomicUsize,
    stats: Stats,
}

impl Service {
    pub(crate) fn new(
        socket: PathBuf,
        state_dir: PathBuf,
        exe: Value,
        config_dir: Option<PathBuf>,
        log: Arc<dyn Fn(&str) + Send + Sync>,
    ) -> Self {
        let state_dir = state_dir.canonicalize().unwrap_or(state_dir);
        Self {
            socket,
            state_dir,
            exe,
            config_dir,
            log,
            inner: std::sync::Mutex::new(Inner {
                epoch: 0,
                manager: None,
                fingerprint: None,
                failed: None,
                snapshot: None,
                reload_asked: None,
                reload_for: None,
            }),
            flight: tokio::sync::Mutex::new(()),
            digest_gate: tokio::sync::Mutex::new(()),
            in_flight: AtomicUsize::new(0),
            stats: Stats::default(),
        }
    }

    /// Loads the detection engine again, as the watch does on `reload`. The
    /// snapshot and the detection cache go with the old engine. A load that
    /// fails keeps the old engine and remembers the failed fingerprint, so
    /// the same files are not loaded again for every query.
    pub(crate) async fn reload(&self) {
        let (fingerprint, loaded) = tokio::task::spawn_blocking(Engine::load_fingerprinted)
            .await
            .unwrap_or_else(|error| (String::new(), Err(error.to_string())));
        let manager = loaded
            .and_then(|engine| Manager::for_queries(self.socket.clone(), engine, SCREEN_MAX_AGE));
        self.install(fingerprint, manager);
    }

    fn install(&self, fingerprint: String, manager: Result<Manager, String>) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        inner.reload_asked = None;
        match manager {
            Ok(manager) => {
                if inner.fingerprint.as_deref() != Some(fingerprint.as_str()) {
                    inner.reload_for = None;
                }
                inner.epoch += 1;
                inner.manager = Some(Arc::new(manager));
                inner.fingerprint = Some(fingerprint);
                inner.failed = None;
                inner.snapshot = None;
            }
            Err(error) => {
                (self.log)(&format!("query engine: {error}"));
                inner.failed = Some(fingerprint);
            }
        }
    }

    pub(crate) fn status(&self) -> Value {
        let (epoch, loaded) = self
            .inner
            .lock()
            .map(|inner| (inner.epoch, inner.manager.is_some()))
            .unwrap_or((0, false));
        json!({
            "engine_epoch": epoch,
            "engine_loaded": loaded,
            "served": self.stats.served.load(Ordering::SeqCst),
            "snapshot_hits": self.stats.snapshot_hits.load(Ordering::SeqCst),
            "recomputes": self.stats.recomputes.load(Ordering::SeqCst),
            "refused": self.stats.refused.load(Ordering::SeqCst),
            "busy": self.stats.busy.load(Ordering::SeqCst),
        })
    }

    /// Answers one query on the coordinator's core connection. It never
    /// waits for an action and never reconnects.
    pub(crate) async fn serve(
        &self,
        actions: &CoordinatorActions,
        watch_running: bool,
        params: &Value,
    ) -> Result<String, Refusal> {
        self.answer(
            watch_running,
            params,
            async {
                let _turn = self.digest_gate.lock().await;
                actions
                    .query_digest(CONTROL_FORMAT, Some(VIEW_FORMAT))
                    .await
            },
            |manager: Arc<Manager>| async move { manager.collect_snapshot().await },
        )
        .await
    }

    async fn answer<D, C, F>(
        &self,
        watch_running: bool,
        params: &Value,
        digest: D,
        collect: C,
    ) -> Result<String, Refusal>
    where
        D: Future<Output = Option<FormatDigest>>,
        C: Fn(Arc<Manager>) -> F,
        F: Future<Output = Result<(Vec<Agent>, String), String>>,
    {
        let result = self
            .answer_inner(watch_running, params, digest, collect)
            .await;
        match &result {
            Ok(_) => self.stats.served.fetch_add(1, Ordering::SeqCst),
            Err(refusal) if refusal.code == "coordinator_busy" => {
                self.stats.busy.fetch_add(1, Ordering::SeqCst)
            }
            Err(_) => self.stats.refused.fetch_add(1, Ordering::SeqCst),
        };
        result
    }

    async fn answer_inner<D, C, F>(
        &self,
        watch_running: bool,
        params: &Value,
        digest: D,
        collect: C,
    ) -> Result<String, Refusal>
    where
        D: Future<Output = Option<FormatDigest>>,
        C: Fn(Arc<Manager>) -> F,
        F: Future<Output = Result<(Vec<Agent>, String), String>>,
    {
        if self.in_flight.fetch_add(1, Ordering::SeqCst) >= MAX_IN_FLIGHT {
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            return Err(Refusal {
                code: "coordinator_busy",
                message: "too many queries in flight".into(),
                reload: false,
            });
        }
        let _in_flight = InFlight(&self.in_flight);
        // The screen watch is the only writer of TRACKED, EPOCH and holds. A
        // snapshot answers what the local command would print after its own
        // writes only while that writer runs.
        if !watch_running {
            return Err(Refusal::new("the screen watch is not running"));
        }
        let request = parse(params)?;
        if request.exe.is_null() || request.exe != self.exe {
            return Err(Refusal::new("another executable asked"));
        }
        if request.state_dir != self.state_dir {
            return Err(Refusal::new("another state directory asked"));
        }
        let (manager, epoch) = self.engine_for(&request)?;
        let digest = digest
            .await
            .ok_or_else(|| Refusal::new("the core connection cannot answer a digest now"))?;
        let view_text = digest
            .global
            .as_deref()
            .unwrap_or_default()
            .trim()
            .to_owned();

        let current = self.snapshot();
        if let Some(snapshot) =
            current.filter(|s| s.valid(&digest.digest, &view_text, epoch, Instant::now()))
        {
            self.stats.snapshot_hits.fetch_add(1, Ordering::SeqCst);
            return self.reply(&snapshot, &request);
        }

        let _flight = self.flight.lock().await;
        // A reload or a recompute may have happened while this waited. The
        // recompute that finished first answers whoever its digest fits.
        let (manager, epoch) = if self.epoch() == epoch {
            (manager, epoch)
        } else {
            self.engine_for(&request)?
        };
        if let Some(snapshot) = self
            .snapshot()
            .filter(|s| s.valid(&digest.digest, &view_text, epoch, Instant::now()))
        {
            self.stats.snapshot_hits.fetch_add(1, Ordering::SeqCst);
            return self.reply(&snapshot, &request);
        }

        self.stats.recomputes.fetch_add(1, Ordering::SeqCst);
        let taken = Instant::now();
        let wall_ms = now_ms();
        let (agents, inventory) = collect(manager).await.map_err(Refusal::new)?;
        let computed = control_text(&inventory).map(|text| sha256_hex(&text));
        let list = view::view_from_text(&view_text)
            .and_then(|saved| render_list(&view::apply_view(&saved, agents.clone())))
            .map(Arc::from);
        let snapshot = Arc::new(Snapshot {
            digest: computed.clone().unwrap_or_default(),
            view_text,
            epoch,
            valid_until: valid_until(taken, wall_ms, &agents),
            agents,
            list,
        });
        // Stored only when the core's digest from before the read is the
        // one of the text the read used (nothing moved in between) and no
        // reload came meanwhile. The answer is the fresh result either way.
        if computed.as_deref() == Some(digest.digest.as_str())
            && let Ok(mut inner) = self.inner.lock()
            && inner.epoch == epoch
        {
            inner.snapshot = Some(snapshot.clone());
        }
        self.reply(&snapshot, &request)
    }

    fn snapshot(&self) -> Option<Arc<Snapshot>> {
        self.inner.lock().ok()?.snapshot.clone()
    }

    fn epoch(&self) -> u64 {
        self.inner.lock().map_or(0, |inner| inner.epoch)
    }

    /// The query Manager and its epoch when the caller's override files are
    /// the ones it loaded. Otherwise a refusal, which asks for a reload when
    /// the caller reads the same directory.
    fn engine_for(&self, request: &Request) -> Result<(Arc<Manager>, u64), Refusal> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| Refusal::new("the query state is unavailable"))?;
        if let (Some(manager), Some(fingerprint)) = (&inner.manager, &inner.fingerprint)
            && *fingerprint == request.fingerprint
        {
            return Ok((manager.clone(), inner.epoch));
        }
        if inner.failed.as_deref() == Some(request.fingerprint.as_str()) {
            return Err(Refusal::new("the detection overrides do not load"));
        }
        if request.config_dir != self.config_dir {
            return Err(Refusal::new("another detection directory asked"));
        }
        if inner.reload_for.as_deref() == Some(request.fingerprint.as_str()) {
            return Err(Refusal::new(
                "the detection overrides differ from the loaded ones",
            ));
        }
        let recently = inner
            .reload_asked
            .is_some_and(|at| at.elapsed() < RELOAD_RETRY);
        if !recently {
            inner.reload_asked = Some(Instant::now());
            inner.reload_for = Some(request.fingerprint.clone());
        }
        Err(Refusal {
            code: "query_refused",
            message: "the detection overrides changed; reloading".into(),
            reload: !recently,
        })
    }

    fn reply(&self, snapshot: &Snapshot, request: &Request) -> Result<String, Refusal> {
        let text = match request.kind {
            Kind::List => snapshot
                .list
                .as_ref()
                .map(|text| text.to_string())
                .map_err(|error| Refusal::new(error.clone()))?,
            Kind::Get => {
                let target = request.target.as_deref().unwrap_or_default();
                if target.starts_with('%') {
                    crate::pane_id(target).map_err(Refusal::new)?;
                }
                let mut matches = snapshot
                    .agents
                    .iter()
                    .filter(|agent| agent.pane_id == target || agent.name == target);
                let agent = matches
                    .next()
                    .ok_or_else(|| Refusal::new("agent target not found"))?;
                if matches.next().is_some() {
                    return Err(Refusal::new("ambiguous agent name"));
                }
                render_get(agent).map_err(Refusal::new)?
            }
        };
        if escaped_len(&text) > MAX_ANSWER {
            let message = format!(
                "the {} answer is {} bytes; over the {MAX_ANSWER} byte frame",
                match request.kind {
                    Kind::List => "list",
                    Kind::Get => "get",
                },
                text.len()
            );
            (self.log)(&format!("query: {message}"));
            return Err(Refusal::new(message));
        }
        Ok(text)
    }
}

/// The length of `text` as a JSON string body, without the quotes.
fn escaped_len(text: &str) -> usize {
    text.bytes()
        .map(|byte| match byte {
            b'"' | b'\\' | b'\n' | b'\r' | b'\t' => 2,
            0..0x20 => 6,
            _ => 1,
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::super::{BindingView, CapabilityView, Evidence, FORMAT, Metadata, Report, Tracked};
    use super::*;

    fn agent(name: &str, pane: &str, state: &str) -> Agent {
        Agent {
            id: name.into(),
            name: name.into(),
            provider: "codex".into(),
            pane_id: pane.into(),
            window_id: "@1".into(),
            workspace: "main".into(),
            cwd: "/tmp/project".into(),
            boot: "00000000-0000-0000-0000-000000000000".into(),
            generation: "1".into(),
            run: format!("run-{name}"),
            process: "running".into(),
            state: state.into(),
            session_id: None,
            evidence: Arc::new(Evidence::from_value(
                json!({"state": state, "source": "screen", "explanations": [{"rule": "a"}]}),
            )),
            seen: false,
            revision: "1".into(),
            returned_idle: false,
            endpoint_id: String::new(),
            endpoint_label: String::new(),
            endpoint_key: String::new(),
            stale: false,
            binding: BindingView::default(),
            capabilities: CapabilityView::default(),
            metadata: None,
            run_evidence: None,
            encoded: String::new(),
            foreground_command: String::new(),
            tracked: Tracked::default(),
            tracked_encoded: String::new(),
            foreground_group: 0,
            process_epoch: None,
            output_generation: 0,
            permission_count: 0,
            question_count: 0,
            title: String::new(),
            progress: String::new(),
        }
    }

    fn with_report(mut agent: Agent, at: u64) -> Agent {
        agent.metadata = Some(Metadata {
            report: Some(Report {
                sequence: 3,
                state: "working".into(),
                at,
            }),
            ..Metadata::default()
        });
        agent
    }

    fn snapshot(valid_for: Duration) -> Snapshot {
        Snapshot {
            digest: "d".into(),
            view_text: "v".into(),
            epoch: 4,
            valid_until: Instant::now() + valid_for,
            agents: Vec::new(),
            list: Ok(Arc::from("{}\n")),
        }
    }

    fn digest(text: &str, view: &str) -> FormatDigest {
        FormatDigest {
            digest: text.into(),
            lines: 1,
            global: Some(view.into()),
        }
    }

    /// A service whose Manager never reaches a server: the tests inject the
    /// digest and the collection.
    fn service() -> (Service, tempfile_guard::Guard) {
        let guard = tempfile_guard::Guard::new();
        let exe = json!({"path": "/bin/masil-agent", "size": 1});
        let service = Service::new(
            guard.path.clone(),
            guard.path.clone(),
            exe,
            Some(PathBuf::from("/config/masil/agent-detection")),
            Arc::new(|_: &str| {}),
        );
        let manager = Manager::for_queries(
            guard.path.clone(),
            Engine::load_fingerprinted().1.unwrap(),
            SCREEN_MAX_AGE,
        )
        .unwrap();
        service.install("fp".into(), Ok(manager));
        (service, guard)
    }

    mod tempfile_guard {
        use std::path::PathBuf;
        use std::sync::atomic::{AtomicUsize, Ordering};

        static NEXT: AtomicUsize = AtomicUsize::new(0);

        pub(super) struct Guard {
            pub(super) path: PathBuf,
        }

        impl Guard {
            pub(super) fn new() -> Self {
                let path = std::env::temp_dir().join(format!(
                    "masil-query-test-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                std::fs::create_dir(&path).unwrap();
                // As the service sees it: /var is a link on macOS.
                Self {
                    path: path.canonicalize().unwrap(),
                }
            }
        }

        impl Drop for Guard {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.path);
            }
        }
    }

    fn params(kind: &str, target: Option<&str>, guard: &tempfile_guard::Guard) -> Value {
        let mut value = json!({
            "kind": kind,
            "state_dir": guard.path,
            "config_dir": "/config/masil/agent-detection",
            "config_fingerprint": "fp",
            "exe_identity": {"path": "/bin/masil-agent", "size": 1},
        });
        if let Some(target) = target {
            value["target"] = json!(target);
        }
        value
    }

    /// An inventory text of `panes` rows, with these output generations.
    fn inventory(rows: &[(&str, &str)]) -> String {
        rows.iter()
            .map(|(pane, output)| {
                let mut fields = vec![String::new(); FIELDS];
                fields[0] = (*pane).into();
                fields[7] = "codex".into();
                fields[14] = (*output).into();
                fields[18] = "1700000000".into();
                fields.join("\t") + "\n"
            })
            .collect()
    }

    fn digest_of(rows: &[(&str, &str)]) -> String {
        sha256_hex(&control_text(&inventory(rows)).unwrap())
    }

    #[test]
    fn the_control_format_is_the_inventory_format_without_the_activity_fields() {
        let kept: Vec<&str> = FORMAT
            .split('\t')
            .enumerate()
            .filter(|(index, _)| !ACTIVITY_FIELDS.contains(index))
            .map(|(_, field)| field)
            .collect();
        assert_eq!(CONTROL_FORMAT, kept.join("\t"));
        assert_eq!(FORMAT.split('\t').count(), FIELDS);
        assert_eq!(VIEW_FORMAT, format!("#{{{}}}", view::OPTION));
        assert!(
            FORMAT
                .split('\t')
                .nth(14)
                .unwrap()
                .contains("output_generation")
        );
        assert!(
            FORMAT
                .split('\t')
                .nth(18)
                .unwrap()
                .contains("last_output_time")
        );
    }

    #[test]
    fn the_control_text_drops_only_activity_and_keeps_escapes_whole() {
        let one = inventory(&[("%1", "5"), ("%2", "9")]);
        let text = control_text(&one).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(
            text.lines()
                .all(|line| line.split('\t').count() == FIELDS - 2)
        );
        // Only the output generation moved: the control text is the same.
        assert_eq!(
            control_text(&inventory(&[("%1", "6"), ("%2", "9")])).unwrap(),
            text
        );
        // A pane field moved: it is not.
        assert_ne!(
            control_text(&inventory(&[("%1", "5"), ("%3", "9")])).unwrap(),
            text
        );
        // An escaped tab belongs to its field instead of ending it.
        let mut fields = vec![String::new(); FIELDS];
        fields[0] = "%1".into();
        fields[9] = "a\\\tb".into();
        let row = fields.join("\t") + "\n";
        assert!(control_text(&row).unwrap().contains("a\\\tb"));
        // Wrong width, a cut line and a dangling escape are no inventory.
        assert_eq!(control_text("%1\t%2\n"), None);
        assert_eq!(control_text(&one[..one.len() - 1]), None);
        assert_eq!(control_text("%1\\"), None);
        assert_eq!(control_text("").as_deref(), Some(""));
    }

    #[test]
    fn a_snapshot_answers_only_for_its_digest_view_epoch_and_age() {
        let snapshot = snapshot(Duration::from_millis(500));
        let now = Instant::now();
        assert!(snapshot.valid("d", "v", 4, now));
        assert!(!snapshot.valid("e", "v", 4, now), "a control field moved");
        assert!(!snapshot.valid("d", "w", 4, now), "the saved view moved");
        assert!(!snapshot.valid("d", "v", 5, now), "the engine was reloaded");
        assert!(
            !snapshot.valid("d", "v", 4, now + Duration::from_millis(500)),
            "the age bound is reached"
        );
    }

    #[test]
    fn the_age_bound_is_cut_short_by_the_first_time_driven_change() {
        let taken = Instant::now();
        let wall = 1_000_000;
        let none: Vec<Agent> = vec![agent("a", "%1", "idle")];
        assert_eq!(valid_until(taken, wall, &none), taken + AGE_BOUND);
        // A report that goes stale in 300 ms.
        let stale_at = wall + 300;
        let reported = vec![with_report(
            agent("a", "%1", "working"),
            stale_at - (super::super::REPORT_FRESH_MS + 1),
        )];
        assert_eq!(
            valid_until(taken, wall, &reported),
            taken + Duration::from_millis(300)
        );
        // One that goes stale later than the bound keeps the bound.
        let later = vec![with_report(agent("a", "%1", "working"), wall)];
        assert_eq!(valid_until(taken, wall, &later), taken + AGE_BOUND);
        // A deadline already behind is in the judgement taken, not ahead.
        let mut late = agent("b", "%2", "idle");
        late.binding.resume_deadline_ms = Some(wall - 5);
        assert_eq!(earliest_transition_ms(&[late.clone()], wall), None);
        late.binding.resume_deadline_ms = Some(wall + 40);
        assert_eq!(earliest_transition_ms(&[late], wall), Some(wall + 40));
    }

    #[test]
    fn a_held_turn_changes_at_its_quiet_check_before_its_cap() {
        let wall = 5_000_000;
        let mut held = with_report(agent("a", "%1", "working"), wall - 19_800);
        held.run_evidence = Some(super::super::RunEvidence {
            report: Some(super::super::ReportSource {
                sequence: 3,
                source: "native_callback".into(),
                observer: None,
                generation: None,
                live: false,
                permissions: None,
                questions: None,
                event: None,
                hold: Some(super::super::TurnHold::default()),
            }),
            ..Default::default()
        });
        assert_eq!(earliest_transition_ms(&[held], wall), Some(wall + 200));
    }

    #[test]
    fn rendered_text_is_what_the_local_formatter_prints() {
        let agents = vec![agent("one", "%1", "idle"), agent("two", "%2", "working")];
        #[derive(Serialize)]
        struct AgentList<'a> {
            agents: &'a [Agent],
        }
        let local = format!(
            "{}\n",
            serde_json::to_string_pretty(&AgentList { agents: &agents }).unwrap()
        );
        assert_eq!(render_list(&agents).unwrap(), local);
        assert!(local.contains("\"agents\": ["));
        let local = format!(
            "{}\n",
            serde_json::to_string_pretty(&json!(agents[0])).unwrap()
        );
        assert_eq!(render_get(&agents[0]).unwrap(), local);
        // `get` shows the evidence reparsed, `list` keeps its raw text.
        assert_ne!(
            render_get(&agents[0]).unwrap(),
            render_list(&agents[..1]).unwrap()
        );
    }

    #[test]
    fn requests_that_cannot_be_checked_are_refused() {
        let guard = tempfile_guard::Guard::new();
        for params in [
            json!({}),
            json!({"kind": "explain", "state_dir": "/s", "config_fingerprint": "f"}),
            json!({"kind": "get", "state_dir": "/s", "config_fingerprint": "f"}),
            json!({"kind": "list", "target": "x", "state_dir": "/s", "config_fingerprint": "f"}),
            params("list", None, &guard)
                .as_object()
                .map(|o| {
                    let mut o = o.clone();
                    o.remove("config_fingerprint");
                    Value::Object(o)
                })
                .unwrap(),
        ] {
            assert!(parse(&params).is_err(), "{params}");
        }
        assert!(parse(&params("get", Some("%1"), &guard)).is_ok());
    }

    type Collected = std::future::Ready<Result<(Vec<Agent>, String), String>>;

    fn collected(
        calls: &Arc<AtomicUsize>,
        rows: &'static [(&'static str, &'static str)],
        agents: Vec<Agent>,
    ) -> impl Fn(Arc<Manager>) -> Collected {
        let calls = calls.clone();
        move |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            std::future::ready(Ok((agents.clone(), inventory(rows))))
        }
    }

    const ROWS: &[(&str, &str)] = &[("%1", "5"), ("%2", "9")];

    #[tokio::test]
    async fn a_valid_snapshot_answers_without_collecting_and_a_moved_pane_does_not() {
        let (service, guard) = service();
        let calls = Arc::new(AtomicUsize::new(0));
        let agents = vec![agent("one", "%1", "idle"), agent("two", "%2", "working")];
        let collect = collected(&calls, ROWS, agents.clone());
        let ask = |kind: &'static str, target: Option<&'static str>, d: FormatDigest| {
            let params = params(kind, target, &guard);
            let collect = &collect;
            let service = &service;
            async move {
                service
                    .answer(true, &params, async { Some(d) }, collect)
                    .await
            }
        };
        let now = digest(&digest_of(ROWS), "");
        let first = ask("list", None, now).await.unwrap();
        let shown = view::apply_view(&view::view_from_text("").unwrap(), agents.clone());
        assert_eq!(first, render_list(&shown).unwrap());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // Same digest and view: the snapshot, byte for byte, for list and get.
        let again = ask("list", None, digest(&digest_of(ROWS), ""))
            .await
            .unwrap();
        assert_eq!(again, first);
        let one = ask("get", Some("one"), digest(&digest_of(ROWS), ""))
            .await
            .unwrap();
        assert_eq!(one, render_get(&agents[0]).unwrap());
        let by_id = ask("get", Some("%2"), digest(&digest_of(ROWS), ""))
            .await
            .unwrap();
        assert_eq!(by_id, render_get(&agents[1]).unwrap());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // A control field moved: collected again (the digest is not the
        // snapshot's), and the new snapshot is not stored because the text
        // the collection used is not the one the digest was taken over.
        let moved = digest(&digest_of(&[("%1", "5"), ("%7", "9")]), "");
        assert!(ask("list", None, moved).await.is_ok());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        // The saved view is part of the check.
        let viewed = ask("list", None, digest(&digest_of(ROWS), "other")).await;
        assert!(viewed.is_err(), "a view that does not decode is refused");
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        // Missing and ambiguous names are refused so the local path explains.
        assert!(
            ask("get", Some("nobody"), digest(&digest_of(ROWS), "other"))
                .await
                .is_err()
        );
        let status = service.status();
        assert_eq!(status["recomputes"], 3);
        assert!(status["snapshot_hits"].as_u64().unwrap() >= 4);
    }

    #[tokio::test]
    async fn an_activity_only_change_waits_for_the_age_bound() {
        let (service, guard) = service();
        let calls = Arc::new(AtomicUsize::new(0));
        let agents = vec![agent("one", "%1", "idle")];
        let rows: &'static [(&str, &str)] = &[("%1", "5")];
        let collect = collected(&calls, rows, agents);
        let params = params("list", None, &guard);
        let ask = || async {
            service
                .answer(
                    true,
                    &params,
                    async { Some(digest(&digest_of(rows), "")) },
                    &collect,
                )
                .await
        };
        ask().await.unwrap();
        // Output generation 5 became 6: the control digest is unchanged.
        assert_eq!(digest_of(rows), digest_of(&[("%1", "6")]));
        ask().await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // Once the snapshot is past its bound it is collected again.
        if let Ok(mut inner) = service.inner.lock()
            && let Some(old) = inner.snapshot.take()
        {
            let mut old = Arc::try_unwrap(old).ok().unwrap();
            old.valid_until = Instant::now() - Duration::from_millis(1);
            inner.snapshot = Some(Arc::new(old));
        }
        ask().await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_snapshot_collected_before_a_reload_is_not_stored_after_it() {
        let (service, guard) = service();
        let params = params("list", None, &guard);
        let calls = Arc::new(AtomicUsize::new(0));
        let rows: &'static [(&str, &str)] = &[("%1", "5")];
        let agents = vec![agent("one", "%1", "idle")];
        // The reload lands while the collection runs.
        let slow = {
            let (calls, service) = (calls.clone(), &service);
            move |_manager: Arc<Manager>| {
                calls.fetch_add(1, Ordering::SeqCst);
                let agents = agents.clone();
                async move {
                    let manager = Manager::for_queries(
                        service.socket.clone(),
                        Engine::load_fingerprinted().1.unwrap(),
                        SCREEN_MAX_AGE,
                    );
                    service.install("fp".into(), manager);
                    Ok((agents, inventory(rows)))
                }
            }
        };
        let epoch = service.epoch();
        let answered = service
            .answer(
                true,
                &params,
                async { Some(digest(&digest_of(rows), "")) },
                slow,
            )
            .await;
        assert!(answered.is_ok(), "the fresh result is still the answer");
        assert_eq!(service.epoch(), epoch + 1);
        assert!(
            service.snapshot().is_none(),
            "the old epoch's snapshot is dropped"
        );
    }

    #[tokio::test]
    async fn concurrent_queries_share_one_recompute() {
        let (service, guard) = service();
        let service = Arc::new(service);
        let params = params("list", None, &guard);
        let calls = Arc::new(AtomicUsize::new(0));
        let rows: &'static [(&str, &str)] = &[("%1", "5")];
        let agents = vec![agent("one", "%1", "idle")];
        let mut tasks = Vec::new();
        for _ in 0..MAX_IN_FLIGHT {
            let (service, params, calls, agents) = (
                service.clone(),
                params.clone(),
                calls.clone(),
                agents.clone(),
            );
            tasks.push(tokio::spawn(async move {
                let collect = move |_manager: Arc<Manager>| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    let agents = agents.clone();
                    async move {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        Ok((agents, inventory(rows)))
                    }
                };
                service
                    .answer(
                        true,
                        &params,
                        async { Some(digest(&digest_of(rows), "")) },
                        collect,
                    )
                    .await
            }));
        }
        let mut texts = Vec::new();
        for task in tasks {
            texts.push(task.await.unwrap().unwrap());
        }
        assert!(texts.windows(2).all(|pair| pair[0] == pair[1]));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "followers await the one in flight"
        );
    }

    #[tokio::test]
    async fn at_most_four_queries_run_at_once() {
        let (service, guard) = service();
        let service = Arc::new(service);
        let params = params("list", None, &guard);
        let rows: &'static [(&str, &str)] = &[("%1", "5")];
        let release = Arc::new(tokio::sync::Notify::new());
        let mut tasks = Vec::new();
        for _ in 0..MAX_IN_FLIGHT {
            let (service, params, release) = (service.clone(), params.clone(), release.clone());
            tasks.push(tokio::spawn(async move {
                service
                    .answer(
                        true,
                        &params,
                        async move {
                            release.notified().await;
                            Some(digest(&digest_of(rows), ""))
                        },
                        |_manager: Arc<Manager>| async move {
                            Ok((vec![agent("one", "%1", "idle")], inventory(rows)))
                        },
                    )
                    .await
            }));
        }
        while service.in_flight.load(Ordering::SeqCst) < MAX_IN_FLIGHT {
            tokio::task::yield_now().await;
        }
        let busy = service
            .answer(
                true,
                &params,
                async { None },
                |_manager: Arc<Manager>| async { Err("unused".to_owned()) },
            )
            .await
            .unwrap_err();
        assert_eq!(busy.code, "coordinator_busy");
        release.notify_waiters();
        for task in tasks {
            task.await.unwrap().unwrap();
        }
        assert_eq!(service.in_flight.load(Ordering::SeqCst), 0);
        assert_eq!(service.status()["busy"], 1);
    }

    #[tokio::test]
    async fn every_unsafe_case_is_refused_for_the_local_path() {
        let (service, guard) = service();
        let rows: &'static [(&str, &str)] = &[("%1", "5")];
        let unused =
            |_manager: Arc<Manager>| async { Err::<(Vec<Agent>, String), _>("x".to_owned()) };
        let good = || params("list", None, &guard);
        let ask = |watch: bool, params: Value, d: Option<FormatDigest>| {
            let service = &service;
            async move {
                service
                    .answer(watch, &params, async { d }, unused)
                    .await
                    .unwrap_err()
            }
        };
        let ok_digest = || Some(digest(&digest_of(rows), ""));
        // The watch is not running.
        assert!(!ask(false, good(), ok_digest()).await.reload);
        // Another state directory, another executable.
        let mut other = good();
        other["state_dir"] = json!("/elsewhere");
        ask(true, other, ok_digest()).await;
        let mut other = good();
        other["exe_identity"] = json!({"path": "/bin/other"});
        ask(true, other, ok_digest()).await;
        let mut other = good();
        other["exe_identity"] = Value::Null;
        ask(true, other, ok_digest()).await;
        // The core connection is absent, busy or broken.
        let refusal = ask(true, good(), None).await;
        assert_eq!(refusal.code, "query_refused");
        // Another detection directory is refused without a reload.
        let mut other = good();
        other["config_fingerprint"] = json!("changed");
        other["config_dir"] = json!("/other/masil/agent-detection");
        assert!(!ask(true, other, ok_digest()).await.reload);
        // The same directory with other bytes asks for a reload, once.
        let mut same = good();
        same["config_fingerprint"] = json!("changed");
        assert!(ask(true, same.clone(), ok_digest()).await.reload);
        assert!(!ask(true, same.clone(), ok_digest()).await.reload);
        // A fingerprint that failed to load is not asked for again.
        service.install("broken".into(), Err("invalid override".into()));
        let mut broken = good();
        broken["config_fingerprint"] = json!("broken");
        let refusal = ask(true, broken, ok_digest()).await;
        assert!(!refusal.reload);
        assert!(refusal.message.contains("do not load"));
        // The earlier engine keeps answering for its own fingerprint.
        assert_eq!(service.status()["engine_loaded"], true);
    }

    #[tokio::test]
    async fn a_reload_replaces_the_engine_and_clears_the_snapshot() {
        let (service, guard) = service();
        let rows: &'static [(&str, &str)] = &[("%1", "5")];
        let params = params("list", None, &guard);
        let agents = vec![agent("one", "%1", "idle")];
        let calls = Arc::new(AtomicUsize::new(0));
        let collect = collected(&calls, rows, agents);
        let digest_now = || async { Some(digest(&digest_of(rows), "")) };
        service
            .answer(true, &params, digest_now(), &collect)
            .await
            .unwrap();
        assert!(service.snapshot().is_some());
        let epoch = service.epoch();
        let manager = Manager::for_queries(
            service.socket.clone(),
            Engine::load_fingerprinted().1.unwrap(),
            SCREEN_MAX_AGE,
        );
        service.install("fp2".into(), manager);
        assert_eq!(service.epoch(), epoch + 1);
        assert!(service.snapshot().is_none());
        // Callers that read the old files are now the mismatch.
        assert!(
            service
                .answer(true, &params, digest_now(), &collect)
                .await
                .is_err()
        );
        let mut fresh = params.clone();
        fresh["config_fingerprint"] = json!("fp2");
        service
            .answer(true, &fresh, digest_now(), &collect)
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_fingerprint_that_stays_different_asks_for_one_reload() {
        let (service, guard) = service();
        let rows: &'static [(&str, &str)] = &[("%1", "5")];
        let params = params("list", None, &guard);
        let calls = Arc::new(AtomicUsize::new(0));
        let collect = collected(&calls, rows, vec![agent("one", "%1", "idle")]);
        let digest_now = || async { Some(digest(&digest_of(rows), "")) };
        let load = |service: &Service, fingerprint: &str| {
            let manager = Manager::for_queries(
                service.socket.clone(),
                Engine::load_fingerprinted().1.unwrap(),
                SCREEN_MAX_AGE,
            );
            service.install(fingerprint.into(), manager);
        };
        load(&service, "loaded");
        let first = service
            .answer(true, &params, digest_now(), &collect)
            .await
            .unwrap_err();
        assert!(first.reload);
        // The reload installs the same files again; the caller still differs.
        load(&service, "loaded");
        for _ in 0..3 {
            let again = service
                .answer(true, &params, digest_now(), &collect)
                .await
                .unwrap_err();
            assert!(
                !again.reload,
                "asked again for the same requested fingerprint"
            );
        }
        // A change of the loaded files lets the next mismatch ask again.
        load(&service, "edited");
        let after = service
            .answer(true, &params, digest_now(), &collect)
            .await
            .unwrap_err();
        assert!(after.reload);
    }

    #[test]
    fn an_answer_that_would_not_fit_its_frame_is_refused_and_logged() {
        let logged = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = logged.clone();
        let guard = tempfile_guard::Guard::new();
        let service = Service::new(
            guard.path.clone(),
            guard.path.clone(),
            json!({}),
            None,
            Arc::new(move |line: &str| sink.lock().unwrap().push(line.to_owned())),
        );
        let mut huge = agent("one", "%1", "idle");
        huge.cwd = "\"".repeat(MAX_ANSWER / 2 + 1);
        let mut snap = snapshot(AGE_BOUND);
        snap.agents = vec![huge];
        let request = parse(&params("get", Some("one"), &guard)).unwrap();
        let refusal = service.reply(&snap, &request).unwrap_err();
        assert!(refusal.message.contains("frame"), "{}", refusal.message);
        assert_eq!(logged.lock().unwrap().len(), 1);
        assert_eq!(escaped_len("a\"b\n\u{1}"), 1 + 2 + 1 + 2 + 6);
    }
}
