//! Official `masil-plugin` Agent Skill installer for Claude Code, Codex, and
//! Antigravity (`agy`), plus Creator Mode prompt builder.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

const SKILL_NAME: &str = "masil-plugin";
const SKILL_MARKER: &str = "<!-- masil-managed-skill:v1 -->";

const SKILL_MD: &str = include_str!("skill_bundle/SKILL.md");
const REF_UI_SPEC_MD: &str = include_str!("skill_bundle/references/ui-spec.md");
const REF_EXAMPLES_MD: &str = include_str!("skill_bundle/references/examples.md");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillTargetStatus {
    pub id: String,
    pub label: String,
    pub cli_installed: bool,
    pub skill_installed: bool,
    pub managed: bool,
    pub paths: Vec<String>,
}

fn home_dir() -> Result<PathBuf, String> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is not set".into())
}

fn cli_exists(bin: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(bin).is_file()))
}

fn claude_skill_dirs() -> Result<Vec<PathBuf>, String> {
    let base = if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|s| !s.is_empty()) {
        PathBuf::from(dir)
    } else {
        home_dir()?.join(".claude")
    };
    Ok(vec![base.join("skills").join(SKILL_NAME)])
}

fn codex_skill_dirs() -> Result<Vec<PathBuf>, String> {
    let home = home_dir()?;
    let codex_base = if let Some(dir) = std::env::var_os("CODEX_HOME").filter(|s| !s.is_empty()) {
        PathBuf::from(dir)
    } else {
        home.join(".codex")
    };
    Ok(vec![
        codex_base.join("skills").join(SKILL_NAME),
        home.join(".agents").join("skills").join(SKILL_NAME),
    ])
}

fn agy_skill_dirs() -> Result<Vec<PathBuf>, String> {
    Ok(vec![
        home_dir()?
            .join(".gemini")
            .join("config")
            .join("skills")
            .join(SKILL_NAME),
    ])
}

fn target_dirs(target: &str) -> Result<Vec<PathBuf>, String> {
    match target {
        "claude" | "claude-code" => claude_skill_dirs(),
        "codex" => codex_skill_dirs(),
        "agy" | "antigravity" => agy_skill_dirs(),
        other => Err(format!(
            "unsupported skill target '{other}' (expected claude, codex, agy, or all)"
        )),
    }
}

fn dir_has_skill(dir: &Path) -> (bool, bool) {
    let skill_md = dir.join("SKILL.md");
    if !skill_md.is_file() {
        return (false, false);
    }
    let managed = fs::read_to_string(&skill_md)
        .map(|s| s.contains(SKILL_MARKER))
        .unwrap_or(false);
    (true, managed)
}

pub fn inspect_all() -> Vec<SkillTargetStatus> {
    let specs: &[(&str, &str, &str)] = &[
        ("claude", "Claude Code", "claude"),
        ("codex", "Codex", "codex"),
        ("agy", "Antigravity (agy)", "agy"),
    ];
    let mut out = Vec::new();
    for (id, label, bin) in specs {
        let Ok(dirs) = target_dirs(id) else {
            continue;
        };
        let mut all_installed = !dirs.is_empty();
        let mut all_managed = !dirs.is_empty();
        for dir in &dirs {
            let (installed, managed) = dir_has_skill(dir);
            all_installed &= installed;
            all_managed &= managed;
        }
        out.push(SkillTargetStatus {
            id: (*id).to_owned(),
            label: (*label).to_owned(),
            cli_installed: cli_exists(bin),
            skill_installed: all_installed,
            managed: all_managed,
            paths: dirs
                .into_iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect(),
        });
    }
    out
}

fn write_skill_bundle(dir: &Path, force: bool) -> Result<(), String> {
    let skill_md = dir.join("SKILL.md");
    if skill_md.exists() && !force {
        let existing = fs::read_to_string(&skill_md).unwrap_or_default();
        if !existing.contains(SKILL_MARKER) {
            return Err(format!(
                "{} exists and is not managed by masil (pass --force to overwrite)",
                skill_md.display()
            ));
        }
    }
    let refs_dir = dir.join("references");
    fs::create_dir_all(&refs_dir)
        .map_err(|e| format!("cannot create {}: {e}", refs_dir.display()))?;
    fs::write(&skill_md, SKILL_MD)
        .map_err(|e| format!("cannot write {}: {e}", skill_md.display()))?;
    fs::write(refs_dir.join("ui-spec.md"), REF_UI_SPEC_MD)
        .map_err(|e| format!("cannot write ui-spec.md: {e}"))?;
    fs::write(refs_dir.join("examples.md"), REF_EXAMPLES_MD)
        .map_err(|e| format!("cannot write examples.md: {e}"))?;
    Ok(())
}

pub fn install_target(target: &str, force: bool) -> Result<Vec<String>, String> {
    let targets: Vec<&str> = if target == "all" {
        vec!["claude", "codex", "agy"]
    } else {
        vec![target]
    };
    let mut installed = Vec::new();
    for t in targets {
        for dir in target_dirs(t)? {
            write_skill_bundle(&dir, force)?;
            installed.push(dir.to_string_lossy().into_owned());
        }
    }
    Ok(installed)
}

pub fn uninstall_target(target: &str) -> Result<Vec<String>, String> {
    let targets: Vec<&str> = if target == "all" {
        vec!["claude", "codex", "agy"]
    } else {
        vec![target]
    };
    let mut removed = Vec::new();
    for t in targets {
        for dir in target_dirs(t)? {
            let (exists, managed) = dir_has_skill(&dir);
            if exists && managed {
                fs::remove_dir_all(&dir)
                    .map_err(|e| format!("cannot remove {}: {e}", dir.display()))?;
                removed.push(dir.to_string_lossy().into_owned());
            }
        }
    }
    Ok(removed)
}

pub fn build_creator_prompt(goal: &str, plugin_id: Option<&str>, plugins_dir: &Path) -> String {
    let id_hint = plugin_id
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|id| {
            format!(
                "Use plugin id `{id}` under `{}/{id}`.",
                plugins_dir.display()
            )
        })
        .unwrap_or_else(|| {
            format!(
                "Choose a short lowercase kebab-case plugin id under `{}/<id>`.",
                plugins_dir.display()
            )
        });
    format!(
        "Create a working Masil plugin for this request using the `masil-plugin` skill.\n\n\
         User request:\n{goal}\n\n\
         Instructions:\n\
         1. Run `masil-agent plugin schema --json` to confirm the exact live schema.\n\
         2. {id_hint} Scaffold it with `masil-agent plugin init` (prefer zero-dependency Node ESM or Python 3; use a theme-only manifest with `theme.toml` if only a theme was requested).\n\
         3. Implement all requested contributions (themes, panels, slash commands, timeline cards, hooks, or presets) and write a short `OVERVIEW.md`.\n\
         4. Validate with `masil-agent plugin check <id>` and test each panel/command handler with `masil-agent plugin test <id> ...` until zero errors remain.\n\
         5. Run `masil-agent plugin enable <id> && masil-agent plugin reload <id>` (and `masil-agent plugin theme set <theme-id>` if a theme should be applied now).\n\
         6. Verify `masil-agent plugin ls` shows the plugin active with zero errors, then summarize what was created and how to use it."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installs_and_uninstalls_managed_skills_cleanly() {
        let dir = std::env::temp_dir().join(format!("masil-skill-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let target_dir = dir.join("masil-plugin");

        write_skill_bundle(&target_dir, false).unwrap();
        assert_eq!(dir_has_skill(&target_dir), (true, true));
        assert!(target_dir.join("references/ui-spec.md").is_file());
        assert!(target_dir.join("references/examples.md").is_file());

        // Overwriting a managed install succeeds without force.
        write_skill_bundle(&target_dir, false).unwrap();

        // Non-managed skill is refused without force.
        fs::write(target_dir.join("SKILL.md"), "# custom").unwrap();
        assert_eq!(dir_has_skill(&target_dir), (true, false));
        assert!(write_skill_bundle(&target_dir, false).is_err());

        let _ = fs::remove_dir_all(&dir);
    }
}
