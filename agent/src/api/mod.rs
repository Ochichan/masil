//! The extension API v1 (P9, docs/extensions.md): JSON lines on a byte
//! stream. A client says hello, then calls CLI verbs by name; each call runs
//! this binary's `agent` command and answers with its JSON and exit code. A
//! scope decides which verbs a connection may call; nothing that answers for
//! a person is in any (scope.rs). `masil-agent api` serves its standard
//! input and output; an extension masil starts is served on FD 3.

pub(crate) mod ext;
pub(crate) mod mcp;
pub(crate) mod scope;
pub(crate) mod tokens;

use scope::Scope;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Notify, Semaphore, mpsc};

pub(crate) const VERSION: u64 = 1;
const MAX_IN: usize = 256 * 1024;
/// Answers over this are refused as `response_too_large`.
const MAX_OUT: usize = 1024 * 1024;
const STDERR: usize = 64 * 1024;
/// Events waiting for a slow reader; past this the subscription ends.
const EVENTS: usize = 256;
/// A reader that takes no answer for this long is closed.
const WRITE_STALL: Duration = Duration::from_secs(30);
/// A call may take this long (`wait`, whose own limit is 5 minutes, a
/// little longer); then its process group gets TERM and, 2 s later, KILL.
pub(super) const CALL_TIME: Duration = Duration::from_secs(60);
const WAIT_TIME: Duration = Duration::from_secs(310);

/// What a connection may do and how much.
pub(crate) struct Session {
    /// The masil server the verbs act on.
    pub(crate) socket: Option<PathBuf>,
    pub(crate) scope: Scope,
    /// A token whose revocation closes this connection.
    pub(crate) token: Option<tokens::Held>,
    /// Events this connection may subscribe to.
    pub(crate) events: Vec<String>,
    /// How often a subscription looks at the inbox.
    pub(crate) inbox_poll: Duration,
    /// Calls a second, and a burst.
    pub(crate) rate: (f64, f64),
    /// Calls at once on this connection.
    pub(crate) concurrent: usize,
    /// Calls at once across the connections that share it (the
    /// coordinator's extensions).
    pub(crate) shared: Option<Arc<Semaphore>>,
    /// With no request and no subscription this long, the connection ends.
    pub(crate) idle: Option<Duration>,
    /// Inbox batches one watcher reads for many connections (the
    /// coordinator's extensions); without it each subscription polls.
    pub(crate) feed: Option<tokio::sync::broadcast::Sender<Value>>,
}

impl Session {
    pub(crate) fn new(socket: Option<PathBuf>, scope: Scope) -> Self {
        Self {
            socket,
            scope,
            token: None,
            events: vec!["inbox".into()],
            inbox_poll: Duration::from_secs(5),
            rate: (5.0, 20.0),
            concurrent: 2,
            shared: None,
            idle: None,
            feed: None,
        }
    }
}

/// The masil server this process runs in, if it runs in one.
pub(crate) fn current_socket() -> Option<PathBuf> {
    std::env::var("TMUX")
        .ok()
        .and_then(|value| value.rsplitn(3, ',').last().map(PathBuf::from))
        .or_else(|| std::env::var_os("MASIL_AGENT_SOCKET").map(PathBuf::from))
        .filter(|path| path.is_absolute())
}

fn failure(id: &Value, code: &str, message: &str) -> Value {
    let exit = crate::managed::failure::classify(&format!("{code}:"))
        .0
        .exit_code();
    json!({"id": id, "ok": false, "exit": exit, "error": {"code": code, "message": message}})
}

/// TERM to a process group, and KILL 2 s later to whatever is left in it.
/// The leader is `child`, reaped here if it was not.
pub(crate) async fn end_group(group: i32, child: &mut tokio::process::Child) {
    // SAFETY: the group is the child's; its leader is not reaped yet, or
    // was reaped just now and the group still has its other members.
    if unsafe { libc::killpg(group, libc::SIGTERM) } != 0 {
        return;
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let _ = tokio::time::timeout_at(deadline, child.wait()).await;
    while tokio::time::Instant::now() < deadline {
        // SAFETY: signal 0 only checks.
        if unsafe { libc::killpg(group, 0) } != 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // SAFETY: as above.
    unsafe { libc::killpg(group, libc::SIGKILL) };
    let _ = child.wait().await;
}

/// Tasks that end with the connection, also when `serve` is dropped:
/// its read calls, its subscription and its writer.
struct Aborting(Vec<tokio::task::JoinHandle<()>>);

impl Drop for Aborting {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

/// Set when the connection ends, also when `serve` is dropped: an act that
/// is still waiting for the shared permit then never runs.
struct Ended(Arc<std::sync::atomic::AtomicBool>);

impl Drop for Ended {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

/// A token bucket.
struct Rate {
    tokens: f64,
    per_second: f64,
    burst: f64,
    at: Instant,
}

impl Rate {
    fn take(&mut self) -> bool {
        let now = Instant::now();
        self.tokens = (self.tokens + now.duration_since(self.at).as_secs_f64() * self.per_second)
            .min(self.burst);
        self.at = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// Serves one connection until its reader ends, it is closed, or it idles.
pub(crate) async fn serve<R, W>(reader: R, writer: W, session: Session)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let session = Arc::new(session);
    let mut incoming = crate::managed::stream::frames(reader, MAX_IN);
    // Answers wait for room; events are bounded.
    let (responses, mut queued) = mpsc::channel::<Value>(64);
    let (events, mut queued_events) = mpsc::channel::<Value>(EVENTS);
    let close = Arc::new(Notify::new());
    let stalled = close.clone();
    let ended = Ended(Arc::default());
    // Ends with `serve`; at its normal end it first writes out the answers.
    let mut writing = Aborting(vec![tokio::spawn(async move {
        let mut writer = writer;
        loop {
            let next = tokio::select! {
                biased;
                Some(value) = queued.recv() => value,
                Some(value) = queued_events.recv() => value,
                else => return,
            };
            let mut line = serde_json::to_vec(&next).unwrap_or_default();
            line.push(b'\n');
            let written = tokio::time::timeout(WRITE_STALL, async {
                writer.write_all(&line).await?;
                writer.flush().await
            })
            .await;
            if !matches!(written, Ok(Ok(()))) {
                // Nobody reads the answers: no more calls.
                stalled.notify_one();
                return;
            }
        }
    })]);
    let permits = Arc::new(Semaphore::new(session.concurrent));
    let mut rate = Rate {
        tokens: session.rate.1,
        per_second: session.rate.0,
        burst: session.rate.1,
        at: Instant::now(),
    };
    let mut greeted = false;
    let mut subscription = Aborting(Vec::new());
    let mut acting: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    // Reads end with the connection, also when `serve` itself is dropped.
    let mut reading = Aborting(Vec::new());
    loop {
        let idle = session
            .idle
            .filter(|_| subscription.0.iter().all(|task| task.is_finished()))
            .unwrap_or(Duration::from_secs(365 * 24 * 60 * 60));
        let bytes = tokio::select! {
            read = incoming.recv() => match read {
                Some(Ok(bytes)) => bytes,
                _ => break,
            },
            _ = close.notified() => break,
            _ = tokio::time::sleep(idle) => break,
        };
        acting.retain(|task| !task.is_finished());
        reading.0.retain(|task| !task.is_finished());
        let Ok(message) = serde_json::from_slice::<Value>(&bytes) else {
            let _ = responses
                .send(failure(
                    &Value::Null,
                    "invalid_argument",
                    "a line that is not JSON",
                ))
                .await;
            continue;
        };
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        let method = message["method"].as_str().unwrap_or_default().to_owned();
        let params = message.get("params").cloned().unwrap_or(json!({}));
        if !greeted {
            if method != "hello" {
                let _ = responses
                    .send(failure(&id, "api_hello_first", "say hello first"))
                    .await;
                continue;
            }
            if params["version"].as_u64() != Some(VERSION) {
                let mut refused =
                    failure(&id, "api_version", "this masil-agent speaks API version 1");
                refused["error"]["supported"] = json!([VERSION]);
                let _ = responses.send(refused).await;
                break;
            }
            if let Err(error) = check_token(&session) {
                let _ = responses.send(failure(&id, "api_token", &error)).await;
                break;
            }
            greeted = true;
            let hello = json!({
                "version": VERSION,
                "scope": session.scope.name(),
                "methods": scope::methods(session.scope),
                "events": session.events,
            });
            let _ = responses
                .send(json!({"id": id, "ok": true, "value": hello}))
                .await;
            continue;
        }
        if !rate.take() {
            let _ = responses
                .send(failure(&id, "rate_limited", "too many calls a second"))
                .await;
            continue;
        }
        if let Err(error) = check_token(&session) {
            let _ = responses.send(failure(&id, "api_token", &error)).await;
            break;
        }
        match method.as_str() {
            "subscribe" => {
                let wanted: Vec<String> = params["events"]
                    .as_array()
                    .map(|events| {
                        events
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default();
                if wanted.is_empty() || !wanted.iter().all(|event| session.events.contains(event)) {
                    let _ = responses
                        .send(failure(
                            &id,
                            "api_forbidden",
                            "events this connection may not have",
                        ))
                        .await;
                    continue;
                }
                let Some(socket) = session.socket.clone() else {
                    let _ = responses
                        .send(failure(
                            &id,
                            "invalid_argument",
                            "no masil server for events",
                        ))
                        .await;
                    continue;
                };
                for old in subscription.0.drain(..) {
                    old.abort();
                }
                let path = socket.clone();
                let start =
                    tokio::task::spawn_blocking(move || crate::managed::inbox_since(&path, None))
                        .await
                        .ok()
                        .and_then(Result::ok)
                        .flatten()
                        .map_or(0, |(_, seq)| seq);
                let after = params["after"].as_i64().unwrap_or(start);
                subscription.0.push(match &session.feed {
                    Some(feed) => tokio::spawn(fed_events(
                        feed.subscribe(),
                        socket,
                        after,
                        events.clone(),
                        responses.clone(),
                    )),
                    None => tokio::spawn(inbox_events(
                        session.clone(),
                        socket,
                        after,
                        events.clone(),
                        responses.clone(),
                        close.clone(),
                    )),
                });
                let _ = responses
                    .send(json!({"id": id, "ok": true, "value": {"events": wanted, "seq": after}}))
                    .await;
            }
            "unsubscribe" => {
                for old in subscription.0.drain(..) {
                    old.abort();
                }
                let _ = responses
                    .send(json!({"id": id, "ok": true, "value": {}}))
                    .await;
            }
            verb => {
                let args: Vec<String> = match params.get("args") {
                    None => Vec::new(),
                    Some(Value::Array(args)) if args.iter().all(Value::is_string) => args
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect(),
                    Some(_) => {
                        let _ = responses
                            .send(failure(
                                &id,
                                "invalid_argument",
                                "args is a list of strings",
                            ))
                            .await;
                        continue;
                    }
                };
                if let Err(error) = scope::allowed(session.scope, verb, &args) {
                    let _ = responses.send(failure(&id, "api_forbidden", &error)).await;
                    continue;
                }
                let acts = scope::needed(verb, &args).is_some_and(|needed| needed > Scope::Read);
                let Ok(permit) = permits.clone().acquire_owned().await else {
                    break;
                };
                let (session, responses, verb, over) = (
                    session.clone(),
                    responses.clone(),
                    verb.to_owned(),
                    ended.0.clone(),
                );
                let task = tokio::spawn(async move {
                    // Waited for here, not in the loop that reads the
                    // connection; a call whose turn comes after its
                    // extension ended does not run.
                    let shared = match &session.shared {
                        Some(shared) => match shared.clone().try_acquire_owned() {
                            Ok(permit) => Some(permit),
                            Err(tokio::sync::TryAcquireError::Closed) => return,
                            // Its turn came after a wait: if the connection
                            // ended meanwhile, it does not run.
                            Err(tokio::sync::TryAcquireError::NoPermits) => {
                                let Ok(permit) = shared.clone().acquire_owned().await else {
                                    return;
                                };
                                if over.load(std::sync::atomic::Ordering::SeqCst) {
                                    return;
                                }
                                Some(permit)
                            }
                        },
                        None => None,
                    };
                    let reply = call(&session, &id, &verb, &args).await;
                    let _ = responses.send(reply).await;
                    drop((permit, shared));
                });
                if acts {
                    acting.push(task);
                } else {
                    reading.0.push(task);
                }
            }
        }
    }
    drop(subscription);
    // Reads stop with the connection; an act that started runs to its end
    // and leaves its record.
    drop(reading);
    drop(ended);
    for task in acting {
        let _ = task.await;
    }
    drop(responses);
    drop(events);
    if let Some(task) = writing.0.pop() {
        let _ = task.await;
    }
}

fn check_token(session: &Session) -> Result<(), String> {
    match &session.token {
        Some(held) => tokens::alive(held),
        None => Ok(()),
    }
}

/// The child's process group, ended if the call is dropped before the
/// child is reaped.
struct Group(Option<i32>);

impl Drop for Group {
    fn drop(&mut self) {
        if let Some(group) = self.0 {
            // SAFETY: the leader is not reaped, so the group is the call's.
            unsafe { libc::killpg(group, libc::SIGKILL) };
        }
    }
}

/// Reads all of `reader`, keeping at most `limit` bytes; the rest is read
/// and dropped so the writer never blocks. True if more came.
async fn drain(reader: &mut (impl AsyncRead + Unpin), limit: usize) -> (Vec<u8>, bool) {
    let mut kept = Vec::new();
    let mut over = false;
    let mut buffer = [0u8; 16 * 1024];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => return (kept, over),
            Ok(read) => {
                let room = limit.saturating_sub(kept.len());
                kept.extend_from_slice(&buffer[..read.min(room)]);
                over |= read > room;
            }
        }
    }
}

/// One verb, as the CLI runs it. The answer carries the exit code; a value
/// and an error may come together (an unknown outcome has both).
pub(crate) async fn call(session: &Session, id: &Value, verb: &str, args: &[String]) -> Value {
    let Ok(executable) = std::env::current_exe() else {
        return failure(id, "failed", "no executable");
    };
    let mut command = tokio::process::Command::new(executable);
    command.arg("agent");
    if let Some(socket) = &session.socket {
        command.arg("--socket").arg(socket);
    }
    command
        .arg(verb)
        .args(args)
        // The verbs that look at whom they serve (queue send) know.
        .env("MASIL_API_CALL", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let Ok(mut child) = command.spawn() else {
        return failure(id, "failed", "could not run the verb");
    };
    let pgid = child.id().and_then(|pid| i32::try_from(pid).ok());
    // A read stops with its caller. An act runs to its end and leaves its
    // record even when the connection, extension or coordinator goes first.
    let acts = scope::needed(verb, args).is_some_and(|needed| needed > Scope::Read);
    let mut group = Group(if acts { None } else { pgid });
    let (Some(mut stdout), Some(mut stderr)) = (child.stdout.take(), child.stderr.take()) else {
        return failure(id, "failed", "no pipes");
    };
    let limit = if verb == "wait" { WAIT_TIME } else { CALL_TIME };
    let output = async {
        let output = tokio::join!(drain(&mut stdout, MAX_OUT), drain(&mut stderr, STDERR));
        (output, child.wait().await)
    };
    let (((out, too_large), (err, _)), status) = match tokio::time::timeout(limit, output).await {
        Ok(output) => output,
        Err(_) => {
            // Armed now, an act's too, in case this call is dropped meanwhile.
            if let Some(pgid) = pgid {
                group.0 = Some(pgid);
                end_group(pgid, &mut child).await;
                group.0 = None;
            }
            return failure(id, "api_timeout", "the verb did not finish in time");
        }
    };
    group.0 = None;
    let exit = status.ok().and_then(|status| status.code()).unwrap_or(5);
    if too_large {
        let mut reply = failure(id, "response_too_large", "the answer exceeds 1 MiB");
        reply["exit"] = json!(exit);
        return reply;
    }
    let mut reply = json!({"id": id, "ok": exit == 0, "exit": exit});
    if !out.is_empty() {
        reply["value"] = serde_json::from_slice::<Value>(&out)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&out).into_owned()));
    }
    if exit != 0 {
        // `masil-agent: error[code]: message`, as the CLI prints it.
        let err = String::from_utf8_lossy(&err);
        let (code, message) = err
            .lines()
            .find_map(|line| {
                let rest = line.strip_prefix("masil-agent: error[")?;
                let (code, message) = rest.split_once("]: ")?;
                Some((code.to_owned(), message.to_owned()))
            })
            .unwrap_or_else(|| ("failed".to_owned(), err.trim().to_owned()));
        reply["error"] = json!({"code": code, "message": message});
    }
    reply
}

/// New inbox events after `after`. A full queue ends the subscription with
/// an `overflow` naming the last sequence sent; a revoked token closes the
/// connection.
async fn inbox_events(
    session: Arc<Session>,
    socket: PathBuf,
    mut after: i64,
    events: mpsc::Sender<Value>,
    responses: mpsc::Sender<Value>,
    close: Arc<Notify>,
) {
    let mut tick = tokio::time::interval(session.inbox_poll);
    loop {
        tick.tick().await;
        let path = socket.clone();
        let from = after;
        let read =
            tokio::task::spawn_blocking(move || crate::managed::inbox_since(&path, Some(from)))
                .await
                .unwrap_or_else(|error| Err(error.to_string()));
        let Ok(Some((items, next))) = read else {
            continue;
        };
        if items.is_empty() {
            after = next;
            continue;
        }
        if let Err(error) = check_token(&session) {
            let _ = responses
                .send(failure(&Value::Null, "api_token", &error))
                .await;
            close.notify_one();
            return;
        }
        if events
            .try_send(json!({"event": "inbox", "items": items, "seq": next}))
            .is_err()
        {
            let _ = responses
                .send(json!({"event": "overflow", "seq": after}))
                .await;
            return;
        }
        after = next;
    }
}

/// Inbox batches from a shared watcher, after `after`; a lagging reader
/// gets an `overflow` and the subscription ends.
async fn fed_events(
    mut feed: tokio::sync::broadcast::Receiver<Value>,
    socket: PathBuf,
    mut after: i64,
    events: mpsc::Sender<Value>,
    responses: mpsc::Sender<Value>,
) {
    // Events before the watcher's next batch are read here once; the
    // receiver already exists, so nothing falls between.
    loop {
        let (path, from) = (socket.clone(), after);
        let read =
            tokio::task::spawn_blocking(move || crate::managed::inbox_since(&path, Some(from)))
                .await
                .unwrap_or_else(|error| Err(error.to_string()));
        let Ok(Some((items, next))) = read else {
            break;
        };
        let full = items.len() >= 200;
        if !items.is_empty()
            && events
                .try_send(json!({"event": "inbox", "items": items, "seq": next}))
                .is_err()
        {
            let _ = responses
                .send(json!({"event": "overflow", "seq": after}))
                .await;
            return;
        }
        after = after.max(next);
        if !full {
            break;
        }
    }
    loop {
        let batch = match feed.recv().await {
            Ok(batch) => batch,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                let _ = responses
                    .send(json!({"event": "overflow", "seq": after}))
                    .await;
                return;
            }
            Err(_) => return,
        };
        let seq = batch["seq"].as_i64().unwrap_or(after);
        let items: Vec<Value> = batch["items"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|item| item["seq"].as_i64().is_some_and(|item| item > after))
            .cloned()
            .collect();
        if !items.is_empty()
            && events
                .try_send(json!({"event": "inbox", "items": items, "seq": seq}))
                .is_err()
        {
            let _ = responses
                .send(json!({"event": "overflow", "seq": after}))
                .await;
            return;
        }
        after = after.max(seq);
    }
}

fn usage() -> String {
    "usage: masil-agent api [--socket SOCKET] [--scope read|act|admin] [--token-file FILE]".into()
}

/// `masil-agent api`: the API on standard input and output; or, with
/// `SSH_ORIGINAL_COMMAND` (an SSH forced command given a command line), that
/// one verb under the same scope, printed.
pub(crate) fn run(args: &[String]) -> Result<i32, String> {
    let mut socket = None;
    let mut asked = Scope::Admin;
    let mut token_file = None;
    let mut index = 0;
    while index < args.len() {
        let value = args.get(index + 1).ok_or_else(usage)?;
        match args[index].as_str() {
            "--socket" => socket = Some(PathBuf::from(value)),
            "--scope" => asked = Scope::parse(value)?,
            "--token-file" => token_file = Some(PathBuf::from(value)),
            _ => return Err(usage()),
        }
        index += 2;
    }
    let socket = socket.or_else(current_socket);
    // The server's state directory, whatever this process's environment
    // (a forced command has sshd's), for this process and its calls.
    if let Some(socket) = &socket
        && let Ok(server) = crate::coordinator::server_info(socket)
    {
        // SAFETY: no other thread exists yet.
        unsafe { std::env::set_var("XDG_STATE_HOME", &server.state) };
    }
    let token = token_file.as_deref().map(tokens::load).transpose()?;
    // The narrower of what was asked and what the token allows.
    let scope = token.as_ref().map_or(asked, |held| held.scope.min(asked));
    let mut session = Session::new(socket, scope);
    session.token = token;
    session.idle = Some(Duration::from_secs(600));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    if let Some(line) = std::env::var("SSH_ORIGINAL_COMMAND")
        .ok()
        .filter(|line| !line.trim().is_empty())
    {
        let words: Vec<String> = line.split_whitespace().map(str::to_owned).collect();
        let (verb, rest) = words.split_first().ok_or_else(usage)?;
        scope::allowed(session.scope, verb, rest)
            .map_err(|error| format!("api_forbidden: {error}"))?;
        let reply = runtime.block_on(call(&session, &Value::Null, verb, rest));
        let shown = reply
            .get("value")
            .cloned()
            .unwrap_or_else(|| reply["error"].clone());
        println!(
            "{}",
            serde_json::to_string_pretty(&shown).map_err(|error| error.to_string())?
        );
        return Ok(reply["exit"].as_i64().unwrap_or(5) as i32);
    }
    runtime.block_on(serve(tokio::io::stdin(), tokio::io::stdout(), session));
    // Standard input is read by a thread a runtime cannot cancel.
    std::process::exit(0)
}
