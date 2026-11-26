//! Policy rules loader/writer (`~/.config/masil/policies.toml`) and SQLite sync
//! for the Phase 3A Zero-Trust Permission Firewall (D-10).

use crate::managed::PermissionPolicyRecord;
use masil_wire::rpc::{PromptRiskAssessment, PromptRiskTier};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

const MAX_POLICY_FILE_BYTES: u64 = 128 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyAction {
    AutoApprove,
    SuggestBatch,
    AlwaysConfirm,
    Deny,
}

impl PolicyAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AutoApprove => "auto_approve",
            Self::SuggestBatch => "suggest_batch",
            Self::AlwaysConfirm => "always_confirm",
            Self::Deny => "deny",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto_approve" => Ok(Self::AutoApprove),
            "suggest_batch" => Ok(Self::SuggestBatch),
            "always_confirm" => Ok(Self::AlwaysConfirm),
            "deny" => Ok(Self::Deny),
            other => Err(format!(
                "invalid_argument: policy action must be auto_approve, suggest_batch, always_confirm, or deny (got `{other}`)"
            )),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyDefaults {
    /// D-10: allow user-authored low-risk rules across all providers (including TUI parsing).
    #[serde(default = "default_true")]
    pub allow_tui_rule_auto_approve: bool,
}

impl Default for PolicyDefaults {
    fn default() -> Self {
        Self {
            allow_tui_rule_auto_approve: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PolicyRule {
    pub id: String,
    #[serde(default = "default_scope")]
    pub scope: String,
    #[serde(default)]
    pub match_command: Vec<String>,
    #[serde(default)]
    pub match_path: Vec<String>,
    #[serde(default = "default_max_risk")]
    pub max_risk: String,
    pub action: PolicyAction,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_source")]
    pub source: String,
}

impl PolicyRule {
    pub fn parsed_max_risk(&self) -> PromptRiskTier {
        match self.max_risk.trim().to_ascii_lowercase().as_str() {
            "low" => PromptRiskTier::Low,
            "medium" => PromptRiskTier::Medium,
            "high" => PromptRiskTier::High,
            "critical" => PromptRiskTier::Critical,
            _ => PromptRiskTier::Low,
        }
    }

    pub fn to_record(&self, now_ms: u64) -> PermissionPolicyRecord {
        let match_json = serde_json::json!({
            "match_command": self.match_command,
            "match_path": self.match_path,
        })
        .to_string();
        PermissionPolicyRecord {
            id: self.id.clone(),
            scope: self.scope.clone(),
            match_json,
            max_risk: self.max_risk.clone(),
            action: self.action.as_str().to_owned(),
            enabled: self.enabled,
            source: self.source.clone(),
            created_ms: now_ms,
            updated_ms: now_ms,
        }
    }

    pub fn from_record(rec: &PermissionPolicyRecord) -> Option<Self> {
        let match_val: serde_json::Value =
            serde_json::from_str(&rec.match_json).unwrap_or_else(|_| serde_json::json!({}));
        let match_command = match_val
            .get("match_command")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect();
        let match_path = match_val
            .get("match_path")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect();
        Some(Self {
            id: rec.id.clone(),
            scope: rec.scope.clone(),
            match_command,
            match_path,
            max_risk: rec.max_risk.clone(),
            action: PolicyAction::parse(&rec.action).ok()?,
            enabled: rec.enabled,
            source: rec.source.clone(),
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PoliciesFile {
    #[serde(default)]
    pub defaults: PolicyDefaults,
    #[serde(default)]
    pub rule: Vec<PolicyRule>,
}

impl PoliciesFile {
    pub fn config_path() -> Option<PathBuf> {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .filter(|p| p.is_absolute())
                    .map(|home| home.join(".config"))
            })
            .map(|base| base.join("masil/policies.toml"))
    }

    pub fn load() -> Self {
        let Some(path) = Self::config_path() else {
            return Self::default();
        };
        Self::load_from(&path)
    }

    pub fn load_from(path: &Path) -> Self {
        read_private_file(path)
            .and_then(|txt| toml::from_str(&txt).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<PathBuf, String> {
        let path = Self::config_path().ok_or("no absolute XDG_CONFIG_HOME or HOME")?;
        self.save_to(&path)?;
        Ok(path)
    }

    pub fn save_to(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("creating {}: {e}", parent.display()))?;
            let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
        }
        let body =
            toml::to_string_pretty(self).map_err(|e| format!("serializing policies.toml: {e}"))?;
        let tmp = path.with_extension("toml.tmp");
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&tmp)
            .map_err(|e| format!("opening {}: {e}", tmp.display()))?;
        file.write_all(body.as_bytes())
            .map_err(|e| format!("writing {}: {e}", tmp.display()))?;
        file.sync_all()
            .map_err(|e| format!("syncing {}: {e}", tmp.display()))?;
        fs::rename(&tmp, path)
            .map_err(|e| format!("renaming {} -> {}: {e}", tmp.display(), path.display()))?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchedPolicy {
    pub rule: PolicyRule,
}

/// Evaluates all enabled rules against `assessment` and `cwd`.
/// Precedence: `Deny` > `AlwaysConfirm` > `AutoApprove` > `SuggestBatch`.
pub fn evaluate_rules(
    rules: &[PolicyRule],
    assessment: &PromptRiskAssessment,
    cwd: Option<&str>,
) -> Option<MatchedPolicy> {
    let command = assessment.command.as_deref().unwrap_or("");
    let mut best: Option<PolicyRule> = None;

    for rule in rules {
        if !rule.enabled {
            continue;
        }
        if !scope_matches(&rule.scope, cwd) {
            continue;
        }
        if !rule_matches_target(rule, command) {
            continue;
        }
        // `Deny` and `AlwaysConfirm` apply regardless of risk tier;
        // permissive actions (`AutoApprove`, `SuggestBatch`) respect `rule.max_risk`.
        if matches!(
            rule.action,
            PolicyAction::AutoApprove | PolicyAction::SuggestBatch
        ) && assessment.tier > rule.parsed_max_risk()
        {
            continue;
        }
        let replace = match &best {
            None => true,
            Some(prev) => action_priority(rule.action) > action_priority(prev.action),
        };
        if replace {
            best = Some(rule.clone());
        }
    }

    best.map(|rule| MatchedPolicy { rule })
}

fn action_priority(action: PolicyAction) -> u8 {
    match action {
        PolicyAction::Deny => 4,
        PolicyAction::AlwaysConfirm => 3,
        PolicyAction::AutoApprove => 2,
        PolicyAction::SuggestBatch => 1,
    }
}

fn scope_matches(scope: &str, cwd: Option<&str>) -> bool {
    let trimmed = scope.trim();
    if trimmed.is_empty() || trimmed == "global" || trimmed == "*" {
        return true;
    }
    let Some(repo_spec) = trimmed.strip_prefix("repo:") else {
        return false;
    };
    let Some(cwd_str) = cwd else {
        return false;
    };
    let expanded = expand_tilde(repo_spec.trim());
    let target_root = Path::new(&expanded)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(&expanded));
    let actual = Path::new(cwd_str)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(cwd_str));
    actual.starts_with(&target_root)
}

fn rule_matches_target(rule: &PolicyRule, command: &str) -> bool {
    if rule.match_command.is_empty() && rule.match_path.is_empty() {
        return false;
    }
    let cmd_trimmed = command.trim();
    if !rule.match_command.is_empty() {
        let matched = rule
            .match_command
            .iter()
            .any(|pat| glob_match(pat.trim(), cmd_trimmed));
        if matched {
            return true;
        }
    }
    if !rule.match_path.is_empty() {
        for raw_pat in &rule.match_path {
            let expanded = expand_tilde(raw_pat.trim());
            // Match against raw text or any whitespace-delimited token.
            if glob_match(raw_pat.trim(), cmd_trimmed) || glob_match(&expanded, cmd_trimmed) {
                return true;
            }
            for tok in cmd_trimmed.split_whitespace() {
                let clean = tok.trim_matches(|c| matches!(c, '"' | '\'' | ',' | ';'));
                let clean_expanded = expand_tilde(clean);
                if glob_match(raw_pat.trim(), clean)
                    || glob_match(&expanded, clean)
                    || glob_match(&expanded, &clean_expanded)
                {
                    return true;
                }
            }
        }
    }
    false
}

/// Simple, deterministic glob matcher supporting `*`, `**`, and `?`.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star_pi: Option<usize> = None;
    let mut star_ti: usize = 0;

    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            // Collapse consecutive `*` (including `**`).
            while pi < p.len() && p[pi] == '*' {
                pi += 1;
            }
            star_pi = Some(pi);
            star_ti = ti;
        } else if let Some(sp) = star_pi {
            pi = sp;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        return format!("{home}/{rest}");
    }
    path.to_owned()
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
    file.take(MAX_POLICY_FILE_BYTES)
        .read_to_string(&mut text)
        .ok()?;
    Some(text)
}

fn default_true() -> bool {
    true
}

fn default_scope() -> String {
    "global".into()
}

fn default_max_risk() -> String {
    "low".into()
}

fn default_source() -> String {
    "user".into()
}
