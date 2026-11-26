//! Phase 1C: Runaway Circuit Breaker (`agent/src/guard/breaker.rs`, D-05).
//! Detects working-tree oscillation (`A -> B -> A`), repeated identical errors,
//! and no-progress loops. Auto-pauses unfocused/AFK panes while showing only a
//! warning banner on actively focused panes (`pane_active == 1` AND recent
//! `client_activity` within `active_focus_grace_secs`).

use super::config::CircuitBreakerConfig;
use crate::managed::{CircuitBreakerRecord, Store};
use crate::native_ui::native_executable;
use crate::observation::now_ms;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::process::Command;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeHistoryEntry {
    pub turn_seq: u64,
    pub tree_oid: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint_id: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BreakerEvaluation {
    pub record: CircuitBreakerRecord,
    pub newly_tripped: bool,
    pub should_auto_interrupt: bool,
    pub pane_actively_focused: bool,
    pub pre_oscillation_checkpoint_id: Option<u64>,
    pub error_excerpt: Option<String>,
}

pub fn record_to_json(
    rec: &CircuitBreakerRecord,
    pre_osc_cp: Option<u64>,
    focused: bool,
    error_excerpt: Option<&str>,
) -> Value {
    let trees: Vec<TreeHistoryEntry> =
        serde_json::from_str(&rec.recent_trees_json).unwrap_or_default();
    let rollback_cp = pre_osc_cp.or_else(|| find_pre_oscillation_checkpoint(&trees));
    json!({
        "run_id": rec.run_id,
        "pane_id": rec.pane_id,
        "tripped": rec.tripped_reason.is_some(),
        "tripped_reason": rec.tripped_reason,
        "tripped_at_ms": rec.tripped_at_ms,
        "same_error_streak": rec.same_error_streak,
        "no_progress_streak": rec.no_progress_streak,
        "bypass_until_turn": rec.bypass_until_turn,
        "recent_trees": trees,
        "pre_oscillation_checkpoint_id": rollback_cp,
        "pane_actively_focused": focused,
        "error_excerpt": error_excerpt,
        "updated_ms": rec.updated_ms,
    })
}

const ERROR_SIGNALS: &[&str] = &[
    "error[e",
    "error:",
    "failed",
    "assertionerror",
    "typeerror",
    "referenceerror",
    "syntaxerror",
    "panic",
    "traceback",
    "cannot find",
    "undefined reference",
    "exited with code 1",
    "exited with code 2",
    "exited with code 101",
    "segmentation fault",
];

/// Extracts and normalizes recurring error lines into a stable 16-hex digest +
/// short human-readable excerpt. Returns `None` when the output shows no error.
pub fn extract_error_fingerprint(text: &str) -> Option<(String, String)> {
    let mut matched_lines: Vec<String> = Vec::new();
    let mut raw_excerpt: Option<String> = None;

    for line in text.lines().rev() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.len() < 5 {
            continue;
        }
        let lower = trimmed.to_ascii_lowercase();
        if lower.contains("0 failed") || lower.contains("0 errors") {
            continue;
        }
        if ERROR_SIGNALS.iter().any(|sig| lower.contains(sig)) {
            if raw_excerpt.is_none() {
                raw_excerpt = Some(truncate_chars(trimmed, 180));
            }
            matched_lines.push(normalize_error_line(&lower));
            if matched_lines.len() >= 4 {
                break;
            }
        }
    }

    if matched_lines.is_empty() {
        return None;
    }
    matched_lines.reverse();
    let canonical = matched_lines.join("\n");
    let digest = Sha256::digest(canonical.as_bytes());
    let hex: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    Some((hex, raw_excerpt.unwrap_or_default()))
}

fn normalize_error_line(lower: &str) -> String {
    let mut out = String::with_capacity(lower.len());
    let bytes = lower.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Normalize hex tokens (0x...)
        if i + 2 < bytes.len() && bytes[i] == b'0' && bytes[i + 1] == b'x' {
            out.push_str("<hex>");
            i += 2;
            while i < bytes.len() && bytes[i].is_ascii_hexdigit() {
                i += 1;
            }
            continue;
        }
        // Normalize digit sequences (line numbers, PIDs, timestamps)
        if bytes[i].is_ascii_digit() {
            out.push('#');
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// Evaluates one completed turn against the circuit breaker rules (pure logic).
#[allow(clippy::too_many_arguments)]
pub fn evaluate_turn_state(
    prev: Option<CircuitBreakerRecord>,
    run_id: &str,
    pane_id: &str,
    turn_seq: u64,
    tree_oid: Option<&str>,
    checkpoint_id: Option<u64>,
    output_sample: &str,
    pane_actively_focused: bool,
    cfg: &CircuitBreakerConfig,
    at_ms: u64,
) -> BreakerEvaluation {
    let mut rec = prev.unwrap_or_else(|| CircuitBreakerRecord {
        run_id: run_id.to_owned(),
        pane_id: pane_id.to_owned(),
        recent_trees_json: "[]".into(),
        last_error_hash: None,
        same_error_streak: 0,
        no_progress_streak: 0,
        tripped_reason: None,
        tripped_at_ms: None,
        bypass_until_turn: 0,
        updated_ms: at_ms,
    });
    rec.pane_id = pane_id.to_owned();
    rec.updated_ms = at_ms;

    let mut trees: Vec<TreeHistoryEntry> =
        serde_json::from_str(&rec.recent_trees_json).unwrap_or_default();
    let window = cfg.oscillation_window.clamp(3, 24);
    let prev_tree_oid = trees.last().map(|e| e.tree_oid.clone());

    let mut oscillation_detected = false;
    let mut pre_osc_cp: Option<u64> = None;

    if let Some(oid) = tree_oid.filter(|s| !s.is_empty()) {
        // Detect A -> B -> A oscillation: `oid` matches an earlier entry while
        // the immediately preceding entry has a different tree OID.
        if prev_tree_oid.as_deref().is_some_and(|last| last != oid)
            && let Some(first_match) = trees.iter().find(|e| e.tree_oid == oid)
        {
            oscillation_detected = true;
            pre_osc_cp = first_match.checkpoint_id;
        }
        trees.push(TreeHistoryEntry {
            turn_seq,
            tree_oid: oid.to_owned(),
            checkpoint_id,
        });
        if trees.len() > window {
            let drain = trees.len() - window;
            trees.drain(0..drain);
        }
    }

    let error_fp = extract_error_fingerprint(output_sample);
    let error_excerpt = error_fp.as_ref().map(|(_, ex)| ex.clone());
    match error_fp {
        Some((hash, _)) => {
            if rec.last_error_hash.as_deref() == Some(hash.as_str()) {
                rec.same_error_streak = rec.same_error_streak.saturating_add(1);
            } else {
                rec.last_error_hash = Some(hash);
                rec.same_error_streak = 1;
            }
        }
        None => {
            rec.last_error_hash = None;
            rec.same_error_streak = 0;
        }
    }

    let tree_unchanged = match (prev_tree_oid.as_deref(), tree_oid) {
        (Some(a), Some(b)) => a == b,
        (None, None) => true,
        _ => false,
    };
    if tree_unchanged || rec.same_error_streak > 1 {
        rec.no_progress_streak = rec.no_progress_streak.saturating_add(1);
    } else {
        rec.no_progress_streak = 0;
    }

    rec.recent_trees_json = serde_json::to_string(&trees).unwrap_or_else(|_| "[]".into());

    let bypassed = turn_seq > 0 && turn_seq <= rec.bypass_until_turn;
    let was_tripped = rec.tripped_reason.is_some();
    let new_reason = if !cfg.enabled || bypassed {
        None
    } else if oscillation_detected {
        Some(format!(
            "tree_oscillation: working tree reverted to an earlier state within {window} turns"
        ))
    } else if rec.same_error_streak >= cfg.same_error_limit {
        Some(format!(
            "same_error_streak: identical error repeated {} consecutive turns (limit {})",
            rec.same_error_streak, cfg.same_error_limit
        ))
    } else if rec.no_progress_streak >= cfg.max_no_progress_turns {
        Some(format!(
            "no_progress_loop: {} consecutive turns with no forward progress (limit {})",
            rec.no_progress_streak, cfg.max_no_progress_turns
        ))
    } else {
        None
    };

    let mut newly_tripped = false;
    if let Some(reason) = new_reason {
        if !was_tripped {
            newly_tripped = true;
            rec.tripped_at_ms = Some(at_ms);
        }
        rec.tripped_reason = Some(reason);
    }

    let should_auto_interrupt =
        newly_tripped && !(pane_actively_focused && cfg.skip_auto_interrupt_when_focused);

    BreakerEvaluation {
        record: rec,
        newly_tripped,
        should_auto_interrupt,
        pane_actively_focused,
        pre_oscillation_checkpoint_id: pre_osc_cp
            .or_else(|| find_pre_oscillation_checkpoint(&trees)),
        error_excerpt,
    }
}

pub fn find_pre_oscillation_checkpoint(trees: &[TreeHistoryEntry]) -> Option<u64> {
    trees.first().and_then(|e| e.checkpoint_id)
}

/// D-05: Checks whether `pane_id` is actively focused by a human user right now:
/// `pane_active == 1 && window_active == 1` AND an attached client on the same
/// session has `client_activity` within `grace_secs`.
pub fn is_pane_actively_focused(socket: &Path, pane_id: &str, grace_secs: u64) -> bool {
    let Ok(masil_bin) = native_executable() else {
        return false;
    };
    let pane_out = Command::new(&masil_bin)
        .arg("-S")
        .arg(socket)
        .args([
            "display-message",
            "-p",
            "-t",
            pane_id,
            "#{pane_active}\t#{window_active}\t#{session_id}",
        ])
        .output();
    let Ok(pane_out) = pane_out else {
        return false;
    };
    if !pane_out.status.success() {
        return false;
    }
    let text = String::from_utf8_lossy(&pane_out.stdout);
    let parts: Vec<&str> = text.trim().split('\t').collect();
    if parts.len() < 3 || parts[0] != "1" || parts[1] != "1" {
        return false;
    }
    let session_id = parts[2];

    let clients_out = Command::new(&masil_bin)
        .arg("-S")
        .arg(socket)
        .args(["list-clients", "-F", "#{session_id}\t#{client_activity}"])
        .output();
    let Ok(clients_out) = clients_out else {
        return false;
    };
    if !clients_out.status.success() {
        return false;
    }
    let now_secs = now_ms() / 1000;
    for line in String::from_utf8_lossy(&clients_out.stdout).lines() {
        let mut fields = line.split('\t');
        let Some(sid) = fields.next() else {
            continue;
        };
        let Some(act_str) = fields.next() else {
            continue;
        };
        if sid == session_id
            && let Ok(act_secs) = act_str.trim().parse::<u64>()
            && now_secs.saturating_sub(act_secs) <= grace_secs
        {
            return true;
        }
    }
    false
}

/// Evaluates and persists a turn for `run_id`, optionally issuing a safe
/// provider interrupt if tripped on an unfocused/AFK pane.
#[allow(clippy::too_many_arguments)]
pub fn record_and_evaluate_turn(
    socket: &Path,
    run_id: &str,
    pane_id: &str,
    turn_seq: u64,
    tree_oid: Option<&str>,
    checkpoint_id: Option<u64>,
    output_sample: &str,
    cfg: &CircuitBreakerConfig,
) -> Option<BreakerEvaluation> {
    let mut store = Store::open(socket).ok()?;
    let prev = store.get_circuit_breaker(run_id).ok().flatten();
    let focused = is_pane_actively_focused(socket, pane_id, cfg.active_focus_grace_secs);
    let eval = evaluate_turn_state(
        prev,
        run_id,
        pane_id,
        turn_seq,
        tree_oid,
        checkpoint_id,
        output_sample,
        focused,
        cfg,
        now_ms(),
    );
    let _ = store.upsert_circuit_breaker(&eval.record);
    if eval.should_auto_interrupt {
        // Safe provider interrupt via `masil-agent agent interrupt` (never raw keys).
        if let Ok(exe) = std::env::current_exe() {
            let _ = Command::new(exe)
                .args([
                    "agent",
                    "--socket",
                    &socket.to_string_lossy(),
                    "interrupt",
                    pane_id,
                    "--any-state",
                ])
                .output();
        }
    }
    Some(eval)
}

/// Extends circuit breaker bypass by `extra_turns` (default 10) and clears any
/// active trip state.
pub fn extend_bypass(
    socket: &Path,
    run_id: &str,
    pane_id: &str,
    extra_turns: u64,
) -> Result<CircuitBreakerRecord, String> {
    let mut store = Store::open(socket)?;
    let at_ms = now_ms();
    let mut rec = store
        .get_circuit_breaker(run_id)?
        .unwrap_or_else(|| CircuitBreakerRecord {
            run_id: run_id.to_owned(),
            pane_id: pane_id.to_owned(),
            recent_trees_json: "[]".into(),
            last_error_hash: None,
            same_error_streak: 0,
            no_progress_streak: 0,
            tripped_reason: None,
            tripped_at_ms: None,
            bypass_until_turn: 0,
            updated_ms: at_ms,
        });
    let trees: Vec<TreeHistoryEntry> =
        serde_json::from_str(&rec.recent_trees_json).unwrap_or_default();
    let current_turn = trees.last().map_or(0, |t| t.turn_seq);
    rec.tripped_reason = None;
    rec.tripped_at_ms = None;
    rec.same_error_streak = 0;
    rec.no_progress_streak = 0;
    rec.bypass_until_turn = current_turn
        .max(rec.bypass_until_turn)
        .saturating_add(extra_turns.max(1));
    rec.updated_ms = at_ms;
    store.upsert_circuit_breaker(&rec)?;
    Ok(rec)
}

/// Clears tripped state without setting a future bypass window.
pub fn dismiss_trip(
    socket: &Path,
    run_id: &str,
    pane_id: &str,
) -> Result<CircuitBreakerRecord, String> {
    let mut store = Store::open(socket)?;
    let at_ms = now_ms();
    let mut rec = store
        .get_circuit_breaker(run_id)?
        .unwrap_or_else(|| CircuitBreakerRecord {
            run_id: run_id.to_owned(),
            pane_id: pane_id.to_owned(),
            recent_trees_json: "[]".into(),
            last_error_hash: None,
            same_error_streak: 0,
            no_progress_streak: 0,
            tripped_reason: None,
            tripped_at_ms: None,
            bypass_until_turn: 0,
            updated_ms: at_ms,
        });
    rec.tripped_reason = None;
    rec.tripped_at_ms = None;
    rec.same_error_streak = 0;
    rec.no_progress_streak = 0;
    rec.updated_ms = at_ms;
    store.upsert_circuit_breaker(&rec)?;
    Ok(rec)
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}
