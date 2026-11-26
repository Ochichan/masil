//! Linux `bubblewrap` (`bwrap`) backend for the Phase 3A OS Micro-Sandbox (D-11).

use super::{SandboxProfile, SandboxSpec, WrappedCommand};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub fn find_bwrap() -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join("bwrap");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    for fallback in ["/usr/bin/bwrap", "/bin/bwrap", "/usr/local/bin/bwrap"] {
        let p = Path::new(fallback);
        if p.is_file() {
            return Some(p.to_path_buf());
        }
    }
    None
}

pub fn is_available() -> bool {
    cfg!(target_os = "linux") && find_bwrap().is_some()
}

/// Builds the exact `bwrap` argument vector for `spec`, `program`, and `args`.
pub fn build_bwrap_args(spec: &SandboxSpec, program: &str, args: &[String]) -> Vec<String> {
    let mut out = vec![
        "--ro-bind".into(),
        "/".into(),
        "/".into(),
        "--dev".into(),
        "/dev".into(),
        "--proc".into(),
        "/proc".into(),
        "--tmpfs".into(),
        "/tmp".into(),
        "--die-with-parent".into(),
    ];

    if spec.profile == SandboxProfile::NetOff {
        out.push("--unshare-net".into());
    }

    for dir in &spec.writable_dirs {
        if dir.exists() {
            let s = dir.to_string_lossy().into_owned();
            out.extend(["--bind".into(), s.clone(), s]);
        }
    }
    for file in &spec.writable_files {
        if file.exists() {
            let s = file.to_string_lossy().into_owned();
            out.extend(["--bind".into(), s.clone(), s]);
        }
    }

    // Mask sensitive credential directories with empty tmpfs and files with /dev/null.
    for denied in &spec.denied_read_dirs {
        if denied.exists() {
            let s = denied.to_string_lossy().into_owned();
            out.extend(["--tmpfs".into(), s]);
        }
    }
    for denied_file in &spec.denied_read_files {
        if denied_file.exists() {
            let s = denied_file.to_string_lossy().into_owned();
            out.extend(["--ro-bind".into(), "/dev/null".into(), s]);
        }
    }

    out.push("--".into());
    out.push(program.to_owned());
    out.extend(args.iter().cloned());
    out
}

pub fn wrap(spec: &SandboxSpec, program: &str, args: &[String]) -> Result<WrappedCommand, String> {
    let bwrap = find_bwrap().ok_or_else(|| {
        "sandbox_unavailable: `bwrap` (bubblewrap) is not installed; install bubblewrap or pass --no-sandbox".to_owned()
    })?;
    fs::create_dir_all(&spec.private_tmp_dir)
        .map_err(|e| format!("creating sandbox temp dir: {e}"))?;
    let _ = fs::set_permissions(&spec.private_tmp_dir, fs::Permissions::from_mode(0o700));
    let wrapped_args = build_bwrap_args(spec, program, args);
    Ok(WrappedCommand {
        program: bwrap.to_string_lossy().into_owned(),
        args: wrapped_args,
        extra_env: vec![(
            "TMPDIR".to_owned(),
            spec.private_tmp_dir.to_string_lossy().into_owned(),
        )],
        badge: format!("sandboxed:bwrap:{}", spec.profile.as_str()),
        profile_path: None,
    })
}
