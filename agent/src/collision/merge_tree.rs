//! Quarantined `git merge-tree --write-tree` runner using an isolated
//! `GIT_OBJECT_DIRECTORY` + `GIT_ALTERNATE_OBJECT_DIRECTORIES` + temp
//! `GIT_INDEX_FILE` so the user's `.git/objects` and `.git/index` suffer
//! **0 bytes of mutation** (D-04).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MergeTreeOutcome {
    Clean {
        merged_tree: String,
    },
    HardConflict {
        merged_tree: Option<String>,
        conflict_files: Vec<String>,
        messages: Vec<String>,
    },
}

/// RAII quarantine directory cleaned up automatically when dropped.
pub struct QuarantineDir {
    pub path: PathBuf,
}

impl QuarantineDir {
    pub fn new(prefix: &str) -> Result<Self, String> {
        let nonce = crate::managed::nonce()
            .unwrap_or_else(|_| format!("{}-{}", std::process::id(), crate::observation::now_ms()));
        let path = std::env::temp_dir().join(format!("{prefix}-{nonce}"));
        fs::create_dir_all(path.join("objects/pack")).map_err(|e| e.to_string())?;
        fs::create_dir_all(path.join("objects/info")).map_err(|e| e.to_string())?;
        Ok(Self { path })
    }

    pub fn objects_dir(&self) -> PathBuf {
        self.path.join("objects")
    }
}

impl Drop for QuarantineDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Resolves a repository's canonical `.git` common objects directory.
pub fn common_objects_dir(worktree: &Path) -> Result<PathBuf, String> {
    let out = git_base(worktree)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_owned());
    }
    let common = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    Ok(common.join("objects"))
}

/// Snapshots a worktree's current working files (HEAD + modified + untracked)
/// into a tree OID inside `quarantine` without touching the user's repo.
pub fn snapshot_worktree_in_quarantine(
    worktree: &Path,
    common_objects: &Path,
    quarantine: &QuarantineDir,
    slot: &str,
) -> Result<(String, String), String> {
    let head = git_stdout(worktree, &["rev-parse", "--verify", "HEAD"])?;
    let index_file = quarantine.path.join(format!("index-{slot}"));
    let q_objects = quarantine.objects_dir();

    // Seed isolated temp index from HEAD.
    run_quarantined(
        worktree,
        &q_objects,
        common_objects,
        &index_file,
        &["read-tree", &head],
    )?;

    // Collect changed/untracked paths from `git status --porcelain -z` (read-only).
    let status_out = git_base(worktree)
        .args(["status", "--porcelain=v1", "-z", "--untracked-files=normal"])
        .output()
        .map_err(|e| e.to_string())?;
    if status_out.status.success() {
        let mut to_add: Vec<String> = Vec::new();
        let mut to_remove: Vec<String> = Vec::new();
        let mut entries = status_out.stdout.split(|&b| b == 0).peekable();
        while let Some(entry) = entries.next() {
            if entry.len() < 4 {
                continue;
            }
            let x = entry[0] as char;
            let y = entry[1] as char;
            let path = String::from_utf8_lossy(&entry[3..]).into_owned();
            // Rename entries carry an extra NUL-terminated old path we must consume.
            if matches!(x, 'R' | 'C') {
                let _ = entries.next();
            }
            let full = worktree.join(&path);
            if y == 'D' || (x == 'D' && !full.exists()) {
                to_remove.push(path);
            } else if full.is_file() || full.is_symlink() {
                // Skip huge files (> 10 MiB) to keep collision radar instant.
                let ok_size = fs::symlink_metadata(&full)
                    .map(|m| m.len() <= 10 * 1024 * 1024)
                    .unwrap_or(false);
                if ok_size {
                    to_add.push(path);
                }
            }
        }
        if !to_remove.is_empty() {
            let mut args: Vec<&str> = vec!["update-index", "--force-remove", "--"];
            for p in &to_remove {
                args.push(p.as_str());
            }
            let _ = run_quarantined(worktree, &q_objects, common_objects, &index_file, &args);
        }
        if !to_add.is_empty() {
            let mut args: Vec<&str> = vec!["add", "--"];
            for p in &to_add {
                args.push(p.as_str());
            }
            let _ = run_quarantined(worktree, &q_objects, common_objects, &index_file, &args);
        }
    }

    let tree_oid = run_quarantined(
        worktree,
        &q_objects,
        common_objects,
        &index_file,
        &["write-tree"],
    )?;
    Ok((head, tree_oid))
}

/// Executes quarantined `git merge-tree --write-tree --merge-base=<base> <tree_A> <tree_B>`
/// between two worktrees of the same repository.
pub fn check_worktree_merge_conflict(
    worktree_a: &Path,
    worktree_b: &Path,
    timeout: Duration,
) -> Result<MergeTreeOutcome, String> {
    let deadline = Instant::now() + timeout;
    let common_objects = common_objects_dir(worktree_a)?;
    let quarantine = QuarantineDir::new("masil-mt")?;
    let (head_a, tree_a) =
        snapshot_worktree_in_quarantine(worktree_a, &common_objects, &quarantine, "a")?;
    if Instant::now() >= deadline {
        return Err("collision_timeout: merge-tree snapshot exceeded budget".into());
    }
    let (head_b, tree_b) =
        snapshot_worktree_in_quarantine(worktree_b, &common_objects, &quarantine, "b")?;
    if Instant::now() >= deadline {
        return Err("collision_timeout: merge-tree snapshot exceeded budget".into());
    }

    let merge_base = git_stdout(worktree_a, &["merge-base", &head_a, &head_b])
        .unwrap_or_else(|_| head_a.clone());

    let out = git_base(worktree_a)
        .env("GIT_OBJECT_DIRECTORY", quarantine.objects_dir())
        .env("GIT_ALTERNATE_OBJECT_DIRECTORIES", &common_objects)
        .args([
            "merge-tree",
            "--write-tree",
            &format!("--merge-base={merge_base}"),
            &tree_a,
            &tree_b,
        ])
        .output()
        .map_err(|e| e.to_string())?;

    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut lines = stdout.lines();
    let merged_tree = lines
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    if out.status.success() {
        return Ok(MergeTreeOutcome::Clean {
            merged_tree: merged_tree.unwrap_or_default(),
        });
    }

    let mut conflict_files: Vec<String> = Vec::new();
    let mut messages: Vec<String> = Vec::new();
    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with("CONFLICT") {
            messages.push(trimmed.to_owned());
            if let Some(path) = extract_conflict_path(trimmed)
                && !conflict_files.contains(&path)
            {
                conflict_files.push(path);
            }
        } else if let Some((_, path_part)) = trimmed.split_once('\t') {
            let p = path_part.trim().to_owned();
            if !p.is_empty() && !conflict_files.contains(&p) {
                conflict_files.push(p);
            }
        }
    }

    Ok(MergeTreeOutcome::HardConflict {
        merged_tree,
        conflict_files,
        messages,
    })
}

fn extract_conflict_path(line: &str) -> Option<String> {
    // e.g. `CONFLICT (content): Merge conflict in src/auth.rs`
    if let Some((_, tail)) = line.rsplit_once(" in ") {
        let clean = tail.trim().trim_matches('"');
        if !clean.is_empty() {
            return Some(clean.to_owned());
        }
    }
    None
}

pub(crate) fn git_base(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.autocrlf=false",
            "-c",
            "core.fsmonitor=false",
        ]);
    cmd
}

pub(crate) fn git_stdout(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = git_base(dir)
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_owned());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn run_quarantined(
    worktree: &Path,
    q_objects: &Path,
    common_objects: &Path,
    index_file: &Path,
    args: &[&str],
) -> Result<String, String> {
    let out = git_base(worktree)
        .env("GIT_OBJECT_DIRECTORY", q_objects)
        .env("GIT_ALTERNATE_OBJECT_DIRECTORIES", common_objects)
        .env("GIT_INDEX_FILE", index_file)
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_owned());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}
