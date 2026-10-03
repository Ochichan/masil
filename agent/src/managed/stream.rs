//! `agent rpc --stream --no-start` (P8b, docs/agent-endpoints.md): one
//! long-lived connection another server's coordinator holds to this one. It
//! answers list, get and read, and sends this server's inbox as it changes.
//! It starts nothing: no server, no coordinator, no store.

use super::Manager;
use super::endpoints::{self, Action, Request};
use super::operations::Store;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

pub(crate) const PROTOCOL: [u64; 2] = [1, 1];
/// A frame read; longer ends the connection.
const MAX_FRAME_IN: usize = 256 * 1024;
/// A response larger than this is answered `response_too_large`.
const MAX_FRAME_OUT: usize = 1024 * 1024;
/// How often the inbox is looked at.
const POLL: Duration = Duration::from_secs(5);
/// How often the server, its boot and its coordinator are looked at.
const STATUS: Duration = Duration::from_secs(30);
/// The open set is sent when it changes, and at least this often.
const OPEN_EVERY: Duration = Duration::from_secs(300);
const BATCH: i64 = 200;
const OPEN_LIMIT: i64 = 4096;

fn now_ms() -> u64 {
    crate::observation::now_ms()
}

/// What this server is, as the hello and status events say.
async fn status(socket: &Path, manager: Option<&Manager>) -> Value {
    let boot = match manager {
        Some(manager) => manager.boot().await.ok(),
        None => None,
    };
    let socket = socket.to_owned();
    let (store, inbox, coordinator) = tokio::task::spawn_blocking(move || {
        let probe = Store::inbox_probe(&socket);
        let coordinator = crate::coordinator::status(&socket)
            .ok()
            .and_then(|status| status["state"].as_str().map(str::to_owned))
            .unwrap_or_else(|| "not_running".into());
        (
            probe.as_ref().map(|(_, instance)| instance.clone()),
            probe.is_some_and(|(enabled, _)| enabled),
            coordinator,
        )
    })
    .await
    .unwrap_or((None, false, "not_running".into()));
    json!({
        "server": if boot.is_some() { "running" } else { "not_running" },
        "boot": boot,
        "store": store,
        "inbox": inbox,
        "coordinator": coordinator,
    })
}

/// The frames of `reader`, one per line, read by a task of their own: a
/// `select!` that drops a receive loses no bytes. The channel ends at EOF
/// (a last line without its newline is not a frame), or after an error.
pub(crate) fn frames<R>(reader: R, max: usize) -> mpsc::Receiver<Result<Vec<u8>, String>>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let (sender, receiver) = mpsc::channel(16);
    tokio::spawn(async move {
        let mut reader = BufReader::new(reader);
        loop {
            let mut line = Vec::new();
            let read = (&mut reader)
                .take(max as u64 + 1)
                .read_until(b'\n', &mut line)
                .await;
            let frame = match read {
                Ok(0) => return,
                Ok(_) if line.last() == Some(&b'\n') => {
                    line.pop();
                    Ok(line)
                }
                Ok(_) if line.len() > max => Err("a frame over the limit".to_owned()),
                Ok(_) => return,
                Err(error) => Err(error.to_string()),
            };
            let ended = frame.is_err();
            if sender.send(frame).await.is_err() || ended {
                return;
            }
        }
    });
    receiver
}

struct Subscription {
    instance: Option<i64>,
    cursor: Option<i64>,
    open: Option<Vec<i64>>,
    open_sent: tokio::time::Instant,
    /// Agent names by run, listed when an event names a run not seen yet.
    names: std::collections::HashMap<String, String>,
}

/// Serves the stream on standard input and output until EOF, a framing
/// error, or a replaced executable. Every path ends with status 0.
pub(crate) fn serve(socket: &str) -> Result<i32, String> {
    let socket = PathBuf::from(socket);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    runtime.block_on(run(socket));
    // Standard input is read by a blocking thread a runtime cannot cancel:
    // end here rather than wait for the next byte.
    std::process::exit(0)
}

async fn run(socket: PathBuf) {
    let exe = crate::coordinator::exe_identity();
    let mut incoming = frames(tokio::io::stdin(), MAX_FRAME_IN);
    let mut stdout = tokio::io::stdout();
    let mut manager = Manager::new(socket.clone(), None).ok();
    let mut last_status = status(&socket, manager.as_ref()).await;
    let hello = json!({
        "version": 1,
        "hello": {
            "protocol": PROTOCOL,
            "now_ms": now_ms(),
            "status": last_status,
        }
    });
    if send(&mut stdout, &hello).await.is_err() {
        return;
    }
    let mut subscription: Option<Subscription> = None;
    let mut poll = tokio::time::interval(POLL);
    let mut check = tokio::time::interval(STATUS);
    // A server not running is looked for this often.
    let mut retry = tokio::time::interval(POLL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    check.tick().await;
    retry.tick().await;
    loop {
        tokio::select! {
            read = incoming.recv() => {
                let Some(Ok(bytes)) = read else { return };
                let Ok(message) = serde_json::from_slice::<Value>(&bytes) else { return };
                let Some(id) = message.get("id").and_then(Value::as_u64) else { return };
                let reply = if message.get("ping") == Some(&json!(true)) {
                    json!({"id": id, "pong": true})
                } else if let Some(wanted) = message.get("subscribe") {
                    let current = Store::inbox_probe(&socket)
                        .and_then(|(_, instance)| instance.parse::<i64>().ok());
                    let instance = wanted["instance"].as_i64();
                    // Another store than the caller knows: start over.
                    let cursor = wanted["after"].as_i64().filter(|_| instance.is_some() && instance == current);
                    subscription = Some(Subscription {
                        instance: current,
                        cursor,
                        open: None,
                        open_sent: tokio::time::Instant::now(),
                        names: std::collections::HashMap::new(),
                    });
                    poll.reset_immediately();
                    json!({"id": id, "ok": true, "value": {"instance": current, "reset": cursor.is_none()}})
                } else if let Some(request) = message.get("request") {
                    if manager.is_none() {
                        manager = Manager::new(socket.clone(), None).ok();
                    }
                    answer(id, request, manager.as_ref()).await
                } else {
                    return;
                };
                if send(&mut stdout, &reply).await.is_err() {
                    return;
                }
            }
            _ = poll.tick(), if subscription.is_some() => {
                let Some(subscribed) = subscription.as_mut() else { continue };
                for event in inbox_events(&socket, subscribed, manager.as_ref()).await {
                    if send(&mut stdout, &event).await.is_err() {
                        return;
                    }
                }
            }
            _ = retry.tick(), if manager.is_none() => {
                manager = Manager::new(socket.clone(), None).ok();
                if manager.is_some() {
                    last_status = status(&socket, manager.as_ref()).await;
                    // A socket file left by a server that died: look again.
                    if last_status["server"] != "running" {
                        manager = None;
                        continue;
                    }
                    let event = json!({"event": "status", "now_ms": now_ms(), "status": last_status});
                    if send(&mut stdout, &event).await.is_err() {
                        return;
                    }
                }
            }
            _ = check.tick() => {
                if !crate::coordinator::exe_unchanged(&exe) {
                    return;
                }
                if manager.is_none() {
                    manager = Manager::new(socket.clone(), None).ok();
                }
                let now = status(&socket, manager.as_ref()).await;
                if now["server"] != "running" {
                    // A server that went away is looked for again.
                    manager = None;
                }
                if now != last_status {
                    last_status = now.clone();
                    let event = json!({"event": "status", "now_ms": now_ms(), "status": now});
                    if send(&mut stdout, &event).await.is_err() {
                        return;
                    }
                }
            }
        }
    }
}

async fn send(stdout: &mut tokio::io::Stdout, value: &Value) -> Result<(), String> {
    let mut bytes = serde_json::to_vec(value).map_err(|error| error.to_string())?;
    bytes.push(b'\n');
    stdout
        .write_all(&bytes)
        .await
        .map_err(|error| error.to_string())?;
    stdout.flush().await.map_err(|error| error.to_string())
}

/// List, get and read; anything else goes by its own call.
async fn answer(id: u64, request: &Value, manager: Option<&Manager>) -> Value {
    let failure = |error: String| json!({"id": id, "ok": false, "error": error});
    let Some(manager) = manager else {
        return failure("server_unreachable: the endpoint's masil server is not running".into());
    };
    let Ok(bytes) = serde_json::to_vec(request) else {
        return failure("invalid_argument: request is not JSON".into());
    };
    let request = match endpoints::decode_request(&bytes) {
        Ok(request) => request,
        Err(error) => return failure(error),
    };
    let streamed = matches!(
        request,
        Request::List
            | Request::Get { .. }
            | Request::Action {
                action: Action::Read { .. },
                ..
            }
    );
    if !streamed {
        return failure("remote_unsupported: only list, get and read go over the stream".into());
    }
    match endpoints::rpc(manager, request).await {
        Ok(value) => {
            let reply = json!({"id": id, "ok": true, "value": value});
            if serde_json::to_vec(&reply).map_or(true, |bytes| bytes.len() > MAX_FRAME_OUT) {
                return failure("response_too_large: the answer exceeds 1 MiB".into());
            }
            reply
        }
        Err(error) => failure(error),
    }
}

/// The inbox events to send now: new rows, and the open set when it
/// changed or has not been sent for a while.
async fn inbox_events(
    socket: &Path,
    subscribed: &mut Subscription,
    manager: Option<&Manager>,
) -> Vec<Value> {
    let after = subscribed.cursor;
    // Read here: a read-only snapshot of milliseconds, and this process
    // serves one connection; a blocking thread would cost wakeups.
    let read = Store::stream_inbox(socket, after, BATCH, OPEN_LIMIT);
    let Ok(Some(inbox)) = read else {
        return Vec::new();
    };
    let mut events = Vec::new();
    if subscribed.instance != Some(inbox.instance) {
        // The store was replaced: the caller starts over from its open rows.
        subscribed.instance = Some(inbox.instance);
        subscribed.cursor = None;
        subscribed.open = None;
        events.push(json!({"event": "instance", "instance": inbox.instance}));
        return events;
    }
    if !inbox.enabled {
        if subscribed.open.as_ref().is_none_or(|open| !open.is_empty()) {
            subscribed.open = Some(Vec::new());
            events.push(json!({"event": "inbox_off", "instance": inbox.instance}));
        }
        subscribed.cursor = Some(inbox.cursor);
        return events;
    }
    let seed = after.is_none();
    let mut items = inbox.items;
    // Names for the other server's messages; listed only for a new run.
    let unknown = items
        .iter()
        .filter_map(|item| item["run"].as_str())
        .any(|run| !run.is_empty() && !subscribed.names.contains_key(run));
    if unknown
        && let Some(manager) = manager
        && let Ok(agents) = manager.list().await
    {
        for agent in agents {
            subscribed.names.insert(agent.run, agent.name);
        }
    }
    for item in &mut items {
        if let Some(name) = item["run"]
            .as_str()
            .and_then(|run| subscribed.names.get(run))
        {
            item["agent"] = json!(name);
        }
    }
    if seed || !items.is_empty() {
        events.push(json!({
            "event": "inbox",
            "instance": inbox.instance,
            "seed": seed,
            "items": items,
            "cursor": inbox.cursor,
            "now_ms": now_ms(),
        }));
    }
    subscribed.cursor = Some(inbox.cursor);
    let changed = subscribed.open.as_ref() != Some(&inbox.open);
    if changed || subscribed.open_sent.elapsed() >= OPEN_EVERY {
        subscribed.open = Some(inbox.open.clone());
        subscribed.open_sent = tokio::time::Instant::now();
        events.push(json!({
            "event": "open",
            "instance": inbox.instance,
            "as_of": inbox.as_of,
            "ids": inbox.open,
            "truncated": inbox.truncated,
        }));
    }
    events
}
