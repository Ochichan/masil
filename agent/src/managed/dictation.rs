//! `agent dictate` (P10a, docs/dictation.md): a command the person chose
//! records and transcribes; its text becomes an item of the agent's prompt
//! queue and is never sent. masil starts, stops and ends that command and
//! never touches audio. One dictation per user at a time, since a machine
//! has one microphone.
//!
//! The microphone must not outlive the dictation, whatever ends `dictate`:
//! the command's standard input is a pipe only `dictate` holds, so it sees
//! EOF when `dictate` goes; it gets its own time limit; its process tree is
//! noted every half second with start times; and its processes carry a
//! per-run nonce in their environment (macOS hides it only for its own
//! platform binaries). A later `dictate` or the panic hook ends what is
//! left by these.

use super::Manager;
use crate::observation::now_ms;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::signal::unix::{Signal, SignalKind, signal};

const OPTION: &str = "@masil-dictation";
const STDERR_KEPT: usize = 64 * 1024;
/// What of a failed run's standard error goes to the log.
const STDERR_LOGGED: usize = 4 * 1024;
const LOG_LIMIT: u64 = 256 * 1024;
/// Processes noted per run, live ones only.
const TREE_LIMIT: usize = 256;
/// How long the output may stay open after the command ended: a helper
/// that left its group may hold it.
const OUTPUT_GRACE: Duration = Duration::from_secs(2);
/// Processes named so are a remote login's.
const REMOTE_PROGRAMS: &[&str] = &[
    "sshd",
    "sshd-session",
    "mosh-server",
    "tailscaled",
    "dropbear",
    "etserver",
    "etterminal",
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    command: Vec<String>,
    #[serde(default = "max_seconds")]
    max_seconds: u64,
    #[serde(default = "finish_seconds")]
    finish_seconds: u64,
    #[serde(default)]
    allow_remote_clients: bool,
}

fn max_seconds() -> u64 {
    120
}

fn finish_seconds() -> u64 {
    60
}

fn load() -> Result<Config, String> {
    let path = super::endpoints::config_root()
        .ok_or("dictation_off: no configuration directory (HOME)")?
        .join("masil/dictation.toml");
    let text = crate::api::ext::read_user_config(&path, "dictation_invalid")?.ok_or_else(|| {
        format!(
            "dictation_off: dictation is off; it needs {} (docs/dictation.md)",
            path.display()
        )
    })?;
    let config: Config =
        toml::from_str(&text).map_err(|error| format!("dictation_invalid: {error}"))?;
    if !config
        .command
        .first()
        .is_some_and(|program| Path::new(program).is_absolute())
    {
        return Err(
            "dictation_invalid: command is a list whose first item is an absolute path".into(),
        );
    }
    for (name, value) in [
        ("max_seconds", config.max_seconds),
        ("finish_seconds", config.finish_seconds),
    ] {
        if !(1..=600).contains(&value) {
            return Err(format!("dictation_invalid: {name} is 1-600 seconds"));
        }
    }
    Ok(config)
}

/// What a dictation in progress leaves: written before its command starts,
/// meaningful while its owner (pid and start time) runs.
#[derive(Clone, Default, Serialize, Deserialize)]
struct Record {
    nonce: String,
    pid: i32,
    started: u64,
    socket: PathBuf,
    target: String,
    run: String,
    pane: String,
    client: Option<String>,
    state: String,
    started_ms: u64,
    group: Option<i32>,
    leader_started: Option<u64>,
    /// The command's live processes as last seen, with their start times:
    /// one that left the group is still found.
    #[serde(default)]
    tree: Vec<(i32, u64)>,
}

impl Record {
    fn owner_runs(&self) -> bool {
        self.pid > 1 && same(self.pid, self.started)
    }
}

/// Whether `pid` is still the process that started at `started`.
fn same(pid: i32, started: u64) -> bool {
    crate::process::info(pid).is_some_and(|info| info.started == started)
}

struct Paths {
    directory: PathBuf,
}

impl Paths {
    fn new() -> Result<Self, String> {
        Ok(Self {
            directory: super::private_directory(&super::state_base()?.join("masil/dictation"))?,
        })
    }

    fn record(&self) -> PathBuf {
        self.directory.join("current.json")
    }

    fn stop(&self) -> PathBuf {
        self.directory.join("stop")
    }

    fn lock(&self) -> Result<std::fs::File, String> {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(self.directory.join("lock"))
            .map_err(|error| format!("dictation lock: {error}"))
    }

    /// The lock, if nobody holds it.
    fn take(&self) -> Result<Option<std::fs::File>, String> {
        let file = self.lock()?;
        // SAFETY: flock on an open descriptor, released when it closes.
        let taken = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
        Ok(taken.then_some(file))
    }

    fn read(&self) -> Option<Record> {
        let bytes = std::fs::read(self.record()).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Written whole and renamed in, so a reader never sees half.
    fn replace(&self, path: &Path, bytes: &[u8]) -> Result<(), String> {
        let temporary = self
            .directory
            .join(format!(".write-{}", std::process::id()));
        write_private(&temporary, bytes)?;
        std::fs::rename(&temporary, path).map_err(|error| error.to_string())
    }

    fn write(&self, record: &Record) -> Result<(), String> {
        let bytes = serde_json::to_vec(record).map_err(|error| error.to_string())?;
        self.replace(&self.record(), &bytes)
    }

    fn clear(&self) {
        let _ = std::fs::remove_file(self.record());
        let _ = std::fs::remove_file(self.stop());
    }

    /// A failed run's line in the log, which is set aside at its limit.
    fn log(&self, entry: &str) {
        let path = self.directory.join("dictation.log");
        if std::fs::metadata(&path).is_ok_and(|metadata| metadata.len() > LOG_LIMIT) {
            let _ = std::fs::rename(&path, self.directory.join("dictation.log.1"));
        }
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
            .and_then(|mut log| log.write_all(entry.as_bytes()));
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

fn nonce() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut random| random.read_exact(&mut bytes))
        .map_err(|error| format!("dictation: {error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// `roots` and every process under them, with start times.
fn tree(roots: &[i32]) -> Vec<(i32, u64)> {
    #[cfg(not(target_os = "macos"))]
    let table: Vec<(i32, crate::process::Info)> = crate::process::all()
        .into_iter()
        .filter_map(|pid| crate::process::info(pid).map(|info| (pid, info)))
        .collect();
    let mut found: Vec<(i32, u64)> = Vec::new();
    let mut next = roots.to_vec();
    while let Some(pid) = next.pop() {
        if found.len() >= TREE_LIMIT || found.iter().any(|(seen, _)| *seen == pid) {
            continue;
        }
        let Some(info) = crate::process::info(pid) else {
            continue;
        };
        found.push((pid, info.started));
        #[cfg(target_os = "macos")]
        next.extend(crate::process::children(pid));
        #[cfg(not(target_os = "macos"))]
        next.extend(
            table
                .iter()
                .filter(|(_, info)| info.parent == pid)
                .map(|(child, _)| *child),
        );
    }
    found
}

/// The noted processes that still run as the same processes.
fn still(noted: &[(i32, u64)]) -> Vec<(i32, u64)> {
    noted
        .iter()
        .filter(|(pid, started)| *pid > 1 && same(*pid, *started))
        .copied()
        .collect()
}

/// This user's processes whose environment carries the run's nonce, with
/// their start times.
fn carriers(nonce: &str) -> Vec<(i32, u64)> {
    let mark = format!("MASIL_DICTATION={nonce}");
    let me = std::process::id() as i32;
    crate::process::all()
        .into_iter()
        .filter(|pid| *pid != me)
        .filter(|pid| {
            crate::process::environment(*pid).is_some_and(|environment| environment.contains(&mark))
        })
        .filter_map(|pid| crate::process::info(pid).map(|info| (pid, info.started)))
        .collect()
}

/// Ends what a dictation left: its group while the recorded leader runs,
/// the noted processes that still run, and every process carrying its
/// nonce. With `wait`, KILL follows TERM 2 s later; without, at once.
fn sweep(record: &Record, wait: bool) {
    let group = record.group.filter(|group| {
        *group > 1
            && record
                .leader_started
                .is_some_and(|started| same(*group, started))
    });
    let mut left: Vec<(i32, u64)> = if record.nonce.is_empty() {
        Vec::new()
    } else {
        carriers(&record.nonce)
    };
    for noted in still(&record.tree) {
        if !left.contains(&noted) {
            left.push(noted);
        }
    }
    if group.is_none() && left.is_empty() {
        return;
    }
    // SAFETY: the group's recorded leader still runs, and each process is
    // the one noted or carries this run's nonce (start times checked).
    if let Some(group) = group {
        unsafe { libc::killpg(group, libc::SIGTERM) };
    }
    for (pid, _) in &left {
        unsafe { libc::kill(*pid, libc::SIGTERM) };
    }
    // SAFETY: signal 0 only checks.
    let group_left = || group.is_some_and(|group| unsafe { libc::killpg(group, 0) } == 0);
    if wait {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline
            && (group_left() || left.iter().any(|(pid, started)| same(*pid, *started)))
        {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    if let Some(group) = group
        && group_left()
    {
        unsafe { libc::killpg(group, libc::SIGKILL) };
    }
    for (pid, started) in left {
        if same(pid, started) {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

/// The running command's group, nonce and noted processes, for the panic
/// hook.
static GROUP: AtomicI32 = AtomicI32::new(0);
static NONCE: OnceLock<String> = OnceLock::new();
static TREE: Mutex<Vec<(i32, u64)>> = Mutex::new(Vec::new());

/// For the panic hook of a `dictate` process.
fn sweep_on_panic() {
    let group = GROUP.load(Ordering::SeqCst);
    if group > 1 {
        // SAFETY: set only while the run's group exists.
        unsafe { libc::killpg(group, libc::SIGKILL) };
    }
    let mut pids = NONCE.get().map(|nonce| carriers(nonce)).unwrap_or_default();
    if let Ok(tree) = TREE.lock() {
        pids.extend(still(&tree));
    }
    for (pid, started) in pids {
        if same(pid, started) {
            // SAFETY: the process is the one noted or carries this nonce.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

fn usage() -> String {
    "usage: masil-agent agent [--client CLIENT] dictate TARGET | dictate --toggle TARGET | dictate --stop | dictate --cancel | dictate --status".into()
}

/// What `dictate` listens for: a stop (SIGINT, or SIGUSR1 after a stop
/// file), or a discard (SIGQUIT, SIGTERM, SIGHUP).
struct Signals {
    interrupt: Signal,
    asked: Signal,
    quit: Signal,
    terminate: Signal,
    hangup: Signal,
}

impl Signals {
    fn new() -> Result<Self, String> {
        let make = |kind| signal(kind).map_err(|error: std::io::Error| error.to_string());
        let signals = Self {
            interrupt: make(SignalKind::interrupt())?,
            asked: make(SignalKind::user_defined1())?,
            quit: make(SignalKind::quit())?,
            terminate: make(SignalKind::terminate())?,
            hangup: make(SignalKind::hangup())?,
        };
        // Ctrl-Z would stop the clock while the microphone stays on.
        for stop in [libc::SIGTSTP, libc::SIGTTIN, libc::SIGTTOU] {
            // SAFETY: setting a disposition has no preconditions.
            unsafe { libc::signal(stop, libc::SIG_IGN) };
        }
        Ok(signals)
    }

    /// Whether a discard arrived and is not yet taken.
    async fn discarded(&mut self) -> bool {
        let mut any = false;
        for signal in [&mut self.quit, &mut self.terminate, &mut self.hangup] {
            any |= pending(signal).await;
        }
        any
    }
}

async fn pending(signal: &mut Signal) -> bool {
    matches!(
        tokio::time::timeout(Duration::ZERO, signal.recv()).await,
        Ok(Some(()))
    )
}

/// `agent dictate`. Runs outside the CLI's cancellable wrapper: SIGINT
/// stops a dictation (its text is still queued) instead of dropping it.
/// Stopping and looking need no server.
pub(super) fn run(
    socket: Option<PathBuf>,
    client: Option<String>,
    args: &[String],
) -> Result<i32, String> {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        sweep_on_panic();
        previous(info);
    }));
    let paths = Paths::new()?;
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    let (toggle, target) = match words.as_slice() {
        ["--status"] => return print(&status(&paths)),
        ["--stop"] => return print(&request_stop(&paths, false)),
        ["--cancel"] => return print(&request_stop(&paths, true)),
        ["--toggle", target] => (true, *target),
        [target] if !target.starts_with('-') => (false, *target),
        _ => return Err(usage()),
    };
    let socket = socket
        .ok_or("usage: specify --socket PATH or run this command inside masil")?
        .canonicalize()
        .map_err(|error| format!("server_unreachable: native socket: {error}"))?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    let value = runtime.block_on(async {
        // Before the lock: a stop must find this process listening.
        let mut signals = Signals::new()?;
        match paths.take()? {
            Some(lock) => start(&paths, lock, socket, client, target, &mut signals).await,
            // The lock decides a toggle: held, this one stops it.
            None if toggle => Ok(request_stop(&paths, false)),
            None => Err(
                "dictation_busy: another dictation is running; stop it with dictate --stop".into(),
            ),
        }
    })?;
    print(&value)
}

fn print(value: &Value) -> Result<i32, String> {
    println!(
        "{}",
        serde_json::to_string_pretty(value).map_err(|error| error.to_string())?
    );
    Ok(0)
}

/// What runs now; reads only.
fn status(paths: &Paths) -> Value {
    match paths.read() {
        None => json!({"state": "idle"}),
        Some(record) => json!({
            // One whose owner is gone is cleaned up by the next start or stop.
            "state": if record.owner_runs() { record.state.as_str() } else { "stale" },
            "target": record.target,
            "socket": record.socket,
            "started_ms": record.started_ms,
        }),
    }
}

/// Asks the dictation in progress to stop (its text is queued) or to be
/// cancelled (nothing is queued); cleans up after one whose owner is gone.
fn request_stop(paths: &Paths, cancel: bool) -> Value {
    let record = paths.read();
    let running = record.as_ref().filter(|record| record.owner_runs());
    if running.is_none()
        && let Ok(Some(_lock)) = paths.take()
    {
        if let Some(record) = &record {
            sweep(record, true);
            masil(&record.socket, &["set-option", "-gqu", OPTION]);
        }
        paths.clear();
        return json!({"state": "idle"});
    }
    // An owner runs, or one is starting and has not written its record:
    // it reads this mark once its record is written.
    let nonce = running
        .map(|record| record.nonce.clone())
        .unwrap_or_default();
    let mark = if cancel {
        format!("cancel:{nonce}")
    } else {
        nonce
    };
    let _ = paths.replace(&paths.stop(), mark.as_bytes());
    if let Some(record) = running {
        // SAFETY: the recorded owner still runs (start time checked).
        unsafe { libc::kill(record.pid, libc::SIGUSR1) };
    }
    json!({"state": if cancel { "cancelling" } else { "stopping" }})
}

/// What the stop file asks of the run with `nonce`, if anything.
#[derive(Clone, Copy, PartialEq)]
enum Asked {
    Stop,
    Cancel,
}

fn asked(paths: &Paths, nonce: &str) -> Option<Asked> {
    let text = std::fs::read_to_string(paths.stop()).ok()?;
    let (cancel, mark) = match text.strip_prefix("cancel:") {
        Some(mark) => (true, mark),
        None => (false, text.as_str()),
    };
    if !mark.is_empty() && mark != nonce {
        // For a run before this one.
        let _ = std::fs::remove_file(paths.stop());
        return None;
    }
    Some(if cancel { Asked::Cancel } else { Asked::Stop })
}

fn masil(socket: &Path, args: &[&str]) {
    let _ = crate::coordinator::masil(socket, args);
}

fn tell(socket: &Path, client: Option<&str>, text: &str) {
    let text = format!("masil dictation: {text}").replace('#', "##");
    match client {
        Some(client) => masil(
            socket,
            &["display-message", "-c", client, "-d", "3000", &text],
        ),
        None => masil(socket, &["display-message", "-d", "3000", &text]),
    }
}

/// Who asked for a dictation.
enum Asker {
    /// A tmux client, by name and pid.
    Client(String, i32),
    /// This command, run outside tmux.
    Outside,
    /// Inside tmux, but no client could be named.
    Unknown,
}

impl Asker {
    fn client(&self) -> Option<&str> {
        match self {
            Self::Client(name, _) => Some(name),
            _ => None,
        }
    }
}

/// `--client`, else the most recent client of the session this command
/// runs in, else this command when it runs outside tmux.
fn asker(socket: &Path, client: Option<&str>) -> Result<Asker, String> {
    let listed = |extra: &[&str]| -> Result<Vec<(u64, String, i32)>, String> {
        let mut args = vec!["list-clients"];
        args.extend_from_slice(extra);
        args.extend_from_slice(&["-F", "#{client_activity}\t#{client_name}\t#{client_pid}"]);
        let output = crate::coordinator::masil(socket, &args)?;
        Ok(output
            .lines()
            .filter_map(|line| {
                let mut fields = line.split('\t');
                Some((
                    fields.next()?.parse().ok()?,
                    fields.next()?.to_owned(),
                    fields.next()?.parse().ok()?,
                ))
            })
            .collect())
    };
    if let Some(name) = client {
        return listed(&[])?
            .into_iter()
            .find(|(_, listed, _)| listed == name)
            .map(|(_, name, pid)| Asker::Client(name, pid))
            .ok_or_else(|| format!("invalid_argument: no client {name} is attached"));
    }
    let Ok(pane) = std::env::var("TMUX_PANE") else {
        return Ok(if std::env::var_os("TMUX").is_some() {
            // A run-shell job: its environment is the session's.
            Asker::Unknown
        } else {
            Asker::Outside
        });
    };
    let session = crate::coordinator::masil(
        socket,
        &["display-message", "-p", "-t", &pane, "#{session_id}"],
    )?;
    Ok(listed(&["-t", session.trim()])?
        .into_iter()
        .max_by_key(|(activity, _, _)| *activity)
        .map_or(Asker::Unknown, |(_, name, pid)| Asker::Client(name, pid)))
}

/// Whether a process belongs to a remote login: an SSH variable in its
/// environment, or a remote login server among its ancestors (by name,
/// which can be read for another user's process). On Linux a process whose
/// environment cannot be read counts as remote; macOS shows none for its
/// platform binaries, so there the ancestors decide.
fn remote(pid: i32) -> bool {
    let ssh = |entries: &[String]| {
        entries.iter().any(|entry| {
            ["SSH_CONNECTION=", "SSH_CLIENT=", "SSH_TTY="]
                .iter()
                .any(|name| entry.starts_with(name))
        })
    };
    let environment = if pid == std::process::id() as i32 {
        Some(
            std::env::vars()
                .map(|(name, value)| format!("{name}={value}"))
                .collect::<Vec<_>>(),
        )
    } else {
        crate::process::environment(pid)
    };
    match environment {
        Some(entries) if ssh(&entries) => return true,
        None if cfg!(not(target_os = "macos")) => return true,
        _ => {}
    }
    let mut at = pid;
    for _ in 0..64 {
        if crate::process::name(at).is_some_and(|name| REMOTE_PROGRAMS.contains(&name.as_str())) {
            return true;
        }
        let parent = crate::process::info(at)
            .map(|info| info.parent)
            .or_else(|| crate::process::other_users_parent(at));
        match parent {
            Some(parent) if parent > 1 => at = parent,
            _ => return false,
        }
    }
    false
}

/// Resets what a child inherits: signal dispositions (an ignored SIGINT
/// would make the command's own trap deaf) and the signal mask; on Linux it
/// gets SIGTERM when `dictate` goes, or does not start if it already went.
fn prepare(command: &mut tokio::process::Command) {
    #[cfg(target_os = "linux")]
    // SAFETY: getpid has no preconditions.
    let parent = unsafe { libc::getpid() };
    // SAFETY: between fork and exec only async-signal-safe calls.
    unsafe {
        command.pre_exec(move || {
            for signal in [
                libc::SIGINT,
                libc::SIGQUIT,
                libc::SIGTERM,
                libc::SIGHUP,
                libc::SIGUSR1,
                libc::SIGTSTP,
                libc::SIGTTIN,
                libc::SIGTTOU,
                libc::SIGPIPE,
            ] {
                libc::signal(signal, libc::SIG_DFL);
            }
            let mut empty: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut empty);
            libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut());
            #[cfg(target_os = "linux")]
            {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                if libc::getppid() != parent {
                    return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
                }
            }
            Ok(())
        });
    }
}

/// Reads a stream into `kept` up to `limit` bytes, the rest read and
/// dropped; notifies `over` once past the limit.
async fn collect(
    mut reader: impl tokio::io::AsyncRead + Unpin,
    kept: Arc<Mutex<Vec<u8>>>,
    limit: usize,
    over: Option<Arc<tokio::sync::Notify>>,
) {
    let mut buffer = [0u8; 8192];
    let mut size = 0usize;
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => return,
            Ok(read) => {
                if let Ok(mut kept) = kept.lock() {
                    let room = limit.saturating_sub(kept.len());
                    kept.extend_from_slice(&buffer[..read.min(room)]);
                }
                size += read;
                if size > limit
                    && let Some(over) = &over
                {
                    over.notify_one();
                    return;
                }
            }
        }
    }
}

/// The text as a queue item takes it: no control keys but newline and tab.
fn clean(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_control() && c != '\n' && c != '\t' {
                ' '
            } else {
                c
            }
        })
        .collect::<String>()
        .trim()
        .to_owned()
}

/// Nothing a person would read: only spaces and invisible characters.
fn blank(text: &str) -> bool {
    text.chars().all(crate::api::scope::invisible)
}

#[derive(PartialEq)]
enum Phase {
    Listening,
    Transcribing,
}

/// The dictation as the lock's owner; whatever happens, its processes,
/// record and option go, and the client hears how it ended.
async fn start(
    paths: &Paths,
    lock: std::fs::File,
    socket: PathBuf,
    client: Option<String>,
    target: &str,
    signals: &mut Signals,
) -> Result<Value, String> {
    // A mark left from before this run.
    let _ = std::fs::remove_file(paths.stop());
    let asker = asker(&socket, client.as_deref());
    let told = asker
        .as_ref()
        .ok()
        .and_then(Asker::client)
        .map(str::to_owned);
    let result = match asker {
        Ok(asker) => dictate(paths, &socket, asker, target, signals).await,
        Err(error) => Err(error),
    };
    let left = paths.read();
    if let Some(record) = left.clone() {
        let _ = tokio::task::spawn_blocking(move || sweep(&record, true)).await;
    }
    paths.clear();
    drop(lock);
    masil(&socket, &["set-option", "-gqu", OPTION]);
    // A stale record of another server's, if this one ended before it.
    if let Some(record) = left.filter(|record| record.socket != socket) {
        masil(&record.socket, &["set-option", "-gqu", OPTION]);
    }
    match &result {
        Err(error) => tell(&socket, told.as_deref(), super::failure::human(error)),
        Ok(value) => {
            if let Some(message) = value["message"].as_str() {
                tell(&socket, told.as_deref(), message);
            }
        }
    }
    result
}

async fn dictate(
    paths: &Paths,
    socket: &Path,
    asker: Asker,
    target: &str,
    signals: &mut Signals,
) -> Result<Value, String> {
    let config = load()?;
    match super::answer::agent_ancestor() {
        Ok(false) => {}
        Ok(true) => {
            return Err(
                "dictation_refused: this command runs inside an agent; dictate from a terminal or the management window"
                    .into(),
            );
        }
        Err(_) => {
            return Err("dictation_refused: this command's ancestry could not be checked".into());
        }
    }
    if !config.allow_remote_clients {
        let refused = match &asker {
            Asker::Client(_, pid) => remote(*pid),
            Asker::Outside => remote(std::process::id() as i32),
            Asker::Unknown => true,
        };
        if refused {
            return Err(match asker {
                Asker::Unknown => "dictation_remote: which client asked cannot be told; pass --client (#{client_name} in a key binding)".into(),
                _ => "dictation_remote: the microphone is this machine's and the request came over a remote login; set allow_remote_clients = true in dictation.toml to allow it".into(),
            });
        }
    }
    let client = asker.client().map(str::to_owned);
    let manager = Manager::new(socket.to_owned(), None)?;
    let agent = manager.get(target).await?;
    if !(agent.endpoint_id.is_empty() || agent.endpoint_id == "local") {
        return Err("remote_unsupported: dictation goes to agents of this server".into());
    }
    // A record whose owner went without cleaning up (this one holds the lock).
    if let Some(left) = paths.read() {
        let stale = left.clone();
        let _ = tokio::task::spawn_blocking(move || sweep(&stale, true)).await;
        masil(&left.socket, &["set-option", "-gqu", OPTION]);
        let _ = std::fs::remove_file(paths.record());
    }
    let nonce = nonce()?;
    let _ = NONCE.set(nonce.clone());
    let me = crate::process::info(std::process::id() as i32).map_or(0, |info| info.started);
    let mut record = Record {
        nonce: nonce.clone(),
        pid: std::process::id() as i32,
        started: me,
        socket: socket.to_owned(),
        target: agent.name.clone(),
        run: agent.run.clone(),
        pane: agent.pane_id.clone(),
        client: client.clone(),
        state: "starting".into(),
        started_ms: now_ms(),
        group: None,
        leader_started: None,
        tree: Vec::new(),
    };
    paths.write(&record)?;
    let outcome = |state: &str, message: &str| json!({"state": state, "target": agent.name, "run": agent.run, "queued": Value::Null, "message": message});
    // A stop or a signal that came while this was being set up: the
    // microphone is never turned on.
    let early = asked(paths, &nonce);
    if early == Some(Asked::Cancel) || signals.discarded().await {
        return Ok(outcome("cancelled", "cancelled before it began"));
    }
    if early == Some(Asked::Stop)
        || pending(&mut signals.interrupt).await
        || pending(&mut signals.asked).await
    {
        return Ok(outcome("stopped", "stopped before it began"));
    }
    let mut command = tokio::process::Command::new(&config.command[0]);
    command.args(&config.command[1..]).env_clear();
    for name in [
        "PATH",
        "HOME",
        "USER",
        "LANG",
        "LC_ALL",
        "TMPDIR",
        "XDG_RUNTIME_DIR",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
        .env("MASIL_DICTATION", &nonce)
        .env(
            "MASIL_DICTATION_MAX_SECONDS",
            config.max_seconds.to_string(),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    if let Some(home) = std::env::var_os("HOME") {
        command.current_dir(home);
    }
    prepare(&mut command);
    let mut child = command
        .spawn()
        .map_err(|error| format!("dictation_failed: {}: {error}", config.command[0]))?;
    let group = child.id().and_then(|pid| i32::try_from(pid).ok());
    if let Some(group) = group {
        GROUP.store(group, Ordering::SeqCst);
        record.group = Some(group);
        record.leader_started = crate::process::info(group).map(|info| info.started);
        record.tree = record
            .leader_started
            .map(|started| vec![(group, started)])
            .unwrap_or_default();
    }
    record.state = "listening".into();
    paths.write(&record)?;
    masil(socket, &["set-option", "-gq", OPTION, &agent.name]);
    tell(
        socket,
        client.as_deref(),
        &format!("listening for {}; press the same key to stop", agent.name),
    );
    // Held until the end: its EOF means masil is gone, not "stop".
    let stdin = child.stdin.take();
    let over = Arc::new(tokio::sync::Notify::new());
    let text = Arc::new(Mutex::new(Vec::new()));
    let errors = Arc::new(Mutex::new(Vec::new()));
    let mut out = tokio::spawn(collect(
        child.stdout.take().ok_or("no pipe")?,
        text.clone(),
        super::queue::TEXT_BYTES,
        Some(over.clone()),
    ));
    let mut err = tokio::spawn(collect(
        child.stderr.take().ok_or("no pipe")?,
        errors.clone(),
        STDERR_KEPT,
        None,
    ));
    let mut look = tokio::time::interval(Duration::from_millis(500));
    let mut phase = Phase::Listening;
    let mut deadline = tokio::time::Instant::now() + Duration::from_secs(config.max_seconds);
    let finish = Duration::from_secs(config.finish_seconds);
    let stop = |phase: &mut Phase, deadline: &mut tokio::time::Instant, record: &mut Record| {
        // One SIGINT, once: the command is writing its text after it.
        if *phase == Phase::Listening {
            if let Some(group) = group {
                // SAFETY: the leader is not reaped; the group is the run's.
                unsafe { libc::killpg(group, libc::SIGINT) };
            }
            *phase = Phase::Transcribing;
            *deadline = tokio::time::Instant::now() + finish;
            record.state = "transcribing".into();
            let _ = paths.write(record);
        }
    };
    // Some(Ok(status)) ended, Some(Err) failed in a way of masil's, None discarded.
    let ended: Option<Result<std::process::ExitStatus, String>> = loop {
        tokio::select! {
            status = child.wait() => break Some(status.map_err(|error| error.to_string())),
            _ = tokio::time::sleep_until(deadline) => {
                if phase == Phase::Listening {
                    stop(&mut phase, &mut deadline, &mut record);
                } else {
                    break Some(Err(format!(
                        "dictation_timeout: no text {} s after the stop",
                        config.finish_seconds
                    )));
                }
            }
            _ = signals.interrupt.recv() => stop(&mut phase, &mut deadline, &mut record),
            _ = signals.asked.recv() => match asked(paths, &nonce) {
                Some(Asked::Stop) => stop(&mut phase, &mut deadline, &mut record),
                Some(Asked::Cancel) => break None,
                None => {}
            },
            _ = signals.quit.recv() => break None,
            _ = signals.terminate.recv() => break None,
            _ = signals.hangup.recv() => break None,
            _ = look.tick() => {
                // Note the command's live processes, so none escapes.
                let mut roots: Vec<i32> = still(&record.tree).iter().map(|(pid, _)| *pid).collect();
                roots.extend(group);
                let noted = still(&tree(&roots));
                if noted != record.tree {
                    record.tree = noted;
                    if let Ok(mut shared) = TREE.lock() {
                        shared.clone_from(&record.tree);
                    }
                    let _ = paths.write(&record);
                }
            }
            _ = over.notified() => break Some(Err(format!(
                "dictation_failed: the text is longer than {} bytes",
                super::queue::TEXT_BYTES
            ))),
        }
    };
    drop(stdin);
    // What it left goes too, and the leader if it is still there.
    if let Some(group) = group {
        crate::api::end_group(group, &mut child).await;
        GROUP.store(0, Ordering::SeqCst);
    }
    {
        let left = record.clone();
        let _ = tokio::task::spawn_blocking(move || sweep(&left, true)).await;
    }
    // A helper outside the group may still hold the pipes.
    let grace = tokio::time::Instant::now() + OUTPUT_GRACE;
    if tokio::time::timeout_at(grace, &mut out).await.is_err() {
        out.abort();
    }
    if tokio::time::timeout_at(grace, &mut err).await.is_err() {
        err.abort();
    }
    let text = text.lock().map(|text| text.clone()).unwrap_or_default();
    let stderr = errors
        .lock()
        .map(|errors| errors.clone())
        .unwrap_or_default();
    let failed = |message: String| -> Result<Value, String> {
        // Only a failed run's error output is kept, and only its end: a
        // command may print what it heard there.
        let tail = &stderr[stderr.len().saturating_sub(STDERR_LOGGED)..];
        paths.log(&format!(
            "{} {message}\n{}\n",
            now_ms(),
            String::from_utf8_lossy(tail).trim_end()
        ));
        Err(message)
    };
    let status = match ended {
        None => return Ok(outcome("cancelled", "cancelled; nothing was queued")),
        Some(Err(message)) => return failed(message),
        Some(Ok(status)) => status,
    };
    if !status.success() {
        return failed(format!("dictation_failed: the command ended with {status}"));
    }
    // A discard that came after the command ended still discards.
    if signals.discarded().await || asked(paths, &nonce) == Some(Asked::Cancel) {
        return Ok(outcome("cancelled", "cancelled; nothing was queued"));
    }
    let Ok(text) = String::from_utf8(text) else {
        return failed("dictation_failed: the text is not UTF-8".into());
    };
    let text = clean(&text);
    if blank(&text) {
        return Ok(outcome("empty", "nothing was heard"));
    }
    // The run this dictation began for, alive or not, in this server's
    // store: a run that ended keeps the item held for the person to move
    // (`queue add --from`).
    let mut store = manager.operation_store().await?;
    let item = match store.queue_add(
        &agent.run,
        &agent.pane_id,
        &agent.provider,
        &text,
        &json!([]),
        now_ms(),
    ) {
        Ok(item) => item,
        Err(error) => return failed(error),
    };
    let (held, checked) = match manager.get(&agent.pane_id).await {
        Ok(current) => (current.run != agent.run, true),
        Err(error) if error.starts_with("target_absent") => (true, true),
        Err(_) => (false, false),
    };
    let command_like = crate::api::scope::begins_like_command(&text);
    let mut message = if held {
        format!(
            "{} ended; the text is held (agent queue --held)",
            agent.name
        )
    } else {
        format!("queued as #{} for {}", item.id, agent.name)
    };
    if command_like {
        message.push_str("; it begins like a command, check it before sending");
    }
    Ok(json!({
        "state": if held { "held" } else { "queued" },
        "target": agent.name,
        "run": agent.run,
        "queued": super::queue::public(&item, held),
        "run_checked": checked,
        "text_bytes": text.len(),
        "command_like": command_like,
        "message": message,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_keeps_lines_and_loses_control_keys() {
        assert_eq!(clean("  hello\u{7}\nworld\t!  \n"), "hello \nworld\t!");
        assert_eq!(clean(" \n "), "");
        assert_eq!(clean("\u{1b}"), "");
        assert!(blank("\u{FEFF} \u{200B}"));
        assert!(!blank("ok"));
    }
}
