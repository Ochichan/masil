//! Setup: commands and file copies that make a new worktree ready to work
//! in. Configured by the user in `~/.config/masil/worktree.toml`, or by the
//! repository in `.masil/worktree.toml` once the user trusts that file.

use super::git;
use super::helper::CANCEL;
use super::registry::{End, Job, Registry, Worktree};
use super::remove::status_entries;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

const MAX_FILE: u64 = 64 * 1024;
const MAX_COMMANDS: usize = 32;
const MAX_COPIES: usize = 64;
const DEFAULT_TIMEOUT_S: u64 = 30 * 60;
const MAX_TIMEOUT_S: u64 = 24 * 60 * 60;
/// Where a repository keeps its own setup, relative to its main worktree.
pub(super) const REPO_FILE: &str = ".masil/worktree.toml";

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(super) struct Setup {
    #[serde(default)]
    pub(super) commands: Vec<Vec<String>>,
    #[serde(default)]
    pub(super) copy: Vec<String>,
    pub(super) timeout_s: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UserFile {
    #[serde(default)]
    repo: Vec<UserRepo>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UserRepo {
    /// The repository's common git directory, such as `~/src/app/.git`.
    #[serde(rename = "match")]
    matches: String,
    #[serde(default)]
    commands: Vec<Vec<String>>,
    #[serde(default)]
    copy: Vec<String>,
    timeout_s: Option<u64>,
}

/// The setup a repository gets, and where it came from.
pub(super) struct Plan {
    pub(super) source: &'static str,
    pub(super) file: PathBuf,
    pub(super) setup: Setup,
}

impl Setup {
    fn check(&self, file: &Path) -> Result<(), String> {
        let invalid = |what: String| format!("setup_invalid: {}: {what}", file.display());
        if self.commands.is_empty() && self.copy.is_empty() {
            return Err(invalid("it has no commands and nothing to copy".into()));
        }
        if self.commands.len() > MAX_COMMANDS || self.copy.len() > MAX_COPIES {
            return Err(invalid(format!(
                "at most {MAX_COMMANDS} commands and {MAX_COPIES} copies"
            )));
        }
        for command in &self.commands {
            if command.first().is_none_or(String::is_empty)
                || command.iter().any(|arg| arg.contains('\0'))
            {
                return Err(invalid(
                    "each command is a non-empty list of arguments, the program first".into(),
                ));
            }
        }
        for path in &self.copy {
            relative(path).map_err(invalid)?;
        }
        if self
            .timeout_s
            .is_some_and(|seconds| !(1..=MAX_TIMEOUT_S).contains(&seconds))
        {
            return Err(invalid(format!("timeout_s is 1 to {MAX_TIMEOUT_S}")));
        }
        Ok(())
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_s.unwrap_or(DEFAULT_TIMEOUT_S))
    }
}

/// The parts of a relative path inside a worktree; no `..`, no root.
fn relative(path: &str) -> Result<Vec<&std::ffi::OsStr>, String> {
    let parts: Vec<_> = Path::new(path)
        .components()
        .map(|component| match component {
            Component::Normal(part) => Ok(part),
            _ => Err(format!(
                "copy path {path} must be relative, without . or .."
            )),
        })
        .collect::<Result<_, _>>()?;
    if parts.is_empty() {
        return Err("a copy path is empty".into());
    }
    Ok(parts)
}

/// Reads a small regular file without following a final symlink.
fn read_file(path: &Path, owned: bool) -> Result<Option<Vec<u8>>, String> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    let metadata = file
        .metadata()
        .map_err(|error| format!("{}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    // SAFETY: geteuid has no preconditions.
    if owned && (metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o022 != 0) {
        return Err(format!(
            "{} must be owned by you and not writable by group or others",
            path.display()
        ));
    }
    if metadata.len() > MAX_FILE {
        return Err(format!("{} exceeds 64 KiB", path.display()));
    }
    let mut bytes = Vec::new();
    file.take(MAX_FILE + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(Some(bytes))
}

fn user_file() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map(|home| home.join(".config"))
        })
        .map(|base| base.join("masil/worktree.toml"))
}

fn expand(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(path),
    }
}

/// A repository's own setup file: its bytes and their digest, parsed from
/// the same bytes.
pub(super) fn repo_setup(repo_root: &Path) -> Result<Option<(PathBuf, Setup, String)>, String> {
    let file = repo_root.join(REPO_FILE);
    let Some(bytes) = read_file(&file, false)? else {
        return Ok(None);
    };
    use sha2::{Digest, Sha256};
    let digest: String = Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| format!("setup_invalid: {} is not UTF-8", file.display()))?;
    let setup: Setup = toml::from_str(text)
        .map_err(|error| format!("setup_invalid: {}: {error}", file.display()))?;
    setup.check(&file)?;
    Ok(Some((file, setup, digest)))
}

/// The common git directory's device and inode, which trust is bound to.
pub(super) fn identity(common_dir: &str) -> Result<(u64, u64), String> {
    std::fs::metadata(common_dir)
        .map(|metadata| (metadata.dev(), metadata.ino()))
        .map_err(|error| format!("{common_dir}: {error}"))
}

/// The setup for a repository: the user's entry for it, else the
/// repository's own file if the user trusts it as it is now.
pub(super) fn plan(
    registry: &Registry,
    common_dir: &str,
    repo_root: &Path,
) -> Result<Plan, String> {
    if let Some(file) = user_file()
        && let Some(bytes) = read_file(&file, true)?
    {
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| format!("setup_invalid: {} is not UTF-8", file.display()))?;
        let parsed: UserFile = toml::from_str(text)
            .map_err(|error| format!("setup_invalid: {}: {error}", file.display()))?;
        for entry in parsed.repo {
            let matches = expand(&entry.matches)
                .canonicalize()
                .is_ok_and(|path| path == Path::new(common_dir));
            if matches {
                let setup = Setup {
                    commands: entry.commands,
                    copy: entry.copy,
                    timeout_s: entry.timeout_s,
                };
                setup.check(&file)?;
                return Ok(Plan {
                    source: "user",
                    file,
                    setup,
                });
            }
        }
    }
    let Some((file, setup, digest)) = repo_setup(repo_root)? else {
        return Err(format!(
            "setup_missing: no setup for this repository; add a [[repo]] entry with match = \
             \"{common_dir}\" to ~/.config/masil/worktree.toml, or a trusted {REPO_FILE}"
        ));
    };
    let trusted = registry.trusted(common_dir)?;
    if trusted != Some((identity(common_dir)?, digest)) {
        return Err(format!(
            "setup_untrusted: {} is not trusted as it is now; read it, then run \
             `masil-agent worktree trust {}`",
            file.display(),
            repo_root.display()
        ));
    }
    Ok(Plan {
        source: "repo",
        file,
        setup,
    })
}

fn error_text(what: &str, path: &Path) -> String {
    format!(
        "{what} {}: {}",
        path.display(),
        std::io::Error::last_os_error()
    )
}

enum Copied {
    /// Made, with the sha256 of what was written.
    Made(String),
    Exists,
    Missing,
}

const DIRECTORY: libc::c_int =
    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;

/// Opens `root`, then each of `parts` below it, never following a symlink.
/// With `make`, a missing directory is made; without it, `None` says one is
/// missing.
fn walk(root: &Path, parts: &[&std::ffi::OsStr], make: bool) -> Result<Option<OwnedFd>, String> {
    let path = CString::new(root.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
    // SAFETY: a valid C string; the result is checked.
    let fd = unsafe { libc::open(path.as_ptr(), DIRECTORY) };
    if fd < 0 {
        return Err(error_text("cannot open", root));
    }
    // SAFETY: fd was just opened and is owned here.
    let mut directory = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut walked = root.to_path_buf();
    for part in parts {
        walked.push(part);
        let name = CString::new(part.as_bytes()).map_err(|e| e.to_string())?;
        // SAFETY: valid descriptor and C string; results are checked.
        let mut fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), DIRECTORY) };
        if fd < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT) {
            if !make {
                return Ok(None);
            }
            // SAFETY: as above.
            if unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o755) } != 0 {
                return Err(error_text("cannot make", &walked));
            }
            // SAFETY: as above.
            fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), DIRECTORY) };
        }
        if fd < 0 {
            return Err(error_text(
                "cannot enter (not a directory, or a symlink)",
                &walked,
            ));
        }
        // SAFETY: fd was just opened and is owned here.
        directory = unsafe { OwnedFd::from_raw_fd(fd) };
    }
    Ok(Some(directory))
}

/// Copies `rel` from the main worktree into the new one. Both paths are
/// walked without following symlinks; the source must be a regular file,
/// and the target is made new, never overwritten. A failed write leaves no
/// partial file.
fn copy_into(source_root: &Path, target_root: &Path, rel: &str) -> Result<Copied, String> {
    let parts = relative(rel)?;
    let (last, parents) = parts.split_last().ok_or("a copy path is empty")?;
    let name = CString::new(last.as_bytes()).map_err(|e| e.to_string())?;
    let source_path = source_root.join(rel);
    let Some(source_directory) = walk(source_root, parents, false)? else {
        return Ok(Copied::Missing);
    };
    // SAFETY: valid descriptor and C string; the result is checked.
    let fd = unsafe {
        libc::openat(
            source_directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        if std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT) {
            return Ok(Copied::Missing);
        }
        return Err(error_text("cannot read", &source_path));
    }
    // SAFETY: fd was just opened and is owned by the File.
    let mut source = unsafe { File::from_raw_fd(fd) };
    let metadata = source
        .metadata()
        .map_err(|error| format!("{}: {error}", source_path.display()))?;
    if !metadata.is_file() {
        return Err(format!("{} is not a regular file", source_path.display()));
    }
    let directory = walk(target_root, parents, true)?.ok_or("a directory could not be made")?;
    // SAFETY: valid descriptor and C string; the result is checked.
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            (metadata.mode() & 0o777) as libc::c_uint,
        )
    };
    let target = target_root.join(rel);
    if fd < 0 {
        if std::io::Error::last_os_error().raw_os_error() == Some(libc::EEXIST) {
            return Ok(Copied::Exists);
        }
        return Err(error_text("cannot make", &target));
    }
    // SAFETY: fd was just opened and is owned by the File.
    let mut file = unsafe { File::from_raw_fd(fd) };
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    let written = (|| -> std::io::Result<()> {
        loop {
            let read = source.read(&mut buffer)?;
            if read == 0 {
                return file.flush();
            }
            digest.update(&buffer[..read]);
            file.write_all(&buffer[..read])?;
        }
    })();
    if let Err(error) = written {
        // SAFETY: the name in the directory this function just made it in.
        unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) };
        return Err(format!("{}: {error}", target.display()));
    }
    Ok(Copied::Made(hex(&digest.finalize())))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The sha256 of a file setup copied, as it is now; `None` if it is not a
/// readable regular file any more.
pub(super) fn copied_digest(path: &Path) -> Option<String> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    std::io::copy(&mut file, &mut digest).ok()?;
    Some(hex(&digest.finalize()))
}

/// Ignored directories, as `git status` names them at the directory level.
/// Loose ignored files are left out: setup records only the directories it
/// makes, so a file a person writes meanwhile is still asked about.
fn ignored_directories(path: &Path) -> Result<BTreeSet<String>, String> {
    let output = git::query(
        path,
        &[
            "status",
            "--porcelain=v2",
            "-z",
            "--untracked-files=normal",
            "--ignored=matching",
            "--ignore-submodules=none",
        ],
    )?;
    Ok(status_entries(&output)
        .into_iter()
        .filter_map(|entry| {
            entry
                .strip_prefix("!! ")
                .and_then(|path| path.strip_suffix('/'))
                .map(str::to_owned)
        })
        .collect())
}

pub(super) fn run(registry: &mut Registry, job: &Job, log: &File) -> End {
    let Some(id) = job.request["worktree"].as_i64() else {
        return End::Failed("the job request names no worktree".into());
    };
    let row = match registry.worktree(id) {
        Ok(Some(row)) if row.state == "ready" => row,
        Ok(Some(row)) => {
            return End::Failed(format!(
                "worktree_unavailable: worktree {} is {}",
                row.name, row.state
            ));
        }
        Ok(None) => return End::Failed(format!("worktree {id} is not in the registry")),
        Err(error) => return End::Failed(error),
    };
    // New directories count as setup's, so no agent may be working here;
    // a start finds this job open and is refused (lease::begin).
    if let Err(error) = super::lease::prune(registry, Some(id)) {
        return End::Failed(error);
    }
    match registry.leases(Some(id)) {
        Ok(leases) if leases.is_empty() => {}
        Ok(leases) => {
            return End::Failed(format!(
                "worktree_in_use: run {} uses worktree {}; setup runs only while no agent works in it",
                leases[0].run, row.name
            ));
        }
        Err(error) => return End::Failed(error),
    }
    let plan = match plan(registry, &row.repo_common_dir, Path::new(&row.repo_root)) {
        Ok(plan) => plan,
        Err(error) => return End::Failed(error),
    };
    let path = Path::new(&row.path);
    let before = match ignored_directories(path) {
        Ok(before) => before,
        Err(error) => return End::Failed(error),
    };
    let outcome = steps(registry, job, &row, &plan, log);
    // Directories setup made, even before it stopped, are setup's, so
    // removal does not ask about them. Agents cannot start here meanwhile.
    if let Ok(after) = ignored_directories(path) {
        // A directory that holds copies is not recorded as a whole: each
        // copy is recorded with its content instead.
        let made: Vec<String> = after
            .difference(&before)
            .filter(|dir| {
                !plan
                    .setup
                    .copy
                    .iter()
                    .any(|copy| copy.starts_with(&format!("{dir}/")))
            })
            .cloned()
            .collect();
        let _ = registry.add_setup_paths(id, &made, "ignored");
    }
    match outcome {
        Ok(mut result) => {
            result["worktree"] = row.to_json();
            result["source"] = json!(plan.source);
            result["file"] = json!(plan.file);
            End::Succeeded(result)
        }
        Err(end) => end,
    }
}

fn steps(
    registry: &mut Registry,
    job: &Job,
    row: &Worktree,
    plan: &Plan,
    log: &File,
) -> Result<Value, End> {
    let path = Path::new(&row.path);
    let mut copied = Vec::new();
    let mut skipped = Vec::new();
    for rel in &plan.setup.copy {
        match copy_into(Path::new(&row.repo_root), path, rel).map_err(End::Failed)? {
            Copied::Made(digest) => {
                // Recorded with its content: once a person edits it, removal
                // asks about it again.
                registry
                    .add_setup_paths(row.id, std::slice::from_ref(rel), &format!("copy:{digest}"))
                    .map_err(End::Failed)?;
                copied.push(rel.clone());
            }
            Copied::Exists => skipped.push(json!({"path": rel, "why": "exists"})),
            Copied::Missing => {
                skipped.push(json!({"path": rel, "why": "missing in the repository"}))
            }
        }
    }
    let deadline = Instant::now() + plan.setup.timeout();
    for (index, argv) in plan.setup.commands.iter().enumerate() {
        if CANCEL.load(Ordering::SeqCst) {
            return Err(End::Cancelled(format!(
                "cancelled before command {}",
                index + 1
            )));
        }
        let _ = writeln!(&*log, "$ {}", argv.join(" "));
        run_command(
            registry,
            job,
            path,
            argv,
            log,
            deadline,
            plan.setup.timeout(),
        )
        .map_err(|end| match end {
            End::Failed(text) => {
                End::Failed(format!("command {} ({}): {text}", index + 1, argv[0]))
            }
            other => other,
        })?;
    }
    Ok(json!({"commands": plan.setup.commands.len(), "copied": copied, "skipped": skipped}))
}

#[allow(clippy::too_many_arguments)]
fn run_command(
    registry: &Registry,
    job: &Job,
    dir: &Path,
    argv: &[String],
    log: &File,
    deadline: Instant,
    timeout: Duration,
) -> Result<(), End> {
    let clone = || {
        log.try_clone()
            .map_err(|error| End::Failed(error.to_string()))
    };
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(clone()?)
        .stderr(clone()?)
        .process_group(0);
    // Setup runs in the user's environment, but never in another
    // repository than the worktree.
    for (name, _) in std::env::vars_os() {
        if name.as_bytes().starts_with(b"GIT_") {
            command.env_remove(name);
        }
    }
    let mut child = command
        .spawn()
        .map_err(|error| End::Failed(format!("cannot start: {error}")))?;
    let pid = child.id() as i32;
    let _ = registry.set_child(
        job.id,
        crate::process::info(pid).map(|info| (pid, info.started)),
    );
    let ended = loop {
        if git::exited(pid) {
            // Anything the command left running in its group stops with it,
            // before the leader is reaped.
            // SAFETY: the group's leader is an unreaped child of this process.
            unsafe { libc::killpg(pid, libc::SIGKILL) };
            break match child.wait() {
                Ok(status) if status.success() => Ok(()),
                Ok(status) => Err(End::Failed(format!("exited with {status}"))),
                Err(error) => Err(End::Failed(error.to_string())),
            };
        }
        if CANCEL.load(Ordering::SeqCst) {
            git::stop(&mut child);
            break Err(End::Cancelled("cancelled while setup ran".into()));
        }
        if Instant::now() >= deadline {
            git::stop(&mut child);
            break Err(End::Failed(format!(
                "setup did not finish within {} s and was stopped",
                timeout.as_secs()
            )));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let _ = registry.set_child(job.id, None);
    ended
}
