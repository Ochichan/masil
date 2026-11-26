//! Detects `same_directory_race` when two or more live agents operate inside
//! the exact same working directory (D-04).

use super::{ActiveWorktreeAgent, CollisionPair, CollisionSeverity};
use std::collections::HashMap;
use std::path::Path;

pub fn detect_same_directory_races(
    agents: &[ActiveWorktreeAgent],
    now_ms: u64,
) -> Vec<CollisionPair> {
    let mut by_dir: HashMap<String, Vec<&ActiveWorktreeAgent>> = HashMap::new();
    for agent in agents {
        if agent.state == "exited" || agent.cwd.trim().is_empty() {
            continue;
        }
        let canon = Path::new(&agent.cwd)
            .canonicalize()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| agent.cwd.trim_end_matches('/').to_owned());
        by_dir.entry(canon).or_default().push(agent);
    }

    let mut pairs = Vec::new();
    for (dir, group) in by_dir {
        if group.len() < 2 {
            continue;
        }
        // Only raise race if at least one agent is actively working/blocked or has dirty files.
        let any_active = group.iter().any(|a| {
            matches!(a.state.as_str(), "working" | "retrying" | "blocked")
                || !a.changed_files.is_empty()
        });
        if !any_active {
            continue;
        }
        for i in 0..group.len() {
            for j in (i + 1)..group.len() {
                let a = group[i];
                let b = group[j];
                let mut files = a.changed_files.clone();
                for f in &b.changed_files {
                    if !files.contains(f) {
                        files.push(f.clone());
                    }
                }
                files.sort();
                let (first, second) = if a.name <= b.name { (a, b) } else { (b, a) };
                pairs.push(CollisionPair {
                    repo_key: first.repo_key.clone(),
                    agent_a: first.name.clone(),
                    agent_b: second.name.clone(),
                    pane_a: first.pane_id.clone(),
                    pane_b: second.pane_id.clone(),
                    worktree_a: first.cwd.clone(),
                    worktree_b: second.cwd.clone(),
                    severity: CollisionSeverity::SameDirectoryRace,
                    files,
                    details: vec![format!(
                        "Both `{}` and `{}` share the same working directory `{dir}`; concurrent edits will race on disk. Move one agent into its own git worktree.",
                        first.name, second.name
                    )],
                    last_warned_ms: None,
                    computed_ms: now_ms,
                });
            }
        }
    }
    pairs
}
