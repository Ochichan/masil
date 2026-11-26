//! Phase 1A: BS Detector — 100% passive, deterministic, zero-LLM Rust turn
//! auditor and Hybrid Risk Card Stack producer (D-02, D-03).

pub mod config;
pub mod risk_score;
pub mod rules_evidence;
pub mod rules_guardrail;
pub mod rules_test;
pub mod ui_card;

use crate::managed::{Store, TurnAuditRecord};
use crate::observation::now_ms;
use config::{GlobalAuditConfig, RepoAuditConfig};
use masil_wire::rpc::TimelineItem;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;
use std::process::Command;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Critical,
    High,
    Warning,
    Info,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Critical => "critical",
            Self::High => "high",
            Self::Warning => "warning",
            Self::Info => "info",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditFinding {
    pub id: String,
    pub rule_id: String,
    pub category: String,
    pub severity: Severity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    pub title: String,
    pub detail: String,
    pub evidence: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RiskHunk {
    pub id: String,
    pub file: String,
    pub old_start: usize,
    pub new_start: usize,
    pub added: usize,
    pub removed: usize,
    pub risk_score: u8,
    pub reason: String,
    pub diff: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditReport {
    pub id: i64,
    pub pane_id: String,
    pub run_id: String,
    pub checkpoint_id: Option<String>,
    pub prev_checkpoint_id: Option<String>,
    /// `'verified' | 'warnings' | 'flagged'`
    pub verdict: String,
    pub risk_score: u8,
    pub claim_verified: Option<bool>,
    pub findings: Vec<AuditFinding>,
    pub hunks: Vec<RiskHunk>,
    pub created_ms: u64,
}

impl AuditReport {
    pub fn count_severity(&self, sev: Severity) -> usize {
        self.findings.iter().filter(|f| f.severity == sev).count()
    }

    pub fn has_critical(&self) -> bool {
        self.count_severity(Severity::Critical) > 0
    }

    pub fn to_summary_json(&self) -> Value {
        json!({
            "id": self.id,
            "verdict": self.verdict,
            "risk_score": self.risk_score,
            "claim_verified": self.claim_verified,
            "critical_count": self.count_severity(Severity::Critical),
            "high_count": self.count_severity(Severity::High),
            "warning_count": self.count_severity(Severity::Warning),
            "info_count": self.count_severity(Severity::Info),
            "hunk_count": self.hunks.len(),
            "checkpoint_id": self.checkpoint_id,
            "prev_checkpoint_id": self.prev_checkpoint_id,
            "created_ms": self.created_ms,
        })
    }

    pub fn to_record(&self) -> TurnAuditRecord {
        TurnAuditRecord {
            id: self.id,
            pane_id: self.pane_id.clone(),
            run_id: self.run_id.clone(),
            checkpoint_id: self.checkpoint_id.clone(),
            prev_checkpoint_id: self.prev_checkpoint_id.clone(),
            verdict: self.verdict.clone(),
            risk_score: i64::from(self.risk_score),
            claim_verified: self.claim_verified,
            findings_json: serde_json::to_string(&self.findings).unwrap_or_else(|_| "[]".into()),
            hunks_json: serde_json::to_string(&self.hunks).unwrap_or_else(|_| "[]".into()),
            created_ms: self.created_ms,
        }
    }

    pub fn from_record(rec: TurnAuditRecord) -> Self {
        let findings: Vec<AuditFinding> =
            serde_json::from_str(&rec.findings_json).unwrap_or_default();
        let hunks: Vec<RiskHunk> = serde_json::from_str(&rec.hunks_json).unwrap_or_default();
        Self {
            id: rec.id,
            pane_id: rec.pane_id,
            run_id: rec.run_id,
            checkpoint_id: rec.checkpoint_id,
            prev_checkpoint_id: rec.prev_checkpoint_id,
            verdict: rec.verdict,
            risk_score: rec.risk_score.clamp(0, 100) as u8,
            claim_verified: rec.claim_verified,
            findings,
            hunks,
            created_ms: rec.created_ms,
        }
    }
}

pub struct TurnAuditInput<'a> {
    pub pane_id: &'a str,
    pub run_id: &'a str,
    pub checkpoint_id: Option<&'a str>,
    pub prev_checkpoint_id: Option<&'a str>,
    pub unified_diff: &'a str,
    pub transcript_slice: &'a [TimelineItem],
    pub pane_tail: &'a str,
    pub last_user_prompt: Option<&'a str>,
    pub repo_cfg: &'a RepoAuditConfig,
}

/// Core 100% passive, deterministic turn auditor (target `< 80ms`/turn).
pub fn audit_turn(input: &TurnAuditInput<'_>) -> AuditReport {
    let files = risk_score::parse_unified_diff(input.unified_diff);
    let mut findings = Vec::new();
    findings.extend(rules_test::check_test_rules(
        &files,
        input.last_user_prompt,
        input.repo_cfg,
    ));
    findings.extend(rules_guardrail::check_guardrail_rules(
        &files,
        input.repo_cfg,
    ));
    let (claim_verified, e01) = rules_evidence::check_evidence_rule(
        input.transcript_slice,
        input.pane_tail,
        !files.is_empty(),
        input.repo_cfg,
    );
    if let Some(finding) = e01 {
        findings.push(finding);
    }

    // Order findings by severity (Critical -> High -> Warning -> Info), then rule ID.
    findings.sort_by(|a, b| {
        a.severity
            .cmp(&b.severity)
            .then_with(|| a.rule_id.cmp(&b.rule_id))
            .then_with(|| a.file.cmp(&b.file))
    });

    let (risk_score, hunks) =
        risk_score::score_turn_and_rank_hunks(&files, &findings, input.repo_cfg);

    let verdict = if findings
        .iter()
        .any(|f| matches!(f.severity, Severity::Critical | Severity::High))
    {
        "flagged"
    } else if !findings.is_empty() {
        "warnings"
    } else {
        "verified"
    };

    AuditReport {
        id: 0,
        pane_id: input.pane_id.to_owned(),
        run_id: input.run_id.to_owned(),
        checkpoint_id: input.checkpoint_id.map(str::to_owned),
        prev_checkpoint_id: input.prev_checkpoint_id.map(str::to_owned),
        verdict: verdict.to_owned(),
        risk_score,
        claim_verified,
        findings,
        hunks,
        created_ms: now_ms(),
    }
}

/// Captures the unified diff for a turn without touching the user's git index.
/// Prefers the sidecar checkpoint store (`prev_commit:w` -> `curr_commit:w`)
/// when two checkpoints exist; falls back to read-only `git diff HEAD`.
pub fn capture_turn_diff(
    cwd: &Path,
    curr_checkpoint_id: Option<u64>,
) -> (Option<String>, Option<String>, String) {
    let Ok(root) = crate::changes::root_of(cwd) else {
        return (None, None, String::new());
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    if let Ok(store) = crate::checkpoint::Store::open(&root, deadline)
        && let Ok(listed) = crate::checkpoint::list(&store)
        && let Some(cps) = listed.get("checkpoints").and_then(Value::as_array)
    {
        let curr_idx = curr_checkpoint_id
            .and_then(|wanted| {
                cps.iter()
                    .position(|c| c.get("id").and_then(Value::as_u64) == Some(wanted))
            })
            .unwrap_or(0);
        if let Some(curr) = cps.get(curr_idx)
            && let (Some(curr_id), Some(curr_commit)) = (
                curr.get("id").and_then(Value::as_u64),
                curr.get("commit").and_then(Value::as_str),
            )
        {
            if let Some(prev) = cps.get(curr_idx + 1)
                && let (Some(prev_id), Some(prev_commit)) = (
                    prev.get("id").and_then(Value::as_u64),
                    prev.get("commit").and_then(Value::as_str),
                )
            {
                let diff = git_sidecar_diff(&store, prev_commit, curr_commit);
                return (Some(curr_id.to_string()), Some(prev_id.to_string()), diff);
            }
            let diff = git_working_tree_diff(&root);
            return (Some(curr_id.to_string()), None, diff);
        }
    }
    (None, None, git_working_tree_diff(&root))
}

fn git_sidecar_diff(
    store: &crate::checkpoint::Store,
    prev_commit: &str,
    curr_commit: &str,
) -> String {
    let out = Command::new("git")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .arg("--git-dir")
        .arg(&store.repo)
        .args([
            "diff-tree",
            "-p",
            "-M",
            "--no-ext-diff",
            "--unified=3",
            &format!("{prev_commit}:w"),
            &format!("{curr_commit}:w"),
        ])
        .output();
    match out {
        Ok(o) if o.status.success() => truncate_bytes(&o.stdout, 512 * 1024),
        _ => String::new(),
    }
}

fn git_working_tree_diff(root: &Path) -> String {
    let out = Command::new("git")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .arg("-C")
        .arg(root)
        .args(["diff", "--no-ext-diff", "--unified=3", "HEAD"])
        .output();
    match out {
        Ok(o) if o.status.success() => truncate_bytes(&o.stdout, 512 * 1024),
        _ => String::new(),
    }
}

fn truncate_bytes(raw: &[u8], max_bytes: usize) -> String {
    let slice = if raw.len() > max_bytes {
        &raw[..max_bytes]
    } else {
        raw
    };
    String::from_utf8_lossy(slice).into_owned()
}

/// End-to-end post-turn audit executed asynchronously by the coordinator when a
/// turn finishes. Persists to SQLite `turn_audits` and publishes a 1-line
/// summary timeline card.
#[allow(clippy::too_many_arguments)]
pub fn run_and_persist_turn_audit(
    socket: &Path,
    pane_id: &str,
    run_id: &str,
    provider: &str,
    cwd: &str,
    reported_session_id: Option<&str>,
    curr_checkpoint_id: Option<u64>,
    pane_tail: &str,
) -> Option<AuditReport> {
    if !GlobalAuditConfig::load().is_enabled() {
        return None;
    }
    let cwd_path = Path::new(cwd);
    let repo_root = crate::changes::root_of(cwd_path).unwrap_or_else(|_| cwd_path.to_path_buf());
    let repo_cfg = RepoAuditConfig::load_for_repo(&repo_root);

    let (cp_id, prev_cp_id, diff) = capture_turn_diff(cwd_path, curr_checkpoint_id);
    let socket_str = socket.to_string_lossy();
    let agent_key = format!("{socket_str}::{pane_id}");

    // Read recent transcript slice (bounded to last 30 items for < 80ms budget).
    let page =
        crate::remote::transcripts::fetch_timeline(crate::remote::transcripts::TimelineQuery {
            server_socket: &socket_str,
            pane_id,
            agent_key: &agent_key,
            provider,
            cwd,
            reported_session_id,
            state: "idle",
            limit: Some(30),
        });

    // Slice from the last user message forward so E01/T03 judge ONLY this turn.
    let turn_start = page
        .items
        .iter()
        .rposition(|it| matches!(it, TimelineItem::UserMessage { .. }))
        .unwrap_or(0);
    let turn_slice = &page.items[turn_start..];
    let last_user_prompt = turn_slice.iter().find_map(|it| match it {
        TimelineItem::UserMessage { text, .. } => Some(text.as_str()),
        _ => None,
    });

    let mut report = audit_turn(&TurnAuditInput {
        pane_id,
        run_id,
        checkpoint_id: cp_id.as_deref(),
        prev_checkpoint_id: prev_cp_id.as_deref(),
        unified_diff: &diff,
        transcript_slice: turn_slice,
        pane_tail,
        last_user_prompt,
        repo_cfg: &repo_cfg,
    });

    if let Ok(mut store) = Store::open(socket)
        && let Ok(id) = store.record_turn_audit(&report.to_record())
    {
        report.id = id;
    }

    // Post 1-line summary card to the agent's conversation timeline.
    if !diff.is_empty() || !report.findings.is_empty() {
        let card = ui_card::build_timeline_summary_card(&report);
        let _ = crate::plugin::runner::upsert_timeline_card(&agent_key, "masil-audit", &card);
    }

    Some(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use masil_wire::rpc::ToolStatus;

    #[test]
    fn detects_all_deterministic_rules_and_honors_repo_overrides() {
        let diff = r#"diff --git a/src/auth/login.test.ts b/src/auth/login.test.ts
index 111..222 100644
--- a/src/auth/login.test.ts
+++ b/src/auth/login.test.ts
@@ -10,6 +10,8 @@ describe("auth", () => {
-  expect(res.ok).toBe(true);
-  expect(res.token).toBeTruthy();
+  it.skip("validates expired token", () => {});
+  expect(true).toBe(true);
diff --git a/src/auth/service.ts b/src/auth/service.ts
index 333..444 100644
--- a/src/auth/service.ts
+++ b/src/auth/service.ts
@@ -1,4 +1,10 @@
+export function verifySession(tok: string) {
+  // @ts-ignore
+  try {
+    return eval(tok);
+  } catch {}
+}
"#;
        let transcript = vec![
            TimelineItem::UserMessage {
                id: "u1".into(),
                seq: 1,
                ts_ms: 1000,
                text: "Fix session token expiry handling".into(),
            },
            TimelineItem::ToolCall {
                id: "tc1".into(),
                seq: 2,
                ts_ms: 1500,
                name: "edit_file".into(),
                summary: "edit src/auth/service.ts".into(),
                args_json: None,
                status: ToolStatus::Completed,
                output: Some("ok".into()),
            },
            TimelineItem::AssistantMessage {
                id: "a1".into(),
                seq: 3,
                ts_ms: 2000,
                markdown: "Done! All tests pass and build succeeded.".into(),
                model: None,
            },
        ];

        let started = std::time::Instant::now();
        let report = audit_turn(&TurnAuditInput {
            pane_id: "%1",
            run_id: "run-1",
            checkpoint_id: Some("2"),
            prev_checkpoint_id: Some("1"),
            unified_diff: diff,
            transcript_slice: &transcript,
            pane_tail: "",
            last_user_prompt: Some("Fix session token expiry handling"),
            repo_cfg: &RepoAuditConfig::default(),
        });
        let elapsed = started.elapsed();
        assert!(
            elapsed.as_millis() < 80,
            "audit_turn took {}ms (> 80ms budget)",
            elapsed.as_millis()
        );

        let rules: Vec<&str> = report.findings.iter().map(|f| f.rule_id.as_str()).collect();
        assert!(rules.contains(&"T01"), "missing T01: {rules:?}");
        assert!(rules.contains(&"T02"), "missing T02: {rules:?}");
        assert!(rules.contains(&"T03"), "missing T03: {rules:?}");
        assert!(rules.contains(&"E01"), "missing E01: {rules:?}");
        assert!(rules.contains(&"G01"), "missing G01: {rules:?}");
        assert!(rules.contains(&"G02"), "missing G02: {rules:?}");
        assert!(rules.contains(&"S01"), "missing S01: {rules:?}");
        assert_eq!(report.verdict, "flagged");
        assert_eq!(report.claim_verified, Some(false));
        assert!(report.risk_score >= 60);
        assert!(!report.hunks.is_empty());
        assert!(report.hunks.len() <= 5);

        // Verify challenge prompt and UI Spec v1 card generation.
        let challenge = ui_card::build_challenge_prompt(&report, Some("T01"), None);
        assert!(challenge.contains("[masil Audit · T01]"));
        let doc = ui_card::audit_to_ui_document(&report);
        assert_eq!(doc.v, 1);
        assert!(!doc.blocks.is_empty());

        // Verify `.masil/audit.toml` ignores and severity overrides work.
        let mut custom = RepoAuditConfig::default();
        custom.ignore_paths.push("src/auth/service.ts".into());
        custom.severity_overrides.insert("T03".into(), "off".into());
        let filtered = audit_turn(&TurnAuditInput {
            pane_id: "%1",
            run_id: "run-1",
            checkpoint_id: Some("2"),
            prev_checkpoint_id: Some("1"),
            unified_diff: diff,
            transcript_slice: &transcript,
            pane_tail: "",
            last_user_prompt: Some("Fix session token expiry handling"),
            repo_cfg: &custom,
        });
        assert!(!filtered.findings.iter().any(|f| f.rule_id == "T03"));
        assert!(
            !filtered
                .findings
                .iter()
                .any(|f| f.file.as_deref() == Some("src/auth/service.ts"))
        );
    }

    #[test]
    fn e01_verifies_when_test_command_succeeds_after_last_write() {
        let transcript = vec![
            TimelineItem::ToolCall {
                id: "w1".into(),
                seq: 1,
                ts_ms: 1000,
                name: "edit_file".into(),
                summary: "edit src/lib.rs".into(),
                args_json: None,
                status: ToolStatus::Completed,
                output: Some("ok".into()),
            },
            TimelineItem::ToolCall {
                id: "t1".into(),
                seq: 2,
                ts_ms: 2000,
                name: "run_command".into(),
                summary: "run_command: cargo test".into(),
                args_json: None,
                status: ToolStatus::Completed,
                output: Some("test result: ok. 12 passed; 0 failed".into()),
            },
            TimelineItem::AssistantMessage {
                id: "a1".into(),
                seq: 3,
                ts_ms: 2500,
                markdown: "All tests pass!".into(),
                model: None,
            },
        ];
        let report = audit_turn(&TurnAuditInput {
            pane_id: "%2",
            run_id: "run-2",
            checkpoint_id: None,
            prev_checkpoint_id: None,
            unified_diff: "",
            transcript_slice: &transcript,
            pane_tail: "",
            last_user_prompt: Some("fix bug"),
            repo_cfg: &RepoAuditConfig::default(),
        });
        assert_eq!(report.claim_verified, Some(true));
        assert!(!report.findings.iter().any(|f| f.rule_id == "E01"));
        assert_eq!(report.verdict, "verified");
    }
}
