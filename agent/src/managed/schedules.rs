//! Schedules (P7b, docs/schedules.md): `agent schedule ...`, and the
//! coordinator's scheduler, which starts each due action on a thread of its
//! own and records how it ended.

use super::Manager;
use super::durable::ClientKey;
use super::operations::ScheduleRow;
use crate::schedule::{self, Spec};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// The longest the scheduler sleeps before it looks at the wall clock.
const LOOK_EVERY: Duration = Duration::from_secs(60);
/// How long an action waits for the management lock.
const LOCK_WAIT: Duration = Duration::from_secs(30);
/// After a failed look, well inside the grace a due time has.
const RETRY: Duration = Duration::from_secs(5);
/// Runs left `running` are settled once per coordinator process: a later
/// scheduler thread of the same process must not touch its own runs.
static SETTLED: AtomicBool = AtomicBool::new(false);

fn usage() -> String {
    "usage: masil-agent agent schedule add NAME (--cron \"M H DOM MON DOW\" | --every 30m) \
     (--layout TEMPLATE [--session S] [--yes] | --start AGENT PROVIDER --cwd DIR [--worktree W] | --queue AGENT --template TEMPLATE) \
     | list | runs NAME [--limit N] | enable NAME | disable NAME | remove NAME"
        .into()
}

fn now_ms() -> u64 {
    crate::observation::now_ms()
}

fn valid_name(name: &str) -> Result<(), String> {
    if super::valid_name(name) {
        Ok(())
    } else {
        Err("schedule_invalid: a schedule name begins with a lowercase letter and uses 1–32 lowercase letters, digits, '-' or '_'".into())
    }
}

/// `agent schedule ...`.
pub(super) async fn command(manager: &Manager, args: &[String]) -> Result<Value, String> {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    let value = match words.as_slice() {
        ["add", name, rest @ ..] => add(manager, name, rest).await?,
        ["list"] => {
            let store = manager.operation_store().await?;
            let local = |ms| schedule::cron::system_local(ms);
            let now = now_ms() as i64;
            let schedules: Vec<Value> = store
                .schedules()?
                .iter()
                .map(|row| {
                    let next = Spec::parse(&row.spec).ok().and_then(|spec| {
                        let lower = row
                            .last_due
                            .unwrap_or(i64::MIN)
                            .max(row.changed_ms)
                            .max(now);
                        spec.next_after(lower, row.created_ms, &local)
                    });
                    json!({
                        "name": row.name,
                        "spec": row.spec,
                        "action": row.action,
                        "enabled": row.enabled,
                        "next_ms": if row.enabled { json!(next) } else { Value::Null },
                        "last_due_ms": row.last_due,
                        "last_state": row.last_state,
                    })
                })
                .collect();
            return Ok(json!({"schedules": schedules}));
        }
        ["runs", name] | ["runs", name, "--limit", _] => {
            let limit = match words.as_slice() {
                [_, _, _, limit] => limit
                    .parse::<usize>()
                    .ok()
                    .filter(|limit| (1..=200).contains(limit))
                    .ok_or("invalid_argument: --limit must be 1-200")?,
                _ => 20,
            };
            let store = manager.operation_store().await?;
            return Ok(json!({"name": name, "runs": store.schedule_runs(name, limit)?}));
        }
        ["enable", name] | ["disable", name] => {
            let enabled = words[0] == "enable";
            manager
                .operation_store()
                .await?
                .schedule_set_enabled(name, enabled, now_ms())?;
            json!({"name": name, "enabled": enabled})
        }
        ["remove", name] => {
            manager.operation_store().await?.schedule_remove(name)?;
            json!({"name": name, "removed": true})
        }
        _ => return Err(usage()),
    };
    // The coordinator runs the schedules: one that should and does not is
    // started, and every one reads them again.
    let socket = manager.native.socket.clone();
    // A schedule just made or switched on is wanted even when the features
    // cannot be read now: no coordinator would ever pick it up otherwise.
    let switched_on = matches!(words[0], "add" | "enable");
    let coordinator = tokio::task::spawn_blocking(move || {
        let wanted = match super::state_base()
            .and_then(|base| super::coordinator_features(&base, &socket))
        {
            Ok(features) => features.iter().any(|feature| feature == "schedules"),
            Err(_) => switched_on,
        };
        if !wanted {
            crate::coordinator::reload(&socket);
            return None;
        }
        Some(match crate::coordinator::ensure(&socket) {
            Ok(_) => {
                crate::coordinator::reload(&socket);
                "running".to_owned()
            }
            Err(error) => format!("error: {error}"),
        })
    })
    .await
    .map_err(|error| error.to_string())?;
    let mut value = value;
    if let Some(coordinator) = coordinator {
        value["coordinator"] = json!(coordinator);
    }
    Ok(value)
}

async fn add(manager: &Manager, name: &str, rest: &[&str]) -> Result<Value, String> {
    valid_name(name)?;
    let mut spec = None;
    let mut action = None;
    let mut session = None;
    let mut yes = false;
    let mut index = 0;
    let take = |index: usize, count: usize| {
        rest.get(index + 1..index + 1 + count)
            .filter(|values| values.iter().all(|value| !value.starts_with("--")))
            .ok_or_else(usage)
    };
    let mut start_cwd = None;
    let mut worktree = None;
    let mut template = None;
    while index < rest.len() {
        match rest[index] {
            "--cron" if spec.is_none() => {
                let text = take(index, 1)?[0];
                spec = Some(Spec::cron(text)?.stored(text));
                index += 2;
            }
            "--every" if spec.is_none() => {
                let text = take(index, 1)?[0];
                spec = Some(Spec::every(text)?.stored(text));
                index += 2;
            }
            "--layout" if action.is_none() => {
                action = Some(("layout", vec![take(index, 1)?[0].to_owned()]));
                index += 2;
            }
            "--start" if action.is_none() => {
                let values = take(index, 2)?;
                action = Some((
                    "start",
                    values.iter().map(|value| (*value).to_owned()).collect(),
                ));
                index += 3;
            }
            "--queue" if action.is_none() => {
                action = Some(("queue", vec![take(index, 1)?[0].to_owned()]));
                index += 2;
            }
            "--session" if session.is_none() => {
                session = Some(take(index, 1)?[0].to_owned());
                index += 2;
            }
            "--cwd" if start_cwd.is_none() => {
                start_cwd = Some(PathBuf::from(take(index, 1)?[0]));
                index += 2;
            }
            "--worktree" if worktree.is_none() => {
                worktree = Some(take(index, 1)?[0].to_owned());
                index += 2;
            }
            "--template" if template.is_none() => {
                template = Some(take(index, 1)?[0].to_owned());
                index += 2;
            }
            "--yes" if !yes => {
                yes = true;
                index += 1;
            }
            _ => return Err(usage()),
        }
    }
    let spec = spec.ok_or_else(usage)?;
    let now = now_ms() as i64;
    let local = |ms| schedule::cron::system_local(ms);
    if Spec::parse(&spec)?.next_after(now, now, &local).is_none() {
        return Err(format!(
            "schedule_invalid: {spec} never comes in the next 8 years"
        ));
    }
    let (kind, values) = action.ok_or_else(usage)?;
    // Template errors are the person's input: exit 2, unless they carry a
    // code of their own (a server that cannot be reached).
    let invalid = |error: String| {
        if super::failure::has_registered_code(&error) {
            error
        } else {
            format!("schedule_invalid: {error}")
        }
    };
    let only = |allowed: &[&str]| {
        let given = [
            ("--session", session.is_some()),
            ("--yes", yes),
            ("--cwd", start_cwd.is_some()),
            ("--worktree", worktree.is_some()),
            ("--template", template.is_some()),
        ];
        match given
            .iter()
            .find(|(option, set)| *set && !allowed.contains(option))
        {
            Some((option, _)) => Err(format!(
                "usage: {option} does not go with --{kind}; {}",
                usage()
            )),
            None => Ok(()),
        }
    };
    let here = std::env::current_dir().map_err(|error| error.to_string())?;
    let action = match kind {
        "layout" => {
            only(&["--session", "--yes"])?;
            let (plan, kept, confirm) = crate::layout::plan_kept(
                &values[0],
                session.as_deref(),
                manager.native.socket.clone(),
            )
            .await
            .map_err(invalid)?;
            if confirm && !yes {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({"plan": plan}))
                        .map_err(|error| error.to_string())?
                );
                return Err(
                    "schedule_unconfirmed: the layout runs commands or agents; review the plan above and add --yes to schedule it as it is now".into(),
                );
            }
            json!({"kind": "layout", "layout": values[0], "kept": kept})
        }
        "start" => {
            only(&["--cwd", "--worktree"])?;
            let provider = crate::providers::find(&values[1])
                .ok_or_else(|| format!("schedule_invalid: unknown provider {}", values[1]))?;
            if !super::valid_name(&values[0]) {
                return Err("schedule_invalid: the agent name begins with a lowercase letter and uses 1–32 lowercase letters, digits, '-' or '_'".into());
            }
            let cwd = here.join(start_cwd.ok_or("schedule_invalid: --start needs --cwd DIR")?);
            let cwd = cwd
                .canonicalize()
                .ok()
                .filter(|path| path.is_dir())
                .ok_or_else(|| format!("schedule_invalid: {} is not a directory", cwd.display()))?;
            let mut action = json!({
                "kind": "start",
                "agent": values[0],
                "provider": provider.id,
                "cwd": cwd,
            });
            if let Some(worktree) = worktree {
                // Checked now; found again when the schedule runs.
                crate::worktree::start_dir(&worktree, &cwd, None)?;
                action["worktree"] = json!(worktree);
            }
            action
        }
        _ => {
            only(&["--template"])?;
            let template = template.ok_or("schedule_invalid: --queue needs --template TEMPLATE")?;
            let path = crate::layout::template_path(&template).map_err(invalid)?;
            let drafts = crate::layout::drafts_for(&path, &values[0]).map_err(invalid)?;
            if drafts.is_empty() {
                return Err(format!(
                    "schedule_invalid: the {template} pane of {} has no queue drafts",
                    values[0]
                ));
            }
            json!({"kind": "queue", "agent": values[0], "template": template, "path": path})
        }
    };
    let row = manager
        .operation_store()
        .await?
        .schedule_add(name, &spec, &action, now_ms())?;
    Ok(json!({"name": row.name, "spec": row.spec, "action": row.action, "enabled": true}))
}

/// The coordinator's scheduler, on a thread of its own: it looks at the
/// schedules at least once a minute and whenever `wake` fires, and returns
/// once `stop` is set. `poke` tells the watch an event was written.
pub(crate) fn serve(
    socket: PathBuf,
    started_ms: u64,
    wake: Arc<tokio::sync::Notify>,
    stop: Arc<AtomicBool>,
    poke: Arc<dyn Fn() + Send + Sync>,
    log: Arc<dyn Fn(&str) + Send + Sync>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => return log(&format!("scheduler: {error}")),
    };
    runtime.block_on(async {
        let manager = match Manager::new(socket.clone(), None) {
            Ok(manager) => manager,
            Err(error) => return log(&format!("scheduler: {error}")),
        };
        let mut failed = None::<String>;
        while !stop.load(Ordering::SeqCst) {
            let looked = look(&manager, &socket, started_ms, &poke, &log).await;
            let sleep = match looked {
                Ok(sleep) => {
                    failed = None;
                    sleep
                }
                Err(error) => {
                    if failed.as_ref() != Some(&error) {
                        log(&format!("scheduler: {error}"));
                    }
                    failed = Some(error);
                    RETRY
                }
            };
            tokio::select! {
                _ = tokio::time::sleep(sleep) => {}
                _ = wake.notified() => {}
            }
        }
    });
}

/// One look: settles what is due and starts what runs now. Returns how long
/// to sleep: until the nearest next time, at most a minute.
async fn look(
    manager: &Manager,
    socket: &Path,
    started_ms: u64,
    poke: &Arc<dyn Fn() + Send + Sync>,
    log: &Arc<dyn Fn(&str) + Send + Sync>,
) -> Result<Duration, String> {
    let mut store = manager.operation_store().await?;
    if !SETTLED.load(Ordering::SeqCst) {
        store.schedule_settle(started_ms, now_ms())?;
        SETTLED.store(true, Ordering::SeqCst);
    }
    // A zone the machine changed to applies from this look.
    schedule::cron::refresh_zone();
    let local = |ms| schedule::cron::system_local(ms);
    let mut sleep = LOOK_EVERY;
    for row in store.schedules()?.into_iter().filter(|row| row.enabled) {
        let spec = match Spec::parse(&row.spec) {
            Ok(spec) => spec,
            Err(error) => {
                log(&format!("scheduler: {}: {error}", row.name));
                continue;
            }
        };
        let now = now_ms() as i64;
        let lower = row.last_due.unwrap_or(i64::MIN).max(row.changed_ms);
        let due = schedule::due(&spec, row.created_ms, lower, now, &local);
        if due.run.is_some() || due.missed.is_some() {
            // One schedule's failure leaves the others to run; it is looked
            // at again soon, within the grace.
            match store.schedule_record(row.id, row.changed_ms, due.missed, due.run, now_ms()) {
                Ok(claim) => {
                    if claim.event {
                        poke();
                    }
                    if let Some(due_ms) = claim.run {
                        start(socket.to_owned(), row.clone(), due_ms, log.clone());
                    }
                }
                Err(error) => {
                    log(&format!("scheduler: {}: {error}", row.name));
                    sleep = sleep.min(RETRY);
                }
            }
        }
        if let Some(next) = due.next {
            let wait = Duration::from_millis(next.saturating_sub(now_ms() as i64).max(0) as u64);
            sleep = sleep.min(wait);
        }
    }
    Ok(sleep)
}

/// Runs a claimed action on a thread of its own and records its end.
fn start(socket: PathBuf, row: ScheduleRow, due_ms: i64, log: Arc<dyn Fn(&str) + Send + Sync>) {
    let spawned = std::thread::Builder::new()
        .name(format!("schedule-{}", row.id))
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => return log(&format!("schedule {}: {error}", row.name)),
            };
            runtime.block_on(async {
                let (state, result) = match perform(&socket, &row, due_ms).await {
                    Ok(result) if result["stage"] == "partial" || result["stage"] == "failed" => {
                        ("failed", result)
                    }
                    Ok(result) => ("done", result),
                    Err(error) => ("failed", json!({"error": error})),
                };
                let recorded = match Manager::new(socket.clone(), None) {
                    Ok(manager) => manager.operation_store().await.and_then(|mut store| {
                        store.schedule_finish(row.id, due_ms, state, &result, now_ms())
                    }),
                    Err(error) => Err(error),
                };
                if let Err(error) = recorded {
                    log(&format!("schedule {}: {error}", row.name));
                }
            });
        });
    if let Err(error) = spawned {
        // The row stays `running`; the next coordinator marks it unknown.
        eprintln!("schedule thread: {error}");
    }
}

async fn perform(socket: &Path, row: &ScheduleRow, due_ms: i64) -> Result<Value, String> {
    let action = &row.action;
    let text = |key: &str| {
        action[key]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| format!("schedule_invalid: the stored action has no {key}"))
    };
    match action["kind"].as_str() {
        Some("layout") => {
            let kept: crate::layout::Kept = serde_json::from_value(action["kept"].clone())
                .map_err(|error| format!("schedule_invalid: {error}"))?;
            crate::layout::apply_kept(&kept, socket.to_owned(), LOCK_WAIT).await
        }
        Some("start") => {
            let manager = Manager::new(socket.to_owned(), None)?.waiting_for_lock(LOCK_WAIT);
            let boot = manager.boot().await?;
            let id = format!("sched.{}.{due_ms}", row.id);
            let cwd = PathBuf::from(text("cwd")?);
            let dir = match action["worktree"].as_str() {
                Some(worktree) => crate::worktree::start_dir(worktree, &cwd, None)?,
                None => cwd,
            };
            manager
                .start_with_operation(
                    &text("agent")?,
                    &text("provider")?,
                    &dir,
                    &[],
                    None,
                    None,
                    Some(ClientKey {
                        pin: &boot,
                        id: &id,
                    }),
                    false,
                )
                .await
        }
        Some("queue") => {
            let manager = Manager::new(socket.to_owned(), None)?.waiting_for_lock(LOCK_WAIT);
            let agent = manager.get(&text("agent")?).await?;
            let drafts = crate::layout::drafts_for(Path::new(&text("path")?), &agent.name)?;
            let added = manager
                .queue_drafts(&agent.run, &agent.pane_id, &agent.provider, &drafts)
                .await?;
            Ok(json!({"agent": agent.name, "run": agent.run, "queued": added}))
        }
        _ => Err("schedule_invalid: unknown stored action".into()),
    }
}
