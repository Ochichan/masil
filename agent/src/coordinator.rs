//! The coordinator: at most one long-lived masil-agent process per masil
//! server boot, for work that outlives a single command. Queries never start
//! it. Mutating commands start it through the server's `run-shell -b`, so it
//! is a server job: it receives SIGTERM when the server exits, does not keep
//! the server alive, and inherits none of the calling pane's environment.
//!
//! Identity is (canonical server socket, server boot, state directory). The
//! state directory belongs to the server (its global environment), so one
//! server has one coordinator whichever client asks.

use crate::ipc;
use serde_json::{Value, json};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Inbox writes reported to this process since it started.
static POKES: AtomicUsize = AtomicUsize::new(0);

/// Protocol and feature generation; a client replaces an older coordinator.
/// 2: features, `reload`, the screen watch and the executable identity.
pub(crate) const GENERATION: u64 = 2;
const REQUEST_FRAME: usize = 8 * 1024;
const RESPONSE_FRAME: usize = 64 * 1024;
const CLIENT_LIMIT: usize = 32;
const DEADLINE: Duration = Duration::from_secs(3);
const DEFAULT_IDLE: Duration = Duration::from_secs(600);
const DEFAULT_WATCH: Duration = Duration::from_secs(2);
/// The server option the status line shows the unseen inbox count from.
pub(crate) const BADGE_OPTION: &str = "@masil-inbox-unseen";
/// Features, the executable and the store are checked this often.
const SUPERVISE_EVERY: Duration = Duration::from_secs(30);
const HELLO_TIMEOUT: Duration = Duration::from_millis(500);
const SPAWN_WAIT: Duration = Duration::from_secs(2);
const LOCK_RETRY: Duration = Duration::from_secs(1);
const NUDGE_WAIT: Duration = Duration::from_secs(1);
const LOG_LIMIT: u64 = 1 << 20;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(3);
const OUTPUT_LIMIT: u64 = 256 * 1024;
/// The environment kept from the job; everything else is dropped, since the
/// server's job environment is that of the client that started the server.
const ENV_ALLOW: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "LANG",
    "TMPDIR",
    "TZ",
    "XDG_CONFIG_HOME",
    "XDG_STATE_HOME",
    "XDG_DATA_HOME",
    "XDG_RUNTIME_DIR",
    "SSH_AUTH_SOCK",
];

// ------------------------------------------------------------- paths & server

pub(crate) struct Paths {
    /// The canonical server socket.
    pub(crate) server: PathBuf,
    pub(crate) lock: PathBuf,
    pub(crate) listen: PathBuf,
    pub(crate) log: PathBuf,
}

fn digest16(socket: &Path) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(socket.as_os_str().as_bytes())[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn sun_path_limit() -> usize {
    // SAFETY: a zeroed sockaddr_un is a valid value of the type.
    let address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_path.len()
}

pub(crate) fn paths(socket: &Path, state: &Path) -> Result<Paths, String> {
    let server = socket
        .canonicalize()
        .map_err(|error| format!("server_unreachable: native socket: {error}"))?;
    let name = digest16(&server);
    let directory = server
        .parent()
        .ok_or("server_unreachable: native socket has no directory")?;
    let listen = directory.join(format!(".agentd-{name}.sock"));
    // The path and its terminating NUL must fit sun_path.
    if listen.as_os_str().len() >= sun_path_limit() {
        return Err(format!(
            "coordinator_socket_path_too_long: {} does not fit a Unix socket address",
            listen.display()
        ));
    }
    let agentd = state.join("masil/agentd");
    Ok(Paths {
        lock: agentd.join(format!("{name}.lock")),
        log: agentd.join(format!("{name}.log")),
        listen,
        server,
    })
}

pub(crate) struct ServerInfo {
    pub(crate) pid: libc::pid_t,
    pub(crate) boot: String,
    pub(crate) state: PathBuf,
}

/// Runs one native command with a time limit and returns standard output.
fn masil(socket: &Path, args: &[&str]) -> Result<String, String> {
    let binary = crate::native_ui::native_executable()?;
    let mut child = Command::new(binary)
        .arg("-u")
        .arg("-S")
        .arg(socket)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("server_unreachable: cannot run masil: {error}"))?;
    let mut stdout = child.stdout.take().ok_or("masil output unavailable")?;
    let mut stderr = child.stderr.take().ok_or("masil output unavailable")?;
    let out = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = (&mut stdout).take(OUTPUT_LIMIT).read_to_string(&mut text);
        text
    });
    let err = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = (&mut stderr).take(OUTPUT_LIMIT).read_to_string(&mut text);
        text
    });
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err("server_unreachable: masil did not answer".into());
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let out = out.join().unwrap_or_default();
    let err = err.join().unwrap_or_default();
    if status.success() {
        Ok(out)
    } else {
        Err(format!(
            "server_unreachable: {}",
            err.lines().next().unwrap_or("masil command failed").trim()
        ))
    }
}

/// The server's pid, boot and state directory. The state directory comes
/// from the server's global environment with the same rule as the
/// operation store: absolute XDG_STATE_HOME, else HOME/.local/state.
pub(crate) fn server_info(socket: &Path) -> Result<ServerInfo, String> {
    let text = masil(
        socket,
        &[
            "display-message",
            "-p",
            "#{pid} #{masil_core_boot_id}",
            ";",
            "show-environment",
            "-g",
        ],
    )?;
    let mut lines = text.lines();
    let first = lines.next().unwrap_or_default();
    let (pid, boot) = first
        .split_once(' ')
        .ok_or("server_unreachable: unexpected server identity")?;
    let pid: libc::pid_t = pid
        .parse()
        .map_err(|_| "server_unreachable: unexpected server pid")?;
    if boot.is_empty() {
        return Err("server_unreachable: the server has no boot id".into());
    }
    let mut xdg = None;
    let mut home = None;
    for line in lines {
        if let Some(value) = line.strip_prefix("XDG_STATE_HOME=") {
            xdg = Some(PathBuf::from(value));
        } else if let Some(value) = line.strip_prefix("HOME=") {
            home = Some(PathBuf::from(value));
        }
    }
    let state = xdg
        .filter(|path| path.is_absolute())
        .or_else(|| {
            home.filter(|path| path.is_absolute())
                .map(|home| home.join(".local/state"))
        })
        .ok_or("server_unreachable: the server has no absolute XDG_STATE_HOME or HOME")?;
    Ok(ServerInfo {
        pid,
        boot: boot.to_owned(),
        state,
    })
}

fn private_directory(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(format!("{} is not a directory you own", path.display()));
    }
    if metadata.mode() & 0o077 != 0 {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------- lock

fn open_lock(path: &Path, create: bool) -> Result<Option<File>, String> {
    let file = match OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if !create && error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("coordinator lock: {error}")),
    };
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err("coordinator lock is not a private owner file".into());
    }
    Ok(Some(file))
}

fn try_flock(file: &File) -> Result<bool, String> {
    // SAFETY: flock on a valid descriptor.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::WouldBlock {
        Ok(false)
    } else {
        Err(format!("coordinator lock: {error}"))
    }
}

/// Whether the open file is still the one at `path`; a lock on a replaced
/// file protects nothing.
fn same_file(file: &File, path: &Path) -> bool {
    match (file.metadata(), fs::symlink_metadata(path)) {
        (Ok(open), Ok(named)) => open.dev() == named.dev() && open.ino() == named.ino(),
        _ => false,
    }
}

/// Takes the lock, retrying for `wait` while another process holds it.
fn acquire(path: &Path, wait: Duration) -> Result<Option<File>, String> {
    let deadline = Instant::now() + wait;
    loop {
        if let Some(file) = open_lock(path, true)?
            && try_flock(&file)?
            && same_file(&file, path)
        {
            return Ok(Some(file));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Whether some process holds the lock now.
fn lock_held(path: &Path) -> Result<bool, String> {
    let Some(file) = open_lock(path, false)? else {
        return Ok(false);
    };
    // Taking it for a moment proves nobody holds it; dropping releases it.
    Ok(!try_flock(&file)?)
}

fn holder(path: &Path) -> Option<Value> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

// -------------------------------------------------------------------- client

fn request(listen: &Path, method: &str, timeout: Duration) -> Result<Value, String> {
    let mut stream = StdUnixStream::connect(listen)
        .map_err(|error| format!("coordinator_unavailable: {error}"))?;
    stream
        .set_read_timeout(Some(timeout))
        .and_then(|()| stream.set_write_timeout(Some(timeout)))
        .map_err(|error| error.to_string())?;
    let body = serde_json::to_vec(&json!({"v": 1, "id": "cli", "method": method}))
        .map_err(|error| error.to_string())?;
    stream
        .write_all(&(body.len() as u32).to_be_bytes())
        .and_then(|()| stream.write_all(&body))
        .map_err(|error| format!("coordinator_unavailable: {error}"))?;
    let mut header = [0; 4];
    stream
        .read_exact(&mut header)
        .map_err(|error| format!("coordinator_unavailable: {error}"))?;
    let length = u32::from_be_bytes(header) as usize;
    if length == 0 || length > RESPONSE_FRAME {
        return Err("coordinator_unavailable: invalid response frame".into());
    }
    let mut body = vec![0; length];
    stream
        .read_exact(&mut body)
        .map_err(|error| format!("coordinator_unavailable: {error}"))?;
    let response: Value =
        serde_json::from_slice(&body).map_err(|_| "coordinator_unavailable: invalid response")?;
    if response["ok"] == true {
        Ok(response["value"].clone())
    } else {
        Err(format!(
            "{}: {}",
            response["error"]["code"]
                .as_str()
                .unwrap_or("coordinator_error"),
            response["error"]["message"]
                .as_str()
                .unwrap_or("request failed")
        ))
    }
}

fn hello(listen: &Path) -> Option<Value> {
    request(listen, "hello", HELLO_TIMEOUT).ok()
}

enum Probe {
    Answer(Value),
    /// A live coordinator replied with an error, such as being busy.
    Busy,
    /// No reply: nothing accepted the connection, or the reply did not come.
    Silent,
}

fn probe(listen: &Path, timeout: Duration) -> Probe {
    match request(listen, "hello", timeout) {
        Ok(value) => Probe::Answer(value),
        Err(error) if !error.starts_with("coordinator_unavailable") => Probe::Busy,
        Err(_) => Probe::Silent,
    }
}

fn wait_unlocked(lock: &Path, limit: Duration) -> Result<bool, String> {
    let deadline = Instant::now() + limit;
    while lock_held(lock)? {
        if Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(true)
}

fn signal_holder(lock: &Path, signal: libc::c_int) -> bool {
    let Some(pid) = holder(lock)
        .and_then(|record| record["pid"].as_i64())
        .and_then(|pid| libc::pid_t::try_from(pid).ok())
        .filter(|pid| *pid > 1)
    else {
        return false;
    };
    // SAFETY: kill with a positive pid of a process recorded by this user.
    unsafe { libc::kill(pid, signal) == 0 }
}

fn current(value: &Value, server: &ServerInfo) -> bool {
    value["boot"] == server.boot.as_str()
        && value["server_pid"].as_i64() == Some(i64::from(server.pid))
        && value["generation"]
            .as_u64()
            .is_some_and(|g| g >= GENERATION)
        && exe_unchanged(&value["exe"])
}

/// The executable this process runs, as a path and the file's identity, so
/// a rebuilt binary is noticed and its coordinator replaced.
fn exe_identity() -> Value {
    let Ok(path) = std::env::current_exe().and_then(|path| path.canonicalize()) else {
        return Value::Null;
    };
    match fs::metadata(&path) {
        Ok(metadata) => json!({
            "path": path,
            "dev": metadata.dev(),
            "ino": metadata.ino(),
            "size": metadata.size(),
            "mtime_ns": metadata.mtime() as i128 * 1_000_000_000 + metadata.mtime_nsec() as i128,
        }),
        Err(_) => Value::Null,
    }
}

/// Whether the file a coordinator recorded as its executable is still the
/// same file. An unknown identity counts as changed.
fn exe_unchanged(recorded: &Value) -> bool {
    let Some(path) = recorded["path"].as_str() else {
        return false;
    };
    fs::metadata(path).is_ok_and(|metadata| {
        recorded["dev"].as_u64() == Some(metadata.dev())
            && recorded["ino"].as_u64() == Some(metadata.ino())
            && recorded["size"].as_u64() == Some(metadata.size())
            && recorded["mtime_ns"].as_i64().map(i128::from)
                == Some(metadata.mtime() as i128 * 1_000_000_000 + metadata.mtime_nsec() as i128)
    })
}

/// Reports the coordinator for `socket` without starting, stopping or
/// replacing anything.
pub(crate) fn status(socket: &Path) -> Result<Value, String> {
    let server = server_info(socket)?;
    let paths = match paths(socket, &server.state) {
        Ok(paths) => paths,
        Err(error) if error.starts_with("coordinator_socket_path_too_long") => {
            return Ok(json!({"state": "socket_path_too_long", "detail": error}));
        }
        Err(error) => return Err(error),
    };
    let features =
        crate::managed::coordinator_features(&server.state, &paths.server).unwrap_or_default();
    let mut report = match request(&paths.listen, "status", HELLO_TIMEOUT) {
        Ok(value) if current(&value, &server) => json!({"state": "running", "coordinator": value}),
        Ok(value) => json!({"state": "stale", "coordinator": value}),
        Err(_) if lock_held(&paths.lock)? => {
            json!({"state": "unresponsive", "holder": holder(&paths.lock)})
        }
        Err(_) if !features.is_empty() => json!({"state": "enabled_but_not_running"}),
        Err(_) => json!({"state": "not_running"}),
    };
    report["server_boot"] = json!(server.boot);
    report["state_dir"] = json!(server.state);
    report["features"] = json!(features);
    report["socket"] = json!(paths.listen);
    Ok(report)
}

/// The server, if this process uses its state directory; a command with
/// another one would act on a store the server never reads.
pub(crate) fn check_state(socket: &Path) -> Result<ServerInfo, String> {
    let server = server_info(socket)?;
    let own = crate::managed::state_base()?;
    let same_state = match (own.canonicalize(), server.state.canonicalize()) {
        (Ok(own), Ok(theirs)) => own == theirs,
        _ => own == server.state,
    };
    if !same_state {
        return Err(format!(
            "coordinator_state_mismatch: this command uses {} but the server uses {}; run it with the server's XDG_STATE_HOME and HOME",
            own.display(),
            server.state.display()
        ));
    }
    Ok(server)
}

/// Starts the coordinator for `socket` unless a current one answers. Only
/// mutating commands call this. A coordinator of an earlier server boot or
/// an older generation is stopped and replaced; a stuck one is nudged to
/// rebind its socket, then terminated.
pub(crate) fn ensure(socket: &Path) -> Result<Value, String> {
    let server = check_state(socket)?;
    let paths = paths(socket, &server.state)?;
    private_directory(
        paths
            .lock
            .parent()
            .ok_or("coordinator lock has no directory")?,
    )?;
    match hello(&paths.listen) {
        Some(value) if current(&value, &server) => return Ok(value),
        Some(_) => {
            let _ = request(&paths.listen, "stop", HELLO_TIMEOUT);
            if !wait_unlocked(&paths.lock, SPAWN_WAIT)? {
                signal_holder(&paths.lock, libc::SIGTERM);
                wait_unlocked(&paths.lock, SPAWN_WAIT)?;
            }
        }
        None if lock_held(&paths.lock)? => {
            // A holder that has not recorded its boot is still starting.
            if holder(&paths.lock).is_none_or(|record| record.get("boot").is_none()) {
                let deadline = Instant::now() + SPAWN_WAIT + COMMAND_TIMEOUT;
                while Instant::now() < deadline {
                    if let Some(value) = hello(&paths.listen).filter(|v| current(v, &server)) {
                        return Ok(value);
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
            let same_boot =
                holder(&paths.lock).is_some_and(|record| record["boot"] == server.boot.as_str());
            if same_boot && signal_holder(&paths.lock, libc::SIGUSR1) {
                // The holder rebinds a socket that was removed under it.
                let deadline = Instant::now() + NUDGE_WAIT;
                while Instant::now() < deadline {
                    if let Some(value) = hello(&paths.listen).filter(|v| current(v, &server)) {
                        return Ok(value);
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
            match probe(&paths.listen, COMMAND_TIMEOUT) {
                Probe::Answer(value) if current(&value, &server) => return Ok(value),
                // Alive but busy: never terminate a working process.
                Probe::Busy => {
                    return Err("coordinator_unavailable: the coordinator is busy; retry".into());
                }
                _ => {}
            }
            signal_holder(&paths.lock, libc::SIGTERM);
            wait_unlocked(&paths.lock, SPAWN_WAIT)?;
        }
        None => {}
    }
    // One spawner at a time: callers that arrive together wait here and
    // find the coordinator the first one started.
    let mut spawn_lock = paths.lock.as_os_str().to_owned();
    spawn_lock.push(".spawn");
    let _spawning = acquire(Path::new(&spawn_lock), SPAWN_WAIT + LOCK_RETRY)?;
    if let Some(value) = hello(&paths.listen).filter(|v| current(v, &server)) {
        return Ok(value);
    }
    spawn(socket, &paths, &server)?;
    let deadline = Instant::now() + SPAWN_WAIT;
    loop {
        if let Some(value) = hello(&paths.listen).filter(|v| current(v, &server)) {
            return Ok(value);
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "coordinator_unavailable: the coordinator did not start; see {}",
                paths.log.display()
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn idle_seconds() -> u64 {
    std::env::var("MASIL_AGENTD_IDLE_SECONDS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|seconds| (1..=3600).contains(seconds))
        .unwrap_or(0)
}

/// The quiet watch tick in milliseconds; 0 keeps the default. For tests.
fn watch_ms() -> u64 {
    std::env::var("MASIL_AGENTD_WATCH_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|ms| (100..=60_000).contains(ms))
        .unwrap_or(0)
}

fn spawn(socket: &Path, paths: &Paths, server: &ServerInfo) -> Result<(), String> {
    let agent = std::env::current_exe()
        .map(|path| {
            // Linux names a replaced executable "<path> (deleted)"; the new
            // one at the same path is the one to start.
            match path
                .to_str()
                .and_then(|text| text.strip_suffix(" (deleted)"))
            {
                Some(text) => PathBuf::from(text),
                None => path,
            }
        })
        .and_then(|path| path.canonicalize())
        .map_err(|error| format!("locating masil-agent: {error}"))?;
    let text = |path: &Path, what: &str| -> Result<String, String> {
        let text = path
            .to_str()
            .ok_or_else(|| format!("invalid_argument: the {what} path is not UTF-8"))?;
        // tmux ends a command at an argument that ends in ';'.
        if text.ends_with(';') {
            return Err(format!("invalid_argument: the {what} path ends in ';'"));
        }
        Ok(text.to_owned())
    };
    let agent = text(&agent, "masil-agent")?;
    let server_socket = text(&paths.server, "server socket")?;
    let state = text(&server.state, "state")?;
    let idle = idle_seconds().to_string();
    let watch = watch_ms().to_string();
    // Values arrive as run-shell arguments, which tmux substitutes without
    // expanding them again; q/s quotes each for the shell. A missing binary
    // exits 0, because a non-zero job status is printed into the pane.
    let script = "[ -x #{q/s:1} ] || exit 0; exec #{q/s:1} agentd --socket #{q/s:2} --state-dir #{q/s:3} --idle-seconds #{q/s:4} --watch-ms #{q/s:5} </dev/null >/dev/null 2>&1";
    masil(
        socket,
        &[
            "run-shell",
            "-b",
            script,
            &agent,
            &server_socket,
            &state,
            &idle,
            &watch,
        ],
    )
    .map(drop)
}

/// Starts the coordinator without waiting, unless one answers or holds the
/// lock (starting or stuck). For commands that must not wait on it.
pub(crate) fn spawn_detached(socket: &Path) {
    let Ok(server) = check_state(socket) else {
        return;
    };
    let Ok(paths) = paths(socket, &server.state) else {
        return;
    };
    if hello(&paths.listen).is_some() || lock_held(&paths.lock).unwrap_or(true) {
        return;
    }
    if paths
        .lock
        .parent()
        .is_some_and(|directory| private_directory(directory).is_ok())
    {
        let _ = spawn(socket, &paths, &server);
    }
}

/// Asks a running coordinator to read its features and detection manifests
/// again. Never starts one; failures are ignored.
pub(crate) fn reload(socket: &Path) {
    let Ok(state) = crate::managed::state_base() else {
        return;
    };
    if let Ok(paths) = paths(socket, &state) {
        let _ = request(&paths.listen, "reload", HELLO_TIMEOUT);
    }
}

/// Tells a running coordinator that the inbox changed. Never starts one,
/// waits at most 50 ms and ignores every failure.
pub(crate) fn poke(socket: &Path) {
    signal(socket, "poke");
}

/// Tells a running coordinator that events were read, so only its badge
/// count can have changed. As `poke`, never starts one.
pub(crate) fn recount(socket: &Path) {
    signal(socket, "recount");
}

fn signal(socket: &Path, method: &str) {
    let Ok(state) = crate::managed::state_base() else {
        return;
    };
    let Ok(paths) = paths(socket, &state) else {
        return;
    };
    let Ok(mut stream) = StdUnixStream::connect(&paths.listen) else {
        return;
    };
    let timeout = Some(Duration::from_millis(50));
    if stream.set_write_timeout(timeout).is_err() || stream.set_read_timeout(timeout).is_err() {
        return;
    }
    let body = json!({"v": 1, "id": method, "method": method}).to_string();
    let _ = stream
        .write_all(&(body.len() as u32).to_be_bytes())
        .and_then(|()| stream.write_all(body.as_bytes()));
}

/// Stops a running coordinator for `socket`; reports `not_running` if none.
pub(crate) fn stop(socket: &Path) -> Result<Value, String> {
    let server = server_info(socket)?;
    let paths = paths(socket, &server.state)?;
    if request(&paths.listen, "stop", HELLO_TIMEOUT).is_err() && !lock_held(&paths.lock)? {
        return Ok(json!({"state": "not_running"}));
    }
    if !wait_unlocked(&paths.lock, SPAWN_WAIT)? {
        signal_holder(&paths.lock, libc::SIGTERM);
        if !wait_unlocked(&paths.lock, SPAWN_WAIT)? {
            return Err("coordinator_unavailable: the coordinator did not stop".into());
        }
    }
    Ok(json!({"state": "stopped"}))
}

/// For autosave while the inbox is on: brings back a coordinator that ended
/// or runs a replaced executable. A current one costs one hello and no
/// process; a store that a newer masil-agent upgraded starts nothing.
pub(crate) fn revive(socket: &Path) {
    let Ok(state) = crate::managed::state_base() else {
        return;
    };
    let Ok(paths) = paths(socket, &state) else {
        return;
    };
    let answering = hello(&paths.listen).is_some_and(|value| {
        value["generation"]
            .as_u64()
            .is_some_and(|generation| generation >= GENERATION)
            && exe_unchanged(&value["exe"])
    });
    if !answering {
        restart_if_enabled(socket);
    }
}

/// Starts the coordinator after a server (re)start when a feature that needs
/// it is switched on. Called by session autosave; creates nothing and
/// starts nothing otherwise.
pub(crate) fn restart_if_enabled(socket: &Path) {
    // Autosave runs in the server's environment, so its own state directory
    // is the server's; no masil process starts unless a feature is on.
    let enabled = crate::managed::state_base()
        .and_then(|base| crate::managed::coordinator_features(&base, socket))
        .is_ok_and(|features| !features.is_empty());
    if enabled {
        let _ = ensure(socket);
    }
}

// -------------------------------------------------------------------- server

struct Options {
    socket: PathBuf,
    state: PathBuf,
    idle: Duration,
    watch: Duration,
}

fn parse(args: &[String]) -> Result<Options, String> {
    let mut socket = None;
    let mut state = None;
    let mut idle = 0u64;
    let mut watch = 0u64;
    let mut index = 0;
    while index < args.len() {
        let value = args.get(index + 1).ok_or("missing value")?;
        match args[index].as_str() {
            "--socket" => socket = Some(PathBuf::from(value)),
            "--state-dir" => state = Some(PathBuf::from(value)),
            "--idle-seconds" => idle = value.parse().map_err(|_| "invalid idle seconds")?,
            "--watch-ms" => watch = value.parse().map_err(|_| "invalid watch interval")?,
            _ => {
                return Err(
                    "usage: masil-agent agentd --socket S --state-dir DIR --idle-seconds N [--watch-ms N]"
                        .into(),
                );
            }
        }
        index += 2;
    }
    let socket = socket
        .filter(|path| path.is_absolute())
        .ok_or("--socket must be absolute")?;
    let state = state
        .filter(|path| path.is_absolute())
        .ok_or("--state-dir must be absolute")?;
    let idle = if (1..=3600).contains(&idle) {
        Duration::from_secs(idle)
    } else {
        DEFAULT_IDLE
    };
    let watch = if (100..=60_000).contains(&watch) {
        Duration::from_millis(watch)
    } else {
        DEFAULT_WATCH
    };
    Ok(Options {
        socket,
        state,
        idle,
        watch,
    })
}

fn log(path: &Path, message: &str) {
    if fs::metadata(path).is_ok_and(|metadata| metadata.len() > LOG_LIMIT) {
        let mut old = path.as_os_str().to_owned();
        old.push(".1");
        let _ = fs::rename(path, PathBuf::from(old));
    }
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
    {
        let _ = writeln!(file, "{} pid {} {message}", now_ms(), std::process::id());
    }
}

/// Keeps only the allowed environment. Runs before any thread exists.
fn rebuild_environment() {
    let drop: Vec<_> = std::env::vars_os()
        .map(|(name, _)| name)
        .filter(|name| {
            let text = name.to_string_lossy();
            !ENV_ALLOW.contains(&text.as_ref()) && !text.starts_with("LC_")
        })
        .collect();
    for name in drop {
        // SAFETY: called before the runtime or any other thread starts.
        unsafe { std::env::remove_var(name) };
    }
    let _ = std::env::set_current_dir("/");
}

fn raise_file_limit() -> usize {
    // SAFETY: getrlimit/setrlimit with a valid struct.
    unsafe {
        let mut limit: libc::rlimit = std::mem::zeroed();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) != 0 {
            return CLIENT_LIMIT;
        }
        let wanted = limit.rlim_max.min(4096);
        if limit.rlim_cur < wanted {
            let raised = libc::rlimit {
                rlim_cur: wanted,
                rlim_max: limit.rlim_max,
            };
            if libc::setrlimit(libc::RLIMIT_NOFILE, &raised) == 0 {
                limit.rlim_cur = wanted;
            }
        }
        (limit.rlim_cur as usize)
            .saturating_sub(32)
            .clamp(1, CLIENT_LIMIT)
    }
}

/// The coordinator process. Every path ends with status 0: a non-zero
/// run-shell job status is printed into the user's pane.
pub(crate) fn run(args: &[String]) -> Result<i32, String> {
    // Signals that arrive before the runtime handles them are ignored rather
    // than killing the process; the parent watch covers a server that exits
    // in that window.
    for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP, libc::SIGUSR1] {
        // SAFETY: installing SIG_IGN has no preconditions.
        unsafe { libc::signal(signal, libc::SIG_IGN) };
    }
    let Ok(options) = parse(args) else {
        return Ok(0);
    };
    let Ok(paths) = paths(&options.socket, &options.state) else {
        return Ok(0);
    };
    if paths
        .lock
        .parent()
        .is_none_or(|directory| private_directory(directory).is_err())
    {
        return Ok(0);
    }
    let log_path = paths.log.clone();
    std::panic::set_hook(Box::new(move |info| {
        log(&log_path, &format!("panic: {info}"));
        std::process::exit(0);
    }));
    rebuild_environment();
    let limit = raise_file_limit();
    if let Err(error) = serve(options, &paths, limit) {
        log(&paths.log, &error);
    }
    Ok(0)
}

fn serve(options: Options, paths: &Paths, limit: usize) -> Result<(), String> {
    let Some(lock) = acquire(&paths.lock, LOCK_RETRY)? else {
        return Ok(());
    };
    // The record names this process from the moment it holds the lock, so no
    // caller signals a previous holder's pid, which may have been reused.
    let started = now_ms();
    write_starting(&lock, started)?;
    let result = serve_locked(&options, paths, limit, &lock, started);
    clear_record(&lock);
    drop(lock);
    result
}

fn serve_locked(
    options: &Options,
    paths: &Paths,
    limit: usize,
    lock: &File,
    started: u64,
) -> Result<(), String> {
    let server = server_info(&options.socket)?;
    if server.state != options.state {
        return Err(format!(
            "the server's state directory is now {}, not {}",
            server.state.display(),
            options.state.display()
        ));
    }
    // SAFETY: getppid has no preconditions.
    if unsafe { libc::getppid() } != server.pid {
        return Err("not started by the server's run-shell; exiting".into());
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    runtime.block_on(async {
        let mut signals = Signals::new()?;
        watch_parent(server.pid)?;
        let (listener, guard) = bind(&paths.listen)?;
        write_record(lock, &server, started)?;
        let shared = Arc::new(Shared {
            identity: json!({
                "generation": GENERATION,
                "pid": std::process::id(),
                "server_pid": server.pid,
                "boot": server.boot,
                "state_dir": options.state,
                "detection_dir": crate::detection::override_directory(),
                "started_ms": started,
                "exe": exe_identity(),
            }),
            features: std::sync::Mutex::new(Vec::new()),
            control: Arc::new(crate::managed::resident::WatchControl::default()),
            reload: tokio::sync::Notify::new(),
            stop: Arc::new(tokio::sync::Notify::new()),
            last_active: Arc::new(std::sync::Mutex::new(Instant::now())),
            watch: std::sync::Mutex::new(None),
        });
        let supervisor = tokio::spawn(supervise(
            shared.clone(),
            options.socket.clone(),
            options.state.clone(),
            options.watch,
            paths.log.clone(),
        ));
        let result = accept_loop(
            listener,
            guard,
            &paths.listen,
            &shared,
            &mut signals,
            options.idle,
            limit,
            &paths.log,
        )
        .await;
        supervisor.abort();
        // A pass that wrote a tracked state also writes its inbox event:
        // let it finish rather than drop it half way.
        shared.control.stopping.store(true, Ordering::SeqCst);
        shared.control.wake.notify_one();
        let watch = shared.watch.lock().ok().and_then(|mut watch| watch.take());
        if let Some(watch) = watch {
            let _ = tokio::time::timeout(DEADLINE, watch).await;
        }
        result
    })
}

/// What the accept loop, the clients and the supervisor share.
struct Shared {
    identity: Value,
    features: std::sync::Mutex<Vec<String>>,
    control: Arc<crate::managed::resident::WatchControl>,
    /// Wakes the supervisor to read the features again.
    reload: tokio::sync::Notify,
    stop: Arc<tokio::sync::Notify>,
    last_active: Arc<std::sync::Mutex<Instant>>,
    /// The running screen watch, awaited when the process ends.
    watch: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Shared {
    fn identity(&self) -> Value {
        let mut identity = self.identity.clone();
        identity["features"] = json!(self.features());
        identity
    }

    fn features(&self) -> Vec<String> {
        self.features
            .lock()
            .map(|features| features.clone())
            .unwrap_or_default()
    }
}

/// Keeps the work in step with the store's features: the screen watch runs
/// while the inbox is on. Also ends the process when its executable was
/// replaced, so the next start runs the new one.
async fn supervise(
    shared: Arc<Shared>,
    socket: PathBuf,
    state: PathBuf,
    watch: Duration,
    log_path: PathBuf,
) {
    loop {
        let (socket_for_read, state_for_read) = (socket.clone(), state.clone());
        // A failed read keeps the last list.
        if let Ok(Ok(features)) = tokio::task::spawn_blocking(move || {
            crate::managed::coordinator_features(&state_for_read, &socket_for_read)
        })
        .await
            && let Ok(mut current) = shared.features.lock()
            && *current != features
        {
            if features.is_empty()
                && let Ok(mut last) = shared.last_active.lock()
            {
                // Idle time counts from the moment the last feature went.
                *last = Instant::now();
            }
            *current = features;
        }
        let wanted = shared.features().iter().any(|feature| feature == "inbox");
        let stopped = match shared.watch.lock() {
            Ok(mut watcher) => {
                if wanted
                    && watcher
                        .as_ref()
                        .is_none_or(tokio::task::JoinHandle::is_finished)
                {
                    match crate::managed::Manager::resident(socket.clone(), watch) {
                        Ok(manager) => {
                            let path = log_path.clone();
                            *watcher = Some(tokio::spawn(crate::managed::resident::watch(
                                manager,
                                shared.control.clone(),
                                watch,
                                move |message: &str| log(&path, message),
                                shared.stop.clone(),
                            )));
                        }
                        Err(error) => log(&log_path, &format!("watch: {error}")),
                    }
                    None
                } else if wanted {
                    None
                } else {
                    watcher.take()
                }
            }
            Err(_) => None,
        };
        if let Some(handle) = stopped {
            handle.abort();
            // A badge write in flight lands before the clear below.
            let _ = handle.await;
            // The inbox went off: no badge for it.
            let socket = socket.clone();
            let _ = tokio::task::spawn_blocking(move || {
                masil(&socket, &["set-option", "-gqu", BADGE_OPTION])
            })
            .await;
        }
        if !exe_unchanged(&shared.identity["exe"]) {
            log(&log_path, "the masil-agent executable changed; exiting");
            shared.stop.notify_one();
            return;
        }
        tokio::select! {
            _ = tokio::time::sleep(SUPERVISE_EVERY) => {}
            _ = shared.reload.notified() => {
                shared.control.reload.store(true, Ordering::SeqCst);
                shared.control.wake.notify_one();
            }
        }
    }
}

fn write_starting(lock: &File, started: u64) -> Result<(), String> {
    let record =
        json!({"pid": std::process::id(), "started_ms": started, "generation": GENERATION});
    let mut file = lock;
    file.set_len(0)
        .and_then(|()| file.write_all(record.to_string().as_bytes()))
        .map_err(|error| format!("coordinator lock record: {error}"))
}

fn clear_record(lock: &File) {
    let _ = lock.set_len(0);
}

fn write_record(lock: &File, server: &ServerInfo, started: u64) -> Result<(), String> {
    let record = json!({
        "pid": std::process::id(),
        "server_pid": server.pid,
        "boot": server.boot,
        "started_ms": started,
        "generation": GENERATION,
    });
    let mut file = lock;
    file.set_len(0)
        .and_then(|()| file.write_all(record.to_string().as_bytes()))
        .map_err(|error| format!("coordinator lock record: {error}"))
}

/// Binds the coordinator socket, replacing a leftover file only when nothing
/// answers on it. The caller holds the lock.
fn bind(path: &Path) -> Result<(std::os::unix::net::UnixListener, ipc::SocketGuard), String> {
    if fs::symlink_metadata(path).is_ok() {
        if StdUnixStream::connect(path).is_ok() {
            return Err("another process answers on the coordinator socket".into());
        }
        fs::remove_file(path).map_err(|error| format!("coordinator socket: {error}"))?;
    }
    ipc::bind_private_socket(path, "coordinator")
}

struct Signals {
    term: tokio::signal::unix::Signal,
    interrupt: tokio::signal::unix::Signal,
    hangup: tokio::signal::unix::Signal,
    rebind: tokio::signal::unix::Signal,
}

impl Signals {
    fn new() -> Result<Self, String> {
        use tokio::signal::unix::{SignalKind, signal};
        let make = |kind| signal(kind).map_err(|error| error.to_string());
        Ok(Self {
            term: make(SignalKind::terminate())?,
            interrupt: make(SignalKind::interrupt())?,
            hangup: make(SignalKind::hangup())?,
            rebind: make(SignalKind::user_defined1())?,
        })
    }
}

/// Sends this process SIGTERM when the server process exits, including a
/// kill -9 that skips the server's own SIGTERM to its jobs.
#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
fn watch_parent(server: libc::pid_t) -> Result<(), String> {
    // SAFETY: kqueue/kevent with valid arguments; the descriptor moves into
    // the thread that owns it.
    let queue = unsafe { libc::kqueue() };
    if queue < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let mut change: libc::kevent = unsafe { std::mem::zeroed() };
    change.ident = server as libc::uintptr_t;
    change.filter = libc::EVFILT_PROC;
    change.flags = libc::EV_ADD | libc::EV_ONESHOT;
    change.fflags = libc::NOTE_EXIT;
    let registered =
        unsafe { libc::kevent(queue, &change, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
    // Registered first, then checked: a server that exited before
    // registration fails with ESRCH or is no longer the parent.
    if registered != 0 || unsafe { libc::getppid() } != server {
        unsafe { libc::close(queue) };
        return Err("the server exited during startup".into());
    }
    std::thread::Builder::new()
        .name("parent-watch".into())
        .spawn(move || {
            let mut event: libc::kevent = unsafe { std::mem::zeroed() };
            loop {
                let count = unsafe {
                    libc::kevent(queue, std::ptr::null(), 0, &mut event, 1, std::ptr::null())
                };
                if count > 0 {
                    break;
                }
                if count < 0
                    && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
                {
                    break;
                }
            }
            unsafe {
                libc::kill(libc::getpid(), libc::SIGTERM);
            }
        })
        .map(drop)
        .map_err(|error| error.to_string())
}

#[cfg(target_os = "linux")]
fn watch_parent(server: libc::pid_t) -> Result<(), String> {
    // SAFETY: prctl with valid arguments; checked again after setting, since
    // the parent may already have exited.
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) } != 0
        || unsafe { libc::getppid() } != server
    {
        return Err("the server exited during startup".into());
    }
    Ok(())
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "linux"
)))]
fn watch_parent(_server: libc::pid_t) -> Result<(), String> {
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn accept_loop(
    listener: std::os::unix::net::UnixListener,
    guard: ipc::SocketGuard,
    path: &Path,
    shared: &Arc<Shared>,
    signals: &mut Signals,
    idle: Duration,
    limit: usize,
    log_path: &Path,
) -> Result<(), String> {
    let mut listener =
        tokio::net::UnixListener::from_std(listener).map_err(|error| error.to_string())?;
    let mut guard = Some(guard);
    let clients = Arc::new(AtomicUsize::new(0));
    let last_active = shared.last_active.clone();
    let stop = shared.stop.clone();
    // A feature is work: the process stays while one is on.
    let busy =
        |clients: &AtomicUsize| clients.load(Ordering::SeqCst) > 0 || !shared.features().is_empty();
    loop {
        let quiet_since = *last_active.lock().map_err(|_| "poisoned")?;
        let deadline = if busy(&clients) {
            Instant::now() + idle
        } else {
            quiet_since + idle
        };
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { continue };
                let Ok(std_stream) = stream.into_std() else { continue };
                if !ipc::peer_is_owner(&std_stream) {
                    continue;
                }
                let Ok(mut stream) = tokio::net::UnixStream::from_std(std_stream) else { continue };
                if clients.load(Ordering::SeqCst) >= limit {
                    // A reply, so callers see a busy coordinator, not a dead one.
                    tokio::spawn(async move {
                        let busy = json!({"v": 1, "id": null, "ok": false,
                            "error": {"code": "coordinator_busy", "message": "too many connections"}});
                        let _ = tokio::time::timeout(
                            Duration::from_millis(200),
                            ipc::write_frame(&mut stream, &busy, RESPONSE_FRAME),
                        )
                        .await;
                    });
                    continue;
                }
                clients.fetch_add(1, Ordering::SeqCst);
                let (clients, last_active, shared) =
                    (clients.clone(), last_active.clone(), shared.clone());
                let idle_seconds = idle.as_secs();
                tokio::spawn(async move {
                    serve_client(stream, &shared, &clients, idle_seconds).await;
                    if clients.fetch_sub(1, Ordering::SeqCst) == 1
                        && let Ok(mut last) = last_active.lock()
                    {
                        *last = Instant::now();
                    }
                });
            }
            _ = signals.term.recv() => break,
            _ = signals.interrupt.recv() => break,
            _ = signals.hangup.recv() => break,
            _ = stop.notified() => break,
            _ = signals.rebind.recv() => {
                if !guard.as_ref().is_some_and(ipc::SocketGuard::still_bound) {
                    // The socket file was removed under a live coordinator.
                    match bind(path) {
                        Ok((std_listener, new_guard)) => match tokio::net::UnixListener::from_std(std_listener) {
                            Ok(new_listener) => {
                                listener = new_listener;
                                guard = Some(new_guard);
                                log(log_path, "rebound the coordinator socket");
                            }
                            Err(error) => log(log_path, &format!("rebind: {error}")),
                        },
                        Err(error) => log(log_path, &format!("rebind: {error}")),
                    }
                }
            }
            _ = tokio::time::sleep_until(deadline.into()) => {
                let quiet = !busy(&clients)
                    && last_active.lock().map(|last| last.elapsed() >= idle).unwrap_or(true);
                if quiet {
                    break;
                }
            }
        }
    }
    // The socket goes first, so a client that arrives now starts a new
    // coordinator, which waits for this one's lock.
    drop(listener);
    drop(guard);
    Ok(())
}

async fn serve_client(
    mut stream: tokio::net::UnixStream,
    shared: &Shared,
    clients: &AtomicUsize,
    idle_seconds: u64,
) {
    loop {
        let Ok(Ok(body)) =
            tokio::time::timeout(DEADLINE, ipc::read_frame(&mut stream, REQUEST_FRAME)).await
        else {
            return;
        };
        let (response, stopping) = respond(&body, shared, clients, idle_seconds);
        let written = tokio::time::timeout(
            DEADLINE,
            ipc::write_frame(&mut stream, &response, RESPONSE_FRAME),
        )
        .await;
        if stopping {
            shared.stop.notify_one();
            return;
        }
        if !matches!(written, Ok(Ok(()))) {
            return;
        }
    }
}

fn respond(
    body: &[u8],
    shared: &Shared,
    clients: &AtomicUsize,
    idle_seconds: u64,
) -> (Value, bool) {
    let failure = |id: &Value, code: &str, message: &str| json!({"v": 1, "id": id, "ok": false, "error": {"code": code, "message": message}});
    let Ok(request) = serde_json::from_slice::<Value>(body) else {
        return (
            failure(&Value::Null, "invalid_request", "request is not JSON"),
            false,
        );
    };
    let Some(object) = request.as_object() else {
        return (
            failure(&Value::Null, "invalid_request", "request is not an object"),
            false,
        );
    };
    let id = object.get("id").cloned().unwrap_or(Value::Null);
    let valid_id = id
        .as_str()
        .is_some_and(|id| !id.is_empty() && id.len() <= 64);
    if object.get("v") != Some(&json!(1))
        || !valid_id
        || object
            .keys()
            .any(|key| !matches!(key.as_str(), "v" | "id" | "method" | "params"))
    {
        return (
            failure(&id, "invalid_request", "expected v, id, method and params"),
            false,
        );
    }
    let ok = |value: Value| json!({"v": 1, "id": id.clone(), "ok": true, "value": value});
    match object.get("method").and_then(Value::as_str) {
        Some("hello") => (ok(shared.identity()), false),
        Some("status") => {
            let mut value = shared.identity();
            value["clients"] = json!(clients.load(Ordering::SeqCst));
            value["idle_seconds"] = json!(idle_seconds);
            value["pokes"] = json!(POKES.load(Ordering::SeqCst));
            if let Ok(report) = shared.control.report.lock() {
                value["watch"] = json!(*report);
            }
            // Names only, to show what the environment allowlist kept.
            let mut names: Vec<String> = std::env::vars_os()
                .map(|(name, _)| name.to_string_lossy().into_owned())
                .collect();
            names.sort();
            value["environment"] = json!(names);
            (ok(value), false)
        }
        Some("stop") => (ok(json!({"stopping": true})), true),
        // A report or inbox write: the watch looks at the panes now.
        Some("poke") => {
            POKES.fetch_add(1, Ordering::SeqCst);
            shared.control.poked.store(true, Ordering::SeqCst);
            shared.control.wake.notify_one();
            (ok(json!({})), false)
        }
        // Events were read: only the badge count can have changed.
        Some("recount") => {
            shared.control.recount.store(true, Ordering::SeqCst);
            shared.control.wake.notify_one();
            (ok(json!({})), false)
        }
        // Features or manifests changed; the reply is the identity before
        // the supervisor reads them.
        Some("reload") => {
            shared.reload.notify_one();
            (ok(shared.identity()), false)
        }
        _ => (failure(&id, "unknown_method", "unknown method"), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared() -> Shared {
        Shared {
            identity: json!({"generation": GENERATION}),
            features: std::sync::Mutex::new(vec!["inbox".into()]),
            control: Arc::default(),
            reload: tokio::sync::Notify::new(),
            stop: Arc::default(),
            last_active: Arc::new(std::sync::Mutex::new(Instant::now())),
            watch: std::sync::Mutex::new(None),
        }
    }

    #[test]
    fn requests_need_the_envelope_and_a_known_method() {
        let shared = shared();
        let clients = AtomicUsize::new(2);
        let call = |text: &str| respond(text.as_bytes(), &shared, &clients, 60);
        let (hello, stop) = call(r#"{"v":1,"id":"a","method":"hello"}"#);
        assert_eq!(hello["value"]["generation"], GENERATION);
        assert!(!stop);
        assert_eq!(hello["value"]["features"], json!(["inbox"]));
        let (status, _) = call(r#"{"v":1,"id":"a","method":"status","params":{}}"#);
        assert_eq!(status["value"]["clients"], 2);
        assert_eq!(status["value"]["watch"]["ticks"], 0);
        assert!(!call(r#"{"v":1,"id":"a","method":"reload"}"#).1);
        assert!(!call(r#"{"v":1,"id":"a","method":"poke"}"#).1);
        assert!(call(r#"{"v":1,"id":"a","method":"stop"}"#).1);
        assert_eq!(
            call(r#"{"v":1,"id":"a","method":"later"}"#).0["error"]["code"],
            "unknown_method"
        );
        for bad in [
            r#"{"v":2,"id":"a","method":"hello"}"#,
            r#"{"v":1,"method":"hello"}"#,
            r#"{"v":1,"id":"a","method":"hello","extra":1}"#,
            "not json",
        ] {
            assert_eq!(call(bad).0["error"]["code"], "invalid_request", "{bad}");
        }
    }

    #[test]
    fn idle_and_paths_are_bounded() {
        let options = parse(&[
            "--socket".into(),
            "/tmp/s".into(),
            "--state-dir".into(),
            "/tmp/state".into(),
            "--idle-seconds".into(),
            "0".into(),
        ])
        .unwrap();
        assert_eq!(options.idle, DEFAULT_IDLE);
        assert_eq!(options.watch, DEFAULT_WATCH);
        assert!(parse(&["--socket".into(), "relative".into()]).is_err());
        assert_eq!(digest16(Path::new("/tmp/a")).len(), 16);
    }

    #[test]
    fn a_lock_is_held_only_while_its_file_is_locked() {
        let dir = std::env::temp_dir().join(format!("masil-coord-{}", std::process::id()));
        private_directory(&dir).unwrap();
        let path = dir.join("x.lock");
        assert!(!lock_held(&path).unwrap());
        let file = acquire(&path, Duration::ZERO).unwrap().unwrap();
        assert!(lock_held(&path).unwrap());
        assert!(
            acquire(&path, Duration::from_millis(100))
                .unwrap()
                .is_none()
        );
        drop(file);
        assert!(!lock_held(&path).unwrap());
        fs::remove_dir_all(dir).unwrap();
    }
}
