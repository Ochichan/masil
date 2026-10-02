//! Leases: which agent runs use a worktree. While any lease stays, masil
//! keeps git's lock on the worktree and refuses to remove it.

use super::create::{self, MARKER};
use super::git;
use super::registry::{LeaseRow, Registry, now_ms};
use super::servers::{self, Server};
use serde_json::Value;
use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long a starting run's lease counts as alive without its pane.
const LAUNCHING_MS: u64 = 30_000;
/// How long a start waits for the registry lock (N6).
const START_LOCK_WAIT: Duration = Duration::from_secs(5);
const SMALL_FILE: u64 = 4096;

/// A masil worktree that contains a start's directory, from its marker.
#[derive(Clone, Debug)]
pub(crate) struct Found {
    pub(super) registry: String,
    pub(super) worktree: i64,
    identity: (u64, u64),
    /// The worktree's top directory and its marker file.
    pub(super) root: PathBuf,
    pub(super) marker: PathBuf,
}

/// A lease a start holds while it launches.
pub(crate) struct Lease {
    id: i64,
    pub(crate) worktree: i64,
    pub(crate) name: String,
}

fn read_small(path: &Path) -> Option<String> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut text = String::new();
    file.take(SMALL_FILE).read_to_string(&mut text).ok()?;
    Some(text)
}

/// The masil worktree that contains `cwd`, found by walking up to `/`: a
/// `.git` file whose git directory holds masil's marker. A `.git` without
/// the marker (a main worktree, a nested clone, a submodule) is passed.
pub(crate) fn detect(cwd: &Path) -> Result<Option<Found>, String> {
    for dir in cwd.ancestors() {
        let dot_git = dir.join(".git");
        if !fs::symlink_metadata(&dot_git).is_ok_and(|metadata| metadata.is_file()) {
            continue;
        }
        let Some(text) = read_small(&dot_git) else {
            continue;
        };
        let Some(git_dir) = text.strip_prefix("gitdir:").map(str::trim) else {
            continue;
        };
        // git writes a relative path when `worktree.useRelativePaths` is on.
        let marker_path = dir.join(git_dir).join(MARKER);
        let Some(marker) = read_small(&marker_path) else {
            continue;
        };
        let Ok(marker) = serde_json::from_str::<Value>(&marker) else {
            continue;
        };
        let (Some(registry), Some(worktree)) =
            (marker["registry"].as_str(), marker["worktree"].as_i64())
        else {
            continue;
        };
        let metadata = fs::metadata(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
        return Ok(Some(Found {
            registry: registry.to_owned(),
            worktree,
            identity: (metadata.dev(), metadata.ino()),
            root: dir.to_owned(),
            marker: marker_path,
        }));
    }
    Ok(None)
}

/// The worktree `spec` names: a path inside one, or a name or branch in the
/// repository that contains `hint`.
pub(super) fn resolve(
    registry: &Registry,
    spec: &str,
    hint: &Path,
) -> Result<super::registry::Worktree, String> {
    if spec.contains('/') || spec == "." || spec == ".." {
        let path = hint
            .join(spec)
            .canonicalize()
            .map_err(|error| format!("invalid_argument: {spec}: {error}"))?;
        let found = detect(&path)?
            .ok_or_else(|| format!("worktree_unavailable: {spec} is not in a masil worktree"))?;
        return known(registry, &found);
    }
    let key = super::repository(
        hint.to_str()
            .ok_or("invalid_argument: the directory path is not UTF-8")?,
    )?
    .key;
    match registry.find(&key, spec)?.as_slice() {
        [] => Err(format!(
            "worktree_unavailable: no worktree named {spec} in the repository of {}",
            hint.display()
        )),
        [row] => Ok(row.clone()),
        rows => Err(format!(
            "invalid_argument: {spec} names {} worktrees; give one by name",
            rows.len()
        )),
    }
}

/// The directory a start with `--worktree SPEC` runs in: the worktree, or
/// `cwd` inside it.
pub(crate) fn start_dir(spec: &str, hint: &Path, cwd: Option<&Path>) -> Result<PathBuf, String> {
    let registry = Registry::open()?;
    let root = PathBuf::from(resolve(&registry, spec, hint)?.path);
    let Some(cwd) = cwd else {
        return Ok(root);
    };
    if cwd.is_absolute() {
        return Err(
            "invalid_argument: with a worktree, the directory is a relative path inside it".into(),
        );
    }
    let path = root
        .join(cwd)
        .canonicalize()
        .map_err(|error| format!("invalid_argument: {}: {error}", cwd.display()))?;
    if !path.starts_with(&root) {
        return Err(format!(
            "invalid_argument: {} is outside the worktree",
            cwd.display()
        ));
    }
    Ok(path)
}

/// The registry row a marker names, if this registry made it.
fn known(registry: &Registry, found: &Found) -> Result<super::registry::Worktree, String> {
    if registry.registry_id()? != found.registry {
        return Err(
            "worktree_registry_mismatch: this worktree was made with another worktree registry; \
             run with the XDG_STATE_HOME that made it, or `worktree forget` it"
                .into(),
        );
    }
    registry.worktree(found.worktree)?.ok_or_else(|| {
        "worktree_registry_mismatch: the worktree registry does not know this worktree; \
             `worktree forget` it"
            .into()
    })
}

/// Takes a lease for `run` before it launches. Refused unless the worktree
/// is ready and is the directory masil made.
pub(crate) fn begin(found: &Found, socket: &Path, boot: &str, run: &str) -> Result<Lease, String> {
    let mut registry = Registry::open()?;
    let socket = socket
        .to_str()
        .ok_or("invalid_argument: the server socket path is not UTF-8")?;
    let _lock = registry
        .lock_within(START_LOCK_WAIT)
        .map_err(|error| format!("lock_busy: {}", error.trim_start_matches("registry_busy: ")))?;
    let row = known(&registry, found)?;
    if row.state != "ready" {
        return Err(format!(
            "worktree_unavailable: worktree {} is {}",
            row.name, row.state
        ));
    }
    if row.identity != Some(found.identity) {
        return Err(format!(
            "worktree_unavailable: this directory is not the worktree {} masil made (a copy?)",
            row.name
        ));
    }
    let now = now_ms();
    let id = registry.insert_lease(row.id, socket, boot, run, now + LAUNCHING_MS, now)?;
    // git's lock keeps `git worktree remove` and `prune` off it; a failure
    // here leaves the lease, which is what masil itself checks.
    let _ = lock_in_git(&row);
    Ok(Lease {
        id,
        worktree: row.id,
        name: row.name,
    })
}

fn reason(worktree: i64) -> String {
    format!("masil worktree {worktree}")
}

fn lock_in_git(row: &super::registry::Worktree) -> Result<(), String> {
    let root = Path::new(&row.repo_root);
    let entry = create::entries(root)?
        .into_iter()
        .find(|entry| entry.path == row.path);
    if entry.is_some_and(|entry| entry.locked.is_none()) {
        git::query(
            root,
            &[
                "worktree",
                "lock",
                "--reason",
                &reason(row.id),
                "--",
                &row.path,
            ],
        )?;
    }
    Ok(())
}

/// Releases git's lock if masil set it. The caller holds the registry lock
/// and has checked that no lease is left.
pub(super) fn unlock_in_git(row: &super::registry::Worktree) -> Result<(), String> {
    let root = Path::new(&row.repo_root);
    let ours = create::entries(root)?
        .into_iter()
        .any(|entry| entry.path == row.path && entry.locked.as_deref() == Some(&reason(row.id)));
    if ours {
        git::query(root, &["worktree", "unlock", "--", &row.path])?;
    }
    Ok(())
}

/// The run started in `pane`.
pub(crate) fn launched(lease: &Lease, pane: &str) {
    if let Ok(mut registry) = Registry::open() {
        let _ = registry.set_lease_live(lease.id, pane);
    }
}

/// The run never started: its lease goes, and git's lock with the last one.
pub(crate) fn abandon(lease: &Lease) {
    let Ok(mut registry) = Registry::open() else {
        return;
    };
    let Ok(_lock) = registry.lock_within(START_LOCK_WAIT) else {
        return;
    };
    let Ok(leases) = registry.leases(Some(lease.worktree)) else {
        return;
    };
    if let Some(row) = leases.iter().find(|row| row.id == lease.id) {
        let _ = registry.end_lease(row, now_ms());
    }
    if leases.iter().all(|row| row.id == lease.id)
        && let Ok(Some(worktree)) = registry.worktree(lease.worktree)
    {
        let _ = unlock_in_git(&worktree);
    }
}

enum Liveness {
    Alive(String),
    Gone,
    Unknown,
}

fn liveness(lease: &LeaseRow, server: Option<&Server>) -> Liveness {
    match server {
        // A lease made after the servers were asked.
        None => Liveness::Unknown,
        Some(Server::Absent) => Liveness::Gone,
        Some(Server::Unknown(_)) => Liveness::Unknown,
        Some(Server::Panes(panes)) => panes
            .iter()
            .find(|pane| {
                !pane.dead && pane.boot == lease.boot && pane.run.as_deref() == Some(&lease.run)
            })
            .map_or(Liveness::Gone, |pane| Liveness::Alive(pane.pane.clone())),
    }
}

/// Drops leases whose runs have ended, and git's lock with a worktree's
/// last lease. Servers are asked outside the registry lock; each lease is
/// read again under it and judged only if it is unchanged since before the
/// servers were asked. A lease made or promoted since, or on a server that
/// does not answer, stays.
pub(super) fn prune(registry: &mut Registry, only: Option<i64>) -> Result<(), String> {
    let leases = registry.leases(only)?;
    if leases.is_empty() {
        return Ok(());
    }
    let seen: HashMap<i64, String> = leases
        .iter()
        .map(|lease| (lease.id, lease.state.clone()))
        .collect();
    let sockets: BTreeSet<String> = leases.iter().map(|lease| lease.socket.clone()).collect();
    let answers: HashMap<String, Server> = sockets
        .into_iter()
        .map(|socket| {
            let answer = servers::query(Path::new(&socket));
            (socket, answer)
        })
        .collect();
    let _lock = registry.lock()?;
    let now = now_ms();
    let mut emptied = BTreeSet::new();
    for lease in registry.leases(only)? {
        if seen.get(&lease.id) != Some(&lease.state) {
            continue;
        }
        match liveness(&lease, answers.get(&lease.socket)) {
            Liveness::Alive(pane) => {
                if lease.state == "launching" {
                    registry.set_lease_live(lease.id, &pane)?;
                }
            }
            // A start may still be making its pane.
            Liveness::Gone if lease.state == "launching" && lease.until_ms > now => {}
            Liveness::Gone => {
                registry.end_lease(&lease, now)?;
                emptied.insert(lease.worktree);
            }
            Liveness::Unknown => {}
        }
    }
    for worktree in emptied {
        if registry.leases(Some(worktree))?.is_empty()
            && let Some(row) = registry.worktree(worktree)?
        {
            let _ = unlock_in_git(&row);
        }
    }
    Ok(())
}
