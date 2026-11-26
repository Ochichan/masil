//! One-click **Shadow Integration Check** (`agent/src/collision/shadow.rs`):
//! materializes the quarantined merged tree of two worktrees into a temporary
//! `/tmp/masil-shadow-<hash>` checkout, runs the user-requested verification
//! command, formats the output into a Masil UI Spec v1 `UiDocument`, and
//! guarantees immediate cleanup via RAII drop guard.

use super::merge_tree::{
    QuarantineDir, common_objects_dir, git_base, git_stdout, snapshot_worktree_in_quarantine,
};
use crate::plugin::ui_spec::{UI_SPEC_VERSION, UiBlock, UiDocument, UiMetric, UiTone};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const MAX_OUTPUT_BYTES: usize = 32 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ShadowCheckReport {
    pub agent_a: String,
    pub agent_b: String,
    pub command: String,
    pub merge_clean: bool,
    pub conflict_files: Vec<String>,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    pub output_tail: String,
    pub ui: UiDocument,
}

struct ShadowTempDir(PathBuf);

impl ShadowTempDir {
    fn new() -> Result<Self, String> {
        let nonce = crate::managed::nonce()
            .unwrap_or_else(|_| format!("{}-{}", std::process::id(), crate::observation::now_ms()));
        let path = PathBuf::from(format!("/tmp/masil-shadow-{nonce}"));
        fs::create_dir_all(&path).map_err(|e| e.to_string())?;
        Ok(Self(path))
    }
}

impl Drop for ShadowTempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Runs a shadow integration check merging `worktree_a` and `worktree_b` into
/// a temporary checkout and executing `cmd` inside it.
pub fn run_shadow_check(
    agent_a: &str,
    worktree_a: &Path,
    agent_b: &str,
    worktree_b: &Path,
    cmd: &str,
    timeout: Duration,
) -> Result<ShadowCheckReport, String> {
    let started = Instant::now();
    let common_objects = common_objects_dir(worktree_a)?;
    let quarantine = QuarantineDir::new("masil-shadow-q")?;
    let (head_a, tree_a) =
        snapshot_worktree_in_quarantine(worktree_a, &common_objects, &quarantine, "a")?;
    let (head_b, tree_b) =
        snapshot_worktree_in_quarantine(worktree_b, &common_objects, &quarantine, "b")?;
    let merge_base = git_stdout(worktree_a, &["merge-base", &head_a, &head_b])
        .unwrap_or_else(|_| head_a.clone());

    let mt = git_base(worktree_a)
        .env("GIT_OBJECT_DIRECTORY", quarantine.objects_dir())
        .env("GIT_ALTERNATE_OBJECT_DIRECTORIES", &common_objects)
        .args([
            "merge-tree",
            "--write-tree",
            &format!("--merge-base={merge_base}"),
            &tree_a,
            &tree_b,
        ])
        .output()
        .map_err(|e| e.to_string())?;

    let mt_stdout = String::from_utf8_lossy(&mt.stdout);
    let mut lines = mt_stdout.lines();
    let merged_tree = lines.next().unwrap_or("").trim().to_owned();

    if !mt.status.success() || merged_tree.is_empty() {
        let conflict_files: Vec<String> = lines
            .filter(|l| l.trim().starts_with("CONFLICT"))
            .map(|l| l.trim().to_owned())
            .collect();
        let duration_ms = started.elapsed().as_millis() as u64;
        let ui = build_ui(
            agent_a,
            agent_b,
            cmd,
            false,
            &conflict_files,
            None,
            duration_ms,
            &conflict_files.join("\n"),
        );
        return Ok(ShadowCheckReport {
            agent_a: agent_a.to_owned(),
            agent_b: agent_b.to_owned(),
            command: cmd.to_owned(),
            merge_clean: false,
            conflict_files,
            exit_code: None,
            duration_ms,
            output_tail: "Hard merge conflict prevented shadow integration run.".into(),
            ui,
        });
    }

    // Materialize `merged_tree` into `/tmp/masil-shadow-<nonce>` via `git archive`
    // (reads from quarantine + alternates, writes 0 bytes to user repo).
    let shadow_dir = ShadowTempDir::new()?;
    let archive_out = git_base(worktree_a)
        .env("GIT_OBJECT_DIRECTORY", quarantine.objects_dir())
        .env("GIT_ALTERNATE_OBJECT_DIRECTORIES", &common_objects)
        .args(["archive", "--format=tar", &merged_tree])
        .output()
        .map_err(|e| e.to_string())?;
    if !archive_out.status.success() {
        return Err(format!(
            "shadow_archive_failed: {}",
            String::from_utf8_lossy(&archive_out.stderr).trim()
        ));
    }
    let mut tar = Command::new("tar")
        .args(["-xf", "-", "-C"])
        .arg(&shadow_dir.0)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    if let Some(mut stdin) = tar.stdin.take() {
        use std::io::Write;
        stdin
            .write_all(&archive_out.stdout)
            .map_err(|e| e.to_string())?;
    }
    let tar_status = tar.wait_with_output().map_err(|e| e.to_string())?;
    if !tar_status.status.success() {
        return Err(format!(
            "shadow_extract_failed: {}",
            String::from_utf8_lossy(&tar_status.stderr).trim()
        ));
    }

    // Run verification command inside the isolated shadow checkout.
    let mut child = Command::new("/bin/sh")
        .args(["-c", cmd])
        .current_dir(&shadow_dir.0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;

    let cmd_deadline = Instant::now() + timeout;
    let mut timed_out = false;
    loop {
        if child.try_wait().map_err(|e| e.to_string())?.is_some() {
            break;
        }
        if Instant::now() >= cmd_deadline {
            timed_out = true;
            let _ = child.kill();
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    let exit_code = if timed_out {
        Some(124)
    } else {
        output.status.code()
    };

    let mut combined = Vec::new();
    combined.extend_from_slice(&output.stdout);
    if !output.stderr.is_empty() {
        if !combined.is_empty() && !combined.ends_with(b"\n") {
            combined.push(b'\n');
        }
        combined.extend_from_slice(&output.stderr);
    }
    if timed_out {
        combined.extend_from_slice(b"\n[masil shadow-check: command timed out]\n");
    }
    let tail_slice = if combined.len() > MAX_OUTPUT_BYTES {
        &combined[combined.len() - MAX_OUTPUT_BYTES..]
    } else {
        &combined
    };
    let output_tail = String::from_utf8_lossy(tail_slice).trim().to_owned();
    let duration_ms = started.elapsed().as_millis() as u64;
    let ui = build_ui(
        agent_a,
        agent_b,
        cmd,
        true,
        &[],
        exit_code,
        duration_ms,
        &output_tail,
    );

    // `shadow_dir` drops here -> immediate cleanup of `/tmp/masil-shadow-*`.
    Ok(ShadowCheckReport {
        agent_a: agent_a.to_owned(),
        agent_b: agent_b.to_owned(),
        command: cmd.to_owned(),
        merge_clean: true,
        conflict_files: Vec::new(),
        exit_code,
        duration_ms,
        output_tail,
        ui,
    })
}

#[allow(clippy::too_many_arguments)]
fn build_ui(
    agent_a: &str,
    agent_b: &str,
    cmd: &str,
    merge_clean: bool,
    conflict_files: &[String],
    exit_code: Option<i32>,
    duration_ms: u64,
    output_tail: &str,
) -> UiDocument {
    let passed = merge_clean && exit_code == Some(0);
    let (badge, tone) = if !merge_clean {
        ("CONFLICT", UiTone::Danger)
    } else if passed {
        ("PASS", UiTone::Success)
    } else {
        ("FAIL", UiTone::Danger)
    };
    let mut blocks = vec![
        UiBlock::Header {
            title: format!("Shadow Check: {agent_a} + {agent_b}"),
            subtitle: Some(cmd.to_owned()),
            badge: Some(badge.into()),
            tone,
        },
        UiBlock::MetricRow {
            items: vec![
                UiMetric {
                    label: "Merge".into(),
                    value: if merge_clean {
                        "Clean".into()
                    } else {
                        format!("{} conflicts", conflict_files.len())
                    },
                    delta: None,
                    tone: if merge_clean {
                        UiTone::Success
                    } else {
                        UiTone::Danger
                    },
                },
                UiMetric {
                    label: "Exit".into(),
                    value: exit_code.map_or_else(|| "—".into(), |c| c.to_string()),
                    delta: None,
                    tone: if passed {
                        UiTone::Success
                    } else {
                        UiTone::Danger
                    },
                },
                UiMetric {
                    label: "Duration".into(),
                    value: format!("{duration_ms}ms"),
                    delta: None,
                    tone: UiTone::Muted,
                },
            ],
        },
    ];
    if !output_tail.is_empty() {
        blocks.push(UiBlock::Code {
            language: Some("text".into()),
            code: output_tail.to_owned(),
            copyable: true,
        });
    }
    UiDocument {
        v: UI_SPEC_VERSION,
        title: Some(format!("Shadow Check: {agent_a} + {agent_b}")),
        subtitle: Some(cmd.to_owned()),
        blocks,
        custom_surface: None,
    }
}
