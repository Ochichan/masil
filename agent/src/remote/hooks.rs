//! Opt-in, idempotent, marker-based provider hook installer for Claude Code,
//! Codex, OpenCode, and Antigravity CLI (`agy`). Hooks are inert outside masil.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::fs;
use std::path::{Path, PathBuf};

const MARKER: &str = "masil-remote-hook";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookProviderStatus {
    pub id: String,
    pub label: String,
    pub cli_installed: bool,
    pub hook_installed: bool,
    pub config_path: String,
}

fn home_dir() -> Result<PathBuf, String> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is not set".into())
}

fn xdg_config_home() -> Result<PathBuf, String> {
    if let Some(v) = std::env::var_os("XDG_CONFIG_HOME").filter(|s| !s.is_empty()) {
        return Ok(PathBuf::from(v));
    }
    Ok(home_dir()?.join(".config"))
}

fn cli_exists(bin: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(bin).is_file()))
}

fn claude_settings_path() -> Result<PathBuf, String> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|s| !s.is_empty()) {
        return Ok(PathBuf::from(dir).join("settings.json"));
    }
    Ok(home_dir()?.join(".claude").join("settings.json"))
}

fn codex_hooks_path() -> Result<PathBuf, String> {
    if let Some(dir) = std::env::var_os("CODEX_HOME").filter(|s| !s.is_empty()) {
        return Ok(PathBuf::from(dir).join("hooks.json"));
    }
    Ok(home_dir()?.join(".codex").join("hooks.json"))
}

fn opencode_plugin_path() -> Result<PathBuf, String> {
    if let Some(dir) = std::env::var_os("OPENCODE_CONFIG_DIR").filter(|s| !s.is_empty()) {
        return Ok(PathBuf::from(dir)
            .join("plugins")
            .join("masil-remote-activity.js"));
    }
    Ok(xdg_config_home()?
        .join("opencode")
        .join("plugins")
        .join("masil-remote-activity.js"))
}

fn agy_hooks_path() -> Result<PathBuf, String> {
    Ok(home_dir()?
        .join(".gemini")
        .join("antigravity-cli")
        .join("hooks.json"))
}

fn masil_agent_bin() -> String {
    std::env::current_exe()
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "masil-agent".into())
}

fn hook_command(provider: &str, event: &str) -> String {
    let bin = masil_agent_bin();
    let action = match event {
        "SessionStart" | "PreInvocation" => " session",
        _ => "",
    };
    format!(
        r#"[ -n "$TMUX_PANE" ] && "{bin}" agent integration hook {provider}{action} >/dev/null 2>&1 || true # {MARKER}"#
    )
}

pub fn inspect_all() -> Vec<HookProviderStatus> {
    let mut out = Vec::new();
    if let Ok(p) = claude_settings_path() {
        out.push(HookProviderStatus {
            id: "claude".into(),
            label: "Claude Code".into(),
            cli_installed: cli_exists("claude"),
            hook_installed: json_file_has_marker(&p),
            config_path: p.to_string_lossy().into_owned(),
        });
    }
    if let Ok(p) = codex_hooks_path() {
        out.push(HookProviderStatus {
            id: "codex".into(),
            label: "Codex".into(),
            cli_installed: cli_exists("codex"),
            hook_installed: json_file_has_marker(&p),
            config_path: p.to_string_lossy().into_owned(),
        });
    }
    if let Ok(p) = opencode_plugin_path() {
        out.push(HookProviderStatus {
            id: "opencode".into(),
            label: "OpenCode".into(),
            cli_installed: cli_exists("opencode"),
            hook_installed: p.is_file()
                && fs::read_to_string(&p)
                    .map(|s| s.contains(MARKER))
                    .unwrap_or(false),
            config_path: p.to_string_lossy().into_owned(),
        });
    }
    if let Ok(p) = agy_hooks_path() {
        out.push(HookProviderStatus {
            id: "agy".into(),
            label: "Antigravity CLI (agy)".into(),
            cli_installed: cli_exists("agy"),
            hook_installed: json_file_has_marker(&p),
            config_path: p.to_string_lossy().into_owned(),
        });
    }
    out
}

pub fn install_provider(provider: &str) -> Result<String, String> {
    match provider {
        "claude" => {
            let path = claude_settings_path()?;
            install_json_hooks(
                &path,
                "claude",
                &[
                    "SessionStart",
                    "UserPromptSubmit",
                    "PermissionRequest",
                    "Notification",
                    "Stop",
                    "StopFailure",
                ],
            )?;
            Ok(format!("Installed Claude Code hooks in {}", path.display()))
        }
        "codex" => {
            let path = codex_hooks_path()?;
            install_json_hooks(
                &path,
                "codex",
                &[
                    "SessionStart",
                    "UserPromptSubmit",
                    "PermissionRequest",
                    "Stop",
                ],
            )?;
            Ok(format!("Installed Codex hooks in {}", path.display()))
        }
        "agy" | "antigravity" => {
            let path = agy_hooks_path()?;
            install_agy_hooks(&path)?;
            Ok(format!(
                "Installed Antigravity CLI hooks in {}",
                path.display()
            ))
        }
        "opencode" => {
            let path = opencode_plugin_path()?;
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            let bin = masil_agent_bin();
            let plugin = format!(
                "// {MARKER}: opt-in masil remote activity bridge (inert outside masil)\n\
                 import {{ spawn }} from 'node:child_process';\n\
                 function report(event, payload) {{\n\
                   if (!process.env.TMUX_PANE) return;\n\
                   try {{\n\
                     const p = spawn({bin_json}, ['agent', 'integration', 'hook', 'opencode'], {{ stdio: ['pipe', 'ignore', 'ignore'] }});\n\
                     p.stdin.end(JSON.stringify({{ event, ...(payload || {{}}) }}));\n\
                   }} catch {{}}\n\
                 }}\n\
                 export default async function masilRemotePlugin() {{\n\
                   return {{\n\
                     event: async ({{ event }}) => {{\n\
                       if (!event || !event.type) return;\n\
                       report(event.type, event);\n\
                     }},\n\
                   }};\n\
                 }}\n",
                bin_json = serde_json::to_string(&bin).unwrap_or_else(|_| "\"masil-agent\"".into())
            );
            fs::write(&path, plugin)
                .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
            Ok(format!("Installed OpenCode plugin at {}", path.display()))
        }
        other => Err(format!("unsupported hook provider: {other}")),
    }
}

pub fn uninstall_provider(provider: &str) -> Result<String, String> {
    match provider {
        "claude" => {
            let path = claude_settings_path()?;
            remove_json_hooks(&path)?;
            Ok(format!("Removed masil hooks from {}", path.display()))
        }
        "codex" => {
            let path = codex_hooks_path()?;
            remove_json_hooks(&path)?;
            Ok(format!("Removed masil hooks from {}", path.display()))
        }
        "agy" | "antigravity" => {
            let path = agy_hooks_path()?;
            remove_json_hooks(&path)?;
            Ok(format!("Removed masil hooks from {}", path.display()))
        }
        "opencode" => {
            let path = opencode_plugin_path()?;
            if path.is_file()
                && fs::read_to_string(&path)
                    .map(|s| s.contains(MARKER))
                    .unwrap_or(false)
            {
                fs::remove_file(&path).map_err(|e| e.to_string())?;
            }
            Ok(format!("Removed OpenCode plugin {}", path.display()))
        }
        other => Err(format!("unsupported hook provider: {other}")),
    }
}

fn json_file_has_marker(path: &Path) -> bool {
    fs::read_to_string(path)
        .map(|s| s.contains(MARKER))
        .unwrap_or(false)
}

fn read_json_object(path: &Path) -> Result<Map<String, Value>, String> {
    if !path.exists() {
        return Ok(Map::new());
    }
    let body =
        fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    if body.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str::<Value>(&body) {
        Ok(Value::Object(map)) => Ok(map),
        _ => Err(format!("{} is not a JSON object", path.display())),
    }
}

fn install_json_hooks(path: &Path, provider: &str, events: &[&str]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut root = read_json_object(path)?;
    let hooks_val = root
        .entry("hooks".to_owned())
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(hooks_obj) = hooks_val.as_object_mut() else {
        return Err("existing `hooks` field is not a JSON object".into());
    };

    for event in events {
        let entry = hooks_obj
            .entry((*event).to_owned())
            .or_insert_with(|| Value::Array(Vec::new()));
        let Some(arr) = entry.as_array_mut() else {
            continue;
        };
        arr.retain(|item| !item.to_string().contains(MARKER));
        arr.push(json!({
            "hooks": [{
                "type": "command",
                "command": hook_command(provider, event),
            }]
        }));
    }

    let formatted =
        serde_json::to_string_pretty(&Value::Object(root)).map_err(|e| e.to_string())?;
    fs::write(path, format!("{formatted}\n"))
        .map_err(|e| format!("cannot write {}: {e}", path.display()))
}

fn install_agy_hooks(path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut root = read_json_object(path)?;
    let hooks_val = root
        .entry("hooks".to_owned())
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(hooks_obj) = hooks_val.as_object_mut() else {
        return Err("existing `hooks` field is not a JSON object".into());
    };
    // Remove any legacy flat entries written by earlier versions of masil-remote-hook.
    let mut empty_keys = Vec::new();
    for (k, v) in hooks_obj.iter_mut() {
        if let Some(arr) = v.as_array_mut() {
            arr.retain(|item| !item.to_string().contains(MARKER));
            if arr.is_empty() {
                empty_keys.push(k.clone());
            }
        }
    }
    for k in empty_keys {
        hooks_obj.remove(&k);
    }
    // Antigravity CLI expects named hook groups under `hooks.<group>.PreInvocation`.
    hooks_obj.insert(
        "masil".into(),
        json!({
            "PreInvocation": [{
                "type": "command",
                "command": hook_command("agy", "PreInvocation"),
                "timeout": 10,
            }]
        }),
    );
    let formatted =
        serde_json::to_string_pretty(&Value::Object(root)).map_err(|e| e.to_string())?;
    fs::write(path, format!("{formatted}\n"))
        .map_err(|e| format!("cannot write {}: {e}", path.display()))
}

fn remove_json_hooks(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    let mut root = read_json_object(path)?;
    if let Some(Value::Object(hooks_obj)) = root.get_mut("hooks") {
        let mut empty_keys = Vec::new();
        for (k, v) in hooks_obj.iter_mut() {
            if k == "masil" && v.to_string().contains(MARKER) {
                empty_keys.push(k.clone());
                continue;
            }
            if let Some(arr) = v.as_array_mut() {
                arr.retain(|item| !item.to_string().contains(MARKER));
                if arr.is_empty() {
                    empty_keys.push(k.clone());
                }
            }
        }
        for k in empty_keys {
            hooks_obj.remove(&k);
        }
    }
    let formatted =
        serde_json::to_string_pretty(&Value::Object(root)).map_err(|e| e.to_string())?;
    fs::write(path, format!("{formatted}\n"))
        .map_err(|e| format!("cannot write {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_hooks_install_and_remove_preserving_user_hooks() {
        let dir = std::env::temp_dir().join(format!("masil-hooks-test-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let p = dir.join("settings.json");
        fs::write(
            &p,
            r#"{"theme":"dark","hooks":{"Stop":[{"hooks":[{"type":"command","command":"echo user"}]}]}}"#,
        )
        .unwrap();

        install_json_hooks(&p, "claude", &["Stop", "SessionStart"]).unwrap();
        assert!(json_file_has_marker(&p));
        let mid = fs::read_to_string(&p).unwrap();
        assert!(mid.contains("echo user"));
        assert!(mid.contains(MARKER));

        remove_json_hooks(&p).unwrap();
        assert!(!json_file_has_marker(&p));
        let after = fs::read_to_string(&p).unwrap();
        assert!(after.contains("echo user"));
        assert!(after.contains("\"theme\": \"dark\""));

        let _ = fs::remove_dir_all(&dir);
    }
}
