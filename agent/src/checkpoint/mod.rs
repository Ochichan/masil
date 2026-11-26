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

/// Default soft cap for the sidecar checkpoint repository (1 GiB, D-12).
const DEFAULT_MAX_STORE_MB: u64 = 1024;

/// `~/.config/masil/checkpoints.toml`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Settings {
    #[serde(default)]
    pub(crate) before_prompt: Option<bool>,
    #[serde(default)]
    pub(crate) auto_after_turn: Option<bool>,
    max_file_mb: Option<u64>,
    max_total_mb: Option<u64>,
    max_store_mb: Option<u64>,
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

    /// Whether automatic pre-prompt checkpoints are active (D-01: default true
    /// when agent management is used; explicitly set `before_prompt = false`
    /// in `~/.config/masil/checkpoints.toml` to disable).
    pub(crate) fn before_prompt_enabled(&self) -> bool {
        self.before_prompt.unwrap_or(true)
    }

    /// Whether automatic post-turn checkpoints are active (D-01: default true).
    pub(crate) fn auto_after_turn_enabled(&self) -> bool {
        self.auto_after_turn.unwrap_or(true)
    }

    /// True only when the user explicitly disabled both automatic turn checkpoints.
    pub(crate) fn auto_turn_checkpoint_disabled(&self) -> bool {
        self.before_prompt == Some(false) && self.auto_after_turn == Some(false)
    }

    fn limits(&self) -> Limits {
        Limits {
            file: self.max_file_mb.unwrap_or(10).saturating_mul(MIB),
            total: self.max_total_mb.unwrap_or(512).saturating_mul(MIB),
        }
    }

    pub(crate) fn max_store_bytes(&self) -> u64 {
        self.max_store_mb
            .unwrap_or(DEFAULT_MAX_STORE_MB)
            .max(16)
            .saturating_mul(MIB)
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

pub(crate) fn default_limits() -> Limits {
    Settings::load().unwrap_or_default().limits()
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
    pub(crate) trigger: &'a str,
    pub(crate) agent: Option<(&'a str, &'a str)>,
    pub(crate) provider: Option<&'a str>,
    pub(crate) turn_seq: Option<u64>,
    pub(crate) prompt_sha256: Option<&'a str>,
    pub(crate) pinned: bool,
    /// When true, reuses the newest checkpoint without creating a new commit
    /// if the resulting working tree OID matches the newest checkpoint's tree.
    pub(crate) skip_if_unchanged: bool,
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
    pub(crate) reused: bool,
}

impl Made {
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "commit": self.commit,
            "tree_oid": self.snapshot.tree,
            "reused": self.reused,
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

/// A checkpoint's metadata is its commit body, read as JSON; a longer one
/// than this reads as none, as in [`metadata_of`].
const METADATA_LIMIT: usize = 64 * 1024;

/// Like [`numbers`], with each checkpoint's metadata from the same git call.
fn numbers_with_metadata(
    store: &Store,
    deadline: Instant,
) -> Result<Vec<(u64, String, Value)>, String> {
    let prefix = store.ref_prefix();
    let output = run(
        store,
        false,
        &[
            "for-each-ref",
            "--format=%(refname)%00%(objectname)%00%(contents:body)%01",
            &prefix,
        ],
        None,
        deadline,
        LIST_LIMIT,
    )?;
    let mut found: Vec<(u64, String, Value)> = output
        .stdout
        .split(|byte| *byte == 1)
        .filter_map(|record| {
            let mut fields = record.splitn(3, |byte| *byte == 0);
            let name = String::from_utf8_lossy(fields.next()?);
            let oid = String::from_utf8_lossy(fields.next()?).into_owned();
            let body = fields.next()?.trim_ascii();
            let id = name.trim_start().strip_prefix(&prefix)?.parse().ok()?;
            let metadata = if body.len() > METADATA_LIMIT {
                Value::Null
            } else {
                serde_json::from_slice(body).unwrap_or(Value::Null)
            };
            Some((id, oid, metadata))
        })
        .collect();
    found.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    Ok(found)
}

/// The newest checkpoint's `(id, commit_oid, working_tree_oid)` for fast dedup.
fn latest_tree(store: &Store, deadline: Instant) -> Option<(u64, String, String)> {
    let existing = numbers(store, deadline).ok()?;
    let (id, commit) = existing.into_iter().next()?;
    let tree = run(
        store,
        false,
        &["rev-parse", "--verify", "-q", &format!("{commit}:w")],
        None,
        deadline,
        4096,
    )
    .ok()
    .map(text)
    .filter(|oid| !oid.is_empty())?;
    Some((id, commit, tree))
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
        if request.skip_if_unchanged
            && let Some((latest_id, latest_commit, latest_tree_oid)) = latest_tree(store, deadline)
            && latest_tree_oid == snapshot.tree
        {
            return Ok(Made {
                id: latest_id,
                commit: latest_commit,
                snapshot,
                started_ms,
                finished_ms: now_ms(),
                reused: true,
            });
        }
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
            "version": 2,
            "worktree": store.root,
            "reason": request.reason,
            "auto": request.auto,
            "trigger": request.trigger,
            "agent": request.agent.map(|(name, run)| json!({"name": name, "run": run})),
            "run_id": request.agent.map(|(_, run)| run),
            "provider": request.provider,
            "turn_seq": request.turn_seq,
            "prompt_sha256": request.prompt_sha256,
            "tree_oid": snapshot.tree,
            "pinned": request.pinned,
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
            reused: false,
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

/// Recursively sums file sizes under `dir` (best-effort, no symlink following).
fn dir_bytes(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let mut total = 0u64;
    for entry in entries.flatten() {
        let Ok(meta) = fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if meta.file_type().is_dir() {
            total = total.saturating_add(dir_bytes(&entry.path()));
        } else if meta.file_type().is_file() {
            total = total.saturating_add(meta.len());
        }
    }
    total
}

/// Drops unpinned checkpoints past the newest [`KEEP`] of this working tree,
/// enforces the sidecar repository soft cap (`max_store_bytes`, D-12), and lets
/// git tidy up only when no checkpoint is being made in the repository.
pub(crate) fn tidy(store: &Store) {
    let max_store_bytes = Settings::load()
        .map(|s| s.max_store_bytes())
        .unwrap_or(DEFAULT_MAX_STORE_MB * MIB);
    tidy_with_cap(store, max_store_bytes);
}

pub(crate) fn tidy_with_cap(store: &Store, max_store_bytes: u64) {
    let Ok((_repo, _)) = store.lock(true, Duration::from_secs(1)) else {
        return;
    };
    let deadline = Instant::now() + MAKE_TIME;
    // With the exclusive lock no git of ours runs here: a ref lock file is
    // left from one that was killed, and would stop every deletion.
    let _ = fs::remove_file(store.repo.join("packed-refs.lock"));
    store.clear_ref_locks();
    let entries = match numbers_with_metadata(store, deadline) {
        Ok(listed) => listed,
        Err(_) => numbers(store, deadline)
            .unwrap_or_default()
            .into_iter()
            .map(|(id, oid)| {
                let meta = metadata_of(store, &oid, deadline);
                (id, oid, meta)
            })
            .collect(),
    };
    let mut unpinned: Vec<u64> = Vec::new();
    for (number, _, meta) in &entries {
        let pinned = meta.get("pinned").and_then(Value::as_bool).unwrap_or(false);
        if !pinned {
            unpinned.push(*number);
        }
    }
    let mut deleted = false;
    for number in unpinned.iter().skip(KEEP) {
        if run(
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
        )
        .is_ok()
        {
            deleted = true;
        }
    }
    // Enforce the repository soft cap by trimming oldest unpinned refs while
    // always keeping at least the newest unpinned checkpoint.
    let repo_bytes = dir_bytes(&store.repo.join("objects"));
    if repo_bytes > max_store_bytes {
        let kept_unpinned: Vec<u64> = unpinned.into_iter().take(KEEP).collect();
        // Drop the oldest half of unpinned refs (keeping index 0, the newest).
        let drop_count = (kept_unpinned.len().saturating_sub(1)).div_ceil(2);
        for number in kept_unpinned.iter().rev().take(drop_count) {
            if run(
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
            )
            .is_ok()
            {
                deleted = true;
            }
        }
    }
    let gc_args: &[&str] = if deleted && repo_bytes > max_store_bytes {
        &["-c", "gc.pruneExpire=now", "gc", "--auto", "--quiet"]
    } else {
        &[
            "-c",
            "gc.pruneExpire=2.weeks.ago",
            "gc",
            "--auto",
            "--quiet",
        ]
    };
    let _ = run(store, false, gc_args, None, deadline, 64 * 1024);
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
    let listed = match numbers_with_metadata(store, deadline) {
        Ok(listed) => listed,
        // One checkpoint whose commit object is missing fails the combined
        // call. Each ref is then read on its own, so the others are listed
        // and that one has no metadata.
        Err(_) => numbers(store, deadline)?
            .into_iter()
            .map(|(id, commit)| {
                let metadata = metadata_of(store, &commit, deadline);
                (id, commit, metadata)
            })
            .collect(),
    };
    for (id, commit, mut entry) in listed {
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
                        trigger: "manual",
                        agent: Some(agent),
                        provider: None,
                        turn_seq: None,
                        prompt_sha256: None,
                        pinned: false,
                        skip_if_unchanged: false,
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

/// Starts an automatic checkpoint before a prompt, unless disabled in
/// `~/.config/masil/checkpoints.toml`, without waiting for it. Never fails the prompt.
pub(crate) fn before_prompt(cwd: &str, name: &str, run: &str, provider: &str, prompt_sha256: &str) {
    let Ok(settings) = Settings::load() else {
        return;
    };
    if !settings.before_prompt_enabled() {
        return;
    }
    let Ok(executable) = crate::worktree::git::current_executable() else {
        return;
    };
    let mut command = std::process::Command::new(executable);
    command
        .args([
            "checkpoint-auto",
            cwd,
            name,
            run,
            &now_ms().to_string(),
            provider,
            prompt_sha256,
        ])
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

/// Takes a post-turn checkpoint synchronously (intended to be called from
/// `spawn_blocking` by the coordinator). Skips creating a duplicate commit when
/// the working tree matches the previous checkpoint's tree OID.
pub(crate) fn after_turn(
    cwd: &str,
    name: &str,
    run: &str,
    provider: &str,
    turn_seq: u64,
    prompt_sha256: Option<&str>,
) -> Option<Made> {
    let settings = Settings::load().ok()?;
    if !settings.auto_after_turn_enabled() {
        return None;
    }
    let root = crate::changes::root_of(Path::new(cwd)).ok()?;
    let deadline = Instant::now() + settings.auto_time();
    let store = Store::open(&root, deadline).ok()?;
    let made = match store.lock(false, LOCK_WAIT) {
        Err(error) => {
            record_skip(&store, "busy", &error);
            return None;
        }
        Ok((_repo, _worktree)) => {
            restore::clean_interrupted(&store);
            make(
                &store,
                &Request {
                    reason: "after turn",
                    auto: true,
                    trigger: "after_turn",
                    agent: Some((name, run)),
                    provider: Some(provider),
                    turn_seq: Some(turn_seq),
                    prompt_sha256,
                    pinned: false,
                    skip_if_unchanged: true,
                    force: &[],
                    prompt_ms: None,
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
        Ok(made) => {
            if !made.reused {
                tidy(&store);
            }
            Some(made)
        }
        Err((why, error)) => {
            record_skip(&store, why, &error);
            None
        }
    }
}

/// Takes a synchronous safety checkpoint (e.g. before DVR restore or Arena squash
/// promotion on a dirty working tree, D-12).
pub(crate) fn make_safety_checkpoint(cwd: &Path, reason: &str) -> Result<Made, String> {
    let settings = Settings::load().unwrap_or_default();
    let root = crate::changes::root_of(cwd)?;
    let deadline = Instant::now() + MAKE_TIME;
    let store = Store::open(&root, deadline)?;
    let (_repo, _worktree) = store.lock(false, LOCK_WAIT)?;
    restore::clean_interrupted(&store);
    make(
        &store,
        &Request {
            reason,
            auto: true,
            trigger: "pre_restore",
            agent: None,
            provider: None,
            turn_seq: None,
            prompt_sha256: None,
            pinned: false,
            skip_if_unchanged: false,
            force: &[],
            prompt_ms: None,
        },
        settings.limits(),
        deadline,
    )
}

/// Marks or unmarks checkpoint `id` as `pinned` in its sidecar commit metadata
/// so [`tidy_with_cap`] permanently protects it from both the 100-turn limit and
/// the 1 GiB soft cap (D-12).
pub(crate) fn set_pinned(store: &Store, id: u64, pinned: bool) -> Result<(), String> {
    let deadline = Instant::now() + MAKE_TIME;
    let (_repo, _worktree) = store.lock(false, LOCK_WAIT)?;
    store.clear_stale_locks();
    let commit = commit_of(store, id, deadline)?;
    let root_tree = text(run(
        store,
        false,
        &["rev-parse", "--verify", &format!("{commit}^{{tree}}")],
        None,
        deadline,
        4096,
    )?);
    let subject = text(run(
        store,
        false,
        &["log", "-1", "--format=%s", &commit],
        None,
        deadline,
        4096,
    )?);
    let mut meta = metadata_of(store, &commit, deadline);
    if !meta.is_object() {
        meta = json!({});
    }
    meta["pinned"] = json!(pinned);
    let new_commit = text(run(
        store,
        false,
        &["commit-tree", &root_tree],
        Some(format!("{subject}\n\n{meta}\n").into_bytes()),
        deadline,
        4096,
    )?);
    run(
        store,
        false,
        &[
            "update-ref",
            &format!("{}{id:010}", store.ref_prefix()),
            &new_commit,
            &commit,
        ],
        None,
        deadline,
        4096,
    )?;
    Ok(())
}

/// Computes a unified diff between two tree OIDs stored in the sidecar checkpoint repo.
pub(crate) fn diff_trees(
    store: &Store,
    prev_tree: Option<&str>,
    curr_tree: &str,
) -> Result<(String, usize), String> {
    let deadline = Instant::now() + MAKE_TIME;
    let empty_tree = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
    let base = prev_tree.filter(|t| !t.is_empty()).unwrap_or(empty_tree);
    let names = run(
        store,
        false,
        &["diff-tree", "-r", "--name-only", base, curr_tree],
        None,
        deadline,
        LIST_LIMIT,
    )?;
    let file_count = String::from_utf8_lossy(&names.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count();
    let patch = run(
        store,
        false,
        &[
            "diff-tree",
            "-r",
            "-p",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            base,
            curr_tree,
        ],
        None,
        deadline,
        DIFF_LIMIT,
    )?;
    Ok((
        String::from_utf8_lossy(&patch.stdout).into_owned(),
        file_count,
    ))
}

/// Full working-tree restore to a sidecar `tree_oid` (D-12 Time-Travel DVR):
/// takes a `pre_restore` safety checkpoint first, then restores all tracked and
/// untracked files (including undoing Bash side effects created or deleted after
/// that turn).
pub(crate) fn restore_tree(
    store: &Store,
    target_tree_oid: &str,
    limits: Limits,
) -> Result<Value, String> {
    let deadline = Instant::now() + MAKE_TIME;
    let (_repo, _worktree) = store.lock(false, LOCK_WAIT)?;
    store.clear_stale_locks();
    restore::clean_interrupted(store);

    // 1. Always take a safety snapshot before mutating the working tree.
    let before = make(
        store,
        &Request {
            reason: "pre DVR restore",
            auto: true,
            trigger: "pre_restore",
            agent: None,
            provider: None,
            turn_seq: None,
            prompt_sha256: None,
            pinned: false,
            skip_if_unchanged: false,
            force: &[],
            prompt_ms: None,
        },
        limits,
        deadline,
    )?;

    // 2. Determine files present now vs target tree so Bash-created files are removed.
    let current_files = list_tree_paths(store, &before.snapshot.tree, deadline)?;
    let target_files = list_tree_paths(store, target_tree_oid, deadline)?;
    let target_set: BTreeSet<&str> = target_files.iter().map(String::as_str).collect();

    let mut removed = Vec::new();
    for rel in &current_files {
        if !target_set.contains(rel.as_str()) {
            let full = store.root.join(rel);
            if full.exists() || fs::symlink_metadata(&full).is_ok() {
                let _ = fs::remove_file(&full);
                prune_empty_parents(&full, &store.root);
                removed.push(rel.clone());
            }
        }
    }

    // 3. Materialize target tree directly into the working tree using the sidecar index.
    run(
        store,
        true,
        &["read-tree", target_tree_oid],
        None,
        deadline,
        64 * 1024,
    )?;
    run(
        store,
        true,
        &["checkout-index", "-a", "-f"],
        None,
        deadline,
        64 * 1024,
    )?;

    Ok(json!({
        "stage": "restored",
        "safety_checkpoint_id": before.id,
        "target_tree_oid": target_tree_oid,
        "restored_files": target_files.len(),
        "removed_files": removed,
    }))
}

/// Exports a sidecar `tree_oid` cleanly into `target_dir` (for `dvr fork`).
pub(crate) fn materialize_tree_into(
    store: &Store,
    tree_oid: &str,
    target_dir: &Path,
) -> Result<usize, String> {
    let deadline = Instant::now() + MAKE_TIME;
    let (_repo, _worktree) = store.lock(false, LOCK_WAIT)?;
    let target_files = list_tree_paths(store, tree_oid, deadline)?;

    // Clear existing tracked/untracked files inside `target_dir` (leaving `.git` intact).
    if let Ok(entries) = fs::read_dir(target_dir) {
        for entry in entries.flatten() {
            if entry.file_name() == ".git" {
                continue;
            }
            let p = entry.path();
            if p.is_dir() {
                let _ = fs::remove_dir_all(&p);
            } else {
                let _ = fs::remove_file(&p);
            }
        }
    }

    let tmp_index = store.repo.join(format!("masil-index/fork-{}", now_ms()));
    let mut read_cmd = store.git(false);
    read_cmd
        .env("GIT_WORK_TREE", target_dir)
        .env("GIT_INDEX_FILE", &tmp_index)
        .args(["read-tree", tree_oid]);
    git::run_within(read_cmd, None, &[0], deadline, 64 * 1024, TIMEOUT)?;

    let mut checkout_cmd = store.git(false);
    checkout_cmd
        .env("GIT_WORK_TREE", target_dir)
        .env("GIT_INDEX_FILE", &tmp_index)
        .args(["checkout-index", "-a", "-f"]);
    let res = git::run_within(checkout_cmd, None, &[0], deadline, 64 * 1024, TIMEOUT);
    let _ = fs::remove_file(&tmp_index);
    res?;
    Ok(target_files.len())
}

fn list_tree_paths(
    store: &Store,
    tree_oid: &str,
    deadline: Instant,
) -> Result<Vec<String>, String> {
    let out = run(
        store,
        false,
        &["ls-tree", "-r", "-z", "--name-only", tree_oid],
        None,
        deadline,
        LIST_LIMIT,
    )?;
    Ok(out
        .stdout
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect())
}

fn prune_empty_parents(file: &Path, stop_at: &Path) {
    let mut cur = file.parent();
    while let Some(dir) = cur {
        if dir == stop_at || !dir.starts_with(stop_at) {
            break;
        }
        if fs::remove_dir(dir).is_err() {
            break;
        }
        cur = dir.parent();
    }
}

/// `masil-agent checkpoint-auto CWD NAME RUN PROMPT_MS [PROVIDER [PROMPT_SHA256]]`:
/// the automatic pre-prompt checkpoint itself. Whatever stops it is recorded as skipped.
pub(crate) fn auto(args: &[String]) -> Result<i32, String> {
    let (cwd, name, run, prompt_ms, provider, prompt_sha256) = match args {
        [cwd, name, run, prompt_ms] => (cwd, name, run, prompt_ms, None, None),
        [cwd, name, run, prompt_ms, provider] => (
            cwd,
            name,
            run,
            prompt_ms,
            (!provider.is_empty()).then_some(provider.as_str()),
            None,
        ),
        [cwd, name, run, prompt_ms, provider, sha] => (
            cwd,
            name,
            run,
            prompt_ms,
            (!provider.is_empty()).then_some(provider.as_str()),
            (!sha.is_empty()).then_some(sha.as_str()),
        ),
        _ => {
            return Err(
                "usage: checkpoint-auto CWD NAME RUN PROMPT_MS [PROVIDER [PROMPT_SHA256]]".into(),
            );
        }
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
                    trigger: "before_prompt",
                    agent: Some((name, run)),
                    provider,
                    turn_seq: None,
                    prompt_sha256,
                    pinned: false,
                    skip_if_unchanged: provider.is_some() || prompt_sha256.is_some(),
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
        Ok(made) => {
            if !made.reused {
                tidy(&store);
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn settings_default_to_enabled_and_support_explicit_opt_out() {
        let defaults = Settings::default();
        assert!(defaults.before_prompt_enabled());
        assert!(defaults.auto_after_turn_enabled());
        assert!(!defaults.auto_turn_checkpoint_disabled());
        assert_eq!(defaults.max_store_bytes(), 1024 * MIB);

        let parsed: Settings = toml::from_str(
            "before_prompt = false\nauto_after_turn = false\nmax_store_mb = 256\nmax_file_mb = 5\nauto_timeout_s = 15\n",
        )
        .unwrap();
        assert!(!parsed.before_prompt_enabled());
        assert!(!parsed.auto_after_turn_enabled());
        assert!(parsed.auto_turn_checkpoint_disabled());
        assert_eq!(parsed.max_store_bytes(), 256 * MIB);
        assert_eq!(parsed.limits().file, 5 * MIB);
        assert_eq!(parsed.auto_time(), Duration::from_secs(15));
    }

    #[test]
    fn unchanged_working_tree_reuses_previous_checkpoint_and_pinned_survives_tidy() {
        let base = std::env::temp_dir().join(format!(
            "masil-ckpt-p0-{}",
            crate::managed::nonce().unwrap()
        ));
        fs::create_dir_all(&base).unwrap();
        let base = base.canonicalize().unwrap();
        let repo = base.join("repo");
        fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        fs::write(repo.join("a.txt"), "hello\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "init"]);

        let bare = base.join("store.git");
        let store = Store::open_in_repo(&repo, &bare, Instant::now() + MAKE_TIME).unwrap();
        let limits = Settings::default().limits();
        let deadline = Instant::now() + MAKE_TIME;

        let first = {
            let (_r, _w) = store.lock(false, LOCK_WAIT).unwrap();
            make(
                &store,
                &Request {
                    reason: "after turn",
                    auto: true,
                    trigger: "after_turn",
                    agent: Some(("cx", "run-1")),
                    provider: Some("codex"),
                    turn_seq: Some(1),
                    prompt_sha256: Some("abc123"),
                    pinned: true,
                    skip_if_unchanged: true,
                    force: &[],
                    prompt_ms: None,
                },
                limits,
                deadline,
            )
            .unwrap()
        };
        assert_eq!(first.id, 1);
        assert!(!first.reused);

        // Repeating with identical working tree reuses checkpoint 1 without creating a new commit.
        let skipped = {
            let (_r, _w) = store.lock(false, LOCK_WAIT).unwrap();
            make(
                &store,
                &Request {
                    reason: "after turn",
                    auto: true,
                    trigger: "after_turn",
                    agent: Some(("cx", "run-1")),
                    provider: Some("codex"),
                    turn_seq: Some(2),
                    prompt_sha256: Some("def456"),
                    pinned: false,
                    skip_if_unchanged: true,
                    force: &[],
                    prompt_ms: None,
                },
                limits,
                deadline,
            )
            .unwrap()
        };
        assert_eq!(skipped.id, first.id);
        assert_eq!(skipped.commit, first.commit);
        assert_eq!(skipped.snapshot.tree, first.snapshot.tree);
        assert!(skipped.reused);

        // Modifying a file creates checkpoint 2 with full v2 metadata.
        fs::write(repo.join("a.txt"), "changed\n").unwrap();
        let second = {
            let (_r, _w) = store.lock(false, LOCK_WAIT).unwrap();
            make(
                &store,
                &Request {
                    reason: "after turn",
                    auto: true,
                    trigger: "after_turn",
                    agent: Some(("cx", "run-1")),
                    provider: Some("codex"),
                    turn_seq: Some(2),
                    prompt_sha256: Some("def456"),
                    pinned: false,
                    skip_if_unchanged: true,
                    force: &[],
                    prompt_ms: None,
                },
                limits,
                deadline,
            )
            .unwrap()
        };
        assert_eq!(second.id, 2);
        assert!(!second.reused);
        assert_ne!(second.snapshot.tree, first.snapshot.tree);

        let listed = list(&store).unwrap();
        let cps = listed["checkpoints"].as_array().unwrap();
        assert_eq!(cps.len(), 2);
        assert_eq!(cps[0]["trigger"], json!("after_turn"));
        assert_eq!(cps[0]["provider"], json!("codex"));
        assert_eq!(cps[0]["turn_seq"], json!(2));
        assert_eq!(cps[0]["prompt_sha256"], json!("def456"));
        assert_eq!(cps[0]["tree_oid"], json!(second.snapshot.tree));
        assert_eq!(cps[0]["pinned"], json!(false));
        assert_eq!(cps[1]["pinned"], json!(true));

        // Even under a tiny store cap, pinned checkpoint #1 is never pruned.
        tidy_with_cap(&store, 0);
        let after_tidy = list(&store).unwrap();
        let ids: Vec<u64> = after_tidy["checkpoints"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|c| c["id"].as_u64())
            .collect();
        assert!(
            ids.contains(&1),
            "pinned checkpoint #1 must survive tidy: {ids:?}"
        );

        fs::remove_dir_all(base).unwrap();
    }
}
