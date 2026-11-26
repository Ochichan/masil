//! macOS Seatbelt (`/usr/bin/sandbox-exec`) backend for the Phase 3A OS Micro-Sandbox (D-11).
//!
//! `sandbox-exec` applies the SBPL profile in-process and `exec()`s the target
//! provider directly, preserving PID, foreground process group, and `pane_current_command`
//! with sub-15ms overhead.

use super::{SandboxProfile, SandboxSpec, WrappedCommand};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub const SANDBOX_EXEC_BIN: &str = "/usr/bin/sandbox-exec";

pub fn is_available() -> bool {
    cfg!(target_os = "macos") && Path::new(SANDBOX_EXEC_BIN).is_file()
}

/// Renders a minimal, practical SBPL v1 profile for the given `SandboxSpec`.
pub fn render_sbpl_profile(spec: &SandboxSpec) -> Result<String, String> {
    let mut lines = vec![
        "(version 1)".to_owned(),
        "(allow default)".to_owned(),
        ";; D-11: deny reading sensitive user credential stores at kernel level".to_owned(),
    ];

    let mut deny_read_rules = Vec::new();
    for path in &spec.denied_read_dirs {
        deny_read_rules.push(format!("  (subpath \"{}\")", escape_sbpl(path)?));
        if let Some(real) = canonical_if_diff(path) {
            deny_read_rules.push(format!("  (subpath \"{}\")", escape_sbpl(&real)?));
        }
    }
    for path in &spec.denied_read_files {
        deny_read_rules.push(format!("  (literal \"{}\")", escape_sbpl(path)?));
        if let Some(real) = canonical_if_diff(path) {
            deny_read_rules.push(format!("  (literal \"{}\")", escape_sbpl(&real)?));
        }
    }
    if !deny_read_rules.is_empty() {
        lines.push("(deny file-read*".to_owned());
        lines.extend(deny_read_rules);
        lines.push(")".to_owned());
    }

    lines.push(";; D-11: deny all writes outside explicitly allowlisted paths".to_owned());
    lines.push("(deny file-write*)".to_owned());
    lines.push("(allow file-write*".to_owned());

    for path in &spec.writable_dirs {
        lines.push(format!("  (subpath \"{}\")", escape_sbpl(path)?));
        if let Some(real) = canonical_if_diff(path) {
            lines.push(format!("  (subpath \"{}\")", escape_sbpl(&real)?));
        }
    }
    for path in &spec.writable_files {
        lines.push(format!("  (literal \"{}\")", escape_sbpl(path)?));
        if let Some(real) = canonical_if_diff(path) {
            lines.push(format!("  (literal \"{}\")", escape_sbpl(&real)?));
        }
    }

    // Standard character devices & PTY slaves required for interactive terminal agents.
    lines.push("  (literal \"/dev/null\")".to_owned());
    lines.push("  (literal \"/dev/zero\")".to_owned());
    lines.push("  (literal \"/dev/tty\")".to_owned());
    lines.push("  (literal \"/dev/ptmx\")".to_owned());
    lines.push("  (literal \"/dev/dtracehelper\")".to_owned());
    lines.push("  (regex #\"^/dev/ttys[0-9]+$\")".to_owned());
    lines.push("  (regex #\"^/dev/fd/[0-9]+$\")".to_owned());
    lines.push(")".to_owned());

    if spec.profile == SandboxProfile::NetOff {
        lines.push(
            ";; D-11 net-off profile: block outbound IP network while keeping local UNIX IPC"
                .to_owned(),
        );
        lines.push("(deny network-outbound (remote ip))".to_owned());
    }

    lines.push(String::new());
    Ok(lines.join("\n"))
}

/// Writes the SBPL profile into the per-run private directory and wraps `program` + `args`.
pub fn wrap(spec: &SandboxSpec, program: &str, args: &[String]) -> Result<WrappedCommand, String> {
    let profile_text = render_sbpl_profile(spec)?;
    fs::create_dir_all(&spec.private_tmp_dir)
        .map_err(|e| format!("creating sandbox temp dir: {e}"))?;
    let _ = fs::set_permissions(&spec.private_tmp_dir, fs::Permissions::from_mode(0o700));
    let profile_path = spec.private_tmp_dir.join("profile.sb");
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&profile_path)
        .map_err(|e| format!("writing seatbelt profile: {e}"))?;
    file.write_all(profile_text.as_bytes())
        .map_err(|e| format!("writing seatbelt profile: {e}"))?;

    let mut wrapped_args = vec![
        "-f".to_owned(),
        profile_path.to_string_lossy().into_owned(),
        program.to_owned(),
    ];
    wrapped_args.extend(args.iter().cloned());

    Ok(WrappedCommand {
        program: SANDBOX_EXEC_BIN.to_owned(),
        args: wrapped_args,
        extra_env: vec![(
            "TMPDIR".to_owned(),
            spec.private_tmp_dir.to_string_lossy().into_owned(),
        )],
        badge: format!("sandboxed:seatbelt:{}", spec.profile.as_str()),
        profile_path: Some(profile_path),
    })
}

fn canonical_if_diff(path: &Path) -> Option<PathBuf> {
    // Also resolve parent when leaf doesn't exist yet (macOS `/var` -> `/private/var`, `/tmp` -> `/private/tmp`).
    if let Ok(canon) = path.canonicalize() {
        return (canon != path).then_some(canon);
    }
    let parent = path.parent()?;
    let name = path.file_name()?;
    let canon_parent = parent.canonicalize().ok()?;
    let joined = canon_parent.join(name);
    (joined != path).then_some(joined)
}

fn escape_sbpl(path: &Path) -> Result<String, String> {
    let text = path.to_str().ok_or("sandbox path is not valid UTF-8")?;
    if text.contains('\0') || text.contains('\n') || text.contains('\r') {
        return Err("sandbox path contains forbidden control characters".into());
    }
    Ok(text.replace('\\', "\\\\").replace('"', "\\\""))
}
