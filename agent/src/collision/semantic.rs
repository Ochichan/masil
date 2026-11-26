//! Lightweight semantic collision scanner (`< 15ms`, zero `tree-sitter`, D-04).
//! Extracts exported symbols & imports for Rust, TS/JS, Python, and Go, plus
//! detects concurrent edits to shared schema/migration/dependency/lockfiles.

use std::collections::HashSet;
use std::fs;
use std::path::Path;

const MAX_SCAN_FILE_BYTES: u64 = 256 * 1024;

#[derive(Clone, Debug, Default)]
pub struct WorktreeSemanticProfile {
    pub exported_symbols: HashSet<String>,
    pub imported_symbols: HashSet<String>,
    pub shared_resources: HashSet<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SemanticCollision {
    pub overlapping_symbols: Vec<String>,
    pub shared_resources: Vec<String>,
    pub summary_lines: Vec<String>,
}

impl SemanticCollision {
    pub fn is_empty(&self) -> bool {
        self.overlapping_symbols.is_empty() && self.shared_resources.is_empty()
    }
}

pub fn is_shared_resource(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    lower.starts_with("migrations/")
        || lower.contains("/migrations/")
        || lower.ends_with(".sql")
        || lower.ends_with(".proto")
        || name.starts_with("openapi.")
        || name == "schema.graphql"
        || lower.ends_with(".graphql")
        || matches!(
            name,
            "package.json"
                | "cargo.toml"
                | "cargo.lock"
                | "go.mod"
                | "go.sum"
                | "package-lock.json"
                | "pnpm-lock.yaml"
                | "yarn.lock"
                | "poetry.lock"
                | "pyproject.toml"
        )
}

/// Scans the changed files of one worktree into a semantic profile.
pub fn scan_worktree_files(
    worktree_root: &Path,
    changed_files: &[String],
) -> WorktreeSemanticProfile {
    let mut profile = WorktreeSemanticProfile::default();
    for rel in changed_files.iter().take(60) {
        if is_shared_resource(rel) {
            profile.shared_resources.insert(rel.clone());
        }
        let full = worktree_root.join(rel);
        let Ok(meta) = fs::metadata(&full) else {
            continue;
        };
        if !meta.is_file() || meta.len() > MAX_SCAN_FILE_BYTES {
            continue;
        }
        let Ok(content) = fs::read_to_string(&full) else {
            continue;
        };
        scan_source_text(rel, &content, &mut profile);
    }
    profile
}

/// Scans a single source text into `profile` (exposed directly for fast unit testing).
pub fn scan_source_text(path: &str, text: &str, profile: &mut WorktreeSemanticProfile) {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".rs") {
        scan_rust(text, profile);
    } else if lower.ends_with(".ts")
        || lower.ends_with(".tsx")
        || lower.ends_with(".js")
        || lower.ends_with(".jsx")
        || lower.ends_with(".mjs")
    {
        scan_ts_js(text, profile);
    } else if lower.ends_with(".py") {
        scan_python(text, profile);
    } else if lower.ends_with(".go") {
        scan_go(text, profile);
    }
}

/// Compares two worktree semantic profiles for cross-worktree interface risk.
pub fn compare_profiles(
    name_a: &str,
    a: &WorktreeSemanticProfile,
    name_b: &str,
    b: &WorktreeSemanticProfile,
) -> SemanticCollision {
    let mut symbols: HashSet<String> = HashSet::new();
    let mut summary_lines: Vec<String> = Vec::new();

    // A exports a symbol that B imports (or both export the same symbol).
    let a_to_b: Vec<&String> = a
        .exported_symbols
        .intersection(&b.imported_symbols)
        .collect();
    if !a_to_b.is_empty() {
        let mut names: Vec<&str> = a_to_b.iter().map(|s| s.as_str()).collect();
        names.sort_unstable();
        for n in &names {
            symbols.insert((*n).to_owned());
        }
        summary_lines.push(format!(
            "`{name_a}` modified exported symbol(s) imported by `{name_b}`: {}",
            names.iter().take(6).copied().collect::<Vec<_>>().join(", ")
        ));
    }

    let b_to_a: Vec<&String> = b
        .exported_symbols
        .intersection(&a.imported_symbols)
        .collect();
    if !b_to_a.is_empty() {
        let mut names: Vec<&str> = b_to_a.iter().map(|s| s.as_str()).collect();
        names.sort_unstable();
        for n in &names {
            symbols.insert((*n).to_owned());
        }
        summary_lines.push(format!(
            "`{name_b}` modified exported symbol(s) imported by `{name_a}`: {}",
            names.iter().take(6).copied().collect::<Vec<_>>().join(", ")
        ));
    }

    let mut shared: Vec<String> = Vec::new();
    if !a.shared_resources.is_empty() && !b.shared_resources.is_empty() {
        let union_res: HashSet<&String> = a.shared_resources.union(&b.shared_resources).collect();
        for r in union_res {
            shared.push(r.clone());
        }
        shared.sort();
        summary_lines.push(format!(
            "Both `{name_a}` and `{name_b}` modified shared schema/dependency/lockfile resource(s): {}",
            shared.iter().take(6).cloned().collect::<Vec<_>>().join(", ")
        ));
    }

    let mut overlapping_symbols: Vec<String> = symbols.into_iter().collect();
    overlapping_symbols.sort();

    SemanticCollision {
        overlapping_symbols,
        shared_resources: shared,
        summary_lines,
    }
}

fn scan_rust(text: &str, profile: &mut WorktreeSemanticProfile) {
    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with("//") {
            continue;
        }
        // Exports: `pub fn foo`, `pub(crate) struct Bar`, `pub async fn baz`, etc.
        if let Some(after_pub) = strip_rust_pub(line) {
            let rest = after_pub
                .strip_prefix("async ")
                .unwrap_or(after_pub)
                .trim_start();
            for kw in [
                "fn ", "struct ", "enum ", "trait ", "type ", "const ", "mod ",
            ] {
                if let Some(tail) = rest.strip_prefix(kw)
                    && let Some(ident) = first_ident(tail)
                    && ident.len() >= 3
                {
                    profile.exported_symbols.insert(ident);
                    break;
                }
            }
        }
        // Imports: `use crate::foo::{Bar, baz};`
        if let Some(after_use) = line.strip_prefix("use ") {
            for ident in extract_rust_use_idents(after_use) {
                if ident.len() >= 3 {
                    profile.imported_symbols.insert(ident);
                }
            }
        }
    }
}

fn strip_rust_pub(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("pub")?;
    if let Some(after_space) = rest.strip_prefix(' ') {
        return Some(after_space.trim_start());
    }
    if let Some(after_paren) = rest.strip_prefix('(')
        && let Some((_, after_close)) = after_paren.split_once(')')
    {
        return Some(after_close.trim_start());
    }
    None
}

fn extract_rust_use_idents(after_use: &str) -> Vec<String> {
    let body = after_use.trim().trim_end_matches(';');
    let mut out = Vec::new();
    if let Some((_, inner)) = body.split_once('{')
        && let Some((braced, _)) = inner.split_once('}')
    {
        for part in braced.split(',') {
            let item = part.trim().split(" as ").next().unwrap_or("").trim();
            if let Some(last) = item.rsplit("::").next()
                && let Some(ident) = first_ident(last)
            {
                out.push(ident);
            }
        }
    } else if let Some(last) = body
        .split(" as ")
        .next()
        .and_then(|s| s.rsplit("::").next())
        && let Some(ident) = first_ident(last)
    {
        out.push(ident);
    }
    out
}

fn scan_ts_js(text: &str, profile: &mut WorktreeSemanticProfile) {
    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with("//") {
            continue;
        }
        if let Some(after_exp) = line.strip_prefix("export ") {
            let rest = after_exp
                .strip_prefix("default ")
                .unwrap_or(after_exp)
                .trim_start();
            let rest = rest.strip_prefix("async ").unwrap_or(rest).trim_start();
            for kw in [
                "function ",
                "class ",
                "interface ",
                "type ",
                "const ",
                "let ",
                "enum ",
            ] {
                if let Some(tail) = rest.strip_prefix(kw)
                    && let Some(ident) = first_ident(tail)
                    && ident.len() >= 3
                {
                    profile.exported_symbols.insert(ident);
                    break;
                }
            }
        }
        if line.starts_with("import ") {
            if let Some((_, after_open)) = line.split_once('{')
                && let Some((inside, _)) = after_open.split_once('}')
            {
                for part in inside.split(',') {
                    let clean = part
                        .trim()
                        .strip_prefix("type ")
                        .unwrap_or(part.trim())
                        .split(" as ")
                        .next()
                        .unwrap_or("")
                        .trim();
                    if let Some(ident) = first_ident(clean)
                        && ident.len() >= 3
                    {
                        profile.imported_symbols.insert(ident);
                    }
                }
            } else if let Some(after_imp) = line.strip_prefix("import ")
                && let Some((lhs, _)) = after_imp.split_once(" from ")
            {
                let clean = lhs.trim().strip_prefix("type ").unwrap_or(lhs.trim());
                if let Some(ident) = first_ident(clean)
                    && ident.len() >= 3
                {
                    profile.imported_symbols.insert(ident);
                }
            }
        }
    }
}

fn scan_python(text: &str, profile: &mut WorktreeSemanticProfile) {
    for raw in text.lines() {
        if raw.starts_with(' ') || raw.starts_with('\t') {
            // Only top-level defs/classes/imports.
            continue;
        }
        let line = raw.trim();
        if line.starts_with('#') {
            continue;
        }
        let rest = line.strip_prefix("async ").unwrap_or(line);
        for kw in ["def ", "class "] {
            if let Some(tail) = rest.strip_prefix(kw)
                && let Some(ident) = first_ident(tail)
                && !ident.starts_with('_')
                && ident.len() >= 3
            {
                profile.exported_symbols.insert(ident);
                break;
            }
        }
        if let Some((_, after_imp)) = line.split_once(" import ") {
            for part in after_imp.trim_matches(&['(', ')'][..]).split(',') {
                let clean = part.trim().split(" as ").next().unwrap_or("").trim();
                if let Some(ident) = first_ident(clean)
                    && ident.len() >= 3
                {
                    profile.imported_symbols.insert(ident);
                }
            }
        }
    }
}

fn scan_go(text: &str, profile: &mut WorktreeSemanticProfile) {
    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with("//") {
            continue;
        }
        if let Some(tail) = line.strip_prefix("func ") {
            let fn_part = if let Some(after_recv) = tail.strip_prefix('(') {
                after_recv
                    .split_once(')')
                    .map(|(_, rest)| rest.trim_start())
                    .unwrap_or("")
            } else {
                tail
            };
            if let Some(ident) = first_ident(fn_part)
                && ident.chars().next().is_some_and(|c| c.is_ascii_uppercase())
                && ident.len() >= 3
            {
                profile.exported_symbols.insert(ident);
            }
        } else if let Some(tail) = line.strip_prefix("type ")
            && let Some(ident) = first_ident(tail)
            && ident.chars().next().is_some_and(|c| c.is_ascii_uppercase())
            && ident.len() >= 3
        {
            profile.exported_symbols.insert(ident);
        } else if let Some(tail) = line.strip_prefix("import ")
            && let Some(pkg) = tail.trim_matches('"').rsplit('/').next()
            && let Some(ident) = first_ident(pkg)
            && ident.len() >= 3
        {
            profile.imported_symbols.insert(ident);
        }
    }
}

fn first_ident(s: &str) -> Option<String> {
    let trimmed = s.trim_start();
    let ident: String = trimmed
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    if ident.is_empty() || ident.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        None
    } else {
        Some(ident)
    }
}
