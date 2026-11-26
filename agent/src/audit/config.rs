//! Read-only audit configuration loaders (`~/.config/masil/audit.toml` and
//! `<repo>/.masil/audit.toml`). Strictly declarative: no shell commands or
//! executable hooks are permitted here (D-02).

use super::Severity;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

const MAX_CONFIG_BYTES: u64 = 64 * 1024;

/// Global user toggle in `~/.config/masil/audit.toml`.
#[derive(Debug, Default, Deserialize)]
pub struct GlobalAuditConfig {
    pub enabled: Option<bool>,
}

impl GlobalAuditConfig {
    pub fn load() -> Self {
        let Some(path) = global_path() else {
            return Self::default();
        };
        read_private_file(&path)
            .and_then(|txt| toml::from_str(&txt).ok())
            .unwrap_or_default()
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }
}

/// Per-repository declarative rules in `<repo>/.masil/audit.toml`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoAuditConfig {
    #[serde(default)]
    pub ignore_paths: Vec<String>,
    #[serde(default)]
    pub severity_overrides: HashMap<String, String>,
    #[serde(default)]
    pub test_globs: Vec<String>,
    #[serde(default)]
    pub sensitive_globs: Vec<String>,
}

impl RepoAuditConfig {
    pub fn load_for_repo(repo_root: &Path) -> Self {
        let path = repo_root.join(".masil/audit.toml");
        let Ok(file) = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(&path)
        else {
            return Self::default();
        };
        let Ok(meta) = file.metadata() else {
            return Self::default();
        };
        if !meta.is_file() {
            return Self::default();
        }
        let mut text = String::new();
        if file
            .take(MAX_CONFIG_BYTES)
            .read_to_string(&mut text)
            .is_err()
        {
            return Self::default();
        }
        toml::from_str(&text).unwrap_or_default()
    }

    pub fn is_ignored(&self, path: &str) -> bool {
        self.ignore_paths.iter().any(|pat| glob_matches(pat, path))
    }

    pub fn is_test_file(&self, path: &str) -> bool {
        if self.test_globs.iter().any(|pat| glob_matches(pat, path)) {
            return true;
        }
        default_test_file(path)
    }

    pub fn is_sensitive_file(&self, path: &str) -> Option<Severity> {
        if self
            .sensitive_globs
            .iter()
            .any(|pat| glob_matches(pat, path))
        {
            return Some(Severity::High);
        }
        default_sensitive_severity(path)
    }

    /// Resolves severity override for `rule_id`. Returns `None` if explicitly
    /// silenced (`"off"` / `"ignore"` / `"none"`).
    pub fn resolve_severity(&self, rule_id: &str, default: Severity) -> Option<Severity> {
        let Some(raw) = self
            .severity_overrides
            .get(rule_id)
            .or_else(|| self.severity_overrides.get(&rule_id.to_ascii_lowercase()))
        else {
            return Some(default);
        };
        match raw.trim().to_ascii_lowercase().as_str() {
            "off" | "ignore" | "disabled" | "none" => None,
            "critical" => Some(Severity::Critical),
            "high" => Some(Severity::High),
            "warning" | "warn" => Some(Severity::Warning),
            "info" => Some(Severity::Info),
            _ => Some(default),
        }
    }
}

fn global_path() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .map(|home| home.join(".config"))
        })
        .map(|base| base.join("masil/audit.toml"))
}

fn read_private_file(path: &Path) -> Option<String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .ok()?;
    let meta = file.metadata().ok()?;
    // SAFETY: geteuid has no preconditions.
    if !meta.is_file() || meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o022 != 0 {
        return None;
    }
    let mut text = String::new();
    file.take(MAX_CONFIG_BYTES).read_to_string(&mut text).ok()?;
    Some(text)
}

pub fn default_test_file(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    lower.starts_with("tests/")
        || lower.starts_with("test/")
        || lower.contains("/tests/")
        || lower.contains("/__tests__/")
        || lower.contains("/__snapshots__/")
        || name.ends_with(".snap")
        || name.contains(".test.")
        || name.contains(".spec.")
        || name.starts_with("test_")
        || name.ends_with("_test.rs")
        || name.ends_with("_test.go")
        || name.ends_with("_test.py")
        || name.ends_with("tests.swift")
        || name.ends_with("test.swift")
}

pub fn default_sensitive_severity(path: &str) -> Option<Severity> {
    let lower = path.to_ascii_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    if name.starts_with(".env")
        || lower.contains("auth")
        || lower.contains("perm")
        || lower.contains("secret")
        || lower.contains("migrat")
        || lower.ends_with(".sql")
    {
        return Some(Severity::High);
    }
    if name.starts_with("dockerfile")
        || lower.starts_with(".github/workflows/")
        || matches!(
            name,
            "cargo.lock"
                | "package-lock.json"
                | "pnpm-lock.yaml"
                | "yarn.lock"
                | "bun.lockb"
                | "go.sum"
                | "poetry.lock"
                | "podfile.lock"
        )
    {
        return Some(Severity::Info);
    }
    None
}

/// Lightweight path glob matcher supporting `*` (single segment), `**` (any
/// path segments), and `?` (single char).
pub fn glob_matches(pattern: &str, path: &str) -> bool {
    let pat = pattern.trim().trim_start_matches("./");
    let text = path.trim().trim_start_matches("./");
    if pat.is_empty() {
        return false;
    }
    if !pat.contains('/')
        && let Some(file_name) = text.rsplit('/').next()
        && glob_chars(pat.as_bytes(), file_name.as_bytes())
    {
        return true;
    }
    glob_chars(pat.as_bytes(), text.as_bytes())
}

fn glob_chars(pat: &[u8], text: &[u8]) -> bool {
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star_pi: Option<usize> = None;
    let mut star_ti: usize = 0;
    let mut star_double = false;

    while ti < text.len() {
        if pi + 1 < pat.len() && pat[pi] == b'*' && pat[pi + 1] == b'*' {
            star_pi = Some(pi + 2);
            if pi + 2 < pat.len() && pat[pi + 2] == b'/' {
                star_pi = Some(pi + 3);
            }
            pi = star_pi.unwrap();
            star_ti = ti;
            star_double = true;
            continue;
        }
        if pi < pat.len() && pat[pi] == b'*' {
            star_pi = Some(pi + 1);
            pi += 1;
            star_ti = ti;
            star_double = false;
            continue;
        }
        if pi < pat.len() && (pat[pi] == b'?' && text[ti] != b'/' || pat[pi] == text[ti]) {
            pi += 1;
            ti += 1;
            continue;
        }
        if let Some(sp) = star_pi {
            if !star_double && text[star_ti] == b'/' {
                return false;
            }
            star_ti += 1;
            ti = star_ti;
            pi = sp;
            continue;
        }
        return false;
    }

    while pi < pat.len() && pat[pi] == b'*' {
        pi += 1;
    }
    pi == pat.len()
}
