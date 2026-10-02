//! What changed in an agent's git working tree: the file list and one
//! file's diff, read without taking any lock in the user's repository.

use crate::worktree::git;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

/// Entries a list shows; the rest are counted.
pub(crate) const SHOWN: usize = 500;
/// A diff is cut here.
pub(crate) const DIFF_LIMIT: usize = 256 * 1024;
/// Bytes of an untracked file read to count its lines.
const COUNT_LIMIT: u64 = 1024 * 1024;
const LIST_LIMIT: usize = 64 * 1024 * 1024;
const TIME: Duration = Duration::from_secs(10);
const TOO_LARGE: &str =
    "changes_too_large: git listed more than masil reads; the working tree has too many changes";
const TIMEOUT: &str =
    "changes_timeout: git did not finish within 10 s; the repository may be very large";

/// git reading the user's repository: no fsmonitor daemon, paths as they
/// are, no external diff or textconv programs.
fn args<'a>(rest: &[&'a str]) -> Vec<&'a str> {
    let mut args = vec!["-c", "core.fsmonitor=false", "-c", "core.quotePath=false"];
    args.extend_from_slice(rest);
    args
}

fn run(
    root: &Path,
    rest: &[&str],
    ok: &[i32],
    deadline: Instant,
    limit: usize,
) -> Result<git::Output, String> {
    git::output_within(root, &args(rest), ok, deadline, limit, TIMEOUT)
}

/// The top of the git working tree that contains `cwd`.
pub(crate) fn root_of(cwd: &Path) -> Result<PathBuf, String> {
    let deadline = Instant::now() + TIME;
    let output = run(
        cwd,
        &["rev-parse", "--show-toplevel"],
        &[0],
        deadline,
        64 * 1024,
    )
    .map_err(|error| {
        format!(
            "changes_unavailable: {} is not in a git working tree: {error}",
            cwd.display()
        )
    })?;
    let text = String::from_utf8(output.stdout)
        .map_err(|_| "changes_unavailable: the working tree path is not UTF-8".to_owned())?;
    Path::new(text.trim())
        .canonicalize()
        .map_err(|error| format!("changes_unavailable: {}: {error}", text.trim()))
}

/// What the changes are measured against.
struct Base {
    /// HEAD, or the empty tree before the first commit.
    tree: String,
    head: Option<String>,
    branch: Option<String>,
}

fn base(root: &Path, deadline: Instant) -> Result<Base, String> {
    let head = run(
        root,
        &["rev-parse", "--verify", "-q", "HEAD"],
        &[0, 1],
        deadline,
        4096,
    )?;
    let head = String::from_utf8_lossy(&head.stdout).trim().to_owned();
    let branch = run(
        root,
        &["symbolic-ref", "-q", "--short", "HEAD"],
        &[0, 1],
        deadline,
        4096,
    )?;
    let branch = String::from_utf8_lossy(&branch.stdout).trim().to_owned();
    let tree = if head.is_empty() {
        // The empty tree's ID depends on the repository's hash.
        let empty = run(
            root,
            &["hash-object", "-t", "tree", "/dev/null"],
            &[0],
            deadline,
            4096,
        )?;
        String::from_utf8_lossy(&empty.stdout).trim().to_owned()
    } else {
        head.clone()
    };
    Ok(Base {
        tree,
        head: (!head.is_empty()).then_some(head),
        branch: (!branch.is_empty()).then_some(branch),
    })
}

/// One changed path.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Entry {
    pub(crate) path: String,
    pub(crate) from: Option<String>,
    pub(crate) kind: &'static str,
    pub(crate) staged: bool,
    pub(crate) unstaged: bool,
    pub(crate) insertions: Option<u64>,
    pub(crate) deletions: Option<u64>,
    pub(crate) binary: bool,
    /// The name is not UTF-8 and is shown approximately; it cannot be
    /// named in `--file`.
    pub(crate) lossy: bool,
}

impl Entry {
    pub(crate) fn to_json(&self) -> Value {
        let mut value = json!({
            "path": self.path,
            "kind": self.kind,
            "staged": self.staged,
            "unstaged": self.unstaged,
            "insertions": self.insertions,
            "deletions": self.deletions,
            "binary": self.binary,
        });
        if let Some(from) = &self.from {
            value["from"] = json!(from);
        }
        if self.lossy {
            value["name_not_utf8"] = json!(true);
        }
        value
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Entries of `git status --porcelain=v2 -z`.
fn status_entries(output: &[u8]) -> Vec<Entry> {
    let mut entries = Vec::new();
    let mut fields = output
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty());
    while let Some(field) = fields.next() {
        let lossy = std::str::from_utf8(field).is_err();
        let line = text(field);
        let kind_of = |xy: &str, sub: &str| -> &'static str {
            if sub.starts_with('S') {
                return "submodule";
            }
            let both: Vec<char> = xy.chars().collect();
            if both.contains(&'D') {
                "deleted"
            } else if both.contains(&'A') {
                "added"
            } else if both.contains(&'T') {
                "type_changed"
            } else {
                "modified"
            }
        };
        let flags = |xy: &str| {
            let mut chars = xy.chars();
            let staged = chars.next().is_some_and(|c| c != '.');
            let unstaged = chars.next().is_some_and(|c| c != '.');
            (staged, unstaged)
        };
        let entry = match line.split(' ').next() {
            Some("1") => {
                let parts: Vec<&str> = line.splitn(9, ' ').collect();
                let (Some(xy), Some(sub), Some(path)) = (parts.get(1), parts.get(2), parts.get(8))
                else {
                    continue;
                };
                let (staged, unstaged) = flags(xy);
                Entry {
                    path: (*path).to_owned(),
                    from: None,
                    kind: kind_of(xy, sub),
                    staged,
                    unstaged,
                    insertions: None,
                    deletions: None,
                    binary: false,
                    lossy,
                }
            }
            Some("2") => {
                let parts: Vec<&str> = line.splitn(10, ' ').collect();
                let (Some(xy), Some(score), Some(path)) =
                    (parts.get(1), parts.get(8), parts.get(9))
                else {
                    continue;
                };
                let from = fields.next().map(text);
                let (staged, unstaged) = flags(xy);
                Entry {
                    path: (*path).to_owned(),
                    from,
                    kind: if score.starts_with('C') {
                        "copied"
                    } else {
                        "renamed"
                    },
                    staged,
                    unstaged,
                    insertions: None,
                    deletions: None,
                    binary: false,
                    lossy,
                }
            }
            Some("u") => {
                let parts: Vec<&str> = line.splitn(11, ' ').collect();
                let Some(path) = parts.get(10) else {
                    continue;
                };
                Entry {
                    path: (*path).to_owned(),
                    from: None,
                    kind: "conflict",
                    staged: true,
                    unstaged: true,
                    insertions: None,
                    deletions: None,
                    binary: false,
                    lossy,
                }
            }
            Some("?") => Entry {
                path: line[2..].to_owned(),
                from: None,
                kind: "untracked",
                staged: false,
                unstaged: true,
                insertions: None,
                deletions: None,
                binary: false,
                lossy,
            },
            _ => continue,
        };
        entries.push(entry);
    }
    entries
}

/// `git diff --numstat -z`: added, deleted (or `-` for binary), path; a
/// rename's paths follow as their own fields.
fn numstat(output: &[u8]) -> HashMap<String, (Option<u64>, Option<u64>, bool)> {
    let mut counts = HashMap::new();
    let mut fields = output.split(|byte| *byte == 0);
    while let Some(field) = fields.next() {
        if field.is_empty() {
            continue;
        }
        let line = text(field);
        let mut parts = line.splitn(3, '\t');
        let (Some(added), Some(deleted), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let path = if path.is_empty() {
            // A rename: the source, then the destination.
            let _from = fields.next();
            fields.next().map(text).unwrap_or_default()
        } else {
            path.to_owned()
        };
        let binary = added == "-";
        counts.insert(path, (added.parse().ok(), deleted.parse().ok(), binary));
    }
    counts
}

/// Rename records of a paired numstat: destination, then its source and
/// counts.
type Counts = (Option<u64>, Option<u64>, bool);

fn renamed_counts(output: &[u8]) -> HashMap<String, (String, Counts)> {
    let mut renames = HashMap::new();
    let mut fields = output.split(|byte| *byte == 0);
    while let Some(field) = fields.next() {
        if field.is_empty() {
            continue;
        }
        let line = text(field);
        let mut parts = line.splitn(3, '\t');
        let (Some(added), Some(deleted), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if !path.is_empty() {
            continue;
        }
        let (Some(from), Some(to)) = (fields.next(), fields.next()) else {
            break;
        };
        renames.insert(
            text(to),
            (
                text(from),
                (added.parse().ok(), deleted.parse().ok(), added == "-"),
            ),
        );
    }
    renames
}

/// Lines in an untracked file, read without following a symlink or
/// blocking on a FIFO: `None` past the read limit, binary when a NUL shows
/// in its first 8 KiB.
fn count_lines(root: &Path, path: &str) -> (Option<u64>, bool) {
    let Ok(file) = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(root.join(path))
    else {
        return (None, false);
    };
    if !file.metadata().is_ok_and(|metadata| metadata.is_file()) {
        return (None, false);
    }
    let mut bytes = Vec::new();
    if file.take(COUNT_LIMIT + 1).read_to_end(&mut bytes).is_err() {
        return (None, false);
    }
    if bytes[..bytes.len().min(8192)].contains(&0) {
        return (None, true);
    }
    if bytes.len() as u64 > COUNT_LIMIT {
        return (None, false);
    }
    let mut lines = bytes.iter().filter(|byte| **byte == b'\n').count() as u64;
    if bytes.last().is_some_and(|byte| *byte != b'\n') {
        lines += 1;
    }
    (Some(lines), false)
}

/// Every changed path, sorted, with counts for the first `shown`.
fn entries(
    root: &Path,
    base: &Base,
    shown: usize,
    deadline: Instant,
) -> Result<Vec<Entry>, String> {
    let status = run(
        root,
        &[
            "status",
            "--porcelain=v2",
            "-z",
            "--untracked-files=all",
            "--ignore-submodules=none",
            "--renames",
        ],
        &[0],
        deadline,
        LIST_LIMIT,
    )?;
    if status.truncated {
        return Err(TOO_LARGE.into());
    }
    let mut entries = status_entries(&status.stdout);
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    if shown == 0 {
        return Ok(entries);
    }
    // Plumbing: porcelain `git diff` refreshes, and so locks, the user's
    // index. Without rename pairing, every path gets its own counts.
    let counts = run(
        root,
        &["diff-index", "--numstat", "-z", "--no-renames", &base.tree],
        &[0],
        deadline,
        LIST_LIMIT,
    )?;
    if counts.truncated {
        return Err(TOO_LARGE.into());
    }
    let counts = numstat(&counts.stdout);
    let renames = if entries.iter().take(shown).any(|entry| entry.from.is_some()) {
        let paired = run(
            root,
            &["diff-index", "--numstat", "-z", "-M", &base.tree],
            &[0],
            deadline,
            LIST_LIMIT,
        )?;
        renamed_counts(&paired.stdout)
    } else {
        HashMap::new()
    };
    for entry in entries.iter_mut().take(shown) {
        if entry.kind == "untracked" {
            if Instant::now() < deadline {
                let (lines, binary) = count_lines(root, &entry.path);
                entry.insertions = lines;
                entry.deletions = lines.map(|_| 0);
                entry.binary = binary;
            }
        } else if let Some(from) = &entry.from {
            // A rename's counts are of the pair, as one paired numstat says.
            if let Some((added, deleted, binary)) = renames
                .get(&entry.path)
                .filter(|(source, _)| source == from)
                .map(|(_, counts)| *counts)
            {
                entry.insertions = added;
                entry.deletions = deleted;
                entry.binary = binary;
            }
        } else if let Some((added, deleted, binary)) = counts.get(&entry.path) {
            entry.insertions = *added;
            entry.deletions = *deleted;
            entry.binary = *binary;
        }
    }
    Ok(entries)
}

/// The changes of the working tree at `root`.
pub(crate) fn list(root: &Path) -> Result<Value, String> {
    let deadline = Instant::now() + TIME;
    let base = base(root, deadline)?;
    let all = entries(root, &base, SHOWN, deadline)?;
    let total = all.len();
    let shown: Vec<Value> = all.iter().take(SHOWN).map(Entry::to_json).collect();
    Ok(json!({
        "root": root,
        "base": {"head": base.head, "branch": base.branch},
        "total": total,
        "truncated": total > SHOWN,
        "files": shown,
    }))
}

/// A path inside the working tree, as given: relative, no `.` or `..`.
pub(crate) fn check_relative(path: &str) -> Result<(), String> {
    inside(path)
}

fn inside(path: &str) -> Result<(), String> {
    let normal = !path.is_empty()
        && Path::new(path)
            .components()
            .all(|part| matches!(part, Component::Normal(_)));
    if normal {
        Ok(())
    } else {
        Err(format!(
            "invalid_argument: {path} must be a relative path inside the working tree"
        ))
    }
}

/// One changed file's diff against the base, cut at [`DIFF_LIMIT`]. The
/// path must be one the change list shows.
pub(crate) fn diff(root: &Path, path: &str) -> Result<Value, String> {
    inside(path)?;
    let deadline = Instant::now() + TIME;
    let base = base(root, deadline)?;
    let entry = entries(root, &base, 0, deadline)?
        .into_iter()
        .find(|entry| entry.path == path)
        .ok_or_else(|| format!("invalid_argument: {path} is not among the changed files"))?;
    if path.ends_with('/') {
        return Err(format!(
            "invalid_argument: {path} is a directory (a repository inside this one), not a file"
        ));
    }
    let output = if entry.kind == "untracked" {
        // `--no-index` differs from /dev/null with status 1.
        run(
            root,
            &[
                "diff",
                "--no-index",
                "--no-color",
                "--no-ext-diff",
                "--no-textconv",
                "--",
                "/dev/null",
                path,
            ],
            &[0, 1],
            deadline,
            DIFF_LIMIT,
        )?
    } else {
        // Plumbing, so the user's index is neither refreshed nor locked.
        let mut rest = vec![
            "diff-index",
            "-p",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "-M",
            base.tree.as_str(),
            "--",
        ];
        if let Some(from) = &entry.from {
            rest.push(from);
        }
        rest.push(path);
        run(root, &rest, &[0], deadline, DIFF_LIMIT)?
    };
    Ok(json!({
        "root": root,
        "path": path,
        "from": entry.from,
        "kind": entry.kind,
        "base": {"head": base.head, "branch": base.branch},
        "truncated": output.truncated,
        "diff": text(&output.stdout),
    }))
}

/// A review request for another agent: what changed, where.
pub(crate) fn handoff_text(list: &Value, limit: usize) -> String {
    let mut text = format!(
        "다음 변경을 검토해 주세요. Please review these changes.\nworking tree: {}\nbase: {}{}\n",
        list["root"].as_str().unwrap_or_default(),
        list["base"]["branch"].as_str().unwrap_or("(detached)"),
        list["base"]["head"]
            .as_str()
            .map(|head| format!(" @ {}", &head[..head.len().min(12)]))
            .unwrap_or_default(),
    );
    let files = list["files"].as_array().cloned().unwrap_or_default();
    let mut written = 0;
    for file in &files {
        let counts = match (file["insertions"].as_u64(), file["deletions"].as_u64()) {
            _ if file["binary"].as_bool() == Some(true) => "binary".to_owned(),
            (Some(added), Some(deleted)) => format!("+{added} -{deleted}"),
            _ => "?".to_owned(),
        };
        // A file name may hold control characters a prompt cannot carry.
        let path: String = file["path"]
            .as_str()
            .unwrap_or_default()
            .chars()
            .map(|c| if c.is_control() { '?' } else { c })
            .collect();
        let line = format!(
            "{} {} {path}\n",
            file["kind"].as_str().unwrap_or_default(),
            counts,
        );
        // Room for the closing note.
        if text.len() + line.len() + 80 > limit {
            break;
        }
        text.push_str(&line);
        written += 1;
    }
    let total = list["total"].as_u64().unwrap_or(files.len() as u64) as usize;
    if written < total {
        text.push_str(&format!("... and {} more files\n", total - written));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_and_numstat_entries_are_read() {
        let status = b"1 .M N... 100644 100644 100644 a b src/my file.rs\0\
                       1 D. N... 100644 000000 000000 a b gone.txt\0\
                       1 .M S.M. 160000 160000 160000 a b vendor/lib\0\
                       2 R. N... 100644 100644 100644 a b R100 new.rs\0old.rs\0\
                       u UU N... 100644 100644 100644 100644 a b c both.rs\0\
                       ? notes.md\0";
        let entries = status_entries(status);
        let kinds: Vec<(&str, &str, Option<&str>)> = entries
            .iter()
            .map(|entry| (entry.path.as_str(), entry.kind, entry.from.as_deref()))
            .collect();
        assert_eq!(
            kinds,
            [
                ("src/my file.rs", "modified", None),
                ("gone.txt", "deleted", None),
                ("vendor/lib", "submodule", None),
                ("new.rs", "renamed", Some("old.rs")),
                ("both.rs", "conflict", None),
                ("notes.md", "untracked", None),
            ]
        );
        assert!(entries[0].unstaged && !entries[0].staged);
        let counts = numstat(b"3\t1\tsrc/my file.rs\0-\t-\tlogo.png\0\x30\t0\t\0old.rs\0new.rs\0");
        assert_eq!(counts["src/my file.rs"], (Some(3), Some(1), false));
        assert_eq!(counts["logo.png"], (None, None, true));
        assert_eq!(counts["new.rs"], (Some(0), Some(0), false));
    }

    #[test]
    fn only_plain_relative_paths_are_inside() {
        for path in ["a.txt", "src/a b.rs"] {
            assert!(inside(path).is_ok(), "{path}");
        }
        for path in ["", "/etc/hosts", "../x", "a/../b", "./a"] {
            assert!(inside(path).is_err(), "{path}");
        }
    }
}
