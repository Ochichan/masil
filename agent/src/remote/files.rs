//! Root-confined workspace file explorer and git changes/diff bridge reusing
//! `crate::changes`.

use crate::changes;
use crate::remote::discovery::{discover_importable_sessions, list_all_host_agents};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

const MAX_DIR_ENTRIES: usize = 500;
const MAX_READ_BYTES: usize = 512 * 1024;

fn canonical_root(cwd: &str) -> Result<PathBuf, String> {
    let p = Path::new(cwd);
    if !p.is_dir() {
        return Err(format!("workspace directory not found: {cwd}"));
    }
    p.canonicalize()
        .map_err(|e| format!("cannot canonicalize workspace root: {e}"))
}

fn resolve_under_root(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let trimmed = rel.trim().trim_start_matches('/');
    if trimmed.is_empty() || trimmed == "." {
        return Ok(root.to_path_buf());
    }
    if trimmed.contains("..") {
        return Err("path traversal outside workspace root is forbidden".into());
    }
    let joined = root.join(trimmed);
    let canon = joined
        .canonicalize()
        .map_err(|e| format!("path not found: {e}"))?;
    if !canon.starts_with(root) {
        return Err("symlink target escapes workspace root".into());
    }
    Ok(canon)
}

pub fn list_directory(cwd: &str, rel_path: &str) -> Result<Value, String> {
    let root = canonical_root(cwd)?;
    let target = resolve_under_root(&root, rel_path)?;
    if !target.is_dir() {
        return Err("target is not a directory".into());
    }
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    let mut truncated = false;
    let rd = fs::read_dir(&target).map_err(|e| format!("cannot read directory: {e}"))?;
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".git" {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let path_rel = entry
            .path()
            .strip_prefix(&root)
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| name.clone());
        let item = json!({
            "name": name,
            "path": path_rel,
            "is_dir": meta.is_dir(),
            "size": if meta.is_dir() { 0 } else { meta.len() },
        });
        if meta.is_dir() {
            dirs.push(item);
        } else {
            files.push(item);
        }
        if dirs.len() + files.len() >= MAX_DIR_ENTRIES {
            truncated = true;
            break;
        }
    }
    dirs.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    files.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    dirs.extend(files);
    Ok(json!({
        "root": root.to_string_lossy(),
        "path": target.strip_prefix(&root).ok().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default(),
        "entries": dirs,
        "truncated": truncated,
    }))
}

pub fn read_file_preview(cwd: &str, rel_path: &str) -> Result<Value, String> {
    let root = canonical_root(cwd)?;
    let target = resolve_under_root(&root, rel_path)?;
    if !target.is_file() {
        return Err("target is not a regular file".into());
    }
    let file = File::open(&target).map_err(|e| format!("cannot open file: {e}"))?;
    let total_size = file.metadata().map(|m| m.len()).unwrap_or(0);
    let mut buf = Vec::new();
    file.take((MAX_READ_BYTES + 1) as u64)
        .read_to_end(&mut buf)
        .map_err(|e| format!("cannot read file: {e}"))?;
    let truncated = buf.len() > MAX_READ_BYTES;
    if truncated {
        buf.truncate(MAX_READ_BYTES);
    }
    let is_binary = buf.iter().take(4096).any(|&b| b == 0);
    let content = if is_binary {
        String::new()
    } else {
        String::from_utf8_lossy(&buf).into_owned()
    };
    Ok(json!({
        "path": target.strip_prefix(&root).ok().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default(),
        "size": total_size,
        "truncated": truncated,
        "binary": is_binary,
        "content": content,
    }))
}

pub fn git_changes_list(cwd: &str) -> Result<Value, String> {
    let root = changes::root_of(Path::new(cwd))?;
    changes::list(&root)
}

pub fn git_changes_diff(cwd: &str, rel_path: &str) -> Result<Value, String> {
    changes::check_relative(rel_path)?;
    let root = changes::root_of(Path::new(cwd))?;
    changes::diff(&root, rel_path)
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn shorten_home(path: &Path, home: &Path) -> String {
    if let Ok(rel) = path.strip_prefix(home) {
        if rel.as_os_str().is_empty() {
            "~".into()
        } else {
            format!("~/{}", rel.to_string_lossy())
        }
    } else {
        path.to_string_lossy().into_owned()
    }
}

fn expand_home_input(raw: &str, home: &Path) -> PathBuf {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        for candidate in [
            home.join("Documents").join("Projects"),
            home.join("Projects"),
            home.join("Developer"),
            home.join("code"),
        ] {
            if candidate.is_dir() {
                return candidate;
            }
        }
        return home.to_path_buf();
    }
    if trimmed == "~" {
        return home.to_path_buf();
    }
    if let Some(rest) = trimmed.strip_prefix("~/") {
        return home.join(rest);
    }
    PathBuf::from(trimmed)
}

fn is_git_repo(dir: &Path) -> bool {
    dir.join(".git").exists()
}

fn build_breadcrumbs(current: &Path, home: &Path) -> Vec<Value> {
    let mut crumbs = Vec::new();
    if let Ok(rel) = current.strip_prefix(home) {
        crumbs.push(json!({
            "label": "~",
            "path": home.to_string_lossy(),
        }));
        let mut acc = home.to_path_buf();
        for comp in rel.components() {
            acc.push(comp);
            crumbs.push(json!({
                "label": comp.as_os_str().to_string_lossy(),
                "path": acc.to_string_lossy(),
            }));
        }
        return crumbs;
    }

    let mut acc = PathBuf::new();
    for comp in current.components() {
        acc.push(comp);
        let label = comp.as_os_str().to_string_lossy().into_owned();
        crumbs.push(json!({
            "label": if label.is_empty() { "/".into() } else { label },
            "path": acc.to_string_lossy(),
        }));
    }
    crumbs
}

fn collect_quick_directories(home: &Path) -> Vec<Value> {
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut out = Vec::new();

    let mut push_dir = |path: PathBuf, source: &str| -> usize {
        if !path.is_dir() {
            return out.len();
        }
        let canon = path.canonicalize().unwrap_or(path);
        let canon_str = canon.to_string_lossy();
        if canon_str.starts_with("/tmp")
            || canon_str.starts_with("/private/tmp")
            || canon_str.starts_with("/var/folders")
            || canon_str.starts_with("/private/var/folders")
        {
            return out.len();
        }
        if !seen.insert(canon.clone()) {
            return out.len();
        }
        let name = canon
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| shorten_home(&canon, home));
        if name.starts_with("uiclimb-rv-") || name.starts_with("tmp.") {
            return out.len();
        }
        out.push(json!({
            "name": name,
            "path": canon.to_string_lossy(),
            "display_path": shorten_home(&canon, home),
            "is_git_repo": is_git_repo(&canon),
            "source": source,
        }));
        out.len()
    };

    // 1. Active masil agents' working directories first.
    for agent in list_all_host_agents() {
        if let Some(cwd) = agent.get("cwd").and_then(Value::as_str)
            && !cwd.is_empty()
        {
            push_dir(PathBuf::from(cwd), "active");
        }
    }

    // 2. Recent external Claude/Codex/Agy/OpenCode workspaces.
    for sess in discover_importable_sessions(30) {
        if !sess.cwd.is_empty() && push_dir(PathBuf::from(&sess.cwd), "recent") >= 14 {
            break;
        }
    }

    // 3. Common project root bookmarks.
    for bookmark in [
        home.join("Documents").join("Projects"),
        home.join("Projects"),
        home.join("Developer"),
        home.join("Documents"),
        home.join("Desktop"),
        home.to_path_buf(),
    ] {
        push_dir(bookmark, "bookmark");
    }

    out
}

/// Lists subdirectories for the mobile GUI directory picker, along with
/// breadcrumbs and quick-access active/recent project workspaces.
pub fn browse_host_directories(
    raw_path: &str,
    show_hidden: bool,
    include_quick: bool,
) -> Result<Value, String> {
    let home = home_dir();
    let home_canon = home.canonicalize().unwrap_or_else(|_| home.clone());
    let requested = expand_home_input(raw_path, &home_canon);
    let current = if requested.is_dir() {
        requested.canonicalize().unwrap_or(requested)
    } else {
        home_canon.clone()
    };
    let parent = current.parent().map(|p| p.to_string_lossy().into_owned());

    let mut entries = Vec::new();
    let mut truncated = false;
    let rd = fs::read_dir(&current)
        .map_err(|e| format!("cannot read directory {}: {e}", current.display()))?;
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !show_hidden && name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        let Ok(meta) = fs::metadata(&path) else {
            continue;
        };
        if !meta.is_dir() {
            continue;
        }
        let canon = path.canonicalize().unwrap_or(path);
        let git = is_git_repo(&canon);
        entries.push((
            !git,
            name.to_ascii_lowercase(),
            json!({
                "name": name,
                "path": canon.to_string_lossy(),
                "display_path": shorten_home(&canon, &home_canon),
                "is_git_repo": git,
            }),
        ));
        if entries.len() >= MAX_DIR_ENTRIES {
            truncated = true;
            break;
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let sorted: Vec<Value> = entries.into_iter().map(|(_, _, v)| v).collect();

    let quick = if include_quick {
        collect_quick_directories(&home_canon)
    } else {
        Vec::new()
    };

    Ok(json!({
        "home": home_canon.to_string_lossy(),
        "current": current.to_string_lossy(),
        "display_current": shorten_home(&current, &home_canon),
        "current_is_git_repo": is_git_repo(&current),
        "parent": parent,
        "breadcrumbs": build_breadcrumbs(&current, &home_canon),
        "entries": sorted,
        "quick": quick,
        "truncated": truncated,
    }))
}

/// Creates a single new child directory under `parent` for the mobile GUI picker.
pub fn create_host_directory(parent: &str, name: &str) -> Result<Value, String> {
    let home = home_dir();
    let home_canon = home.canonicalize().unwrap_or_else(|_| home.clone());
    let clean_name = name.trim();
    if clean_name.is_empty()
        || clean_name == "."
        || clean_name == ".."
        || clean_name.contains('/')
        || clean_name.contains('\\')
        || clean_name.contains('\0')
    {
        return Err("invalid folder name".into());
    }
    let parent_path = expand_home_input(parent, &home_canon);
    if !parent_path.is_dir() {
        return Err(format!(
            "parent directory not found: {}",
            parent_path.display()
        ));
    }
    let target = parent_path.join(clean_name);
    fs::create_dir(&target).map_err(|e| format!("cannot create directory: {e}"))?;
    let canon = target.canonicalize().unwrap_or(target);
    Ok(json!({
        "name": clean_name,
        "path": canon.to_string_lossy(),
        "display_path": shorten_home(&canon, &home_canon),
        "is_git_repo": false,
    }))
}
