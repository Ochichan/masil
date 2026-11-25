//! Zero-dependency plugin scaffolds (`masil-agent plugin init`).

use super::manifest::{plugins_dir, valid_id};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Template {
    Theme,
    Command,
    Panel,
    Hook,
    Preset,
    Full,
}

impl Template {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "theme" => Ok(Self::Theme),
            "command" | "cmd" => Ok(Self::Command),
            "panel" => Ok(Self::Panel),
            "hook" => Ok(Self::Hook),
            "preset" => Ok(Self::Preset),
            "full" | "all" => Ok(Self::Full),
            _ => Err(format!(
                "unknown template '{s}' (expected theme, command, panel, hook, preset, or full)"
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Node,
    Python,
    Bash,
}

impl Lang {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "node" | "js" | "mjs" => Ok(Self::Node),
            "python" | "py" | "python3" => Ok(Self::Python),
            "bash" | "sh" => Ok(Self::Bash),
            _ => Err(format!(
                "unknown lang '{s}' (expected node, python, or bash)"
            )),
        }
    }
}

fn title_case(id: &str) -> String {
    id.split(['-', '_'])
        .filter(|p| !p.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn init_plugin(
    id: &str,
    dir_override: Option<&Path>,
    template: Template,
    lang: Lang,
) -> Result<PathBuf, String> {
    if !valid_id(id) {
        return Err(
            "plugin id must be 1-48 lowercase letters, digits, '-' or '_' starting with a letter"
                .into(),
        );
    }
    let dir = match dir_override {
        Some(p) => p.to_path_buf(),
        None => plugins_dir()?.join(id),
    };
    if dir.join("masil-plugin.toml").exists() {
        return Err(format!(
            "plugin already exists at {}",
            dir.join("masil-plugin.toml").display()
        ));
    }
    fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;

    let display_name = title_case(id);
    let needs_handler = !matches!(template, Template::Theme | Template::Preset);

    let (entry_toml, script_name, script_body) = match lang {
        Lang::Node => (
            r#"entry = ["node", "plugin.mjs"]"#,
            "plugin.mjs",
            node_handler(&display_name, id),
        ),
        Lang::Python => (
            r#"entry = ["python3", "plugin.py"]"#,
            "plugin.py",
            python_handler(&display_name, id),
        ),
        Lang::Bash => (
            r#"entry = ["./plugin.sh"]"#,
            "plugin.sh",
            bash_handler(&display_name, id),
        ),
    };

    let mut manifest = format!(
        r#"schema_version = 1
id = "{id}"
name = "{display_name}"
version = "0.1.0"
description = "Custom Masil plugin ({display_name})."
author = "local"
scope = "read"
"#
    );

    if needs_handler {
        manifest.push_str(entry_toml);
        manifest.push('\n');
    }

    if matches!(template, Template::Theme | Template::Full) {
        manifest.push_str(&format!(
            r#"
[[themes]]
id = "{id}"
name = "{display_name}"
appearance = "dark"
file = "theme.toml"
"#
        ));
        fs::write(
            dir.join("theme.toml"),
            r##"# Omarchy-style palette for Masil TUI and Mobile
canvas = "#131722"
panel = "#1b2130"
raised = "#262f44"
text = "#e4e9f5"
muted = "#96a3be"
accent = "#6cc4d4"
accent_text = "#09161b"
warning = "#f0be6a"
danger = "#ea757c"
success = "#79c88e"
track = "#344058"
"##,
        )
        .map_err(|e| e.to_string())?;
    }

    if matches!(template, Template::Panel | Template::Full) {
        manifest.push_str(&format!(
            r#"
[[panels]]
id = "main"
title = "{display_name}"
icon = "Sparkles"
context = "agent"
handler = "panel.main"
refresh_secs = 30
"#
        ));
    }

    if matches!(template, Template::Command | Template::Full) {
        manifest.push_str(&format!(
            r#"
[[commands]]
id = "run"
slash = "{id}"
title = "{display_name}"
description = "Run {display_name} command"
argument_hint = "[note]"
context = "agent"
handler = "command.run"
icon = "Terminal"
"#
        ));
    }

    if matches!(template, Template::Hook | Template::Full) {
        manifest.push_str(
            r#"
[[hooks]]
event = "agent.state_changed"
handler = "hook.state"
timeout_secs = 5
"#,
        );
    }

    if matches!(template, Template::Preset | Template::Full) {
        manifest.push_str(&format!(
            r#"
[[presets]]
id = "{id}-reviewer"
name = "{display_name} Reviewer"
description = "Focused code review session"
provider = "claude"
effort = "high"
mode = "plan"
worktree = false
initial_prompt = "Review the current uncommitted changes and list concrete regressions."
"#
        ));
    }

    fs::write(dir.join("masil-plugin.toml"), manifest).map_err(|e| e.to_string())?;
    fs::write(
        dir.join("OVERVIEW.md"),
        format!(
            "{display_name} extends Masil across TUI and Mobile.\n\nEdit `masil-plugin.toml` and its handler, then run `masil-agent plugin check {id}` and `masil-agent plugin reload {id}`.\n"
        ),
    )
    .map_err(|e| e.to_string())?;

    if needs_handler {
        let script_path = dir.join(script_name);
        fs::write(&script_path, script_body).map_err(|e| e.to_string())?;
        let _ = fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755));
    }

    Ok(dir)
}

fn node_handler(display_name: &str, id: &str) -> String {
    format!(
        r#"import fs from "node:fs";

const raw = fs.readFileSync(0, "utf8").trim();
const req = raw ? JSON.parse(raw) : {{}};
const storage = req.storage && typeof req.storage === "object" ? req.storage : {{}};
const runs = Number(storage.runs || 0) + 1;
const cwd = req.agent?.cwd || process.cwd();
const agentName = req.agent?.name || "workspace";

if (req.kind === "command") {{
  const note = (req.args || "").trim() || "Ready";
  console.log(JSON.stringify({{
    toast: `{display_name}: ${{note}}`,
    storage_patch: {{ runs, last_note: note }},
    ui: {{
      v: 1,
      title: "{display_name}",
      subtitle: agentName,
      blocks: [
        {{ type: "callout", title: "Command output", text: note, tone: "accent" }},
        {{
          type: "key_value",
          rows: [
            {{ key: "Plugin", value: "{id}", mono: true }},
            {{ key: "Directory", value: cwd, mono: true, copyable: true }},
            {{ key: "Invocations", value: String(runs) }},
          ],
        }},
      ],
    }},
  }}));
  process.exit(0);
}}

if (req.kind === "hook") {{
  console.log(JSON.stringify({{ ok: true, storage_patch: {{ last_event: req.event || "unknown" }} }}));
  process.exit(0);
}}

const note = req.action === "save_note" && req.form_values?.note
  ? String(req.form_values.note).trim()
  : String(storage.last_note || "No note saved yet.");

console.log(JSON.stringify({{
  v: 1,
  title: "{display_name}",
  subtitle: agentName,
  storage_patch: {{ runs, last_note: note }},
  blocks: [
    {{
      type: "header",
      title: "{display_name}",
      subtitle: cwd,
      badge: `run #${{runs}}`,
      tone: "accent",
    }},
    {{
      type: "metric_row",
      items: [
        {{ label: "Runs", value: String(runs), tone: "accent" }},
        {{ label: "Agent", value: agentName, tone: "success" }},
      ],
    }},
    {{ type: "callout", title: "Saved note", text: note, tone: "normal" }},
    {{
      type: "button_row",
      buttons: [
        {{ id: "refresh", label: "Refresh", style: "primary", action: "refresh" }},
      ],
    }},
    {{
      type: "form",
      id: "note-form",
      submit_label: "Save note",
      action: "save_note",
      fields: [
        {{ name: "note", label: "Workspace note", placeholder: "Write a short note..." }},
      ],
    }},
  ],
}}));
"#
    )
}

fn python_handler(display_name: &str, id: &str) -> String {
    format!(
        r#"#!/usr/bin/env python3
import json
import os
import sys

raw = sys.stdin.read().strip()
req = json.loads(raw) if raw else {{}}
storage = req.get("storage") if isinstance(req.get("storage"), dict) else {{}}
runs = int(storage.get("runs", 0)) + 1
agent = req.get("agent") if isinstance(req.get("agent"), dict) else {{}}
cwd = agent.get("cwd") or os.getcwd()
agent_name = agent.get("name") or "workspace"

if req.get("kind") == "command":
    note = (req.get("args") or "").strip() or "Ready"
    print(json.dumps({{
        "toast": "{display_name}: " + note,
        "storage_patch": {{"runs": runs, "last_note": note}},
        "ui": {{
            "v": 1,
            "title": "{display_name}",
            "subtitle": agent_name,
            "blocks": [
                {{"type": "callout", "title": "Command output", "text": note, "tone": "accent"}},
                {{"type": "key_value", "rows": [
                    {{"key": "Plugin", "value": "{id}", "mono": True}},
                    {{"key": "Directory", "value": cwd, "mono": True, "copyable": True}},
                    {{"key": "Invocations", "value": str(runs)}},
                ]}},
            ],
        }},
    }}))
    sys.exit(0)

if req.get("kind") == "hook":
    print(json.dumps({{"ok": True, "storage_patch": {{"last_event": req.get("event") or "unknown"}}}}))
    sys.exit(0)

form_values = req.get("form_values") if isinstance(req.get("form_values"), dict) else {{}}
note = str(form_values.get("note") or storage.get("last_note") or "No note saved yet.").strip()

print(json.dumps({{
    "v": 1,
    "title": "{display_name}",
    "subtitle": agent_name,
    "storage_patch": {{"runs": runs, "last_note": note}},
    "blocks": [
        {{"type": "header", "title": "{display_name}", "subtitle": cwd, "badge": f"run #{{runs}}", "tone": "accent"}},
        {{"type": "metric_row", "items": [
            {{"label": "Runs", "value": str(runs), "tone": "accent"}},
            {{"label": "Agent", "value": agent_name, "tone": "success"}},
        ]}},
        {{"type": "callout", "title": "Saved note", "text": note, "tone": "normal"}},
        {{"type": "button_row", "buttons": [
            {{"id": "refresh", "label": "Refresh", "style": "primary", "action": "refresh"}},
        ]}},
    ],
}}))
"#
    )
}

fn bash_handler(display_name: &str, id: &str) -> String {
    format!(
        r#"#!/usr/bin/env bash
set -euo pipefail
cat >/dev/null
cat <<'JSON'
{{"v":1,"title":"{display_name}","blocks":[{{"type":"header","title":"{display_name}","badge":"{id}","tone":"accent"}},{{"type":"callout","text":"Bash plugin handler executed successfully.","tone":"success"}}]}}
JSON
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::manifest::load_plugin_dir;
    use std::collections::HashSet;

    #[test]
    fn all_templates_and_langs_scaffold_clean_manifests() {
        let base = std::env::temp_dir().join(format!("masil-scaf-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        for (template, lang, name) in [
            (Template::Theme, Lang::Node, "t-theme"),
            (Template::Panel, Lang::Node, "t-panel-node"),
            (Template::Command, Lang::Python, "t-cmd-py"),
            (Template::Hook, Lang::Bash, "t-hook-bash"),
            (Template::Preset, Lang::Node, "t-preset"),
            (Template::Full, Lang::Node, "t-full"),
        ] {
            let dir = base.join(name);
            init_plugin(name, Some(&dir), template, lang).unwrap();
            let loaded = load_plugin_dir(&dir, None, "test", &HashSet::new());
            assert!(
                loaded.errors.is_empty(),
                "{name} errors: {:?}",
                loaded.errors
            );
        }
        let _ = fs::remove_dir_all(&base);
    }
}
