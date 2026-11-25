//! First-class local plugin platform (`masil-agent plugin ...`), bridging
//! Omarchy-style directory/theme conventions, Paseo-style CLI/skill workflow,
//! and DeepSeek Harness-style Creator Mode across TUI and Mobile.

pub mod manifest;
pub mod runner;
pub mod scaffold;
pub mod skill;
pub mod ui_spec;

use manifest::{LoadedPlugin, PluginsConfig, load_catalog, load_plugin_dir, plugins_dir, valid_id};
use serde_json::{Map, Value, json};
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

fn usage() -> String {
    "usage: masil-agent plugin <command> [args]\n\n\
     Commands:\n  \
       init <id> [--dir PATH] [--template theme|command|panel|hook|preset|full] [--lang node|python|bash]\n  \
       install <path> [--id ID] [--no-build]\n  \
       ls [--json]\n  \
       check <id-or-path> [--json]\n  \
       test <id> <panel|command|hook> <target> [--args TEXT] [--action ID] [--cwd DIR] [--json]\n  \
       enable <id> | disable <id> | reload [id] | remove <id>\n  \
       logs <id> [--tail N]\n  \
       theme list | set <theme-id> | reset [--json]\n  \
       skill status | install [--target claude|codex|agy|all] [--force] | uninstall [--target ...]\n  \
       schema [--json]\n  \
       create-prompt <goal> [--id ID]"
        .into()
}

pub fn catalog_json() -> Result<Value, String> {
    let catalog = load_catalog()?;
    let skills = skill::inspect_all();
    let mut themes = Vec::new();
    let mut panels = Vec::new();
    let mut commands = Vec::new();
    let mut presets = Vec::new();
    let mut plugins = Vec::new();

    for p in &catalog.plugins {
        let active = p.is_active(catalog.enabled);
        let manifest_summary = p.manifest.as_ref().map(|m| {
            json!({
                "name": m.name,
                "version": m.version,
                "description": m.description,
                "author": m.author,
                "scope": m.scope,
                "has_entry": !m.entry.is_empty(),
                "theme_count": p.themes.len(),
                "panel_count": m.panels.len(),
                "command_count": m.commands.len(),
                "hook_count": m.hooks.len(),
                "preset_count": m.presets.len(),
            })
        });
        plugins.push(json!({
            "id": p.id,
            "dir": p.dir.to_string_lossy(),
            "source": p.source,
            "enabled": p.enabled,
            "active": active,
            "status": if !catalog.enabled || !p.enabled {
                "disabled"
            } else if !p.errors.is_empty() {
                "error"
            } else {
                "running"
            },
            "overview": p.overview,
            "errors": p.errors,
            "warnings": p.warnings,
            "manifest": manifest_summary,
        }));

        if !active {
            continue;
        }
        themes.extend(p.themes.iter().cloned());
        if let Some(m) = &p.manifest {
            for panel in &m.panels {
                panels.push(json!({
                    "plugin_id": p.id,
                    "id": panel.id,
                    "title": panel.title,
                    "icon": panel.icon,
                    "context": panel.context,
                    "refresh_secs": panel.refresh_secs,
                }));
            }
            for cmd in &m.commands {
                commands.push(json!({
                    "plugin_id": p.id,
                    "id": cmd.id,
                    "slash": cmd.slash.trim_start_matches('/'),
                    "title": cmd.title,
                    "description": cmd.description,
                    "argument_hint": cmd.argument_hint,
                    "context": cmd.context,
                    "icon": cmd.icon,
                }));
            }
            for preset in &m.presets {
                presets.push(json!({
                    "plugin_id": p.id,
                    "id": preset.id,
                    "name": preset.name,
                    "description": preset.description,
                    "provider": preset.provider,
                    "model": preset.model,
                    "effort": preset.effort,
                    "mode": preset.mode,
                    "worktree": preset.worktree,
                    "initial_prompt": preset.initial_prompt,
                }));
            }
        }
    }

    Ok(json!({
        "enabled": catalog.enabled,
        "active_theme": catalog.active_theme,
        "reload_nonce": catalog.reload_nonce,
        "plugins": plugins,
        "themes": themes,
        "panels": panels,
        "commands": commands,
        "presets": presets,
        "skills": skills,
    }))
}

pub fn set_active_theme(theme_id: Option<&str>) -> Result<Value, String> {
    let mut cfg = PluginsConfig::load()?;
    let next = match theme_id.map(str::trim).filter(|s| !s.is_empty()) {
        None | Some("reset" | "default") => None,
        Some(builtin @ ("dark" | "light" | "system" | "terminal")) => Some(builtin.to_owned()),
        Some(id) => {
            let catalog = load_catalog()?;
            if !catalog.all_themes().iter().any(|t| t.id == id) {
                return Err(format!(
                    "unknown theme '{id}'; run `masil-agent plugin theme list` to see available themes"
                ));
            }
            Some(id.to_owned())
        }
    };
    cfg.active_theme = next.clone();
    cfg.bump_reload();
    cfg.save()?;

    // Fire `theme.changed` hooks asynchronously in a short-lived runtime if called outside tokio.
    let theme_val = next.clone().unwrap_or_else(|| "dark".into());
    if tokio::runtime::Handle::try_current().is_ok() {
        tokio::spawn(async move {
            runner::dispatch_hook_event(
                "theme.changed",
                json!({ "theme": theme_val }),
                None,
                crate::api::current_socket(),
            )
            .await;
        });
    } else if let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        rt.block_on(runner::dispatch_hook_event(
            "theme.changed",
            json!({ "theme": theme_val }),
            None,
            crate::api::current_socket(),
        ));
    }

    Ok(json!({ "active_theme": next, "reload_nonce": cfg.reload_nonce }))
}

pub fn run(args: &[String]) -> Result<i32, String> {
    if args.is_empty() || matches!(args[0].as_str(), "--help" | "-h" | "help") {
        println!("{}", usage());
        return Ok(0);
    }
    let json_flag = args.iter().any(|a| a == "--json");
    match args[0].as_str() {
        "schema" => {
            let doc = ui_spec::schema_document();
            println!(
                "{}",
                serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?
            );
            Ok(0)
        }
        "init" => cmd_init(&args[1..], json_flag),
        "install" | "add" => cmd_install(&args[1..], json_flag),
        "ls" | "list" => cmd_ls(json_flag),
        "check" => cmd_check(&args[1..], json_flag),
        "test" => cmd_test(&args[1..]),
        "enable" => cmd_toggle(&args[1..], true, json_flag),
        "disable" => cmd_toggle(&args[1..], false, json_flag),
        "reload" => cmd_reload(&args[1..], json_flag),
        "remove" | "rm" => cmd_remove(&args[1..], json_flag),
        "logs" => cmd_logs(&args[1..], json_flag),
        "theme" => cmd_theme(&args[1..], json_flag),
        "skill" => cmd_skill(&args[1..], json_flag),
        "create-prompt" => cmd_create_prompt(&args[1..]),
        _ => Err(usage()),
    }
}

fn cmd_init(args: &[String], as_json: bool) -> Result<i32, String> {
    let mut id: Option<String> = None;
    let mut dir: Option<PathBuf> = None;
    let mut template = scaffold::Template::Panel;
    let mut lang = scaffold::Lang::Node;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--json" => i += 1,
            "--dir" => {
                dir = Some(PathBuf::from(
                    args.get(i + 1).ok_or("--dir requires a path")?,
                ));
                i += 2;
            }
            "--template" | "-t" => {
                template = scaffold::Template::parse(
                    args.get(i + 1).ok_or("--template requires a value")?,
                )?;
                i += 2;
            }
            "--lang" | "-l" => {
                lang = scaffold::Lang::parse(args.get(i + 1).ok_or("--lang requires a value")?)?;
                i += 2;
            }
            val if !val.starts_with('-') && id.is_none() => {
                id = Some(val.to_owned());
                i += 1;
            }
            other => return Err(format!("unknown option '{other}'\n{}", usage())),
        }
    }
    let id = id.ok_or("usage: masil-agent plugin init <id> [--template ...] [--lang ...]")?;
    let created = scaffold::init_plugin(&id, dir.as_deref(), template, lang)?;
    if let Some(custom_dir) = dir {
        let abs = custom_dir.canonicalize().unwrap_or(custom_dir);
        let mut cfg = PluginsConfig::load()?;
        cfg.linked
            .insert(id.clone(), abs.to_string_lossy().into_owned());
        cfg.bump_reload();
        cfg.save()?;
    } else {
        let mut cfg = PluginsConfig::load()?;
        cfg.bump_reload();
        cfg.save()?;
    }
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "id": id,
                "dir": created.to_string_lossy(),
            }))
            .map_err(|e| e.to_string())?
        );
    } else {
        println!("Created plugin '{id}' at {}", created.display());
        println!("Next: masil-agent plugin check {id} && masil-agent plugin reload {id}");
    }
    Ok(0)
}

fn cmd_install(args: &[String], as_json: bool) -> Result<i32, String> {
    let mut path_arg: Option<PathBuf> = None;
    let mut override_id: Option<String> = None;
    let mut no_build = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--json" => i += 1,
            "--no-build" => {
                no_build = true;
                i += 1;
            }
            "--id" => {
                override_id = Some(args.get(i + 1).ok_or("--id requires a value")?.clone());
                i += 2;
            }
            val if !val.starts_with('-') && path_arg.is_none() => {
                path_arg = Some(PathBuf::from(val));
                i += 1;
            }
            other => return Err(format!("unknown option '{other}'")),
        }
    }
    let raw_path = path_arg.ok_or("usage: masil-agent plugin install <path> [--id ID]")?;
    let abs = raw_path
        .canonicalize()
        .map_err(|e| format!("cannot resolve {}: {e}", raw_path.display()))?;
    let loaded = load_plugin_dir(&abs, override_id.as_deref(), "local", &HashSet::new());
    if !loaded.errors.is_empty() {
        return Err(format!(
            "cannot install plugin at {}: {}",
            abs.display(),
            loaded.errors.join("; ")
        ));
    }
    if !valid_id(&loaded.id) {
        return Err(format!("invalid plugin id '{}'", loaded.id));
    }
    if !no_build
        && let Some(manifest) = &loaded.manifest
        && !manifest.build.is_empty()
    {
        runner::run_build_steps(&abs, &manifest.build)?;
    }
    let mut cfg = PluginsConfig::load()?;
    cfg.linked
        .insert(loaded.id.clone(), abs.to_string_lossy().into_owned());
    cfg.set_plugin_enabled(&loaded.id, true);
    cfg.save()?;

    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "id": loaded.id,
                "dir": abs.to_string_lossy(),
                "warnings": loaded.warnings,
            }))
            .map_err(|e| e.to_string())?
        );
    } else {
        println!("Installed plugin '{}' from {}", loaded.id, abs.display());
        for w in &loaded.warnings {
            eprintln!("warning: {w}");
        }
    }
    Ok(0)
}

fn cmd_ls(as_json: bool) -> Result<i32, String> {
    let val = catalog_json()?;
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&val).map_err(|e| e.to_string())?
        );
        return Ok(0);
    }
    let enabled = val["enabled"].as_bool().unwrap_or(true);
    let theme = val["active_theme"].as_str().unwrap_or("(default)");
    println!("Plugins enabled: {enabled} | Active theme: {theme}");
    let plugins = val["plugins"].as_array().cloned().unwrap_or_default();
    if plugins.is_empty() {
        println!("No plugins installed. Create one with `masil-agent plugin init <id>`.");
        return Ok(0);
    }
    for p in plugins {
        let id = p["id"].as_str().unwrap_or("?");
        let status = p["status"].as_str().unwrap_or("?");
        let src = p["source"].as_str().unwrap_or("?");
        let name = p["manifest"]["name"].as_str().unwrap_or(id);
        let tc = p["manifest"]["theme_count"].as_u64().unwrap_or(0);
        let pc = p["manifest"]["panel_count"].as_u64().unwrap_or(0);
        let cc = p["manifest"]["command_count"].as_u64().unwrap_or(0);
        let hc = p["manifest"]["hook_count"].as_u64().unwrap_or(0);
        let pr = p["manifest"]["preset_count"].as_u64().unwrap_or(0);
        println!(
            "  {id:<22} {status:<10} {name} [themes:{tc} panels:{pc} cmds:{cc} hooks:{hc} presets:{pr}] ({src})"
        );
        if let Some(errs) = p["errors"].as_array() {
            for e in errs {
                if let Some(s) = e.as_str() {
                    println!("    error: {s}");
                }
            }
        }
    }
    Ok(0)
}

fn resolve_target_plugin(id_or_path: &str) -> Result<LoadedPlugin, String> {
    let as_path = PathBuf::from(id_or_path);
    if as_path.is_dir() && as_path.join("masil-plugin.toml").is_file() {
        let abs = as_path.canonicalize().unwrap_or(as_path);
        return Ok(load_plugin_dir(&abs, None, "path", &HashSet::new()));
    }
    let catalog = load_catalog()?;
    catalog
        .find_plugin(id_or_path)
        .cloned()
        .ok_or_else(|| format!("plugin '{id_or_path}' not found"))
}

fn cmd_check(args: &[String], as_json: bool) -> Result<i32, String> {
    let target = args
        .iter()
        .find(|a| !a.starts_with('-'))
        .ok_or("usage: masil-agent plugin check <id-or-path> [--json]")?;
    let plugin = resolve_target_plugin(target)?;
    let mut errors = plugin.errors.clone();
    let warnings = plugin.warnings.clone();
    let mut checked_handlers = Vec::new();

    if errors.is_empty()
        && let Some(manifest) = &plugin.manifest
        && !manifest.entry.is_empty()
    {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let sample_agent = json!({
            "name": "check",
            "provider": "claude",
            "state": "idle",
            "cwd": cwd,
            "pane": "%0",
            "server_socket": ""
        });
        for panel in &manifest.panels {
            match rt.block_on(runner::invoke_panel(
                &plugin,
                &panel.id,
                Some(sample_agent.clone()),
                None,
                None,
                Map::new(),
                None,
            )) {
                Ok(doc) => checked_handlers.push(json!({
                    "kind": "panel",
                    "id": panel.id,
                    "ok": true,
                    "blocks": doc.blocks.len(),
                })),
                Err(e) => errors.push(e),
            }
        }
        for cmd in &manifest.commands {
            match rt.block_on(runner::invoke_command(
                &plugin,
                &cmd.id,
                String::new(),
                Some(sample_agent.clone()),
                None,
            )) {
                Ok(_) => checked_handlers.push(json!({
                    "kind": "command",
                    "id": cmd.id,
                    "ok": true,
                })),
                Err(e) => errors.push(e),
            }
        }
    }

    let ok = errors.is_empty();
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "id": plugin.id,
                "dir": plugin.dir.to_string_lossy(),
                "ok": ok,
                "errors": errors,
                "warnings": warnings,
                "checked": checked_handlers,
            }))
            .map_err(|e| e.to_string())?
        );
    } else if ok {
        println!(
            "OK: plugin '{}' passed validation ({} themes, {} handlers checked)",
            plugin.id,
            plugin.themes.len(),
            checked_handlers.len()
        );
        for w in &warnings {
            eprintln!("warning: {w}");
        }
    } else {
        for e in &errors {
            eprintln!("error: {e}");
        }
        for w in &warnings {
            eprintln!("warning: {w}");
        }
    }
    Ok(if ok { 0 } else { 2 })
}

fn cmd_test(args: &[String]) -> Result<i32, String> {
    let mut pos = Vec::new();
    let mut cmd_args = String::new();
    let mut action: Option<String> = None;
    let mut cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--json" => i += 1,
            "--args" => {
                cmd_args = args.get(i + 1).ok_or("--args requires text")?.clone();
                i += 2;
            }
            "--action" => {
                action = Some(args.get(i + 1).ok_or("--action requires id")?.clone());
                i += 2;
            }
            "--cwd" => {
                cwd = args.get(i + 1).ok_or("--cwd requires path")?.clone();
                i += 2;
            }
            val if !val.starts_with('-') => {
                pos.push(val);
                i += 1;
            }
            other => return Err(format!("unknown option '{other}'")),
        }
    }
    if pos.len() < 3 {
        return Err("usage: masil-agent plugin test <id> <panel|command|hook> <target> [--args TEXT] [--action ID] [--cwd DIR]".into());
    }
    let plugin = resolve_target_plugin(pos[0])?;
    if !plugin.errors.is_empty() {
        return Err(plugin.errors.join("; "));
    }
    let agent = json!({
        "name": "test-agent",
        "provider": "claude",
        "state": "idle",
        "cwd": cwd,
        "pane": "%0",
        "server_socket": crate::api::current_socket().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default(),
    });
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let socket = crate::api::current_socket();
    let result = match pos[1] {
        "panel" => {
            let doc = rt.block_on(runner::invoke_panel(
                &plugin,
                pos[2],
                Some(agent),
                action,
                None,
                Map::new(),
                socket,
            ))?;
            serde_json::to_value(doc).map_err(|e| e.to_string())?
        }
        "command" => {
            let out = rt.block_on(runner::invoke_command(
                &plugin,
                pos[2],
                cmd_args,
                Some(agent),
                socket,
            ))?;
            serde_json::to_value(out).map_err(|e| e.to_string())?
        }
        "hook" => {
            let manifest = plugin.manifest.as_ref().ok_or("no manifest")?;
            let hook = manifest
                .hooks
                .iter()
                .find(|h| h.event == pos[2] || h.handler == pos[2])
                .ok_or_else(|| format!("no hook matching '{}'", pos[2]))?;
            let req = runner::PluginRequest {
                v: 1,
                kind: "hook".into(),
                handler: hook.handler.clone(),
                plugin_id: plugin.id.clone(),
                action: None,
                payload: None,
                form_values: Map::new(),
                args: cmd_args,
                event: Some(hook.event.clone()),
                event_data: Some(json!({ "state": "idle", "test": true })),
                agent: Some(agent),
                storage: runner::load_storage(&plugin.id),
            };
            rt.block_on(runner::invoke_raw(&plugin, &req, socket, hook.timeout_secs))?
        }
        other => {
            return Err(format!(
                "unknown target kind '{other}' (expected panel, command, or hook)"
            ));
        }
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&result).map_err(|e| e.to_string())?
    );
    Ok(0)
}

fn cmd_toggle(args: &[String], enabled: bool, as_json: bool) -> Result<i32, String> {
    let id = args
        .iter()
        .find(|a| !a.starts_with('-'))
        .ok_or("usage: masil-agent plugin enable|disable <id>")?;
    let catalog = load_catalog()?;
    if catalog.find_plugin(id).is_none() {
        return Err(format!("plugin '{id}' not found"));
    }
    let mut cfg = PluginsConfig::load()?;
    cfg.set_plugin_enabled(id, enabled);
    cfg.save()?;
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "id": id,
                "enabled": enabled,
                "reload_nonce": cfg.reload_nonce,
            }))
            .map_err(|e| e.to_string())?
        );
    } else {
        println!(
            "{} plugin '{id}'",
            if enabled { "Enabled" } else { "Disabled" }
        );
    }
    Ok(0)
}

fn cmd_reload(args: &[String], as_json: bool) -> Result<i32, String> {
    let target = args.iter().find(|a| !a.starts_with('-'));
    let mut cfg = PluginsConfig::load()?;
    cfg.bump_reload();
    cfg.save()?;
    let catalog = load_catalog()?;
    if let Some(id) = target {
        let Some(p) = catalog.find_plugin(id) else {
            return Err(format!("plugin '{id}' not found"));
        };
        if !p.errors.is_empty() {
            return Err(format!(
                "plugin '{id}' reload error: {}",
                p.errors.join("; ")
            ));
        }
    }
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&catalog_json()?).map_err(|e| e.to_string())?
        );
    } else {
        println!(
            "Reloaded plugins (nonce={}, active={}/{})",
            cfg.reload_nonce,
            catalog.active_plugins().count(),
            catalog.plugins.len()
        );
    }
    Ok(0)
}

fn cmd_remove(args: &[String], as_json: bool) -> Result<i32, String> {
    let id = args
        .iter()
        .find(|a| !a.starts_with('-'))
        .ok_or("usage: masil-agent plugin remove <id>")?;
    let mut cfg = PluginsConfig::load()?;
    let was_linked = cfg.linked.remove(id).is_some();
    cfg.disabled.retain(|x| x != id);
    if cfg.active_theme.as_deref() == Some(id) {
        cfg.active_theme = None;
    }
    let managed_dir = plugins_dir()?.join(id);
    let removed_dir = if !was_linked && managed_dir.is_dir() {
        fs::remove_dir_all(&managed_dir)
            .map_err(|e| format!("cannot remove {}: {e}", managed_dir.display()))?;
        true
    } else {
        false
    };
    if !was_linked && !removed_dir {
        return Err(format!("plugin '{id}' not found"));
    }
    cfg.bump_reload();
    cfg.save()?;
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "id": id,
                "unlinked": was_linked,
                "deleted_dir": removed_dir,
            }))
            .map_err(|e| e.to_string())?
        );
    } else if was_linked {
        println!("Unlinked local plugin '{id}' (source folder kept intact).");
    } else {
        println!("Removed plugin '{id}'.");
    }
    Ok(0)
}

fn cmd_logs(args: &[String], as_json: bool) -> Result<i32, String> {
    let mut id: Option<String> = None;
    let mut tail = 80usize;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--json" => i += 1,
            "--tail" | "-n" => {
                tail = args
                    .get(i + 1)
                    .ok_or("--tail requires a number")?
                    .parse()
                    .map_err(|_| "--tail must be a positive integer")?;
                i += 2;
            }
            val if !val.starts_with('-') && id.is_none() => {
                id = Some(val.to_owned());
                i += 1;
            }
            other => return Err(format!("unknown option '{other}'")),
        }
    }
    let id = id.ok_or("usage: masil-agent plugin logs <id> [--tail N]")?;
    let lines = runner::read_plugin_logs(&id, tail)?;
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "id": id, "lines": lines }))
                .map_err(|e| e.to_string())?
        );
    } else if lines.is_empty() {
        println!("(no logs yet for '{id}')");
    } else {
        for line in lines {
            println!("{line}");
        }
    }
    Ok(0)
}

fn cmd_theme(args: &[String], as_json: bool) -> Result<i32, String> {
    let sub = args.first().map(String::as_str).unwrap_or("list");
    match sub {
        "list" | "ls" => {
            let catalog = load_catalog()?;
            let themes = catalog.all_themes();
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "active_theme": catalog.active_theme,
                        "builtin": ["dark", "light", "system", "terminal"],
                        "plugin_themes": themes,
                    }))
                    .map_err(|e| e.to_string())?
                );
            } else {
                println!(
                    "Active theme: {}",
                    catalog.active_theme.as_deref().unwrap_or("dark")
                );
                println!("Built-in: dark, light, system, terminal");
                for t in themes {
                    println!(
                        "  {:<22} {} ({}, plugin:{})",
                        t.id, t.name, t.appearance, t.plugin_id
                    );
                }
            }
            Ok(0)
        }
        "set" => {
            let id = args
                .get(1)
                .ok_or("usage: masil-agent plugin theme set <theme-id>")?;
            let out = set_active_theme(Some(id))?;
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&out).map_err(|e| e.to_string())?
                );
            } else {
                println!("Active theme set to '{id}'.");
            }
            Ok(0)
        }
        "reset" => {
            let out = set_active_theme(None)?;
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&out).map_err(|e| e.to_string())?
                );
            } else {
                println!("Active theme reset to default.");
            }
            Ok(0)
        }
        _ => Err("usage: masil-agent plugin theme list | set <theme-id> | reset".into()),
    }
}

fn cmd_skill(args: &[String], as_json: bool) -> Result<i32, String> {
    let sub = args.first().map(String::as_str).unwrap_or("status");
    let mut target = "all".to_owned();
    let mut force = false;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--json" => i += 1,
            "--force" => {
                force = true;
                i += 1;
            }
            "--target" | "-t" => {
                target = args.get(i + 1).ok_or("--target requires value")?.clone();
                i += 2;
            }
            val if !val.starts_with('-') => {
                target = val.to_owned();
                i += 1;
            }
            other => return Err(format!("unknown option '{other}'")),
        }
    }
    match sub {
        "status" | "ls" => {
            let statuses = skill::inspect_all();
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({ "skills": statuses }))
                        .map_err(|e| e.to_string())?
                );
            } else {
                for s in statuses {
                    println!(
                        "  {:<10} {:<20} cli={} skill={} paths={}",
                        s.id,
                        s.label,
                        if s.cli_installed { "found" } else { "missing" },
                        if s.skill_installed { "installed" } else { "off" },
                        s.paths.join(", ")
                    );
                }
            }
            Ok(0)
        }
        "install" => {
            let paths = skill::install_target(&target, force)?;
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "installed": paths,
                        "skills": skill::inspect_all(),
                    }))
                    .map_err(|e| e.to_string())?
                );
            } else {
                for p in paths {
                    println!("Installed masil-plugin skill -> {p}");
                }
            }
            Ok(0)
        }
        "uninstall" | "remove" => {
            let paths = skill::uninstall_target(&target)?;
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "removed": paths,
                        "skills": skill::inspect_all(),
                    }))
                    .map_err(|e| e.to_string())?
                );
            } else if paths.is_empty() {
                println!("No masil-managed skills found for target '{target}'.");
            } else {
                for p in paths {
                    println!("Removed masil-plugin skill -> {p}");
                }
            }
            Ok(0)
        }
        _ => Err("usage: masil-agent plugin skill status | install [--target claude|codex|agy|all] [--force] | uninstall [--target ...]".into()),
    }
}

fn cmd_create_prompt(args: &[String]) -> Result<i32, String> {
    let mut goal_parts = Vec::new();
    let mut plugin_id = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--id" => {
                plugin_id = Some(args.get(i + 1).ok_or("--id requires value")?.clone());
                i += 2;
            }
            val => {
                goal_parts.push(val.to_owned());
                i += 1;
            }
        }
    }
    let goal = goal_parts.join(" ");
    if goal.trim().is_empty() {
        return Err("usage: masil-agent plugin create-prompt <goal> [--id ID]".into());
    }
    let dir = plugins_dir()?;
    println!(
        "{}",
        skill::build_creator_prompt(&goal, plugin_id.as_deref(), &dir)
    );
    Ok(0)
}
