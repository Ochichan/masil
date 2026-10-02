//! Checkpoints of an agent's working tree (P6b, docs/checkpoints.md): the
//! files as they are, stored in masil's own repository so the user's
//! refs, index and objects are never touched.

mod restore;
mod store;

use crate::worktree::git::{self, Output};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
pub(crate) use store::Store;

/// Checkpoints kept per working tree.
const KEEP: usize = 100;
const LOCK_WAIT: Duration = Duration::from_secs(5);
const MAKE_TIME: Duration = Duration::from_secs(60);
const DEFAULT_AUTO_TIME: Duration = Duration::from_secs(10);
const MIB: u64 = 1024 * 1024;
/// Excluded paths a checkpoint's metadata and listing show.
const SHOWN_EXCLUDED: usize = 200;
const LIST_LIMIT: usize = 256 * 1024 * 1024;
const DIFF_LIMIT: usize = 256 * 1024;
/// Bytes of the skipped-checkpoint log kept.
const EVENTS_LIMIT: u64 = 64 * 1024;
/// Names restore uses for its temporary files; never saved.
pub(crate) const RESTORE_PREFIX: &str = ".masil-restore-";
const TIMEOUT: &str = "checkpoint_timeout: the checkpoint did not finish in time";

pub(crate) fn now_ms() -> u64 {
    crate::observation::now_ms()
}

/// `~/.config/masil/checkpoints.toml`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Settings {
    #[serde(default)]
    pub(crate) before_prompt: bool,
    max_file_mb: Option<u64>,
    max_total_mb: Option<u64>,
    auto_timeout_s: Option<u64>,
}

impl Settings {
    fn path() -> Option<PathBuf> {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .filter(|path| path.is_absolute())
                    .map(|home| home.join(".config"))
            })
            .map(|base| base.join("masil/checkpoints.toml"))
    }

    /// The user's settings; defaults when there is no file. The file must
    /// be the user's and not writable by others.
    pub(crate) fn load() -> Result<Self, String> {
        let Some(path) = Self::path() else {
            return Ok(Self::default());
        };
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(format!("{}: {error}", path.display())),
        };
        let metadata = file
            .metadata()
            .map_err(|error| format!("{}: {error}", path.display()))?;
        // SAFETY: geteuid has no preconditions.
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o022 != 0
        {
            return Err(format!(
                "{} must be a regular file owned by you and not writable by group or others",
                path.display()
            ));
        }
        let mut text = String::new();
        file.take(64 * 1024)
            .read_to_string(&mut text)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        toml::from_str(&text)
            .map_err(|error| format!("invalid_argument: {}: {error}", path.display()))
    }

    fn limits(&self) -> Limits {
        Limits {
            file: self.max_file_mb.unwrap_or(10).saturating_mul(MIB),
            total: self.max_total_mb.unwrap_or(512).saturating_mul(MIB),
        }
    }

    fn auto_time(&self) -> Duration {
        self.auto_timeout_s
            .map(|seconds| Duration::from_secs(seconds.clamp(1, 300)))
            .unwrap_or(DEFAULT_AUTO_TIME)
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    file: u64,
    total: u64,
}

/// The working tree as a tree object in the checkpoint repository.
pub(crate) struct Snapshot {
    pub(crate) tree: String,
    pub(crate) files: usize,
    pub(crate) bytes: u64,
    /// Why and which paths were left out, in path order, as git names
    /// them (bytes, not necessarily UTF-8).
    pub(crate) excluded: Vec<(&'static str, Vec<u8>)>,
}

fn run(
    store: &Store,
    work: bool,
    args: &[&str],
    input: Option<Vec<u8>>,
    deadline: Instant,
    limit: usize,
) -> Result<Output, String> {
    let mut command = store.git(work);
    command.args(args);
    git::run_within(command, input, &[0], deadline, limit, TIMEOUT)
}

fn text(output: Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// What the user's git counts as the working tree's files: tracked ones,
/// and untracked ones the user's ignore rules do not exclude. Read-only.
fn eligible(root: &Path, deadline: Instant) -> Result<Vec<Vec<u8>>, String> {
    let output = git::output_within(
        root,
        &[
            "-c",
            "core.fsmonitor=false",
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
            "--deduplicate",
        ],
        &[0],
        deadline,
        LIST_LIMIT,
        TIMEOUT,
    )?;
    if output.truncated {
        return Err("checkpoint_too_large: the working tree lists too many files".into());
    }
    Ok(output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(<[u8]>::to_vec)
        .collect())
}

/// The files a checkpoint takes, and those it leaves out and why.
struct Selection {
    included: Vec<Vec<u8>>,
    excluded: Vec<(&'static str, Vec<u8>)>,
    bytes: u64,
}

fn timed_out(error: &str) -> bool {
    error.starts_with("checkpoint_timeout")
}

/// The eligible files within the limits, plus `force` regardless of limits
/// and ignore rules.
fn select(
    store: &Store,
    limits: Limits,
    force: &[String],
    deadline: Instant,
) -> Result<Selection, String> {
    use std::os::unix::ffi::OsStrExt;
    let mut paths = eligible(&store.root, deadline)?;
    for path in force {
        if !paths.iter().any(|known| known == path.as_bytes()) {
            paths.push(path.as_bytes().to_vec());
        }
    }
    paths.sort();
    let forced: BTreeSet<&[u8]> = force.iter().map(|path| path.as_bytes()).collect();
    // Whether a leading directory is a real directory, not a symlink: git
    // refuses a path beyond a symlink.
    let mut real: HashMap<Vec<u8>, bool> = HashMap::new();
    let mut selection = Selection {
        included: Vec::new(),
        excluded: Vec::new(),
        bytes: 0,
    };
    for path in &paths {
        let base = path.rsplit(|byte| *byte == b'/').next().unwrap_or(path);
        if base.starts_with(RESTORE_PREFIX.as_bytes()) {
            continue;
        }
        let mut plain = true;
        for (index, byte) in path.iter().enumerate() {
            if *byte != b'/' {
                continue;
            }
            let parent = &path[..index];
            let is_real = *real.entry(parent.to_vec()).or_insert_with(|| {
                fs::symlink_metadata(store.root.join(std::ffi::OsStr::from_bytes(parent)))
                    .is_ok_and(|metadata| metadata.is_dir())
            });
            if !is_real {
                plain = false;
                break;
            }
        }
        if !plain {
            selection.excluded.push(("beyond_symlink", path.clone()));
            continue;
        }
        let Ok(metadata) = fs::symlink_metadata(store.root.join(std::ffi::OsStr::from_bytes(path)))
        else {
            // Gone since it was listed, or deleted but still tracked.
            continue;
        };
        let kind = metadata.file_type();
        if kind.is_dir() {
            selection.excluded.push(("submodule", path.clone()));
            continue;
        }
        if !(kind.is_file() || kind.is_symlink()) {
            selection.excluded.push(("special", path.clone()));
            continue;
        }
        let size = if kind.is_file() { metadata.len() } else { 0 };
        let is_forced = forced.contains(path.as_slice());
        if !is_forced && size > limits.file {
            selection.excluded.push(("too_large", path.clone()));
            continue;
        }
        if !is_forced && selection.bytes.saturating_add(size) > limits.total {
            selection.excluded.push(("total_limit", path.clone()));
            continue;
        }
        selection.bytes += size;
        selection.included.push(path.clone());
    }
    Ok(selection)
}

/// Brings the working tree's checkpoint index to the selected files and
/// writes it as a tree. The caller holds the store's locks.
pub(crate) fn snapshot(
    store: &Store,
    limits: Limits,
    force: &[String],
    deadline: Instant,
) -> Result<Snapshot, String> {
    let mut selection = select(store, limits, force, deadline)?;
    if let Err(error) = sync_index(store, &selection.included, deadline) {
        if timed_out(&error) {
            return Err(error);
        }
        // A path changed kind since it was listed, or the index is damaged:
        // list again, into a fresh index.
        let _ = fs::remove_file(store.index());
        selection = select(store, limits, force, deadline)?;
        sync_index(store, &selection.included, deadline)?;
    }
    let tree = match run(store, true, &["write-tree"], None, deadline, 4096) {
        Ok(tree) => text(tree),
        Err(error) if timed_out(&error) => return Err(error),
        Err(_) => {
            // The index may name objects that are gone; hash everything again.
            let _ = fs::remove_file(store.index());
            sync_index(store, &selection.included, deadline)?;
            text(run(store, true, &["write-tree"], None, deadline, 4096)?)
        }
    };
    Ok(Snapshot {
        tree,
        files: selection.included.len(),
        bytes: selection.bytes,
        excluded: selection.excluded,
    })
}

fn sync_index(store: &Store, included: &[Vec<u8>], deadline: Instant) -> Result<(), String> {
    let current = run(store, true, &["ls-files", "-z"], None, deadline, LIST_LIMIT)?;
    let keep: BTreeSet<&[u8]> = included.iter().map(Vec::as_slice).collect();
    let mut removed = Vec::new();
    for path in current.stdout.split(|byte| *byte == 0) {
        if !path.is_empty() && !keep.contains(path) {
            removed.extend_from_slice(path);
            removed.push(0);
        }
    }
    if !removed.is_empty() {
        run(
            store,
            true,
            &["update-index", "--force-remove", "-z", "--stdin"],
            Some(removed),
            deadline,
            64 * 1024,
        )?;
    }
    let mut list = Vec::new();
    for path in included {
        list.extend_from_slice(path);
        list.push(0);
    }
    // Unchanged files are skipped by their stat data; a file that vanished
    // since it was listed leaves the index.
    run(
        store,
        true,
        &["update-index", "--add", "--remove", "-z", "--stdin"],
        Some(list),
        deadline,
        64 * 1024,
    )
    .map(drop)
}

/// Who a checkpoint was made for and why.
pub(crate) struct Request<'a> {
    pub(crate) reason: &'a str,
    pub(crate) auto: bool,
    pub(crate) agent: Option<(&'a str, &'a str)>,
    /// Paths saved whatever the limits and ignore rules say.
    pub(crate) force: &'a [String],
    /// For an automatic checkpoint: when its prompt was sent.
    pub(crate) prompt_ms: Option<u64>,
}

pub(crate) struct Made {
    pub(crate) id: u64,
    pub(crate) commit: String,
    pub(crate) snapshot: Snapshot,
    pub(crate) started_ms: u64,
    pub(crate) finished_ms: u64,
}

impl Made {
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "commit": self.commit,
            "files": self.snapshot.files,
            "bytes": self.snapshot.bytes,
            "excluded_count": self.snapshot.excluded.len(),
            "excluded": shown_excluded(&self.snapshot.excluded),
            "started_ms": self.started_ms,
            "finished_ms": self.finished_ms,
        })
    }
}

fn shown_excluded(excluded: &[(&str, Vec<u8>)]) -> Vec<Value> {
    excluded
        .iter()
        .take(SHOWN_EXCLUDED)
        .map(|(why, path)| json!({"path": String::from_utf8_lossy(path), "why": why}))
        .collect()
}

/// HEAD and branch of the user's repository, for the record.
fn head(root: &Path, deadline: Instant) -> Value {
    let read = |args: &[&str]| {
        git::output_within(root, args, &[0, 1], deadline, 4096, TIMEOUT)
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
            .ok()
            .filter(|text| !text.is_empty())
    };
    json!({
        "oid": read(&["rev-parse", "--verify", "-q", "HEAD"]),
        "branch": read(&["symbolic-ref", "-q", "--short", "HEAD"]),
    })
}

/// The checkpoint numbers of this working tree, newest first.
fn numbers(store: &Store, deadline: Instant) -> Result<Vec<(u64, String)>, String> {
    let prefix = store.ref_prefix();
    let output = run(
        store,
        false,
        &["for-each-ref", "--format=%(refname) %(objectname)", &prefix],
        None,
        deadline,
        LIST_LIMIT,
    )?;
    let mut numbers: Vec<(u64, String)> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let (name, oid) = line.split_once(' ')?;
            Some((name.strip_prefix(&prefix)?.parse().ok()?, oid.to_owned()))
        })
        .collect();
    numbers.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    Ok(numbers)
}

/// Makes a checkpoint. The caller holds the store's locks (shared
/// repository lock and this working tree's lock).
pub(crate) fn make(
    store: &Store,
    request: &Request<'_>,
    limits: Limits,
    deadline: Instant,
) -> Result<Made, String> {
    let started_ms = now_ms();
    store.clear_stale_locks();
    let result = (|| -> Result<Made, String> {
        let snapshot = snapshot(store, limits, request.force, deadline)?;
        let mut excluded = Vec::new();
        for (why, path) in &snapshot.excluded {
            excluded.extend_from_slice(why.as_bytes());
            excluded.push(0);
            excluded.extend_from_slice(path);
            excluded.push(0);
        }
        let list = text(run(
            store,
            false,
            &["hash-object", "-w", "--stdin"],
            Some(excluded),
            deadline,
            4096,
        )?);
        let root = text(run(
            store,
            false,
            &["mktree", "-z"],
            Some(format!("040000 tree {}\tw\0100644 blob {list}\tx\0", snapshot.tree).into_bytes()),
            deadline,
            4096,
        )?);
        let finished_ms = now_ms();
        let metadata = json!({
            "version": 1,
            "worktree": store.root,
            "reason": request.reason,
            "auto": request.auto,
            "agent": request.agent.map(|(name, run)| json!({"name": name, "run": run})),
            "head": head(&store.root, deadline),
            "files": snapshot.files,
            "bytes": snapshot.bytes,
            "excluded_count": snapshot.excluded.len(),
            "started_ms": started_ms,
            "prompt_ms": request.prompt_ms,
            "finished_ms": finished_ms,
        });
        // git takes a blank first line for no subject, and the metadata with it.
        let subject: String = request
            .reason
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("checkpoint")
            .chars()
            .filter(|c| !c.is_control())
            .take(200)
            .collect();
        let commit = text(run(
            store,
            false,
            &["commit-tree", &root],
            Some(format!("{subject}\n\n{metadata}\n").into_bytes()),
            deadline,
            4096,
        )?);
        let id = numbers(store, deadline)?.first().map_or(1, |(n, _)| n + 1);
        run(
            store,
            false,
            &[
                "update-ref",
                &format!("{}{id:010}", store.ref_prefix()),
                &commit,
                "",
            ],
            None,
            deadline,
            4096,
        )?;
        Ok(Made {
            id,
            commit,
            snapshot,
            started_ms,
            finished_ms,
        })
    })();
    if let Err(error) = &result
        && !timed_out(error)
    {
        // Whatever the index holds may now name objects gc can drop. A
        // timeout keeps it: its stat data still saves the next run time.
        let _ = fs::remove_file(store.index());
    }
    result
}

/// Drops checkpoints past the newest [`KEEP`] of this working tree and lets
/// git tidy up, only when no checkpoint is being made in the repository.
pub(crate) fn tidy(store: &Store) {
    let Ok((_repo, _)) = store.lock(true, Duration::from_secs(1)) else {
        return;
    };
    let deadline = Instant::now() + MAKE_TIME;
    // With the exclusive lock no git of ours runs here: a ref lock file is
    // left from one that was killed, and would stop every deletion.
    let _ = fs::remove_file(store.repo.join("packed-refs.lock"));
    store.clear_ref_locks();
    if let Ok(numbers) = numbers(store, deadline) {
        for (number, _) in numbers.iter().skip(KEEP) {
            let _ = run(
                store,
                false,
                &[
                    "update-ref",
                    "-d",
                    &format!("{}{number:010}", store.ref_prefix()),
                ],
                None,
                deadline,
                4096,
            );
        }
    }
    let _ = run(
        store,
        false,
        &[
            "-c",
            "gc.pruneExpire=2.weeks.ago",
            "gc",
            "--auto",
            "--quiet",
        ],
        None,
        deadline,
        64 * 1024,
    );
}

/// The checkpoint `id` of this working tree.
fn commit_of(store: &Store, id: u64, deadline: Instant) -> Result<String, String> {
    numbers(store, deadline)?
        .into_iter()
        .find(|(number, _)| *number == id)
        .map(|(_, oid)| oid)
        .ok_or_else(|| {
            format!("checkpoint_absent: there is no checkpoint {id} of this working tree")
        })
}

fn metadata_of(store: &Store, commit: &str, deadline: Instant) -> Value {
    run(
        store,
        false,
        &["log", "-1", "--format=%b", commit],
        None,
        deadline,
        64 * 1024,
    )
    .ok()
    .and_then(|output| serde_json::from_slice(output.stdout.trim_ascii()).ok())
    .unwrap_or(Value::Null)
}

/// An automatic checkpoint that began this long after its prompt may hold
/// the agent's first edits.
const LATE_MS: u64 = 1000;

/// Checkpoints of this working tree, newest first, with skipped automatic
/// ones and an interrupted restore.
pub(crate) fn list(store: &Store) -> Result<Value, String> {
    let deadline = Instant::now() + MAKE_TIME;
    let mut entries = Vec::new();
    for (id, commit) in numbers(store, deadline)? {
        let mut entry = metadata_of(store, &commit, deadline);
        if !entry.is_object() {
            entry = json!({});
        }
        if let (Some(prompt), Some(started)) =
            (entry["prompt_ms"].as_u64(), entry["started_ms"].as_u64())
            && started > prompt + LATE_MS
        {
            entry["may_include_prompt_effects"] = json!(true);
        }
        entry["id"] = json!(id);
        entry["commit"] = json!(commit);
        entries.push(entry);
    }
    let skipped: Vec<Value> = events(store)
        .into_iter()
        .filter(|event| event["worktree_key"] == json!(store.worktree_key))
        .collect();
    let mut value = json!({"root": store.root, "checkpoints": entries, "skipped": skipped});
    if let Some(journal) = restore::interrupted(store) {
        value["interrupted_restore"] = journal;
    }
    Ok(value)
}

/// One checkpoint and how the working tree differs from it now; with a
/// path, that file's diff from the checkpoint to now.
pub(crate) fn show(
    store: &Store,
    id: u64,
    file: Option<&str>,
    limits: Limits,
) -> Result<Value, String> {
    if let Some(path) = file {
        crate::changes::check_relative(path)?;
    }
    let deadline = Instant::now() + MAKE_TIME;
    let commit = commit_of(store, id, deadline)?;
    let (_repo, _worktree) = store.lock(false, LOCK_WAIT)?;
    store.clear_stale_locks();
    let now = snapshot(store, limits, &[], deadline)?;
    let then = format!("{commit}:w");
    let mut value = metadata_of(store, &commit, deadline);
    if !value.is_object() {
        value = json!({});
    }
    value["id"] = json!(id);
    value["commit"] = json!(commit);
    // Left out now (too large, behind a symlink): not deleted.
    let excluded_now: HashMap<String, &str> = now
        .excluded
        .iter()
        .map(|(why, path)| (String::from_utf8_lossy(path).into_owned(), *why))
        .collect();
    let kinds = run(
        store,
        false,
        &[
            "diff-tree",
            "-r",
            "-z",
            "-M",
            "--name-status",
            &then,
            &now.tree,
        ],
        None,
        deadline,
        LIST_LIMIT,
    )?;
    let mut changes: Vec<(String, Option<String>, &str)> = Vec::new();
    let mut fields = kinds
        .stdout
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty());
    while let Some(status) = fields.next() {
        let status = String::from_utf8_lossy(status).into_owned();
        let renamed = status.starts_with('R') || status.starts_with('C');
        let from = if renamed {
            fields
                .next()
                .map(|path| String::from_utf8_lossy(path).into_owned())
        } else {
            None
        };
        let Some(path) = fields
            .next()
            .map(|path| String::from_utf8_lossy(path).into_owned())
        else {
            break;
        };
        let kind = match status.chars().next() {
            Some('A') => "added",
            Some('D') if excluded_now.contains_key(&path) => "excluded_now",
            Some('D') => "deleted",
            Some('R') => "renamed",
            Some('C') => "copied",
            Some('T') => "type_changed",
            _ => "modified",
        };
        changes.push((path, from, kind));
    }
    if let Some(path) = file {
        // The pair, so a renamed file shows as a rename.
        let mut pathspec = vec![path.to_owned()];
        if let Some((_, Some(from), _)) = changes.iter().find(|(name, _, _)| name == path) {
            pathspec.push(from.clone());
        }
        let mut args = vec![
            "diff-tree",
            "-p",
            "-M",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            &then,
            &now.tree,
            "--",
        ];
        args.extend(pathspec.iter().map(String::as_str));
        let diff = run(store, false, &args, None, deadline, DIFF_LIMIT)?;
        value["path"] = json!(path);
        value["truncated"] = json!(diff.truncated);
        value["diff"] = json!(String::from_utf8_lossy(&diff.stdout));
        return Ok(value);
    }
    let counts = run(
        store,
        false,
        &["diff-tree", "-r", "-z", "-M", "--numstat", &then, &now.tree],
        None,
        deadline,
        LIST_LIMIT,
    )?;
    let counts = numstat(&counts.stdout);
    let files: Vec<Value> = changes
        .iter()
        .take(crate::changes::SHOWN)
        .map(|(path, from, kind)| {
            let (insertions, deletions, binary) =
                counts.get(path).copied().unwrap_or((None, None, false));
            let mut entry = json!({"path": path, "kind": kind, "insertions": insertions,
                                   "deletions": deletions, "binary": binary});
            if let Some(from) = from {
                entry["from"] = json!(from);
            }
            entry
        })
        .collect();
    value["since"] = json!({"total": changes.len(), "files": files});
    Ok(value)
}

fn numstat(output: &[u8]) -> HashMap<String, (Option<u64>, Option<u64>, bool)> {
    let mut counts = HashMap::new();
    let mut fields = output.split(|byte| *byte == 0);
    while let Some(field) = fields.next() {
        if field.is_empty() {
            continue;
        }
        let line = String::from_utf8_lossy(field).into_owned();
        let mut parts = line.splitn(3, '\t');
        let (Some(added), Some(deleted), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let path = if path.is_empty() {
            let _from = fields.next();
            fields
                .next()
                .map(|path| String::from_utf8_lossy(path).into_owned())
                .unwrap_or_default()
        } else {
            path.to_owned()
        };
        counts.insert(
            path,
            (added.parse().ok(), deleted.parse().ok(), added == "-"),
        );
    }
    counts
}

// Skipped automatic checkpoints.

fn events_path(store: &Store) -> PathBuf {
    store.repo.join("masil-events.jsonl")
}

fn record_skip(store: &Store, why: &str, error: &str) {
    let Ok(_held) = store::flock(&store.repo.join("masil-locks/events"), true, LOCK_WAIT) else {
        return;
    };
    let path = events_path(store);
    if fs::metadata(&path).is_ok_and(|metadata| metadata.len() > EVENTS_LIMIT)
        && let Ok(text) = fs::read_to_string(&path)
    {
        // Keep the newer half.
        let lines: Vec<&str> = text.lines().collect();
        let kept = lines[lines.len() / 2..].join("\n") + "\n";
        let _ = fs::write(&path, kept);
    }
    let line = json!({"time_ms": now_ms(), "worktree_key": store.worktree_key, "worktree": store.root,
                      "skipped": why, "error": error});
    if let Ok(mut file) = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)
    {
        let _ = writeln!(file, "{line}");
    }
}

fn events(store: &Store) -> Vec<Value> {
    fs::read_to_string(events_path(store))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

// Commands.

/// What `agent checkpoint TARGET ...` asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Command {
    List,
    Make { reason: String },
    Show { id: u64, path: Option<String> },
}

impl Command {
    /// `make [--reason TEXT] | list | show ID [--file PATH]`.
    pub(crate) fn parse(args: &[String]) -> Result<Self, String> {
        match args {
            [action, rest @ ..] if action == "make" => match rest {
                [] => Ok(Self::Make {
                    reason: "checkpoint".into(),
                }),
                [flag, reason] if flag == "--reason" => Ok(Self::Make {
                    reason: reason.clone(),
                }),
                _ => Err("usage: checkpoint TARGET make [--reason TEXT]".into()),
            },
            [action] if action == "list" => Ok(Self::List),
            [action, id, rest @ ..] if action == "show" => {
                let id = id
                    .parse::<u64>()
                    .map_err(|_| format!("invalid_argument: {id} is not a checkpoint number"))?;
                let path = match rest {
                    [] => None,
                    [flag, path] if flag == "--file" => Some(path.clone()),
                    _ => return Err("usage: checkpoint TARGET show ID [--file PATH]".into()),
                };
                Ok(Self::Show { id, path })
            }
            _ => Err(
                "usage: checkpoint TARGET make [--reason TEXT] | list | show ID [--file PATH] | restore ID PATH... [--confirm TOKEN]"
                    .into(),
            ),
        }
    }
}

/// Runs `command` for the working tree at `root`, for `agent`. Blocking.
pub(crate) fn command_in(
    root: &Path,
    agent: (&str, &str),
    command: Command,
) -> Result<Value, String> {
    let settings = Settings::load()?;
    let deadline = Instant::now() + MAKE_TIME;
    let store = Store::open(root, deadline)?;
    match command {
        Command::Make { reason } => {
            let made = {
                let (_repo, _worktree) = store.lock(false, LOCK_WAIT)?;
                restore::clean_interrupted(&store);
                make(
                    &store,
                    &Request {
                        reason: &reason,
                        auto: false,
                        agent: Some(agent),
                        force: &[],
                        prompt_ms: None,
                    },
                    settings.limits(),
                    deadline,
                )?
            };
            tidy(&store);
            let mut value = made.to_json();
            value["stage"] = json!("checkpoint_made");
            value["root"] = json!(root);
            Ok(value)
        }
        Command::List => list(&store),
        Command::Show { id, path } => show(&store, id, path.as_deref(), settings.limits()),
    }
}

/// Starts an automatic checkpoint before a prompt, if the user asked for
/// them, without waiting for it. Never fails the prompt.
pub(crate) fn before_prompt(cwd: &str, name: &str, run: &str) {
    let Ok(settings) = Settings::load() else {
        return;
    };
    if !settings.before_prompt {
        return;
    }
    let Ok(executable) = std::env::current_exe() else {
        return;
    };
    let mut command = std::process::Command::new(executable);
    command
        .args(["checkpoint-auto", cwd, name, run, &now_ms().to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Its own process group, so a terminal's signals to the prompt's
    // group do not reach it; posix_spawn keeps this off the prompt's time.
    command.process_group(0);
    if let Ok(mut child) = command.spawn() {
        // Reaped here, so a long-lived caller keeps no zombie.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

/// `masil-agent checkpoint-auto CWD NAME RUN PROMPT_MS`: the automatic
/// checkpoint itself. Whatever stops it is recorded as skipped.
pub(crate) fn auto(args: &[String]) -> Result<i32, String> {
    let [cwd, name, run, prompt_ms] = args else {
        return Err("usage: checkpoint-auto CWD NAME RUN PROMPT_MS".into());
    };
    let Ok(root) = crate::changes::root_of(Path::new(cwd)) else {
        // Not a git working tree: nothing to save.
        return Ok(0);
    };
    let settings = Settings::load()?;
    let deadline = Instant::now() + settings.auto_time();
    // A store that cannot be opened has nowhere to record why.
    let store = Store::open(&root, deadline)?;
    let made = match store.lock(false, LOCK_WAIT) {
        Err(error) => Err(("busy", error)),
        Ok((_repo, _worktree)) => {
            restore::clean_interrupted(&store);
            make(
                &store,
                &Request {
                    reason: "before prompt",
                    auto: true,
                    agent: Some((name, run)),
                    force: &[],
                    prompt_ms: prompt_ms.parse().ok(),
                },
                settings.limits(),
                deadline,
            )
            .map_err(|error| {
                let why = if timed_out(&error) {
                    "timeout"
                } else if error.starts_with("checkpoint_too_large") {
                    "too_large"
                } else {
                    "error"
                };
                (why, error)
            })
        }
    };
    match made {
        Ok(_) => tidy(&store),
        Err((why, error)) => record_skip(&store, why, &error),
    }
    Ok(0)
}

// Restore (P6c).

/// A restore after its first half: either the preview, or the files saved
/// as they are and the working tree's lock held, ready to write.
pub(crate) enum Prepared {
    Preview(Value),
    Ready(Box<restore::Ready>),
}

impl Prepared {
    pub(crate) fn root(&self) -> &Path {
        match self {
            Self::Preview(_) => Path::new("/"),
            Self::Ready(ready) => &ready.store.root,
        }
    }
}

/// Plans a restore of `paths` from checkpoint `id`. Without `token`, the
/// preview; with the preview's token, a checkpoint of the files as they are
/// now, under the working tree's lock, which stays held.
pub(crate) fn prepare_restore(
    root: &Path,
    id: u64,
    paths: &[String],
    token: Option<&str>,
    agent: (&str, &str),
    deadline: Instant,
) -> Result<Prepared, String> {
    let settings = Settings::load()?;
    let store = Store::open(root, deadline)?;
    let (repo_lock, worktree_lock) = store.lock(false, LOCK_WAIT)?;
    store.clear_stale_locks();
    restore::clean_interrupted(&store);
    let plan = restore::plan(&store, id, paths, deadline)?;
    let Some(token) = token else {
        return Ok(Prepared::Preview(restore::preview(
            &store, &plan, deadline,
        )?));
    };
    if token != plan.token {
        return Err(format!(
            "checkpoint_stale: the files changed since the preview, or the token is not its; preview again (now {})",
            plan.token
        ));
    }
    let ready = restore::prepare(
        store,
        plan,
        settings.limits(),
        agent,
        (repo_lock, worktree_lock),
        deadline,
    )?;
    Ok(Prepared::Ready(Box::new(ready)))
}

/// Writes a prepared restore. The caller holds the server's management
/// lock and has checked the agents again.
pub(crate) fn finish_restore(prepared: Prepared, deadline: Instant) -> Result<Value, String> {
    match prepared {
        Prepared::Preview(preview) => Ok(preview),
        Prepared::Ready(ready) => restore::write(*ready, deadline),
    }
}
