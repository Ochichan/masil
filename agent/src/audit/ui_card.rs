//! Renders `AuditReport` into Masil UI Spec v1 (`UiDocument`), concise
//! conversation timeline summary cards, and auto-assembled challenge prompts
//! (D-03).

use super::{AuditFinding, AuditReport, Severity};
use crate::plugin::runner::TimelineCardInput;
use crate::plugin::ui_spec::{
    UI_SPEC_VERSION, UiBlock, UiButton, UiDocument, UiListItem, UiMetric, UiTone,
};
use serde_json::json;

/// Builds a concise 1-line summary timeline card posted at turn completion.
pub fn build_timeline_summary_card(report: &AuditReport) -> TimelineCardInput {
    let critical = report.count_severity(Severity::Critical);
    let high = report.count_severity(Severity::High);
    let warning = report.count_severity(Severity::Warning);

    let (title, badge, tone) = if critical > 0 {
        (
            format!("🚨 BS 경고 {critical}건 · 카드 리뷰"),
            format!("CRITICAL {critical}"),
            UiTone::Danger,
        )
    } else if high > 0 {
        (
            format!("⚠️ 감사 주의 {high}건 · 카드 리뷰"),
            format!("HIGH {high}"),
            UiTone::Warning,
        )
    } else if warning > 0 {
        (
            format!("ℹ️ 감사 참고 {warning}건"),
            format!("RISK {}", report.risk_score),
            UiTone::Accent,
        )
    } else {
        (
            "✅ 검증됨".to_owned(),
            format!("RISK {}", report.risk_score),
            UiTone::Success,
        )
    };

    let subtitle = report
        .checkpoint_id
        .as_deref()
        .map(|cp| format!("Turn audit #{id} · checkpoint #{cp}", id = report.id))
        .unwrap_or_else(|| format!("Turn audit #{}", report.id));

    let mut blocks = vec![json!({
        "type": "header",
        "title": title,
        "subtitle": subtitle,
        "badge": badge,
        "tone": tone,
    })];

    let urgent: Vec<&AuditFinding> = report
        .findings
        .iter()
        .filter(|f| matches!(f.severity, Severity::Critical | Severity::High))
        .take(3)
        .collect();
    if !urgent.is_empty() {
        let items: Vec<_> = urgent
            .iter()
            .map(|f| {
                let loc = match (&f.file, f.line) {
                    (Some(file), Some(ln)) => format!("{file}:{ln}"),
                    (Some(file), None) => file.clone(),
                    _ => "turn".into(),
                };
                json!({
                    "id": f.id,
                    "title": format!("[{}] {}", f.rule_id, f.title),
                    "subtitle": loc,
                    "badge": f.severity.as_str().to_uppercase(),
                    "tone": if f.severity == Severity::Critical { "danger" } else { "warning" },
                })
            })
            .collect();
        blocks.push(json!({
            "type": "list",
            "items": items,
        }));
    }

    TimelineCardInput {
        id: format!("audit-{}", report.id),
        title,
        subtitle: Some(subtitle),
        blocks,
    }
}

/// Converts a full `AuditReport` into a `UiDocument` for TUI/Mobile rendering.
pub fn audit_to_ui_document(report: &AuditReport) -> UiDocument {
    let critical = report.count_severity(Severity::Critical);
    let high = report.count_severity(Severity::High);
    let other = report.findings.len().saturating_sub(critical + high);

    let verdict_tone = match report.verdict.as_str() {
        "flagged" => UiTone::Danger,
        "warnings" => UiTone::Warning,
        _ => UiTone::Success,
    };

    let mut blocks = vec![
        UiBlock::Header {
            title: format!("Turn Audit #{}", report.id),
            subtitle: report
                .checkpoint_id
                .as_deref()
                .map(|c| format!("checkpoint #{c}")),
            badge: Some(report.verdict.to_uppercase()),
            tone: verdict_tone,
        },
        UiBlock::MetricRow {
            items: vec![
                UiMetric {
                    label: "Risk".into(),
                    value: format!("{}/100", report.risk_score),
                    delta: None,
                    tone: if report.risk_score >= 60 {
                        UiTone::Danger
                    } else if report.risk_score >= 30 {
                        UiTone::Warning
                    } else {
                        UiTone::Success
                    },
                },
                UiMetric {
                    label: "Critical".into(),
                    value: critical.to_string(),
                    delta: None,
                    tone: if critical > 0 {
                        UiTone::Danger
                    } else {
                        UiTone::Muted
                    },
                },
                UiMetric {
                    label: "High".into(),
                    value: high.to_string(),
                    delta: None,
                    tone: if high > 0 {
                        UiTone::Warning
                    } else {
                        UiTone::Muted
                    },
                },
                UiMetric {
                    label: "Other".into(),
                    value: other.to_string(),
                    delta: None,
                    tone: UiTone::Muted,
                },
            ],
        },
    ];

    if !report.findings.is_empty() {
        let items = report
            .findings
            .iter()
            .map(|f| {
                let loc = match (&f.file, f.line) {
                    (Some(file), Some(ln)) => format!("{file}:{ln} — {}", f.detail),
                    (Some(file), None) => format!("{file} — {}", f.detail),
                    _ => f.detail.clone(),
                };
                UiListItem {
                    id: Some(f.id.clone()),
                    title: format!("[{}] {}", f.rule_id, f.title),
                    subtitle: Some(loc),
                    badge: Some(f.severity.as_str().to_uppercase()),
                    tone: match f.severity {
                        Severity::Critical => UiTone::Danger,
                        Severity::High => UiTone::Warning,
                        Severity::Warning => UiTone::Accent,
                        Severity::Info => UiTone::Muted,
                    },
                    action: Some("audit.challenge".into()),
                    payload: Some(json!({ "audit_id": report.id, "finding_id": f.id })),
                }
            })
            .collect();
        blocks.push(UiBlock::List {
            title: Some("Findings".into()),
            items,
        });
    }

    for hunk in &report.hunks {
        blocks.push(UiBlock::Divider);
        blocks.push(UiBlock::Header {
            title: format!("{}:{}", hunk.file, hunk.new_start),
            subtitle: Some(hunk.reason.clone()),
            badge: Some(format!("RISK {}", hunk.risk_score)),
            tone: if hunk.risk_score >= 60 {
                UiTone::Danger
            } else if hunk.risk_score >= 30 {
                UiTone::Warning
            } else {
                UiTone::Accent
            },
        });
        blocks.push(UiBlock::Diff {
            text: hunk.diff.clone(),
        });
        blocks.push(UiBlock::ButtonRow {
            buttons: vec![
                UiButton {
                    id: format!("challenge-{}", hunk.id),
                    label: "Challenge".into(),
                    style: "secondary".into(),
                    action: "audit.challenge".into(),
                    payload: Some(json!({ "audit_id": report.id, "file": hunk.file })),
                    confirm: None,
                },
                UiButton {
                    id: format!("rollback-{}", hunk.id),
                    label: "Rollback file".into(),
                    style: "danger".into(),
                    action: "audit.rollback_file".into(),
                    payload: Some(json!({
                        "audit_id": report.id,
                        "checkpoint_id": report.prev_checkpoint_id,
                        "path": hunk.file,
                    })),
                    confirm: Some(format!("Restore `{}` to pre-turn checkpoint?", hunk.file)),
                },
            ],
        });
    }

    UiDocument {
        v: UI_SPEC_VERSION,
        title: Some(format!("Turn Audit #{}", report.id)),
        subtitle: Some(format!("verdict: {}", report.verdict)),
        blocks,
        custom_surface: None,
    }
}

/// Builds a concise, evidence-grounded follow-up challenge prompt for the agent.
pub fn build_challenge_prompt(
    report: &AuditReport,
    finding_id: Option<&str>,
    file_hint: Option<&str>,
) -> String {
    if let Some(fid) = finding_id
        && let Some(finding) = report
            .findings
            .iter()
            .find(|f| f.id == fid || f.rule_id.eq_ignore_ascii_case(fid))
    {
        return format_finding_challenge(finding);
    }
    if let Some(file) = file_hint {
        if let Some(finding) = report
            .findings
            .iter()
            .find(|f| f.file.as_deref() == Some(file))
        {
            return format_finding_challenge(finding);
        }
        return format!(
            "[masil Audit] Please review your recent changes in `{file}` (risk score {}/100): explain why this edit is safe, verify edge cases, and re-run the relevant tests.",
            report.risk_score
        );
    }
    if let Some(top) = report.findings.first() {
        return format_finding_challenge(top);
    }
    format!(
        "[masil Audit] Please review your latest turn (audit #{}) and verify that all changed files pass tests without skipped assertions or suppressed errors.",
        report.id
    )
}

fn format_finding_challenge(f: &AuditFinding) -> String {
    let loc = match (&f.file, f.line) {
        (Some(file), Some(ln)) => format!("`{file}:{ln}`"),
        (Some(file), None) => format!("`{file}`"),
        _ => "your latest turn".to_owned(),
    };
    let instruction = match f.rule_id.as_str() {
        "T01" => {
            "Remove the skipped test marker, fix the underlying implementation, and re-run the test suite so it genuinely passes."
        }
        "T02" => {
            "Restore the removed or weakened test assertions, fix the implementation to satisfy them, and re-run the tests."
        }
        "T03" => {
            "Verify whether modifying existing test/snapshot expectations was truly required; revert unintended expectation changes and re-run tests."
        }
        "E01" => {
            "Actually execute the relevant test/build command now after your latest file edit and share the real exit status and output."
        }
        "G01" => {
            "Remove the newly added type/lint suppression and fix the underlying type or lint error properly."
        }
        "G02" => {
            "Do not swallow errors or introduce unguarded risky primitives; handle errors explicitly and explain any unsafe/process call."
        }
        "S01" => {
            "Double-check security and migration impact in this sensitive file and confirm tests cover the change."
        }
        _ => "Review this finding, fix the root cause, and re-run verification.",
    };
    format!(
        "[masil Audit · {}] At {}: {} ({}). {}",
        f.rule_id, loc, f.title, f.detail, instruction
    )
}
