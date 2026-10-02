//! The coordinator reads `--answers` OpenCode servers (P3b): the agent's
//! state, its requests, turn ends and errors.
//!
//! Only the coordinator reads them. A run's task holds `GET /event` while
//! the agent works or waits, and closes it after 30 s of quiet idle; the
//! pane drawing again opens it. Tasks only read: the watch loop writes what
//! they send (docs/agent-answers.md).

use super::{Agent, Manager, answer, inbox::Effect, integration, now_ms};
use serde_json::{Map, Value, json};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{Notify, mpsc};

/// No frame for this long is a lost connection; OpenCode sends a heartbeat
/// every 10 s.
const SILENCE: Duration = Duration::from_secs(25);
const RETRY_SECONDS: [u64; 4] = [1, 2, 5, 10];
/// A larger frame is skipped whole.
const FRAME_LIMIT: usize = 1 << 20;
/// How far a catch-up reads.
const CATCH_UP_SESSIONS: usize = 8;
const CATCH_UP_PAGES: usize = 3;
const PAGE: usize = 10;
/// A run first seen catches up this far back.
const FIRST_LOOK_BACK_MS: u64 = 120_000;
/// Turn ends and errors read from the server; requests keep the plugin's
/// key (`hook:opencode`) so one request is one event.
const SOURCE: &str = "api:opencode";
/// Frames read only for their type.
const SKIPPED: &[&str] = &[
    "message.part.delta",
    "message.part.updated",
    "message.part.removed",
];

/// What the coordinator reports for a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ApiState {
    pub state: &'static str,
    pub permissions: u32,
    pub questions: u32,
    pub live: bool,
}

pub(super) enum Update {
    State {
        run: String,
        state: ApiState,
    },
    Effects(Vec<Effect>),
    /// Reading has failed since `since` (ms).
    Lost {
        run: String,
        since: u64,
    },
    /// Reading works: any loss of this run is over, including one a previous
    /// coordinator recorded.
    Restored {
        run: String,
    },
}

/// The runs the coordinator observes, by run.
pub(super) struct Observers {
    runs: HashMap<String, Observed>,
    timing: Timing,
}

/// Times that scale with the coordinator's tick (2 s by default).
#[derive(Clone, Copy)]
struct Timing {
    /// Quiet idle that ends a connection: no session busy, no request
    /// pending, and the pane not drawing. 30 s by default.
    quiet: Duration,
    /// Failing this long writes `observation_lost`. 30 s by default.
    lost_after: Duration,
}

struct Observed {
    output: u64,
    wake: Arc<Notify>,
    task: tokio::task::JoinHandle<()>,
}

impl Observers {
    pub(super) fn new(tick: Duration) -> Self {
        Self {
            runs: HashMap::new(),
            timing: Timing {
                quiet: tick * 15,
                lost_after: tick * 15,
            },
        }
    }

    /// Starts a task for each newly observable run, wakes those whose pane
    /// drew since the last look, and stops those that are gone.
    pub(super) async fn sync(
        &mut self,
        manager: &Manager,
        agents: &[Agent],
        updates: &mpsc::UnboundedSender<Update>,
    ) {
        let mut seen = HashSet::new();
        for agent in agents.iter().filter(|agent| observable(agent)) {
            seen.insert(agent.run.clone());
            if let Some(observed) = self.runs.get_mut(&agent.run) {
                if observed.output != agent.output_generation {
                    observed.output = agent.output_generation;
                    observed.wake.notify_one();
                }
                continue;
            }
            let Ok(Some(secret)) = manager.run_secret(&agent.run).await else {
                continue;
            };
            let wake = Arc::new(Notify::new());
            let target = Target {
                run: agent.run.clone(),
                pane: agent.pane_id.clone(),
                pid: agent.foreground_group,
                secret,
            };
            let task = tokio::spawn(observe(target, self.timing, wake.clone(), updates.clone()));
            self.runs.insert(
                agent.run.clone(),
                Observed {
                    output: agent.output_generation,
                    wake,
                    task,
                },
            );
        }
        self.runs.retain(|run, observed| {
            let keep = seen.contains(run);
            if !keep {
                observed.task.abort();
            }
            keep
        });
    }

    pub(super) fn stop(&mut self) {
        for (_, observed) in self.runs.drain() {
            observed.task.abort();
        }
    }

    pub(super) fn len(&self) -> usize {
        self.runs.len()
    }
}

/// A dropped JoinHandle would detach its task; the tasks hold passwords and
/// retry connections, so they end with the watch.
impl Drop for Observers {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A running `--answers` OpenCode whose start found its server.
pub(super) fn observable(agent: &Agent) -> bool {
    answer::requested(agent)
        && agent.process == "running"
        && agent.foreground_group > 1
        && agent
            .run_evidence
            .as_ref()
            .and_then(|evidence| evidence.answer_channel_state.as_deref())
            == Some("ready")
}

struct Target {
    run: String,
    pane: String,
    pid: i32,
    secret: String,
}

/// What a run's task keeps across connections.
struct Memory {
    /// When the last connection closed; a new one catches up from there.
    closed_ms: u64,
    /// The latest turn of each session, by its user message.
    turns: HashMap<String, Turn>,
    /// Whether each session has a parent (None: a root session).
    parents: HashMap<String, Option<String>>,
    failing: Option<Instant>,
    /// When the current failures began (ms), for the loss's key.
    failing_ms: u64,
    attempt: usize,
    lost: bool,
    /// No connection worked yet in this task.
    first: bool,
}

struct Turn {
    message: String,
    /// The turn failed or the person stopped it: it does not end as done.
    errored: bool,
    ended: bool,
}

async fn observe(
    target: Target,
    timing: Timing,
    wake: Arc<Notify>,
    updates: mpsc::UnboundedSender<Update>,
) {
    let mut memory = Memory {
        closed_ms: now_ms().saturating_sub(FIRST_LOOK_BACK_MS),
        turns: HashMap::new(),
        parents: HashMap::new(),
        failing: None,
        failing_ms: 0,
        attempt: 0,
        lost: false,
        first: true,
    };
    let mut endpoint = None;
    loop {
        match follow(&target, timing, &mut endpoint, &wake, &updates, &mut memory).await {
            Ok(()) => {
                memory.closed_ms = now_ms();
                wake.notified().await;
            }
            Err(_) => {
                // The port is found again: the server may have restarted.
                endpoint = None;
                if memory.failing.is_none() {
                    memory.failing_ms = now_ms();
                }
                let since = *memory.failing.get_or_insert_with(Instant::now);
                if !memory.lost && since.elapsed() >= timing.lost_after {
                    memory.lost = true;
                    let _ = updates.send(Update::Lost {
                        run: target.run.clone(),
                        since: memory.failing_ms,
                    });
                }
                // The pane drawing does not hurry a retry: each one starts
                // lsof and probes the ports.
                let delay = RETRY_SECONDS[memory.attempt.min(RETRY_SECONDS.len() - 1)];
                memory.attempt += 1;
                tokio::time::sleep(Duration::from_secs(delay)).await;
            }
        }
    }
}

/// One connection: Ok when it closed after quiet idle, Err when it failed.
async fn follow(
    target: &Target,
    timing: Timing,
    slot: &mut Option<answer::Endpoint>,
    wake: &Notify,
    updates: &mpsc::UnboundedSender<Update>,
    memory: &mut Memory,
) -> Result<(), String> {
    if slot.is_none() {
        *slot = Some(answer::discover(target.secret.clone(), target.pid).await?);
    }
    let endpoint = slot.as_ref().ok_or("no endpoint")?;
    let mut response = endpoint.events().await?;
    let mut frames = Frames::default();
    let first = tokio::time::timeout(Duration::from_secs(5), frames.next(&mut response))
        .await
        .map_err(|_| "OpenCode sent no first event".to_owned())??;
    if first.get("type").and_then(Value::as_str) != Some("server.connected") {
        return Err("OpenCode's event stream did not start with server.connected".into());
    }
    // Read after the stream is open: events from now on wait on it, and
    // applying one the snapshot already holds changes nothing.
    let snapshot_ms = now_ms();
    let mut view = View::snapshot(endpoint).await?;
    memory.failing = None;
    memory.attempt = 0;
    if memory.lost || memory.first {
        memory.lost = false;
        memory.first = false;
        let _ = updates.send(Update::Restored {
            run: target.run.clone(),
        });
    }
    let mut effects = Vec::new();
    for (kind, request) in &view.requests {
        let frame = json!({"type": kind, "properties": request});
        if let Some(frame) = frame.as_object() {
            effects.extend(integration::api_request_effects(
                &target.pane,
                &target.run,
                frame,
            ));
        }
    }
    // Requests answered while no connection was open; one the plugin
    // recorded after the snapshot is not judged by it.
    effects.push(Effect::ResolveAbsent {
        run: target.run.clone(),
        source: "hook:opencode".into(),
        present: view.request_ids(),
        before: snapshot_ms,
        resolution: "replied",
    });
    effects.extend(catch_up(endpoint, target, &view, memory).await);
    let _ = updates.send(Update::Effects(effects));
    let mut last = view.state(true);
    let _ = updates.send(Update::State {
        run: target.run.clone(),
        state: last,
    });
    let mut quiet_since = view.idle().then(Instant::now);
    let mut last_frame = Instant::now();
    loop {
        let close_at = quiet_since.map(|at| tokio::time::Instant::from_std(at + timing.quiet));
        let silent_at = tokio::time::Instant::from_std(last_frame + SILENCE);
        let frame = tokio::select! {
            frame = frames.next(&mut response) => frame?,
            // Heartbeats come every 10 s; the pane drawing does not count.
            _ = tokio::time::sleep_until(silent_at) => {
                return Err("OpenCode's event stream went silent".into());
            }
            _ = wake.notified() => {
                // The pane drew: someone is at it, so idle is not quiet yet.
                if quiet_since.is_some() {
                    quiet_since = Some(Instant::now());
                }
                continue;
            }
            _ = sleep_until(close_at), if close_at.is_some() => {
                let _ = updates.send(Update::State {
                    run: target.run.clone(),
                    state: ApiState { live: false, ..view.state(false) },
                });
                return Ok(());
            }
        };
        last_frame = Instant::now();
        let effects = view.apply(&frame, endpoint, target, memory).await?;
        if !effects.is_empty() {
            let _ = updates.send(Update::Effects(effects));
        }
        let state = view.state(true);
        if state != last {
            last = state;
            let _ = updates.send(Update::State {
                run: target.run.clone(),
                state,
            });
        }
        quiet_since = if view.idle() {
            quiet_since.or_else(|| Some(Instant::now()))
        } else {
            None
        };
    }
}

async fn sleep_until(at: Option<tokio::time::Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// The pane's server as the events tell it.
#[derive(Default)]
struct View {
    /// Sessions working or retrying.
    busy: HashSet<String>,
    /// Pending requests: (`permission.asked` or `question.asked`, request).
    requests: Vec<(&'static str, Value)>,
}

impl View {
    async fn snapshot(endpoint: &answer::Endpoint) -> Result<Self, String> {
        let status = endpoint.get_json("/session/status").await?;
        let permissions = endpoint.get_json("/permission").await?;
        let questions = endpoint.get_json("/question").await?;
        let busy = status
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(_, status)| status["type"] != "idle")
            .map(|(session, _)| session.clone())
            .collect();
        let mut requests = Vec::new();
        for (kind, list) in [
            ("permission.asked", permissions),
            ("question.asked", questions),
        ] {
            for request in list.as_array().cloned().unwrap_or_default() {
                if request["id"].is_string() {
                    requests.push((kind, request));
                }
            }
        }
        Ok(Self { busy, requests })
    }

    fn request_ids(&self) -> Vec<String> {
        self.requests
            .iter()
            .filter_map(|(_, request)| request["id"].as_str().map(str::to_owned))
            .collect()
    }

    fn count(&self, kind: &str) -> u32 {
        self.requests.iter().filter(|(k, _)| *k == kind).count() as u32
    }

    fn idle(&self) -> bool {
        self.busy.is_empty() && self.requests.is_empty()
    }

    fn state(&self, live: bool) -> ApiState {
        let permissions = self.count("permission.asked");
        let questions = self.count("question.asked");
        ApiState {
            state: if permissions + questions > 0 {
                "blocked"
            } else if !self.busy.is_empty() {
                "working"
            } else {
                "idle"
            },
            permissions,
            questions,
            live,
        }
    }

    /// Applies one event; returns the inbox effects it makes.
    async fn apply(
        &mut self,
        frame: &Map<String, Value>,
        endpoint: &answer::Endpoint,
        target: &Target,
        memory: &mut Memory,
    ) -> Result<Vec<Effect>, String> {
        let kind = frame
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let properties = frame.get("properties").cloned().unwrap_or(Value::Null);
        let session = properties["sessionID"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let mut effects = Vec::new();
        match kind {
            "server.instance.disposed" => return Err("OpenCode's instance closed".into()),
            "session.status" => {
                if properties["status"]["type"] == "idle" {
                    if self.busy.remove(&session) {
                        effects.extend(turn_end(endpoint, target, memory, &session).await);
                    }
                } else if !session.is_empty() {
                    self.busy.insert(session);
                }
            }
            "session.created" | "session.updated" => {
                if let Some(id) = properties["info"]["id"].as_str() {
                    let parent = properties["info"]["parentID"].as_str().map(str::to_owned);
                    memory.parents.insert(id.to_owned(), parent);
                }
            }
            "session.deleted" => {
                if let Some(id) = properties["info"]["id"].as_str() {
                    self.busy.remove(id);
                    memory.turns.remove(id);
                }
            }
            "permission.asked" | "question.asked" => {
                let id = properties["id"].as_str().unwrap_or_default();
                if !id.is_empty() && !self.requests.iter().any(|(_, r)| r["id"] == id) {
                    let kind = if kind == "permission.asked" {
                        "permission.asked"
                    } else {
                        "question.asked"
                    };
                    self.requests.push((kind, properties.clone()));
                }
                effects.extend(integration::api_request_effects(
                    &target.pane,
                    &target.run,
                    frame,
                ));
            }
            "permission.replied" | "question.replied" | "question.rejected" => {
                let id = properties["requestID"].as_str().unwrap_or_default();
                self.requests.retain(|(_, request)| request["id"] != id);
                effects.extend(integration::api_request_effects(
                    &target.pane,
                    &target.run,
                    frame,
                ));
            }
            // OpenCode publishes a turn's user message again as it
            // summarizes; only a new message starts a turn.
            "message.updated" if properties["info"]["role"] == "user" => {
                if let Some(id) = properties["info"]["id"].as_str()
                    && memory
                        .turns
                        .get(&session)
                        .is_none_or(|turn| turn.message != id)
                {
                    memory.turns.insert(
                        session,
                        Turn {
                            message: id.to_owned(),
                            errored: false,
                            ended: false,
                        },
                    );
                }
            }
            // A failed assistant message failed its turn. This is how a
            // full context that is not compacted shows; a compacted one
            // leaves the message without an error.
            "message.updated" if properties["info"]["role"] == "assistant" => {
                let error = &properties["info"]["error"];
                if !error.is_null()
                    && let Some(turn) = memory.turns.get_mut(&session)
                {
                    turn.errored = true;
                    if error["name"] != "MessageAbortedError" {
                        let key = format!("error:{session}:{}", turn.message);
                        effects.push(error_event(target, &key, error));
                    }
                }
            }
            "session.error" => {
                let name = properties["error"]["name"].as_str().unwrap_or_default();
                // Published also when the context is compacted and the turn
                // goes on; a failed one shows on its assistant message.
                if name == "ContextOverflowError" {
                    return Ok(effects);
                }
                let turn = memory.turns.get_mut(&session);
                let key = match &turn {
                    Some(turn) => format!("error:{session}:{}", turn.message),
                    None => format!(
                        "error:{}",
                        frame.get("id").and_then(Value::as_str).unwrap_or_default()
                    ),
                };
                if let Some(turn) = turn {
                    turn.errored = true;
                }
                // Stopping a turn is the person's own act, not an error.
                if name != "MessageAbortedError" {
                    effects.push(error_event(target, &key, &properties["error"]));
                }
            }
            _ => {}
        }
        Ok(effects)
    }
}

/// A root session that went idle ended its turn; one that failed or was
/// stopped is not reported as done.
async fn turn_end(
    endpoint: &answer::Endpoint,
    target: &Target,
    memory: &mut Memory,
    session: &str,
) -> Vec<Effect> {
    if !root(endpoint, memory, session).await {
        return Vec::new();
    }
    let Some(turn) = memory.turns.get_mut(session) else {
        return Vec::new();
    };
    if turn.ended || turn.errored {
        return Vec::new();
    }
    turn.ended = true;
    vec![turn_completed(target, &turn.message)]
}

/// Whether `session` has no parent; asked once per session.
async fn root(endpoint: &answer::Endpoint, memory: &mut Memory, session: &str) -> bool {
    if let Some(parent) = memory.parents.get(session) {
        return parent.is_none();
    }
    let Ok(info) = endpoint.get_json(&format!("/session/{session}")).await else {
        return false;
    };
    let parent = info["parentID"].as_str().map(str::to_owned);
    let root = parent.is_none();
    memory.parents.insert(session.to_owned(), parent);
    root
}

fn turn_completed(target: &Target, message: &str) -> Effect {
    Effect::Event {
        source: SOURCE.into(),
        source_ref: format!("turn_completed:{}:{message}", target.run),
        provider: "opencode".into(),
        pane: target.pane.clone(),
        run: target.run.clone(),
        revision: None,
        kind: "turn_completed",
        native_ref: Some(message.to_owned()),
        summary: None,
    }
}

fn error_event(target: &Target, source_ref: &str, error: &Value) -> Effect {
    let name = error["name"].as_str().unwrap_or("error");
    let message = error["data"]["message"]
        .as_str()
        .or_else(|| error["message"].as_str())
        .unwrap_or_default();
    let clip = |text: &str, chars, bytes| {
        let text = text
            .chars()
            .map(|character| {
                if character.is_control() {
                    ' '
                } else {
                    character
                }
            })
            .collect::<String>();
        // Provider error text can echo keys or URLs with credentials.
        integration::summary_text(&text, chars, bytes)
    };
    Effect::Event {
        source: SOURCE.into(),
        source_ref: source_ref.to_owned(),
        provider: "opencode".into(),
        pane: target.pane.clone(),
        run: target.run.clone(),
        revision: None,
        kind: "error",
        native_ref: None,
        summary: Some(json!({
            "error": clip(name, 64, 128),
            "message": clip(message, 256, 768),
        })),
    }
}

/// Turns that started or ended while no connection was open: the user
/// messages of root sessions updated since the last close, and the turn
/// ends and errors after them.
async fn catch_up(
    endpoint: &answer::Endpoint,
    target: &Target,
    view: &View,
    memory: &mut Memory,
) -> Vec<Effect> {
    let since = memory.closed_ms.saturating_sub(2_000);
    let Ok(sessions) = endpoint
        .get_json(&format!(
            "/session?roots=true&start={since}&limit={CATCH_UP_SESSIONS}"
        ))
        .await
    else {
        return Vec::new();
    };
    let mut effects = Vec::new();
    for session in sessions.as_array().cloned().unwrap_or_default() {
        let Some(id) = session["id"].as_str() else {
            continue;
        };
        let parent = session["parentID"].as_str().map(str::to_owned);
        memory.parents.insert(id.to_owned(), parent.clone());
        if parent.is_some() {
            continue;
        }
        let messages = recent_messages(endpoint, id, since).await;
        let Some(start) = messages.iter().rposition(|message| {
            message["info"]["role"] == "user"
                && message["info"]["time"]["created"].as_u64().unwrap_or(0) >= since
        }) else {
            continue;
        };
        let Some(user) = messages[start]["info"]["id"].as_str() else {
            continue;
        };
        let mut turn = Turn {
            message: user.to_owned(),
            errored: false,
            ended: false,
        };
        let mut completed = false;
        for message in &messages[start + 1..] {
            let info = &message["info"];
            if info["role"] != "assistant" {
                continue;
            }
            let error = &info["error"];
            if !error.is_null() {
                turn.errored = true;
                if error["name"] != "MessageAbortedError" {
                    effects.push(error_event(target, &format!("error:{id}:{user}"), error));
                }
            }
            completed = info["time"]["completed"].is_u64();
        }
        if completed && !view.busy.contains(id) && !turn.errored {
            turn.ended = true;
            effects.push(turn_completed(target, user));
        }
        memory.turns.insert(id.to_owned(), turn);
    }
    effects
}

/// The session's messages back to `since`, oldest first, at most
/// CATCH_UP_PAGES pages.
async fn recent_messages(endpoint: &answer::Endpoint, session: &str, since: u64) -> Vec<Value> {
    let mut pages = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..CATCH_UP_PAGES {
        let path = match &cursor {
            Some(before) => format!("/session/{session}/message?limit={PAGE}&before={before}"),
            None => format!("/session/{session}/message?limit={PAGE}"),
        };
        let Ok((page, next)) = endpoint.get_page(&path).await else {
            break;
        };
        let items = page.as_array().cloned().unwrap_or_default();
        let older = items
            .first()
            .is_some_and(|item| item["info"]["time"]["created"].as_u64().unwrap_or(0) < since);
        pages.push(items);
        match next {
            Some(next) if !older => cursor = Some(next),
            _ => break,
        }
    }
    pages.into_iter().rev().flatten().collect()
}

/// Server-sent events split into JSON objects. A frame over FRAME_LIMIT and
/// the frames that only stream message parts are skipped without parsing.
#[derive(Default)]
struct Frames {
    buffer: Vec<u8>,
    /// Bytes already searched for a frame end.
    scanned: usize,
    /// Dropping the rest of an oversized frame.
    skipping: bool,
}

impl Frames {
    async fn next(
        &mut self,
        response: &mut reqwest::Response,
    ) -> Result<Map<String, Value>, String> {
        loop {
            // A frame end can straddle the last search: look one byte back.
            while let Some(end) = find(&self.buffer[self.scanned.saturating_sub(1)..], b"\n\n")
                .map(|at| at + self.scanned.saturating_sub(1))
            {
                let event: Vec<u8> = self.buffer.drain(..end + 2).collect();
                self.scanned = 0;
                if std::mem::take(&mut self.skipping) {
                    continue;
                }
                if let Some(frame) = parse_event(&event) {
                    return Ok(frame);
                }
            }
            self.scanned = self.buffer.len();
            if self.buffer.len() > FRAME_LIMIT {
                // Keep the framing: drop what is read and the rest of this
                // event up to its blank line. A trailing newline may be the
                // first half of that blank line.
                let newline = self.buffer.last() == Some(&b'\n');
                self.buffer.clear();
                if newline {
                    self.buffer.push(b'\n');
                }
                self.scanned = self.buffer.len();
                self.skipping = true;
            }
            // Cancel-safe: nothing is consumed until a chunk arrives.
            match response.chunk().await {
                Ok(Some(chunk)) => self
                    .buffer
                    .extend(chunk.iter().filter(|byte| **byte != b'\r')),
                Ok(None) => return Err("OpenCode closed its event stream".into()),
                Err(error) => return Err(format!("OpenCode's event stream failed: {error}")),
            }
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// The JSON object of one event's `data:` lines; None for comments, other
/// fields and skipped types.
fn parse_event(event: &[u8]) -> Option<Map<String, Value>> {
    let mut data = Vec::new();
    for line in event.split(|byte| *byte == b'\n') {
        if let Some(rest) = line.strip_prefix(b"data:") {
            if !data.is_empty() {
                data.push(b'\n');
            }
            data.extend_from_slice(rest.strip_prefix(b" ").unwrap_or(rest));
        }
    }
    if data.is_empty() || event_type(&data).is_some_and(|kind| SKIPPED.contains(&kind)) {
        return None;
    }
    match serde_json::from_slice::<Value>(&data) {
        Ok(Value::Object(frame)) => Some(frame),
        _ => None,
    }
}

/// The `"type"` value near the start of an event, read without parsing it.
fn event_type(data: &[u8]) -> Option<&str> {
    let head = &data[..data.len().min(256)];
    let at = find(head, b"\"type\":\"")? + 8;
    let end = head[at..].iter().position(|byte| *byte == b'"')?;
    std::str::from_utf8(&head[at..at + end]).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_are_split_and_part_streams_skipped_unparsed() {
        let frame = parse_event(b"data: {\"id\":\"e1\",\"type\":\"session.status\",\"properties\":{\"sessionID\":\"s\"}}\n\n");
        assert_eq!(frame.unwrap()["type"], "session.status");
        // Not valid JSON, but skipped before parsing by its type.
        assert!(
            parse_event(
                b"data: {\"id\":\"e2\",\"type\":\"message.part.delta\",\"properties\":{broken\n\n"
            )
            .is_none()
        );
        assert!(parse_event(b": heartbeat\n\n").is_none());
        let joined = parse_event(b"data: {\"type\":\ndata: \"x\"}\n\n");
        assert_eq!(joined.unwrap()["type"], "x");
    }

    #[test]
    fn state_comes_from_requests_then_busy_sessions() {
        let mut view = View::default();
        assert_eq!(view.state(true).state, "idle");
        view.busy.insert("s".into());
        assert_eq!(view.state(true).state, "working");
        view.requests.push(("question.asked", json!({"id": "q"})));
        let state = view.state(true);
        assert_eq!(
            (state.state, state.permissions, state.questions),
            ("blocked", 0, 1)
        );
        assert!(!view.idle());
    }
}
