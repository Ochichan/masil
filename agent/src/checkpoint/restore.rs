//! Restoring files from a checkpoint (P6c). Files only: never `git reset`
//! or `checkout`, never the agent's conversation. Every byte a restore
//! overwrites or deletes is first saved in a new checkpoint, and nothing is
//! written unless the files are exactly as the preview showed.

use super::store::{Held, Store};
use super::{Limits, Made, RESTORE_PREFIX, Request, make, now_ms};
use crate::worktree::git::{self, Output};
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashSet};
use std::ffi::{CString, OsStr};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Instant;

const TIMEOUT: &str = "checkpoint_timeout: the restore did not finish in time";
const DIFF_LIMIT: usize = 256 * 1024;
pub(crate) const NOTE: &str = "Files only: the agent's conversation and its provider state are not rolled back. 파일만 되돌린다. agent의 대화와 provider 상태는 되돌리지 않는다.";

/// A path's state: what restore compares before it writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum State {
    Absent,
    File {
        oid: String,
        executable: bool,
    },
    Link {
        oid: String,
    },
    /// A directory or a special file; restore never replaces one.
    Other(&'static str),
}

impl State {
    fn describe(&self) -> Value {
        match self {
            Self::Absent => json!({"kind": "absent"}),
            Self::File { oid, executable } => {
                json!({"kind": "file", "oid": oid, "executable": executable})
            }
            Self::Link { oid } => json!({"kind": "symlink", "oid": oid}),
            Self::Other(kind) => json!({"kind": kind}),
        }
    }

    fn token_part(&self) -> String {
        match self {
            Self::Absent => "absent".into(),
            Self::File { oid, executable } => format!("file:{oid}:{executable}"),
            Self::Link { oid } => format!("link:{oid}"),
            Self::Other(kind) => format!("other:{kind}"),
        }
    }
}

/// What restore will do to one path.
#[derive(Clone, Debug)]
pub(crate) struct Step {
    pub(crate) path: String,
    pub(crate) then: State,
    pub(crate) now: State,
}

impl Step {
    fn action(&self) -> &'static str {
        match (&self.then, &self.now) {
            (then, now) if then == now => "unchanged",
            (State::Absent, _) => "delete",
            _ => "restore",
        }
    }
}

pub(crate) struct Plan {
    pub(crate) id: u64,
    pub(crate) steps: Vec<Step>,
    pub(crate) token: String,
}

fn run(
    store: &Store,
    args: &[&str],
    input: Option<Vec<u8>>,
    deadline: Instant,
    limit: usize,
) -> Result<Output, String> {
    let mut command = store.git(false);
    command.args(args);
    git::run_within(command, input, &[0], deadline, limit, TIMEOUT)
}

/// The blob id of what `file` holds, hashed by git from the descriptor
/// itself (git never resolves the path), and stored when `write`.
fn hash_file(store: &Store, file: File, write: bool, deadline: Instant) -> Result<String, String> {
    let mut command = store.git(false);
    command.args(["hash-object", "--no-filters", "--stdin"]);
    if write {
        command.arg("-w");
    }
    let output = git::run_with_stdin(command, Stdio::from(file), &[0], deadline, 4096, TIMEOUT)?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn hash_bytes(
    store: &Store,
    bytes: Vec<u8>,
    write: bool,
    deadline: Instant,
) -> Result<String, String> {
    let mut args = vec!["hash-object", "--no-filters", "--stdin"];
    if write {
        args.push("-w");
    }
    Ok(
        String::from_utf8_lossy(&run(store, &args, Some(bytes), deadline, 4096)?.stdout)
            .trim()
            .to_owned(),
    )
}

fn c_name(part: &OsStr) -> Result<CString, String> {
    CString::new(part.as_bytes()).map_err(|_| "a path holds a NUL byte".to_owned())
}

fn last_error(what: &str, path: &str) -> String {
    format!("{what} {path}: {}", std::io::Error::last_os_error())
}

const DIRECTORY: libc::c_int =
    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;

/// The directory that holds `path`, opened from the working tree's top
/// without following a symlink. With `make`, missing directories are made;
/// without it, `None` says one is missing.
fn parent_of(root: &Path, path: &str, make: bool) -> Result<Option<(OwnedFd, CString)>, String> {
    let parts: Vec<&OsStr> = Path::new(path)
        .components()
        .map(|part| match part {
            Component::Normal(part) => Ok(part),
            _ => Err(format!(
                "invalid_argument: {path} is not a plain relative path"
            )),
        })
        .collect::<Result<_, _>>()?;
    let (last, parents) = parts
        .split_last()
        .ok_or("invalid_argument: an empty path")?;
    let top = CString::new(root.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
    // SAFETY: a valid C string; the result is checked.
    let fd = unsafe { libc::open(top.as_ptr(), DIRECTORY) };
    if fd < 0 {
        return Err(last_error("cannot open", &root.display().to_string()));
    }
    // SAFETY: fd was just opened and is owned here.
    let mut directory = unsafe { OwnedFd::from_raw_fd(fd) };
    for part in parents {
        let name = c_name(part)?;
        // SAFETY: valid descriptor and C string; results are checked.
        let mut fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), DIRECTORY) };
        if fd < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT) {
            if !make {
                return Ok(None);
            }
            // SAFETY: as above; the mode is masked by the umask.
            if unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o777) } != 0 {
                return Err(last_error("cannot make a directory for", path));
            }
            // SAFETY: as above.
            fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), DIRECTORY) };
        }
        if fd < 0 {
            return Err(format!(
                "invalid_argument: {path} goes through something that is not a plain directory (a symlink?)"
            ));
        }
        // SAFETY: fd was just opened and is owned here.
        directory = unsafe { OwnedFd::from_raw_fd(fd) };
    }
    Ok(Some((directory, c_name(last)?)))
}

/// A path's state now, read through descriptors: no symlink is followed,
/// no FIFO blocks.
fn now_state(
    store: &Store,
    path: &str,
    write: bool,
    deadline: Instant,
) -> Result<(State, Option<u32>), String> {
    match parent_of(&store.root, path, false)? {
        None => Ok((State::Absent, None)),
        Some((directory, name)) => state_at(store, &directory, &name, path, write, deadline),
    }
}

/// When and what a directory entry was, to tell whether anyone changed it
/// since: device, inode, size and both times.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stamp {
    dev: u64,
    ino: u64,
    size: i64,
    modified: (i64, i64),
    changed: (i64, i64),
}

/// The entry `name` in `directory`, not following a symlink; `None` when
/// absent.
fn stamp_at(directory: &OwnedFd, name: &CString, path: &str) -> Result<Option<Stamp>, String> {
    // SAFETY: a zeroed stat is valid for fstatat to fill.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: valid descriptor, C string and buffer.
    if unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            name.as_ptr(),
            &mut stat,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        if std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT) {
            return Ok(None);
        }
        return Err(last_error("cannot read", path));
    }
    Ok(Some(Stamp {
        dev: stat.st_dev as u64,
        ino: stat.st_ino as u64,
        size: stat.st_size as i64,
        modified: (stat.st_mtime as i64, stat.st_mtime_nsec as i64),
        changed: (stat.st_ctime as i64, stat.st_ctime_nsec as i64),
    }))
}

/// The state of `name` in `directory` (an already opened, walked
/// directory), and a regular file's permission bits.
fn state_at(
    store: &Store,
    directory: &OwnedFd,
    name: &CString,
    path: &str,
    write: bool,
    deadline: Instant,
) -> Result<(State, Option<u32>), String> {
    // SAFETY: a zeroed stat is valid for fstatat to fill.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: valid descriptor, C string and buffer.
    if unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            name.as_ptr(),
            &mut stat,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        if std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT) {
            return Ok((State::Absent, None));
        }
        return Err(last_error("cannot read", path));
    }
    let mode = stat.st_mode as u32 & libc::S_IFMT as u32;
    if mode == libc::S_IFLNK as u32 {
        let mut target = vec![0u8; 4096];
        // SAFETY: valid descriptor, C string and buffer of that length.
        let length = unsafe {
            libc::readlinkat(
                directory.as_raw_fd(),
                name.as_ptr(),
                target.as_mut_ptr().cast(),
                target.len(),
            )
        };
        if length < 0 {
            return Err(last_error("cannot read the symlink", path));
        }
        target.truncate(length as usize);
        let oid = hash_bytes(store, target, write, deadline)?;
        return Ok((State::Link { oid }, None));
    }
    if mode != libc::S_IFREG as u32 {
        let kind = if mode == libc::S_IFDIR as u32 {
            "directory"
        } else {
            "special"
        };
        return Ok((State::Other(kind), None));
    }
    // SAFETY: valid descriptor and C string; the result is checked.
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(last_error("cannot read", path));
    }
    // SAFETY: fd was just opened and is owned by the File.
    let file = unsafe { File::from_raw_fd(fd) };
    let metadata = file
        .metadata()
        .map_err(|error| format!("{path}: {error}"))?;
    if !metadata.is_file() {
        return Ok((State::Other("special"), None));
    }
    // Only the permission bits: setuid and setgid do not carry over.
    let permissions = metadata.mode() & 0o777;
    let oid = hash_file(store, file, write, deadline)?;
    Ok((
        State::File {
            oid,
            executable: permissions & 0o100 != 0,
        },
        Some(permissions),
    ))
}

/// A path's state in a tree of the checkpoint repository.
fn tree_state(store: &Store, tree: &str, path: &str, deadline: Instant) -> Result<State, String> {
    let output = run(
        store,
        &["ls-tree", "-z", tree, "--", path],
        None,
        deadline,
        64 * 1024,
    )?;
    let Some(entry) = output
        .stdout
        .split(|byte| *byte == 0)
        .find(|entry| !entry.is_empty())
    else {
        return Ok(State::Absent);
    };
    let entry = String::from_utf8_lossy(entry).into_owned();
    let (meta, name) = entry.split_once('\t').unwrap_or_default();
    if name != path {
        return Ok(State::Absent);
    }
    let mut fields = meta.split(' ');
    let (Some(mode), Some(_kind), Some(oid)) = (fields.next(), fields.next(), fields.next()) else {
        return Ok(State::Absent);
    };
    Ok(match mode {
        "100644" => State::File {
            oid: oid.into(),
            executable: false,
        },
        "100755" => State::File {
            oid: oid.into(),
            executable: true,
        },
        "120000" => State::Link { oid: oid.into() },
        "160000" => State::Other("submodule"),
        _ => State::Other("tree"),
    })
}

/// Paths a checkpoint left out (its `x` blob), as git named them.
fn left_out(store: &Store, commit: &str, deadline: Instant) -> Result<HashSet<Vec<u8>>, String> {
    let output = run(
        store,
        &["cat-file", "blob", &format!("{commit}:x")],
        None,
        deadline,
        64 * 1024 * 1024,
    )?;
    let mut paths = HashSet::new();
    let mut fields = output.stdout.split(|byte| *byte == 0);
    while let (Some(_why), Some(path)) = (fields.next(), fields.next()) {
        if !path.is_empty() {
            paths.insert(path.to_vec());
        }
    }
    Ok(paths)
}

fn names(output: &[u8]) -> BTreeSet<Vec<u8>> {
    output
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
        .map(<[u8]>::to_vec)
        .collect()
}

/// Checks the paths and works out what restoring them from checkpoint `id`
/// means. The caller holds the store's locks.
pub(crate) fn plan(
    store: &Store,
    id: u64,
    paths: &[String],
    deadline: Instant,
) -> Result<Plan, String> {
    if paths.is_empty() {
        return Err("usage: restore needs at least one path".into());
    }
    // The same path twice is one file.
    let mut unique = Vec::new();
    for path in paths {
        if !unique.contains(path) {
            unique.push(path.clone());
        }
    }
    let paths = unique.as_slice();
    let commit = super::commit_of(store, id, deadline)?;
    let tree = format!("{commit}:w");
    // Arguments must name exactly a path the checkpoint or the working tree
    // has: a name typed in another Unicode form or letter case is refused,
    // not taken for a missing file.
    let saved = names(
        &run(
            store,
            &["ls-tree", "-r", "-z", "--name-only", &tree],
            None,
            deadline,
            256 * 1024 * 1024,
        )?
        .stdout,
    );
    let current: BTreeSet<Vec<u8>> = super::eligible(&store.root, deadline)?
        .into_iter()
        .collect();
    let excluded = left_out(store, &commit, deadline)?;
    let mut seen = HashSet::new();
    let mut steps = Vec::new();
    for path in paths {
        if !saved.contains(path.as_bytes()) && !current.contains(path.as_bytes()) {
            // Walked without following symlinks: a path through one is
            // refused here.
            let exists = !matches!(now_state(store, path, false, deadline)?.0, State::Absent);
            if exists && is_ignored(store, path, deadline)? {
                return Err(format!(
                    "checkpoint_incomplete: {path} is ignored by git, so no checkpoint holds it; it is not touched"
                ));
            }
            return Err(format!(
                "invalid_argument: {path} is neither in checkpoint {id} nor among the working tree's files; give it exactly as listed"
            ));
        }
        if Path::new(path)
            .components()
            .any(|part| part.as_os_str().eq_ignore_ascii_case(".git"))
        {
            return Err(format!("invalid_argument: {path} is inside git's own data"));
        }
        // Left out itself, or inside a directory that was left out.
        let mut prefixes = vec![path.as_bytes().to_vec()];
        for (index, byte) in path.bytes().enumerate() {
            if byte == b'/' {
                prefixes.push(path.as_bytes()[..index].to_vec());
                prefixes.push(path.as_bytes()[..=index].to_vec());
            }
        }
        if prefixes.iter().any(|prefix| excluded.contains(prefix)) {
            return Err(format!(
                "checkpoint_incomplete: checkpoint {id} left {path} out; it cannot be restored from it"
            ));
        }
        if inside_nested_repository(&store.root, path)? {
            return Err(format!(
                "invalid_argument: {path} is inside a repository of its own (a submodule or a clone); restore does not write there"
            ));
        }
        let then = tree_state(store, &tree, path, deadline)?;
        if let State::Other(kind) = then {
            return Err(format!(
                "invalid_argument: {path} is a {kind} in checkpoint {id}, not a file"
            ));
        }
        if let State::File { oid, .. } | State::Link { oid } = &then
            && run(store, &["cat-file", "-e", oid], None, deadline, 4096).is_err()
        {
            return Err(format!(
                "checkpoint_incomplete: the content of {path} is missing from checkpoint {id}"
            ));
        }
        let (now, _) = now_state(store, path, true, deadline)?;
        if let State::Other(kind) = now {
            return Err(format!(
                "invalid_argument: {path} is now a {kind}; restore replaces files only"
            ));
        }
        if let Some((directory, name)) = parent_of(&store.root, path, false)? {
            // SAFETY: a zeroed stat is valid for fstatat to fill.
            let mut stat: libc::stat = unsafe { std::mem::zeroed() };
            // SAFETY: valid descriptor, C string and buffer.
            if unsafe {
                libc::fstatat(
                    directory.as_raw_fd(),
                    name.as_ptr(),
                    &mut stat,
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } == 0
                && !seen.insert((stat.st_dev, stat.st_ino))
            {
                return Err(format!(
                    "invalid_argument: {path} names a file another argument names"
                ));
            }
        }
        if then == State::Absent && now != State::Absent {
            // The checkpoint may hold it under a name in other letter case,
            // which this file system takes for the same file.
            let folded = path.to_lowercase();
            if let Some(other) = saved.iter().find(|name| {
                String::from_utf8_lossy(name).to_lowercase() == folded
                    && name.as_slice() != path.as_bytes()
            }) {
                return Err(format!(
                    "checkpoint_incomplete: checkpoint {id} has {}, which differs from {path} only in letter case; restore that name",
                    String::from_utf8_lossy(other)
                ));
            }
            // Never saved because ignored: deleting it would lose it.
            if is_ignored(store, path, deadline)? {
                return Err(format!(
                    "checkpoint_incomplete: {path} is ignored by git, so no checkpoint holds it; it is not deleted"
                ));
            }
        }
        steps.push(Step {
            path: path.clone(),
            then,
            now,
        });
    }
    steps.sort_by(|a, b| a.path.cmp(&b.path));
    let token = token(store, id, &steps);
    Ok(Plan { id, steps, token })
}

/// Whether a directory on the way to `path` (below the top) holds a
/// `.git`: a submodule or a repository of its own.
fn inside_nested_repository(root: &Path, path: &str) -> Result<bool, String> {
    let parts: Vec<&OsStr> = Path::new(path)
        .components()
        .map(Component::as_os_str)
        .collect();
    let Some((_, parents)) = parts.split_last() else {
        return Ok(false);
    };
    let mut walked = String::new();
    for part in parents {
        if !walked.is_empty() {
            walked.push('/');
        }
        walked.push_str(&part.to_string_lossy());
        let probe = format!("{walked}/.git");
        if let Some((directory, name)) = parent_of(root, &probe, false)?
            && stamp_at(&directory, &name, &probe)?.is_some()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether the user's ignore rules exclude `path` (an untracked path; git
/// does not call tracked files ignored).
fn is_ignored(store: &Store, path: &str, deadline: Instant) -> Result<bool, String> {
    let mut command = git::command(&store.root);
    command.args([
        "-c",
        "core.fsmonitor=false",
        "check-ignore",
        "-q",
        "--",
        path,
    ]);
    Ok(git::run_within(command, None, &[0, 1], deadline, 4096, TIMEOUT)?.code == Some(0))
}

fn token(store: &Store, id: u64, steps: &[Step]) -> String {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    digest.update(format!("{}\0{id}\0", store.worktree_key));
    for step in steps {
        digest.update(format!(
            "{}\0{}\0{}\0",
            step.path,
            step.then.token_part(),
            step.now.token_part()
        ));
    }
    digest.finalize()[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The preview: each path's action and diff from now to the checkpoint.
pub(crate) fn preview(store: &Store, plan: &Plan, deadline: Instant) -> Result<Value, String> {
    let empty = hash_bytes(store, Vec::new(), true, deadline)?;
    let mut files = Vec::new();
    for step in &plan.steps {
        let side = |state: &State| match state {
            State::File { oid, .. } | State::Link { oid } => oid.clone(),
            _ => empty.clone(),
        };
        let diff = if step.action() == "unchanged" {
            Output {
                stdout: Vec::new(),
                truncated: false,
                code: Some(0),
            }
        } else {
            run(
                store,
                &[
                    "diff",
                    "--no-color",
                    "--no-ext-diff",
                    "--no-textconv",
                    &side(&step.now),
                    &side(&step.then),
                ],
                None,
                deadline,
                DIFF_LIMIT,
            )?
        };
        files.push(json!({
            "path": step.path,
            "action": step.action(),
            "checkpoint": step.then.describe(),
            "now": step.now.describe(),
            "diff": String::from_utf8_lossy(&diff.stdout),
            "truncated": diff.truncated,
        }));
    }
    Ok(json!({
        "stage": "preview",
        "id": plan.id,
        "root": store.root,
        "token": plan.token,
        "files": files,
        "note": NOTE,
    }))
}

// The restore journal: written before anything changes in the working tree.

fn journal_path(store: &Store) -> PathBuf {
    store
        .repo
        .join("masil-restore")
        .join(format!("{}.json", store.worktree_key))
}

fn write_journal(store: &Store, journal: &Value) -> Result<(), String> {
    let path = journal_path(store);
    let directory = path.parent().ok_or("journal has no directory")?;
    crate::managed::private_directory(directory)?;
    let temporary = directory.join(format!("{}.tmp", store.worktree_key));
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&temporary)
        .map_err(|error| format!("restore journal: {error}"))?;
    file.write_all(journal.to_string().as_bytes())
        .and_then(|_| file.sync_all())
        .map_err(|error| format!("restore journal: {error}"))?;
    fs::rename(&temporary, &path).map_err(|error| format!("restore journal: {error}"))
}

/// An interrupted restore of this working tree, if any.
pub(crate) fn interrupted(store: &Store) -> Option<Value> {
    fs::read_to_string(journal_path(store))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
}

/// Removes what an interrupted restore left: only the temporary files its
/// journal names. The caller holds this working tree's lock.
pub(crate) fn clean_interrupted(store: &Store) {
    let Some(journal) = interrupted(store) else {
        return;
    };
    for file in journal["files"].as_array().into_iter().flatten() {
        let (Some(path), Some(temporary)) = (file["path"].as_str(), file["temporary"].as_str())
        else {
            continue;
        };
        if let Ok(Some((directory, _))) = parent_of(&store.root, path, false)
            && let Ok(name) = CString::new(temporary)
        {
            // SAFETY: valid descriptor and C string; a missing file is fine.
            unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) };
        }
    }
    // The record stays, so `list` can still say what happened, until the
    // next restore replaces it.
    if journal.get("cleaned_ms").is_none() {
        let mut journal = journal;
        journal["cleaned_ms"] = json!(now_ms());
        let _ = write_journal(store, &journal);
    }
}

fn temporary_name() -> Result<String, String> {
    Ok(format!("{RESTORE_PREFIX}{}", crate::managed::nonce()?))
}

/// Moves `temporary` onto `name` in `directory`. When nothing should be
/// there, only if nothing is: a file made meanwhile is not replaced.
fn replace(directory: &OwnedFd, temporary: &CString, name: &CString, create: bool) -> bool {
    let fd = directory.as_raw_fd();
    if create {
        #[cfg(target_os = "macos")]
        // SAFETY: both names in the directory held open here.
        let moved = unsafe {
            libc::renameatx_np(fd, temporary.as_ptr(), fd, name.as_ptr(), libc::RENAME_EXCL)
        };
        #[cfg(target_os = "linux")]
        // SAFETY: as above.
        let moved = unsafe {
            libc::renameat2(
                fd,
                temporary.as_ptr(),
                fd,
                name.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        return moved == 0;
    }
    // SAFETY: both names in the directory held open here.
    unsafe { libc::renameat(fd, temporary.as_ptr(), fd, name.as_ptr()) == 0 }
}

/// Writes one step. Measured, written and replaced through one opened
/// directory; the entry must be as previewed when measured, and unchanged
/// (same inode, size and times) when it is replaced or deleted.
fn write_step(
    store: &Store,
    step: &Step,
    journal: &mut Value,
    index: usize,
    deadline: Instant,
) -> Result<&'static str, String> {
    if step.action() == "unchanged" {
        return Ok("unchanged");
    }
    let create_parents = !matches!(step.then, State::Absent);
    let Some((directory, name)) = parent_of(&store.root, &step.path, create_parents)? else {
        return Ok("stale");
    };
    let before = stamp_at(&directory, &name, &step.path)?;
    let (now, permissions) = state_at(store, &directory, &name, &step.path, false, deadline)?;
    if now != step.now || stamp_at(&directory, &name, &step.path)? != before {
        return Ok("stale");
    }
    let unchanged =
        || -> Result<bool, String> { Ok(stamp_at(&directory, &name, &step.path)? == before) };
    if step.then == State::Absent {
        if !unchanged()? {
            return Ok("stale");
        }
        // SAFETY: valid descriptor and C string; a regular file or symlink,
        // checked just above.
        if unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) } != 0 {
            return Err(last_error("cannot delete", &step.path));
        }
        // SAFETY: fsync on a descriptor this function owns.
        unsafe { libc::fsync(directory.as_raw_fd()) };
        return Ok("deleted");
    }
    let temporary = temporary_name()?;
    journal["files"][index]["temporary"] = json!(temporary);
    write_journal(store, journal)?;
    let temporary_c = CString::new(temporary.as_str()).map_err(|e| e.to_string())?;
    let remove_temporary = || {
        // SAFETY: the temporary name this function made.
        unsafe { libc::unlinkat(directory.as_raw_fd(), temporary_c.as_ptr(), 0) };
    };
    let made = match &step.then {
        State::Link { oid } => {
            let target = run(store, &["cat-file", "blob", oid], None, deadline, 64 * 1024)?;
            let target = CString::new(target.stdout)
                .map_err(|_| "a symlink target holds a NUL byte".to_owned())?;
            // SAFETY: valid C strings and descriptor.
            if unsafe {
                libc::symlinkat(target.as_ptr(), directory.as_raw_fd(), temporary_c.as_ptr())
            } == 0
            {
                Ok(())
            } else {
                Err(last_error("cannot make the symlink", &step.path))
            }
        }
        State::File { oid, executable } => {
            // The file's own permission bits stay; only the executable bits
            // follow the checkpoint (git keeps nothing else). A new file
            // gets the usual bits less the umask, applied by the kernel.
            let (create_mode, fixed) = match permissions {
                Some(base) => (
                    0o600,
                    Some(if *executable {
                        base | ((base & 0o444) >> 2)
                    } else {
                        base & !0o111
                    }),
                ),
                None => (if *executable { 0o777 } else { 0o666 }, None),
            };
            // SAFETY: valid descriptor and C string; the result is checked.
            let fd = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    temporary_c.as_ptr(),
                    libc::O_WRONLY
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC,
                    create_mode as libc::c_uint,
                )
            };
            if fd < 0 {
                return Err(last_error("cannot write next to", &step.path));
            }
            // SAFETY: fd was just opened and is owned by the File.
            let file = unsafe { File::from_raw_fd(fd) };
            (|| -> Result<(), String> {
                let mut command = store.git(false);
                command.args(["cat-file", "blob", oid]);
                git::run_into(
                    command,
                    file.try_clone().map_err(|e| e.to_string())?,
                    deadline,
                    TIMEOUT,
                )?;
                file.sync_all().map_err(|error| error.to_string())?;
                if let Some(mode) = fixed
                    // SAFETY: fchmod on a descriptor this function owns.
                    && unsafe { libc::fchmod(file.as_raw_fd(), mode as libc::mode_t) } != 0
                {
                    return Err(last_error("cannot set the permissions of", &step.path));
                }
                Ok(())
            })()
        }
        _ => Err(format!("{} has nothing to restore", step.path)),
    };
    if let Err(error) = made {
        remove_temporary();
        return Err(error);
    }
    // Changed while the content was written: theirs stays, nothing lost.
    if !unchanged()? {
        remove_temporary();
        return Ok("stale");
    }
    if !replace(&directory, &temporary_c, &name, before.is_none()) {
        let exists = std::io::Error::last_os_error().raw_os_error() == Some(libc::EEXIST);
        remove_temporary();
        if exists {
            return Ok("stale");
        }
        return Err(last_error("cannot replace", &step.path));
    }
    // SAFETY: fsync on a descriptor this function owns.
    unsafe { libc::fsync(directory.as_raw_fd()) };
    Ok("restored")
}

/// A confirmed restore after its first half: the files as they are now
/// are in checkpoint `before`, the journal is written, and the working
/// tree's lock is held until the writes are done.
pub(crate) struct Ready {
    pub(crate) store: Store,
    plan: Plan,
    before: Made,
    journal: Value,
    /// Set once the first file may change: from then on the journal stays
    /// until the restore finishes, so an interrupted one is visible.
    writing: bool,
    _locks: (Held, Option<Held>),
}

impl Drop for Ready {
    fn drop(&mut self) {
        // Stopped before anything was written: nothing to remember.
        if !self.writing {
            let _ = fs::remove_file(journal_path(&self.store));
        }
    }
}

/// The first half: the journal, then a checkpoint of every target as it is
/// now, whatever the limits and ignore rules.
pub(crate) fn prepare(
    store: Store,
    plan: Plan,
    limits: Limits,
    agent: (&str, &str),
    locks: (Held, Option<Held>),
    deadline: Instant,
) -> Result<Ready, String> {
    let journal = json!({
        "started_ms": now_ms(),
        "source": plan.id,
        "files": plan.steps.iter().map(|step| json!({"path": step.path, "action": step.action()})).collect::<Vec<_>>(),
    });
    write_journal(&store, &journal)?;
    let targets: Vec<String> = plan.steps.iter().map(|step| step.path.clone()).collect();
    let before = make(
        &store,
        &Request {
            reason: &format!("before restore of {}", plan.id),
            auto: false,
            agent: Some(agent),
            force: &targets,
            prompt_ms: None,
        },
        limits,
        deadline,
    );
    let before = match before {
        Ok(before) => before,
        Err(error) => {
            let _ = fs::remove_file(journal_path(&store));
            return Err(format!("{error}; nothing was restored"));
        }
    };
    let mut ready = Ready {
        store,
        plan,
        before,
        journal,
        writing: false,
        _locks: locks,
    };
    ready.journal["before"] = json!(ready.before.id);
    write_journal(&ready.store, &ready.journal)?;
    Ok(ready)
}

/// The second half: each file, only if it is still as previewed and as the
/// first half saved it. The caller holds the server's management lock.
pub(crate) fn write(mut ready: Ready, deadline: Instant) -> Result<Value, String> {
    let tree = format!("{}:w", ready.before.commit);
    let mut results = Vec::new();
    let mut partial = false;
    let steps = ready.plan.steps.clone();
    ready.writing = true;
    for (index, step) in steps.iter().enumerate() {
        // What will be overwritten or deleted must be in that checkpoint.
        // Every error stays this file's, so the files already done are
        // still reported.
        let result = match tree_state(&ready.store, &tree, &step.path, deadline) {
            Ok(saved) if saved != step.now => Ok("stale"),
            Ok(_) => write_step(&ready.store, step, &mut ready.journal, index, deadline),
            Err(error) => Err(error),
        };
        let (result, error) = match result {
            Ok(result) => (result, None),
            Err(error) => ("failed", Some(error)),
        };
        partial |= matches!(result, "stale" | "failed");
        ready.journal["files"][index]["result"] = json!(result);
        // The journal only helps after a crash; a failure to update it
        // does not stop the files.
        let _ = write_journal(&ready.store, &ready.journal);
        results.push(json!({"path": step.path, "result": result, "error": error}));
    }
    let _ = fs::remove_file(journal_path(&ready.store));
    Ok(json!({
        "stage": if partial { "partly_restored" } else { "restored" },
        "id": ready.plan.id,
        "before": ready.before.id,
        "root": ready.store.root,
        "files": results,
        "note": format!("{NOTE} Checkpoint {} holds the files as they were before.", ready.before.id),
    }))
}
