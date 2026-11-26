//! Phase 1B: Real-Time Cross-Worktree Collision Radar (`agent/src/collision/`, D-04).
//! Groups active agents by `repo_key`, detects `same_directory_race`, runs
//! quarantined `git merge-tree --write-tree`, scans lightweight cross-worktree
//! semantic export/import & shared resource dependencies, and persists results
//! into SQLite `collision_snapshots`.

pub mod merge_tree;
pub mod same_dir;
pub mod semantic;
pub mod shadow;

use crate::guard::config::GuardConfig;
use crate::managed::{CollisionSnapshotRecord, Store};
use crate::observation::now_ms;
use crate::plugin::runner::TimelineCardInput;
use merge_tree::{MergeTreeOutcome, check_worktree_merge_conflict, git_base, git_stdout};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const MAX_PAIRWISE_WORKTREES: usize = 8;
const PAIRWISE_BUDGET: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CollisionSeverity {
    Clean,
    SameFileOverlap,
    SemanticRisk,
    HardConflict,
    SameDirectoryRace,
}

impl CollisionSeverity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::SameFileOverlap => "same_file_overlap",
            Self::SemanticRisk => "semantic_risk",
            Self::HardConflict => "hard_conflict",
            Self::SameDirectoryRace => "same_directory_race",
        }
    }

    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "same_directory_race" => Self::SameDirectoryRace,
            "hard_conflict" => Self::HardConflict,
            "semantic_risk" => Self::SemanticRisk,
            "same_file_overlap" => Self::SameFileOverlap,
            _ => Self::Clean,
        }
    }

    pub fn is_auto_warn_worthy(self) -> bool {
        matches!(
            self,
            Self::HardConflict | Self::SemanticRisk | Self::SameDirectoryRace
        )
    }
}

#[derive(Clone, Debug)]
pub struct ActiveWorktreeAgent {
    pub name: String,
    pub pane_id: String,
    #[allow(dead_code)]
    pub run_id: String,
    pub state: String,
    pub cwd: String,
    pub repo_key: String,
    pub repo_root: String,
    pub changed_files: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CollisionPair {
    pub repo_key: String,
    pub agent_a: String,
    pub agent_b: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pane_a: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pane_b: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub worktree_a: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub worktree_b: String,
    pub severity: CollisionSeverity,
    pub files: Vec<String>,
    pub details: Vec<String>,
    pub last_warned_ms: Option<u64>,
    pub computed_ms: u64,
}

impl CollisionPair {
    pub fn to_record(&self) -> CollisionSnapshotRecord {
        CollisionSnapshotRecord {
            repo_key: self.repo_key.clone(),
            agent_a: self.agent_a.clone(),
            agent_b: self.agent_b.clone(),
            severity: self.severity.as_str().to_owned(),
            files_json: serde_json::to_string(&self.files).unwrap_or_else(|_| "[]".into()),
            details_json: serde_json::to_string(&json!({
                "details": self.details,
                "pane_a": self.pane_a,
                "pane_b": self.pane_b,
                "worktree_a": self.worktree_a,
                "worktree_b": self.worktree_b,
            }))
            .unwrap_or_else(|_| "{}".into()),
            last_warned_ms: self.last_warned_ms,
            computed_ms: self.computed_ms,
        }
    }

    pub fn from_record(rec: CollisionSnapshotRecord) -> Self {
        let files: Vec<String> = serde_json::from_str(&rec.files_json).unwrap_or_default();
        let parsed: Value = serde_json::from_str(&rec.details_json).unwrap_or(Value::Null);
        let details: Vec<String> = parsed
            .get("details")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        Self {
            repo_key: rec.repo_key,
            agent_a: rec.agent_a,
            agent_b: rec.agent_b,
            pane_a: parsed["pane_a"].as_str().unwrap_or("").to_owned(),
            pane_b: parsed["pane_b"].as_str().unwrap_or("").to_owned(),
            worktree_a: parsed["worktree_a"].as_str().unwrap_or("").to_owned(),
            worktree_b: parsed["worktree_b"].as_str().unwrap_or("").to_owned(),
            severity: CollisionSeverity::from_str_lossy(&rec.severity),
            files,
            details,
            last_warned_ms: rec.last_warned_ms,
            computed_ms: rec.computed_ms,
        }
    }
}

/// Computes canonical `repo_key` (16-hex SHA-256 of canonical `--git-common-dir`)
/// and working tree top-level root for `cwd`.
pub fn resolve_repo_identity(cwd: &Path) -> Option<(String, String)> {
    let common = git_stdout(
        cwd,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .ok()?;
    let root = git_stdout(cwd, &["rev-parse", "--show-toplevel"]).ok()?;
    let canon_common = Path::new(&common)
        .canonicalize()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or(common);
    let digest = Sha256::digest(canon_common.as_bytes());
    let key: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    Some((key, root))
}

/// Lists modified/untracked relative file paths in a worktree (read-only).
pub fn changed_files_in_worktree(worktree: &Path) -> Vec<String> {
    let Ok(out) = git_base(worktree)
        .args(["status", "--porcelain=v1", "-z", "--untracked-files=normal"])
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    let mut files = Vec::new();
    let mut entries = out.stdout.split(|&b| b == 0);
    while let Some(entry) = entries.next() {
        if entry.len() < 4 {
            continue;
        }
        let x = entry[0] as char;
        let path = String::from_utf8_lossy(&entry[3..]).into_owned();
        if matches!(x, 'R' | 'C') {
            let _ = entries.next();
        }
        if !path.is_empty() && !files.contains(&path) {
            files.push(path);
        }
    }
    files.sort();
    files
}

/// Computes the collision matrix across `agents` (optionally filtered to one
/// `repo_filter` key). Pure computation with bounded 8-worktree cap & 5s budget.
pub fn compute_matrix(
    agents: &[ActiveWorktreeAgent],
    repo_filter: Option<&str>,
    at_ms: u64,
) -> Vec<CollisionPair> {
    let deadline = Instant::now() + PAIRWISE_BUDGET;
    let filtered: Vec<&ActiveWorktreeAgent> = agents
        .iter()
        .filter(|a| a.state != "exited")
        .filter(|a| repo_filter.is_none_or(|rk| a.repo_key == rk))
        .collect();

    // 1. Same-directory races first (D-04).
    let owned_filtered: Vec<ActiveWorktreeAgent> = filtered.iter().copied().cloned().collect();
    let mut pairs = same_dir::detect_same_directory_races(&owned_filtered, at_ms);
    let race_keys: HashSet<(String, String, String)> = pairs
        .iter()
        .map(|p| (p.repo_key.clone(), p.agent_a.clone(), p.agent_b.clone()))
        .collect();

    // 2. Group remaining agents by repo_key for cross-worktree analysis.
    let mut by_repo: HashMap<&str, Vec<&ActiveWorktreeAgent>> = HashMap::new();
    for agent in &filtered {
        if !agent.repo_key.is_empty() {
            by_repo.entry(&agent.repo_key).or_default().push(*agent);
        }
    }

    for (repo_key, mut group) in by_repo {
        if group.len() < 2 {
            continue;
        }
        group.sort_by(|a, b| a.name.cmp(&b.name));
        group.truncate(MAX_PAIRWISE_WORKTREES);

        // Precompute lightweight semantic profiles once per worktree.
        let profiles: Vec<semantic::WorktreeSemanticProfile> = group
            .iter()
            .map(|a| semantic::scan_worktree_files(Path::new(&a.repo_root), &a.changed_files))
            .collect();

        for i in 0..group.len() {
            for j in (i + 1)..group.len() {
                if Instant::now() >= deadline {
                    break;
                }
                let a = group[i];
                let b = group[j];
                let key = (repo_key.to_owned(), a.name.clone(), b.name.clone());
                if race_keys.contains(&key) {
                    continue;
                }
                // Skip identical worktree root already handled by same_dir.
                if a.repo_root == b.repo_root {
                    continue;
                }

                let set_a: HashSet<&str> = a.changed_files.iter().map(String::as_str).collect();
                let set_b: HashSet<&str> = b.changed_files.iter().map(String::as_str).collect();
                let mut overlap: Vec<String> = set_a
                    .intersection(&set_b)
                    .map(|s| (*s).to_owned())
                    .collect();
                overlap.sort();

                let mut severity = if !overlap.is_empty() {
                    CollisionSeverity::SameFileOverlap
                } else {
                    CollisionSeverity::Clean
                };
                let mut details: Vec<String> = Vec::new();
                let mut files: Vec<String> = overlap.clone();

                // Semantic cross-worktree check (< 15ms).
                let sem = semantic::compare_profiles(&a.name, &profiles[i], &b.name, &profiles[j]);
                if !sem.is_empty() {
                    severity = severity.max(CollisionSeverity::SemanticRisk);
                    details.extend(sem.summary_lines);
                    for r in sem.shared_resources {
                        if !files.contains(&r) {
                            files.push(r);
                        }
                    }
                }

                // Quarantined `git merge-tree --write-tree` when either worktree has changes.
                if (!a.changed_files.is_empty() || !b.changed_files.is_empty())
                    && let Ok(mt) = check_worktree_merge_conflict(
                        Path::new(&a.repo_root),
                        Path::new(&b.repo_root),
                        deadline
                            .saturating_duration_since(Instant::now())
                            .max(Duration::from_millis(500)),
                    )
                    && let MergeTreeOutcome::HardConflict {
                        conflict_files,
                        messages,
                        ..
                    } = mt
                {
                    severity = CollisionSeverity::HardConflict;
                    for cf in conflict_files {
                        if !files.contains(&cf) {
                            files.push(cf);
                        }
                    }
                    details.extend(messages);
                }

                if severity == CollisionSeverity::SameFileOverlap && details.is_empty() {
                    details.push(format!(
                        "Both `{}` and `{}` edited the same file(s): {}",
                        a.name,
                        b.name,
                        overlap.join(", ")
                    ));
                }

                files.sort();
                pairs.push(CollisionPair {
                    repo_key: repo_key.to_owned(),
                    agent_a: a.name.clone(),
                    agent_b: b.name.clone(),
                    pane_a: a.pane_id.clone(),
                    pane_b: b.pane_id.clone(),
                    worktree_a: a.repo_root.clone(),
                    worktree_b: b.repo_root.clone(),
                    severity,
                    files,
                    details,
                    last_warned_ms: None,
                    computed_ms: at_ms,
                });
            }
        }
    }

    pairs.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then_with(|| a.agent_a.cmp(&b.agent_a))
            .then_with(|| a.agent_b.cmp(&b.agent_b))
    });
    pairs
}

/// Scans live agents on `socket`, persists collision snapshots to SQLite, and
/// optionally stages queue-only sibling warnings if `collision.auto_warn_sibling = true`.
pub fn scan_and_persist_socket(
    socket: &Path,
    agents: &[crate::managed::Agent],
    cfg: &GuardConfig,
) -> Vec<CollisionPair> {
    if !cfg.collision.enabled {
        return Vec::new();
    }
    let at_ms = now_ms();
    let mut active = Vec::new();
    for agent in agents {
        if agent.state == "exited" || agent.cwd.trim().is_empty() {
            continue;
        }
        let cwd_path = Path::new(&agent.cwd);
        let (repo_key, repo_root) = resolve_repo_identity(cwd_path)
            .unwrap_or_else(|| (format!("dir:{}", agent.cwd), agent.cwd.clone()));
        let changed_files = if repo_key.starts_with("dir:") {
            Vec::new()
        } else {
            changed_files_in_worktree(Path::new(&repo_root))
        };
        active.push(ActiveWorktreeAgent {
            name: agent.name.clone(),
            pane_id: agent.pane_id.clone(),
            run_id: agent.run.clone(),
            state: agent.state.clone(),
            cwd: agent.cwd.clone(),
            repo_key,
            repo_root,
            changed_files,
        });
    }

    let pairs = compute_matrix(&active, None, at_ms);
    if let Ok(mut store) = Store::open(socket) {
        let mut by_repo: HashMap<String, Vec<(String, String)>> = HashMap::new();
        for pair in &pairs {
            let _ = store.upsert_collision_snapshot(&pair.to_record());
            by_repo
                .entry(pair.repo_key.clone())
                .or_default()
                .push((pair.agent_a.clone(), pair.agent_b.clone()));
        }
        for (repo_key, keep) in by_repo {
            let _ = store.prune_collisions_for_repo(&repo_key, &keep);
        }
        if cfg.collision.auto_warn_sibling {
            maybe_auto_warn_siblings(socket, &mut store, &pairs, cfg, at_ms);
        }
    }
    pairs
}

/// Formats a concise warning message staged into a sibling agent's prompt queue.
pub fn format_sibling_warning(pair: &CollisionPair, target: &str, sibling: &str) -> String {
    let files = if pair.files.is_empty() {
        "(working directory)".to_owned()
    } else {
        pair.files
            .iter()
            .take(8)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ")
    };
    let detail = pair
        .details
        .first()
        .cloned()
        .unwrap_or_else(|| format!("severity={}", pair.severity.as_str()));
    format!(
        "[masil Collision Radar · {}] Heads up `{target}`: sibling agent `{sibling}` is concurrently editing conflicting/coupled files ({files}). Detail: {detail}. Please coordinate interfaces or avoid conflicting edits until reconciled.",
        pair.severity.as_str()
    )
}

/// Safely stages a sibling collision warning into `target`'s prompt queue
/// (`agent queue add` — NEVER injects raw keys into an active pane, D-04).
pub fn warn_agent_via_queue(
    socket: &Path,
    target_pane_or_name: &str,
    message: &str,
) -> Result<Value, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let out = Command::new(exe)
        .args([
            "agent",
            "--socket",
            &socket.to_string_lossy(),
            "queue",
            target_pane_or_name,
            "add",
            message,
        ])
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_owned());
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(stdout.trim()).or_else(|_| Ok(json!({ "queued": true })))
}

fn maybe_auto_warn_siblings(
    socket: &Path,
    store: &mut Store,
    pairs: &[CollisionPair],
    cfg: &GuardConfig,
    at_ms: u64,
) {
    let cooldown_ms = cfg.collision.auto_warn_cooldown_secs.saturating_mul(1000);
    let existing = store.list_collision_snapshots(None).unwrap_or_default();
    let warned_map: HashMap<(String, String, String), Option<u64>> = existing
        .into_iter()
        .map(|r| ((r.repo_key, r.agent_a, r.agent_b), r.last_warned_ms))
        .collect();

    for pair in pairs {
        if !pair.severity.is_auto_warn_worthy() {
            continue;
        }
        let key = (
            pair.repo_key.clone(),
            pair.agent_a.clone(),
            pair.agent_b.clone(),
        );
        if let Some(Some(last)) = warned_map.get(&key)
            && at_ms.saturating_sub(*last) < cooldown_ms
        {
            continue;
        }
        let target_a = if pair.pane_a.is_empty() {
            pair.agent_a.as_str()
        } else {
            pair.pane_a.as_str()
        };
        let target_b = if pair.pane_b.is_empty() {
            pair.agent_b.as_str()
        } else {
            pair.pane_b.as_str()
        };
        let msg_a = format_sibling_warning(pair, &pair.agent_a, &pair.agent_b);
        let msg_b = format_sibling_warning(pair, &pair.agent_b, &pair.agent_a);
        let ok_a = warn_agent_via_queue(socket, target_a, &msg_a).is_ok();
        let ok_b = warn_agent_via_queue(socket, target_b, &msg_b).is_ok();
        if ok_a || ok_b {
            let _ =
                store.mark_collision_warned(&pair.repo_key, &pair.agent_a, &pair.agent_b, at_ms);
            // Record `⚡ Auto-warned sibling` timeline card for visibility.
            let socket_str = socket.to_string_lossy();
            for (pane, sibling) in [(&pair.pane_a, &pair.agent_b), (&pair.pane_b, &pair.agent_a)] {
                if !pane.is_empty() {
                    let agent_key = format!("{socket_str}::{pane}");
                    let card = TimelineCardInput {
                        id: format!("collision-warn-{sibling}"),
                        title: format!("⚡ Auto-warned sibling `{sibling}`"),
                        subtitle: Some(format!(
                            "{} · {}",
                            pair.severity.as_str(),
                            pair.files.join(", ")
                        )),
                        blocks: vec![json!({
                            "type": "callout",
                            "title": "Collision Radar Auto-Warn",
                            "text": format!("Queued collision alert about `{sibling}` without interrupting active input."),
                            "tone": "warning",
                        })],
                    };
                    let _ = crate::plugin::runner::upsert_timeline_card(
                        &agent_key,
                        "masil-collision",
                        &card,
                    );
                }
            }
        }
    }
}

/// Resolves a worktree path on disk by name or path for CLI/RPC `shadow-check`.
pub fn resolve_worktree_path(target: &str, repo_hint: Option<&Path>) -> Result<PathBuf, String> {
    let candidate = PathBuf::from(target);
    if candidate.is_dir() {
        return candidate.canonicalize().map_err(|e| e.to_string());
    }
    let mut args = Vec::new();
    if let Some(repo) = repo_hint {
        args.push(repo.to_string_lossy().into_owned());
    }
    if let Ok(listed) = crate::worktree::listing(&args)
        && let Some(arr) = listed.get("worktrees").and_then(Value::as_array)
    {
        for wt in arr {
            let name = wt["name"].as_str().unwrap_or("");
            let branch = wt["branch"].as_str().unwrap_or("");
            let path = wt["path"].as_str().unwrap_or("");
            if (name == target || branch == target || path == target) && !path.is_empty() {
                return PathBuf::from(path)
                    .canonicalize()
                    .map_err(|e| e.to_string());
            }
        }
    }
    Err(format!("target_absent: cannot resolve worktree `{target}`"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn run_git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn count_files(dir: &Path) -> usize {
        let mut total = 0;
        if let Ok(entries) = fs::read_dir(dir) {
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    total += count_files(&p);
                } else {
                    total += 1;
                }
            }
        }
        total
    }

    #[test]
    fn quarantined_merge_tree_detects_conflict_with_zero_repo_object_pollution() {
        let base =
            std::env::temp_dir().join(format!("masil-coll-{}", crate::managed::nonce().unwrap()));
        let main_repo = base.join("repo");
        let wt_b = base.join("wt-b");
        fs::create_dir_all(&main_repo).unwrap();
        run_git(&main_repo, &["init", "-q", "-b", "main"]);
        fs::write(
            main_repo.join("shared.rs"),
            "pub fn greet() -> &'static str { \"hi\" }\n",
        )
        .unwrap();
        run_git(&main_repo, &["add", "-A"]);
        run_git(&main_repo, &["commit", "-q", "-m", "base"]);
        run_git(
            &main_repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feat-b",
                wt_b.to_str().unwrap(),
            ],
        );

        // Conflicting uncommitted edits in main_repo and wt_b.
        fs::write(
            main_repo.join("shared.rs"),
            "pub fn greet() -> &'static str { \"alpha\" }\n",
        )
        .unwrap();
        fs::write(
            wt_b.join("shared.rs"),
            "pub fn greet() -> &'static str { \"beta\" }\n",
        )
        .unwrap();

        let obj_dir = main_repo.join(".git/objects");
        let objects_before = count_files(&obj_dir);

        let outcome =
            check_worktree_merge_conflict(&main_repo, &wt_b, Duration::from_secs(5)).unwrap();
        let objects_after = count_files(&obj_dir);
        assert_eq!(
            objects_before, objects_after,
            "quarantined merge-tree must write 0 objects to user repo"
        );
        match outcome {
            MergeTreeOutcome::HardConflict { conflict_files, .. } => {
                assert!(
                    conflict_files.iter().any(|f| f == "shared.rs"),
                    "expected shared.rs conflict, got {conflict_files:?}"
                );
            }
            other => panic!("expected HardConflict, got {other:?}"),
        }

        // Now test clean merge + Shadow Integration Check + guaranteed temp cleanup.
        fs::write(
            wt_b.join("shared.rs"),
            "pub fn greet() -> &'static str { \"hi\" }\n",
        )
        .unwrap();
        fs::write(wt_b.join("extra.txt"), "from-b\n").unwrap();
        let report = shadow::run_shadow_check(
            "agent-a",
            &main_repo,
            "agent-b",
            &wt_b,
            "test -f extra.txt && grep -q alpha shared.rs",
            Duration::from_secs(10),
        )
        .unwrap();
        assert!(report.merge_clean);
        assert_eq!(report.exit_code, Some(0));
        assert_eq!(report.ui.v, 1);

        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn semantic_scanner_catches_cross_language_exports_and_shared_resources() {
        let mut a = semantic::WorktreeSemanticProfile::default();
        let mut b = semantic::WorktreeSemanticProfile::default();
        let started = Instant::now();
        semantic::scan_source_text(
            "src/auth.ts",
            "export function verifyToken(t: string) {}",
            &mut a,
        );
        semantic::scan_source_text(
            "src/api.ts",
            "import { verifyToken } from './auth';",
            &mut b,
        );
        semantic::scan_source_text("src/lib.rs", "pub struct SessionLease {}", &mut a);
        semantic::scan_source_text("src/main.rs", "use crate::lib::{SessionLease};", &mut b);
        a.shared_resources.insert("Cargo.lock".into());
        b.shared_resources.insert("migrations/002.sql".into());
        let coll = semantic::compare_profiles("a", &a, "b", &b);
        assert!(started.elapsed().as_millis() < 15);
        assert!(coll.overlapping_symbols.contains(&"verifyToken".to_owned()));
        assert!(
            coll.overlapping_symbols
                .contains(&"SessionLease".to_owned())
        );
        assert_eq!(coll.shared_resources.len(), 2);
    }

    #[test]
    fn same_directory_race_detects_concurrent_agents_in_one_dir() {
        let dir = std::env::temp_dir().to_string_lossy().into_owned();
        let agents = vec![
            ActiveWorktreeAgent {
                name: "alpha".into(),
                pane_id: "%1".into(),
                run_id: "r1".into(),
                state: "working".into(),
                cwd: dir.clone(),
                repo_key: "rk".into(),
                repo_root: dir.clone(),
                changed_files: vec!["a.rs".into()],
            },
            ActiveWorktreeAgent {
                name: "beta".into(),
                pane_id: "%2".into(),
                run_id: "r2".into(),
                state: "idle".into(),
                cwd: dir.clone(),
                repo_key: "rk".into(),
                repo_root: dir,
                changed_files: vec![],
            },
        ];
        let pairs = same_dir::detect_same_directory_races(&agents, 1234);
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].severity, CollisionSeverity::SameDirectoryRace);
    }
}
