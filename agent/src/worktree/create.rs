//! The create job: reserve a name, make the directory, `git worktree add`
//! into it, mark it as masil's, and only then call it ready.

use super::git::{self, Ran};
use super::helper::CANCEL;
use super::registry::{End, Job, Registry, Reservation, Worktree, now_ms};
use serde::Deserialize;
use serde_json::{Value, json};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

/// Name of the marker file in a linked worktree's git directory.
pub(super) const MARKER: &str = "masil-worktree";
/// Suffixes tried for a name derived from the branch.
const SUFFIXES: u32 = 99;

#[derive(Deserialize, serde::Serialize, Clone, Debug, PartialEq)]
pub(super) struct Request {
    pub(super) repo_root: String,
    pub(super) common_dir: String,
    pub(super) repo_key: String,
    pub(super) branch: String,
    pub(super) from: Option<String>,
    pub(super) name: Option<String>,
    pub(super) data_root: String,
}

enum Stop {
    Failed(String),
    Cancelled(String),
}

impl From<String> for Stop {
    fn from(error: String) -> Self {
        Self::Failed(error)
    }
}

impl From<&str> for Stop {
    fn from(error: &str) -> Self {
        Self::Failed(error.into())
    }
}

/// How the branch comes to exist.
enum Plan {
    Existing,
    Tracking(String),
    New,
}

/// A worktree name: 1 to 64 of `[A-Za-z0-9._-]`, not starting with `.` or
/// `-`.
pub(super) fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && !name.starts_with(['.', '-'])
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

/// The name a branch gets: `/` becomes `-` and other characters outside the
/// name set are dropped; what is left must still be a valid name.
pub(super) fn derived_name(branch: &str) -> String {
    let name: String = branch
        .replace('/', "-")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || "._-".contains(*c))
        .take(64)
        .collect();
    if valid_name(&name) {
        return name;
    }
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(branch.as_bytes());
    let hex: String = digest[..4].iter().map(|b| format!("{b:02x}")).collect();
    format!("wt-{hex}")
}

pub(super) fn run(registry: &mut Registry, job: &Job, log: &File) -> End {
    let request: Request = match serde_json::from_value(job.request.clone()) {
        Ok(request) => request,
        Err(error) => return End::Failed(format!("the job request is not readable: {error}")),
    };
    match create(registry, job, &request, log) {
        Ok(result) => End::Succeeded(result),
        Err(Stop::Failed(error)) => End::Failed(error),
        Err(Stop::Cancelled(error)) => End::Cancelled(error),
    }
}

fn cancelled() -> bool {
    CANCEL.load(Ordering::SeqCst)
}

fn create(
    registry: &mut Registry,
    job: &Job,
    request: &Request,
    log: &File,
) -> Result<Value, Stop> {
    let root = Path::new(&request.repo_root);
    let branch = &request.branch;
    let local = git::probe(
        root,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}^{{commit}}"),
        ],
    )?;
    let (plan, from_oid) = match (local, &request.from) {
        (Some(_), Some(_)) => {
            return Err(Stop::Failed(format!(
                "branch {branch} exists; --from applies only to a new branch"
            )));
        }
        (Some(oid), None) => (Plan::Existing, oid.trim().to_owned()),
        (None, Some(from)) => (Plan::New, commit(root, from)?),
        (None, None) => match remote_candidates(root, branch)?.as_slice() {
            [] => (Plan::New, commit(root, "HEAD")?),
            [(reference, oid)] => (Plan::Tracking(reference.clone()), oid.clone()),
            several => {
                let names: Vec<&str> = several
                    .iter()
                    .map(|(reference, _)| reference.trim_start_matches("refs/remotes/"))
                    .collect();
                return Err(Stop::Failed(format!(
                    "branch {branch} is on several remotes ({}); give --from",
                    names.join(", ")
                )));
            }
        },
    };
    if cancelled() {
        return Err(Stop::Cancelled("cancelled before anything was made".into()));
    }

    let parent = crate::managed::private_directory(
        &crate::managed::private_directory(Path::new(&request.data_root))?.join(&request.repo_key),
    )?;
    let (worktree, path) = {
        let _lock = registry.lock()?;
        let (name, path) = choose_name(registry, request, &parent)?;
        let path_text = path
            .to_str()
            .ok_or_else(|| Stop::Failed("the worktree path is not UTF-8".into()))?
            .to_owned();
        let id = registry.reserve(
            job.id,
            &Reservation {
                repo_common_dir: &request.common_dir,
                repo_key: &request.repo_key,
                repo_root: &request.repo_root,
                name: &name,
                branch,
                from_oid: &from_oid,
                path: &path_text,
            },
            now_ms(),
        )?;
        (id, path)
    };
    // From here every failure goes through the cleanup.
    let made = Making {
        job: job.id,
        root,
        branch,
        from_oid: &from_oid,
        worktree,
        path: &path,
    };
    match make(registry, &made, &plan, log) {
        Ok(result) => Ok(result),
        Err(stop) => Err(clean_after(registry, worktree, stop)),
    }
}

struct Making<'a> {
    job: i64,
    root: &'a Path,
    branch: &'a str,
    from_oid: &'a str,
    worktree: i64,
    path: &'a Path,
}

fn make(
    registry: &mut Registry,
    made: &Making<'_>,
    plan: &Plan,
    log: &File,
) -> Result<Value, Stop> {
    let Making {
        job,
        root,
        branch,
        from_oid,
        worktree,
        path,
    } = *made;
    if cancelled() {
        return Err(Stop::Cancelled("cancelled before anything was made".into()));
    }
    fs::create_dir(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let metadata =
        fs::symlink_metadata(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let identity = (metadata.dev(), metadata.ino());
    registry.set_creating(worktree, identity)?;
    if !matches!(plan, Plan::Existing) {
        // Made here, and only if no such branch exists, so a cleanup never
        // deletes a branch someone else made in the meantime.
        git::query(
            root,
            &[
                "update-ref",
                "-m",
                "masil worktree create",
                &format!("refs/heads/{branch}"),
                from_oid,
                "",
            ],
        )
        .map_err(|error| format!("branch {branch} could not be made: {error}"))?;
        registry.set_branch_created(worktree)?;
        if let Plan::Tracking(reference) = plan {
            git::query(
                root,
                &["branch", &format!("--set-upstream-to={reference}"), branch],
            )?;
        }
    }
    let path_text = path.to_str().ok_or("the worktree path is not UTF-8")?;
    let args = ["worktree", "add", "--", path_text, branch];
    let _ = writeln!(&*log, "$ git {}", args.join(" "));
    let started = |pid: i32| {
        let child = crate::process::info(pid).map(|info| (pid, info.started));
        let _ = registry.set_child(job, child);
    };
    let ended = || {
        let _ = registry.set_child(job, None);
    };
    match git::run_logged(root, &args, log, &CANCEL, &started, &ended)? {
        Ran::Exited(status) if status.success() => {}
        Ran::Exited(status) => {
            return Err(Stop::Failed(format!(
                "git worktree add failed ({status}); see `worktree jobs {job}`"
            )));
        }
        Ran::Cancelled => {
            return Err(Stop::Cancelled(
                "cancelled while git made the worktree".into(),
            ));
        }
    }
    // The effect exists from here on; a cancel now is too late.
    let same = fs::symlink_metadata(path)
        .is_ok_and(|metadata| (metadata.dev(), metadata.ino()) == identity);
    if !same {
        return Err(Stop::Failed(format!(
            "{} was replaced while git made the worktree",
            path.display()
        )));
    }
    write_marker(registry, worktree, path)?;
    registry.set_state(worktree, &["creating"], "ready", now_ms())?;
    let row = registry
        .worktree(worktree)?
        .ok_or("the worktree row disappeared")?;
    let mut result = row.to_json();
    result["branch_source"] = json!(match plan {
        Plan::Existing => "existing",
        Plan::Tracking(_) => "remote",
        Plan::New => "new",
    });
    Ok(result)
}

/// Cleans up after this helper's own failure or cancel, and says so in
/// the job's error.
fn clean_after(registry: &mut Registry, worktree: i64, stop: Stop) -> Stop {
    let note = match remove_half_made(registry, worktree, Cleanup::Own) {
        Ok(Cleaned::Removed) => return stop,
        Ok(Cleaned::Kept(why)) => format!("; the worktree was kept: {why}"),
        Ok(Cleaned::Left(why)) => format!("; {why}"),
        Err(error) => format!(
            "; the half-made worktree could not be removed yet, a later worktree command retries: {error}"
        ),
    };
    match stop {
        Stop::Failed(text) => Stop::Failed(text + &note),
        Stop::Cancelled(text) => Stop::Cancelled(text + &note),
    }
}

fn commit(root: &Path, revision: &str) -> Result<String, String> {
    git::query(
        root,
        &[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{revision}^{{commit}}"),
        ],
    )
    .map(|oid| oid.trim().to_owned())
}

/// `refs/remotes/<remote>/<branch>` for each configured remote that has it.
fn remote_candidates(root: &Path, branch: &str) -> Result<Vec<(String, String)>, String> {
    let remotes = git::query(root, &["remote"])?;
    let references = git::query(
        root,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/remotes/",
        ],
    )?;
    let mut found = Vec::new();
    for remote in remotes.lines().filter(|line| !line.is_empty()) {
        let wanted = format!("refs/remotes/{remote}/{branch}");
        if let Some(oid) = references
            .lines()
            .find_map(|line| line.strip_prefix(&wanted)?.strip_prefix(' '))
        {
            found.push((wanted, oid.to_owned()));
        }
    }
    Ok(found)
}

/// The requested name, or the branch's with `-2`, `-3`... when taken. A
/// name is taken by a live registry row or by anything at its path.
fn choose_name(
    registry: &Registry,
    request: &Request,
    parent: &Path,
) -> Result<(String, PathBuf), Stop> {
    let free = |name: &str| -> Result<Option<PathBuf>, String> {
        let path = parent.join(name);
        let text = path.to_str().ok_or("the worktree path is not UTF-8")?;
        let on_disk = fs::symlink_metadata(&path).is_ok();
        Ok((!on_disk && !registry.taken(&request.repo_key, name, text)?).then_some(path))
    };
    if let Some(name) = &request.name {
        return match free(name)? {
            Some(path) => Ok((name.clone(), path)),
            None => Err(Stop::Failed(format!(
                "a worktree named {name} already exists for this repository"
            ))),
        };
    }
    let base = derived_name(&request.branch);
    for suffix in 1..=SUFFIXES {
        let name = if suffix == 1 {
            base.clone()
        } else {
            // Shortened so the numbered name stays within 64 characters.
            let suffix = format!("-{suffix}");
            format!("{}{suffix}", &base[..base.len().min(64 - suffix.len())])
        };
        if let Some(path) = free(&name)? {
            return Ok((name, path));
        }
    }
    Err(Stop::Failed(format!(
        "{base} and {SUFFIXES} numbered names are all taken"
    )))
}

/// Writes the marker that tells a start in this worktree which registry
/// row it belongs to. A marker already there is kept.
fn write_marker(registry: &Registry, worktree: i64, path: &Path) -> Result<(), String> {
    let git_dir = git::query(path, &["rev-parse", "--absolute-git-dir"])?;
    let marker = Path::new(git_dir.trim()).join(MARKER);
    let body = json!({"registry": registry.registry_id()?, "worktree": worktree});
    let mut file = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&marker)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
        Err(error) => return Err(format!("{}: {error}", marker.display())),
    };
    file.write_all(format!("{body}\n").as_bytes())
        .map_err(|error| format!("{}: {error}", marker.display()))
}

/// One worktree as `git worktree list --porcelain` shows it.
struct Entry {
    path: String,
    branch: Option<String>,
    locked: Option<String>,
}

fn entries(root: &Path) -> Result<Vec<Entry>, String> {
    let list = git::query(root, &["worktree", "list", "--porcelain", "-z"])?;
    let mut entries: Vec<Entry> = Vec::new();
    for field in list.split('\0') {
        if let Some(path) = field.strip_prefix("worktree ") {
            entries.push(Entry {
                path: path.to_owned(),
                branch: None,
                locked: None,
            });
        } else if let Some(entry) = entries.last_mut() {
            if let Some(branch) = field.strip_prefix("branch refs/heads/") {
                entry.branch = Some(branch.to_owned());
            } else if field == "locked" {
                entry.locked = Some(String::new());
            } else if let Some(reason) = field.strip_prefix("locked ") {
                entry.locked = Some(reason.to_owned());
            }
        }
    }
    Ok(entries)
}

/// Who cleans up a half-made worktree.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Cleanup {
    /// The helper that made it, right after its own failure or cancel.
    Own,
    /// Someone else, later, after its helper died. A person may have used
    /// the worktree since, so git must agree to remove it.
    Later,
}

pub(super) enum Cleaned {
    Removed,
    /// Left in place, and ready to use.
    Kept(String),
    /// No longer a worktree, but files stay at its path.
    Left(String),
}

/// Removes what a create made before it stopped, and marks the row failed.
/// Never runs a repository-wide prune. Only the directory with the identity
/// the create recorded, and git's record of it, are removed; a branch the
/// create made goes only while it still points where it started and no
/// worktree has it checked out.
pub(super) fn remove_half_made(
    registry: &mut Registry,
    worktree: i64,
    cleanup: Cleanup,
) -> Result<Cleaned, String> {
    // Read again: only a row still being made is cleaned.
    let Some(row) = registry.worktree(worktree)? else {
        return Ok(Cleaned::Removed);
    };
    match row.state.as_str() {
        "reserved" | "creating" => {}
        // Cleaned already, by its helper or an earlier command.
        "failed" | "removed" => return Ok(Cleaned::Removed),
        state => return Ok(Cleaned::Kept(format!("it is {state}"))),
    }
    let root = Path::new(&row.repo_root);
    let path = Path::new(&row.path);
    let entry = entries(root)?
        .into_iter()
        .find(|entry| entry.path == row.path);
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if entry.is_some() {
                // Only git's record of masil's own path; nothing is on disk.
                // Twice, in case git's own lock is still on the record.
                git::query(
                    root,
                    &["worktree", "remove", "--force", "--force", "--", &row.path],
                )?;
            }
        }
        Err(error) => return Err(format!("{}: {error}", path.display())),
        Ok(metadata) if Some((metadata.dev(), metadata.ino())) == row.identity => match entry {
            None => {
                if let Some(left) = remove_leftover(path, cleanup)? {
                    registry.set_state(row.id, &["reserved", "creating"], "failed", now_ms())?;
                    return Ok(Cleaned::Left(left));
                }
            }
            Some(entry) => {
                // git holds this lock until its checkout is complete.
                let initializing = entry.locked.as_deref() == Some("initializing");
                // A plain remove still deletes ignored files (`.env`, notes),
                // so a later cleanup keeps any worktree with anything to show.
                if cleanup == Cleanup::Later && !initializing {
                    let status = git::query(
                        path,
                        &["status", "--porcelain", "-z", "-uall", "--ignored=matching"],
                    )?;
                    if !status.is_empty() {
                        return keep(registry, &row, "it has changed, untracked or ignored files");
                    }
                }
                let mut args = vec!["worktree", "remove"];
                match (cleanup, initializing) {
                    (_, true) => args.extend(["--force", "--force"]),
                    (Cleanup::Own, false) => args.push("--force"),
                    (Cleanup::Later, false) => {}
                }
                args.extend(["--", &row.path]);
                if let Err(error) = git::query(root, &args) {
                    if cleanup == Cleanup::Later && !initializing {
                        return keep(registry, &row, &format!("git would not remove it: {error}"));
                    }
                    return Err(error);
                }
            }
        },
        // Not the directory masil made: it and git's record of it stay.
        Ok(_) => {}
    }
    if row.branch_created
        && let Some(oid) = &row.from_oid
        && !entries(root)?
            .iter()
            .any(|entry| entry.branch.as_deref() == Some(row.branch.as_str()))
    {
        // Fails, and is left alone, if the branch moved or is gone.
        let _ = git::probe(
            root,
            &[
                "update-ref",
                "-d",
                &format!("refs/heads/{}", row.branch),
                oid,
            ],
        );
    }
    registry.set_state(row.id, &["reserved", "creating"], "failed", now_ms())?;
    Ok(Cleaned::Removed)
}

/// What is left at masil's path when git keeps no record of it: nothing,
/// git's `.git` file, or part of a checkout git failed to delete. The first
/// two go. The rest goes only right after the helper's own failure; later,
/// someone may have put files there, so they stay.
fn remove_leftover(path: &Path, cleanup: Cleanup) -> Result<Option<String>, String> {
    let error = |error: std::io::Error| format!("{}: {error}", path.display());
    let names: Vec<std::ffi::OsString> = match fs::read_dir(path) {
        Ok(entries) => entries
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<Result<_, _>>()
            .map_err(error)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(error(e)),
    };
    let git_file = path.join(".git");
    let only_git_file = names.len() == 1
        && names[0] == ".git"
        && fs::symlink_metadata(&git_file).is_ok_and(|metadata| metadata.is_file());
    if names.is_empty() || only_git_file {
        if only_git_file {
            fs::remove_file(&git_file).map_err(error)?;
        }
        fs::remove_dir(path).map_err(error)?;
        return Ok(None);
    }
    if cleanup == Cleanup::Own {
        fs::remove_dir_all(path).map_err(error)?;
        return Ok(None);
    }
    Ok(Some(format!(
        "{} is no longer a worktree but still has files; they were left in place",
        path.display()
    )))
}

/// A worktree git finished making but will not remove (it has changes, or
/// someone locked it) stays as a ready worktree.
fn keep(registry: &mut Registry, row: &Worktree, why: &str) -> Result<Cleaned, String> {
    write_marker(registry, row.id, Path::new(&row.path))?;
    registry.set_state(row.id, &["creating"], "ready", now_ms())?;
    Ok(Cleaned::Kept(why.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_branch_gives_a_safe_name() {
        assert_eq!(derived_name("feat/login"), "feat-login");
        assert_eq!(derived_name("fix/a.b_c"), "fix-a.b_c");
        // Empty, dot-led or dash-led results fall back to a digest name.
        for branch in ["한글", ".hidden", "-x", "..", "/lead"] {
            let name = derived_name(branch);
            assert!(
                name.starts_with("wt-") && name.len() == 11,
                "{branch} -> {name}"
            );
            assert!(valid_name(&name));
        }
        assert_ne!(derived_name("한글"), derived_name("日本"));
        assert_eq!(derived_name(&"a".repeat(80)).len(), 64);
    }

    #[test]
    fn names_follow_the_rule() {
        for name in ["a", "feat-1", "A.b_c", &"x".repeat(64)] {
            assert!(valid_name(name), "{name}");
        }
        for name in ["", ".", "..", ".x", "-x", "a/b", "a b", &"x".repeat(65)] {
            assert!(!valid_name(name), "{name}");
        }
    }
}
