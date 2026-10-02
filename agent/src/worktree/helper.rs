//! Each worktree job runs in its own detached helper, `worktree job-run ID`
//! (D19). The helper owns the job: its PID and start time are the job's
//! owner, and a job whose owner died is settled by whoever finds it.

use super::create::{self, Cleaned, Cleanup};
use super::registry::{CancelStep, Claim, End, Job, Registry, now_ms};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Set by SIGTERM: the job's owner was asked to stop.
pub(super) static CANCEL: AtomicBool = AtomicBool::new(false);
/// Job output kept in the registry.
const OUTPUT_TAIL: u64 = 64 * 1024;
/// A queued job whose helper has not recorded itself by then never will.
const OWNERLESS_MS: u64 = 10_000;
const CLAIM_RETRY: Duration = Duration::from_millis(200);
/// A waiting command reads the job this often at first, then every
/// [`WAIT_POLL_SLOW`], so a long setup costs little.
const WAIT_POLL: Duration = Duration::from_millis(100);
const WAIT_POLL_SLOW: Duration = Duration::from_millis(250);
const WAIT_FAST_FOR: Duration = Duration::from_secs(2);

extern "C" fn on_term(_: libc::c_int) {
    CANCEL.store(true, Ordering::SeqCst);
}

/// Whether the recorded owner is still the same process.
pub(super) fn alive(job: &Job) -> bool {
    job.owner.is_some_and(|(pid, started)| {
        crate::process::info(pid).is_some_and(|info| info.started == started)
    })
}

fn me() -> Result<(i32, u64), String> {
    let pid = std::process::id() as i32;
    crate::process::info(pid)
        .map(|info| (pid, info.started))
        .ok_or_else(|| "cannot read this process's start time".into())
}

/// Starts the helper of a new job and records it as the owner.
pub(super) fn spawn(registry: &Registry, id: i64) -> Result<Child, String> {
    let executable =
        std::env::current_exe().map_err(|error| format!("locating masil-agent: {error}"))?;
    let log = registry.open_job_log(id)?;
    let mut command = Command::new(executable);
    command
        .args(["worktree", "job-run", &id.to_string()])
        .stdin(Stdio::null())
        .stdout(log.try_clone().map_err(|error| error.to_string())?)
        .stderr(log);
    // SAFETY: setsid is async-signal-safe and has no preconditions.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let child = command
        .spawn()
        .map_err(|error| format!("starting the worktree job helper: {error}"))?;
    let pid = child.id() as i32;
    if let Some(info) = crate::process::info(pid) {
        registry.set_owner(id, pid, info.started)?;
    }
    Ok(child)
}

/// The helper's main: claim the job, run it, record how it ended.
pub(super) fn run(id: i64) -> Result<i32, String> {
    // SAFETY: the handler only stores to an atomic. Installed before the
    // claim, so a cancel never finds a helper that ignores it. The helper
    // has no terminal (setsid), so SIGINT and SIGHUP stay at their defaults
    // for git and hooks.
    unsafe {
        libc::signal(libc::SIGTERM, on_term as *const () as libc::sighandler_t);
    }
    let owner = me()?;
    let mut registry = Registry::open()?;
    if !registry.set_owner(id, owner.0, owner.1)? {
        // Another helper owns this job.
        return Ok(0);
    }
    let job = loop {
        if CANCEL.load(Ordering::SeqCst) {
            // Asked to stop while queued: nothing has run.
            registry.request_cancel(id, now_ms())?;
            return Ok(0);
        }
        match registry.claim(id, owner, &alive, now_ms())? {
            Claim::Running(job) => break *job,
            Claim::Gone => return Ok(0),
            Claim::Wait => std::thread::sleep(CLAIM_RETRY),
            Claim::Reconcile(jobs) => {
                let _lock = registry.lock()?;
                for job in jobs {
                    settle_orphan(&mut registry, job.id)?;
                }
            }
        }
    };
    let log = registry.open_job_log(id)?;
    let end = match job.kind.as_str() {
        "create" => create::run(&mut registry, &job, &log),
        kind => End::Failed(format!("this masil-agent cannot run {kind} jobs")),
    };
    let tail = output_tail(&registry, id);
    registry.finish_job(id, end, tail.as_deref(), now_ms())?;
    Ok(0)
}

fn output_tail(registry: &Registry, id: i64) -> Option<String> {
    let mut file = std::fs::File::open(registry.job_log(id).ok()?).ok()?;
    let length = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(length.saturating_sub(OUTPUT_TAIL)))
        .ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes).trim().to_owned();
    (!text.is_empty()).then_some(text)
}

/// Settles one open job whose owner is gone. The caller holds the registry
/// lock; the job is read again under it, and each end applies only if the
/// job is still as read.
pub(super) fn settle_orphan(registry: &mut Registry, id: i64) -> Result<(), String> {
    let Some(job) = registry.job(id)? else {
        return Ok(());
    };
    if !job.open() || alive(&job) {
        return Ok(());
    }
    let now = now_ms();
    if job.state == "queued" {
        registry.end_orphan(
            &job,
            "failed",
            "its helper ended before the job started",
            None,
            now,
        )?;
        return Ok(());
    }
    stop_orphan_child(&job);
    let settled = serde_json::json!({"reconciled": true});
    let Some(worktree) = job.worktree_id.filter(|_| job.kind == "create") else {
        let error = if job.kind == "create" {
            "its helper ended before it reserved a worktree"
        } else {
            "its helper ended"
        };
        let state = if job.kind == "create" {
            "failed"
        } else {
            "outcome_unknown"
        };
        registry.end_orphan(&job, state, error, Some(&settled), now)?;
        return Ok(());
    };
    if let Some(row) = registry.worktree(worktree)?
        && row.state == "ready"
    {
        let mut result = row.to_json();
        result["reconciled"] = serde_json::json!(true);
        registry.end_orphan(
            &job,
            "succeeded",
            "its helper ended after the worktree was made",
            Some(&result),
            now,
        )?;
        return Ok(());
    }
    let (state, error) = match create::remove_half_made(registry, worktree, Cleanup::Later) {
        Ok(Cleaned::Removed) => (
            "failed",
            "its helper ended; the half-made worktree was removed".to_owned(),
        ),
        Ok(Cleaned::Kept(why)) => (
            "outcome_unknown",
            format!("its helper ended; the worktree was kept: {why}"),
        ),
        Ok(Cleaned::Left(why)) => ("failed", format!("its helper ended; {why}")),
        Err(error) => (
            "outcome_unknown",
            format!(
                "its helper ended; the half-made worktree could not be removed yet, a later worktree command retries: {error}"
            ),
        ),
    };
    registry.end_orphan(&job, state, &error, Some(&settled), now)?;
    Ok(())
}

/// Kills the process group a dead owner left running, so nothing writes to
/// the worktree while it is settled. Only a leader that is still the
/// recorded process is signalled: its group ID cannot name another group.
fn stop_orphan_child(job: &Job) {
    let Some((pid, started)) = job.child else {
        return;
    };
    let same = || crate::process::info(pid).is_some_and(|info| info.started == started);
    if !same() {
        return;
    }
    // SAFETY: the group's leader was just checked to be the recorded child.
    unsafe { libc::killpg(pid, libc::SIGKILL) };
    for _ in 0..100 {
        if !same() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Whether an open job can no longer finish by itself: its helper never
/// recorded itself, or the recorded one is gone.
fn orphaned(job: &Job, now: u64) -> bool {
    match job.owner {
        Some(_) => !alive(job),
        None => now.saturating_sub(job.created_ms) > OWNERLESS_MS,
    }
}

/// Settles every orphaned open job, then retries the cleanup of worktrees
/// still being made whose create job has ended. Takes the registry lock
/// only when there is something to do.
pub(super) fn sweep(registry: &mut Registry) -> Result<(), String> {
    let now = now_ms();
    let orphans: Vec<i64> = registry
        .open_jobs()?
        .into_iter()
        .filter(|job| orphaned(job, now))
        .map(|job| job.id)
        .collect();
    let unfinished = registry.unfinished_worktrees()?;
    if orphans.is_empty() && unfinished.is_empty() {
        return Ok(());
    }
    let _lock = registry.lock()?;
    for id in orphans {
        let Some(job) = registry.job(id)? else {
            continue;
        };
        if job.owner.is_none() && job.state == "queued" && orphaned(&job, now_ms()) {
            registry.end_orphan(&job, "failed", "its helper did not start", None, now_ms())?;
        } else {
            settle_orphan(registry, id)?;
        }
    }
    // Read again under the lock; a failure is retried by a later command.
    for worktree in registry.unfinished_worktrees()? {
        let _ = create::remove_half_made(registry, worktree, Cleanup::Later);
    }
    Ok(())
}

/// Waits for a job to end. `child` is the helper this process started, if
/// any; it is reaped here so a crashed helper is not taken for alive.
pub(super) fn wait(
    registry: &mut Registry,
    id: i64,
    mut child: Option<Child>,
) -> Result<Job, String> {
    let started = std::time::Instant::now();
    loop {
        if let Some(helper) = child.as_mut()
            && matches!(helper.try_wait(), Ok(Some(_)) | Err(_))
        {
            child = None;
        }
        let job = registry
            .job(id)?
            .ok_or_else(|| format!("target_absent: worktree job {id} is not in the registry"))?;
        if !job.open() {
            return Ok(job);
        }
        if orphaned(&job, now_ms()) {
            sweep(registry)?;
            continue;
        }
        std::thread::sleep(if started.elapsed() < WAIT_FAST_FOR {
            WAIT_POLL
        } else {
            WAIT_POLL_SLOW
        });
    }
}

/// `worktree cancel`: a queued job ends here; a running one is marked and
/// its live owner gets SIGTERM.
pub(super) fn cancel(registry: &mut Registry, id: i64) -> Result<Job, String> {
    match registry.request_cancel(id, now_ms())? {
        CancelStep::Cancelled(job) | CancelStep::Unchanged(job) => {
            if job.state == "cancel_requested" {
                signal(&job);
            }
            Ok(job)
        }
        CancelStep::Signal(job) => {
            signal(&job);
            Ok(job)
        }
    }
}

fn signal(job: &Job) {
    if let Some((pid, started)) = job.owner
        && crate::process::info(pid).is_some_and(|info| info.started == started)
    {
        // SAFETY: the PID was just checked to be the recorded owner.
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
}
