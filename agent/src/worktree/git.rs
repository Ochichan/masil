//! git as worktree jobs run it: with the user's configuration and hooks,
//! but never in a repository the caller's environment points to.

use std::fs::File;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Variables that move git to another repository, index or object store.
const SCRUBBED: [&str; 7] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
];
/// How long a cancelled git has between SIGTERM and SIGKILL (B-14).
const GRACE: Duration = Duration::from_secs(2);
/// Error text kept from a failed query.
const ERROR_TAIL: usize = 2048;

pub(crate) fn command(dir: &Path) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(dir);
    for name in SCRUBBED {
        command.env_remove(name);
    }
    command
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .stdin(Stdio::null());
    command
}

pub(crate) fn tail(bytes: &[u8], limit: usize) -> String {
    let start = bytes.len().saturating_sub(limit);
    String::from_utf8_lossy(&bytes[start..]).trim().to_owned()
}

/// A read-only git query: its stdout, or its stderr's end as the error.
pub(crate) fn query(dir: &Path, args: &[&str]) -> Result<String, String> {
    probe(dir, args)?.ok_or_else(|| format!("git {} found nothing", args.join(" ")))
}

/// Like [`query`], but `None` when git exits 1, which `rev-parse --verify
/// --quiet` and `show-ref` use for "no such ref".
pub(crate) fn probe(dir: &Path, args: &[&str]) -> Result<Option<String>, String> {
    let output = command(dir)
        .args(args)
        .output()
        .map_err(|error| format!("git: {error}"))?;
    if output.status.success() {
        return Ok(Some(String::from_utf8_lossy(&output.stdout).into_owned()));
    }
    if output.status.code() == Some(1) && output.stderr.is_empty() {
        return Ok(None);
    }
    Err(format!(
        "git {}: {}",
        args.first().copied().unwrap_or_default(),
        tail(&output.stderr, ERROR_TAIL)
    ))
}

pub(super) enum Ran {
    Exited(ExitStatus),
    Cancelled,
}

/// Runs git with its output going to `log`, in its own process group so a
/// cancel stops git and every hook it started. `started` and `ended` record
/// the child while it is unreaped, so its group can be stopped if this
/// process dies.
pub(super) fn run_logged(
    dir: &Path,
    args: &[&str],
    log: &File,
    cancel: &AtomicBool,
    started: &dyn Fn(i32),
    ended: &dyn Fn(),
) -> Result<Ran, String> {
    let clone = || log.try_clone().map_err(|error| error.to_string());
    let mut child = command(dir)
        .args(args)
        .stdout(clone()?)
        .stderr(clone()?)
        .process_group(0)
        .spawn()
        .map_err(|error| format!("git: {error}"))?;
    started(child.id() as i32);
    let ran = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(Ran::Exited(status)),
            Ok(None) => {}
            Err(error) => {
                stop(&mut child);
                break Err(error.to_string());
            }
        }
        if cancel.load(Ordering::SeqCst) {
            stop(&mut child);
            break Ok(Ran::Cancelled);
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    ended();
    ran
}

/// SIGTERM to the child's group, then SIGKILL to whatever is left of it
/// once the leader has exited or [`GRACE`] has passed. The leader is reaped
/// only after that, so the group ID cannot name another group meanwhile.
pub(crate) fn stop(child: &mut Child) {
    let group = child.id() as i32;
    // SAFETY: signals to a group led by an unreaped child of this process.
    unsafe { libc::killpg(group, libc::SIGTERM) };
    let deadline = Instant::now() + GRACE;
    while Instant::now() < deadline && !exited(group) {
        std::thread::sleep(Duration::from_millis(20));
    }
    // SAFETY: as above; the leader is still unreaped.
    unsafe { libc::killpg(group, libc::SIGKILL) };
    let _ = child.wait();
}

/// Whether the child `pid` has exited, without reaping it.
pub(crate) fn exited(pid: i32) -> bool {
    // SAFETY: a zeroed siginfo_t is valid for waitid to fill.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: waits on this process's own child; WNOWAIT leaves it unreaped.
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    result == 0 && signal_pid(&info) != 0
}

#[cfg(target_os = "linux")]
fn signal_pid(info: &libc::siginfo_t) -> libc::pid_t {
    // SAFETY: waitid filled the child fields.
    unsafe { info.si_pid() }
}

#[cfg(not(target_os = "linux"))]
fn signal_pid(info: &libc::siginfo_t) -> libc::pid_t {
    info.si_pid
}

/// What [`output_within`] read.
pub(crate) struct Output {
    pub(crate) stdout: Vec<u8>,
    /// Output stopped at the byte limit; git was stopped there.
    pub(crate) truncated: bool,
    /// git's exit status, when it ended by itself.
    pub(crate) code: Option<i32>,
}

/// Runs git in its own process group and returns its standard output, at
/// most `limit` bytes. Exit codes in `ok` count as success. A git still
/// running at `deadline` is stopped and `timeout` is the error; output past
/// `limit` stops git too.
pub(crate) fn output_within(
    dir: &Path,
    args: &[&str],
    ok: &[i32],
    deadline: Instant,
    limit: usize,
    timeout: &str,
) -> Result<Output, String> {
    let mut command = command(dir);
    command.args(args);
    run_within(command, None, ok, deadline, limit, timeout)
}

/// Like [`output_within`] for a prepared git command, writing `input` to
/// its standard input.
pub(crate) fn run_within(
    mut command: Command,
    input: Option<Vec<u8>>,
    ok: &[i32],
    deadline: Instant,
    limit: usize,
    timeout: &str,
) -> Result<Output, String> {
    command.stdin(if input.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    run_prepared(command, input, ok, deadline, limit, timeout)
}

/// Like [`run_within`], reading standard input from `stdin` (a descriptor
/// git reads without resolving any path).
pub(crate) fn run_with_stdin(
    mut command: Command,
    stdin: Stdio,
    ok: &[i32],
    deadline: Instant,
    limit: usize,
    timeout: &str,
) -> Result<Output, String> {
    command.stdin(stdin);
    run_prepared(command, None, ok, deadline, limit, timeout)
}

/// Runs git with its standard output going straight into `file`.
pub(crate) fn run_into(
    mut command: Command,
    file: std::fs::File,
    deadline: Instant,
    timeout: &str,
) -> Result<(), String> {
    command.stdin(Stdio::null()).stdout(file);
    let mut child = command
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|error| format!("git: {error}"))?;
    loop {
        if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
            return if status.success() {
                Ok(())
            } else {
                Err(format!("git failed ({status})"))
            };
        }
        if Instant::now() >= deadline {
            stop(&mut child);
            return Err(timeout.to_owned());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn run_prepared(
    mut command: Command,
    input: Option<Vec<u8>>,
    ok: &[i32],
    deadline: Instant,
    limit: usize,
    timeout: &str,
) -> Result<Output, String> {
    use std::io::{Read, Write};
    use std::sync::Arc;
    let name = command
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let name = subcommand(&name.iter().map(String::as_str).collect::<Vec<_>>()).to_owned();
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|error| format!("git: {error}"))?;
    if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
        std::thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
    }
    let full = Arc::new(AtomicBool::new(false));
    let mut stdout = child.stdout.take().ok_or("git output unavailable")?;
    let mut stderr = child.stderr.take().ok_or("git output unavailable")?;
    let reader = {
        let full = Arc::clone(&full);
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = (&mut stdout).take(limit as u64 + 1).read_to_end(&mut bytes);
            if bytes.len() > limit {
                bytes.truncate(limit);
                full.store(true, Ordering::SeqCst);
            }
            bytes
        })
    };
    // Read to the end, keeping the tail: a pipe closed early would kill a
    // chatty git (warnings per file) with SIGPIPE.
    let errors = std::thread::spawn(move || {
        let mut kept = Vec::new();
        let mut buffer = [0u8; 8192];
        while let Ok(read) = stderr.read(&mut buffer) {
            if read == 0 {
                break;
            }
            kept.extend_from_slice(&buffer[..read]);
            if kept.len() > 2 * ERROR_TAIL {
                kept.drain(..kept.len() - ERROR_TAIL);
            }
        }
        kept
    });
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
            break Some(status);
        }
        if full.load(Ordering::SeqCst) {
            stop(&mut child);
            break None;
        }
        if Instant::now() >= deadline {
            stop(&mut child);
            let _ = reader.join();
            return Err(timeout.to_owned());
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    let stdout = reader.join().unwrap_or_default();
    let stderr = errors.join().unwrap_or_default();
    // Past the limit git may also have died of SIGPIPE; what was read stands.
    let Some(status) = status.filter(|_| !full.load(Ordering::SeqCst)) else {
        return Ok(Output {
            stdout,
            truncated: true,
            code: None,
        });
    };
    if status.code().is_some_and(|code| ok.contains(&code)) {
        return Ok(Output {
            stdout,
            truncated: false,
            code: status.code(),
        });
    }
    Err(format!(
        "git {name} failed ({status}): {}",
        tail(&stderr, ERROR_TAIL)
    ))
}

/// The git subcommand in `args`, past any `-C DIR` and `-c NAME=VALUE`.
fn subcommand<'a>(args: &[&'a str]) -> &'a str {
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if *arg == "-c" || *arg == "-C" {
            args.next();
        } else if !arg.starts_with('-') {
            return arg;
        }
    }
    ""
}
