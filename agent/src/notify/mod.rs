//! Notifications (P7a, docs/notifications.md): the coordinator claims new
//! inbox events and tells the user through tmux, the OS, or a command of
//! theirs. Each (event, route) is recorded before it is sent and never
//! sent twice. Off unless `agent notify enable`.

pub(crate) mod routes;

use crate::managed::NotifyEvent;
use crate::native_ui::Context;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

/// Events older than this when claimed were recorded while nothing ran to
/// notify; they are summed up, not sent one by one.
pub(crate) const STALE_MS: u64 = 10 * 60 * 1000;
/// Above this many events in one pass, a route gets one summary.
pub(crate) const COALESCE: usize = 3;
const SETTINGS_BYTES: u64 = 64 * 1024;
/// Batches a route may have waiting; more are recorded `skipped_backlog`.
const BACKLOG: usize = 100;
/// At most one tmux message a second; what arrives meanwhile is merged.
const TMUX_EVERY: Duration = Duration::from_secs(1);
/// The session variables OS notifications need, read again from the
/// server's global environment when the coordinator starts.
const SESSION_VARIABLES: [&str; 4] = [
    "DBUS_SESSION_BUS_ADDRESS",
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "XDG_RUNTIME_DIR",
];

/// `~/.config/masil/notify.toml`.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct Settings {
    pub(crate) events: Vec<String>,
    pub(crate) tmux: bool,
    pub(crate) bell: bool,
    pub(crate) os: bool,
    /// Add the event's own text (a tool name, a question) to messages.
    pub(crate) detail: bool,
    /// A command (argv) given each batch as JSON on standard input.
    pub(crate) hook: Vec<String>,
    /// Worktree jobs ending, through the OS and the hook only.
    pub(crate) jobs: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            events: [
                "approval_requested",
                "question_asked",
                "blocked",
                "turn_completed",
                "operation_unknown",
                "missed_schedule",
            ]
            .map(String::from)
            .to_vec(),
            tmux: true,
            bell: false,
            os: false,
            detail: false,
            hook: Vec::new(),
            jobs: false,
        }
    }
}

const KINDS: [&str; 10] = [
    "blocked",
    "approval_requested",
    "question_asked",
    "turn_completed",
    "returned_idle",
    "error",
    "observation_lost",
    "operation_unknown",
    "missed_schedule",
    "answered",
];

impl Settings {
    fn path() -> Option<PathBuf> {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .filter(|path| path.is_absolute())
                    .map(|home| home.join(".config"))
            })
            .map(|base| base.join("masil/notify.toml"))
    }

    /// The user's settings; defaults without a file. The file must be the
    /// user's and not writable by others.
    pub(crate) fn load() -> Result<Self, String> {
        let Some(path) = Self::path() else {
            return Ok(Self::default());
        };
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(format!("notify_invalid: {}: {error}", path.display())),
        };
        let metadata = file
            .metadata()
            .map_err(|error| format!("notify_invalid: {}: {error}", path.display()))?;
        if metadata.len() > SETTINGS_BYTES {
            return Err(format!(
                "notify_invalid: {} is larger than 64 KiB",
                path.display()
            ));
        }
        // SAFETY: geteuid has no preconditions.
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o022 != 0
        {
            return Err(format!(
                "notify_invalid: {} must be a regular file owned by you and not writable by group or others",
                path.display()
            ));
        }
        let mut text = String::new();
        file.take(SETTINGS_BYTES)
            .read_to_string(&mut text)
            .map_err(|error| format!("notify_invalid: {}: {error}", path.display()))?;
        let settings: Self = toml::from_str(&text)
            .map_err(|error| format!("notify_invalid: {}: {error}", path.display()))?;
        if let Some(kind) = settings
            .events
            .iter()
            .find(|kind| !KINDS.contains(&kind.as_str()))
        {
            return Err(format!(
                "notify_invalid: {}: unknown event {kind}; known: {}",
                path.display(),
                KINDS.join(", ")
            ));
        }
        if settings.hook.first().is_some_and(String::is_empty) {
            return Err(format!(
                "notify_invalid: {}: hook is a list of arguments, the program first",
                path.display()
            ));
        }
        Ok(settings)
    }

    /// The routes notifications take, by name.
    pub(crate) fn routes(&self) -> Vec<String> {
        let mut routes = Vec::new();
        if self.tmux {
            routes.push("tmux".to_owned());
        }
        if self.os {
            routes.push("os".to_owned());
        }
        if !self.hook.is_empty() {
            routes.push("hook".to_owned());
        }
        routes
    }
}

/// An event's kind in words.
pub(crate) fn kind_words(kind: &str) -> &str {
    match kind {
        "approval_requested" => "approval requested",
        "question_asked" => "question asked",
        "blocked" => "waiting for you",
        "turn_completed" => "turn completed",
        "returned_idle" => "idle again",
        "error" => "error",
        "observation_lost" => "observation lost",
        "operation_unknown" => "outcome unknown",
        "missed_schedule" => "schedule missed",
        "answered" => "answered",
        "test" => "test notification",
        "job_failed" => "failed",
        "job_succeeded" => "done",
        "job_cancelled" => "cancelled",
        "job_outcome_unknown" => "outcome unknown",
        other => other,
    }
}

/// The part of an event's summary a message may show with `detail`: one
/// field per kind, its first line, 80 characters.
pub(crate) fn detail_of(kind: &str, summary: &Value) -> Option<String> {
    let field = match kind {
        "approval_requested" => ["tool", "title", "permission"]
            .iter()
            .find_map(|key| summary[key].as_str()),
        "question_asked" => ["question", "title"]
            .iter()
            .find_map(|key| summary[key].as_str()),
        "error" | "observation_lost" | "job_failed" | "job_cancelled" | "job_outcome_unknown" => {
            ["message", "error"]
                .iter()
                .find_map(|key| summary[key].as_str())
        }
        "missed_schedule" => summary["schedule"].as_str(),
        _ => None,
    }?;
    let line: String = field
        .lines()
        .next()
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_control())
        .take(80)
        .collect();
    (!line.is_empty()).then_some(line)
}

/// One event, ready for any route.
#[derive(Clone, Debug)]
pub(crate) struct Notice {
    /// The inbox event; 0 for a test or a job.
    pub(crate) id: i64,
    pub(crate) kind: String,
    pub(crate) agent: String,
    pub(crate) pane: String,
    pub(crate) run: String,
    pub(crate) provider: String,
    pub(crate) observed_ms: u64,
    pub(crate) summary: Option<Value>,
    /// A worktree job's ID, kind and state.
    pub(crate) job: Option<Value>,
}

impl Notice {
    pub(crate) fn from_event(event: &NotifyEvent, agent: Option<String>) -> Self {
        // A schedule's event has no pane; its schedule names it. An
        // endpoint's event carries `endpoint::agent`.
        let schedule = event
            .summary
            .as_ref()
            .and_then(|summary| summary["schedule"].as_str())
            .filter(|_| event.pane.is_empty())
            .map(|name| format!("schedule {name}"));
        let remote = event
            .summary
            .as_ref()
            .and_then(|summary| summary["endpoint_agent"].as_str())
            .map(str::to_owned);
        let schedule = remote.or(schedule);
        Self {
            id: event.id,
            kind: event.kind.clone(),
            agent: agent.or(schedule).unwrap_or_else(|| event.pane.clone()),
            pane: event.pane.clone(),
            run: event.run.clone(),
            provider: event.provider.clone(),
            observed_ms: event.observed_ms,
            summary: event.summary.clone(),
            job: None,
        }
    }

    /// "builder: approval requested", with the detail if asked for.
    pub(crate) fn line(&self, detail: bool) -> String {
        let mut line = format!("{}: {}", self.agent, kind_words(&self.kind));
        if detail
            && let Some(more) = self
                .summary
                .as_ref()
                .and_then(|summary| detail_of(&self.kind, summary))
        {
            line.push_str(" — ");
            line.push_str(&more);
        }
        line
    }

    fn to_json(&self, detail: bool) -> Value {
        json!({
            "id": self.id,
            "kind": self.kind,
            "agent": self.agent,
            "pane": self.pane,
            "run": self.run,
            "provider": self.provider,
            "observed_ms": self.observed_ms,
            "summary": if detail { self.summary.clone().unwrap_or(Value::Null) } else { Value::Null },
            "job": self.job.clone().unwrap_or(Value::Null),
        })
    }
}

/// What a route sends for one batch: one message per notice, or one
/// summary when there are more than [`COALESCE`].
#[derive(Clone, Debug)]
pub(crate) struct Batch {
    pub(crate) server: String,
    pub(crate) notices: Vec<Notice>,
    /// Events recorded while nothing was running to notify.
    pub(crate) stale: usize,
}

impl Batch {
    pub(crate) fn summarized(&self) -> bool {
        self.notices.len() > COALESCE || self.stale > 0
    }

    /// The messages for tmux and the OS.
    pub(crate) fn lines(&self, detail: bool) -> Vec<String> {
        if !self.summarized() {
            return self
                .notices
                .iter()
                .map(|notice| notice.line(detail))
                .collect();
        }
        let mut counts: Vec<(String, usize)> = Vec::new();
        for notice in &self.notices {
            match counts.iter_mut().find(|(kind, _)| *kind == notice.kind) {
                Some((_, count)) => *count += 1,
                None => counts.push((notice.kind.clone(), 1)),
            }
        }
        let mut parts: Vec<String> = counts
            .iter()
            .map(|(kind, count)| format!("{count} {}", kind_words(kind)))
            .collect();
        if self.stale > 0 {
            parts.push(format!(
                "{} while masil was not running; see the inbox",
                self.stale
            ));
        }
        vec![format!("masil: {}", parts.join(", "))]
    }

    /// The hook's input: always one shape.
    pub(crate) fn hook_json(&self, detail: bool) -> Value {
        json!({
            "version": 1,
            "server": self.server,
            "summarized": self.summarized(),
            "stale": self.stale,
            "events": self.notices.iter().map(|notice| notice.to_json(detail)).collect::<Vec<_>>(),
        })
    }
}

/// What `notify enable` keeps from its own environment for OS notifications
/// and the hook; job notifications take the same from theirs.
const KEPT: [&str; 9] = [
    "PATH",
    "DBUS_SESSION_BUS_ADDRESS",
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "XDG_RUNTIME_DIR",
    "LANG",
    "LC_ALL",
    "HOME",
    "USER",
];

pub(crate) fn kept_environment() -> Vec<(String, String)> {
    KEPT.iter()
        .filter_map(|name| {
            std::env::var(name)
                .ok()
                .map(|value| ((*name).to_owned(), value))
        })
        .collect()
}

/// The settings when worktree job notifications are wanted and have a
/// route: the OS or the hook, since a job may end with no server.
pub(crate) fn job_settings() -> Option<Settings> {
    Settings::load()
        .ok()
        .filter(|settings| settings.jobs && (settings.os || !settings.hook.is_empty()))
}

/// Sends one job notice through the OS and the hook, and waits for both.
pub(crate) fn send_job(settings: &Settings, notice: Notice) -> Result<(), String> {
    let batch = Batch {
        server: String::new(),
        notices: vec![notice],
        stale: 0,
    };
    let env = kept_environment();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    let (os, hook) = runtime.block_on(async {
        let os = async {
            if settings.os {
                routes::os(&batch, settings.detail, &env).await
            } else {
                Ok(())
            }
        };
        let hook = async {
            if settings.hook.is_empty() {
                Ok(())
            } else {
                routes::hook(&settings.hook, &batch, settings.detail, &env).await
            }
        };
        tokio::join!(os, hook)
    });
    os.and(hook)
}

/// A batch on its way through one route, with the log rows it settles.
struct Job {
    batch: Batch,
    rows: Vec<i64>,
    settings: Arc<Settings>,
    env: Arc<Vec<(String, String)>>,
}

impl Job {
    fn merge(&mut self, other: Job) {
        self.batch.notices.extend(other.batch.notices);
        self.batch.stale += other.batch.stale;
        self.rows.extend(other.rows);
        self.settings = other.settings;
        self.env = other.env;
    }
}

/// What a route reports: the log rows and how sending went.
pub(crate) type Outcome = (Vec<i64>, String);

/// The coordinator's notifications: turns claimed events into batches and
/// hands them to one task per route, which sends them in order. Sending
/// never holds up the watch. Dropping it ends the route tasks, and with
/// them any hook still running.
pub(crate) struct Notifier {
    server: String,
    native: Context,
    settings: Option<Arc<Settings>>,
    /// The last settings error, logged once.
    error: Option<String>,
    session: Option<Vec<(String, String)>>,
    /// Agent names by run, from the watch's latest listing.
    pub(crate) names: HashMap<String, String>,
    routes: HashMap<String, (mpsc::Sender<Job>, tokio::task::JoinHandle<()>)>,
    /// One send at a time per route, tests included.
    gates: HashMap<&'static str, Arc<tokio::sync::Mutex<()>>>,
    done: mpsc::UnboundedSender<Outcome>,
    pub(crate) outcomes: mpsc::UnboundedReceiver<Outcome>,
}

impl Drop for Notifier {
    fn drop(&mut self) {
        for (_, task) in self.routes.values() {
            task.abort();
        }
    }
}

impl Notifier {
    pub(crate) fn new(native: Context) -> Self {
        let (done, outcomes) = mpsc::unbounded_channel();
        Self {
            server: native
                .socket
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            native,
            settings: None,
            error: None,
            session: None,
            names: HashMap::new(),
            routes: HashMap::new(),
            gates: ["tmux", "os", "hook"]
                .into_iter()
                .map(|route| (route, Arc::default()))
                .collect(),
            done,
            outcomes,
        }
    }

    /// Settings and session variables are read again before the next pass.
    pub(crate) fn reload(&mut self) {
        self.settings = None;
        self.session = None;
    }

    /// The settings file is read again before the next pass; the session
    /// variables are kept.
    pub(crate) fn recheck(&mut self) {
        self.settings = None;
    }

    /// The settings, read when needed. A broken file keeps nothing sent
    /// (and nothing claimed) until it is fixed; the error is returned once.
    pub(crate) fn settings(&mut self) -> Result<Option<Arc<Settings>>, String> {
        if self.settings.is_none() {
            match Settings::load() {
                Ok(settings) => {
                    self.settings = Some(Arc::new(settings));
                    self.error = None;
                }
                Err(error) => {
                    let first = self.error.as_ref() != Some(&error);
                    self.error = Some(error.clone());
                    return if first { Err(error) } else { Ok(None) };
                }
            }
        }
        Ok(self.settings.clone())
    }

    /// The environment for OS notifications and the hook: what `notify
    /// enable` kept, with the session variables the server has now.
    pub(crate) async fn environment(
        &mut self,
        kept: Vec<(String, String)>,
    ) -> Vec<(String, String)> {
        if self.session.is_none() {
            let mut session = Vec::new();
            if let Ok(output) = self
                .native
                .tmux(
                    ["show-environment", "-g"].map(std::ffi::OsString::from),
                    None,
                )
                .await
            {
                for line in String::from_utf8_lossy(&output.stdout).lines() {
                    if let Some((name, value)) = line.split_once('=')
                        && SESSION_VARIABLES.contains(&name)
                    {
                        session.push((name.to_owned(), value.to_owned()));
                    }
                }
            }
            self.session = Some(session);
        }
        let session = self.session.as_deref().unwrap_or_default();
        let mut env: Vec<(String, String)> = kept
            .into_iter()
            .filter(|(name, _)| !session.iter().any(|(other, _)| other == name))
            .collect();
        env.extend(session.iter().cloned());
        env
    }

    /// Sends one test notice through every route at once, outside the
    /// queues and the log; the reply names each route's outcome.
    pub(crate) fn test(
        &self,
        settings: Arc<Settings>,
        env: Vec<(String, String)>,
        reply: tokio::sync::oneshot::Sender<Value>,
    ) {
        let batch = Batch {
            server: self.server.clone(),
            notices: vec![Notice {
                id: 0,
                kind: "test".into(),
                agent: "masil".into(),
                pane: String::new(),
                run: String::new(),
                provider: String::new(),
                observed_ms: crate::observation::now_ms(),
                summary: None,
                job: None,
            }],
            stale: 0,
        };
        let native = self.native.clone();
        let gate = |route: &str| self.gates.get(route).cloned().unwrap_or_default();
        let (tmux_gate, os_gate, hook_gate) = (gate("tmux"), gate("os"), gate("hook"));
        tokio::spawn(async move {
            let detail = settings.detail;
            let outcome = |result: Result<(), String>| match result {
                Ok(()) => "sent".to_owned(),
                Err(error) => format!("failed: {error}"),
            };
            let tmux = async {
                if settings.tmux {
                    let _held = tmux_gate.lock().await;
                    Some(outcome(
                        routes::tmux(&native, &batch, detail, settings.bell).await,
                    ))
                } else {
                    None
                }
            };
            let os = async {
                if settings.os {
                    let _held = os_gate.lock().await;
                    Some(outcome(routes::os(&batch, detail, &env).await))
                } else {
                    None
                }
            };
            let hook = async {
                if settings.hook.is_empty() {
                    None
                } else {
                    let _held = hook_gate.lock().await;
                    Some(outcome(
                        routes::hook(&settings.hook, &batch, detail, &env).await,
                    ))
                }
            };
            let (tmux, os, hook) = tokio::join!(tmux, os, hook);
            let mut routes = serde_json::Map::new();
            for (name, result) in [("tmux", tmux), ("os", os), ("hook", hook)] {
                if let Some(result) = result {
                    routes.insert(name.into(), json!(result));
                }
            }
            let _ = reply.send(json!({"routes": routes}));
        });
    }

    /// Hands the claimed events to their routes. A route with a full
    /// backlog gets none; its rows are returned to record as skipped.
    pub(crate) fn send(
        &mut self,
        events: Vec<NotifyEvent>,
        stale: usize,
        settings: Arc<Settings>,
        env: Vec<(String, String)>,
    ) -> Vec<i64> {
        let env = Arc::new(env);
        let mut skipped = Vec::new();
        for route in settings.routes() {
            let mut rows = Vec::new();
            let notices: Vec<Notice> = events
                .iter()
                .filter_map(|event| {
                    let (_, row) = event.rows.iter().find(|(name, _)| *name == route)?;
                    rows.push(*row);
                    Some(Notice::from_event(
                        event,
                        self.names.get(&event.run).cloned(),
                    ))
                })
                .collect();
            if notices.is_empty() && stale == 0 {
                continue;
            }
            let job = Job {
                batch: Batch {
                    server: self.server.clone(),
                    notices,
                    stale,
                },
                rows,
                settings: settings.clone(),
                env: env.clone(),
            };
            let sender = self.route(&route);
            if let Err(error) = sender.try_send(job) {
                let job = match error {
                    mpsc::error::TrySendError::Full(job)
                    | mpsc::error::TrySendError::Closed(job) => job,
                };
                skipped.extend(job.rows);
            }
        }
        skipped
    }

    fn route(&mut self, name: &str) -> mpsc::Sender<Job> {
        if let Some((sender, task)) = self.routes.get(name)
            && !task.is_finished()
        {
            return sender.clone();
        }
        let (sender, jobs) = mpsc::channel(BACKLOG);
        let task = tokio::spawn(run_route(
            name.to_owned(),
            self.native.clone(),
            jobs,
            self.done.clone(),
            self.gates.get(name).cloned().unwrap_or_default(),
        ));
        self.routes.insert(name.to_owned(), (sender.clone(), task));
        sender
    }
}

async fn run_route(
    name: String,
    native: Context,
    mut jobs: mpsc::Receiver<Job>,
    done: mpsc::UnboundedSender<Outcome>,
    gate: Arc<tokio::sync::Mutex<()>>,
) {
    while let Some(mut job) = jobs.recv().await {
        let held = gate.lock().await;
        while let Ok(more) = jobs.try_recv() {
            job.merge(more);
        }
        let started = tokio::time::Instant::now();
        let detail = job.settings.detail;
        let result = match name.as_str() {
            "tmux" => routes::tmux(&native, &job.batch, detail, job.settings.bell).await,
            "os" => routes::os(&job.batch, detail, &job.env).await,
            "hook" => routes::hook(&job.settings.hook, &job.batch, detail, &job.env).await,
            _ => Err("unknown route".into()),
        };
        let outcome = match result {
            Ok(()) => "sent".to_owned(),
            Err(error) => format!("failed: {error}"),
        };
        let _ = done.send((job.rows, outcome));
        drop(held);
        if name == "tmux" {
            tokio::time::sleep_until(started + TMUX_EVERY).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notice(kind: &str, agent: &str) -> Notice {
        Notice {
            id: 7,
            kind: kind.into(),
            agent: agent.into(),
            pane: "%1".into(),
            run: "r".into(),
            provider: "claude".into(),
            observed_ms: 1,
            summary: Some(json!({"tool": "Bash: rm -rf /tmp/x\nmore"})),
            job: None,
        }
    }

    #[test]
    fn lines_say_who_and_what_and_only_add_detail_when_asked() {
        let one = notice("approval_requested", "builder");
        assert_eq!(one.line(false), "builder: approval requested");
        assert_eq!(
            one.line(true),
            "builder: approval requested — Bash: rm -rf /tmp/x"
        );
        let batch = Batch {
            server: "s".into(),
            notices: vec![
                one.clone(),
                notice("turn_completed", "a"),
                notice("turn_completed", "b"),
                notice("error", "c"),
            ],
            stale: 0,
        };
        assert_eq!(
            batch.lines(false),
            ["masil: 1 approval requested, 2 turn completed, 1 error"]
        );
        let hook = batch.hook_json(false);
        assert_eq!(hook["events"].as_array().unwrap().len(), 4);
        assert!(hook["events"][0]["summary"].is_null());
        let quiet = Batch {
            server: "s".into(),
            notices: vec![],
            stale: 5,
        };
        assert_eq!(
            quiet.lines(false),
            ["masil: 5 while masil was not running; see the inbox"]
        );
    }
}
