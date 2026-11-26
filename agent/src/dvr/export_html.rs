//! Self-contained, zero-CDN PR Verification Trace HTML Exporter (Phase 3B, D-12).
//!
//! Produces a single offline HTML artifact containing:
//! - Interactive vanilla-JS turn scrubber (`Turn 1 .. Turn N`)
//! - Per-turn prompt preview, unified diff viewer, BS Detector verdict badge,
//!   and terminal/test-evidence excerpt
//! - Mandatory secret & home-path redaction summary banner (`redact_secrets` /
//!   `redact_secrets_strict`) with zero external network/CDN requests.

use crate::briefing::redact::{RedactionResult, redact_secrets, redact_secrets_strict};
use serde::Serialize;
use std::collections::BTreeSet;

#[derive(Debug, Clone, Serialize)]
pub struct TraceTurnEntry {
    pub turn_seq: u64,
    pub provider: String,
    pub prompt_preview: String,
    pub checkpoint_id: String,
    pub tree_oid: String,
    pub audit_verdict: String,
    pub files_changed: u64,
    pub pinned: bool,
    pub pin_note: Option<String>,
    pub diff: String,
    pub terminal_excerpt: String,
    pub created_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TraceExportInput {
    pub agent_name: String,
    pub provider: String,
    pub run_id: String,
    pub repo_label: String,
    pub strict_redaction: bool,
    pub exported_at_ms: u64,
    pub turns: Vec<TraceTurnEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TraceExportOutput {
    pub html: String,
    pub turn_count: usize,
    pub total_redactions: usize,
    pub redaction_kinds: Vec<String>,
}

/// Builds a self-contained zero-external-resource HTML verification trace.
pub fn render_trace_html(input: &TraceExportInput) -> TraceExportOutput {
    let mut total_redactions = 0usize;
    let mut kinds = BTreeSet::new();

    let mut apply_redact = |raw: &str| -> String {
        let res: RedactionResult = if input.strict_redaction {
            redact_secrets_strict(raw)
        } else {
            redact_secrets(raw)
        };
        total_redactions += res.total_redactions;
        for k in res.kinds {
            kinds.insert(k);
        }
        res.text
    };

    let safe_agent = apply_redact(&input.agent_name);
    let safe_repo = apply_redact(&input.repo_label);
    let safe_turns: Vec<TraceTurnEntry> = input
        .turns
        .iter()
        .map(|t| TraceTurnEntry {
            turn_seq: t.turn_seq,
            provider: t.provider.clone(),
            prompt_preview: apply_redact(&t.prompt_preview),
            checkpoint_id: t.checkpoint_id.clone(),
            tree_oid: t.tree_oid.clone(),
            audit_verdict: t.audit_verdict.clone(),
            files_changed: t.files_changed,
            pinned: t.pinned,
            pin_note: t.pin_note.as_deref().map(&mut apply_redact),
            diff: apply_redact(&t.diff),
            terminal_excerpt: apply_redact(&t.terminal_excerpt),
            created_ms: t.created_ms,
        })
        .collect();

    let redaction_kinds: Vec<String> = kinds.into_iter().collect();
    let verified_turns = safe_turns
        .iter()
        .filter(|t| t.audit_verdict == "verified")
        .count();
    let flagged_turns = safe_turns
        .iter()
        .filter(|t| t.audit_verdict == "flagged")
        .count();

    // Safely embed JSON inside <script> by escaping `</` and `<script` sequences.
    let turns_json = serde_json::to_string(&safe_turns)
        .unwrap_or_else(|_| "[]".into())
        .replace("</", "<\\/")
        .replace("<!--", "<\\!--");

    let kinds_label = if redaction_kinds.is_empty() {
        "none detected".to_owned()
    } else {
        redaction_kinds.join(", ")
    };

    let html = format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>masil PR Verification Trace — {title_agent}</title>
<style>
  :root {{
    --bg: #0e1116; --panel: #161b22; --raised: #1f2631; --border: #2d3646;
    --text: #e6edf3; --muted: #8b949e; --accent: #58a6ff;
    --ok: #3fb950; --warn: #d29922; --danger: #f85149;
  }}
  * {{ box-sizing: border-box; }}
  body {{ margin: 0; font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif; background: var(--bg); color: var(--text); line-height: 1.5; }}
  header {{ padding: 20px 24px; border-bottom: 1px solid var(--border); background: var(--panel); }}
  h1 {{ margin: 0 0 6px; font-size: 20px; }}
  .meta {{ color: var(--muted); font-size: 13px; display: flex; flex-wrap: wrap; gap: 14px; }}
  .banner {{ margin: 16px 24px 0; padding: 10px 14px; border-radius: 8px; background: var(--raised); border: 1px solid var(--border); font-size: 13px; }}
  .metrics {{ display: grid; grid-template-columns: repeat(auto-fit, minmax(150px, 1fr)); gap: 12px; padding: 16px 24px; }}
  .metric {{ background: var(--panel); border: 1px solid var(--border); border-radius: 10px; padding: 12px 14px; }}
  .metric .label {{ font-size: 11px; color: var(--muted); text-transform: uppercase; letter-spacing: 0.04em; }}
  .metric .val {{ font-size: 20px; font-weight: 700; margin-top: 4px; }}
  .scrubber {{ margin: 0 24px 16px; padding: 16px; background: var(--panel); border: 1px solid var(--border); border-radius: 10px; }}
  .scrubber input[type=range] {{ width: 100%; accent-color: var(--accent); }}
  .turn-nav {{ display: flex; align-items: center; justify-content: space-between; gap: 12px; margin-bottom: 8px; }}
  button {{ background: var(--raised); color: var(--text); border: 1px solid var(--border); border-radius: 6px; padding: 6px 12px; cursor: pointer; font-size: 13px; }}
  button:hover {{ border-color: var(--accent); }}
  main {{ padding: 0 24px 32px; display: grid; gap: 16px; }}
  .card {{ background: var(--panel); border: 1px solid var(--border); border-radius: 10px; padding: 16px; }}
  .card h2 {{ margin: 0 0 10px; font-size: 15px; display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }}
  .badge {{ display: inline-block; padding: 2px 8px; border-radius: 999px; font-size: 11px; font-weight: 600; text-transform: uppercase; }}
  .badge.verified {{ background: rgba(63,185,80,0.18); color: var(--ok); }}
  .badge.warnings {{ background: rgba(210,153,34,0.2); color: var(--warn); }}
  .badge.flagged {{ background: rgba(248,81,73,0.2); color: var(--danger); }}
  .badge.pinned {{ background: rgba(88,166,255,0.2); color: var(--accent); }}
  pre {{ margin: 0; padding: 12px; border-radius: 8px; background: #0a0d12; border: 1px solid var(--border); overflow-x: auto; font-size: 12px; font-family: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace; white-space: pre-wrap; word-break: break-word; }}
  .diff-add {{ color: #7ee787; }}
  .diff-del {{ color: #ffa198; }}
  .diff-hunk {{ color: #79c0ff; }}
</style>
</head>
<body>
<header>
  <h1>masil PR Verification Trace · {title_agent}</h1>
  <div class="meta">
    <span>Provider: <strong>{provider}</strong></span>
    <span>Run: <code>{run_id}</code></span>
    <span>Workspace: <code>{repo_label}</code></span>
    <span>Redaction Mode: <strong>{redact_mode}</strong></span>
  </div>
</header>
<div class="banner">
  <strong>Secret &amp; Path Redaction:</strong> {total_redactions} item(s) scrubbed ({kinds_label}). Self-contained offline HTML artifact (zero external CDN or network assets).
</div>
<section class="metrics">
  <div class="metric"><div class="label">Turns Recorded</div><div class="val">{turn_count}</div></div>
  <div class="metric"><div class="label">BS Audit Verified</div><div class="val" style="color:var(--ok)">{verified_turns}</div></div>
  <div class="metric"><div class="label">BS Audit Flagged</div><div class="val" style="color:var(--danger)">{flagged_turns}</div></div>
  <div class="metric"><div class="label">Secrets Scrubbed</div><div class="val" style="color:var(--accent)">{total_redactions}</div></div>
</section>
<section class="scrubber">
  <div class="turn-nav">
    <button id="prevBtn" type="button">← Previous Turn</button>
    <strong id="turnLabel">Turn 1 / {turn_count}</strong>
    <button id="nextBtn" type="button">Next Turn →</button>
  </div>
  <input id="turnSlider" type="range" min="0" max="{max_idx}" value="{max_idx}">
</section>
<main>
  <article class="card">
    <h2>Turn Details <span id="verdictBadge" class="badge verified">verified</span> <span id="pinBadge" class="badge pinned" style="display:none">PINNED</span></h2>
    <div id="turnMeta" class="meta" style="margin-bottom:10px"></div>
    <pre id="promptBox"></pre>
  </article>
  <article class="card">
    <h2>Unified Diff</h2>
    <pre id="diffBox"></pre>
  </article>
  <article class="card">
    <h2>Terminal &amp; Verification Evidence</h2>
    <pre id="termBox"></pre>
  </article>
</main>
<script>
const TURNS = {turns_json};
const slider = document.getElementById('turnSlider');
const turnLabel = document.getElementById('turnLabel');
const verdictBadge = document.getElementById('verdictBadge');
const pinBadge = document.getElementById('pinBadge');
const turnMeta = document.getElementById('turnMeta');
const promptBox = document.getElementById('promptBox');
const diffBox = document.getElementById('diffBox');
const termBox = document.getElementById('termBox');

function esc(s) {{
  return String(s || '').replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;');
}}

function formatDiff(raw) {{
  if (!raw) return '<span style="color:var(--muted)">(no file changes in this turn)</span>';
  return raw.split('\n').map(line => {{
    if (line.startsWith('+') && !line.startsWith('+++')) return '<span class="diff-add">' + esc(line) + '</span>';
    if (line.startsWith('-') && !line.startsWith('---')) return '<span class="diff-del">' + esc(line) + '</span>';
    if (line.startsWith('@@')) return '<span class="diff-hunk">' + esc(line) + '</span>';
    return esc(line);
  }}).join('\n');
}}

function render(idx) {{
  if (!TURNS.length) {{
    turnLabel.textContent = 'No turns recorded';
    promptBox.textContent = '(empty)';
    diffBox.textContent = '(empty)';
    termBox.textContent = '(empty)';
    return;
  }}
  const clamped = Math.max(0, Math.min(idx, TURNS.length - 1));
  slider.value = String(clamped);
  const t = TURNS[clamped];
  turnLabel.textContent = 'Turn ' + t.turn_seq + ' (' + (clamped + 1) + ' / ' + TURNS.length + ')';
  const v = (t.audit_verdict || 'verified').toLowerCase();
  verdictBadge.textContent = v;
  verdictBadge.className = 'badge ' + (v === 'flagged' ? 'flagged' : v === 'warnings' ? 'warnings' : 'verified');
  pinBadge.style.display = t.pinned ? 'inline-block' : 'none';
  pinBadge.textContent = t.pin_note ? ('PINNED · ' + t.pin_note) : 'PINNED';
  turnMeta.textContent = 'Checkpoint #' + t.checkpoint_id + ' · Tree ' + (t.tree_oid || '').slice(0, 10) + ' · Files changed: ' + t.files_changed;
  promptBox.textContent = t.prompt_preview || '(turn recorded)';
  diffBox.innerHTML = formatDiff(t.diff);
  termBox.textContent = t.terminal_excerpt || '(no terminal excerpt captured)';
}}

slider.addEventListener('input', () => render(Number(slider.value)));
document.getElementById('prevBtn').addEventListener('click', () => render(Number(slider.value) - 1));
document.getElementById('nextBtn').addEventListener('click', () => render(Number(slider.value) + 1));
render(TURNS.length - 1);
</script>
</body>
</html>
"#,
        title_agent = escape_html(&safe_agent),
        provider = escape_html(&input.provider),
        run_id = escape_html(&input.run_id),
        repo_label = escape_html(&safe_repo),
        redact_mode = if input.strict_redaction {
            "strict (patterns + high-entropy)"
        } else {
            "standard"
        },
        total_redactions = total_redactions,
        kinds_label = escape_html(&kinds_label),
        turn_count = safe_turns.len(),
        verified_turns = verified_turns,
        flagged_turns = flagged_turns,
        max_idx = safe_turns.len().saturating_sub(1),
        turns_json = turns_json,
    );

    TraceExportOutput {
        html,
        turn_count: safe_turns.len(),
        total_redactions,
        redaction_kinds,
    }
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
