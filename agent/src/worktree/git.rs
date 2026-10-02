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

pub(super) fn command(dir: &Path) -> Command {
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

pub(super) fn tail(bytes: &[u8], limit: usize) -> String {
    let start = bytes.len().saturating_sub(limit);
    String::from_utf8_lossy(&bytes[start..]).trim().to_owned()
}

/// A read-only git query: its stdout, or its stderr's end as the error.
pub(super) fn query(dir: &Path, args: &[&str]) -> Result<String, String> {
    probe(dir, args)?.ok_or_else(|| format!("git {} found nothing", args.join(" ")))
}

/// Like [`query`], but `None` when git exits 1, which `rev-parse --verify
/// --quiet` and `show-ref` use for "no such ref".
pub(super) fn probe(dir: &Path, args: &[&str]) -> Result<Option<String>, String> {
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

/// SIGTERM to the child's group, SIGKILL after [`GRACE`]. The child is not
/// reaped before the signals, so its group ID cannot name another group.
pub(super) fn stop(child: &mut Child) {
    let group = child.id() as i32;
    // SAFETY: signals to a group led by an unreaped child of this process.
    unsafe { libc::killpg(group, libc::SIGTERM) };
    let deadline = Instant::now() + GRACE;
    while Instant::now() < deadline {
        if matches!(child.try_wait(), Ok(Some(_)) | Err(_)) {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    // SAFETY: as above; the leader is still unreaped.
    unsafe { libc::killpg(group, libc::SIGKILL) };
    let _ = child.wait();
}
