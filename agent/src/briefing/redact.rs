//! Deterministic secret & local-path redaction shared by Phase 2B Live Activity
//! opt-in 1-line summaries (D-09) and Phase 3B PR Verification HTML traces.
//!
//! Zero external regex crates required — pure linear scans for high-signal secret
//! formats (`sk-*`, `ghp_*`, `AKIA*`, JWTs, `Bearer *`, Slack tokens, PEM blocks,
//! `.env` assignments, and home directory paths).

use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RedactionResult {
    pub text: String,
    pub total_redactions: usize,
    pub kinds: Vec<String>,
}

/// Redacts secrets and home-directory paths from `input`, returning the masked
/// text alongside exact counts and categories.
pub fn redact_secrets(input: &str) -> RedactionResult {
    let mut text = input.to_owned();
    let mut total = 0usize;
    let mut kinds: Vec<String> = Vec::new();

    let mut record = |kind: &str, count: usize| {
        if count > 0 {
            total += count;
            if !kinds.iter().any(|k| k == kind) {
                kinds.push(kind.to_owned());
            }
        }
    };

    // 1. PEM / SSH private key blocks.
    let (next, c) = redact_pem_blocks(&text);
    text = next;
    record("private_key", c);

    // 2. Bearer tokens (`Bearer <token>`).
    let (next, c) = redact_bearer_tokens(&text);
    text = next;
    record("bearer_token", c);

    // 3. Well-known prefixed API keys & tokens.
    for (prefix, min_tail, kind) in [
        ("sk-ant-", 16usize, "api_key"),
        ("sk-proj-", 16, "api_key"),
        ("sk-", 16, "api_key"),
        ("ghp_", 20, "github_token"),
        ("gho_", 20, "github_token"),
        ("ghu_", 20, "github_token"),
        ("ghs_", 20, "github_token"),
        ("github_pat_", 20, "github_token"),
        ("AKIA", 16, "aws_access_key"),
        ("ASIA", 16, "aws_access_key"),
        ("xoxb-", 12, "slack_token"),
        ("xoxp-", 12, "slack_token"),
        ("xoxa-", 12, "slack_token"),
        ("xoxs-", 12, "slack_token"),
        ("AIza", 20, "google_api_key"),
    ] {
        let (next, c) = redact_prefixed_token(&text, prefix, min_tail, kind);
        text = next;
        record(kind, c);
    }

    // 4. JWT (`eyJ<base64url>.eyJ<base64url>.<sig>`).
    let (next, c) = redact_jwts(&text);
    text = next;
    record("jwt", c);

    // 5. `.env`-style secret assignments (`API_KEY=...`, `SECRET=...`, `PASSWORD=...`, `TOKEN=...`).
    let (next, c) = redact_env_assignments(&text);
    text = next;
    record("env_secret", c);

    // 6. Local home directory paths (`/Users/<user>` or `/home/<user>`).
    let (next, c) = redact_home_paths(&text);
    text = next;
    record("home_path", c);

    RedactionResult {
        text,
        total_redactions: total,
        kinds,
    }
}

/// Strict secret redaction for Phase 3B PR Verification HTML traces (`--redact-strict`):
/// runs all deterministic `redact_secrets` patterns PLUS high-entropy secret token
/// scanning (mixed-case alphanumeric/symbol tokens ≥ 28 chars with Shannon entropy ≥ 4.0).
pub fn redact_secrets_strict(input: &str) -> RedactionResult {
    let mut res = redact_secrets(input);
    let (next, count) = redact_high_entropy_tokens(&res.text);
    res.text = next;
    if count > 0 {
        res.total_redactions += count;
        if !res.kinds.iter().any(|k| k == "high_entropy_secret") {
            res.kinds.push("high_entropy_secret".into());
        }
    }
    res
}

fn redact_high_entropy_tokens(input: &str) -> (String, usize) {
    let mut out = String::with_capacity(input.len());
    let mut count = 0usize;
    let bytes = input.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'+' | b'/') {
            let start = i;
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric()
                    || matches!(bytes[i], b'_' | b'-' | b'+' | b'/' | b'='))
            {
                i += 1;
            }
            let tok = &input[start..i];
            if looks_high_entropy_secret(tok) {
                out.push_str("[REDACTED:high_entropy_secret]");
                count += 1;
            } else {
                out.push_str(tok);
            }
        } else {
            let ch = input[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    (out, count)
}

fn looks_high_entropy_secret(tok: &str) -> bool {
    if tok.len() < 28 || tok.len() > 256 {
        return false;
    }
    // Skip file paths, URLs, and already-redacted placeholders.
    if tok.contains('/') && tok.matches('/').count() >= 2 {
        return false;
    }
    if tok.starts_with("REDACTED") {
        return false;
    }
    // Skip pure lowercase hex (git commit SHAs are 40 lowercase hex chars).
    if tok
        .bytes()
        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return false;
    }
    // Require at least 3 character classes (upper, lower, digit/symbol) to avoid prose/identifiers.
    let has_upper = tok.bytes().any(|b| b.is_ascii_uppercase());
    let has_lower = tok.bytes().any(|b| b.is_ascii_lowercase());
    let has_digit = tok.bytes().any(|b| b.is_ascii_digit());
    if !(has_upper && has_lower && has_digit) {
        return false;
    }
    shannon_entropy(tok.as_bytes()) >= 4.0
}

fn shannon_entropy(bytes: &[u8]) -> f64 {
    if bytes.is_empty() {
        return 0.0;
    }
    let mut freq = [0u32; 256];
    for &b in bytes {
        freq[b as usize] += 1;
    }
    let len = bytes.len() as f64;
    let mut h = 0.0f64;
    for &c in &freq {
        if c > 0 {
            let p = f64::from(c) / len;
            h -= p * p.log2();
        }
    }
    h
}

/// Sanitizes and redacts an opt-in Live Activity 1-line summary (D-09):
/// - Passes through `redact_secrets`
/// - Collapses whitespace to single line
/// - Truncates to `max_chars` (60 chars for APNs Live Activity)
pub fn redact_live_activity_summary(raw: &str, max_chars: usize) -> Option<String> {
    let redacted = redact_secrets(raw).text;
    let single_line = redacted
        .replace(['\r', '\n', '\t'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if single_line.is_empty() {
        return None;
    }
    let count = single_line.chars().count();
    if count <= max_chars {
        Some(single_line)
    } else if max_chars > 1 {
        let mut s: String = single_line.chars().take(max_chars - 1).collect();
        s.push('…');
        Some(s)
    } else {
        Some(single_line.chars().take(max_chars).collect())
    }
}

fn is_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.')
}

fn redact_pem_blocks(input: &str) -> (String, usize) {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    let mut count = 0usize;
    while let Some(start) = rest.find("-----BEGIN ") {
        if let Some(hdr_end) = rest[start..].find("PRIVATE KEY-----") {
            let after_hdr = start + hdr_end + "PRIVATE KEY-----".len();
            if let Some(end_rel) = rest[after_hdr..].find("-----END ") {
                let end_start = after_hdr + end_rel;
                let end_close = rest[end_start..]
                    .find("-----")
                    .and_then(|_| {
                        let tail = &rest[end_start + 9..];
                        tail.find("-----").map(|k| end_start + 9 + k + 5)
                    })
                    .unwrap_or(rest.len());
                out.push_str(&rest[..start]);
                out.push_str("[REDACTED:private_key]");
                rest = &rest[end_close..];
                count += 1;
                continue;
            }
        }
        out.push_str(&rest[..start + 11]);
        rest = &rest[start + 11..];
    }
    out.push_str(rest);
    (out, count)
}

fn redact_bearer_tokens(input: &str) -> (String, usize) {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0usize;
    let mut count = 0usize;
    while i < bytes.len() {
        if i + 7 < bytes.len()
            && input[i..].len() >= 7
            && input[i..i + 7].eq_ignore_ascii_case("bearer ")
        {
            let tok_start = i + 7;
            let mut tok_end = tok_start;
            while tok_end < bytes.len()
                && (is_token_byte(bytes[tok_end]) || matches!(bytes[tok_end], b'+' | b'/' | b'='))
            {
                tok_end += 1;
            }
            if tok_end.saturating_sub(tok_start) >= 12 {
                out.push_str(&input[i..tok_start]);
                out.push_str("[REDACTED:bearer_token]");
                i = tok_end;
                count += 1;
                continue;
            }
        }
        let ch = input[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    (out, count)
}

fn redact_prefixed_token(
    input: &str,
    prefix: &str,
    min_tail: usize,
    kind: &str,
) -> (String, usize) {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0usize;
    let mut count = 0usize;
    while i < bytes.len() {
        if input[i..].starts_with(prefix) {
            let boundary_ok = i == 0 || !is_token_byte(bytes[i - 1]);
            if boundary_ok {
                let start = i + prefix.len();
                let mut end = start;
                while end < bytes.len() && is_token_byte(bytes[end]) {
                    end += 1;
                }
                if end.saturating_sub(start) >= min_tail {
                    out.push_str(&format!("[REDACTED:{kind}]"));
                    i = end;
                    count += 1;
                    continue;
                }
            }
        }
        let ch = input[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    (out, count)
}

fn redact_jwts(input: &str) -> (String, usize) {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0usize;
    let mut count = 0usize;
    while i < bytes.len() {
        if input[i..].starts_with("eyJ") && (i == 0 || !is_token_byte(bytes[i - 1])) {
            let mut end = i;
            while end < bytes.len()
                && (bytes[end].is_ascii_alphanumeric() || matches!(bytes[end], b'-' | b'_' | b'.'))
            {
                end += 1;
            }
            let cand = &input[i..end];
            let parts: Vec<&str> = cand.split('.').collect();
            if parts.len() == 3
                && parts[0].len() >= 10
                && parts[1].starts_with("eyJ")
                && parts[1].len() >= 10
                && parts[2].len() >= 10
            {
                out.push_str("[REDACTED:jwt]");
                i = end;
                count += 1;
                continue;
            }
        }
        let ch = input[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    (out, count)
}

fn redact_env_assignments(input: &str) -> (String, usize) {
    let mut out_lines = Vec::new();
    let mut count = 0usize;
    for line in input.split('\n') {
        if let Some(eq) = line.find('=') {
            let lhs = line[..eq].trim();
            let rhs = line[eq + 1..].trim();
            let key_token = lhs.split_whitespace().last().unwrap_or("");
            let upper = key_token.to_ascii_uppercase();
            let looks_secret_key = !key_token.is_empty()
                && key_token
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_')
                && (upper.contains("SECRET")
                    || upper.contains("PASSWORD")
                    || upper.contains("PASSWD")
                    || upper.ends_with("_TOKEN")
                    || upper == "TOKEN"
                    || upper.ends_with("_API_KEY")
                    || upper == "API_KEY"
                    || upper.ends_with("_PRIVATE_KEY"));
            if looks_secret_key
                && !rhs.is_empty()
                && !rhs.starts_with("[REDACTED:")
                && rhs.trim_matches(|c| c == '"' || c == '\'').len() >= 4
            {
                out_lines.push(format!("{}=[REDACTED:env_secret]", &line[..eq]));
                count += 1;
                continue;
            }
        }
        out_lines.push(line.to_owned());
    }
    (out_lines.join("\n"), count)
}

fn redact_home_paths(input: &str) -> (String, usize) {
    let mut out = String::with_capacity(input.len());
    let mut i = 0usize;
    let mut count = 0usize;
    let bytes = input.as_bytes();
    while i < bytes.len() {
        let prefix = if input[i..].starts_with("/Users/") {
            Some("/Users/")
        } else if input[i..].starts_with("/home/") {
            Some("/home/")
        } else {
            None
        };
        if let Some(pref) = prefix {
            let user_start = i + pref.len();
            let mut user_end = user_start;
            while user_end < bytes.len()
                && (bytes[user_end].is_ascii_alphanumeric()
                    || matches!(bytes[user_end], b'_' | b'-' | b'.'))
            {
                user_end += 1;
            }
            if user_end > user_start {
                out.push('~');
                i = user_end;
                count += 1;
                continue;
            }
        }
        let ch = input[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    (out, count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_keys_jwts_env_pem_and_home_paths() {
        let sample = "\
Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.signature123456
OPENAI_API_KEY=sk-proj-abcdefghijklmnopqrstuvwxyz123456
GH=ghp_1234567890abcdefghij1234567890ab
AWS=AKIAIOSFODNN7EXAMPLE
PATH=/Users/alice/Documents/secret
-----BEGIN RSA PRIVATE KEY-----
MIIEpAIBAAKCAQEA12345
-----END RSA PRIVATE KEY-----
";
        let res = redact_secrets(sample);
        assert!(!res.text.contains("sk-proj-"));
        assert!(!res.text.contains("ghp_"));
        assert!(!res.text.contains("AKIAIOSFODNN7EXAMPLE"));
        assert!(!res.text.contains("eyJhbGci"));
        assert!(!res.text.contains("MIIEpAIBAAKCAQEA"));
        assert!(!res.text.contains("/Users/alice"));
        assert!(res.text.contains("~/Documents/secret"));
        assert!(res.total_redactions >= 6);
    }

    #[test]
    fn live_activity_summary_redacts_and_caps_at_60_chars() {
        let raw = "Running deploy with sk-1234567890abcdef1234567890abcdef\nfor /Users/bob/app and a very long suffix that should truncate cleanly";
        let summary = redact_live_activity_summary(raw, 60).unwrap();
        assert!(!summary.contains("sk-1234"));
        assert!(!summary.contains("/Users/bob"));
        assert!(!summary.contains('\n'));
        assert!(summary.chars().count() <= 60);
    }
}
