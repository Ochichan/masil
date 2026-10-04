//! `masil-agent ext` (P9, docs/extensions.md): API tokens, and the
//! extensions configured in `extensions.toml`. An extension masil starts
//! has the API on FD 3, a socket pair (`MASIL_API_FD=3`), in its scope; its
//! standard output is the action's result for `ext run` and a log for a
//! resident one, so a stray print never breaks the protocol.

use super::scope::Scope;
use super::{Session, tokens};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::sync::{Notify, Semaphore, broadcast};

/// Resident extensions a server keeps at once.
pub(crate) const RESIDENT_LIMIT: i64 = 8;
const MAX_EXTENSIONS: usize = 32;
const LOG_LIMIT: u64 = 256 * 1024;
const RUN_STDERR: usize = 64 * 1024;
/// A resident that ends this often in this window is not started again.
const FAILURES: i64 = 5;
const FAILURE_WINDOW: Duration = Duration::from_secs(600);
const BACKOFF_FIRST: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
/// Calls at once across all of a coordinator's extensions.
const SHARED_CALLS: usize = 4;

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Extension {
    pub(crate) name: String,
    pub(crate) command: Vec<String>,
    #[serde(default = "read_scope")]
    pub(crate) scope: String,
    #[serde(default)]
    pub(crate) events: Vec<String>,
    #[serde(default)]
    pub(crate) resident: bool,
    #[serde(default)]
    pub(crate) actions: Vec<String>,
    /// How long `ext run` may take, in seconds.
    #[serde(default)]
    pub(crate) timeout: Option<u64>,
}

fn read_scope() -> String {
    "read".into()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    extension: Vec<Extension>,
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name.as_bytes()[0].is_ascii_lowercase()
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'_'
        })
}

fn config_path() -> Result<PathBuf, String> {
    Ok(crate::managed::endpoints::config_root()
        .ok_or("extension_invalid: no configuration directory (HOME)")?
        .join("masil/extensions.toml"))
}

/// A configuration file of the user's (`extensions.toml`,
/// `dictation.toml`): None when missing. It must be the user's own regular
/// file of at most 64 KiB, not a symlink, not writable by group or others.
/// Errors begin with `code`.
pub(crate) fn read_user_config(path: &Path, code: &str) -> Result<Option<String>, String> {
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{code}: {}: {error}", path.display())),
    };
    let metadata = file
        .metadata()
        .map_err(|error| format!("{code}: {error}"))?;
    // SAFETY: geteuid has no preconditions.
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o022 != 0
        || metadata.len() > 64 * 1024
    {
        return Err(format!(
            "{code}: {} must be your own file of at most 64 KiB, not writable by group or others",
            path.display()
        ));
    }
    let mut text = String::new();
    file.take(64 * 1024)
        .read_to_string(&mut text)
        .map_err(|error| format!("{code}: {error}"))?;
    Ok(Some(text))
}

/// The configured extensions; none without a file.
pub(crate) fn load() -> Result<Vec<Extension>, String> {
    let Some(text) = read_user_config(&config_path()?, "extension_invalid")? else {
        return Ok(Vec::new());
    };
    let parsed: File =
        toml::from_str(&text).map_err(|error| format!("extension_invalid: {error}"))?;
    if parsed.extension.len() > MAX_EXTENSIONS {
        return Err("extension_invalid: at most 32 extensions".into());
    }
    let mut seen = Vec::new();
    for extension in &parsed.extension {
        let refuse = |why: &str| Err(format!("extension_invalid: {}: {why}", extension.name));
        if !valid_name(&extension.name) || seen.contains(&extension.name) {
            return refuse("a unique name of 1-32 lowercase letters, digits, '-' or '_'");
        }
        seen.push(extension.name.clone());
        if !extension
            .command
            .first()
            .is_some_and(|program| Path::new(program).is_absolute())
        {
            return refuse("command is a list whose first item is an absolute path");
        }
        if Scope::parse(&extension.scope).is_err() {
            return refuse("scope is read, act or admin");
        }
        if extension.events.iter().any(|event| event != "inbox") {
            return refuse("events may be [\"inbox\"]");
        }
        if extension.actions.iter().any(|action| !valid_name(action)) {
            return refuse("actions are names of lowercase letters, digits, '-' or '_'");
        }
        if extension
            .timeout
            .is_some_and(|seconds| !(1..=3600).contains(&seconds))
        {
            return refuse("timeout is 1-3600 seconds");
        }
    }
    Ok(parsed.extension)
}

fn find(name: &str) -> Result<Extension, String> {
    load()?
        .into_iter()
        .find(|extension| extension.name == name)
        .ok_or_else(|| format!("extension_invalid: no extension named {name} in extensions.toml"))
}

/// `$XDG_STATE_HOME/masil/extensions/<server>`: logs, locks, process records.
fn state_dir(socket: &Path) -> Result<PathBuf, String> {
    crate::managed::private_directory(
        &crate::managed::state_base()?
            .join("masil/extensions")
            .join(crate::coordinator::digest16(socket)),
    )
}

/// A log file opened for appending, renamed aside past its limit.
fn log_file(directory: &Path, name: &str) -> Result<std::fs::File, String> {
    let path = directory.join(format!("{name}.log"));
    if std::fs::metadata(&path).is_ok_and(|metadata| metadata.len() > LOG_LIMIT) {
        let _ = std::fs::rename(&path, directory.join(format!("{name}.log.1")));
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)
        .map_err(|error| format!("extension log: {error}"))
}

/// Starts `extension` (with `action` last on its command line) in a process
/// group of its own, its environment masil's choice, the API on FD 3.
fn spawn(
    extension: &Extension,
    action: Option<&str>,
    stdout: Stdio,
    stderr: Stdio,
) -> Result<(tokio::process::Child, tokio::net::UnixStream), String> {
    let (ours, theirs) =
        std::os::unix::net::UnixStream::pair().map_err(|error| format!("extension: {error}"))?;
    let mut command = tokio::process::Command::new(&extension.command[0]);
    command.args(&extension.command[1..]);
    if let Some(action) = action {
        command.arg(action);
    }
    command.env_clear();
    for name in ["PATH", "HOME", "USER", "LANG", "LC_ALL"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
        .env("MASIL_EXTENSION", &extension.name)
        .env("MASIL_API_VERSION", super::VERSION.to_string())
        .env("MASIL_API_FD", "3");
    if let Some(home) = std::env::var_os("HOME") {
        command.current_dir(home);
    }
    let fd = theirs.as_raw_fd();
    // SAFETY: between fork and exec, only async-signal-safe calls. FD 3
    // becomes the child's end of the pair without close-on-exec (dup2
    // clears it; when the end already is FD 3, fcntl does), and the
    // signals the coordinator ignored before its runtime took them are
    // the defaults again.
    unsafe {
        command.pre_exec(move || {
            let moved = if fd == 3 {
                libc::fcntl(3, libc::F_SETFD, 0)
            } else {
                libc::dup2(fd, 3)
            };
            if moved < 0 {
                return Err(std::io::Error::last_os_error());
            }
            for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP, libc::SIGUSR1] {
                libc::signal(signal, libc::SIG_DFL);
            }
            Ok(())
        });
    }
    command
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .process_group(0);
    let child = command
        .spawn()
        .map_err(|error| format!("extension {}: {error}", extension.name))?;
    drop(theirs);
    ours.set_nonblocking(true)
        .map_err(|error| error.to_string())?;
    let ours = tokio::net::UnixStream::from_std(ours).map_err(|error| error.to_string())?;
    Ok((child, ours))
}

fn session(extension: &Extension, socket: Option<PathBuf>) -> Result<Session, String> {
    let mut session = Session::new(socket, Scope::parse(&extension.scope)?);
    session.events = extension.events.clone();
    Ok(session)
}

fn usage() -> String {
    "usage: masil-agent ext list | run NAME [ACTION] | enable NAME | disable NAME [--socket SOCKET] | token create NAME --scope read|act|admin | token list | token revoke NAME".into()
}

pub(crate) fn run(args: &[String]) -> Result<i32, String> {
    let mut socket = None;
    let mut words: Vec<&str> = Vec::new();
    let mut index = 0;
    while index < args.len() {
        if args[index] == "--socket" {
            socket = Some(PathBuf::from(args.get(index + 1).ok_or_else(usage)?));
            index += 2;
        } else {
            words.push(&args[index]);
            index += 1;
        }
    }
    // Stores and state are named by the canonical path.
    let socket = socket
        .or_else(super::current_socket)
        .map(|socket| socket.canonicalize().unwrap_or(socket));
    let value: Value = match words.as_slice() {
        ["token", "create", name, "--scope", scope] => tokens::create(name, Scope::parse(scope)?)?,
        ["token", "list"] => tokens::list()?,
        ["token", "revoke", name] => tokens::revoke(name)?,
        ["run", name] => return Ok(run_once(socket, name, None)),
        ["run", name, action] => return Ok(run_once(socket, name, Some(action))),
        ["list"] => list(socket.as_deref())?,
        ["enable", name] => switch(socket.as_deref(), name, true)?,
        ["disable", name] => switch(socket.as_deref(), name, false)?,
        _ => return Err(usage()),
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&value).map_err(|error| error.to_string())?
    );
    Ok(0)
}

fn list(socket: Option<&Path>) -> Result<Value, String> {
    let configured = load()?;
    let running = socket.and_then(crate::coordinator::extensions_status);
    let stored: HashMap<String, Value> = socket
        .and_then(|socket| crate::managed::extension_rows(socket).ok())
        .unwrap_or_default()
        .into_iter()
        .map(|row| (row["name"].as_str().unwrap_or_default().to_owned(), row))
        .collect();
    let extensions: Vec<Value> = configured
        .iter()
        .map(|extension| {
            let mut value = json!({
                "name": extension.name,
                "scope": extension.scope,
                "resident": extension.resident,
                "actions": extension.actions,
                "events": extension.events,
            });
            if let Some(row) = stored.get(&extension.name) {
                value["enabled"] = row["enabled"].clone();
                value["state"] = row["state"].clone();
                value["restarts"] = row["restarts"].clone();
            }
            if let Some(live) = running
                .as_ref()
                .and_then(|running| running.get(&extension.name))
            {
                value["process"] = live.clone();
            }
            value
        })
        .collect();
    Ok(json!({"extensions": extensions}))
}

fn switch(socket: Option<&Path>, name: &str, enabled: bool) -> Result<Value, String> {
    let socket = socket.ok_or("usage: give --socket SOCKET or run inside masil")?;
    if enabled && !find(name)?.resident {
        return Err(format!(
            "extension_invalid: {name} is not resident = true in extensions.toml"
        ));
    }
    crate::managed::extension_set(socket, name, enabled, RESIDENT_LIMIT)?;
    let coordinator = if enabled {
        match crate::coordinator::ensure(socket) {
            Ok(_) => {
                crate::coordinator::reload(socket);
                "running".to_owned()
            }
            Err(error) => format!("error: {error}"),
        }
    } else {
        crate::coordinator::reload(socket);
        "reloaded".to_owned()
    };
    Ok(json!({"name": name, "enabled": enabled, "coordinator": coordinator}))
}

/// Says a problem where the person is: the log, and a message in tmux.
fn report(socket: Option<&Path>, directory: Option<&Path>, name: &str, message: &str) {
    if let Some(directory) = directory
        && let Ok(mut log) = log_file(directory, name)
    {
        let _ = writeln!(log, "{} ext run: {message}", crate::observation::now_ms());
    }
    if let Some(socket) = socket {
        let text = format!("masil ext {name}: {message}").replace('#', "##");
        let _ = crate::coordinator::masil(socket, &["display-message", "-d", "3000", "-l", &text]);
    }
}

/// `ext run NAME [ACTION]`, as a key binding's `run-shell -b` starts it:
/// the extension runs once with the API on FD 3; its standard output is
/// the result tmux shows. Exits 0 on every path (tmux prints any other
/// status into the pane); problems go to the log and a tmux message.
fn run_once(socket: Option<PathBuf>, name: &str, action: Option<&str>) -> i32 {
    let directory = socket.as_deref().and_then(|socket| state_dir(socket).ok());
    let result = (|| -> Result<(), String> {
        let extension = find(name)?;
        if let Some(action) = action
            && !extension.actions.iter().any(|allowed| allowed == action)
        {
            return Err(format!(
                "extension_invalid: {action} is not one of its actions"
            ));
        }
        let directory = directory
            .clone()
            .ok_or("usage: give --socket SOCKET or run inside masil")?;
        // One run of an extension at a time.
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(directory.join(format!("{name}.lock")))
            .map_err(|error| format!("extension lock: {error}"))?;
        // SAFETY: flock on an open descriptor, released when it closes.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err("it is already running".into());
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| error.to_string())?;
        let limit = Duration::from_secs(extension.timeout.unwrap_or(60));
        let session = session(&extension, socket.clone())?;
        runtime.block_on(async move {
            let (mut child, ours) = spawn(&extension, action, Stdio::inherit(), Stdio::piped())?;
            let group = child.id().and_then(|pid| i32::try_from(pid).ok());
            let mut stderr = child.stderr.take().ok_or("no pipe")?;
            let log = log_file(&directory, name)?;
            let errors = tokio::spawn(async move {
                let mut log = log;
                let mut written = 0usize;
                let mut buffer = [0u8; 8192];
                // Kept up to the limit, the rest read and dropped.
                while let Ok(read) = stderr.read(&mut buffer).await {
                    if read == 0 {
                        break;
                    }
                    let room = RUN_STDERR.saturating_sub(written);
                    let _ = log.write_all(&buffer[..read.min(room)]);
                    written += read.min(room);
                }
            });
            let (reader, writer) = ours.into_split();
            let api = tokio::spawn(super::serve(reader, writer, session));
            let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .map_err(|error| error.to_string())?;
            let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
                .map_err(|error| error.to_string())?;
            let outcome = tokio::select! {
                status = tokio::time::timeout(limit, child.wait()) => match status {
                    Ok(Ok(status)) if status.success() => Ok(()),
                    Ok(Ok(status)) => Err(format!("ended with {status}")),
                    Ok(Err(error)) => Err(error.to_string()),
                    Err(_) => Err(format!("did not finish in {} s and was stopped", limit.as_secs())),
                },
                _ = term.recv() => Err("stopped".to_owned()),
                _ = hangup.recv() => Err("stopped".to_owned()),
            };
            // What it left in its group goes too; past its time, it goes first.
            if let Some(group) = group {
                super::end_group(group, &mut child).await;
            }
            let _ = tokio::time::timeout(Duration::from_secs(2), errors).await;
            // Its end of FD 3 is closed: reads stop, acts go on by themselves.
            let _ = tokio::time::timeout(Duration::from_secs(2), api).await;
            outcome
        })
    })();
    if let Err(error) = result {
        report(socket.as_deref(), directory.as_deref(), name, &error);
    }
    0
}

// ---------------------------------------------------------------- residents

type Log = Arc<dyn Fn(&str) + Send + Sync>;

/// The coordinator's resident extensions (`ext enable`), kept by a thread
/// with its own runtime.
#[derive(Default)]
pub(crate) struct Residents {
    running: Mutex<HashMap<String, Resident>>,
    wake: Notify,
    stopping: AtomicBool,
    /// The thread runs.
    thread: AtomicBool,
}

struct Resident {
    extension: Extension,
    stop: Arc<Notify>,
    stopped: Arc<AtomicBool>,
    status: Arc<Mutex<Value>>,
    /// Failures counted and when their window began.
    counters: Arc<Mutex<(i64, i64)>>,
    /// When the row was last switched on: a later one means `ext enable`
    /// ran again.
    changed_ms: i64,
    task: tokio::task::JoinHandle<()>,
}

impl Resident {
    fn end(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.stop.notify_one();
    }
}

/// Where this coordinator records its extensions' process groups, for its
/// panic hook.
static RECORDS: OnceLock<PathBuf> = OnceLock::new();

#[derive(serde::Serialize, Deserialize)]
struct Record {
    group: i32,
    started: u64,
}

/// Ends the process groups that records in `directory` name, when the
/// recorded leader still runs (same start time), and removes the records.
/// With `wait`, KILL follows TERM 2 s later; without, at once.
pub(crate) fn sweep_records(directory: &Path, wait: bool) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let mut ended = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("pid") {
            continue;
        }
        let record = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Record>(&bytes).ok());
        if let Some(record) = record
            && record.group > 1
            && crate::process::info(record.group).is_some_and(|info| info.started == record.started)
        {
            // SAFETY: the recorded leader still runs, so the group is its.
            unsafe { libc::killpg(record.group, libc::SIGTERM) };
            ended.push(record);
        }
        let _ = std::fs::remove_file(&path);
    }
    if ended.is_empty() {
        return;
    }
    // The group outlives its leader while any member runs; a group id is
    // not given to a new process meanwhile.
    // SAFETY: signal 0 only checks.
    let left = |group: i32| unsafe { libc::killpg(group, 0) } == 0;
    if wait {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && ended.iter().any(|record| left(record.group)) {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    for record in ended {
        if left(record.group) {
            // SAFETY: as above.
            unsafe { libc::killpg(record.group, libc::SIGKILL) };
        }
    }
}

/// For the coordinator's panic hook.
pub(crate) fn sweep_on_panic() {
    if let Some(directory) = RECORDS.get() {
        sweep_records(directory, false);
    }
}

impl Residents {
    pub(crate) fn wake(&self) {
        self.wake.notify_one();
    }

    /// Asks every extension and the thread to end.
    pub(crate) fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        if let Ok(running) = self.running.lock() {
            running.values().for_each(Resident::end);
        }
        self.wake.notify_one();
    }

    /// Waits up to `limit` for the thread to end.
    pub(crate) async fn stopped(&self, limit: Duration) {
        let deadline = tokio::time::Instant::now() + limit;
        while self.thread.load(Ordering::SeqCst) && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Each running extension's state (`extensions_status`).
    pub(crate) fn status(&self) -> Value {
        let Ok(running) = self.running.lock() else {
            return json!({});
        };
        Value::Object(
            running
                .iter()
                .map(|(name, resident)| {
                    let status = resident
                        .status
                        .lock()
                        .map(|status| status.clone())
                        .unwrap_or_default();
                    (name.clone(), status)
                })
                .collect(),
        )
    }

    /// Starts the thread unless it runs.
    pub(crate) fn start(self: &Arc<Self>, socket: PathBuf, inbox: Arc<Notify>, log: Log) {
        self.stopping.store(false, Ordering::SeqCst);
        if self.thread.swap(true, Ordering::SeqCst) {
            return;
        }
        let residents = self.clone();
        let failed = log.clone();
        let spawned = std::thread::Builder::new()
            .name("extensions".into())
            .spawn(move || {
                let served = residents.serve(socket.clone(), inbox.clone(), log.clone());
                residents.thread.store(false, Ordering::SeqCst);
                // Asked to run again while it was ending.
                if served && !residents.stopping.load(Ordering::SeqCst) {
                    residents.start(socket, inbox, log);
                }
            });
        if let Err(error) = spawned {
            self.thread.store(false, Ordering::SeqCst);
            failed(&format!("extensions: {error}"));
        }
    }

    /// Keeps the extensions until asked to stop; false if it could not
    /// begin.
    fn serve(&self, socket: PathBuf, inbox: Arc<Notify>, log: Log) -> bool {
        // Stores and state are named by the canonical path.
        let socket = socket.canonicalize().unwrap_or(socket);
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                log(&format!("extensions: {error}"));
                return false;
            }
        };
        let directory = match state_dir(&socket) {
            Ok(directory) => directory,
            Err(error) => {
                log(&format!("extensions: {error}"));
                return false;
            }
        };
        let _ = RECORDS.set(directory.clone());
        // What an ended coordinator left running.
        sweep_records(&directory, true);
        runtime.block_on(async {
            let shared = Arc::new(Semaphore::new(SHARED_CALLS));
            let (feed, _) = broadcast::channel::<Value>(256);
            let watcher = tokio::spawn(watch_inbox(socket.clone(), inbox, feed.clone()));
            let mut last_error = None;
            while !self.stopping.load(Ordering::SeqCst) {
                let result = self
                    .reconcile(&socket, &directory, &shared, &feed, &log)
                    .await;
                // A file being edited is said once, not every pass.
                if let Err(error) = &result
                    && last_error.as_ref() != Some(error)
                {
                    log(&format!("extensions: {error}"));
                }
                last_error = result.err();
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(30)) => {}
                    _ = self.wake.notified() => {}
                }
            }
            let ending: Vec<Resident> = self
                .running
                .lock()
                .map(|mut running| running.drain().map(|(_, resident)| resident).collect())
                .unwrap_or_default();
            ending.iter().for_each(Resident::end);
            let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
            for resident in ending {
                let _ = tokio::time::timeout_at(deadline, resident.task).await;
            }
            watcher.abort();
        });
        drop(runtime);
        sweep_records(&directory, false);
        true
    }

    /// Runs what the store has on and extensions.toml configures as
    /// resident; ends the rest, and any whose configuration changed.
    async fn reconcile(
        &self,
        socket: &Path,
        directory: &Path,
        shared: &Arc<Semaphore>,
        feed: &broadcast::Sender<Value>,
        log: &Log,
    ) -> Result<(), String> {
        let path = socket.to_owned();
        let (rows, configured) = tokio::task::spawn_blocking(move || {
            Ok::<_, String>((crate::managed::extension_rows(&path)?, load()?))
        })
        .await
        .map_err(|error| error.to_string())??;
        let wanted: HashMap<String, (Extension, Value)> = rows
            .into_iter()
            .filter(|row| {
                row["enabled"] == true
                    && !matches!(row["state"].as_str(), Some("failed" | "stopped"))
            })
            .filter_map(|row| {
                let extension = configured
                    .iter()
                    .find(|extension| extension.resident && row["name"] == json!(extension.name))?;
                Some((extension.name.clone(), (extension.clone(), row)))
            })
            .collect();
        let afresh = {
            let mut running = self.running.lock().map_err(|_| "extensions unavailable")?;
            // One that ended since the rows were read is started again only
            // after they are read again (it may have recorded `failed`).
            let mut ended = Vec::new();
            let mut afresh = Vec::new();
            running.retain(|name, resident| {
                if resident.task.is_finished() {
                    ended.push(name.clone());
                }
                let keep = wanted
                    .get(name)
                    .is_some_and(|(extension, _)| *extension == resident.extension)
                    && !resident.stopped.load(Ordering::SeqCst)
                    && !resident.task.is_finished();
                if !keep {
                    resident.end();
                } else if let Some(changed) = wanted
                    .get(name)
                    .and_then(|(_, row)| row["changed_ms"].as_i64())
                    .filter(|changed| *changed > resident.changed_ms)
                {
                    // Enabled again while it runs: its failures count
                    // afresh, and the row says what it is doing now.
                    resident.changed_ms = changed;
                    if let Ok(mut counters) = resident.counters.lock() {
                        *counters = (0, 0);
                    }
                    let state = resident
                        .status
                        .lock()
                        .ok()
                        .and_then(|status| status["state"].as_str().map(str::to_owned))
                        .unwrap_or_else(|| "running".into());
                    afresh.push((name.clone(), state));
                }
                keep
            });
            for (name, (extension, row)) in wanted {
                if running.contains_key(&name) || ended.contains(&name) {
                    continue;
                }
                let counters = Arc::new(Mutex::new((
                    row["restarts"].as_i64().unwrap_or(0),
                    row["window_ms"].as_i64().unwrap_or(0),
                )));
                let job = Keep {
                    counters: counters.clone(),
                    extension: extension.clone(),
                    socket: socket.to_owned(),
                    directory: directory.to_owned(),
                    shared: shared.clone(),
                    feed: feed.clone(),
                    stop: Arc::new(Notify::new()),
                    stopped: Arc::new(AtomicBool::new(false)),
                    status: Arc::new(Mutex::new(json!({"state": "starting"}))),
                    log: log.clone(),
                };
                let resident = Resident {
                    extension,
                    stop: job.stop.clone(),
                    stopped: job.stopped.clone(),
                    status: job.status.clone(),
                    counters,
                    changed_ms: row["changed_ms"].as_i64().unwrap_or(0),
                    task: tokio::spawn(keep(job)),
                };
                running.insert(name, resident);
            }
            afresh
        };
        for (name, state) in afresh {
            let (socket, extension) = (socket.to_owned(), name.clone());
            let written = tokio::task::spawn_blocking(move || {
                crate::managed::extension_state(&socket, &extension, &state, 0, 0)
            })
            .await
            .unwrap_or_else(|error| Err(error.to_string()));
            if let Err(error) = written {
                log(&format!("extension {name}: {error}"));
            }
        }
        Ok(())
    }
}

/// One reader of the inbox for every extension: it reads when the watch
/// counted the badge, and every 30 s otherwise.
async fn watch_inbox(socket: PathBuf, inbox: Arc<Notify>, feed: broadcast::Sender<Value>) {
    // The first read, before any extension runs, only finds the end; if it
    // fails, reading starts from the beginning rather than skip anything.
    // From then on every batch goes out, listened to or not: a subscriber
    // reads what came before it itself and drops what it already has.
    let mut cursor: Option<i64> = None;
    let mut now = true;
    loop {
        if !now {
            tokio::select! {
                _ = inbox.notified() => {}
                _ = tokio::time::sleep(Duration::from_secs(30)) => {}
            }
        }
        let (path, after) = (socket.clone(), cursor);
        let read = tokio::task::spawn_blocking(move || crate::managed::inbox_since(&path, after))
            .await
            .unwrap_or_else(|error| Err(error.to_string()));
        now = false;
        let Ok(Some((items, next))) = read else {
            cursor.get_or_insert(0);
            continue;
        };
        if !items.is_empty() {
            now = items.len() >= 200;
            let _ = feed.send(json!({"items": items, "seq": next}));
        }
        cursor = Some(next);
    }
}

/// What keeping one resident extension takes.
struct Keep {
    counters: Arc<Mutex<(i64, i64)>>,
    extension: Extension,
    socket: PathBuf,
    directory: PathBuf,
    shared: Arc<Semaphore>,
    feed: broadcast::Sender<Value>,
    stop: Arc<Notify>,
    stopped: Arc<AtomicBool>,
    status: Arc<Mutex<Value>>,
    log: Log,
}

impl Keep {
    fn show(&self, state: &str, detail: Value) {
        if let Ok(mut status) = self.status.lock() {
            *status =
                json!({"state": state, "detail": detail, "since_ms": crate::observation::now_ms()});
        }
    }

    async fn record(&self, state: &str, restarts: i64, window_ms: i64) {
        let (socket, name, state) = (
            self.socket.clone(),
            self.extension.name.clone(),
            state.to_owned(),
        );
        let written = tokio::task::spawn_blocking(move || {
            crate::managed::extension_state(&socket, &name, &state, restarts, window_ms)
        })
        .await
        .unwrap_or_else(|error| Err(error.to_string()));
        if let Err(error) = written {
            (self.log)(&format!("extension {}: {error}", self.extension.name));
        }
    }

    /// One start: output into the log, the API on FD 3, until the
    /// extension ends (`Some`) or is asked to stop (`None`).
    async fn once(
        &self,
        restarts: i64,
        window_ms: i64,
    ) -> Option<Result<std::process::ExitStatus, String>> {
        let mut session = match session(&self.extension, Some(self.socket.clone())) {
            Ok(session) => session,
            Err(error) => return Some(Err(error)),
        };
        session.shared = Some(self.shared.clone());
        session.feed = Some(self.feed.clone());
        let (mut child, ours) = match spawn(&self.extension, None, Stdio::piped(), Stdio::piped()) {
            Ok(started) => started,
            Err(error) => return Some(Err(error)),
        };
        let group = child.id().and_then(|pid| i32::try_from(pid).ok());
        let record = group.map(|group| {
            self.directory
                .join(format!("{}.{group}.pid", self.extension.name))
        });
        if let (Some(group), Some(path)) = (group, &record)
            && let Some(info) = crate::process::info(group)
        {
            let bytes = serde_json::to_vec(&Record {
                group,
                started: info.started,
            })
            .unwrap_or_default();
            if let Err(error) = write_private(path, &bytes) {
                (self.log)(&format!("extension {}: {error}", self.extension.name));
            }
        }
        tokio::spawn(pump(
            self.directory.clone(),
            self.extension.name.clone(),
            child.stdout.take(),
            child.stderr.take(),
        ));
        self.show("running", json!({"pid": group}));
        self.record("running", restarts, window_ms).await;
        let (reader, writer) = ours.into_split();
        let mut api = tokio::spawn(super::serve(reader, writer, session));
        let ended = tokio::select! {
            status = child.wait() => Some(status.map_err(|error| error.to_string())),
            _ = self.stop.notified() => None,
        };
        // What it left in its group goes too; asked to stop, it goes first.
        if let Some(group) = group {
            super::end_group(group, &mut child).await;
        }
        // Its end of FD 3 is closed: reads stop, acts go on by themselves.
        if tokio::time::timeout(Duration::from_secs(2), &mut api)
            .await
            .is_err()
        {
            api.abort();
        }
        if let Some(path) = record {
            let _ = std::fs::remove_file(path);
        }
        ended
    }
}

/// Keeps one resident extension: started, started again after it fails
/// (1 s, doubling to 60 s between), given up after five failures in ten
/// minutes. Ending with status 0 means it stopped on purpose.
async fn keep(job: Keep) {
    let mut backoff = BACKOFF_FIRST;
    let counted = || {
        job.counters
            .lock()
            .map(|counters| *counters)
            .unwrap_or((0, 0))
    };
    loop {
        if job.stopped.load(Ordering::SeqCst) {
            return;
        }
        let began = Instant::now();
        let (restarts, window_ms) = counted();
        let Some(ended) = job.once(restarts, window_ms).await else {
            job.show("stopped", Value::Null);
            return;
        };
        if matches!(&ended, Ok(status) if status.success()) {
            job.show("stopped", json!("ended with status 0"));
            job.record("stopped", restarts, window_ms).await;
            return;
        }
        let why = match &ended {
            Ok(status) => status.to_string(),
            Err(error) => error.clone(),
        };
        // Read again: an `ext enable` meanwhile starts the count over.
        let (mut restarts, mut window_ms) = counted();
        let now = crate::observation::now_ms() as i64;
        if now - window_ms > FAILURE_WINDOW.as_millis() as i64 {
            window_ms = now;
            restarts = 0;
        }
        restarts += 1;
        if let Ok(mut counters) = job.counters.lock() {
            *counters = (restarts, window_ms);
        }
        if restarts >= FAILURES {
            (job.log)(&format!(
                "extension {}: failed {restarts} times in ten minutes ({why}); not started again",
                job.extension.name
            ));
            job.show("failed", json!(why));
            job.record("failed", restarts, window_ms).await;
            return;
        }
        // A run that lasted starts the waits over.
        if began.elapsed() >= BACKOFF_MAX {
            backoff = BACKOFF_FIRST;
        }
        job.show(
            "backoff",
            json!({"reason": why, "restarts": restarts, "wait_ms": backoff.as_millis() as u64}),
        );
        job.record("backoff", restarts, window_ms).await;
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = job.stop.notified() => {
                job.show("stopped", Value::Null);
                return;
            }
        }
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .and_then(|mut file| file.write_all(bytes))
        .map_err(|error| format!("{}: {error}", path.display()))
}

async fn read_some(
    reader: &mut Option<impl tokio::io::AsyncRead + Unpin>,
    buffer: &mut [u8],
) -> usize {
    match reader {
        Some(reader) => reader.read(buffer).await.unwrap_or(0),
        None => std::future::pending().await,
    }
}

/// Copies a resident's output into its log, set aside at its limit.
async fn pump(
    directory: PathBuf,
    name: String,
    mut stdout: Option<tokio::process::ChildStdout>,
    mut stderr: Option<tokio::process::ChildStderr>,
) {
    let mut log = log_file(&directory, &name).ok();
    let mut size = log
        .as_ref()
        .and_then(|file| file.metadata().ok())
        .map_or(0, |metadata| metadata.len());
    let mut out = [0u8; 8192];
    let mut err = [0u8; 8192];
    while stdout.is_some() || stderr.is_some() {
        let (first, read) = tokio::select! {
            read = read_some(&mut stdout, &mut out) => (true, read),
            read = read_some(&mut stderr, &mut err) => (false, read),
        };
        if read == 0 {
            if first {
                stdout = None;
            } else {
                stderr = None;
            }
            continue;
        }
        if size > LOG_LIMIT {
            log = log_file(&directory, &name).ok();
            size = 0;
        }
        let bytes = if first { &out[..read] } else { &err[..read] };
        if let Some(log) = log.as_mut() {
            let _ = log.write_all(bytes);
        }
        size += read as u64;
    }
}
