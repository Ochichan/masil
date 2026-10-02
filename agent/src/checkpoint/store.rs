//! Where checkpoints live: a bare repository per repository, outside the
//! user's, holding only its own objects. Every git call against it turns
//! off filters, hooks, fsmonitor and line-ending conversion, so it stores
//! the bytes on disk and runs nothing of the user's.

use crate::worktree::git;
use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// Stored as on disk: no filters, no conversions. Overrides the work
/// tree's `.gitattributes`.
const ATTRIBUTES: &str = "* -text -eol -filter -ident -working-tree-encoding\n";
/// Config for every git call on a checkpoint repository.
const SAFE: [&str; 10] = [
    "-c",
    "core.autocrlf=false",
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.untrackedCache=false",
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "gc.autoDetach=false",
];

fn key(text: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(text.as_bytes())[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A working tree and the checkpoint repository of its repository.
pub(crate) struct Store {
    /// The working tree's top directory (canonical).
    pub(crate) root: PathBuf,
    pub(crate) worktree_key: String,
    /// The bare checkpoint repository.
    pub(crate) repo: PathBuf,
}

/// Held flocks, released when dropped.
pub(crate) struct Held(#[allow(dead_code)] File);

impl Store {
    /// The store for the working tree at `root`, made if missing.
    pub(crate) fn open(root: &Path, deadline: Instant) -> Result<Self, String> {
        let common = git::output_within(
            root,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            &[0],
            deadline,
            64 * 1024,
            "checkpoint_timeout: git did not answer in time",
        )?;
        let common = String::from_utf8(common.stdout)
            .map_err(|_| "changes_unavailable: the git directory path is not UTF-8".to_owned())?;
        let common = Path::new(common.trim())
            .canonicalize()
            .map_err(|error| format!("changes_unavailable: {}: {error}", common.trim()))?;
        let root_text = root
            .to_str()
            .ok_or("changes_unavailable: the working tree path is not UTF-8")?;
        let base = crate::managed::private_directory(
            &crate::managed::state_base()?.join("masil/checkpoints"),
        )?;
        let repo = base.join(format!(
            "{}.git",
            key(common
                .to_str()
                .ok_or("the git directory path is not UTF-8")?)
        ));
        let store = Self {
            root: root.to_path_buf(),
            worktree_key: key(root_text),
            repo,
        };
        store.ensure(deadline)?;
        Ok(store)
    }

    /// Makes the repository on first use: no template (so no hooks from
    /// `init.templateDir`), owner-only, with its attributes.
    fn ensure(&self, deadline: Instant) -> Result<(), String> {
        // Two first uses at once must not both run git init.
        let parent = self
            .repo
            .parent()
            .ok_or("checkpoint directory has no parent")?;
        let mut name = self.repo.file_name().unwrap_or_default().to_os_string();
        name.push(".init.lock");
        let _init = flock(&parent.join(name), true, Duration::from_secs(10))?;
        if !self.repo.join("HEAD").exists() {
            // init writes HEAD through a ref transaction: no user hooks.
            let mut args = SAFE.to_vec();
            args.extend([
                "init",
                "--bare",
                "--quiet",
                "--template=",
                self.repo.to_str().unwrap_or_default(),
            ]);
            git::output_within(
                parent,
                &args,
                &[0],
                deadline,
                64 * 1024,
                "checkpoint_timeout: git init did not finish in time",
            )?;
            fs::set_permissions(&self.repo, fs::Permissions::from_mode(0o700))
                .map_err(|error| format!("{}: {error}", self.repo.display()))?;
        }
        for dir in ["info", "masil-index", "masil-locks"] {
            crate::managed::private_directory(&self.repo.join(dir))?;
        }
        let attributes = self.repo.join("info/attributes");
        if fs::read_to_string(&attributes).ok().as_deref() != Some(ATTRIBUTES) {
            fs::write(&attributes, ATTRIBUTES)
                .map_err(|error| format!("{}: {error}", attributes.display()))?;
        }
        Ok(())
    }

    pub(crate) fn index(&self) -> PathBuf {
        self.repo.join("masil-index").join(&self.worktree_key)
    }

    pub(crate) fn ref_prefix(&self) -> String {
        format!("refs/masil/{}/", self.worktree_key)
    }

    /// git on the checkpoint repository, with this working tree and its
    /// index when `work` is set; it runs in the working tree so the paths
    /// it is given are relative to it.
    pub(crate) fn git(&self, work: bool) -> Command {
        let mut command = git::command(if work { &self.root } else { &self.repo });
        command
            .env("GIT_DIR", &self.repo)
            .env("GIT_LITERAL_PATHSPECS", "1")
            .env("GIT_AUTHOR_NAME", "masil")
            .env("GIT_AUTHOR_EMAIL", "masil@localhost")
            .env("GIT_COMMITTER_NAME", "masil")
            .env("GIT_COMMITTER_EMAIL", "masil@localhost")
            .args(SAFE);
        if work {
            command
                .env("GIT_WORK_TREE", &self.root)
                .env("GIT_INDEX_FILE", self.index());
        }
        command
    }

    /// Takes the repository lock (shared for making, exclusive for pruning)
    /// and then this working tree's lock, in that order.
    pub(crate) fn lock(
        &self,
        exclusive_repo: bool,
        wait: Duration,
    ) -> Result<(Held, Option<Held>), String> {
        let repo = flock(&self.repo.join("masil-locks/repo"), exclusive_repo, wait)?;
        if exclusive_repo {
            return Ok((repo, None));
        }
        let worktree = flock(
            &self.repo.join("masil-locks").join(&self.worktree_key),
            true,
            wait,
        )?;
        Ok((repo, Some(worktree)))
    }

    /// Lock files a killed git left for this working tree. Only called
    /// with this working tree's lock and the shared repository lock held,
    /// so no git of ours is using them.
    pub(crate) fn clear_stale_locks(&self) {
        let _ = fs::remove_file(
            self.repo
                .join("masil-index")
                .join(format!("{}.lock", self.worktree_key)),
        );
        remove_locks(&self.repo.join(self.ref_prefix()));
    }

    /// Ref lock files of every working tree. Only called with the exclusive
    /// repository lock held.
    pub(crate) fn clear_ref_locks(&self) {
        if let Ok(worktrees) = fs::read_dir(self.repo.join("refs/masil")) {
            for worktree in worktrees.flatten() {
                remove_locks(&worktree.path());
            }
        }
    }
}

fn remove_locks(dir: &Path) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().ends_with(".lock") {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
}

/// Takes `path`'s flock within `wait`; `checkpoint_busy` otherwise.
pub(crate) fn flock(path: &Path, exclusive: bool, wait: Duration) -> Result<Held, String> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    let operation = if exclusive {
        libc::LOCK_EX
    } else {
        libc::LOCK_SH
    };
    let deadline = Instant::now() + wait;
    loop {
        // SAFETY: flock on a descriptor this function owns.
        if unsafe { libc::flock(file.as_raw_fd(), operation | libc::LOCK_NB) } == 0 {
            return Ok(Held(file));
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EWOULDBLOCK) {
            return Err(format!("{}: {error}", path.display()));
        }
        if Instant::now() >= deadline {
            return Err(
                "checkpoint_busy: another checkpoint of this working tree is being made".into(),
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
