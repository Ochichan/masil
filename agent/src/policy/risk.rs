//! Deterministic Blast-Radius risk scoring for interactive permission prompts
//! across both structured HTTP APIs and live TUI screens (Phase 3A, D-10).

use masil_wire::rpc::{
    InteractivePrompt, InteractivePromptKind, PromptRiskAssessment, PromptRiskTier,
};
use sha2::{Digest, Sha256};
use std::path::Path;

/// Critical secret paths or system roots that always trigger `Critical` tier.
const CRITICAL_PATH_TOKENS: &[&str] = &[
    "~/.ssh",
    "/.ssh/",
    "/.ssh",
    "~/.aws",
    "/.aws/",
    "/.aws",
    "~/.gnupg",
    "/.gnupg/",
    "~/.kube",
    "/.kube/",
    "~/.netrc",
    "/.netrc",
    "~/.config/gh",
    "/.config/gh/",
    "/etc/passwd",
    "/etc/shadow",
    "/etc/sudoers",
    "/etc/hosts",
    ".env",
    "id_rsa",
    "id_ed25519",
    "credentials.json",
    "secrets.yaml",
    "secrets.json",
];

/// Destructive command signatures that always trigger `Critical` tier.
const CRITICAL_CMD_PATTERNS: &[(&str, &str)] = &[
    ("rm -rf", "recursive forced deletion (`rm -rf`)"),
    ("rm -fr", "recursive forced deletion (`rm -fr`)"),
    ("rm -r ", "recursive deletion (`rm -r`)"),
    ("git push --force", "forced git push"),
    ("git push -f", "forced git push"),
    ("git reset --hard", "destructive git reset --hard"),
    ("git clean -fd", "destructive git clean"),
    ("chmod 777", "world-writable chmod 777"),
    ("chmod -r 777", "recursive chmod 777"),
    ("mkfs", "filesystem format command"),
    ("dd if=", "raw disk write (`dd`)"),
    ("drop table", "SQL DROP TABLE"),
    ("drop database", "SQL DROP DATABASE"),
    ("truncate table", "SQL TRUNCATE TABLE"),
    ("sudo ", "elevated privilege (`sudo`)"),
    ("su -", "privilege switch (`su`)"),
];

/// High-risk external / network / infrastructure command prefixes.
const HIGH_CMD_PREFIXES: &[(&str, &str)] = &[
    ("git push", "remote repository push"),
    ("docker ", "container engine command"),
    ("docker-compose", "container orchestration command"),
    ("kubectl ", "Kubernetes cluster command"),
    ("terraform ", "infrastructure mutation command"),
    ("psql ", "PostgreSQL client command"),
    ("mysql ", "MySQL client command"),
    ("redis-cli", "Redis client command"),
    ("mongosh", "MongoDB client command"),
    ("ssh ", "remote SSH invocation"),
    ("scp ", "remote file copy (`scp`)"),
    ("rsync ", "remote sync (`rsync`)"),
    ("aws ", "AWS CLI invocation"),
    ("gcloud ", "Google Cloud CLI invocation"),
    ("az ", "Azure CLI invocation"),
    ("gh pr merge", "GitHub PR merge"),
    ("gh release", "GitHub release action"),
    ("npm publish", "package registry publish"),
    ("cargo publish", "crates.io publish"),
];

/// Safe read-only / test / lint command prefixes (Low risk inside worktree).
const LOW_CMD_PREFIXES: &[&str] = &[
    "git status",
    "git diff",
    "git log",
    "git show",
    "git branch",
    "git rev-parse",
    "git ls-files",
    "cargo test",
    "cargo check",
    "cargo clippy",
    "cargo fmt --check",
    "npm test",
    "npm run test",
    "npm run typecheck",
    "npm run lint",
    "pnpm test",
    "pnpm run typecheck",
    "yarn test",
    "npx vitest",
    "vitest",
    "jest",
    "pytest",
    "python3 -m unittest",
    "python -m unittest",
    "go test",
    "go vet",
    "ruff check",
    "eslint",
    "tsc --noemit",
    "ls",
    "cat",
    "head",
    "tail",
    "rg",
    "grep",
    "find",
    "fd",
    "pwd",
    "which",
    "wc",
    "stat",
    "file",
    "echo",
    "tree",
];

/// Medium-risk local worktree write / build / dependency commands.
const MEDIUM_CMD_PREFIXES: &[&str] = &[
    "npm install",
    "npm ci",
    "pnpm install",
    "yarn install",
    "yarn add",
    "pip install",
    "uv pip install",
    "cargo build",
    "cargo add",
    "cargo fmt",
    "git add",
    "git commit",
    "git checkout",
    "git switch",
    "git stash",
    "git merge",
    "git rebase",
    "mkdir",
    "touch",
    "cp ",
    "mv ",
    "make ",
    "python3 scripts/build.py",
];

/// Computes a stable SHA-256 hex digest over the interactive prompt identity,
/// title, body, and option list for Guard #3 re-verification.
pub fn compute_prompt_digest(prompt: &InteractivePrompt) -> String {
    let mut hasher = Sha256::new();
    hasher.update(prompt.id.as_bytes());
    hasher.update(b"\x1f");
    hasher.update(format!("{:?}", prompt.kind).as_bytes());
    hasher.update(b"\x1f");
    hasher.update(prompt.title.as_bytes());
    hasher.update(b"\x1f");
    hasher.update(prompt.body.as_bytes());
    for opt in &prompt.options {
        hasher.update(b"\x1e");
        hasher.update(opt.index.to_string().as_bytes());
        hasher.update(b":");
        hasher.update(opt.label.as_bytes());
        hasher.update(b":");
        if let Some(hk) = &opt.hotkey {
            hasher.update(hk.as_bytes());
        }
        hasher.update(if opt.is_destructive { b":D" } else { b":S" });
    }
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Detects whether a command string appears visually truncated by TUI width limits.
pub fn is_command_truncated(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.contains('…')
        || trimmed.contains('⋯')
        || trimmed.ends_with("...")
        || trimmed.starts_with("...")
        || trimmed.to_ascii_lowercase().contains("[truncated]")
    {
        return true;
    }
    // `...` inside a shell command that is NOT a git range (`a...b`) or JS spread (`...args`)
    // e.g. `cargo test ... --nocapture`
    if trimmed.contains(" ... ") {
        return true;
    }
    false
}

/// Extracts the primary command or tool payload from an `InteractivePrompt`.
pub fn extract_prompt_command(prompt: &InteractivePrompt) -> Option<String> {
    // Look for explicit `command:` / `bash:` / `$ ` lines in body first.
    for raw_line in prompt.body.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        let lower = line.to_ascii_lowercase();
        for prefix in ["command:", "bash:", "shell:", "cmd:", "run:"] {
            if lower.starts_with(prefix) {
                let rest = line[prefix.len()..].trim();
                if !rest.is_empty() {
                    return Some(rest.to_owned());
                }
            }
        }
        if let Some(rest) = line.strip_prefix("$ ")
            && !rest.trim().is_empty()
        {
            return Some(rest.trim().to_owned());
        }
    }
    // Check title if it names a tool/command like `Bash(cargo test)` or `Permission · bash`
    let title = prompt.title.trim();
    if let Some(start) = title.find('(')
        && let Some(end) = title.rfind(')')
        && end > start + 1
    {
        let inside = title[start + 1..end].trim();
        if !inside.is_empty() {
            return Some(inside.to_owned());
        }
    }
    // Fallback to first non-empty body line when it is a Permission prompt.
    if prompt.kind == InteractivePromptKind::Permission {
        if let Some(first) = prompt.body.lines().map(str::trim).find(|l| !l.is_empty()) {
            return Some(first.to_owned());
        }
        if !title.is_empty() {
            return Some(title.to_owned());
        }
    }
    None
}

/// Evaluates Blast-Radius risk for a raw command or path string.
pub fn assess_command_risk(command: &str, cwd: Option<&str>) -> PromptRiskAssessment {
    let trimmed = command.trim();
    let truncated = is_command_truncated(trimmed);
    let mut tier = PromptRiskTier::Medium;
    let mut score: u8 = 45;
    let mut reasons: Vec<String> = Vec::new();

    if trimmed.is_empty() {
        return PromptRiskAssessment {
            tier: PromptRiskTier::Medium,
            label: PromptRiskTier::Medium.label().to_owned(),
            score: 50,
            reasons: vec!["empty or unrecognized command".into()],
            command: None,
            truncated: false,
            matched_rule_id: None,
            matched_action: None,
            batch_eligible: false,
            prompt_digest: sha256_hex(""),
        };
    }

    let lower_full = trimmed.to_ascii_lowercase();

    // 1. Check pipe-to-shell patterns (`curl ... | sh`, `wget ... | bash`).
    if (lower_full.contains("curl ") || lower_full.contains("wget "))
        && (lower_full.contains("| sh")
            || lower_full.contains("| bash")
            || lower_full.contains("| zsh")
            || lower_full.contains("|sh")
            || lower_full.contains("|bash"))
    {
        bump(
            &mut tier,
            &mut score,
            PromptRiskTier::Critical,
            98,
            &mut reasons,
            "remote script piped directly to shell".into(),
        );
    }

    // 2. Check critical secret paths & destructive commands across the whole string.
    for token in CRITICAL_PATH_TOKENS {
        if contains_path_token(&lower_full, token) {
            bump(
                &mut tier,
                &mut score,
                PromptRiskTier::Critical,
                95,
                &mut reasons,
                format!("references protected secret or system path `{token}`"),
            );
        }
    }
    for (pat, desc) in CRITICAL_CMD_PATTERNS {
        if lower_full.contains(pat) {
            bump(
                &mut tier,
                &mut score,
                PromptRiskTier::Critical,
                96,
                &mut reasons,
                (*desc).to_owned(),
            );
        }
    }

    // 3. Split compound shell commands (`&&`, `||`, `;`, `|`) and score every segment;
    //    overall tier is the MAXIMUM risk across all segments!
    let segments = split_shell_segments(trimmed);
    let mut all_segments_low = !segments.is_empty();

    for seg in &segments {
        let seg_trimmed = seg.trim();
        if seg_trimmed.is_empty() {
            continue;
        }
        let seg_lower = seg_trimmed.to_ascii_lowercase();

        // Check command substitution `$(...)` or backticks -> at least High.
        if seg_trimmed.contains("$(") || seg_trimmed.contains('`') {
            all_segments_low = false;
            bump(
                &mut tier,
                &mut score,
                PromptRiskTier::High,
                78,
                &mut reasons,
                "contains dynamic shell command substitution".into(),
            );
        }

        // Check high-risk external/network commands.
        if seg_lower.starts_with("curl ") || seg_lower.starts_with("wget ") {
            all_segments_low = false;
            let upload = seg_lower.contains(" -x post")
                || seg_lower.contains(" -x put")
                || seg_lower.contains(" -d ")
                || seg_lower.contains(" --data")
                || seg_lower.contains(" -f ")
                || seg_lower.contains(" --upload-file");
            bump(
                &mut tier,
                &mut score,
                PromptRiskTier::High,
                if upload { 85 } else { 72 },
                &mut reasons,
                if upload {
                    "network data upload via curl/wget".into()
                } else {
                    "external network request via curl/wget".into()
                },
            );
            continue;
        }

        let mut seg_high = false;
        for (prefix, desc) in HIGH_CMD_PREFIXES {
            if seg_lower == prefix.trim() || seg_lower.starts_with(prefix) {
                all_segments_low = false;
                seg_high = true;
                bump(
                    &mut tier,
                    &mut score,
                    PromptRiskTier::High,
                    76,
                    &mut reasons,
                    (*desc).to_owned(),
                );
                break;
            }
        }
        if seg_high {
            continue;
        }

        // Check out-of-worktree path references (`../` or absolute paths outside cwd).
        if let Some(outside) = find_outside_worktree_path(seg_trimmed, cwd) {
            all_segments_low = false;
            let is_write_like = seg_lower.contains('>')
                || seg_lower.starts_with("rm ")
                || seg_lower.starts_with("mv ")
                || seg_lower.starts_with("cp ")
                || seg_lower.starts_with("tee ")
                || seg_lower.starts_with("chmod ")
                || seg_lower.starts_with("sed -i");
            if is_write_like {
                bump(
                    &mut tier,
                    &mut score,
                    PromptRiskTier::High,
                    80,
                    &mut reasons,
                    format!("writes outside active worktree (`{outside}`)"),
                );
            } else {
                bump(
                    &mut tier,
                    &mut score,
                    PromptRiskTier::High,
                    70,
                    &mut reasons,
                    format!("accesses path outside active worktree (`{outside}`)"),
                );
            }
            continue;
        }

        // Shell redirection (`>`, `>>`) or in-place sed makes a command at least Medium.
        if seg_trimmed.contains('>') || seg_lower.contains("sed -i") {
            all_segments_low = false;
            bump(
                &mut tier,
                &mut score,
                PromptRiskTier::Medium,
                50,
                &mut reasons,
                "writes to files via shell redirection or in-place edit".into(),
            );
            continue;
        }

        // Check medium-risk local write/build/install prefixes.
        if MEDIUM_CMD_PREFIXES
            .iter()
            .any(|p| seg_lower == p.trim() || seg_lower.starts_with(p))
        {
            all_segments_low = false;
            bump(
                &mut tier,
                &mut score,
                PromptRiskTier::Medium,
                45,
                &mut reasons,
                "modifies worktree files or dependencies".into(),
            );
            continue;
        }

        // Check read-only / test / lint prefixes.
        let is_low = LOW_CMD_PREFIXES.iter().any(|p| {
            seg_lower == *p
                || seg_lower.starts_with(&format!("{p} "))
                || seg_lower.starts_with(&format!("{p}\t"))
        }) || seg_lower.starts_with("read:")
            || seg_lower.starts_with("glob:")
            || seg_lower.starts_with("grep:");

        if !is_low {
            all_segments_low = false;
        }
    }

    if all_segments_low && tier == PromptRiskTier::Medium && reasons.is_empty() {
        tier = PromptRiskTier::Low;
        score = 15;
        reasons.push("read-only inspection or worktree test/lint command".into());
    } else if reasons.is_empty() {
        reasons.push("local worktree execution".into());
    }

    if truncated {
        reasons.push("command appears visually truncated (auto-approve blocked)".into());
        if tier == PromptRiskTier::Low {
            score = score.max(35);
        }
    }

    PromptRiskAssessment {
        tier,
        label: tier.label().to_owned(),
        score,
        reasons,
        command: Some(trimmed.to_owned()),
        truncated,
        matched_rule_id: None,
        matched_action: None,
        batch_eligible: tier == PromptRiskTier::Low && !truncated,
        prompt_digest: sha256_hex(trimmed),
    }
}

/// Assesses an `InteractivePrompt` (Structured API or Live TUI).
pub fn assess_prompt(prompt: &InteractivePrompt, cwd: Option<&str>) -> PromptRiskAssessment {
    let digest = compute_prompt_digest(prompt);

    // Non-permission questions/inputs require human judgment; never batch-approve.
    if matches!(
        prompt.kind,
        InteractivePromptKind::Question | InteractivePromptKind::Input
    ) {
        return PromptRiskAssessment {
            tier: PromptRiskTier::Medium,
            label: PromptRiskTier::Medium.label().to_owned(),
            score: 40,
            reasons: vec!["interactive question requires human answer".into()],
            command: None,
            truncated: false,
            matched_rule_id: None,
            matched_action: None,
            batch_eligible: false,
            prompt_digest: digest,
        };
    }

    let extracted = extract_prompt_command(prompt);
    let mut assessment = match extracted.as_deref() {
        Some(cmd) => assess_command_risk(cmd, cwd),
        None => {
            // Check body/title directly if no command line extracted.
            let combined = format!("{} {}", prompt.title, prompt.body);
            let mut a = assess_command_risk(&combined, cwd);
            // If generic fallback with no explicit safe read command, never classify as Low.
            if prompt.id.starts_with("fallback") && a.tier == PromptRiskTier::Low {
                a.tier = PromptRiskTier::Medium;
                a.label = PromptRiskTier::Medium.label().to_owned();
                a.score = 50;
                a.batch_eligible = false;
                a.reasons
                    .push("unparsed fallback prompt requires manual review".into());
            }
            a
        }
    };

    // Also inspect full body for secret paths or destructive operations even when
    // `extract_prompt_command` picked a single line (e.g. `edit: ~/.ssh/config`).
    let body_lower = format!("{}\n{}", prompt.title, prompt.body).to_ascii_lowercase();
    for token in CRITICAL_PATH_TOKENS {
        if contains_path_token(&body_lower, token) {
            bump(
                &mut assessment.tier,
                &mut assessment.score,
                PromptRiskTier::Critical,
                95,
                &mut assessment.reasons,
                format!("prompt references protected path `{token}`"),
            );
            assessment.label = assessment.tier.label().to_owned();
        }
    }
    if assessment.tier == PromptRiskTier::Low
        && (prompt.title.to_ascii_lowercase().contains("edit")
            || prompt.title.to_ascii_lowercase().contains("write")
            || body_lower.starts_with("edit:")
            || body_lower.starts_with("write:"))
    {
        assessment.tier = PromptRiskTier::Medium;
        assessment.label = PromptRiskTier::Medium.label().to_owned();
        assessment.score = assessment.score.max(45);
        assessment
            .reasons
            .push("file edit/write inside worktree".into());
    }

    assessment.batch_eligible = assessment.tier == PromptRiskTier::Low
        && !assessment.truncated
        && !prompt.id.starts_with("fallback");
    assessment.prompt_digest = digest;
    assessment
}

fn bump(
    tier: &mut PromptRiskTier,
    score: &mut u8,
    candidate_tier: PromptRiskTier,
    candidate_score: u8,
    reasons: &mut Vec<String>,
    reason: String,
) {
    if candidate_tier > *tier {
        *tier = candidate_tier;
    }
    if candidate_score > *score {
        *score = candidate_score;
    }
    if !reasons.iter().any(|r| r == &reason) {
        reasons.push(reason);
    }
}

fn contains_path_token(lower_text: &str, token: &str) -> bool {
    if token == ".env" {
        // Match `.env`, `.env.local`, `.env.production`, etc. as path or token boundaries
        // without falsely matching `environment.rs`.
        for (idx, _) in lower_text.match_indices(".env") {
            let prev = lower_text[..idx].chars().next_back();
            let after = lower_text[idx + 4..].chars().next();
            let prev_ok =
                prev.is_none_or(|c| matches!(c, ' ' | '\t' | '\n' | '/' | '"' | '\'' | ':' | '='));
            let after_ok = after.is_none_or(|c| {
                matches!(c, ' ' | '\t' | '\n' | '.' | '"' | '\'' | ',' | ';' | ')')
            });
            if prev_ok && after_ok {
                return true;
            }
        }
        return false;
    }
    lower_text.contains(token)
}

fn split_shell_segments(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let chars: Vec<char> = cmd.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if ch == '\'' && !in_double {
            in_single = !in_single;
            cur.push(ch);
            i += 1;
            continue;
        }
        if ch == '"' && !in_single {
            in_double = !in_double;
            cur.push(ch);
            i += 1;
            continue;
        }
        if !in_single && !in_double {
            if ch == ';' {
                out.push(std::mem::take(&mut cur));
                i += 1;
                continue;
            }
            if (ch == '&' && chars.get(i + 1) == Some(&'&'))
                || (ch == '|' && chars.get(i + 1) == Some(&'|'))
            {
                out.push(std::mem::take(&mut cur));
                i += 2;
                continue;
            }
            if ch == '|' {
                out.push(std::mem::take(&mut cur));
                i += 1;
                continue;
            }
        }
        cur.push(ch);
        i += 1;
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

fn find_outside_worktree_path(segment: &str, cwd: Option<&str>) -> Option<String> {
    let canonical_cwd = cwd.and_then(|c| Path::new(c).canonicalize().ok());
    for raw_tok in segment.split_whitespace() {
        let tok = raw_tok.trim_matches(|c| matches!(c, '"' | '\'' | ',' | ';'));
        if tok.is_empty() || tok.starts_with('-') || tok.contains("://") {
            continue;
        }
        if tok == ".." || tok.starts_with("../") || tok.contains("/../") {
            return Some(tok.to_owned());
        }
        if tok.starts_with("~/") {
            return Some(tok.to_owned());
        }
        if tok.starts_with('/') {
            // Ignore benign system paths / dev null / standard tool binaries
            if tok.starts_with("/dev/")
                || tok.starts_with("/usr/")
                || tok.starts_with("/bin/")
                || tok.starts_with("/opt/")
                || tok.starts_with("/System/")
                || tok.starts_with("/Library/")
            {
                continue;
            }
            if let Some(ref root) = canonical_cwd {
                let candidate = Path::new(tok)
                    .canonicalize()
                    .unwrap_or_else(|_| Path::new(tok).to_path_buf());
                if candidate.starts_with(root) {
                    continue;
                }
                return Some(tok.to_owned());
            } else if tok.starts_with("/Users/")
                || tok.starts_with("/home/")
                || tok.starts_with("/etc/")
            {
                return Some(tok.to_owned());
            }
        }
    }
    None
}

fn sha256_hex(input: &str) -> String {
    Sha256::digest(input.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
