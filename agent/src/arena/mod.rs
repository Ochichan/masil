//! Phase 3B: Native Best-of-N "Agent Arena" (D-12).
//!
//! Spawns N isolated worktree racers from a common git base commit/tree, runs
//! optional `--verify-cmd` + passive BS Detector audit, renders a live
//! `UiDocument` scorecard, supports safe winner promotion (`branch` or `squash`
//! with automatic pre-apply safety checkpoint when the base worktree is dirty,
//! plus `archive` or `clean` loser handling), and synthesizes strengths across
//! racers via a judge agent.

pub mod scorecard;

use crate::audit::{TurnAuditInput, audit_turn, config::RepoAuditConfig};
use crate::checkpoint;
use crate::managed::{ArenaRacerRecord, ArenaRecord, Store};
use crate::observation::now_ms;
use crate::plugin::ui_spec::UiDocument;
use crate::remote::discovery::{discover_masil_servers, run_self_agent_json};
use crate::worktree::git;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

const ARENA_HELP: &str = "\
usage: masil-agent arena start <PROMPT> [--repo PATH] --racers a,b[,c] [--verify-cmd CMD] [--socket PATH] [--no-sandbox] [--no-launch]
       masil-agent arena list [--socket PATH] [--json]
       masil-agent arena status <ARENA_ID> [--socket PATH] [--json]
       masil-agent arena verify <ARENA_ID> [--socket PATH] [--json]
       masil-agent arena promote <ARENA_ID> --winner <RACER> [--mode branch|squash] [--losers archive|clean] [--socket PATH]
       masil-agent arena synthesize <ARENA_ID> --judge <PROVIDER> [--winner <RACER>] [--socket PATH] [--no-launch]";

#[derive(Debug, Clone)]
pub struct StartArenaRequest {
    pub prompt: String,
    pub repo_path: PathBuf,
    pub racers: Vec<String>,
    pub verify_cmd: Option<String>,
    pub no_sandbox: bool,
    pub launch_agents: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromoteMode {
    Branch,
    Squash,
}

impl PromoteMode {
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "branch" => Ok(Self::Branch),
            "squash" | "patch" => Ok(Self::Squash),
            other => Err(format!(
                "invalid_argument: --mode must be branch or squash (got `{other}`)"
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoserDisposition {
    Archive,
    Clean,
}

impl LoserDisposition {
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "archive" | "keep" => Ok(Self::Archive),
            "clean" | "remove" | "delete" => Ok(Self::Clean),
            other => Err(format!(
                "invalid_argument: --losers must be archive or clean (got `{other}`)"
            )),
        }
    }
}

/// Opens the operation store for `socket` or falls back to a standalone arena store
/// under `$XDG_STATE_HOME/masil/arenas.sqlite3` when no masil server is running.
fn open_arena_store(socket: Option<&Path>) -> Result<Store, String> {
    if let Some(sock) = socket {
        return Store::open(sock);
    }
    let base = crate::managed::state_base()?;
    let dir = crate::managed::private_directory(&base.join("masil"))?;
    Store::open_path(&dir.join("arenas.sqlite3"))
}

fn resolve_socket(explicit: Option<&str>) -> Option<PathBuf> {
    explicit
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("TMUX")
                .ok()
                .and_then(|v| v.rsplitn(3, ',').last().map(PathBuf::from))
        })
        .or_else(|| std::env::var_os("MASIL_AGENT_SOCKET").map(PathBuf::from))
        .or_else(|| {
            discover_masil_servers()
                .into_iter()
                .next()
                .map(|s| PathBuf::from(s.socket))
        })
}

fn arena_worktrees_root() -> Result<PathBuf, String> {
    let base = crate::managed::state_base()?.join("masil/arenas");
    crate::managed::private_directory(&base)
}

fn slugify(input: &str, max_len: usize) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
        if out.len() >= max_len {
            break;
        }
    }
    out.trim_matches('-').to_owned()
}

/// Starts a new Best-of-N Agent Arena, creating N isolated worktrees from `HEAD`
/// and optionally launching sandboxed managed agents in each worktree.
pub fn start_arena(socket: Option<&Path>, req: &StartArenaRequest) -> Result<Value, String> {
    if req.prompt.trim().is_empty() {
        return Err("invalid_argument: arena prompt cannot be empty".into());
    }
    if req.racers.len() < 2 || req.racers.len() > 6 {
        return Err("invalid_argument: arena requires between 2 and 6 racers".into());
    }
    let repo_root = crate::changes::root_of(&req.repo_path)?;
    let base_commit = git::query(&repo_root, &["rev-parse", "HEAD"])?
        .trim()
        .to_owned();
    let base_tree_oid = git::query(&repo_root, &["rev-parse", "HEAD^{tree}"])
        .ok()
        .map(|s| s.trim().to_owned());

    let ts = now_ms();
    let prompt_slug = {
        let s = slugify(&req.prompt, 16);
        if s.is_empty() { "race".into() } else { s }
    };
    let arena_id = format!("arena-{prompt_slug}-{}", ts % 1_000_000);
    let wt_base = arena_worktrees_root()?.join(&arena_id);
    fs::create_dir_all(&wt_base).map_err(|e| format!("creating {}: {e}", wt_base.display()))?;

    let mut store = open_arena_store(socket)?;
    let arena = ArenaRecord {
        id: arena_id.clone(),
        repo_key: repo_root.to_string_lossy().into_owned(),
        base_commit: base_commit.clone(),
        base_tree_oid,
        prompt: req.prompt.clone(),
        verify_cmd: req.verify_cmd.clone(),
        status: "running".into(),
        winner_racer: None,
        created_ms: ts,
        updated_ms: ts,
    };
    store.upsert_arena(&arena)?;

    let mut racers_out = Vec::new();
    let mut name_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let mut first_pane: Option<String> = None;

    for raw_spec in &req.racers {
        let (provider, model) = match raw_spec.split_once(':') {
            Some((p, m)) => (p.trim().to_owned(), Some(m.trim().to_owned())),
            None => (raw_spec.trim().to_owned(), None),
        };
        if provider.is_empty() {
            continue;
        }
        let count = name_counts.entry(provider.clone()).or_insert(0);
        *count += 1;
        let racer_name = if *count == 1 {
            provider.clone()
        } else {
            format!("{provider}-{}", *count)
        };
        let branch_name = format!("arena/{arena_id}/{racer_name}");
        let wt_path = wt_base.join(&racer_name);

        git::query(
            &repo_root,
            &[
                "worktree",
                "add",
                "-b",
                &branch_name,
                wt_path.to_str().ok_or("worktree path not UTF-8")?,
                &base_commit,
            ],
        )?;

        let mut pane_id = String::new();
        let mut run_id = format!("arena-run-{arena_id}-{racer_name}");
        let mut status = "ready".to_owned();

        if req.launch_agents
            && let Some(sock) = socket
        {
            let agent_name = format!("ar-{}-{}", slugify(&racer_name, 10), ts % 10_000);
            let mut start_args = vec![
                "start".into(),
                agent_name.clone(),
                provider.clone(),
                "--cwd".into(),
                wt_path.to_string_lossy().into_owned(),
            ];
            if req.no_sandbox {
                start_args.push("--no-sandbox".into());
            }
            if let Some(ref split_target) = first_pane {
                start_args.push("--split".into());
                start_args.push(split_target.clone());
            }
            if let Ok(started) = run_self_agent_json(Some(&sock.to_string_lossy()), &start_args) {
                if let Some(p) = started.get("pane_id").and_then(Value::as_str) {
                    pane_id = p.to_owned();
                    if first_pane.is_none() {
                        first_pane = Some(p.to_owned());
                    }
                }
                if let Some(r) = started.get("run").and_then(Value::as_str) {
                    run_id = r.to_owned();
                }
                status = "running".into();
                // Queue the arena prompt cleanly onto the launched agent.
                if !pane_id.is_empty() {
                    let _ = run_self_agent_json(
                        Some(&sock.to_string_lossy()),
                        &[
                            "queue".into(),
                            pane_id.clone(),
                            "add".into(),
                            req.prompt.clone(),
                        ],
                    );
                }
            }
        }

        let rec = ArenaRacerRecord {
            arena_id: arena_id.clone(),
            racer_name,
            provider,
            model,
            worktree_id: wt_path.to_string_lossy().into_owned(),
            pane_id,
            run_id,
            status,
            elapsed_ms: Some(0),
            verify_exit_code: None,
            audit_verdict: None,
            files_changed: Some(0),
            insertions: Some(0),
            deletions: Some(0),
        };
        store.upsert_arena_racer(&rec)?;
        racers_out.push(rec);
    }

    let doc = scorecard::build_scorecard_document(&arena, &racers_out);
    Ok(json!({
        "arena": arena,
        "racers": racers_out,
        "scorecard": doc,
    }))
}

/// Evaluates every racer in `arena_id`: runs `verify_cmd` (if configured),
/// runs passive BS Detector audit on the base→racer diff, and records diff stats.
pub fn verify_arena(socket: Option<&Path>, arena_id: &str) -> Result<Value, String> {
    let mut store = open_arena_store(socket)?;
    let mut arena = store
        .get_arena(arena_id)?
        .ok_or_else(|| format!("arena_not_found: `{arena_id}`"))?;
    let mut racers = store.list_arena_racers(arena_id)?;

    for racer in &mut racers {
        let wt = Path::new(&racer.worktree_id);
        if !wt.exists() {
            racer.status = "missing".into();
            store.upsert_arena_racer(racer)?;
            continue;
        }
        let started = Instant::now();
        let mut verify_output = String::new();
        let verify_code = if let Some(ref cmd) = arena.verify_cmd {
            match Command::new("/bin/sh")
                .arg("-c")
                .arg(cmd)
                .current_dir(wt)
                .output()
            {
                Ok(out) => {
                    verify_output = format!(
                        "{}\n{}",
                        String::from_utf8_lossy(&out.stdout),
                        String::from_utf8_lossy(&out.stderr)
                    );
                    out.status.code().or(Some(1))
                }
                Err(e) => {
                    verify_output = e.to_string();
                    Some(127)
                }
            }
        } else {
            None
        };

        let (diff_text, files_changed, insertions, deletions) =
            worktree_diff_vs_base(wt, &arena.base_commit);
        let repo_cfg = RepoAuditConfig::load_for_repo(wt);
        let audit_report = audit_turn(&TurnAuditInput {
            pane_id: &racer.pane_id,
            run_id: &racer.run_id,
            checkpoint_id: None,
            prev_checkpoint_id: None,
            unified_diff: &diff_text,
            transcript_slice: &[],
            pane_tail: &verify_output,
            last_user_prompt: Some(&arena.prompt),
            repo_cfg: &repo_cfg,
        });

        racer.verify_exit_code = verify_code.map(i64::from);
        racer.audit_verdict = Some(audit_report.verdict);
        racer.files_changed = Some(files_changed as i64);
        racer.insertions = Some(insertions as i64);
        racer.deletions = Some(deletions as i64);
        racer.elapsed_ms = Some(
            racer
                .elapsed_ms
                .unwrap_or(0)
                .max(started.elapsed().as_millis() as u64),
        );
        racer.status = "completed".into();
        store.upsert_arena_racer(racer)?;
    }

    if arena.status == "running" {
        arena.status = "completed".into();
        arena.updated_ms = now_ms();
        store.upsert_arena(&arena)?;
    }

    let doc = scorecard::build_scorecard_document(&arena, &racers);
    Ok(json!({
        "arena": arena,
        "racers": racers,
        "scorecard": doc,
    }))
}

/// Returns status and live `UiDocument` scorecard for `arena_id`.
pub fn status_arena(socket: Option<&Path>, arena_id: &str) -> Result<Value, String> {
    let store = open_arena_store(socket)?;
    let arena = store
        .get_arena(arena_id)?
        .ok_or_else(|| format!("arena_not_found: `{arena_id}`"))?;
    let racers = store.list_arena_racers(arena_id)?;
    let doc = scorecard::build_scorecard_document(&arena, &racers);
    Ok(json!({
        "arena": arena,
        "racers": racers,
        "scorecard": doc,
    }))
}

pub fn list_arenas(socket: Option<&Path>, limit: usize) -> Result<Value, String> {
    let store = open_arena_store(socket)?;
    let arenas = store.list_arenas(limit)?;
    let mut rows = Vec::with_capacity(arenas.len());
    for a in arenas {
        let racers = store.list_arena_racers(&a.id).unwrap_or_default();
        rows.push(json!({
            "arena": a,
            "racers": racers,
        }));
    }
    Ok(json!({ "arenas": rows }))
}

/// Builds a TUI palette `UiDocument` for the most recent arena (or empty state).
pub fn build_latest_arena_ui_document(socket: Option<&Path>) -> UiDocument {
    if let Ok(store) = open_arena_store(socket)
        && let Ok(arenas) = store.list_arenas(1)
        && let Some(latest) = arenas.first()
    {
        let racers = store.list_arena_racers(&latest.id).unwrap_or_default();
        return scorecard::build_scorecard_document(latest, &racers);
    }
    UiDocument {
        v: crate::plugin::ui_spec::UI_SPEC_VERSION,
        title: Some("Agent Arena (Best-of-N)".into()),
        subtitle: Some("D-12 · Multi-Provider Parallel Race & Safe Promotion".into()),
        blocks: vec![crate::plugin::ui_spec::UiBlock::Empty {
            title: "No Agent Arena runs yet".into(),
            body: Some(
                "Start one with `masil-agent arena start \"<prompt>\" --racers claude,codex --verify-cmd \"cargo test\"`."
                    .into(),
            ),
        }],
        custom_surface: None,
    }
}

/// Promotes the chosen `winner` racer according to D-12 safety rules:
/// - `PromoteMode::Branch`: commits winner worktree changes & points `arena/<id>-winner` branch at it.
/// - `PromoteMode::Squash`: if base worktree is dirty, takes a sidecar safety checkpoint FIRST,
///   then applies winner's full diff (including new untracked files) cleanly onto current branch!
/// - `LoserDisposition::Archive`: closes live panes while keeping loser worktrees & branches.
/// - `LoserDisposition::Clean`: closes live panes and removes loser worktrees & branches.
pub fn promote_arena(
    socket: Option<&Path>,
    arena_id: &str,
    winner_name: &str,
    mode: PromoteMode,
    losers: LoserDisposition,
) -> Result<Value, String> {
    let mut store = open_arena_store(socket)?;
    let mut arena = store
        .get_arena(arena_id)?
        .ok_or_else(|| format!("arena_not_found: `{arena_id}`"))?;
    let mut racers = store.list_arena_racers(arena_id)?;
    let winner = racers
        .iter()
        .find(|r| r.racer_name == winner_name)
        .cloned()
        .ok_or_else(|| format!("arena_racer_not_found: `{winner_name}` in `{arena_id}`"))?;

    let repo_root = PathBuf::from(&arena.repo_key);
    let winner_wt = PathBuf::from(&winner.worktree_id);
    if !winner_wt.exists() {
        return Err(format!(
            "arena_worktree_missing: winner worktree `{}` no longer exists",
            winner_wt.display()
        ));
    }

    let mut safety_checkpoint_id: Option<u64> = None;
    let mut promoted_branch: Option<String> = None;
    let applied_files: usize;

    match mode {
        PromoteMode::Branch => {
            // Snapshot any uncommitted winner changes into a commit on the winner branch.
            let dirty = git::query(&winner_wt, &["status", "--porcelain"])?;
            if !dirty.trim().is_empty() {
                run_git_write(&winner_wt, &["add", "-A"])?;
                run_git_write(
                    &winner_wt,
                    &[
                        "commit",
                        "-q",
                        "-m",
                        &format!("arena({arena_id}): winner {}", winner.racer_name),
                    ],
                )?;
            }
            let winner_head = git::query(&winner_wt, &["rev-parse", "HEAD"])?
                .trim()
                .to_owned();
            let branch = format!("arena/{arena_id}-winner");
            run_git_write(&repo_root, &["branch", "-f", &branch, &winner_head])?;
            promoted_branch = Some(branch);
            let (_, files, _, _) = worktree_diff_vs_base(&winner_wt, &arena.base_commit);
            applied_files = files;
        }
        PromoteMode::Squash => {
            // D-12: if the target main worktree is dirty, take a safety checkpoint FIRST!
            let main_dirty = git::query(&repo_root, &["status", "--porcelain"])?;
            if !main_dirty.trim().is_empty() {
                let saved = checkpoint::make_safety_checkpoint(
                    &repo_root,
                    &format!("pre arena squash promote ({arena_id} -> {winner_name})"),
                )?;
                safety_checkpoint_id = Some(saved.id);
            }

            // Stage untracked files in winner's worktree temporarily (using intent-to-add
            // or a temporary index) so we get a complete binary-safe patch from base_commit.
            let patch = build_complete_patch(&winner_wt, &arena.base_commit)?;
            if !patch.is_empty() {
                let mut cmd = git::command(&repo_root);
                cmd.args(["apply", "--3way", "--whitespace=nowarn"]);
                cmd.stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped());
                let mut child = cmd
                    .spawn()
                    .map_err(|e| format!("spawning git apply: {e}"))?;
                if let Some(mut stdin) = child.stdin.take() {
                    use std::io::Write;
                    stdin
                        .write_all(&patch)
                        .map_err(|e| format!("writing patch to git apply: {e}"))?;
                }
                let out = child
                    .wait_with_output()
                    .map_err(|e| format!("waiting for git apply: {e}"))?;
                if !out.status.success() {
                    return Err(format!(
                        "arena_apply_failed: {}",
                        String::from_utf8_lossy(&out.stderr).trim()
                    ));
                }
            }
            let (_, files, _, _) = worktree_diff_vs_base(&winner_wt, &arena.base_commit);
            applied_files = files;
        }
    }

    // Handle losers (and close any live racer panes).
    let mut cleaned_losers = Vec::new();
    let mut archived_losers = Vec::new();

    for racer in &mut racers {
        if let Some(sock) = socket
            && !racer.pane_id.is_empty()
        {
            let _ = run_self_agent_json(
                Some(&sock.to_string_lossy()),
                &["close".into(), racer.pane_id.clone()],
            );
        }
        if racer.racer_name == winner_name {
            racer.status = "promoted".into();
            store.upsert_arena_racer(racer)?;
            continue;
        }
        match losers {
            LoserDisposition::Archive => {
                racer.status = "archived".into();
                archived_losers.push(racer.worktree_id.clone());
            }
            LoserDisposition::Clean => {
                let wt_path = Path::new(&racer.worktree_id);
                if wt_path.exists() {
                    let _ = run_git_write(
                        &repo_root,
                        &["worktree", "remove", "--force", &racer.worktree_id],
                    );
                }
                let branch_name = format!("arena/{arena_id}/{}", racer.racer_name);
                let _ = run_git_write(&repo_root, &["branch", "-D", &branch_name]);
                racer.status = "cleaned".into();
                cleaned_losers.push(racer.worktree_id.clone());
            }
        }
        store.upsert_arena_racer(racer)?;
    }

    arena.status = "promoted".into();
    arena.winner_racer = Some(winner_name.to_owned());
    arena.updated_ms = now_ms();
    store.upsert_arena(&arena)?;

    Ok(json!({
        "stage": "promoted",
        "arena_id": arena_id,
        "winner": winner_name,
        "mode": match mode { PromoteMode::Branch => "branch", PromoteMode::Squash => "squash" },
        "losers_disposition": match losers { LoserDisposition::Archive => "archive", LoserDisposition::Clean => "clean" },
        "promoted_branch": promoted_branch,
        "safety_checkpoint_id": safety_checkpoint_id,
        "applied_files": applied_files,
        "archived_losers": archived_losers,
        "cleaned_losers": cleaned_losers,
    }))
}

/// Prepares (and optionally launches) a synthesis judge agent that merges the
/// strongest elements (e.g. extra tests or edge-case guards) from losers onto the winner.
pub fn synthesize_arena(
    socket: Option<&Path>,
    arena_id: &str,
    judge_provider: &str,
    preferred_winner: Option<&str>,
    launch_agent: bool,
) -> Result<Value, String> {
    let store = open_arena_store(socket)?;
    let arena = store
        .get_arena(arena_id)?
        .ok_or_else(|| format!("arena_not_found: `{arena_id}`"))?;
    let racers = store.list_arena_racers(arena_id)?;
    if racers.len() < 2 {
        return Err("arena_synthesize_requires_two_racers".into());
    }
    let ranked = scorecard::rank_racers(&racers);
    let winner = preferred_winner
        .and_then(|w| racers.iter().find(|r| r.racer_name == w))
        .or_else(|| {
            arena
                .winner_racer
                .as_deref()
                .and_then(|w| racers.iter().find(|r| r.racer_name == w))
        })
        .or_else(|| ranked.first().copied())
        .ok_or("arena has no racers")?;

    let (winner_diff, _, _, _) =
        worktree_diff_vs_base(Path::new(&winner.worktree_id), &arena.base_commit);
    let mut prompt = format!(
        "[masil Agent Arena Synthesis]\nOriginal task: {}\nPrimary baseline winner: {} ({})\n\n--- WINNER DIFF ---\n{}\n",
        arena.prompt,
        winner.racer_name,
        winner.provider,
        winner_diff.chars().take(12_000).collect::<String>()
    );
    for loser in racers.iter().filter(|r| r.racer_name != winner.racer_name) {
        let (ldiff, _, _, _) =
            worktree_diff_vs_base(Path::new(&loser.worktree_id), &arena.base_commit);
        prompt.push_str(&format!(
            "\n--- ALTERNATIVE RACER: {} ({}) ---\n{}\n",
            loser.racer_name,
            loser.provider,
            ldiff.chars().take(8_000).collect::<String>()
        ));
    }
    prompt.push_str(
        "\nInstructions: Keep the winner's core architecture, and cherry-pick ONLY clearly superior test coverage, error handling, or edge-case fixes from the alternative racers. Run verification before finishing.\n",
    );

    let mut launched = Value::Null;
    if launch_agent && let Some(sock) = socket {
        let judge_name = format!("judge-{}", now_ms() % 10_000);
        if let Ok(started) = run_self_agent_json(
            Some(&sock.to_string_lossy()),
            &[
                "start".into(),
                judge_name,
                judge_provider.to_owned(),
                "--cwd".into(),
                winner.worktree_id.clone(),
            ],
        ) {
            if let Some(pane) = started.get("pane_id").and_then(Value::as_str) {
                let _ = run_self_agent_json(
                    Some(&sock.to_string_lossy()),
                    &[
                        "queue".into(),
                        pane.to_owned(),
                        "add".into(),
                        prompt.clone(),
                    ],
                );
            }
            launched = started;
        }
    }

    Ok(json!({
        "stage": "synthesis_prepared",
        "arena_id": arena_id,
        "judge_provider": judge_provider,
        "base_winner": winner.racer_name,
        "worktree": winner.worktree_id,
        "prompt": prompt,
        "launched": launched,
    }))
}

fn worktree_diff_vs_base(wt: &Path, base_commit: &str) -> (String, usize, usize, usize) {
    let patch_bytes = build_complete_patch(wt, base_commit).unwrap_or_default();
    let diff_text = String::from_utf8_lossy(&patch_bytes).into_owned();
    let mut files = 0usize;
    let mut insertions = 0usize;
    let mut deletions = 0usize;
    for line in diff_text.lines() {
        if line.starts_with("diff --git ") {
            files += 1;
        } else if line.starts_with('+') && !line.starts_with("+++") {
            insertions += 1;
        } else if line.starts_with('-') && !line.starts_with("---") {
            deletions += 1;
        }
    }
    (diff_text, files, insertions, deletions)
}

/// Produces a complete unified diff from `base_commit` to `wt`'s current working
/// tree (including untracked files) using a temporary `GIT_INDEX_FILE` so the
/// racer's real index is never mutated!
fn build_complete_patch(wt: &Path, base_commit: &str) -> Result<Vec<u8>, String> {
    let tmp_idx = std::env::temp_dir().join(format!(
        "masil-arena-idx-{}-{}.idx",
        std::process::id(),
        now_ms()
    ));
    let _ = fs::remove_file(&tmp_idx);
    // Seed temp index from HEAD/base_commit, then `git add -A` into temp index only.
    let read_out = git::command(wt)
        .env("GIT_INDEX_FILE", &tmp_idx)
        .args(["read-tree", base_commit])
        .output()
        .map_err(|e| format!("git read-tree failed: {e}"))?;
    if !read_out.status.success() {
        let _ = fs::remove_file(&tmp_idx);
        return Err(String::from_utf8_lossy(&read_out.stderr).trim().to_owned());
    }
    let add_out = git::command(wt)
        .env("GIT_INDEX_FILE", &tmp_idx)
        .args(["add", "-A"])
        .output()
        .map_err(|e| format!("git add -A failed: {e}"))?;
    if !add_out.status.success() {
        let _ = fs::remove_file(&tmp_idx);
        return Err(String::from_utf8_lossy(&add_out.stderr).trim().to_owned());
    }
    let diff_out = git::command(wt)
        .env("GIT_INDEX_FILE", &tmp_idx)
        .args([
            "diff",
            "--cached",
            "--binary",
            "--no-color",
            "--no-ext-diff",
            base_commit,
        ])
        .output()
        .map_err(|e| format!("git diff --cached failed: {e}"))?;
    let _ = fs::remove_file(&tmp_idx);
    if !diff_out.status.success() {
        return Err(String::from_utf8_lossy(&diff_out.stderr).trim().to_owned());
    }
    Ok(diff_out.stdout)
}

fn run_git_write(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = git::command(dir)
        .env("GIT_AUTHOR_NAME", "masil")
        .env("GIT_AUTHOR_EMAIL", "masil@localhost")
        .env("GIT_COMMITTER_NAME", "masil")
        .env("GIT_COMMITTER_EMAIL", "masil@localhost")
        .args(args)
        .output()
        .map_err(|e| format!("git {:?} failed: {e}", args))?;
    if !out.status.success() {
        return Err(format!(
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Top-level CLI entrypoint: `masil-agent arena ...`.
pub fn run(args: &[String]) -> Result<i32, String> {
    let Some(sub) = args.first().map(String::as_str) else {
        println!("{ARENA_HELP}");
        return Ok(0);
    };
    if matches!(sub, "--help" | "-h" | "help") {
        println!("{ARENA_HELP}");
        return Ok(0);
    }
    let rest = &args[1..];
    match sub {
        "start" => {
            let mut prompt: Option<String> = None;
            let mut repo: Option<PathBuf> = None;
            let mut racers: Vec<String> = Vec::new();
            let mut verify_cmd: Option<String> = None;
            let mut socket: Option<&str> = None;
            let mut no_sandbox = false;
            let mut no_launch = false;
            let mut i = 0;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--repo" => {
                        repo = Some(PathBuf::from(
                            rest.get(i + 1).ok_or("usage: --repo requires a path")?,
                        ));
                        i += 2;
                    }
                    "--racers" => {
                        let raw = rest.get(i + 1).ok_or("usage: --racers requires a list")?;
                        racers = raw
                            .split(',')
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(str::to_owned)
                            .collect();
                        i += 2;
                    }
                    "--verify-cmd" => {
                        verify_cmd = Some(
                            rest.get(i + 1)
                                .ok_or("usage: --verify-cmd requires a command")?
                                .clone(),
                        );
                        i += 2;
                    }
                    "--socket" => {
                        socket = Some(
                            rest.get(i + 1)
                                .ok_or("usage: --socket requires a path")?
                                .as_str(),
                        );
                        i += 2;
                    }
                    "--no-sandbox" => {
                        no_sandbox = true;
                        i += 1;
                    }
                    "--no-launch" => {
                        no_launch = true;
                        i += 1;
                    }
                    other if other.starts_with('-') => {
                        return Err(format!("usage: unknown arena start option {other}"));
                    }
                    val if prompt.is_none() => {
                        prompt = Some(val.to_owned());
                        i += 1;
                    }
                    other => return Err(format!("usage: unexpected arena argument {other}")),
                }
            }
            let prompt = prompt.ok_or("usage: arena start <PROMPT> --racers a,b")?;
            let repo_path = match repo {
                Some(p) => p,
                None => std::env::current_dir().map_err(|e| e.to_string())?,
            };
            let sock = resolve_socket(socket);
            let res = start_arena(
                sock.as_deref(),
                &StartArenaRequest {
                    prompt,
                    repo_path,
                    racers,
                    verify_cmd,
                    no_sandbox,
                    launch_agents: !no_launch,
                },
            )?;
            println!(
                "{}",
                serde_json::to_string_pretty(&res).map_err(|e| e.to_string())?
            );
            Ok(0)
        }
        "list" => {
            let socket = rest
                .windows(2)
                .find(|w| w[0] == "--socket")
                .map(|w| w[1].as_str());
            let sock = resolve_socket(socket);
            let res = list_arenas(sock.as_deref(), 30)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&res).map_err(|e| e.to_string())?
            );
            Ok(0)
        }
        "status" => {
            let arena_id = rest.first().ok_or("usage: arena status <ARENA_ID>")?;
            let socket = rest
                .windows(2)
                .find(|w| w[0] == "--socket")
                .map(|w| w[1].as_str());
            let sock = resolve_socket(socket);
            let res = status_arena(sock.as_deref(), arena_id)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&res).map_err(|e| e.to_string())?
            );
            Ok(0)
        }
        "verify" => {
            let arena_id = rest.first().ok_or("usage: arena verify <ARENA_ID>")?;
            let socket = rest
                .windows(2)
                .find(|w| w[0] == "--socket")
                .map(|w| w[1].as_str());
            let sock = resolve_socket(socket);
            let res = verify_arena(sock.as_deref(), arena_id)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&res).map_err(|e| e.to_string())?
            );
            Ok(0)
        }
        "promote" => {
            let mut arena_id: Option<String> = None;
            let mut winner: Option<String> = None;
            let mut mode = PromoteMode::Branch;
            let mut losers = LoserDisposition::Archive;
            let mut socket: Option<&str> = None;
            let mut i = 0;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--winner" => {
                        winner = Some(
                            rest.get(i + 1)
                                .ok_or("usage: --winner requires a racer name")?
                                .clone(),
                        );
                        i += 2;
                    }
                    "--mode" => {
                        mode = PromoteMode::parse(
                            rest.get(i + 1)
                                .ok_or("usage: --mode requires branch|squash")?,
                        )?;
                        i += 2;
                    }
                    "--losers" => {
                        losers = LoserDisposition::parse(
                            rest.get(i + 1)
                                .ok_or("usage: --losers requires archive|clean")?,
                        )?;
                        i += 2;
                    }
                    "--socket" => {
                        socket = Some(
                            rest.get(i + 1)
                                .ok_or("usage: --socket requires a path")?
                                .as_str(),
                        );
                        i += 2;
                    }
                    other if other.starts_with('-') => {
                        return Err(format!("usage: unknown arena promote option {other}"));
                    }
                    val if arena_id.is_none() => {
                        arena_id = Some(val.to_owned());
                        i += 1;
                    }
                    other => return Err(format!("usage: unexpected argument {other}")),
                }
            }
            let arena_id = arena_id.ok_or("usage: arena promote <ARENA_ID> --winner <RACER>")?;
            let winner = winner.ok_or("usage: arena promote requires --winner <RACER>")?;
            let sock = resolve_socket(socket);
            let res = promote_arena(sock.as_deref(), &arena_id, &winner, mode, losers)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&res).map_err(|e| e.to_string())?
            );
            Ok(0)
        }
        "synthesize" => {
            let mut arena_id: Option<String> = None;
            let mut judge: Option<String> = None;
            let mut winner: Option<String> = None;
            let mut socket: Option<&str> = None;
            let mut no_launch = false;
            let mut i = 0;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--judge" => {
                        judge = Some(
                            rest.get(i + 1)
                                .ok_or("usage: --judge requires a provider")?
                                .clone(),
                        );
                        i += 2;
                    }
                    "--winner" => {
                        winner = Some(
                            rest.get(i + 1)
                                .ok_or("usage: --winner requires a racer name")?
                                .clone(),
                        );
                        i += 2;
                    }
                    "--socket" => {
                        socket = Some(
                            rest.get(i + 1)
                                .ok_or("usage: --socket requires a path")?
                                .as_str(),
                        );
                        i += 2;
                    }
                    "--no-launch" => {
                        no_launch = true;
                        i += 1;
                    }
                    other if other.starts_with('-') => {
                        return Err(format!("usage: unknown arena synthesize option {other}"));
                    }
                    val if arena_id.is_none() => {
                        arena_id = Some(val.to_owned());
                        i += 1;
                    }
                    other => return Err(format!("usage: unexpected argument {other}")),
                }
            }
            let arena_id =
                arena_id.ok_or("usage: arena synthesize <ARENA_ID> --judge <PROVIDER>")?;
            let judge = judge.ok_or("usage: arena synthesize requires --judge <PROVIDER>")?;
            let sock = resolve_socket(socket);
            let res = synthesize_arena(
                sock.as_deref(),
                &arena_id,
                &judge,
                winner.as_deref(),
                !no_launch,
            )?;
            println!(
                "{}",
                serde_json::to_string_pretty(&res).map_err(|e| e.to_string())?
            );
            Ok(0)
        }
        other => Err(format!(
            "usage: unknown arena command `{other}`\n{ARENA_HELP}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn init_repo(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "masil-arena-{name}-{}-{}",
            std::process::id(),
            now_ms()
        ));
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        let root = dir.canonicalize().unwrap();
        run_git_write(&root, &["init", "-q", "-b", "main"]).unwrap();
        fs::write(root.join("lib.txt"), "base\n").unwrap();
        run_git_write(&root, &["add", "lib.txt"]).unwrap();
        run_git_write(&root, &["commit", "-q", "-m", "base"]).unwrap();
        root
    }

    #[test]
    fn arena_race_verify_and_safe_squash_promote_with_dirty_base_checkpoint() {
        let root = init_repo("e2e");
        let started = start_arena(
            None,
            &StartArenaRequest {
                prompt: "Add feature hello".into(),
                repo_path: root.clone(),
                racers: vec!["claude".into(), "codex".into()],
                verify_cmd: Some("grep -q winner_ok feature.txt".into()),
                no_sandbox: true,
                launch_agents: false,
            },
        )
        .unwrap();
        let arena_id = started["arena"]["id"].as_str().unwrap().to_owned();
        let racers = started["racers"].as_array().unwrap();
        assert_eq!(racers.len(), 2);

        let wt_claude = PathBuf::from(racers[0]["worktree_id"].as_str().unwrap());
        let wt_codex = PathBuf::from(racers[1]["worktree_id"].as_str().unwrap());

        // Claude implements passing feature; Codex writes failing output.
        fs::write(wt_claude.join("feature.txt"), "winner_ok\n").unwrap();
        fs::write(wt_codex.join("feature.txt"), "broken_attempt\n").unwrap();

        let verified = verify_arena(None, &arena_id).unwrap();
        let v_racers = verified["racers"].as_array().unwrap();
        let claude_row = v_racers
            .iter()
            .find(|r| r["racer_name"] == "claude")
            .unwrap();
        let codex_row = v_racers
            .iter()
            .find(|r| r["racer_name"] == "codex")
            .unwrap();
        assert_eq!(claude_row["verify_exit_code"], json!(0));
        assert_eq!(codex_row["verify_exit_code"], json!(1));
        assert_eq!(verified["scorecard"]["v"], json!(1));

        // Make base worktree dirty before squash promotion -> must take safety checkpoint!
        fs::write(root.join("wip-notes.txt"), "unsaved user note\n").unwrap();
        let promoted = promote_arena(
            None,
            &arena_id,
            "claude",
            PromoteMode::Squash,
            LoserDisposition::Clean,
        )
        .unwrap();
        assert_eq!(promoted["stage"], json!("promoted"));
        assert!(
            promoted["safety_checkpoint_id"].as_u64().is_some(),
            "dirty base worktree MUST take a safety checkpoint before squash apply"
        );
        assert_eq!(
            fs::read_to_string(root.join("feature.txt")).unwrap(),
            "winner_ok\n"
        );
        assert!(
            !wt_codex.exists(),
            "LoserDisposition::Clean must remove loser worktree"
        );

        let _ = run_git_write(
            &root,
            &[
                "worktree",
                "remove",
                "--force",
                &wt_claude.to_string_lossy(),
            ],
        );
        let _ = fs::remove_dir_all(root);
    }
}
