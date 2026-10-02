//! The remove job. The worktree is marked `removing` first, so no new run
//! can start in it; then leases, panes on every server and the files are
//! checked; only then is it `deleting` and handed to `git worktree remove`.

use super::create::entries;
use super::git::{self, Ran};
use super::helper::CANCEL;
use super::lease;
use super::registry::{End, Job, Registry, Worktree, now_ms};
use super::servers::{self, Server};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::fs::File;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

/// Paths a refusal lists, per kind.
const SHOWN: usize = 200;

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
pub(super) struct Request {
    pub(super) worktree: i64,
    pub(super) force: bool,
    pub(super) confirm: Option<String>,
    pub(super) delete_branch: bool,
    /// The default socket directory, as the command that asked saw it.
    pub(super) socket_dir: Option<String>,
}

pub(super) fn run(registry: &mut Registry, job: &Job, log: &File) -> End {
    let request: Request = match serde_json::from_value(job.request.clone()) {
        Ok(request) => request,
        Err(error) => return End::Failed(format!("the job request is not readable: {error}")),
    };
    let id = request.worktree;
    match start(registry, job.id, id) {
        Ok(Some(row)) => match remove(registry, job, &request, &row, log) {
            Ok(end) => end,
            Err(end) => {
                // Nothing was deleted: the worktree is usable again.
                if let Ok(_lock) = registry.lock() {
                    let _ = registry.stop_removing(id);
                }
                end
            }
        },
        Ok(None) => End::Failed(format!("worktree {id} is not in the registry")),
        Err(end) => end,
    }
}

/// Step 1: `ready` (or a half-deleted `deleting`) becomes this job's.
fn start(registry: &mut Registry, job: i64, id: i64) -> Result<Option<Worktree>, End> {
    let _lock = registry.lock().map_err(End::Failed)?;
    match registry.begin_removing(id, job).map_err(End::Failed)? {
        None => Ok(None),
        Some(state) if state == "ready" || state == "deleting" => {
            registry.worktree(id).map_err(End::Failed)
        }
        Some(state) => Err(End::Failed(format!(
            "worktree_unavailable: worktree {id} is {state}"
        ))),
    }
}

/// Steps 2-8. An `Err` means nothing was deleted.
fn remove(
    registry: &mut Registry,
    job: &Job,
    request: &Request,
    row: &Worktree,
    log: &File,
) -> Result<End, End> {
    let failed = End::Failed;
    let path = Path::new(&row.path);
    // Step 2: every server that might have a pane in it, fixed before any
    // lease is dropped.
    let mut sockets: BTreeSet<PathBuf> = request
        .socket_dir
        .as_deref()
        .map(|dir| servers::sockets_in(Path::new(dir)))
        .unwrap_or_default()
        .into_iter()
        .collect();
    sockets.extend(
        registry
            .run_sockets(row.id)
            .map_err(failed)?
            .into_iter()
            .map(PathBuf::from),
    );
    // Step 3: runs that use it.
    lease::prune(registry, Some(row.id)).map_err(failed)?;
    if let Some(lease) = registry.leases(Some(row.id)).map_err(failed)?.first() {
        return Err(End::Failed(format!(
            "worktree_in_use: run {} on {} uses it (lease {})",
            lease.run, lease.socket, lease.state
        )));
    }
    // Step 4: any pane in it, agent or not.
    let exists = std::fs::symlink_metadata(path).is_ok();
    if exists && let Some(identity) = row.identity {
        for socket in &sockets {
            match servers::query(socket) {
                Server::Absent => {}
                Server::Unknown(error) => {
                    return Err(End::Failed(format!(
                        "server_unanswered: {}: {error}",
                        socket.display()
                    )));
                }
                Server::Panes(panes) => {
                    if let Some(pane) = panes
                        .iter()
                        .find(|pane| !pane.dead && servers::inside(Path::new(&pane.path), identity))
                    {
                        return Err(End::Failed(format!(
                            "worktree_in_use: pane {} on {} is in it",
                            pane.pane,
                            socket.display()
                        )));
                    }
                }
            }
        }
    }
    let identity_now = std::fs::metadata(path)
        .ok()
        .map(|metadata| (metadata.dev(), metadata.ino()));
    if exists && identity_now != row.identity {
        return Err(End::Failed(format!(
            "worktree_unavailable: {} is not the directory masil made",
            row.path
        )));
    }
    // Step 5: what would be lost.
    let root = Path::new(&row.repo_root);
    let entry = entries(root)
        .map_err(failed)?
        .into_iter()
        .find(|entry| entry.path == row.path);
    if let Some(reason) = entry.as_ref().and_then(|entry| entry.locked.as_deref())
        && reason != lease_reason(row.id)
    {
        return Err(End::Failed(format!(
            "worktree_locked: git has it locked ({reason}); unlock it first"
        )));
    }
    if exists && entry.is_none() {
        return Err(End::Failed(format!(
            "worktree_unavailable: git no longer lists {} as a worktree (its record in the \
             repository is gone); masil leaves its files alone",
            row.path
        )));
    }
    // A half-deleted worktree is removed again only on request, like one
    // with changes.
    let deleting = row.state == "deleting";
    let mut changed = false;
    if exists && entry.is_some() {
        let check = files(registry, row).map_err(failed)?;
        changed = !check.changes.is_empty() || deleting;
        if changed && !request.force {
            return Err(End::Refused(
                format!(
                    "worktree_dirty: {} has {} changed or untracked files{}; nothing more was \
                     removed. To remove them too, rerun with --force --confirm {}",
                    row.name,
                    check.changes.len(),
                    if deleting {
                        " and was partly removed"
                    } else {
                        ""
                    },
                    check.token
                ),
                check.to_json(),
            ));
        }
        let asked = changed || !check.ignored.is_empty();
        if asked && request.confirm.as_deref() != Some(check.token.as_str()) {
            let what = if changed {
                "changed, untracked and ignored files"
            } else {
                "ignored files"
            };
            return Err(End::Refused(
                format!(
                    "worktree_dirty: {} has {what} that would be deleted; nothing was removed. \
                     After checking the list, rerun with {}--confirm {}",
                    row.name,
                    if changed { "--force " } else { "" },
                    check.token
                ),
                check.to_json(),
            ));
        }
    }
    if CANCEL.load(Ordering::SeqCst) {
        return Err(End::Cancelled(
            "cancelled before anything was removed".into(),
        ));
    }
    // Step 6: still this job's, still unused; masil's git lock goes.
    {
        let _lock = registry.lock().map_err(failed)?;
        let current = registry
            .worktree(row.id)
            .map_err(failed)?
            .ok_or_else(|| End::Failed("the worktree row disappeared".into()))?;
        let mine = registry.removing_job(row.id).map_err(failed)? == Some(job.id);
        if !matches!(current.state.as_str(), "removing" | "deleting") || !mine {
            return Err(End::Failed(format!(
                "worktree {} changed while it was checked ({})",
                row.name, current.state
            )));
        }
        if !registry.leases(Some(row.id)).map_err(failed)?.is_empty() {
            return Err(End::Failed(format!(
                "worktree_in_use: a run started in {} while it was checked",
                row.name
            )));
        }
        lease::unlock_in_git(&current).map_err(failed)?;
        registry
            .set_state(row.id, &["removing", "deleting"], "deleting", now_ms())
            .map_err(failed)?;
    }
    // Step 7. From here on it is no longer ready, whatever happens.
    Ok(delete(
        registry,
        job,
        request,
        row,
        entry.is_some(),
        exists,
        changed,
        log,
    ))
}

#[allow(clippy::too_many_arguments)]
fn delete(
    registry: &mut Registry,
    job: &Job,
    request: &Request,
    row: &Worktree,
    listed: bool,
    exists: bool,
    changed: bool,
    log: &File,
) -> End {
    let root = Path::new(&row.repo_root);
    if listed {
        let mut args = vec!["worktree", "remove"];
        if !exists {
            // Only git's record is left, perhaps still under git's lock.
            args.extend(["--force", "--force"]);
        } else if changed {
            // Confirmed with the token: changed and untracked files go too.
            args.push("--force");
        }
        args.extend(["--", row.path.as_str()]);
        let _ = writeln!(&*log, "$ git {}", args.join(" "));
        let started = |pid: i32| {
            let child = crate::process::info(pid).map(|info| (pid, info.started));
            let _ = registry.set_child(job.id, child);
        };
        let ended = || {
            let _ = registry.set_child(job.id, None);
        };
        match git::run_logged(root, &args, log, &CANCEL, &started, &ended) {
            Ok(Ran::Exited(status)) if status.success() => {}
            Ok(Ran::Exited(status)) => {
                return End::Failed(format!(
                    "git worktree remove failed ({status}); {} is left as deleting, see `worktree jobs {}`",
                    row.name, job.id
                ));
            }
            Ok(Ran::Cancelled) => {
                return End::Cancelled(format!(
                    "cancelled while git removed it; {} is left as deleting",
                    row.name
                ));
            }
            Err(error) => return End::Failed(error),
        }
    } else if exists {
        return End::Failed(format!(
            "git no longer knows {}; its files stay at {} and it is left as deleting",
            row.name, row.path
        ));
    }
    if let Err(error) = registry.set_state(row.id, &["deleting"], "removed", now_ms()) {
        return End::Failed(error);
    }
    let mut result = row.to_json();
    result["state"] = json!("removed");
    if request.delete_branch {
        let (deleted, note) = delete_branch(row);
        result["branch_deleted"] = json!(deleted);
        if let Some(note) = note {
            result["branch_note"] = json!(note);
        }
    }
    End::Succeeded(result)
}

fn delete_branch(row: &Worktree) -> (bool, Option<String>) {
    if !row.branch_created {
        return (
            false,
            Some(format!(
                "masil did not make branch {}; it stays",
                row.branch
            )),
        );
    }
    // `-d`: git keeps a branch that is not merged.
    match git::query(Path::new(&row.repo_root), &["branch", "-d", &row.branch]) {
        Ok(_) => (true, None),
        Err(error) => (false, Some(error)),
    }
}

fn lease_reason(worktree: i64) -> String {
    format!("masil worktree {worktree}")
}

/// What `git worktree remove` would delete.
struct Files {
    /// Tracked changes and untracked files, as `XY path`.
    changes: Vec<String>,
    /// Ignored paths that setup did not put there.
    ignored: Vec<String>,
    token: String,
}

impl Files {
    fn to_json(&self) -> Value {
        json!({
            "changes": self.changes.iter().take(SHOWN).collect::<Vec<_>>(),
            "changes_count": self.changes.len(),
            "ignored": self.ignored.iter().take(SHOWN).collect::<Vec<_>>(),
            "ignored_count": self.ignored.len(),
            "token": self.token,
        })
    }
}

/// Entries of `git status --porcelain=v2 -z`, shown as `XY path`, `?? path`
/// or `!! path`; a rename or copy carries its source.
fn status_entries(output: &str) -> Vec<String> {
    let mut entries = Vec::new();
    let mut fields = output.split('\0').filter(|field| !field.is_empty());
    while let Some(field) = fields.next() {
        let parts: Vec<&str> = field.splitn(11, ' ').collect();
        let entry = match parts.as_slice() {
            ["1", xy, rest @ ..] if rest.len() >= 6 => {
                // `1 XY sub mH mI mW hH hI path`; the path may hold spaces.
                let path = field.splitn(9, ' ').nth(8).unwrap_or_default();
                format!("{xy} {path}")
            }
            ["2", xy, ..] => {
                // `2 XY sub mH mI mW hH hI Xscore path`, then the source.
                let path = field.splitn(10, ' ').nth(9).unwrap_or_default();
                let from = fields.next().unwrap_or_default();
                format!("{xy} {path} (from {from})")
            }
            ["u", xy, ..] => {
                let path = field.splitn(11, ' ').nth(10).unwrap_or_default();
                format!("{xy} {path}")
            }
            ["?", ..] => format!("?? {}", &field[2..]),
            ["!", ..] => format!("!! {}", &field[2..]),
            _ => continue,
        };
        entries.push(entry);
    }
    entries
}

fn files(registry: &Registry, row: &Worktree) -> Result<Files, String> {
    let path = Path::new(&row.path);
    let mut changes = status_entries(&git::query(
        path,
        &[
            "status",
            "--porcelain=v2",
            "-z",
            "--untracked-files=all",
            "--ignored=no",
            "--ignore-submodules=none",
        ],
    )?);
    changes.sort();
    let setup: BTreeSet<String> = registry.setup_paths(row.id)?.into_iter().collect();
    let mut ignored: Vec<String> = status_entries(&git::query(
        path,
        &[
            "status",
            "--porcelain=v2",
            "-z",
            "--untracked-files=normal",
            "--ignored=matching",
            "--ignore-submodules=none",
        ],
    )?)
    .into_iter()
    .filter_map(|entry| entry.strip_prefix("!! ").map(str::to_owned))
    .filter(|ignored| !setup.contains(ignored.trim_end_matches('/')))
    .collect();
    ignored.sort();
    let head = git::query(path, &["rev-parse", "--verify", "HEAD"]).unwrap_or_default();
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    let identity = row.identity.unwrap_or_default();
    digest.update(format!(
        "{}\0{}:{}\0{}\0",
        row.id,
        identity.0,
        identity.1,
        head.trim()
    ));
    for entry in changes
        .iter()
        .chain(std::iter::once(&String::from("--")))
        .chain(&ignored)
    {
        digest.update(entry.as_bytes());
        digest.update(b"\0");
    }
    let token: String = digest.finalize()[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok(Files {
        changes,
        ignored,
        token,
    })
}

/// Settles a remove job whose helper died. Before step 6 nothing was
/// deleted, so the worktree is ready again; after it, only git can say.
pub(super) fn settle(registry: &mut Registry, job: &Job) -> (&'static str, String) {
    let Some(id) = job.request["worktree"].as_i64() else {
        return ("failed", "its helper ended".into());
    };
    let Ok(Some(row)) = registry.worktree(id) else {
        return ("failed", "its helper ended".into());
    };
    if registry.removing_job(id).ok().flatten() != Some(job.id) {
        return ("failed", "its helper ended".into());
    }
    match row.state.as_str() {
        "removing" => {
            let _ = registry.stop_removing(id);
            (
                "failed",
                "its helper ended before anything was removed".into(),
            )
        }
        "deleting" => {
            let gone = std::fs::symlink_metadata(&row.path).is_err();
            let listed = entries(Path::new(&row.repo_root))
                .map(|entries| entries.iter().any(|entry| entry.path == row.path));
            if gone && matches!(listed, Ok(false)) {
                let _ = registry.set_state(id, &["deleting"], "removed", now_ms());
                (
                    "succeeded",
                    "its helper ended after the worktree was removed".into(),
                )
            } else {
                let _ = registry.stop_removing(id);
                (
                    "outcome_unknown",
                    format!(
                        "its helper ended while git removed it; {} is left as deleting, run `worktree remove` again",
                        row.name
                    ),
                )
            }
        }
        _ => ("failed", "its helper ended".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::status_entries;

    #[test]
    fn status_v2_entries_keep_paths_and_rename_sources() {
        let output = "1 .M N... 100644 100644 100644 a b my file.txt\0\
                      2 R. N... 100644 100644 100644 a b R100 new name\0old name\0\
                      2 .R N... 100644 100644 100644 a b R100 Rnew\0README\0\
                      u UU N... 100644 100644 100644 100644 a b c both.txt\0\
                      ? zz untracked\0! node_modules/\0";
        assert_eq!(
            status_entries(output),
            [
                ".M my file.txt",
                "R. new name (from old name)",
                ".R Rnew (from README)",
                "UU both.txt",
                "?? zz untracked",
                "!! node_modules/",
            ]
        );
    }
}
