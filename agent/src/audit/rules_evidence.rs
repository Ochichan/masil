//! Evidence verification rule (`E01`): cross-checks agent completion claims
//! ("all tests pass", "build succeeded", "테스트 통과") against actual observed
//! test/build execution timestamps AFTER the last file write (D-02).

use super::config::RepoAuditConfig;
use super::{AuditFinding, Severity};
use masil_wire::rpc::{TimelineItem, ToolStatus};

const PASS_CLAIM_PHRASES: &[&str] = &[
    "all tests pass",
    "all tests passed",
    "tests pass",
    "tests passed",
    "test suite pass",
    "all checks pass",
    "all checks passed",
    "build succeeded",
    "build passed",
    "0 failed",
    "passed all tests",
    "테스트 통과",
    "테스트 모두 통과",
    "테스트가 통과",
    "빌드 성공",
    "검증 통과",
    "전부 통과",
];

const TEST_OR_BUILD_COMMANDS: &[&str] = &[
    "cargo test",
    "cargo check",
    "cargo clippy",
    "pytest",
    "unittest",
    "vitest",
    "jest",
    "npm test",
    "npm run test",
    "npm run typecheck",
    "npm run build",
    "pnpm test",
    "yarn test",
    "go test",
    "make test",
    "swift test",
    "xcodebuild test",
    "tsc ",
    "tsc\n",
    "rspec",
];

const WRITE_TOOL_NAMES: &[&str] = &[
    "edit",
    "write",
    "edit_file",
    "write_file",
    "replace_file_content",
    "multi_replace_file_content",
    "notebookedit",
    "apply_patch",
    "create_file",
    "str_replace",
];

const SHELL_TOOL_NAMES: &[&str] = &[
    "bash",
    "shell",
    "run_command",
    "exec",
    "terminal",
    "command",
    "sh",
];

const PANE_PASS_MARKERS: &[&str] = &[
    "test result: ok.",
    "0 failed",
    "passed (",
    "ok\n",
    "ran ",
    "finished `",
    "✓ ",
];

const FAILURE_OUTPUT_MARKERS: &[&str] = &[
    "test result: failed",
    "command exited with code 1",
    "command exited with code 2",
    "command exited with code 101",
    "failed (",
    "traceback (most recent call last)",
    "assertionerror",
    "error[e",
];

/// Returns `(claim_verified, optional_E01_finding)`.
/// - `None` when the turn made no test/build-pass claim.
/// - `Some(true)` when a passing test/build run was observed after the last write.
/// - `Some(false)` + `CRITICAL` `E01` finding when pass was claimed without fresh proof.
pub fn check_evidence_rule(
    transcript_slice: &[TimelineItem],
    pane_tail: &str,
    has_code_changes: bool,
    cfg: &RepoAuditConfig,
) -> (Option<bool>, Option<AuditFinding>) {
    let claim_text = latest_assistant_claim(transcript_slice, pane_tail);
    let Some(matched_claim) = claim_text else {
        return (None, None);
    };

    // Determine the sequence position and timestamp of the latest file write in
    // this turn's transcript slice.
    let mut last_write_idx: Option<usize> = None;
    let mut last_write_ts: i64 = 0;
    for (idx, item) in transcript_slice.iter().enumerate() {
        if let TimelineItem::ToolCall {
            name,
            summary,
            ts_ms,
            ..
        } = item
            && is_write_tool(name, summary)
        {
            last_write_idx = Some(idx);
            last_write_ts = last_write_ts.max(*ts_ms);
        }
    }

    // Look for a completed, non-failing test/build tool call strictly after the
    // last write tool call (or anywhere in slice if no write tool appeared).
    let verified_by_transcript =
        transcript_slice
            .iter()
            .enumerate()
            .any(|(idx, item)| match item {
                TimelineItem::ToolCall {
                    name,
                    summary,
                    status,
                    output,
                    ts_ms,
                    ..
                } => {
                    let after_write = match last_write_idx {
                        Some(w_idx) => idx > w_idx && (*ts_ms == 0 || *ts_ms >= last_write_ts),
                        None => true,
                    };
                    if !after_write || *status != ToolStatus::Completed {
                        return false;
                    }
                    if !is_test_or_build_call(name, summary) {
                        return false;
                    }
                    if let Some(out) = output {
                        let lower = out.to_ascii_lowercase();
                        if FAILURE_OUTPUT_MARKERS.iter().any(|m| lower.contains(m)) {
                            return false;
                        }
                    }
                    true
                }
                _ => false,
            });

    // If no transcript tool calls are available (e.g. pane-only agent), check PTY
    // tail for explicit test/build runner pass output — only when transcript had
    // no later write tool call.
    let verified_by_pane =
        !verified_by_transcript && last_write_idx.is_none() && pane_has_passing_test_run(pane_tail);

    if verified_by_transcript || verified_by_pane {
        return (Some(true), None);
    }

    // Even without file edits, claiming "all tests pass" without running any test
    // is still an unverified claim, though note whether code changed in the detail.
    let finding = cfg
        .resolve_severity("E01", Severity::Critical)
        .map(|sev| AuditFinding {
            id: "E01:unverified-pass-claim".into(),
            rule_id: "E01".into(),
            category: "unverified_claim".into(),
            severity: sev,
            file: None,
            line: None,
            title: "Unverified test/build pass claim".into(),
            detail: if has_code_changes {
                format!(
                    "Agent claimed \"{matched_claim}\", but no passing test or build run was observed after the last file modification."
                )
            } else {
                format!(
                    "Agent claimed \"{matched_claim}\", but no test or build execution was observed in this turn."
                )
            },
            evidence: matched_claim,
        });

    (Some(false), finding)
}

fn latest_assistant_claim(transcript_slice: &[TimelineItem], pane_tail: &str) -> Option<String> {
    for item in transcript_slice.iter().rev() {
        if let TimelineItem::AssistantMessage { markdown, .. } = item {
            if let Some(phrase) = find_claim_phrase(markdown) {
                return Some(phrase);
            }
            // Only inspect the most recent assistant message.
            return None;
        }
    }
    // Pane fallback when no transcript assistant messages exist.
    find_claim_phrase(pane_tail)
}

fn find_claim_phrase(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    for phrase in PASS_CLAIM_PHRASES {
        if lower.contains(phrase) {
            // Avoid false positives on negations like "not all tests pass" or "tests failed".
            if let Some(pos) = lower.find(phrase) {
                let prefix = &lower[pos.saturating_sub(16)..pos];
                if prefix.contains("not ")
                    || prefix.contains("didn't ")
                    || prefix.contains("did not ")
                    || prefix.contains("fail")
                    || prefix.contains("않")
                    || prefix.contains("못")
                {
                    continue;
                }
            }
            return Some((*phrase).to_owned());
        }
    }
    None
}

fn is_write_tool(name: &str, summary: &str) -> bool {
    let n = name.to_ascii_lowercase();
    if WRITE_TOOL_NAMES.iter().any(|w| n == *w || n.contains(w)) {
        return true;
    }
    let s = summary.to_ascii_lowercase();
    SHELL_TOOL_NAMES.iter().any(|sh| n.contains(sh))
        && (s.contains("sed -i") || s.contains(" > ") || s.contains("tee "))
}

fn is_test_or_build_call(name: &str, summary: &str) -> bool {
    let n = name.to_ascii_lowercase();
    let s = summary.to_ascii_lowercase();
    let looks_shell = SHELL_TOOL_NAMES.iter().any(|sh| n.contains(sh)) || n.contains("test");
    looks_shell && TEST_OR_BUILD_COMMANDS.iter().any(|cmd| s.contains(cmd))
}

fn pane_has_passing_test_run(pane_tail: &str) -> bool {
    let lower = pane_tail.to_ascii_lowercase();
    if FAILURE_OUTPUT_MARKERS.iter().any(|m| lower.contains(m)) {
        return false;
    }
    let has_runner = TEST_OR_BUILD_COMMANDS.iter().any(|cmd| lower.contains(cmd))
        || lower.contains("test result: ok.")
        || lower.contains("vitest")
        || lower.contains("pytest");
    let has_pass = PANE_PASS_MARKERS.iter().any(|m| lower.contains(m));
    has_runner && has_pass
}
