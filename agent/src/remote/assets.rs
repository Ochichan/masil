//! Phase 2A (D-08): Mobile finger-markup screenshot upload storage, idempotent
//! `.git/info/exclude` registration, universal 24-provider structured prompt
//! formatting, and 24-hour automatic cleanup.

use crate::remote::config::{now_ms, random_token_b64, sha256_hex};
use masil_wire::frames::{ASSET_MAX_UPLOAD_BYTES, AssetUploadEndPayload, AssetUploadMeta};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

pub const MARKUP_TMP_DIR_NAME: &str = ".masil-tmp";
pub const MARKUP_RETENTION_SECS: u64 = 24 * 60 * 60;

pub struct AssetUploadBuffer {
    bytes: Vec<u8>,
}

impl AssetUploadBuffer {
    pub fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<(), String> {
        if self.bytes.len().saturating_add(chunk.len()) > ASSET_MAX_UPLOAD_BYTES {
            return Err("asset upload exceeds 10 MiB limit".into());
        }
        self.bytes.extend_from_slice(chunk);
        Ok(())
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

#[derive(Debug, Clone)]
pub struct SavedMarkupAsset {
    pub absolute_path: PathBuf,
    pub prompt_path: String,
    pub in_git_worktree: bool,
    pub size_bytes: usize,
    pub structured_prompt: String,
}

fn extension_for_mime(mime: &str, filename: &str) -> Result<&'static str, String> {
    let m = mime.trim().to_ascii_lowercase();
    match m.as_str() {
        "image/png" => Ok("png"),
        "image/jpeg" | "image/jpg" => Ok("jpg"),
        "image/webp" => Ok("webp"),
        _ => {
            let lower = filename.trim().to_ascii_lowercase();
            if lower.ends_with(".png") {
                Ok("png")
            } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
                Ok("jpg")
            } else if lower.ends_with(".webp") {
                Ok("webp")
            } else {
                Err(format!("unsupported image mime type: {mime}"))
            }
        }
    }
}

fn validate_image_bytes(bytes: &[u8], ext: &str) -> Result<(), String> {
    if bytes.is_empty() {
        return Err("empty image upload".into());
    }
    if bytes.len() > ASSET_MAX_UPLOAD_BYTES {
        return Err("asset upload exceeds 10 MiB limit".into());
    }
    let ok = match ext {
        "png" => {
            bytes.len() >= 8 && bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A])
        }
        "jpg" => bytes.len() >= 3 && bytes.starts_with(&[0xFF, 0xD8, 0xFF]),
        "webp" => bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP",
        _ => false,
    };
    if !ok {
        return Err(format!("uploaded bytes do not match {ext} image header"));
    }
    Ok(())
}

fn resolve_git_worktree_and_git_dir(cwd: &Path) -> Option<(PathBuf, PathBuf)> {
    let top_out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !top_out.status.success() {
        return None;
    }
    let worktree = PathBuf::from(String::from_utf8_lossy(&top_out.stdout).trim());
    if worktree.as_os_str().is_empty() {
        return None;
    }
    let gd_out = Command::new("git")
        .arg("-C")
        .arg(&worktree)
        .args(["rev-parse", "--git-dir"])
        .output()
        .ok()?;
    if !gd_out.status.success() {
        return None;
    }
    let raw_gd = PathBuf::from(String::from_utf8_lossy(&gd_out.stdout).trim());
    let git_dir = if raw_gd.is_absolute() {
        raw_gd
    } else {
        worktree.join(raw_gd)
    };
    Some((worktree, git_dir))
}

/// Idempotently appends `.masil-tmp/` to `<git-dir>/info/exclude` so markup
/// screenshots never pollute `git status` or `.gitignore` (D-08).
pub fn ensure_git_info_exclude(git_dir: &Path) -> Result<(), String> {
    let info_dir = git_dir.join("info");
    fs::create_dir_all(&info_dir)
        .map_err(|e| format!("cannot create {}: {e}", info_dir.display()))?;
    let exclude_path = info_dir.join("exclude");
    let existing = fs::read_to_string(&exclude_path).unwrap_or_default();
    let needle = format!("{MARKUP_TMP_DIR_NAME}/");
    if existing
        .lines()
        .any(|line| line.trim() == needle || line.trim() == MARKUP_TMP_DIR_NAME)
    {
        return Ok(());
    }
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&exclude_path)
        .map_err(|e| format!("cannot open {}: {e}", exclude_path.display()))?;
    if !existing.is_empty() && !existing.ends_with('\n') {
        writeln!(f).map_err(|e| e.to_string())?;
    }
    writeln!(f, "{needle}").map_err(|e| e.to_string())?;
    Ok(())
}

fn current_uid() -> u32 {
    // SAFETY: getuid is always safe to call on Unix.
    unsafe { libc::getuid() }
}

fn format_timestamp_compact() -> String {
    // Use epoch millis + short random suffix for collision-free, lexicographic naming.
    let ms = now_ms();
    let suffix: String = random_token_b64(3)
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(4)
        .collect();
    format!("markup-{ms}-{suffix}")
}

/// Saves a mobile visual markup image to `<worktree>/.masil-tmp/` (with automatic
/// `.git/info/exclude`) or `/tmp/masil-markup-<uid>-<hash>/` for non-Git directories,
/// then formats the universal 24-provider structured feedback prompt (D-08).
pub fn save_markup_asset(
    agent_cwd: Option<&Path>,
    bytes: &[u8],
    end: &AssetUploadEndPayload,
) -> Result<SavedMarkupAsset, String> {
    let ext = extension_for_mime(&end.mime, &end.filename)?;
    validate_image_bytes(bytes, ext)?;

    let stem = format_timestamp_compact();
    let file_name = format!("{stem}.{ext}");

    let (target_dir, prompt_path, in_git_worktree) = match agent_cwd
        .filter(|p| p.exists() && p.is_dir())
        .and_then(|cwd| resolve_git_worktree_and_git_dir(cwd).map(|g| (cwd, g)))
    {
        Some((_cwd, (worktree, git_dir))) => {
            ensure_git_info_exclude(&git_dir)?;
            let dir = worktree.join(MARKUP_TMP_DIR_NAME);
            fs::create_dir_all(&dir)
                .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
            (dir, format!("{MARKUP_TMP_DIR_NAME}/{file_name}"), true)
        }
        None => {
            let key = agent_cwd
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| end.agent_id.clone());
            let hash = &sha256_hex(key.as_bytes())[..12];
            let dir = PathBuf::from(format!("/tmp/masil-markup-{}-{hash}", current_uid()));
            fs::create_dir_all(&dir)
                .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
            let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
            let abs = dir.join(&file_name);
            (dir, abs.to_string_lossy().into_owned(), false)
        }
    };

    let absolute_path = target_dir.join(&file_name);
    {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&absolute_path)
            .map_err(|e| format!("cannot write {}: {e}", absolute_path.display()))?;
        f.write_all(bytes)
            .map_err(|e| format!("cannot write image bytes: {e}"))?;
        let _ = f.sync_all();
    }

    let structured_prompt =
        build_structured_visual_prompt(&prompt_path, end.prompt.as_deref(), &end.meta);

    Ok(SavedMarkupAsset {
        absolute_path,
        prompt_path,
        in_git_worktree,
        size_bytes: bytes.len(),
        structured_prompt,
    })
}

/// Formats the universal 24-provider structured visual feedback prompt (D-08).
pub fn build_structured_visual_prompt(
    image_path: &str,
    user_prompt: Option<&str>,
    meta: &AssetUploadMeta,
) -> String {
    let mut lines = Vec::new();
    lines.push("[masil Mobile Visual Feedback]".to_owned());

    if let Some(url) = meta.url.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let mut details = Vec::new();
        if let Some(vp) = meta
            .viewport
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            details.push(format!("viewport: {vp}"));
        }
        if let Some(th) = meta
            .theme
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            details.push(th.to_owned());
        }
        if details.is_empty() {
            lines.push(format!("URL: {url}"));
        } else {
            lines.push(format!("URL: {url} ({})", details.join(", ")));
        }
    }

    lines.push(format!("Annotated screenshot: {image_path}"));

    let mut errors: Vec<&str> = Vec::new();
    for e in &meta.console_errors {
        let t = e.trim();
        if !t.is_empty() {
            errors.push(t);
        }
    }
    for e in &meta.network_errors {
        let t = e.trim();
        if !t.is_empty() {
            errors.push(t);
        }
    }
    if !errors.is_empty() {
        let shown = errors.len().min(20);
        lines.push(format!("Recent console/network errors ({shown}):"));
        for err in errors.into_iter().take(20) {
            lines.push(format!("  - {}", err.replace('\n', " ")));
        }
    }

    let instr = user_prompt.unwrap_or("").trim();
    if !instr.is_empty() {
        lines.push("User instruction:".to_owned());
        for row in instr.lines() {
            lines.push(format!("  {row}"));
        }
    }

    lines.join("\n")
}

/// Removes `markup-*` temporary images older than `max_age` from both `/tmp/masil-markup-<uid>-*`
/// and every provided worktree's `.masil-tmp/` directory (D-08: 24h auto-cleanup).
pub fn cleanup_expired_markups(worktree_roots: &[PathBuf], max_age: Duration) -> usize {
    let mut removed = 0usize;
    let now = SystemTime::now();

    let mut dirs: Vec<PathBuf> = worktree_roots
        .iter()
        .map(|root| root.join(MARKUP_TMP_DIR_NAME))
        .collect();

    let tmp_prefix = format!("masil-markup-{}-", current_uid());
    if let Ok(entries) = fs::read_dir("/tmp") {
        for ent in entries.flatten() {
            if ent.file_name().to_string_lossy().starts_with(&tmp_prefix) && ent.path().is_dir() {
                dirs.push(ent.path());
            }
        }
    }

    for dir in dirs {
        let Ok(files) = fs::read_dir(&dir) else {
            continue;
        };
        for file in files.flatten() {
            let name = file.file_name();
            if !name.to_string_lossy().starts_with("markup-") {
                continue;
            }
            let Ok(meta) = file.metadata() else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            let Ok(modified) = meta.modified() else {
                continue;
            };
            if let Ok(age) = now.duration_since(modified)
                && age >= max_age
                && fs::remove_file(file.path()).is_ok()
            {
                removed += 1;
            }
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use masil_wire::frames::AssetDeliveryMode;

    const MINIMAL_PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52,
    ];

    #[test]
    fn saves_inside_git_worktree_with_clean_git_status_and_idempotent_exclude() {
        let tmp = std::env::temp_dir().join(format!(
            "masil-asset-git-test-{}-{}",
            std::process::id(),
            now_ms()
        ));
        fs::create_dir_all(&tmp).unwrap();
        let init = Command::new("git")
            .args(["init", "-q"])
            .current_dir(&tmp)
            .status()
            .unwrap();
        assert!(init.success());

        let end = AssetUploadEndPayload {
            agent_id: "%1".into(),
            mime: "image/png".into(),
            filename: "markup.png".into(),
            delivery_mode: AssetDeliveryMode::Queue,
            prompt: Some("Increase contrast on the red box.".into()),
            meta: AssetUploadMeta {
                url: Some("http://localhost:5173/dashboard".into()),
                viewport: Some("390x844".into()),
                theme: Some("dark".into()),
                server_socket: None,
                cwd: None,
                console_errors: vec!["TypeError: Cannot read properties of undefined".into()],
                network_errors: vec!["GET /api/metrics -> 500".into()],
                annotations_count: 1,
            },
        };

        let saved1 = save_markup_asset(Some(&tmp), MINIMAL_PNG, &end).unwrap();
        assert!(saved1.in_git_worktree);
        assert!(saved1.prompt_path.starts_with(".masil-tmp/markup-"));
        assert!(saved1.absolute_path.exists());
        assert!(
            saved1
                .structured_prompt
                .contains("[masil Mobile Visual Feedback]")
        );
        assert!(
            saved1
                .structured_prompt
                .contains("URL: http://localhost:5173/dashboard (viewport: 390x844, dark)")
        );
        assert!(
            saved1
                .structured_prompt
                .contains("Recent console/network errors (2):")
        );

        // Save a second time to verify `.git/info/exclude` is idempotent.
        let saved2 = save_markup_asset(Some(&tmp), MINIMAL_PNG, &end).unwrap();
        assert!(saved2.absolute_path.exists());
        let exclude = fs::read_to_string(tmp.join(".git/info/exclude")).unwrap();
        let occurrences = exclude
            .lines()
            .filter(|l| l.trim() == ".masil-tmp/")
            .count();
        assert_eq!(occurrences, 1);

        // `git status --porcelain` must stay completely clean!
        let status = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&tmp)
            .output()
            .unwrap();
        assert!(status.status.success());
        assert!(
            String::from_utf8_lossy(&status.stdout).trim().is_empty(),
            "expected clean git status, got: {}",
            String::from_utf8_lossy(&status.stdout)
        );

        // Cleanup with 0s retention removes both files.
        let removed = cleanup_expired_markups(std::slice::from_ref(&tmp), Duration::from_secs(0));
        assert!(removed >= 2);
        assert!(!saved1.absolute_path.exists());
        assert!(!saved2.absolute_path.exists());

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn falls_back_to_tmp_for_non_git_directory_and_rejects_bad_magic() {
        let non_git = std::env::temp_dir().join(format!(
            "masil-asset-nongit-{}-{}",
            std::process::id(),
            now_ms()
        ));
        fs::create_dir_all(&non_git).unwrap();

        let end = AssetUploadEndPayload {
            agent_id: "%2".into(),
            mime: "image/png".into(),
            filename: "shot.png".into(),
            delivery_mode: AssetDeliveryMode::Prefill,
            prompt: None,
            meta: AssetUploadMeta::default(),
        };

        assert!(save_markup_asset(Some(&non_git), b"not-a-png", &end).is_err());

        let saved = save_markup_asset(Some(&non_git), MINIMAL_PNG, &end).unwrap();
        assert!(!saved.in_git_worktree);
        assert!(saved.prompt_path.starts_with("/tmp/masil-markup-"));
        assert!(saved.absolute_path.exists());

        let _ = fs::remove_file(&saved.absolute_path);
        if let Some(parent) = saved.absolute_path.parent() {
            let _ = fs::remove_dir_all(parent);
        }
        let _ = fs::remove_dir_all(&non_git);
    }
}
