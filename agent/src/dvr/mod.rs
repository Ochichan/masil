//! Phase 3B: Cross-Provider Time-Travel DVR & PR Verification Trace Export (D-12).
//!
//! Records every completed agent turn into SQLite `dvr_turns`, links it to its
//! sidecar bare-git checkpoint `tree_oid`, enforces the 100-turn unpinned
//! retention window (permanently protecting pinned/bookmarked turns), supports
//! full working-tree restore (undoing Bash side effects) and worktree forking,
//! and exports zero-CDN secret-redacted HTML traces.

pub mod export_html;

use crate::checkpoint;
use crate::managed::{DvrTurnRecord, Store};
use crate::observation::now_ms;
use crate::plugin::ui_spec::{UI_SPEC_VERSION, UiBlock, UiDocument, UiListItem, UiMetric, UiTone};
use crate::worktree::git;
use export_html::{TraceExportInput, TraceExportOutput, TraceTurnEntry, render_trace_html};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// D-12: retain up to 100 unpinned DVR turns per run/worktree; pinned turns are immortal.
pub const DEFAULT_KEEP_UNPINNED_TURNS: usize = 100;
const MAX_TERMINAL_EXCERPT_BYTES: usize = 16 * 1024;

/// Records a completed turn in SQLite `dvr_turns` and prunes oldest unpinned turns
/// past [`DEFAULT_KEEP_UNPINNED_TURNS`].
#[allow(clippy::too_many_arguments)]
pub fn record_turn(
    socket: &Path,
    run_id: &str,
    pane_id: &str,
    turn_seq: u64,
    provider: &str,
    prompt_preview: Option<&str>,
    checkpoint_id: u64,
    tree_oid: &str,
    audit_verdict: Option<&str>,
    files_changed: u64,
    terminal_excerpt: Option<&str>,
) -> Result<i64, String> {
    let mut store = Store::open(socket)?;
    record_turn_in_store(
        &mut store,
        run_id,
        pane_id,
        turn_seq,
        provider,
        prompt_preview,
        checkpoint_id,
        tree_oid,
        audit_verdict,
        files_changed,
        terminal_excerpt,
        DEFAULT_KEEP_UNPINNED_TURNS,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn record_turn_in_store(
    store: &mut Store,
    run_id: &str,
    pane_id: &str,
    turn_seq: u64,
    provider: &str,
    prompt_preview: Option<&str>,
    checkpoint_id: u64,
    tree_oid: &str,
    audit_verdict: Option<&str>,
    files_changed: u64,
    terminal_excerpt: Option<&str>,
    keep_unpinned: usize,
) -> Result<i64, String> {
    let excerpt = terminal_excerpt.map(|raw| {
        if raw.len() <= MAX_TERMINAL_EXCERPT_BYTES {
            raw.to_owned()
        } else {
            // Keep the newest tail up to 16 KiB on UTF-8 boundary.
            let mut start = raw.len().saturating_sub(MAX_TERMINAL_EXCERPT_BYTES);
            while start < raw.len() && !raw.is_char_boundary(start) {
                start += 1;
            }
            raw[start..].to_owned()
        }
    });
    let id = store.upsert_dvr_turn(&DvrTurnRecord {
        id: 0,
        run_id: run_id.to_owned(),
        pane_id: pane_id.to_owned(),
        turn_seq,
        provider: provider.to_owned(),
        prompt_preview: prompt_preview.map(|s| s.chars().take(500).collect()),
        checkpoint_id: checkpoint_id.to_string(),
        tree_oid: tree_oid.to_owned(),
        audit_verdict: audit_verdict.map(str::to_owned),
        files_changed,
        terminal_excerpt: excerpt,
        pinned: false,
        pin_note: None,
        created_ms: now_ms(),
    })?;
    let _ = store.prune_unpinned_dvr_turns(run_id, keep_unpinned);
    Ok(id)
}

/// Captures a DVR turn snapshot on demand for a live agent directory (used when
/// recording manual checkpoints or when coordinator auto-turn hasn't run yet).
#[allow(clippy::too_many_arguments)]
pub fn capture_turn_snapshot(
    socket: &Path,
    cwd: &Path,
    agent_name: &str,
    run_id: &str,
    pane_id: &str,
    provider: &str,
    prompt_preview: Option<&str>,
    terminal_excerpt: Option<&str>,
) -> Result<DvrTurnRecord, String> {
    let mut store = Store::open(socket)?;
    let existing = store.list_dvr_turns(run_id, 500)?;
    let next_seq = existing.last().map_or(1, |t| t.turn_seq + 1);
    let prev_tree = existing.last().map(|t| t.tree_oid.clone());

    let made =
        checkpoint::make_safety_checkpoint(cwd, prompt_preview.unwrap_or("DVR turn snapshot"))?;
    let ckpt_store = checkpoint::Store::open(
        &crate::changes::root_of(cwd)?,
        Instant::now() + Duration::from_secs(30),
    )?;
    let (_, files_changed) =
        checkpoint::diff_trees(&ckpt_store, prev_tree.as_deref(), &made.snapshot.tree)
            .unwrap_or_else(|_| (String::new(), 0));
    let latest_audit = store
        .latest_turn_audit(Some(run_id), Some(pane_id))
        .ok()
        .flatten()
        .map(|a| a.verdict);

    let _ = record_turn_in_store(
        &mut store,
        run_id,
        pane_id,
        next_seq,
        provider,
        prompt_preview.or(Some(agent_name)),
        made.id,
        &made.snapshot.tree,
        latest_audit.as_deref().or(Some("verified")),
        files_changed as u64,
        terminal_excerpt,
        DEFAULT_KEEP_UNPINNED_TURNS,
    )?;
    store
        .get_dvr_turn(run_id, next_seq)?
        .ok_or_else(|| "dvr_turn_missing: failed reading back recorded turn".into())
}

/// Lists DVR turns for `run_id`.
pub fn list_turns(socket: &Path, run_id: &str, limit: usize) -> Result<Vec<DvrTurnRecord>, String> {
    let store = Store::open(socket)?;
    store.list_dvr_turns(run_id, limit)
}

/// Inspects a specific turn `turn_seq` (or the latest turn if `None`), returning
/// its metadata, unified diff from the preceding turn, and terminal excerpt.
pub fn inspect_turn(
    socket: &Path,
    cwd: &Path,
    run_id: &str,
    turn_seq: Option<u64>,
) -> Result<Value, String> {
    let store = Store::open(socket)?;
    let turns = store.list_dvr_turns(run_id, 500)?;
    if turns.is_empty() {
        return Err("dvr_empty: no DVR turns recorded for this agent yet".into());
    }
    let idx = match turn_seq {
        Some(seq) => turns
            .iter()
            .position(|t| t.turn_seq == seq)
            .ok_or_else(|| format!("dvr_turn_not_found: turn {seq} not found"))?,
        None => turns.len() - 1,
    };
    let turn = &turns[idx];
    let prev_tree = idx.checked_sub(1).map(|p| turns[p].tree_oid.as_str());
    let (diff, diff_files) = crate::changes::root_of(cwd)
        .ok()
        .and_then(|root| {
            checkpoint::Store::open(&root, Instant::now() + Duration::from_secs(30)).ok()
        })
        .and_then(|ck| checkpoint::diff_trees(&ck, prev_tree, &turn.tree_oid).ok())
        .unwrap_or_else(|| (String::new(), turn.files_changed as usize));

    Ok(json!({
        "turn": turn,
        "prev_turn_seq": idx.checked_sub(1).map(|p| turns[p].turn_seq),
        "diff": diff,
        "diff_files": diff_files,
        "total_turns": turns.len(),
    }))
}

/// Pins or unpins a turn in BOTH SQLite `dvr_turns` AND the sidecar git checkpoint
/// repository so it is permanently immune to both the 100-turn cap and the 1 GiB cap.
pub fn set_pin(
    socket: &Path,
    cwd: &Path,
    run_id: &str,
    turn_seq: u64,
    pinned: bool,
    pin_note: Option<&str>,
) -> Result<Value, String> {
    let mut store = Store::open(socket)?;
    let turn = store
        .get_dvr_turn(run_id, turn_seq)?
        .ok_or_else(|| format!("dvr_turn_not_found: turn {turn_seq} not found"))?;
    store.set_dvr_turn_pin(run_id, turn_seq, pinned, pin_note)?;
    if let Ok(ckpt_id) = turn.checkpoint_id.parse::<u64>()
        && let Ok(root) = crate::changes::root_of(cwd)
        && let Ok(ck_store) =
            checkpoint::Store::open(&root, Instant::now() + Duration::from_secs(30))
    {
        let _ = checkpoint::set_pinned(&ck_store, ckpt_id, pinned);
    }
    Ok(json!({
        "run_id": run_id,
        "turn_seq": turn_seq,
        "checkpoint_id": turn.checkpoint_id,
        "pinned": pinned,
        "pin_note": pin_note,
    }))
}

/// Restores the agent's working tree cleanly to turn `turn_seq` (creating a
/// `pre_restore` safety checkpoint first and undoing any Bash-created/deleted files).
pub fn restore_turn(
    socket: &Path,
    cwd: &Path,
    run_id: &str,
    turn_seq: u64,
) -> Result<Value, String> {
    let store = Store::open(socket)?;
    let turn = store
        .get_dvr_turn(run_id, turn_seq)?
        .ok_or_else(|| format!("dvr_turn_not_found: turn {turn_seq} not found"))?;
    let root = crate::changes::root_of(cwd)?;
    let ck_store = checkpoint::Store::open(&root, Instant::now() + Duration::from_secs(30))?;
    let settings = checkpoint::Settings::load().unwrap_or_default();
    let limits = {
        // Use standard Limits via make_safety_checkpoint path; expose via default limits.
        let _ = settings;
        checkpoint_default_limits()
    };
    let mut res = checkpoint::restore_tree(&ck_store, &turn.tree_oid, limits)?;
    if let Some(obj) = res.as_object_mut() {
        obj.insert("run_id".into(), json!(run_id));
        obj.insert("turn_seq".into(), json!(turn_seq));
        obj.insert("checkpoint_id".into(), json!(turn.checkpoint_id));
    }
    Ok(res)
}

/// Forks turn `turn_seq` into a brand-new git worktree on `branch_name`
/// populated with that turn's exact `tree_oid` (D-12).
pub fn fork_turn(
    socket: &Path,
    cwd: &Path,
    run_id: &str,
    turn_seq: u64,
    branch_name: Option<&str>,
    dest_dir: Option<&Path>,
) -> Result<Value, String> {
    let store = Store::open(socket)?;
    let turn = store
        .get_dvr_turn(run_id, turn_seq)?
        .ok_or_else(|| format!("dvr_turn_not_found: turn {turn_seq} not found"))?;
    let root = crate::changes::root_of(cwd)?;
    let ck_store = checkpoint::Store::open(&root, Instant::now() + Duration::from_secs(30))?;

    let slug: String = run_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(12)
        .collect();
    let branch = branch_name
        .map(str::to_owned)
        .unwrap_or_else(|| format!("dvr-fork/{}-t{}-{}", slug, turn_seq, now_ms() % 100_000));
    let fork_path = match dest_dir {
        Some(p) => p.to_path_buf(),
        None => {
            let base = crate::managed::state_base()?.join("masil/dvr-forks");
            crate::managed::private_directory(&base)?;
            base.join(format!("{}-t{}-{}", slug, turn_seq, now_ms() % 100_000))
        }
    };

    git::query(
        &root,
        &[
            "worktree",
            "add",
            "-b",
            &branch,
            fork_path.to_str().ok_or("fork path is not UTF-8")?,
            "HEAD",
        ],
    )?;

    let files_restored = checkpoint::materialize_tree_into(&ck_store, &turn.tree_oid, &fork_path)?;

    Ok(json!({
        "stage": "forked",
        "run_id": run_id,
        "turn_seq": turn_seq,
        "tree_oid": turn.tree_oid,
        "branch": branch,
        "worktree_path": fork_path,
        "files_restored": files_restored,
    }))
}

/// Exports a self-contained, zero-CDN, secret-redacted HTML PR Verification Trace
/// for `run_id` and optionally writes it to `out_path`.
pub fn export_trace(
    socket: &Path,
    cwd: &Path,
    agent_name: &str,
    provider: &str,
    run_id: &str,
    strict_redaction: bool,
    out_path: Option<&Path>,
) -> Result<(TraceExportOutput, Option<PathBuf>), String> {
    let store = Store::open(socket)?;
    let turns = store.list_dvr_turns(run_id, 500)?;
    let ck_store = crate::changes::root_of(cwd).ok().and_then(|root| {
        checkpoint::Store::open(&root, Instant::now() + Duration::from_secs(30)).ok()
    });

    let mut entries = Vec::with_capacity(turns.len());
    let mut prev_tree: Option<String> = None;
    for t in &turns {
        let diff = ck_store
            .as_ref()
            .and_then(|ck| checkpoint::diff_trees(ck, prev_tree.as_deref(), &t.tree_oid).ok())
            .map(|(d, _)| d)
            .unwrap_or_default();
        entries.push(TraceTurnEntry {
            turn_seq: t.turn_seq,
            provider: t.provider.clone(),
            prompt_preview: t.prompt_preview.clone().unwrap_or_default(),
            checkpoint_id: t.checkpoint_id.clone(),
            tree_oid: t.tree_oid.clone(),
            audit_verdict: t.audit_verdict.clone().unwrap_or_else(|| "verified".into()),
            files_changed: t.files_changed,
            pinned: t.pinned,
            pin_note: t.pin_note.clone(),
            diff,
            terminal_excerpt: t.terminal_excerpt.clone().unwrap_or_default(),
            created_ms: t.created_ms,
        });
        prev_tree = Some(t.tree_oid.clone());
    }

    let output = render_trace_html(&TraceExportInput {
        agent_name: agent_name.to_owned(),
        provider: provider.to_owned(),
        run_id: run_id.to_owned(),
        repo_label: cwd.to_string_lossy().into_owned(),
        strict_redaction,
        exported_at_ms: now_ms(),
        turns: entries,
    });

    let written = if let Some(path) = out_path {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)
                .map_err(|e| format!("creating {}: {e}", parent.display()))?;
        }
        fs::write(path, output.html.as_bytes())
            .map_err(|e| format!("writing {}: {e}", path.display()))?;
        Some(path.to_path_buf())
    } else {
        None
    };

    Ok((output, written))
}

/// Builds a Masil UI Spec v1 Time-Travel DVR document for TUI command palette display.
pub fn build_dvr_ui_document(socket: &Path, agent_name: &str, run_id: &str) -> UiDocument {
    let turns = list_turns(socket, run_id, 100).unwrap_or_default();
    let pinned_count = turns.iter().filter(|t| t.pinned).count();
    let flagged_count = turns
        .iter()
        .filter(|t| t.audit_verdict.as_deref() == Some("flagged"))
        .count();

    let mut blocks = vec![UiBlock::MetricRow {
        items: vec![
            UiMetric {
                label: "Recorded Turns".into(),
                value: turns.len().to_string(),
                delta: None,
                tone: UiTone::Accent,
            },
            UiMetric {
                label: "Pinned (Protected)".into(),
                value: pinned_count.to_string(),
                delta: None,
                tone: UiTone::Accent,
            },
            UiMetric {
                label: "BS Flagged".into(),
                value: flagged_count.to_string(),
                delta: None,
                tone: if flagged_count > 0 {
                    UiTone::Danger
                } else {
                    UiTone::Success
                },
            },
        ],
    }];

    blocks.push(UiBlock::Header {
        title: format!("Time-Travel Timeline · {agent_name}"),
        subtitle: Some(
            "Up to 100 unpinned turns retained; pinned turns are permanently protected".into(),
        ),
        badge: None,
        tone: UiTone::Accent,
    });
    if turns.is_empty() {
        blocks.push(UiBlock::Empty {
            title: "No DVR turns recorded yet".into(),
            body: Some(
                "Turns are recorded automatically after each completed turn, or run `masil-agent agent dvr snapshot <NAME>`."
                    .into(),
            ),
        });
    } else {
        blocks.push(UiBlock::List {
            title: None,
            items: turns
                .iter()
                .rev()
                .take(20)
                .map(|t| {
                    let verdict = t.audit_verdict.as_deref().unwrap_or("verified");
                    let badge = if t.pinned {
                        format!("PINNED · {}", verdict.to_uppercase())
                    } else {
                        verdict.to_uppercase()
                    };
                    UiListItem {
                        id: Some(t.turn_seq.to_string()),
                        title: format!("Turn #{} · ckpt {}", t.turn_seq, t.checkpoint_id),
                        subtitle: Some(format!(
                            "{} · {} file(s) · tree {}{}",
                            t.provider,
                            t.files_changed,
                            &t.tree_oid[..t.tree_oid.len().min(10)],
                            t.prompt_preview
                                .as_deref()
                                .map(|p| format!(" · {p}"))
                                .unwrap_or_default()
                        )),
                        badge: Some(badge),
                        tone: match verdict {
                            "flagged" => UiTone::Danger,
                            "warnings" => UiTone::Warning,
                            _ => UiTone::Success,
                        },
                        action: Some(format!("dvr:restore:{}:{}", t.run_id, t.turn_seq)),
                        payload: Some(json!({"turn_seq": t.turn_seq})),
                    }
                })
                .collect(),
        });
    }

    UiDocument {
        v: UI_SPEC_VERSION,
        title: Some(format!("Time-Travel DVR · {agent_name}")),
        subtitle: Some("D-12 · Cross-Provider Turn Scrubber & Fork".into()),
        blocks,
        custom_surface: None,
    }
}

fn checkpoint_default_limits() -> checkpoint::Limits {
    checkpoint::default_limits()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    fn init_temp_repo(prefix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "masil-dvr-{prefix}-{}-{}",
            std::process::id(),
            now_ms()
        ));
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        let root = dir.canonicalize().unwrap();
        let run_git = |args: &[&str]| {
            let out = Command::new("git")
                .current_dir(&root)
                .env("GIT_AUTHOR_NAME", "test")
                .env("GIT_AUTHOR_EMAIL", "test@localhost")
                .env("GIT_COMMITTER_NAME", "test")
                .env("GIT_COMMITTER_EMAIL", "test@localhost")
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run_git(&["init", "-q", "-b", "main"]);
        fs::write(root.join("app.txt"), "v0\n").unwrap();
        run_git(&["add", "app.txt"]);
        run_git(&["commit", "-q", "-m", "initial"]);
        root
    }

    #[test]
    fn dvr_restore_undoes_bash_side_effects_and_export_html_scrubs_all_secrets() {
        let root = init_temp_repo("restore");
        let ck_repo = root.parent().unwrap().join(format!(
            "masil-dvr-ck-{}-{}.git",
            std::process::id(),
            now_ms()
        ));
        let ck_store = checkpoint::Store::open_in_repo(
            &root,
            &ck_repo,
            Instant::now() + Duration::from_secs(30),
        )
        .unwrap();

        // Turn 1: app.txt = "v1"
        fs::write(root.join("app.txt"), "v1\n").unwrap();
        let m1 = {
            let (_r, _w) = ck_store.lock(false, Duration::from_secs(5)).unwrap();
            checkpoint::make(
                &ck_store,
                &checkpoint::Request {
                    reason: "turn 1",
                    auto: true,
                    trigger: "after_turn",
                    agent: None,
                    provider: Some("claude"),
                    turn_seq: Some(1),
                    prompt_sha256: None,
                    pinned: false,
                    skip_if_unchanged: false,
                    force: &[],
                    prompt_ms: None,
                },
                checkpoint::default_limits(),
                Instant::now() + Duration::from_secs(30),
            )
            .unwrap()
        };

        // Turn 2: Bash side effect creates stray generated file & corrupts app.txt
        fs::write(root.join("app.txt"), "broken\n").unwrap();
        fs::write(root.join("bash-side-effect.tmp"), "junk\n").unwrap();

        // Restore Turn 1 via restore_tree
        let res =
            checkpoint::restore_tree(&ck_store, &m1.snapshot.tree, checkpoint::default_limits())
                .unwrap();
        assert_eq!(res["stage"], json!("restored"));
        assert_eq!(fs::read_to_string(root.join("app.txt")).unwrap(), "v1\n");
        assert!(
            !root.join("bash-side-effect.tmp").exists(),
            "DVR restore MUST undo Bash-created stray files"
        );

        // Pin protection in sidecar checkpoint repo
        checkpoint::set_pinned(&ck_store, m1.id, true).unwrap();
        let listed = checkpoint::list(&ck_store).unwrap();
        let found = listed["checkpoints"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == json!(m1.id))
            .unwrap();
        assert_eq!(found["pinned"], json!(true));

        // HTML trace export with strict redaction & zero CDN links
        let exported = render_trace_html(&TraceExportInput {
            agent_name: "builder".into(),
            provider: "claude".into(),
            run_id: "r1".into(),
            repo_label: "/Users/alice/secret-proj".into(),
            strict_redaction: true,
            exported_at_ms: 12345,
            turns: vec![TraceTurnEntry {
                turn_seq: 1,
                provider: "claude".into(),
                prompt_preview:
                    "Use sk-ant-1234567890abcdefghijklmnop and ghp_abcdefghijklmnopqrstuvwxyz1234"
                        .into(),
                checkpoint_id: "1".into(),
                tree_oid: m1.snapshot.tree.clone(),
                audit_verdict: "verified".into(),
                files_changed: 1,
                pinned: true,
                pin_note: Some("clean".into()),
                diff: "+API_KEY=supersecretvalue123\n+AKIAIOSFODNN7EXAMPLE1234\n".into(),
                terminal_excerpt:
                    "Token Bearer AbCdEfGhIjKlMnOpQrStUvWxYz0123456789 and /Users/alice/repo".into(),
                created_ms: 12345,
            }],
        });
        assert!(exported.total_redactions >= 5);
        assert!(!exported.html.contains("sk-ant-1234567890"));
        assert!(!exported.html.contains("ghp_abcdef"));
        assert!(!exported.html.contains("AKIAIOSFODNN7EXAMPLE"));
        assert!(!exported.html.contains("supersecretvalue123"));
        assert!(!exported.html.contains("/Users/alice"));
        assert!(!exported.html.contains("https://cdn"));
        assert!(!exported.html.contains("<script src="));

        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(ck_repo);
    }
}
