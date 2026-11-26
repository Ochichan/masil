//! Test integrity rules (`T01`, `T02`, `T03`) for BS Detector (D-02).

use super::config::RepoAuditConfig;
use super::risk_score::DiffFile;
use super::{AuditFinding, Severity};

const SKIP_PATTERNS: &[&str] = &[
    ".skip(",
    "test.skip",
    "it.skip",
    "xit(",
    "xdescribe(",
    "describe.skip",
    "@pytest.mark.skip",
    "@unittest.skip",
    "#[ignore",
    "t.Skip(",
    "XCTSkip(",
    "skipTest(",
];

const DUMMY_ASSERT_PATTERNS: &[&str] = &[
    "assert!(true)",
    "assert_eq!(1, 1)",
    "assert_eq!(true, true)",
    "expect(true).toBe(true)",
    "expect(true).toEqual(true)",
    "expect(1).toBe(1)",
    "assert True",
    "assertEqual(1, 1)",
    "assertTrue(True)",
];

const TEST_PROMPT_KEYWORDS: &[&str] = &[
    "test",
    "spec",
    "snap",
    "snapshot",
    "fixture",
    "assert",
    "unittest",
    "vitest",
    "pytest",
    "regression",
    "테스트",
    "스냅샷",
    "회귀",
    "검증",
];

pub fn check_test_rules(
    files: &[DiffFile],
    last_prompt: Option<&str>,
    cfg: &RepoAuditConfig,
) -> Vec<AuditFinding> {
    let mut findings = Vec::new();
    let prompt_mentions_tests = last_prompt.is_none_or(|p| {
        let lower = p.to_ascii_lowercase();
        TEST_PROMPT_KEYWORDS.iter().any(|kw| lower.contains(kw))
    });

    for file in files {
        if cfg.is_ignored(&file.path) {
            continue;
        }
        let is_test = cfg.is_test_file(&file.path);
        let is_snapshot = file.path.ends_with(".snap");
        let mut added_assertions = 0usize;
        let mut removed_assertions = 0usize;
        let mut first_removed_assert_line: Option<usize> = None;
        let mut t01_hits = Vec::new();
        let mut dummy_hits = Vec::new();

        for hunk in &file.hunks {
            for line in &hunk.lines {
                let text = line.content.trim();
                if line.kind == '+' {
                    if let Some(matched) = SKIP_PATTERNS.iter().find(|pat| text.contains(**pat)) {
                        t01_hits.push((line.new_line, *matched, text.to_owned()));
                    }
                    if is_test {
                        if is_assertion_line(text) {
                            added_assertions += 1;
                        }
                        if let Some(matched) = DUMMY_ASSERT_PATTERNS
                            .iter()
                            .find(|pat| text.contains(**pat))
                        {
                            dummy_hits.push((line.new_line, *matched, text.to_owned()));
                        }
                    }
                } else if line.kind == '-' && is_test && is_assertion_line(text) {
                    removed_assertions += 1;
                    if first_removed_assert_line.is_none() {
                        first_removed_assert_line = line.old_line;
                    }
                }
            }
        }

        // T01: Test skip insertion (CRITICAL).
        if let Some((line_no, pat, excerpt)) = t01_hits.first()
            && let Some(sev) = cfg.resolve_severity("T01", Severity::Critical)
        {
            findings.push(AuditFinding {
                id: format!("T01:{}:{}", file.path, line_no.unwrap_or(0)),
                rule_id: "T01".into(),
                category: "test_skip".into(),
                severity: sev,
                file: Some(file.path.clone()),
                line: *line_no,
                title: format!(
                    "Test skip marker `{pat}` inserted ({} occurrence{})",
                    t01_hits.len(),
                    if t01_hits.len() == 1 { "" } else { "s" }
                ),
                detail: format!(
                    "Added test skip `{pat}` in `{}`: {}",
                    file.path,
                    truncate(excerpt, 140)
                ),
                evidence: excerpt.clone(),
            });
        }

        // T02: Assertion deletion or dummy assertion insertion (HIGH).
        if is_test && removed_assertions > added_assertions {
            let delta = removed_assertions - added_assertions;
            if let Some(sev) = cfg.resolve_severity("T02", Severity::High) {
                findings.push(AuditFinding {
                    id: format!("T02-del:{}:{}", file.path, first_removed_assert_line.unwrap_or(0)),
                    rule_id: "T02".into(),
                    category: "assertion_weakened".into(),
                    severity: sev,
                    file: Some(file.path.clone()),
                    line: first_removed_assert_line,
                    title: format!("Net {delta} assertion(s) removed in test file"),
                    detail: format!(
                        "`{}` removed {removed_assertions} assertion(s) and added only {added_assertions}.",
                        file.path
                    ),
                    evidence: format!("-{removed_assertions} / +{added_assertions} assertions"),
                });
            }
        }
        if let Some((line_no, pat, excerpt)) = dummy_hits.first()
            && let Some(sev) = cfg.resolve_severity("T02", Severity::High)
        {
            findings.push(AuditFinding {
                id: format!("T02-dummy:{}:{}", file.path, line_no.unwrap_or(0)),
                rule_id: "T02".into(),
                category: "dummy_assertion".into(),
                severity: sev,
                file: Some(file.path.clone()),
                line: *line_no,
                title: format!("Tautological dummy assertion `{pat}` added"),
                detail: format!("`{}` contains dummy assertion `{pat}`.", file.path),
                evidence: truncate(excerpt, 140),
            });
        }

        // T03: Unrequested existing test / snapshot modification (WARNING).
        if (is_test || is_snapshot)
            && !file.is_new
            && !prompt_mentions_tests
            && file.hunks.iter().any(|h| h.removed_count > 0)
            && let Some(sev) = cfg.resolve_severity("T03", Severity::Warning)
        {
            let first_line = file
                .hunks
                .first()
                .and_then(|h| h.lines.iter().find_map(|l| l.new_line.or(l.old_line)));
            findings.push(AuditFinding {
                id: format!("T03:{}:{}", file.path, first_line.unwrap_or(0)),
                rule_id: "T03".into(),
                category: "unrequested_test_edit".into(),
                severity: sev,
                file: Some(file.path.clone()),
                line: first_line,
                title: if is_snapshot {
                    "Existing snapshot expectation edited without prompt request".into()
                } else {
                    "Existing test modified without test keyword in prompt".into()
                },
                detail: format!(
                    "Existing {} `{}` was edited during a turn whose prompt did not ask to change tests.",
                    if is_snapshot { "snapshot" } else { "test file" },
                    file.path
                ),
                evidence: format!(
                    "+{} / -{} lines",
                    file.added_lines(),
                    file.removed_lines()
                ),
            });
        }
    }

    findings
}

fn is_assertion_line(trimmed: &str) -> bool {
    if trimmed.starts_with("//") || trimmed.starts_with('#') || trimmed.starts_with("/*") {
        return false;
    }
    trimmed.contains("assert")
        || trimmed.contains("expect(")
        || trimmed.contains(".should")
        || trimmed.contains("XCTAssert")
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}
