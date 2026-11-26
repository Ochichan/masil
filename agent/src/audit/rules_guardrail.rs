//! Guardrail bypass, swallowed error, risky primitive, and sensitive path
//! rules (`G01`, `G02`, `S01`) for BS Detector (D-02).

use super::config::RepoAuditConfig;
use super::risk_score::DiffFile;
use super::{AuditFinding, Severity};

const LINT_BYPASS_PATTERNS: &[&str] = &[
    "@ts-ignore",
    "@ts-expect-error",
    "@ts-nocheck",
    "eslint-disable",
    "#[allow(",
    "# noqa",
    "# type: ignore",
    "//nolint",
];

const RISKY_PRIMITIVES: &[(&str, &str)] = &[
    ("unsafe {", "new `unsafe` block"),
    ("dangerouslySetInnerHTML", "`dangerouslySetInnerHTML` usage"),
    ("eval(", "dynamic `eval(` execution"),
    ("Command::new(", "process execution via `Command::new`"),
    ("child_process", "`child_process` process execution"),
];

pub fn check_guardrail_rules(files: &[DiffFile], cfg: &RepoAuditConfig) -> Vec<AuditFinding> {
    let mut findings = Vec::new();

    for file in files {
        if cfg.is_ignored(&file.path) {
            continue;
        }
        let is_test = cfg.is_test_file(&file.path);
        let mut bypass_hits: Vec<(Option<usize>, &str, String)> = Vec::new();
        let mut as_any_count = 0usize;
        let mut first_as_any_line: Option<usize> = None;
        let mut g02_hits: Vec<(Option<usize>, String, String)> = Vec::new();

        for hunk in &file.hunks {
            let added_lines: Vec<(Option<usize>, &str)> = hunk
                .lines
                .iter()
                .filter(|l| l.kind == '+')
                .map(|l| (l.new_line, l.content.trim()))
                .collect();

            for (idx, (line_no, text)) in added_lines.iter().enumerate() {
                if let Some(pat) = LINT_BYPASS_PATTERNS.iter().find(|p| text.contains(**p)) {
                    bypass_hits.push((*line_no, *pat, (*text).to_owned()));
                }
                if text.contains("as any") {
                    as_any_count += 1;
                    if first_as_any_line.is_none() {
                        first_as_any_line = *line_no;
                    }
                }

                // Swallowed errors (empty catch / except: pass).
                if is_swallowed_catch(text, added_lines.get(idx + 1).map(|(_, s)| *s)) {
                    g02_hits.push((
                        *line_no,
                        "Swallowed exception (`catch {}` / `except: pass`)".into(),
                        (*text).to_owned(),
                    ));
                }

                // Risky primitives (skip test files for Command::new / child_process noise).
                if !is_test {
                    for (needle, desc) in RISKY_PRIMITIVES {
                        if text.contains(*needle) {
                            g02_hits.push((
                                *line_no,
                                format!("Risky primitive: {desc}"),
                                (*text).to_owned(),
                            ));
                        }
                    }
                }
            }
        }

        // G01: Type / lint bypasses (WARNING).
        if let Some((line_no, pat, excerpt)) = bypass_hits.first()
            && let Some(sev) = cfg.resolve_severity("G01", Severity::Warning)
        {
            findings.push(AuditFinding {
                id: format!("G01:{}:{}", file.path, line_no.unwrap_or(0)),
                rule_id: "G01".into(),
                category: "lint_type_bypass".into(),
                severity: sev,
                file: Some(file.path.clone()),
                line: *line_no,
                title: format!(
                    "Type/lint suppression `{pat}` added ({} total)",
                    bypass_hits.len()
                ),
                detail: format!(
                    "Added suppression directive(s) in `{}` starting with `{pat}`.",
                    file.path
                ),
                evidence: truncate(excerpt, 140),
            });
        } else if as_any_count >= 2
            && let Some(sev) = cfg.resolve_severity("G01", Severity::Warning)
        {
            findings.push(AuditFinding {
                id: format!("G01-any:{}:{}", file.path, first_as_any_line.unwrap_or(0)),
                rule_id: "G01".into(),
                category: "lint_type_bypass".into(),
                severity: sev,
                file: Some(file.path.clone()),
                line: first_as_any_line,
                title: format!("Repeated `as any` casts added ({as_any_count} times)"),
                detail: format!(
                    "`{}` introduces {as_any_count} `as any` type bypasses in one turn.",
                    file.path
                ),
                evidence: format!("{as_any_count}x `as any`"),
            });
        }

        // G02: Swallowed errors / risky primitives (HIGH).
        if let Some((line_no, title, excerpt)) = g02_hits.first()
            && let Some(sev) = cfg.resolve_severity("G02", Severity::High)
        {
            findings.push(AuditFinding {
                id: format!("G02:{}:{}", file.path, line_no.unwrap_or(0)),
                rule_id: "G02".into(),
                category: "error_hiding_or_risky_primitive".into(),
                severity: sev,
                file: Some(file.path.clone()),
                line: *line_no,
                title: title.clone(),
                detail: format!(
                    "`{}` introduces {} safety-sensitive pattern(s): {}",
                    file.path,
                    g02_hits.len(),
                    title
                ),
                evidence: truncate(excerpt, 140),
            });
        }

        // S01: Sensitive area modification (INFO / HIGH).
        if let Some(default_sev) = cfg.is_sensitive_file(&file.path)
            && let Some(sev) = cfg.resolve_severity("S01", default_sev)
        {
            let first_line = file
                .hunks
                .first()
                .and_then(|h| h.lines.iter().find_map(|l| l.new_line.or(l.old_line)));
            findings.push(AuditFinding {
                id: format!("S01:{}", file.path),
                rule_id: "S01".into(),
                category: "sensitive_path".into(),
                severity: sev,
                file: Some(file.path.clone()),
                line: first_line,
                title: format!("Sensitive file modified: `{}`", file.path),
                detail: format!(
                    "Changes touch sensitive area `{}` (+{} / -{} lines).",
                    file.path,
                    file.added_lines(),
                    file.removed_lines()
                ),
                evidence: format!("+{} / -{} lines", file.added_lines(), file.removed_lines()),
            });
        }
    }

    findings
}

fn is_swallowed_catch(line: &str, next_line: Option<&str>) -> bool {
    let compact: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    if compact.contains("catch{}")
        || (compact.contains("catch(") && compact.contains("){}"))
        || compact == "except:pass"
        || (compact.starts_with("except") && compact.ends_with(":pass"))
    {
        return true;
    }
    if let Some(next) = next_line {
        let next_trim = next.trim();
        if (line.starts_with("except ") || line.starts_with("except:")) && next_trim == "pass" {
            return true;
        }
        if (line.contains("catch") && line.ends_with('{')) && next_trim == "}" {
            return true;
        }
    }
    false
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
