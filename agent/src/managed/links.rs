//! Connections this server's coordinator holds to its endpoints (P8b,
//! docs/agent-endpoints.md): one `rpc --stream --no-start` child per wanted
//! endpoint, kept up with backoff. It carries list, get and read for the
//! desk, and merges the endpoint's inbox into this one. Everything that
//! changes an endpoint goes by its own call, as before.

use super::Manager;
use super::endpoints::{self, Endpoint, Request};
use super::operations::LinkRow;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Notify, mpsc, oneshot};

/// How often wanted links are compared with the store and endpoints.json.
const RECONCILE: Duration = Duration::from_secs(30);
/// The hello must come within this (ssh connects in 5 s).
const HELLO: Duration = Duration::from_secs(10);
/// Quiet this long, the link pings; no pong this long after, it is cut.
const QUIET: Duration = Duration::from_secs(30);
const PONG: Duration = Duration::from_secs(10);
/// Merged events of a link down this long are settled.
const DOWN_SETTLE: Duration = Duration::from_secs(30);
const BACKOFF_FIRST: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// A connection that stayed up this long starts the backoff over.
const STEADY: Duration = Duration::from_secs(60);
/// A frame read from the stream: a list of 64 agents fits.
const MAX_FRAME: usize = 1024 * 1024 + 4096;
/// A call through a link takes no longer than a call of its own would.
pub(crate) const CALL_DEADLINE: Duration = Duration::from_secs(8);

fn now_ms() -> u64 {
    crate::observation::now_ms()
}

/// What a link is doing, for `endpoints status` and the desk.
#[derive(Clone, Debug)]
struct Status {
    state: &'static str,
    detail: Option<String>,
    since_ms: u64,
    remote: Value,
}

impl Status {
    fn new(state: &'static str) -> Self {
        Self {
            state,
            detail: None,
            since_ms: now_ms(),
            remote: Value::Null,
        }
    }
}

/// One read the desk sends through a link, and where its answer goes.
struct Call {
    request: Request,
    reply: oneshot::Sender<Result<Value, String>>,
}

struct Link {
    key: String,
    revision: i64,
    status: Arc<Mutex<Status>>,
    calls: mpsc::Sender<Call>,
    stop: Arc<Notify>,
    stopped: Arc<AtomicBool>,
}

impl Link {
    fn halt(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.stop.notify_one();
    }
}

/// The coordinator's links, shared with its socket clients.
#[derive(Default)]
pub(crate) struct Links {
    links: Mutex<HashMap<String, Link>>,
    wake: Notify,
    stopping: AtomicBool,
    running: AtomicBool,
}

impl Links {
    /// Waits up to `limit` for the links thread to end (it settles the
    /// merged events last).
    pub(crate) async fn stopped(&self, limit: Duration) {
        let deadline = tokio::time::Instant::now() + limit;
        while self.running.load(Ordering::SeqCst) && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Looks at the wanted links again now.
    pub(crate) fn wake(&self) {
        self.wake.notify_one();
    }

    /// Ends every link and the thread that keeps them.
    pub(crate) fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        if let Ok(links) = self.links.lock() {
            for link in links.values() {
                link.halt();
            }
        }
        self.wake.notify_one();
    }

    /// Starts the thread that keeps the links, unless it runs.
    pub(crate) fn start(
        self: &Arc<Self>,
        socket: PathBuf,
        poke: Arc<dyn Fn() + Send + Sync>,
        log: Arc<dyn Fn(&str) + Send + Sync>,
    ) {
        // A thread still ending sees this and goes on.
        self.stopping.store(false, Ordering::SeqCst);
        if self.running.swap(true, Ordering::SeqCst) {
            return;
        }
        let links = self.clone();
        let spawned = std::thread::Builder::new()
            .name("links".into())
            .spawn(move || {
                loop {
                    links.serve(socket.clone(), poke.clone(), log.clone());
                    links.running.store(false, Ordering::SeqCst);
                    // A start that came while this one was ending runs on.
                    if links.stopping.load(Ordering::SeqCst)
                        || links.running.swap(true, Ordering::SeqCst)
                    {
                        return;
                    }
                }
            });
        if spawned.is_err() {
            self.running.store(false, Ordering::SeqCst);
        }
    }

    /// A read through the endpoint's link, if it is up and still the
    /// endpoint `key` describes; else `not_connected`, and the caller makes
    /// its own call.
    pub(crate) async fn call(
        &self,
        endpoint: &str,
        key: &str,
        request: Request,
    ) -> Result<Value, String> {
        let streamed = matches!(
            request,
            Request::List
                | Request::Get { .. }
                | Request::Action {
                    action: endpoints::Action::Read { .. },
                    ..
                }
        );
        if !streamed {
            return Err("not_connected: only list, get and read go over a link".into());
        }
        let sender = {
            let links = self.links.lock().map_err(|_| "links unavailable")?;
            let Some(link) = links.get(endpoint) else {
                return Err("not_connected: no link to this endpoint".into());
            };
            let up = link
                .status
                .lock()
                .is_ok_and(|status| status.state == "connected");
            if link.key != key || !up {
                return Err("not_connected: the link is down or describes another endpoint".into());
            }
            link.calls.clone()
        };
        let (reply, answer) = oneshot::channel();
        sender
            .try_send(Call { request, reply })
            .map_err(|_| "not_connected: the link is busy")?;
        match tokio::time::timeout(CALL_DEADLINE, answer).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("not_connected: the link closed".into()),
            // A read: the caller may make its own call.
            Err(_) => Err(format!(
                "not_connected: endpoint '{endpoint}' did not answer through the link within 8 seconds"
            )),
        }
    }

    /// Every link this coordinator keeps.
    pub(crate) fn status(&self) -> Value {
        let Ok(links) = self.links.lock() else {
            return json!({});
        };
        let mut report = serde_json::Map::new();
        for (id, link) in links.iter() {
            if let Ok(status) = link.status.lock() {
                report.insert(
                    id.clone(),
                    json!({
                        "state": status.state,
                        "detail": status.detail,
                        "since_ms": status.since_ms,
                        "remote": status.remote,
                        "key": link.key,
                        "revision": link.revision,
                    }),
                );
            }
        }
        Value::Object(report)
    }

    fn serve(
        self: &Arc<Self>,
        socket: PathBuf,
        poke: Arc<dyn Fn() + Send + Sync>,
        log: Arc<dyn Fn(&str) + Send + Sync>,
    ) {
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return log("links: no runtime");
        };
        runtime.block_on(async {
            let manager = match Manager::new(socket.clone(), None) {
                Ok(manager) => Arc::new(manager),
                Err(error) => return log(&format!("links: {error}")),
            };
            loop {
                while !self.stopping.load(Ordering::SeqCst) {
                    if let Err(error) = self.reconcile(&manager, &poke, &log).await {
                        log(&format!("links: {error}"));
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(RECONCILE) => {}
                        _ = self.wake.notified() => {}
                    }
                }
                if let Ok(mut links) = self.links.lock() {
                    for (_, link) in links.drain() {
                        link.halt();
                    }
                }
                // Let the link tasks end their children.
                tokio::time::sleep(Duration::from_millis(200)).await;
                // Nothing of an endpoint waits here once no link runs.
                if let Err(error) = settle_all(&manager, &poke).await {
                    log(&format!("links: {error}"));
                }
                if self.stopping.load(Ordering::SeqCst) {
                    break;
                }
            }
        });
    }

    /// Starts, keeps or ends links to match what is wanted.
    async fn reconcile(
        self: &Arc<Self>,
        manager: &Arc<Manager>,
        poke: &Arc<dyn Fn() + Send + Sync>,
        log: &Arc<dyn Fn(&str) + Send + Sync>,
    ) -> Result<(), String> {
        let mut store = manager.operation_store().await?;
        let rows = store.links()?;
        let local = store.instance()?;
        let mut wanted: HashMap<String, (Endpoint, String, LinkRow)> = HashMap::new();
        for row in rows.iter().filter(|row| row.desired == "connected") {
            let root = row.config_root.as_deref().map(Path::new);
            let found = endpoints::load_in(root)
                .ok()
                .and_then(|all| all.into_iter().find(|e| e.id == row.endpoint && e.enabled));
            match found {
                Some(endpoint) => {
                    let key = super::fleet::key(&endpoint);
                    wanted.insert(row.endpoint.clone(), (endpoint, key, row.clone()));
                }
                // Removed or disabled: nothing of it waits here.
                None => {
                    store.link_settle(&row.endpoint, "remote_unavailable", now_ms())?;
                }
            }
        }
        let mut links = self.links.lock().map_err(|_| "links unavailable")?;
        links.retain(|id, link| {
            let keep = !link.stopped.load(Ordering::SeqCst)
                && wanted
                    .get(id)
                    .is_some_and(|(_, key, row)| *key == link.key && row.revision == link.revision);
            if !keep {
                link.halt();
            }
            keep
        });
        for (id, (endpoint, key, row)) in wanted {
            if links.contains_key(&id) {
                continue;
            }
            let (calls, receiver) = mpsc::channel(16);
            let link = Link {
                key: key.clone(),
                revision: row.revision,
                status: Arc::new(Mutex::new(Status::new("connecting"))),
                calls,
                stop: Arc::new(Notify::new()),
                stopped: Arc::new(AtomicBool::new(false)),
            };
            let task = Task {
                endpoint,
                row,
                local: local.to_string(),
                status: link.status.clone(),
                stop: link.stop.clone(),
                stopped: link.stopped.clone(),
                manager: manager.clone(),
                poke: poke.clone(),
                log: log.clone(),
            };
            tokio::spawn(task.run(receiver));
            links.insert(id, link);
        }
        Ok(())
    }
}

/// How one connection ended.
enum Ended {
    /// Asked to stop.
    Stopped,
    /// Down for a reason a retry cannot change: wait for the person.
    Final(&'static str, String),
    /// Down for now; `steady` if it had stayed up a while, `connected` if
    /// it got past its hello.
    Retry(String, bool, bool),
}

struct Task {
    endpoint: Endpoint,
    row: LinkRow,
    /// This server's store instance: a link to itself is refused.
    local: String,
    status: Arc<Mutex<Status>>,
    stop: Arc<Notify>,
    stopped: Arc<AtomicBool>,
    manager: Arc<Manager>,
    poke: Arc<dyn Fn() + Send + Sync>,
    log: Arc<dyn Fn(&str) + Send + Sync>,
}

impl Task {
    fn set(&self, state: &'static str, detail: Option<String>) {
        if let Ok(mut status) = self.status.lock()
            && (status.state != state || status.detail != detail)
        {
            status.state = state;
            status.detail = detail;
            status.since_ms = now_ms();
        }
    }

    async fn run(self, mut calls: mpsc::Receiver<Call>) {
        let mut backoff = BACKOFF_FIRST;
        let mut down_since = Instant::now();
        let mut settled = false;
        let mut seed = rand_seed();
        loop {
            if self.stopped.load(Ordering::SeqCst) {
                return;
            }
            self.set("connecting", None);
            let ended = self.connect(&mut calls, &mut down_since).await;
            let detail = match ended {
                Ended::Stopped => return,
                Ended::Final(state, detail) => {
                    self.set(state, Some(detail));
                    // Down for good: settle, and answer calls until stopped.
                    let _ = self.settle("remote_disconnected").await;
                    loop {
                        tokio::select! {
                            _ = self.stop.notified() => return,
                            Some(call) = calls.recv() => {
                                let _ = call.reply.send(Err("not_connected: the link is down".into()));
                            }
                        }
                    }
                }
                Ended::Retry(detail, steady, connected) => {
                    if steady {
                        backoff = BACKOFF_FIRST;
                    }
                    // Back up even briefly: a seed may have reopened events,
                    // and the next fall settles them again.
                    if connected {
                        settled = false;
                    }
                    detail
                }
            };
            self.set("backoff", Some(detail));
            // ±20% so links that fell together do not return together.
            seed = seed ^ (seed << 13) ^ (seed >> 7) ^ (seed << 17);
            let spread = (seed % 401) as f64 / 1000.0 - 0.2;
            let wait = backoff.mul_f64(1.0 + spread);
            backoff = (backoff * 2).min(BACKOFF_MAX);
            let until = tokio::time::Instant::now() + wait;
            loop {
                let settle_at = down_since + DOWN_SETTLE;
                tokio::select! {
                    _ = tokio::time::sleep_until(until) => break,
                    _ = self.stop.notified() => return,
                    _ = tokio::time::sleep_until(settle_at.into()), if !settled => {
                        settled = true;
                        let _ = self.settle("remote_disconnected").await;
                    }
                    Some(call) = calls.recv() => {
                        let _ = call.reply.send(Err("not_connected: the link is down".into()));
                    }
                }
            }
        }
    }

    async fn settle(&self, resolution: &str) -> Result<(), String> {
        let mut store = self.manager.operation_store().await?;
        if store.link_settle(&self.endpoint.id, resolution, now_ms())? > 0 {
            (self.poke)();
        }
        Ok(())
    }

    /// One connection, from spawning the child to its end.
    async fn connect(&self, calls: &mut mpsc::Receiver<Call>, down_since: &mut Instant) -> Ended {
        let mut command = self
            .endpoint
            .stream_command(self.row.ssh_auth_sock.as_deref());
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => return Ended::Retry(format!("cannot start: {error}"), false, false),
        };
        let group = child.id().map(|pid| pid as i32);
        let mut guard = Group(group);
        let (Some(mut stdin), Some(stdout), Some(mut stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            return Ended::Retry("no pipes".into(), false, false);
        };
        let errors = tokio::spawn(async move {
            let mut text = Vec::new();
            let _ = (&mut stderr).take(64 * 1024).read_to_end(&mut text).await;
            String::from_utf8_lossy(&text).into_owned()
        });
        let mut incoming = super::stream::frames(stdout, MAX_FRAME);
        let hello = tokio::select! {
            read = tokio::time::timeout(HELLO, incoming.recv()) => read,
            _ = self.stop.notified() => return Ended::Stopped,
        };
        let hello = match hello {
            Ok(Some(Ok(bytes))) => serde_json::from_slice::<Value>(&bytes).ok(),
            _ => None,
        };
        let Some(hello) = hello.filter(|hello| hello["version"] == 1) else {
            let status = tokio::time::timeout(Duration::from_secs(2), child.wait())
                .await
                .ok()
                .and_then(Result::ok);
            if status.is_some() {
                // Reaped: its group ID is no longer this link's to signal.
                guard.0 = None;
            }
            let stderr = tokio::time::timeout(Duration::from_secs(1), errors)
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or_default();
            return classify(status.and_then(|status| status.code()), &stderr);
        };
        let hello = &hello["hello"];
        if hello["protocol"][0] != json!(super::stream::PROTOCOL[0]) {
            return Ended::Final(
                "version_mismatch",
                format!("the endpoint speaks stream protocol {}", hello["protocol"]),
            );
        }
        if hello["status"]["store"].as_str() == Some(self.local.as_str()) {
            return Ended::Final(
                "self_link",
                "the endpoint is this server; a link to it would repeat its own events".into(),
            );
        }
        let skew = hello["now_ms"]
            .as_i64()
            .map_or(0, |remote| now_ms() as i64 - remote);
        self.remote(&hello["status"]);
        let connected_at = Instant::now();
        let subscribe =
            json!({"id": 0, "subscribe": {"instance": self.row.instance, "after": null}});
        if write(&mut stdin, &subscribe).await.is_err() {
            return Ended::Retry("the stream closed".into(), false, false);
        }
        *down_since = Instant::now();
        let mut pending: HashMap<u64, oneshot::Sender<Result<Value, String>>> = HashMap::new();
        let mut next = 1u64;
        let mut last_frame = Instant::now();
        let mut ping: Option<(u64, Instant)> = None;
        let ended = loop {
            // Asleep until something is due: a ping after quiet, or the
            // pong's deadline. No wakeups while frames come.
            let due = match ping {
                Some((_, sent)) => sent + PONG,
                None => last_frame + QUIET,
            };
            tokio::select! {
                read = incoming.recv() => {
                    let bytes = match read {
                        Some(Ok(bytes)) => bytes,
                        None => break "the stream closed".to_owned(),
                        Some(Err(error)) => break error,
                    };
                    last_frame = Instant::now();
                    let Ok(frame) = serde_json::from_slice::<Value>(&bytes) else {
                        break "the stream sent something that is not JSON".into();
                    };
                    if let Some(id) = frame.get("id").and_then(Value::as_u64) {
                        if ping.is_some_and(|(sent, _)| sent == id) {
                            ping = None;
                        } else if let Some(reply) = pending.remove(&id) {
                            let result = if frame["ok"] == true {
                                Ok(frame["value"].clone())
                            } else {
                                Err(frame["error"].as_str().unwrap_or("the endpoint failed").to_owned())
                            };
                            let _ = reply.send(result);
                        }
                    } else if let Some(event) = frame["event"].as_str()
                        && let Err(error) = self.event(event, &frame, skew).await
                    {
                        (self.log)(&format!("link {}: {error}", self.endpoint.id));
                        // The far side moved past these events: reconnect,
                        // and its seed sends what is still open.
                        if event == "inbox" {
                            break format!("events could not be merged: {error}");
                        }
                    }
                }
                Some(call) = calls.recv() => {
                    let id = next;
                    next += 1;
                    let request = json!({"id": id, "request": call.request});
                    if write(&mut stdin, &request).await.is_err() {
                        let _ = call.reply.send(Err("not_connected: the link closed".into()));
                        break "the stream closed".into();
                    }
                    pending.insert(id, call.reply);
                }
                _ = tokio::time::sleep_until(due.into()) => {
                    match ping {
                        Some((_, sent)) if sent.elapsed() >= PONG => {
                            break "no answer to a ping; the connection is half open".into();
                        }
                        None if last_frame.elapsed() >= QUIET => {
                            let id = next;
                            next += 1;
                            if write(&mut stdin, &json!({"id": id, "ping": true})).await.is_err() {
                                break "the stream closed".into();
                            }
                            ping = Some((id, Instant::now()));
                        }
                        _ => {}
                    }
                    // Calls the far side lost are answered by the deadline
                    // of whoever waits; drop their senders here.
                    pending.retain(|_, reply| !reply.is_closed());
                }
                _ = self.stop.notified() => return Ended::Stopped,
            }
        };
        for (_, reply) in pending.drain() {
            let _ = reply.send(Err("not_connected: the link closed".into()));
        }
        *down_since = Instant::now();
        let _ = child.start_kill();
        Ended::Retry(ended, connected_at.elapsed() >= STEADY, true)
    }

    /// What the endpoint says about itself.
    fn remote(&self, status: &Value) {
        let running = status["server"] == "running";
        if let Ok(mut current) = self.status.lock() {
            current.remote = status.clone();
        }
        self.set(
            if running {
                "connected"
            } else {
                "server_not_running"
            },
            None,
        );
    }

    async fn event(&self, event: &str, frame: &Value, skew: i64) -> Result<(), String> {
        let instance = frame["instance"].as_i64();
        match (event, instance) {
            ("inbox", Some(instance)) => {
                let items = frame["items"].as_array().cloned().unwrap_or_default();
                let cursor = frame["cursor"]
                    .as_i64()
                    .ok_or("inbox event without a cursor")?;
                let seed = frame["seed"] == true;
                let mut store = self.manager.operation_store().await?;
                let added = store.link_merge(
                    &self.endpoint.id,
                    self.row.revision,
                    instance,
                    &items,
                    cursor,
                    skew,
                    seed,
                    now_ms(),
                )?;
                if added.is_some_and(|added| added > 0) {
                    (self.poke)();
                }
            }
            ("open", Some(instance)) if frame["truncated"] != true => {
                let ids: Vec<i64> = frame["ids"]
                    .as_array()
                    .map(|ids| ids.iter().filter_map(Value::as_i64).collect())
                    .unwrap_or_default();
                let as_of = frame["as_of"].as_i64().ok_or("open event without as_of")?;
                let mut store = self.manager.operation_store().await?;
                if store.link_open(&self.endpoint.id, instance, as_of, &ids, now_ms())? > 0 {
                    (self.poke)();
                }
            }
            ("inbox_off", _) => {
                self.settle("remote_inbox_off").await?;
            }
            ("status", _) => self.remote(&frame["status"]),
            _ => {}
        }
        Ok(())
    }
}

/// Settles the merged events of every link: the links are ending.
async fn settle_all(manager: &Manager, poke: &Arc<dyn Fn() + Send + Sync>) -> Result<(), String> {
    let mut store = manager.operation_store().await?;
    let mut settled = 0;
    for row in store.links()? {
        let root = row.config_root.as_deref().map(Path::new);
        let present = endpoints::load_in(root)
            .ok()
            .is_some_and(|all| all.iter().any(|e| e.id == row.endpoint && e.enabled));
        let resolution = if present {
            "remote_disconnected"
        } else {
            "remote_unavailable"
        };
        settled += store.link_settle(&row.endpoint, resolution, now_ms())?;
    }
    if settled > 0 {
        poke();
    }
    Ok(())
}

/// Why a child ended before its hello.
fn classify(code: Option<i32>, stderr: &str) -> Ended {
    let first = stderr
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("no output")
        .chars()
        .take(200)
        .collect::<String>();
    if endpoints::host_key_failure(stderr) {
        return Ended::Final("host_key", first);
    }
    // ssh's own refusal ends with 255; a shell's "Permission denied" (a
    // binary that cannot run) is not one.
    // Tailscale SSH refuses by policy: "tailnet policy does not permit you".
    if code == Some(255)
        && (stderr.contains("Permission denied")
            || stderr.contains("Too many authentication failures")
            || stderr.contains("does not permit"))
    {
        return Ended::Final("auth_required", first);
    }
    if code == Some(2)
        && (stderr.contains("unknown agent command") || stderr.contains("invalid agent command"))
    {
        return Ended::Final("version_mismatch", first);
    }
    Ended::Retry(first, false, false)
}

async fn write(stdin: &mut tokio::process::ChildStdin, value: &Value) -> Result<(), String> {
    let mut bytes = serde_json::to_vec(value).map_err(|error| error.to_string())?;
    bytes.push(b'\n');
    stdin
        .write_all(&bytes)
        .await
        .map_err(|error| error.to_string())?;
    stdin.flush().await.map_err(|error| error.to_string())
}

/// The child's process group, killed when its connection ends.
struct Group(Option<i32>);

impl Drop for Group {
    fn drop(&mut self) {
        if let Some(group) = self.0 {
            // SAFETY: the group of a child this task started; `kill_on_drop`
            // reaps the leader after this.
            unsafe { libc::killpg(group, libc::SIGKILL) };
        }
    }
}

fn rand_seed() -> u64 {
    let time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or(1);
    (time ^ u64::from(std::process::id()).rotate_left(32)) | 1
}

fn usage() -> String {
    "usage: masil-agent agent [--socket MASIL_SOCKET] endpoints connect ID | disconnect ID | status [ID]".into()
}

/// `agent endpoints connect|disconnect|status`: this server's links.
pub(super) async fn command(manager: &Manager, args: &[String]) -> Result<Value, String> {
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["connect", id] => connect(manager, id).await,
        ["disconnect", id] => disconnect(manager, id).await,
        ["status"] => status(manager, None).await,
        ["status", id] => status(manager, Some(id)).await,
        _ => Err(usage()),
    }
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| error.to_string())?
}

/// Wants a link: recorded with this caller's SSH agent and configuration
/// root, which the coordinator uses for it; starts the coordinator (D1: a
/// change).
async fn connect(manager: &Manager, id: &str) -> Result<Value, String> {
    let endpoint = endpoints::load()?
        .into_iter()
        .find(|endpoint| endpoint.id == id)
        .ok_or_else(|| format!("target_absent: no endpoint {id}"))?;
    if !endpoint.enabled {
        return Err(format!("target_absent: endpoint '{id}' is disabled"));
    }
    let socket = manager.native.socket.clone();
    blocking(move || crate::coordinator::check_state(&socket).map(drop)).await?;
    let ssh_auth_sock = std::env::var("SSH_AUTH_SOCK").ok();
    let root = endpoints::config_root().map(|root| root.to_string_lossy().into_owned());
    let row = manager.operation_store().await?.link_set(
        id,
        true,
        ssh_auth_sock.as_deref(),
        root.as_deref(),
        now_ms(),
    )?;
    let socket = manager.native.socket.clone();
    let coordinator = blocking(move || {
        Ok(match crate::coordinator::ensure(&socket) {
            Ok(_) => {
                crate::coordinator::reload(&socket);
                "running".to_owned()
            }
            Err(error) => format!("error: {error}"),
        })
    })
    .await?;
    Ok(json!({
        "endpoint": id,
        "desired": row.desired,
        "revision": row.revision,
        "coordinator": coordinator,
    }))
}

/// Stops wanting a link; its merged events are settled now. Starts nothing.
async fn disconnect(manager: &Manager, id: &str) -> Result<Value, String> {
    let row = manager
        .operation_store()
        .await?
        .link_set(id, false, None, None, now_ms())?;
    let socket = manager.native.socket.clone();
    blocking(move || {
        crate::coordinator::reload(&socket);
        Ok(())
    })
    .await?;
    Ok(json!({"endpoint": id, "desired": row.desired, "revision": row.revision}))
}

/// What is wanted and what the coordinator does about it. Reads only.
async fn status(manager: &Manager, only: Option<&str>) -> Result<Value, String> {
    let socket = manager.native.socket.clone();
    let (rows, running) = blocking(move || {
        Ok((
            super::operations::Store::links_readonly(&socket)?,
            crate::coordinator::links_status(&socket),
        ))
    })
    .await?;
    let configured = endpoints::load().unwrap_or_default();
    let links: Vec<Value> = rows
        .iter()
        .filter(|row| only.is_none_or(|id| row.endpoint == id))
        .map(|row| {
            let live = running.as_ref().map(|links| links[&row.endpoint].clone());
            let key = configured
                .iter()
                .find(|endpoint| endpoint.id == row.endpoint)
                .map(super::fleet::key);
            let state = match (&live, row.desired.as_str()) {
                (_, "stopped") => json!("stopped"),
                (None, _) => json!("not_running"),
                (Some(Value::Null), _) => json!("unavailable"),
                (Some(link), _) => link["state"].clone(),
            };
            let mut value = json!({
                "endpoint": row.endpoint,
                "desired": row.desired,
                "state": state,
            });
            if let Some(link) = live.filter(|link| !link.is_null()) {
                value["detail"] = link["detail"].clone();
                value["remote"] = link["remote"].clone();
                value["since_ms"] = link["since_ms"].clone();
                // The coordinator reads the configuration root `connect`
                // recorded; this command may read another.
                if key.as_ref().is_some_and(|key| link["key"] != json!(key)) {
                    value["warning"] = json!("the coordinator uses another configuration of this endpoint than this command reads");
                }
            }
            value
        })
        .collect();
    Ok(json!({
        "coordinator": if running.is_some() { "running" } else { "not_running" },
        "links": links,
    }))
}
