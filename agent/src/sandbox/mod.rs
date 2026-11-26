//! Phase 3A: Default-ON Practical Worktree OS Micro-Sandbox (D-11).
//!
//! Wraps managed agent launches with macOS Seatbelt (`/usr/bin/sandbox-exec`)
//! or Linux Bubblewrap (`bwrap`), denying reads of `~/.ssh`, `~/.aws`,
//! `~/.gnupg`, `~/.kube`, `~/.netrc`, `~/.config/gh`, and restricting writes
//! strictly to the canonical worktree, parent `.git` common metadata, shared
//! package caches (omitted in `strict`), active provider state dir, and an
//! isolated per-run temporary directory.
//!
//! Repository-local overrides in `.masil/sandbox.toml` are ignored until the
//! user explicitly signs their SHA-256 digest via `masil-agent sandbox trust`.

pub mod bwrap;
pub mod seatbelt;

use crate::guard::config::{GuardConfig, SandboxConfig};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

pub const REPO_SANDBOX_FILE: &str = ".masil/sandbox.toml";
const MAX_REPO_SANDBOX_BYTES: u64 = 64 * 1024;

const SANDBOX_HELP: &str = "\
usage: masil-agent sandbox status [--repo PATH]
       masil-agent sandbox trust <REPO> [--confirm SHA256]
       masil-agent sandbox untrust <REPO>";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxProfile {
    Worktree,
    Strict,
    NetOff,
    Off,
}

impl SandboxProfile {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Worktree => "worktree",
            Self::Strict => "strict",
            Self::NetOff => "net-off",
            Self::Off => "off",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "worktree" | "default" | "on" => Ok(Self::Worktree),
            "strict" => Ok(Self::Strict),
            "net-off" | "net_off" | "offline" => Ok(Self::NetOff),
            "off" | "none" | "disabled" => Ok(Self::Off),
            other => Err(format!(
                "invalid_argument: sandbox profile must be worktree, strict, net-off, or off (got `{other}`)"
            )),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxStatus {
    pub available: bool,
    pub default_enabled: bool,
    pub default_profile: String,
    pub fail_closed: bool,
    pub backend: String,
    pub detail: String,
}

/// Optional repository-local sandbox extension (`.masil/sandbox.toml`), applied
/// ONLY after explicit SHA-256 user trust (`masil-agent sandbox trust --confirm`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoSandboxFile {
    #[serde(default)]
    pub allow_write: Vec<String>,
    #[serde(default)]
    pub allow_read: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct TrustStore {
    entries: BTreeMap<String, TrustEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct TrustEntry {
    dev: u64,
    ino: u64,
    sha256: String,
    trusted_ms: u64,
}

#[derive(Debug, Clone)]
pub struct SandboxSpec {
    pub profile: SandboxProfile,
    #[allow(dead_code)]
    pub worktree: PathBuf,
    pub private_tmp_dir: PathBuf,
    pub writable_dirs: Vec<PathBuf>,
    pub writable_files: Vec<PathBuf>,
    pub denied_read_dirs: Vec<PathBuf>,
    pub denied_read_files: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct WrappedCommand {
    pub program: String,
    pub args: Vec<String>,
    pub extra_env: Vec<(String, String)>,
    pub badge: String,
    #[allow(dead_code)]
    pub profile_path: Option<PathBuf>,
}

/// Reports host OS micro-sandbox availability & configured defaults.
pub fn status() -> SandboxStatus {
    let cfg = GuardConfig::load().sandbox;
    status_with_config(&cfg)
}

pub fn status_with_config(cfg: &SandboxConfig) -> SandboxStatus {
    if seatbelt::is_available() {
        SandboxStatus {
            available: true,
            default_enabled: cfg.default_enabled,
            default_profile: cfg.default_profile.clone(),
            fail_closed: cfg.fail_closed,
            backend: "seatbelt".into(),
            detail: format!("macOS Seatbelt ({}) ready", seatbelt::SANDBOX_EXEC_BIN),
        }
    } else if bwrap::is_available() {
        SandboxStatus {
            available: true,
            default_enabled: cfg.default_enabled,
            default_profile: cfg.default_profile.clone(),
            fail_closed: cfg.fail_closed,
            backend: "bwrap".into(),
            detail: "Linux bubblewrap (`bwrap`) ready".into(),
        }
    } else {
        let detail = if cfg!(target_os = "linux") {
            "bubblewrap (`bwrap`) not found; install `bubblewrap` (e.g. `sudo apt install bubblewrap`) to enable OS micro-sandboxing".into()
        } else {
            "no supported OS micro-sandbox backend found on this platform".into()
        };
        SandboxStatus {
            available: false,
            default_enabled: cfg.default_enabled,
            default_profile: cfg.default_profile.clone(),
            fail_closed: cfg.fail_closed,
            backend: "none".into(),
            detail,
        }
    }
}

/// Resolves the effective `SandboxProfile` for `agent start` from CLI overrides and `guard.toml`.
pub fn resolve_start_profile(
    explicit_sandbox: Option<&str>,
    no_sandbox: bool,
    cfg: &SandboxConfig,
) -> Result<SandboxProfile, String> {
    if no_sandbox {
        return Ok(SandboxProfile::Off);
    }
    if let Some(raw) = explicit_sandbox {
        return SandboxProfile::parse(raw);
    }
    if !cfg.default_enabled {
        return Ok(SandboxProfile::Off);
    }
    SandboxProfile::parse(&cfg.default_profile)
}

/// Builds a `SandboxSpec` for an agent launch in `cwd`.
pub fn build_spec(
    profile: SandboxProfile,
    cwd: &Path,
    provider: &str,
    run_id: &str,
    server_socket: Option<&Path>,
) -> Result<SandboxSpec, String> {
    let worktree = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute());

    // Isolated per-run temp directory (never opens all of `/tmp` or `/var/folders` to writes!).
    let safe_run: String = run_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .take(36)
        .collect();
    let run_slug = if safe_run.is_empty() {
        format!("pid-{}", std::process::id())
    } else {
        safe_run
    };
    let private_tmp_base = crate::managed::state_base()
        .map(|base| base.join("masil/sandbox"))
        .unwrap_or_else(|_| {
            // SAFETY: geteuid has no preconditions.
            PathBuf::from(format!("/tmp/masil-sbx-{}", unsafe { libc::geteuid() }))
        });
    let private_tmp_dir = private_tmp_base.join(&run_slug);

    let mut writable_dirs = vec![worktree.clone(), private_tmp_dir.clone()];
    let mut writable_files = Vec::new();

    // Allow parent Git common dir (`git commit`, `git status`, worktree metadata).
    if let Some(git_common) = detect_git_common_dir(&worktree) {
        writable_dirs.push(git_common);
    }

    // Allow masil state/config dirs for agent hooks, checkpoints, and operation tokens.
    if let Ok(state_base) = crate::managed::state_base() {
        writable_dirs.push(state_base.join("masil"));
    }
    if let Some(ref h) = home {
        writable_dirs.push(h.join(".local/state/masil"));
        writable_dirs.push(h.join(".config/masil"));
        // Single active provider's session/state directory ONLY (never sibling providers!).
        for pdir in provider_state_dirs(h, provider) {
            writable_dirs.push(pdir);
        }
        // Shared language package caches (omitted in `strict` mode per D-11).
        if profile != SandboxProfile::Strict {
            for cache in [
                ".cargo",
                ".npm",
                ".pnpm-store",
                ".cache/pip",
                ".cache/uv",
                "go/pkg/mod",
            ] {
                writable_dirs.push(h.join(cache));
            }
        }
    }

    // Allow connecting to the specific masil/tmux server socket file (and bridge socket),
    // NOT the entire parent temp directory.
    if let Some(sock) = server_socket {
        writable_files.push(sock.to_path_buf());
        if let Some(parent) = sock.parent() {
            writable_dirs.push(parent.join(".masil"));
        }
    }

    // Apply `.masil/sandbox.toml` ONLY when SHA-256 trusted by the user.
    if let Some(trusted_cfg) = load_trusted_repo_sandbox(&worktree) {
        for raw in trusted_cfg.allow_write {
            let resolved = resolve_extra_path(&worktree, home.as_deref(), &raw);
            if !is_protected_secret_path(home.as_deref(), &resolved) {
                writable_dirs.push(resolved);
            }
        }
    }

    let mut denied_read_dirs = Vec::new();
    let mut denied_read_files = Vec::new();
    if let Some(ref h) = home {
        for d in [".ssh", ".aws", ".gnupg", ".kube", ".config/gh"] {
            let p = h.join(d);
            // Never deny if the user explicitly launched an agent inside that directory.
            if !worktree.starts_with(&p) {
                denied_read_dirs.push(p);
            }
        }
        let netrc = h.join(".netrc");
        if !worktree.starts_with(&netrc) {
            denied_read_files.push(netrc);
        }
    }

    Ok(SandboxSpec {
        profile,
        worktree,
        private_tmp_dir,
        writable_dirs,
        writable_files,
        denied_read_dirs,
        denied_read_files,
    })
}

/// Returns the expected honest `@masil-sandbox` badge for `profile` on this host.
pub fn expected_badge(profile: SandboxProfile) -> String {
    if profile == SandboxProfile::Off {
        return "unsandboxed:user_disabled".into();
    }
    if seatbelt::is_available() {
        return format!("sandboxed:seatbelt:{}", profile.as_str());
    }
    if bwrap::is_available() {
        return format!("sandboxed:bwrap:{}", profile.as_str());
    }
    "unsandboxed:backend_missing".into()
}

/// Wraps `program` + `args` using the platform's OS micro-sandbox, or falls back
/// according to `cfg.fail_closed` when unavailable.
pub fn wrap_command(
    spec: &SandboxSpec,
    program: &str,
    args: &[String],
    fail_closed: bool,
) -> Result<WrappedCommand, String> {
    if spec.profile == SandboxProfile::Off {
        return Ok(WrappedCommand {
            program: program.to_owned(),
            args: args.to_vec(),
            extra_env: Vec::new(),
            badge: "unsandboxed:user_disabled".into(),
            profile_path: None,
        });
    }
    if seatbelt::is_available() {
        return seatbelt::wrap(spec, program, args);
    }
    if bwrap::is_available() {
        return bwrap::wrap(spec, program, args);
    }
    if fail_closed {
        return Err(
            "sandbox_unavailable: OS micro-sandbox backend is missing and fail_closed=true".into(),
        );
    }
    eprintln!(
        "masil-agent: warning: OS micro-sandbox backend unavailable (`bwrap` missing); running unsandboxed (see `masil-agent doctor`)"
    );
    Ok(WrappedCommand {
        program: program.to_owned(),
        args: args.to_vec(),
        extra_env: Vec::new(),
        badge: "unsandboxed:backend_missing".into(),
        profile_path: None,
    })
}

fn provider_state_dirs(home: &Path, provider: &str) -> Vec<PathBuf> {
    match provider {
        "claude" => vec![home.join(".claude"), home.join(".claude.json")],
        "codex" => vec![home.join(".codex")],
        "opencode" => vec![
            home.join(".config/opencode"),
            home.join(".local/share/opencode"),
        ],
        "kilo" => vec![home.join(".config/kilo"), home.join(".local/share/kilo")],
        "gemini" => vec![home.join(".gemini")],
        "agy" | "antigravity" => vec![home.join(".gemini/antigravity"), home.join(".antigravity")],
        "cursor" => vec![home.join(".cursor")],
        "cline" => vec![home.join(".cline")],
        "qwen" => vec![home.join(".qwen")],
        "kiro" => vec![home.join(".kiro")],
        "omp" => vec![home.join(".omp"), home.join(".config/omp")],
        _ => Vec::new(),
    }
}

fn detect_git_common_dir(dir: &Path) -> Option<PathBuf> {
    let out = crate::worktree::git::query(
        dir,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .ok()?;
    let trimmed = out.trim();
    if trimmed.is_empty() {
        return None;
    }
    let p = PathBuf::from(trimmed);
    p.canonicalize().ok().or(Some(p))
}

fn resolve_extra_path(worktree: &Path, home: Option<&Path>, raw: &str) -> PathBuf {
    let trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix("~/")
        && let Some(h) = home
    {
        return h.join(rest);
    }
    let p = PathBuf::from(trimmed);
    if p.is_absolute() { p } else { worktree.join(p) }
}

fn is_protected_secret_path(home: Option<&Path>, candidate: &Path) -> bool {
    let Some(h) = home else {
        return false;
    };
    let canon = candidate
        .canonicalize()
        .unwrap_or_else(|_| candidate.to_path_buf());
    for d in [".ssh", ".aws", ".gnupg", ".kube", ".netrc", ".config/gh"] {
        let protected = h.join(d);
        let protected_canon = protected.canonicalize().unwrap_or(protected);
        if canon.starts_with(&protected_canon) || protected_canon.starts_with(&canon) {
            return true;
        }
    }
    false
}

// ------------------------------------------------------------------
// SHA-256 trust store for `.masil/sandbox.toml` (D-11)
// ------------------------------------------------------------------

fn trust_store_path() -> Result<PathBuf, String> {
    let base = crate::managed::state_base()?;
    let dir = crate::managed::private_directory(&base.join("masil"))?;
    Ok(dir.join("sandbox-trust.json"))
}

fn load_trust_store() -> TrustStore {
    let Ok(path) = trust_store_path() else {
        return TrustStore::default();
    };
    let Ok(file) = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(&path)
    else {
        return TrustStore::default();
    };
    let Ok(meta) = file.metadata() else {
        return TrustStore::default();
    };
    // SAFETY: geteuid has no preconditions.
    if !meta.is_file() || meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o022 != 0 {
        return TrustStore::default();
    }
    let mut text = String::new();
    if file.take(256 * 1024).read_to_string(&mut text).is_err() {
        return TrustStore::default();
    }
    serde_json::from_str(&text).unwrap_or_default()
}

fn save_trust_store(store: &TrustStore) -> Result<(), String> {
    let path = trust_store_path()?;
    let body = serde_json::to_string_pretty(store).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&tmp)
        .map_err(|e| format!("writing {}: {e}", tmp.display()))?;
    file.write_all(body.as_bytes())
        .map_err(|e| format!("writing {}: {e}", tmp.display()))?;
    file.sync_all()
        .map_err(|e| format!("syncing {}: {e}", tmp.display()))?;
    fs::rename(&tmp, &path).map_err(|e| format!("renaming {}: {e}", path.display()))
}

fn read_repo_sandbox(
    repo_root: &Path,
) -> Result<Option<(PathBuf, RepoSandboxFile, String)>, String> {
    let file_path = repo_root.join(REPO_SANDBOX_FILE);
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(&file_path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", file_path.display())),
    };
    let meta = file
        .metadata()
        .map_err(|e| format!("{}: {e}", file_path.display()))?;
    if !meta.is_file() {
        return Err(format!("{} is not a regular file", file_path.display()));
    }
    if meta.len() > MAX_REPO_SANDBOX_BYTES {
        return Err(format!("{} exceeds 64 KiB", file_path.display()));
    }
    let mut bytes = Vec::new();
    file.take(MAX_REPO_SANDBOX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("{}: {e}", file_path.display()))?;
    let digest: String = Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| format!("{} is not valid UTF-8", file_path.display()))?;
    let parsed: RepoSandboxFile = toml::from_str(text)
        .map_err(|e| format!("sandbox_invalid: {}: {e}", file_path.display()))?;
    Ok(Some((file_path, parsed, digest)))
}

fn repo_identity(repo_dir: &Path) -> Result<(String, u64, u64), String> {
    let key_dir = detect_git_common_dir(repo_dir).unwrap_or_else(|| {
        repo_dir
            .canonicalize()
            .unwrap_or_else(|_| repo_dir.to_path_buf())
    });
    let meta = fs::metadata(&key_dir).map_err(|e| format!("{}: {e}", key_dir.display()))?;
    Ok((
        key_dir.to_string_lossy().into_owned(),
        meta.dev(),
        meta.ino(),
    ))
}

/// Returns `Some(RepoSandboxFile)` ONLY if `.masil/sandbox.toml` exists AND its
/// current SHA-256 matches the user's recorded trust entry for this repository.
pub fn load_trusted_repo_sandbox(worktree: &Path) -> Option<RepoSandboxFile> {
    let repo_root = crate::changes::root_of(worktree).unwrap_or_else(|_| worktree.to_path_buf());
    let (_, parsed, digest) = read_repo_sandbox(&repo_root).ok()??;
    let (key, dev, ino) = repo_identity(&repo_root).ok()?;
    let store = load_trust_store();
    let entry = store.entries.get(&key)?;
    if entry.dev == dev && entry.ino == ino && entry.sha256 == digest {
        Some(parsed)
    } else {
        None
    }
}

/// Top-level CLI entrypoint: `masil-agent sandbox status|trust|untrust ...`.
pub fn run(args: &[String]) -> Result<i32, String> {
    let Some(sub) = args.first().map(String::as_str) else {
        println!("{SANDBOX_HELP}");
        return Ok(0);
    };
    if matches!(sub, "--help" | "-h" | "help") {
        println!("{SANDBOX_HELP}");
        return Ok(0);
    }
    let rest = &args[1..];
    match sub {
        "status" => {
            let repo_arg = rest
                .windows(2)
                .find(|w| w[0] == "--repo")
                .map(|w| PathBuf::from(&w[1]));
            let st = status();
            let repo_info = repo_arg.map(|p| {
                let root = crate::changes::root_of(&p).unwrap_or(p);
                let found = read_repo_sandbox(&root).ok().flatten();
                let trusted = load_trusted_repo_sandbox(&root).is_some();
                json!({
                    "repo": root,
                    "has_repo_sandbox_file": found.is_some(),
                    "sha256": found.as_ref().map(|(_, _, d)| d),
                    "trusted": trusted,
                })
            });
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "sandbox": st,
                    "repo": repo_info,
                }))
                .map_err(|e| e.to_string())?
            );
            Ok(0)
        }
        "trust" => {
            let mut repo_arg: Option<String> = None;
            let mut confirm: Option<String> = None;
            let mut i = 0;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--confirm" => {
                        confirm = Some(
                            rest.get(i + 1)
                                .ok_or("usage: --confirm requires a SHA256 digest")?
                                .clone(),
                        );
                        i += 2;
                    }
                    other if other.starts_with('-') => {
                        return Err(format!("usage: unknown sandbox trust option {other}"));
                    }
                    val if repo_arg.is_none() => {
                        repo_arg = Some(val.to_owned());
                        i += 1;
                    }
                    other => return Err(format!("usage: unexpected argument {other}")),
                }
            }
            let repo_path =
                PathBuf::from(repo_arg.ok_or("usage: sandbox trust <REPO> [--confirm SHA256]")?);
            let root = crate::changes::root_of(&repo_path).unwrap_or(repo_path);
            let Some((file_path, parsed, digest)) = read_repo_sandbox(&root)? else {
                return Err(format!(
                    "sandbox_missing: {} does not exist",
                    root.join(REPO_SANDBOX_FILE).display()
                ));
            };
            let Some(given) = confirm else {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "stage": "review_required",
                        "file": file_path,
                        "sha256": digest,
                        "config": parsed,
                        "hint": format!(
                            "Inspect the paths above, then re-run: masil-agent sandbox trust {} --confirm {digest}",
                            root.display()
                        ),
                    }))
                    .map_err(|e| e.to_string())?
                );
                return Ok(0);
            };
            if given.trim().to_ascii_lowercase() != digest {
                return Err(format!(
                    "sandbox_digest_mismatch: expected {digest}, got {}",
                    given.trim()
                ));
            }
            let (key, dev, ino) = repo_identity(&root)?;
            let mut store = load_trust_store();
            store.entries.insert(
                key.clone(),
                TrustEntry {
                    dev,
                    ino,
                    sha256: digest.clone(),
                    trusted_ms: crate::observation::now_ms(),
                },
            );
            save_trust_store(&store)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "stage": "trusted",
                    "repo": root,
                    "key": key,
                    "sha256": digest,
                }))
                .map_err(|e| e.to_string())?
            );
            Ok(0)
        }
        "untrust" => {
            let repo_arg = rest.first().ok_or("usage: sandbox untrust <REPO>")?;
            let root = crate::changes::root_of(Path::new(repo_arg))
                .unwrap_or_else(|_| PathBuf::from(repo_arg));
            let (key, _, _) = repo_identity(&root)?;
            let mut store = load_trust_store();
            let removed = store.entries.remove(&key).is_some();
            if removed {
                save_trust_store(&store)?;
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "stage": "untrusted",
                    "repo": root,
                    "removed": removed,
                }))
                .map_err(|e| e.to_string())?
            );
            Ok(0)
        }
        other => Err(format!(
            "usage: unknown sandbox command `{other}`\n{SANDBOX_HELP}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn bwrap_args_enforce_ro_root_worktree_bind_and_secret_tmpfs_masks() {
        let dir = std::env::temp_dir().join(format!("masil-bwrap-test-{}", std::process::id()));
        let wt = dir.join("wt");
        let ssh = dir.join(".ssh");
        let tmp = dir.join("sbx-tmp");
        fs::create_dir_all(&wt).unwrap();
        fs::create_dir_all(&ssh).unwrap();
        fs::create_dir_all(&tmp).unwrap();

        let spec = SandboxSpec {
            profile: SandboxProfile::NetOff,
            worktree: wt.clone(),
            private_tmp_dir: tmp.clone(),
            writable_dirs: vec![wt.clone(), tmp.clone()],
            writable_files: vec![],
            denied_read_dirs: vec![ssh.clone()],
            denied_read_files: vec![],
        };
        let args = bwrap::build_bwrap_args(&spec, "/bin/echo", &["hi".into()]);
        assert!(args.contains(&"--ro-bind".into()));
        assert!(args.contains(&"--die-with-parent".into()));
        assert!(args.contains(&"--unshare-net".into()));
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--tmpfs" && w[1] == ssh.to_string_lossy())
        );
        assert!(
            args.windows(3)
                .any(|w| w[0] == "--bind" && w[1] == wt.to_string_lossy())
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_seatbelt_empirically_blocks_ssh_reads_and_outside_writes_while_allowing_worktree() {
        assert!(
            seatbelt::is_available(),
            "macOS must have /usr/bin/sandbox-exec"
        );
        let base = std::env::temp_dir().join(format!(
            "masil-seatbelt-emp-{}-{}",
            std::process::id(),
            crate::observation::now_ms()
        ));
        fs::create_dir_all(&base).unwrap();
        let base = base.canonicalize().unwrap();
        let wt = base.join("worktree");
        let sibling = base.join("sibling-project");
        let fake_ssh = base.join("fake-ssh");
        let run_tmp = base.join("run-tmp");
        fs::create_dir_all(&wt).unwrap();
        fs::create_dir_all(&sibling).unwrap();
        fs::create_dir_all(&fake_ssh).unwrap();
        fs::create_dir_all(&run_tmp).unwrap();
        let secret_file = fake_ssh.join("id_ed25519");
        fs::write(&secret_file, "PRIVATE-KEY-MATERIAL\n").unwrap();

        let spec = SandboxSpec {
            profile: SandboxProfile::Worktree,
            worktree: wt.clone(),
            private_tmp_dir: run_tmp.clone(),
            writable_dirs: vec![wt.clone(), run_tmp.clone()],
            writable_files: vec![],
            denied_read_dirs: vec![fake_ssh.clone()],
            denied_read_files: vec![],
        };

        // 1. Reading secret directory MUST fail with EPERM.
        let wrapped_read = seatbelt::wrap(
            &spec,
            "/bin/cat",
            &[secret_file.to_string_lossy().into_owned()],
        )
        .unwrap();
        let out_read = Command::new(&wrapped_read.program)
            .args(&wrapped_read.args)
            .output()
            .unwrap();
        assert!(
            !out_read.status.success(),
            "Seatbelt MUST block reading denied secret dir"
        );
        assert!(
            !String::from_utf8_lossy(&out_read.stdout).contains("PRIVATE-KEY-MATERIAL"),
            "secret bytes must never leak"
        );

        // 2. Writing to sibling directory outside worktree MUST fail with EPERM.
        let outside_target = sibling.join("pwned.txt");
        let wrapped_out_write = seatbelt::wrap(
            &spec,
            "/bin/sh",
            &[
                "-c".into(),
                format!("echo nope > '{}'", outside_target.display()),
            ],
        )
        .unwrap();
        let out_write = Command::new(&wrapped_out_write.program)
            .args(&wrapped_out_write.args)
            .output()
            .unwrap();
        assert!(
            !out_write.status.success(),
            "Seatbelt MUST block writes outside worktree"
        );
        assert!(!outside_target.exists());

        // 3. Writing inside the worktree MUST succeed, and launch overhead stays fast.
        let inside_target = wt.join("ok.txt");
        let started = std::time::Instant::now();
        let wrapped_in_write = seatbelt::wrap(
            &spec,
            "/bin/sh",
            &[
                "-c".into(),
                format!("echo ok > '{}'", inside_target.display()),
            ],
        )
        .unwrap();
        let in_write = Command::new(&wrapped_in_write.program)
            .args(&wrapped_in_write.args)
            .output()
            .unwrap();
        let elapsed_ms = started.elapsed().as_millis();
        assert!(
            in_write.status.success(),
            "Seatbelt must allow writes inside worktree: {}",
            String::from_utf8_lossy(&in_write.stderr)
        );
        assert_eq!(fs::read_to_string(&inside_target).unwrap().trim(), "ok");
        assert!(
            elapsed_ms < 500,
            "seatbelt launch + sh execution took {elapsed_ms}ms"
        );

        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn resolve_start_profile_honors_default_on_and_opt_out() {
        let def = SandboxConfig::default();
        assert!(def.default_enabled);
        assert_eq!(
            resolve_start_profile(None, false, &def).unwrap(),
            SandboxProfile::Worktree
        );
        assert_eq!(
            resolve_start_profile(None, true, &def).unwrap(),
            SandboxProfile::Off
        );
        assert_eq!(
            resolve_start_profile(Some("strict"), false, &def).unwrap(),
            SandboxProfile::Strict
        );
        assert_eq!(
            resolve_start_profile(Some("net-off"), false, &def).unwrap(),
            SandboxProfile::NetOff
        );
    }
}
