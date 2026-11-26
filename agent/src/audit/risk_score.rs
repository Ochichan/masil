//! Unified diff parser and Blast-Radius risk scorer (`0..100`) that extracts
//! the top-5 highest-risk hunks (<= 64 KiB JSON) for the Hybrid Risk Card Stack
//! (D-03).

use super::config::RepoAuditConfig;
use super::{AuditFinding, RiskHunk, Severity};

const MAX_TOP_HUNKS: usize = 5;
const MAX_HUNK_TEXT_CHARS: usize = 3_000;
pub(crate) const MAX_HUNKS_JSON_BYTES: usize = 60 * 1024;

#[derive(Clone, Debug)]
pub struct DiffLine {
    pub kind: char, // '+' | '-' | ' '
    pub old_line: Option<usize>,
    pub new_line: Option<usize>,
    pub content: String,
}

#[derive(Clone, Debug)]
pub struct DiffHunk {
    pub header: String,
    pub old_start: usize,
    pub new_start: usize,
    pub added_count: usize,
    pub removed_count: usize,
    pub lines: Vec<DiffLine>,
}

#[derive(Clone, Debug)]
pub struct DiffFile {
    pub path: String,
    pub is_new: bool,
    pub is_deleted: bool,
    pub hunks: Vec<DiffHunk>,
}

impl DiffFile {
    pub fn added_lines(&self) -> usize {
        self.hunks.iter().map(|h| h.added_count).sum()
    }

    pub fn removed_lines(&self) -> usize {
        self.hunks.iter().map(|h| h.removed_count).sum()
    }
}

/// Parses standard git unified diff output (`git diff` / `git diff-tree -p`).
pub fn parse_unified_diff(raw: &str) -> Vec<DiffFile> {
    let mut files: Vec<DiffFile> = Vec::new();
    let mut cur_file: Option<DiffFile> = None;
    let mut cur_hunk: Option<DiffHunk> = None;
    let mut old_ln: usize = 0;
    let mut new_ln: usize = 0;

    let flush_hunk = |file: &mut Option<DiffFile>, hunk: &mut Option<DiffHunk>| {
        if let (Some(f), Some(h)) = (file.as_mut(), hunk.take()) {
            f.hunks.push(h);
        }
    };
    let flush_file =
        |files: &mut Vec<DiffFile>, file: &mut Option<DiffFile>, hunk: &mut Option<DiffHunk>| {
            flush_hunk(file, hunk);
            if let Some(f) = file.take() {
                files.push(f);
            }
        };

    for raw_line in raw.lines() {
        if let Some(rest) = raw_line.strip_prefix("diff --git ") {
            flush_file(&mut files, &mut cur_file, &mut cur_hunk);
            let path = parse_diff_git_path(rest);
            cur_file = Some(DiffFile {
                path,
                is_new: false,
                is_deleted: false,
                hunks: Vec::new(),
            });
            continue;
        }
        if raw_line.starts_with("new file mode") {
            if let Some(f) = cur_file.as_mut() {
                f.is_new = true;
            }
            continue;
        }
        if raw_line.starts_with("deleted file mode") {
            if let Some(f) = cur_file.as_mut() {
                f.is_deleted = true;
            }
            continue;
        }
        if let Some(p) = raw_line.strip_prefix("+++ ") {
            let clean = p
                .trim()
                .trim_matches('"')
                .strip_prefix("b/")
                .unwrap_or(p.trim());
            if clean != "/dev/null"
                && !clean.is_empty()
                && let Some(f) = cur_file.as_mut()
            {
                f.path = clean.to_owned();
            }
            continue;
        }
        if raw_line.starts_with("--- ")
            || raw_line.starts_with("index ")
            || raw_line.starts_with("Binary files ")
        {
            continue;
        }
        if raw_line.starts_with("@@ ") {
            flush_hunk(&mut cur_file, &mut cur_hunk);
            let (os, ns) = parse_hunk_header(raw_line);
            old_ln = os;
            new_ln = ns;
            cur_hunk = Some(DiffHunk {
                header: raw_line.to_owned(),
                old_start: os,
                new_start: ns,
                added_count: 0,
                removed_count: 0,
                lines: Vec::new(),
            });
            continue;
        }
        if let Some(h) = cur_hunk.as_mut() {
            if let Some(content) = raw_line.strip_prefix('+') {
                h.added_count += 1;
                h.lines.push(DiffLine {
                    kind: '+',
                    old_line: None,
                    new_line: Some(new_ln),
                    content: content.to_owned(),
                });
                new_ln += 1;
            } else if let Some(content) = raw_line.strip_prefix('-') {
                h.removed_count += 1;
                h.lines.push(DiffLine {
                    kind: '-',
                    old_line: Some(old_ln),
                    new_line: None,
                    content: content.to_owned(),
                });
                old_ln += 1;
            } else if let Some(content) = raw_line.strip_prefix(' ') {
                h.lines.push(DiffLine {
                    kind: ' ',
                    old_line: Some(old_ln),
                    new_line: Some(new_ln),
                    content: content.to_owned(),
                });
                old_ln += 1;
                new_ln += 1;
            }
        }
    }
    flush_file(&mut files, &mut cur_file, &mut cur_hunk);
    files
}

fn parse_diff_git_path(rest: &str) -> String {
    // Handle `a/foo b/foo` cleanly (taking the `b/` side).
    if let Some((_, b_side)) = rest.rsplit_once(" b/") {
        return b_side.trim().trim_matches('"').to_owned();
    }
    rest.split_whitespace()
        .last()
        .unwrap_or("")
        .trim_matches('"')
        .trim_start_matches("b/")
        .to_owned()
}

fn parse_hunk_header(line: &str) -> (usize, usize) {
    // Format: `@@ -12,7 +14,9 @@ optional context`
    let mut old_start = 1usize;
    let mut new_start = 1usize;
    for tok in line.split_whitespace() {
        if let Some(r) = tok.strip_prefix('-') {
            if let Some(n) = r.split(',').next().and_then(|s| s.parse::<usize>().ok()) {
                old_start = n.max(1);
            }
        } else if let Some(r) = tok.strip_prefix('+')
            && let Some(n) = r.split(',').next().and_then(|s| s.parse::<usize>().ok())
        {
            new_start = n.max(1);
        }
    }
    (old_start, new_start)
}

/// Computes overall turn risk score (`0..=100`) and top-5 Blast-Radius hunks.
pub fn score_turn_and_rank_hunks(
    files: &[DiffFile],
    findings: &[AuditFinding],
    cfg: &RepoAuditConfig,
) -> (u8, Vec<RiskHunk>) {
    let mut candidates: Vec<RiskHunk> = Vec::new();
    let mut max_hunk_score: u32 = 0;

    for file in files {
        if cfg.is_ignored(&file.path) {
            continue;
        }
        let file_findings: Vec<&AuditFinding> = findings
            .iter()
            .filter(|f| f.file.as_deref() == Some(file.path.as_str()))
            .collect();
        let file_finding_bonus: u32 = file_findings
            .iter()
            .map(|f| match f.severity {
                Severity::Critical => 45,
                Severity::High => 28,
                Severity::Warning => 12,
                Severity::Info => 6,
            })
            .sum();
        let sensitive_bonus: u32 = match cfg.is_sensitive_file(&file.path) {
            Some(Severity::High | Severity::Critical) => 24,
            Some(_) => 10,
            None => 0,
        };

        for (hunk_idx, hunk) in file.hunks.iter().enumerate() {
            let churn = hunk.added_count + hunk.removed_count;
            let churn_score = ((churn as u32).min(60) * 20) / 60;
            let api_bonus: u32 = if touches_public_api(hunk) { 12 } else { 0 };
            let raw = (file_finding_bonus + sensitive_bonus + churn_score + api_bonus).min(100);
            max_hunk_score = max_hunk_score.max(raw);

            let mut reasons = Vec::new();
            if !file_findings.is_empty() {
                let ids: Vec<&str> = file_findings.iter().map(|f| f.rule_id.as_str()).collect();
                reasons.push(format!("rules {}", ids.join(",")));
            }
            if sensitive_bonus > 0 {
                reasons.push("sensitive area".into());
            }
            if api_bonus > 0 {
                reasons.push("exported symbol".into());
            }
            if churn >= 25 {
                reasons.push(format!(
                    "high churn (+{}/-{})",
                    hunk.added_count, hunk.removed_count
                ));
            }
            if reasons.is_empty() {
                reasons.push(format!(
                    "+{}/-{} lines",
                    hunk.added_count, hunk.removed_count
                ));
            }

            let mut diff_text = String::new();
            diff_text.push_str(&hunk.header);
            diff_text.push('\n');
            for l in &hunk.lines {
                diff_text.push(l.kind);
                diff_text.push_str(&l.content);
                diff_text.push('\n');
                if diff_text.len() >= MAX_HUNK_TEXT_CHARS {
                    diff_text.truncate(MAX_HUNK_TEXT_CHARS);
                    diff_text.push_str("\n…(truncated)");
                    break;
                }
            }

            candidates.push(RiskHunk {
                id: format!("hunk:{}:{}", file.path, hunk_idx),
                file: file.path.clone(),
                old_start: hunk.old_start,
                new_start: hunk.new_start,
                added: hunk.added_count,
                removed: hunk.removed_count,
                risk_score: raw as u8,
                reason: reasons.join(" · "),
                diff: diff_text,
            });
        }
    }

    candidates.sort_by(|a, b| {
        b.risk_score
            .cmp(&a.risk_score)
            .then_with(|| (b.added + b.removed).cmp(&(a.added + a.removed)))
            .then_with(|| a.file.cmp(&b.file))
    });
    candidates.truncate(MAX_TOP_HUNKS);

    // Enforce <= 64 KiB serialized hunks cap.
    while !candidates.is_empty()
        && serde_json::to_vec(&candidates)
            .map(|b| b.len())
            .unwrap_or(0)
            > MAX_HUNKS_JSON_BYTES
    {
        candidates.pop();
    }

    let global_finding_score: u32 = findings
        .iter()
        .map(|f| match f.severity {
            Severity::Critical => 50,
            Severity::High => 25,
            Severity::Warning => 10,
            Severity::Info => 4,
        })
        .sum();
    let total_risk = max_hunk_score.max(global_finding_score).min(100) as u8;
    (total_risk, candidates)
}

fn touches_public_api(hunk: &DiffHunk) -> bool {
    hunk.lines.iter().any(|l| {
        if l.kind == ' ' {
            return false;
        }
        let t = l.content.trim();
        t.starts_with("pub fn ")
            || t.starts_with("pub struct ")
            || t.starts_with("pub enum ")
            || t.starts_with("pub trait ")
            || t.starts_with("export function ")
            || t.starts_with("export class ")
            || t.starts_with("export interface ")
            || t.starts_with("export type ")
            || t.starts_with("export const ")
            || t.starts_with("def ")
            || t.starts_with("class ")
    })
}
