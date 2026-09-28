//! Foreground, read-only observation daemon.
//!
//! This module owns only bounded in-memory projections. It never starts a core
//! or provider process and it does not read terminal screen contents.

use crate::observation::{self, Config, NativeSession, SourceState};
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener as StdUnixListener, UnixStream as StdUnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Semaphore, watch};
use tokio::time::timeout;

const REQUEST_FRAME: usize = 8 * 1024;
const RESPONSE_FRAME: usize = 64 * 1024;
const CLIENT_LIMIT: usize = 32;
const DEADLINE: Duration = Duration::from_secs(3);

#[derive(Clone)]
struct CorePane {
    process: &'static str,
    pty_generation: Option<String>,
    invalidated: bool,
}

#[derive(Clone)]
struct CoreState {
    freshness: &'static str,
    reason: Option<&'static str>,
    core_boot_id: Option<String>,
    stream_epoch: Option<String>,
    last_event_seq: Option<String>,
    observed_at_ms: u64,
    panes: HashMap<String, CorePane>,
}

impl CoreState {
    fn initial(config: &Config) -> Self {
        let panes = config
            .sources
            .iter()
            .flat_map(|source| &source.sessions)
            .map(|session| {
                (
                    session.pane_id.clone(),
                    CorePane {
                        process: "unknown",
                        pty_generation: None,
                        invalidated: false,
                    },
                )
            })
            .collect();
        Self {
            freshness: "connecting",
            reason: None,
            core_boot_id: None,
            stream_epoch: None,
            last_event_seq: None,
            observed_at_ms: 0,
            panes,
        }
    }

    fn mark_stale(&mut self, freshness: &'static str, reason: &'static str) {
        self.freshness = freshness;
        self.reason = Some(reason);
        self.observed_at_ms = observation::now_ms();
    }

    fn invalidate(&mut self, freshness: &'static str, reason: &'static str) {
        self.mark_stale(freshness, reason);
        for pane in self.panes.values_mut() {
            pane.invalidated = true;
        }
    }
}

struct DaemonState {
    config: Config,
    sources: Vec<watch::Receiver<SourceState>>,
    core: watch::Receiver<CoreState>,
}

struct SocketGuard {
    path: PathBuf,
    dev: u64,
    ino: u64,
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let Ok(metadata) = fs::symlink_metadata(&self.path) else {
            return;
        };
        if metadata.file_type().is_socket()
            && metadata.dev() == self.dev
            && metadata.ino() == self.ino
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

struct CoreLock(File);

impl Drop for CoreLock {
    fn drop(&mut self) {
        // The persistent lock inode is intentionally retained. Removing it
        // would let a waiter and a new opener lock different inodes.
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

struct Strict(Value);

impl<'de> Deserialize<'de> for Strict {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct StrictVisitor;
        impl<'de> Visitor<'de> for StrictVisitor {
            type Value = Strict;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("bounded JSON without duplicate object keys")
            }
            fn visit_bool<E: de::Error>(self, value: bool) -> Result<Strict, E> {
                Ok(Strict(value.into()))
            }
            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Strict, E> {
                Ok(Strict(value.into()))
            }
            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Strict, E> {
                Ok(Strict(value.into()))
            }
            fn visit_f64<E: de::Error>(self, value: f64) -> Result<Strict, E> {
                serde_json::Number::from_f64(value)
                    .map(|number| Strict(Value::Number(number)))
                    .ok_or_else(|| E::custom("non-finite number"))
            }
            fn visit_str<E: de::Error>(self, value: &str) -> Result<Strict, E> {
                Ok(Strict(value.into()))
            }
            fn visit_none<E: de::Error>(self) -> Result<Strict, E> {
                Ok(Strict(Value::Null))
            }
            fn visit_unit<E: de::Error>(self) -> Result<Strict, E> {
                Ok(Strict(Value::Null))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut values: A) -> Result<Strict, A::Error> {
                let mut result = Vec::new();
                while let Some(Strict(value)) = values.next_element()? {
                    if result.len() == 64 {
                        return Err(de::Error::custom("array limit"));
                    }
                    result.push(value);
                }
                Ok(Strict(Value::Array(result)))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut values: A) -> Result<Strict, A::Error> {
                let mut result = Map::new();
                while let Some(key) = values.next_key::<String>()? {
                    if result.contains_key(&key) {
                        return Err(de::Error::custom("duplicate key"));
                    }
                    if result.len() == 32 {
                        return Err(de::Error::custom("object limit"));
                    }
                    let Strict(value) = values.next_value()?;
                    result.insert(key, value);
                }
                Ok(Strict(Value::Object(result)))
            }
        }
        deserializer.deserialize_any(StrictVisitor)
    }
}

fn bounded(value: &Value, depth: usize) -> bool {
    if depth > 8 {
        return false;
    }
    match value {
        Value::Array(values) => values.iter().all(|value| bounded(value, depth + 1)),
        Value::Object(values) => values.values().all(|value| bounded(value, depth + 1)),
        _ => true,
    }
}

fn private_parent(path: &Path) -> Result<(), String> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or("manager socket requires an absolute parent directory")?;
    let metadata = fs::metadata(parent).map_err(|_| "manager socket parent is unavailable")?;
    if !path.is_absolute()
        || !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err("manager socket must be in a private owner directory".into());
    }
    Ok(())
}

fn lock_core(core: &Path) -> Result<(CoreLock, PathBuf), String> {
    let canonical = fs::canonicalize(core).map_err(|_| "core bridge is unavailable")?;
    let metadata = fs::metadata(&canonical).map_err(|_| "core bridge is unavailable")?;
    if !metadata.file_type().is_socket() {
        return Err("core bridge is not a Unix socket".into());
    }
    let parent = canonical.parent().ok_or("core bridge path has no parent")?;
    let parent_metadata = fs::metadata(parent).map_err(|_| "core bridge parent is unavailable")?;
    if !parent_metadata.is_dir()
        || parent_metadata.uid() != unsafe { libc::geteuid() }
        || parent_metadata.mode() & 0o077 != 0
    {
        return Err("core bridge must be in a private owner directory".into());
    }
    let file_name = canonical
        .file_name()
        .ok_or("core bridge path has no file name")?;
    let mut lock_name = OsString::from(file_name);
    lock_name.push(".agentd.lock");
    let lock_path = canonical.with_file_name(lock_name);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&lock_path)
        .map_err(|_| "cannot open the core agentd lock")?;
    let lock_metadata = file
        .metadata()
        .map_err(|_| "cannot inspect the core agentd lock")?;
    if !lock_metadata.is_file()
        || lock_metadata.uid() != unsafe { libc::geteuid() }
        || lock_metadata.mode() & 0o077 != 0
    {
        return Err("core agentd lock is not a private owner file".into());
    }
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result != 0 {
        return Err(
            if io::Error::last_os_error().kind() == io::ErrorKind::WouldBlock {
                "an agentd already observes this core".into()
            } else {
                "cannot lock the core agentd lock".into()
            },
        );
    }
    Ok((CoreLock(file), canonical))
}

fn bind_manager(path: &Path) -> Result<(StdUnixListener, SocketGuard), String> {
    private_parent(path)?;
    match fs::symlink_metadata(path) {
        Ok(_) => return Err("refusing existing manager socket path".into()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(_) => return Err("cannot inspect manager socket path".into()),
    }
    let listener = StdUnixListener::bind(path).map_err(|_| "cannot bind manager socket")?;
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(_) => return Err("cannot inspect bound manager socket".into()),
    };
    let guard = SocketGuard {
        path: path.to_owned(),
        dev: metadata.dev(),
        ino: metadata.ino(),
    };
    if !metadata.file_type().is_socket() {
        return Err("bound manager path is not a Unix socket".into());
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|_| "cannot make manager socket private")?;
    listener
        .set_nonblocking(true)
        .map_err(|_| "cannot make manager socket nonblocking")?;
    Ok((listener, guard))
}

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd"
))]
fn peer_is_owner(stream: &StdUnixStream) -> bool {
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    unsafe {
        libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) == 0 && uid == libc::geteuid()
    }
}

#[cfg(target_os = "linux")]
fn peer_is_owner(stream: &StdUnixStream) -> bool {
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        ) == 0
            && credentials.uid == libc::geteuid()
    }
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "linux"
)))]
fn peer_is_owner(_stream: &StdUnixStream) -> bool {
    false
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    v: u64,
    kind: String,
    request_id: String,
    #[serde(default)]
    id: Option<String>,
}

fn parse_request(body: &[u8]) -> Result<Request, (String, String)> {
    let Strict(value) =
        serde_json::from_slice(body).map_err(|_| (String::new(), "invalid JSON request".into()))?;
    let request_id = value
        .get("request_id")
        .and_then(Value::as_str)
        .filter(|value| {
            !value.is_empty() && value.len() <= 128 && !value.contains(['\0', '\r', '\n'])
        })
        .unwrap_or_default()
        .to_owned();
    if !bounded(&value, 1) {
        return Err((request_id, "request nesting limit exceeded".into()));
    }
    let request: Request = serde_json::from_value(value)
        .map_err(|_| (request_id.clone(), "invalid request schema".into()))?;
    if request.v != 1
        || request.request_id.is_empty()
        || request.request_id.len() > 128
        || request.request_id.contains(['\0', '\r', '\n'])
    {
        return Err((request_id, "invalid request envelope".into()));
    }
    match request.kind.as_str() {
        "inspect" if request.id.as_ref().is_some_and(|id| valid_label(id)) => {}
        "status" | "agents" | "stop" if request.id.is_none() => {}
        _ => return Err((request_id, "invalid management request".into())),
    }
    Ok(request)
}

fn valid_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

async fn read_frame(stream: &mut UnixStream) -> Result<Vec<u8>, String> {
    let mut header = [0; 4];
    stream
        .read_exact(&mut header)
        .await
        .map_err(|_| "incomplete request frame")?;
    let length = u32::from_be_bytes(header) as usize;
    if length == 0 || length > REQUEST_FRAME {
        return Err("request exceeds frame limit".into());
    }
    let mut body = vec![0; length];
    stream
        .read_exact(&mut body)
        .await
        .map_err(|_| "incomplete request frame")?;
    Ok(body)
}

async fn write_frame(stream: &mut UnixStream, value: &Value) -> Result<(), String> {
    let body = serde_json::to_vec(value).map_err(|_| "cannot encode response")?;
    if body.is_empty() || body.len() > RESPONSE_FRAME {
        return Err("response exceeds frame limit".into());
    }
    stream
        .write_all(&(body.len() as u32).to_be_bytes())
        .await
        .map_err(|_| "cannot write response")?;
    stream
        .write_all(&body)
        .await
        .map_err(|_| "cannot write response".to_string())
}

fn error_response(request_id: &str, code: &str, message: &str) -> Value {
    json!({"v":1,"kind":"error","request_id":request_id,"code":code,"message":message})
}

fn core_json(core: &CoreState) -> Value {
    json!({
        "freshness": core.freshness,
        "reason": core.reason,
        "core_boot_id": core.core_boot_id,
        "stream_epoch": core.stream_epoch,
        "last_event_seq": core.last_event_seq,
        "observed_at_ms": core.observed_at_ms,
    })
}

fn source_snapshots(state: &DaemonState) -> HashMap<String, SourceState> {
    state
        .sources
        .iter()
        .map(|receiver| {
            let source = receiver.borrow().clone();
            (source.source_id.clone(), source)
        })
        .collect()
}

fn short_observation(
    source_id: &str,
    pane_id: &str,
    native: &NativeSession,
    source: &SourceState,
    core: &CoreState,
) -> Value {
    let pane = core.panes.get(pane_id);
    let binding = if pane.is_some_and(|pane| pane.invalidated) {
        "invalidated"
    } else {
        "explicit_unverified"
    };
    json!({
        "id": native.id,
        "source_id": source_id,
        "session_id": native.session_id,
        "pane_id": pane_id,
        "native": {
            "exists": native.exists,
            "activity": native.activity,
            "last_activity": native.last_activity,
            "attention": native.attention,
            "permission_count": native.permission_count,
            "question_count": native.question_count,
            "freshness": source.freshness,
            "observed_at_ms": native.observed_at_ms,
        },
        "core": {
            "process": pane.map_or("unknown", |pane| pane.process),
            "pty_generation": pane.and_then(|pane| pane.pty_generation.as_deref()),
            "freshness": core.freshness,
        },
        "binding": binding,
        "frontend_verified": false,
        "capabilities": {
            "read": true,
            "input": false,
            "approval": false,
            "completion": false,
            "child_aggregation": false,
        },
    })
}

fn full_observation(state: &DaemonState, id: &str) -> Option<Value> {
    let core = state.core.borrow().clone();
    let sources = source_snapshots(state);
    for source_config in &state.config.sources {
        let source = sources.get(&source_config.id)?;
        for session_config in &source_config.sessions {
            if session_config.id != id {
                continue;
            }
            let native = source
                .sessions
                .iter()
                .find(|native| native.id == session_config.id)?;
            let mut row = short_observation(
                &source_config.id,
                &session_config.pane_id,
                native,
                source,
                &core,
            );
            row["native"] = json!({
                "id": native.id,
                "session_id": native.session_id,
                "parent_session_id": native.parent_session_id,
                "exists": native.exists,
                "activity": native.activity,
                "last_activity": native.last_activity,
                "attention": native.attention,
                "permission_ids": native.permission_ids,
                "question_ids": native.question_ids,
                "permission_count": native.permission_count,
                "question_count": native.question_count,
                "request_ids_truncated": native.request_ids_truncated,
                "observed_at_ms": native.observed_at_ms,
                "freshness": source.freshness,
                "reason": source.reason,
                "source_epoch": source.epoch,
                "provider_version": source.provider_version,
                "native_events": source.native_events,
                "reconciliations": source.reconciliations,
            });
            return Some(row);
        }
    }
    None
}

fn response(state: &DaemonState, request: &Request) -> Value {
    match request.kind.as_str() {
        "status" => {
            let core = state.core.borrow().clone();
            json!({
                "v": 1,
                "kind": "status",
                "request_id": request.request_id,
                "status": "running",
                "pid": std::process::id(),
                "core": core_json(&core),
                "sources": state.config.sources.len(),
                "observations": state.config.sources.iter().map(|source| source.sessions.len()).sum::<usize>(),
            })
        }
        "agents" => {
            let core = state.core.borrow().clone();
            let sources = source_snapshots(state);
            let mut observations = Vec::new();
            for source_config in &state.config.sources {
                let Some(source) = sources.get(&source_config.id) else {
                    continue;
                };
                for session_config in &source_config.sessions {
                    let Some(native) = source
                        .sessions
                        .iter()
                        .find(|native| native.id == session_config.id)
                    else {
                        continue;
                    };
                    observations.push(short_observation(
                        &source_config.id,
                        &session_config.pane_id,
                        native,
                        source,
                        &core,
                    ));
                }
            }
            json!({"v":1,"kind":"agents","request_id":request.request_id,"observations":observations})
        }
        "inspect" => match full_observation(state, request.id.as_deref().unwrap_or_default()) {
            Some(observation) => json!({
                "v":1,"kind":"inspect","request_id":request.request_id,"observation":observation
            }),
            None => error_response(&request.request_id, "not_found", "observation not found"),
        },
        "stop" => json!({"v":1,"kind":"stop","request_id":request.request_id,"accepted":true}),
        _ => unreachable!("request parser validates the kind"),
    }
}

async fn handle_client(mut stream: UnixStream, state: Arc<DaemonState>, stop: watch::Sender<bool>) {
    let body = match timeout(DEADLINE, read_frame(&mut stream)).await {
        Ok(Ok(body)) => body,
        _ => return,
    };
    let request = match parse_request(&body) {
        Ok(request) => request,
        Err((request_id, message)) => {
            let value = error_response(&request_id, "invalid_request", &message);
            let _ = timeout(DEADLINE, write_frame(&mut stream, &value)).await;
            return;
        }
    };
    let should_stop = request.kind == "stop";
    let value = response(&state, &request);
    if matches!(
        timeout(DEADLINE, write_frame(&mut stream, &value)).await,
        Ok(Ok(()))
    ) && should_stop
    {
        let _ = stop.send(true);
    }
}

fn exact_object<'a>(
    value: &'a Value,
    fields: &[&str],
    description: &str,
) -> Result<&'a Map<String, Value>, &'static str> {
    let Some(object) = value.as_object() else {
        return Err("invalid core protocol");
    };
    if object.len() != fields.len() || fields.iter().any(|field| !object.contains_key(*field)) {
        let _ = description;
        return Err("invalid core protocol");
    }
    Ok(object)
}

fn decimal(value: Option<&Value>) -> Result<String, &'static str> {
    let Some(text) = value.and_then(Value::as_str) else {
        return Err("invalid core protocol");
    };
    if text.is_empty()
        || text.len() > 20
        || !text.bytes().all(|byte| byte.is_ascii_digit())
        || text.parse::<u64>().is_err()
    {
        return Err("invalid core protocol");
    }
    Ok(text.to_owned())
}

fn core_value(body: &[u8]) -> Result<Value, &'static str> {
    let Strict(value) = serde_json::from_slice(body).map_err(|_| "invalid core protocol")?;
    if !bounded(&value, 1) {
        return Err("invalid core protocol");
    }
    Ok(value)
}

async fn core_write(stream: &mut UnixStream, value: &Value) -> Result<(), &'static str> {
    let body = serde_json::to_vec(value).map_err(|_| "invalid core protocol")?;
    if body.is_empty() || body.len() > REQUEST_FRAME {
        return Err("invalid core protocol");
    }
    timeout(DEADLINE, async {
        stream.write_all(&(body.len() as u32).to_be_bytes()).await?;
        stream.write_all(&body).await
    })
    .await
    .map_err(|_| "core deadline")?
    .map_err(|_| "core unavailable")
}

async fn core_read(stream: &mut UnixStream) -> Result<Value, &'static str> {
    timeout(DEADLINE, async {
        let mut header = [0; 4];
        stream.read_exact(&mut header).await?;
        let length = u32::from_be_bytes(header) as usize;
        if length == 0 || length > RESPONSE_FRAME {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "frame"));
        }
        let mut body = vec![0; length];
        stream.read_exact(&mut body).await?;
        Ok::<_, io::Error>(body)
    })
    .await
    .map_err(|_| "core deadline")?
    .map_err(|_| "core unavailable")
    .and_then(|body| core_value(&body))
}

async fn core_read_stream(stream: &mut UnixStream) -> Result<Value, &'static str> {
    let mut header = [0; 4];
    stream
        .read_exact(&mut header[..1])
        .await
        .map_err(|_| "core stream ended")?;
    let body = timeout(DEADLINE, async {
        stream
            .read_exact(&mut header[1..])
            .await
            .map_err(|_| "core stream ended")?;
        let length = u32::from_be_bytes(header) as usize;
        if length == 0 || length > RESPONSE_FRAME {
            return Err("invalid core protocol");
        }
        let mut body = vec![0; length];
        stream
            .read_exact(&mut body)
            .await
            .map_err(|_| "core stream ended")?;
        Ok(body)
    })
    .await
    .map_err(|_| "core frame deadline")??;
    core_value(&body)
}

fn validate_hello(value: &Value) -> Result<String, &'static str> {
    let hello = exact_object(
        value,
        &[
            "v",
            "kind",
            "request_id",
            "core_boot_id",
            "negotiated_version",
            "capabilities",
            "limits",
        ],
        "hello",
    )?;
    if hello.get("v").and_then(Value::as_u64) != Some(1)
        || hello.get("kind").and_then(Value::as_str) != Some("hello")
        || hello.get("request_id").and_then(Value::as_str) != Some("agentd-hello")
        || hello
            .get("negotiated_version")
            .and_then(|value| value.get("major"))
            .and_then(Value::as_u64)
            != Some(1)
        || hello
            .get("capabilities")
            .and_then(|value| value.get("watch"))
            .and_then(Value::as_bool)
            != Some(true)
    {
        return Err("invalid core protocol");
    }
    let boot_id = hello
        .get("core_boot_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 128)
        .ok_or("invalid core protocol")?;
    Ok(boot_id.to_owned())
}

fn validate_ack(
    value: &Value,
    boot_id: &str,
    scope: &HashSet<String>,
    state: &mut CoreState,
) -> Result<(String, String), &'static str> {
    let ack = exact_object(
        value,
        &[
            "v",
            "kind",
            "request_id",
            "core_boot_id",
            "stream_epoch",
            "fence_seq",
            "scope_revision",
            "complete",
            "panes",
        ],
        "watch acknowledgement",
    )?;
    if ack.get("v").and_then(Value::as_u64) != Some(1)
        || ack.get("kind").and_then(Value::as_str) != Some("watch")
        || ack.get("request_id").and_then(Value::as_str) != Some("agentd-watch")
        || ack.get("core_boot_id").and_then(Value::as_str) != Some(boot_id)
        || ack.get("complete").and_then(Value::as_bool) != Some(true)
    {
        return Err("invalid core protocol");
    }
    let epoch = decimal(ack.get("stream_epoch"))?;
    let sequence = decimal(ack.get("fence_seq"))?;
    decimal(ack.get("scope_revision"))?;
    let panes = ack
        .get("panes")
        .and_then(Value::as_array)
        .filter(|panes| panes.len() == scope.len())
        .ok_or("invalid core protocol")?;
    let mut seen = HashSet::new();
    for value in panes {
        let pane = exact_object(
            value,
            &[
                "pane_id",
                "pty_generation",
                "screen_generation",
                "width",
                "height",
                "dead",
            ],
            "watch pane",
        )?;
        let pane_id = pane
            .get("pane_id")
            .and_then(Value::as_str)
            .filter(|pane_id| scope.contains(*pane_id))
            .ok_or("invalid core protocol")?;
        if !seen.insert(pane_id.to_owned())
            || pane.get("width").and_then(Value::as_u64).is_none()
            || pane.get("height").and_then(Value::as_u64).is_none()
        {
            return Err("invalid core protocol");
        }
        let generation = decimal(pane.get("pty_generation"))?;
        decimal(pane.get("screen_generation"))?;
        let dead = pane
            .get("dead")
            .and_then(Value::as_bool)
            .ok_or("invalid core protocol")?;
        let target = state
            .panes
            .get_mut(pane_id)
            .ok_or("invalid core protocol")?;
        target.pty_generation = Some(generation);
        target.process = if dead { "exited" } else { "running" };
        target.invalidated = dead;
    }
    Ok((epoch, sequence))
}

fn validate_watch_error(value: &Value) -> bool {
    let Some(error) = value.as_object() else {
        return false;
    };
    error.len() == 5
        && ["v", "kind", "request_id", "code", "message"]
            .iter()
            .all(|field| error.contains_key(*field))
        && error.get("v").and_then(Value::as_u64) == Some(1)
        && error.get("kind").and_then(Value::as_str) == Some("error")
        && error.get("request_id").and_then(Value::as_str) == Some("agentd-watch")
        && error.get("code").and_then(Value::as_str).is_some()
        && error.get("message").and_then(Value::as_str).is_some()
}

fn apply_event(
    value: &Value,
    boot_id: &str,
    epoch: &str,
    last_sequence: u64,
    state: &mut CoreState,
) -> Result<u64, &'static str> {
    let event = exact_object(
        value,
        &[
            "v",
            "kind",
            "core_boot_id",
            "stream_epoch",
            "event_seq",
            "pane_id",
            "pty_generation",
            "screen_generation",
            "reason",
        ],
        "watch event",
    )?;
    if event.get("v").and_then(Value::as_u64) != Some(1)
        || event.get("kind").and_then(Value::as_str) != Some("event")
        || event.get("core_boot_id").and_then(Value::as_str) != Some(boot_id)
        || event.get("stream_epoch").and_then(Value::as_str) != Some(epoch)
    {
        return Err("invalid core protocol");
    }
    let sequence_text = decimal(event.get("event_seq"))?;
    let sequence = sequence_text
        .parse::<u64>()
        .map_err(|_| "invalid core protocol")?;
    if sequence <= last_sequence {
        return Err("invalid core protocol");
    }
    let pane_id = event
        .get("pane_id")
        .and_then(Value::as_str)
        .ok_or("invalid core protocol")?;
    let generation = decimal(event.get("pty_generation"))?;
    decimal(event.get("screen_generation"))?;
    let reason = event
        .get("reason")
        .and_then(Value::as_str)
        .ok_or("invalid core protocol")?;
    let pane = state
        .panes
        .get_mut(pane_id)
        .ok_or("invalid core protocol")?;
    let generation_changed = pane.pty_generation.as_deref() != Some(&generation);
    match reason {
        "screen_dirty" => {
            if generation_changed {
                pane.pty_generation = Some(generation);
                pane.invalidated = true;
                pane.process = "unknown";
            }
        }
        "resized" => {
            if generation_changed {
                pane.pty_generation = Some(generation);
                pane.invalidated = true;
                pane.process = "unknown";
            }
        }
        "pty_changed" => {
            pane.pty_generation = Some(generation);
            pane.process = "unknown";
            pane.invalidated = true;
        }
        "exited" => {
            pane.pty_generation = Some(generation);
            pane.process = "exited";
            pane.invalidated = true;
        }
        "removed" => {
            pane.pty_generation = Some(generation);
            pane.process = "removed";
            pane.invalidated = true;
        }
        _ => return Err("invalid core protocol"),
    }
    state.last_event_seq = Some(sequence_text);
    state.observed_at_ms = observation::now_ms();
    Ok(sequence)
}

fn validate_gap(
    value: &Value,
    boot_id: &str,
    epoch: &str,
    last_sequence: u64,
) -> Result<(), &'static str> {
    let gap = exact_object(
        value,
        &[
            "v",
            "kind",
            "core_boot_id",
            "stream_epoch",
            "after_seq",
            "first_available_seq",
            "last_seq",
            "code",
        ],
        "watch gap",
    )?;
    if gap.get("v").and_then(Value::as_u64) != Some(1)
        || gap.get("kind").and_then(Value::as_str) != Some("gap")
        || gap.get("core_boot_id").and_then(Value::as_str) != Some(boot_id)
        || gap.get("stream_epoch").and_then(Value::as_str) != Some(epoch)
        || gap.get("code").and_then(Value::as_str) != Some("resync_required")
    {
        return Err("invalid core protocol");
    }
    let after = decimal(gap.get("after_seq"))?
        .parse::<u64>()
        .map_err(|_| "invalid core protocol")?;
    let first = decimal(gap.get("first_available_seq"))?
        .parse::<u64>()
        .map_err(|_| "invalid core protocol")?;
    let last = decimal(gap.get("last_seq"))?
        .parse::<u64>()
        .map_err(|_| "invalid core protocol")?;
    if after < last_sequence || first <= after || first - after <= 1 || last < first {
        return Err("invalid core protocol");
    }
    Ok(())
}

async fn observe_core(core: PathBuf, config: Config, sender: watch::Sender<CoreState>) {
    let mut state = CoreState::initial(&config);
    let mut stream = match timeout(DEADLINE, UnixStream::connect(&core)).await {
        Ok(Ok(stream)) => stream,
        _ => {
            state.mark_stale("stale", "unavailable");
            sender.send_replace(state);
            return;
        }
    };
    if core_write(
        &mut stream,
        &json!({"v":1,"kind":"hello","request_id":"agentd-hello"}),
    )
    .await
    .is_err()
    {
        state.mark_stale("stale", "unavailable");
        sender.send_replace(state);
        return;
    }
    let hello = match core_read(&mut stream)
        .await
        .and_then(|value| validate_hello(&value))
    {
        Ok(hello) => hello,
        Err(_) => {
            state.mark_stale("stale", "protocol");
            sender.send_replace(state);
            return;
        }
    };
    let pane_ids: Vec<_> = state.panes.keys().cloned().collect();
    let scope: HashSet<_> = pane_ids.iter().cloned().collect();
    let request = json!({
        "v":1,"kind":"watch","request_id":"agentd-watch",
        "expected_core_boot_id":hello,"pane_ids":pane_ids,
    });
    if core_write(&mut stream, &request).await.is_err() {
        state.mark_stale("stale", "unavailable");
        sender.send_replace(state);
        return;
    }
    let ack = match core_read(&mut stream).await {
        Ok(ack) => ack,
        Err(_) => {
            state.mark_stale("stale", "unavailable");
            sender.send_replace(state);
            return;
        }
    };
    if validate_watch_error(&ack) {
        state.invalidate("stale", "target_invalid");
        sender.send_replace(state);
        return;
    }
    let (epoch, sequence_text) = match validate_ack(&ack, &hello, &scope, &mut state) {
        Ok(identity) => identity,
        Err(_) => {
            state.mark_stale("stale", "protocol");
            sender.send_replace(state);
            return;
        }
    };
    let mut sequence = sequence_text.parse::<u64>().unwrap_or_default();
    state.freshness = "fresh";
    state.reason = None;
    state.core_boot_id = Some(hello.clone());
    state.stream_epoch = Some(epoch.clone());
    state.last_event_seq = Some(sequence_text);
    state.observed_at_ms = observation::now_ms();
    sender.send_replace(state.clone());

    loop {
        let value = match core_read_stream(&mut stream).await {
            Ok(value) => value,
            Err(_) => {
                state.invalidate("stale", "stream_lost");
                sender.send_replace(state);
                return;
            }
        };
        match value.get("kind").and_then(Value::as_str) {
            Some("event") => match apply_event(&value, &hello, &epoch, sequence, &mut state) {
                Ok(next) => {
                    sequence = next;
                    sender.send_replace(state.clone());
                }
                Err(_) => {
                    state.invalidate("stale", "protocol");
                    sender.send_replace(state);
                    return;
                }
            },
            Some("gap") if validate_gap(&value, &hello, &epoch, sequence).is_ok() => {
                state.invalidate("gap", "resync_required");
                sender.send_replace(state);
                return;
            }
            _ => {
                state.invalidate("stale", "protocol");
                sender.send_replace(state);
                return;
            }
        }
    }
}

async fn run(listener: StdUnixListener, core: PathBuf, config: Config) -> Result<i32, String> {
    let listener =
        UnixListener::from_std(listener).map_err(|_| "cannot register manager socket")?;
    let mut source_receivers = Vec::with_capacity(config.sources.len());
    let mut tasks = Vec::with_capacity(config.sources.len() + 1);
    for source in &config.sources {
        let (sender, receiver) = watch::channel(SourceState::initial(source));
        source_receivers.push(receiver);
        tasks.push(tokio::spawn(crate::opencode::observe_source(
            source.clone(),
            sender,
        )));
    }
    let (core_sender, core_receiver) = watch::channel(CoreState::initial(&config));
    tasks.push(tokio::spawn(observe_core(
        core,
        config.clone(),
        core_sender,
    )));
    let state = Arc::new(DaemonState {
        config,
        sources: source_receivers,
        core: core_receiver,
    });
    let clients = Arc::new(Semaphore::new(CLIENT_LIMIT));
    let (stop_sender, mut stop_receiver) = watch::channel(false);

    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|_| "cannot register termination signal")?;

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { continue; };
                let Ok(standard) = stream.into_std() else { continue; };
                if !peer_is_owner(&standard) { continue; }
                let Ok(stream) = UnixStream::from_std(standard) else { continue; };
                let Ok(permit) = clients.clone().try_acquire_owned() else { continue; };
                let state = Arc::clone(&state);
                let stop = stop_sender.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    handle_client(stream, state, stop).await;
                });
            }
            changed = stop_receiver.changed() => {
                if changed.is_err() || *stop_receiver.borrow() { break; }
            }
            result = tokio::signal::ctrl_c() => {
                let _ = result;
                break;
            }
            _ = terminate.recv() => break,
        }
    }
    for task in tasks {
        task.abort();
    }
    Ok(0)
}

/// Run one foreground agentd instance.
pub fn serve(socket: &Path, core: &Path, config: Config) -> Result<i32, String> {
    observation::validate(&config)?;
    let (_lock, canonical_core) = lock_core(core)?;
    let (listener, _socket_guard) = bind_manager(socket)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(2)
        .build()
        .map_err(|_| "cannot build agentd runtime")?;
    runtime.block_on(run(listener, canonical_core, config))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn management_requests_are_strict_and_bounded() {
        let request =
            parse_request(br#"{"v":1,"kind":"inspect","request_id":"r1","id":"agent-1"}"#)
                .expect("valid request");
        assert_eq!(request.kind, "inspect");
        assert!(parse_request(br#"{"v":1,"v":1,"kind":"status","request_id":"r1"}"#).is_err());
        let error =
            parse_request(br#"{"v":1,"kind":"status","request_id":"r1","unexpected":true}"#)
                .expect_err("unknown fields must be rejected");
        assert_eq!(error.0, "r1");
        assert!(parse_request(br#"{"v":1,"kind":"inspect","request_id":"r1"}"#).is_err());
    }

    #[test]
    fn pty_lifecycle_permanently_invalidates_the_association() {
        let mut state = CoreState {
            freshness: "fresh",
            reason: None,
            core_boot_id: Some("boot".into()),
            stream_epoch: Some("3".into()),
            last_event_seq: Some("5".into()),
            observed_at_ms: 1,
            panes: HashMap::from([(
                "%0".into(),
                CorePane {
                    process: "running",
                    pty_generation: Some("2".into()),
                    invalidated: false,
                },
            )]),
        };
        let changed = json!({
            "v":1,"kind":"event","core_boot_id":"boot","stream_epoch":"3",
            "event_seq":"6","pane_id":"%0","pty_generation":"3",
            "screen_generation":"8","reason":"pty_changed"
        });
        assert_eq!(apply_event(&changed, "boot", "3", 5, &mut state), Ok(6));
        assert!(state.panes["%0"].invalidated);

        let dirty = json!({
            "v":1,"kind":"event","core_boot_id":"boot","stream_epoch":"3",
            "event_seq":"7","pane_id":"%0","pty_generation":"3",
            "screen_generation":"9","reason":"screen_dirty"
        });
        assert_eq!(apply_event(&dirty, "boot", "3", 6, &mut state), Ok(7));
        assert!(state.panes["%0"].invalidated);
    }
}
