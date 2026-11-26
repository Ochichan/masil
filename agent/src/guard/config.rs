//! Configuration loader for `~/.config/masil/guard.toml` covering Phase 1B
//! (`[collision]`) and Phase 1C (`[circuit_breaker]`, `[cache_warmth]`,
//! `[cache_ttl]`) policies (D-01, D-04, D-05, D-06).

use serde::Deserialize;
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

const MAX_CONFIG_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, Deserialize)]
pub struct CollisionConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// D-04: false by default (1-tap manual warning). When true, coordinator
    /// stages queue-only sibling warnings on hard_conflict / semantic_risk.
    #[serde(default)]
    pub auto_warn_sibling: bool,
    /// Minimum cooldown per agent pair before another automatic warning (default 5 min).
    #[serde(default = "default_warn_cooldown_secs")]
    pub auto_warn_cooldown_secs: u64,
}

impl Default for CollisionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            auto_warn_sibling: false,
            auto_warn_cooldown_secs: default_warn_cooldown_secs(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CircuitBreakerConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_oscillation_window")]
    pub oscillation_window: usize,
    #[serde(default = "default_same_error_limit")]
    pub same_error_limit: u64,
    #[serde(default = "default_max_no_progress_turns")]
    pub max_no_progress_turns: u64,
    /// D-05: active focused panes only get a warning banner, never auto-interrupted.
    #[serde(default = "default_true")]
    pub skip_auto_interrupt_when_focused: bool,
    #[serde(default = "default_active_focus_grace_secs")]
    pub active_focus_grace_secs: u64,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            oscillation_window: default_oscillation_window(),
            same_error_limit: default_same_error_limit(),
            max_no_progress_turns: default_max_no_progress_turns(),
            skip_auto_interrupt_when_focused: true,
            active_focus_grace_secs: default_active_focus_grace_secs(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CacheWarmthConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for CacheWarmthConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Phase 3A: Default-ON OS Micro-Sandbox configuration (D-11).
#[derive(Debug, Clone, Deserialize)]
pub struct SandboxConfig {
    /// D-11: true by default (`masil-agent agent start` applies `--sandbox worktree`).
    #[serde(default = "default_true")]
    pub default_enabled: bool,
    /// Default sandbox profile when enabled (`worktree` | `strict` | `net-off`).
    #[serde(default = "default_sandbox_profile")]
    pub default_profile: String,
    /// When true, refuse starting an agent if the OS sandbox binary (`bwrap`/`sandbox-exec`) is unavailable.
    #[serde(default)]
    pub fail_closed: bool,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            default_enabled: true,
            default_profile: default_sandbox_profile(),
            fail_closed: false,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct GuardConfig {
    #[serde(default)]
    pub collision: CollisionConfig,
    #[serde(default)]
    pub circuit_breaker: CircuitBreakerConfig,
    #[serde(default)]
    pub cache_warmth: CacheWarmthConfig,
    #[serde(default)]
    pub sandbox: SandboxConfig,
    /// Optional manual TTL overrides (seconds) keyed by provider or tier name
    /// (`claude_api`, `claude_sub`, `claude`, `codex`, `gemini`, `agy`, etc.).
    #[serde(default)]
    pub cache_ttl: HashMap<String, u64>,
}

impl GuardConfig {
    pub fn load() -> Self {
        let Some(path) = config_path() else {
            return Self::default();
        };
        Self::load_from(&path)
    }

    pub fn load_from(path: &Path) -> Self {
        read_private_file(path)
            .and_then(|txt| toml::from_str(&txt).ok())
            .unwrap_or_default()
    }
}

fn default_true() -> bool {
    true
}

fn default_warn_cooldown_secs() -> u64 {
    300
}

fn default_oscillation_window() -> usize {
    6
}

fn default_same_error_limit() -> u64 {
    4
}

fn default_max_no_progress_turns() -> u64 {
    8
}

fn default_active_focus_grace_secs() -> u64 {
    30
}

fn default_sandbox_profile() -> String {
    "worktree".into()
}

fn config_path() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .map(|home| home.join(".config"))
        })
        .map(|base| base.join("masil/guard.toml"))
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
