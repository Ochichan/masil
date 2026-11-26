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
       sync-theme [--socket PATH]\n  \
       open <plugin-id> [panel-id] [--socket PATH]\n  \
       run-cmd <plugin-id> <command-id> [--args TEXT] [--socket PATH]\n  \
       menu [--socket PATH] [--client CLIENT]\n  \
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

    if let Ok(catalog) = load_catalog() {
        sync_theme_to_masil_servers(&catalog);
    }

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

struct MasilBarColors<'a> {
    panel: &'a str,
    raised: &'a str,
    text: &'a str,
    muted: &'a str,
    accent: &'a str,
    accent_text: &'a str,
    warning: &'a str,
    track: &'a str,
}

fn builtin_bar_colors(theme: &str) -> MasilBarColors<'static> {
    match theme {
        "light" => MasilBarColors {
            panel: "#fafbfb",
            raised: "#dce3e6",
            text: "#192229",
            muted: "#54646f",
            accent: "#00707f",
            accent_text: "#ffffff",
            warning: "#905400",
            track: "#becacf",
        },
        "terminal" => MasilBarColors {
            panel: "default",
            raised: "brightblack",
            text: "default",
            muted: "white",
            accent: "cyan",
            accent_text: "black",
            warning: "yellow",
            track: "brightblack",
        },
        _ => MasilBarColors {
            panel: "#141a22",
            raised: "#1f2731",
            text: "#e6ebf1",
            muted: "#9ba8b6",
            accent: "#5bb5c2",
            accent_text: "#051316",
            warning: "#ebb85c",
            track: "#303a45",
        },
    }
}

fn is_temp_config_isolated() -> bool {
    let Ok(cfg_root) = manifest::config_root() else {
        return false;
    };
    let tmp = std::env::temp_dir();
    let s = cfg_root.to_string_lossy();
    cfg_root.starts_with(&tmp) || s.starts_with("/tmp/") || s.starts_with("/private/tmp/")
}

/// Applies the active plugin theme (or restores built-in colors on reset) live
/// to running `masil` servers whose UI layer is enabled and not in `tmux` mode.
pub fn sync_theme_to_masil_servers(catalog: &manifest::PluginCatalog) {
    if std::env::var_os("MASIL_NO_SERVER_THEME_SYNC").is_some() {
        return;
    }
    let Ok(masil_bin) = crate::native_ui::native_executable() else {
        return;
    };
    if !masil_bin.is_file() {
        return;
    }

    let mut sockets: Vec<String> = Vec::new();
    let isolated_temp = is_temp_config_isolated();
    let has_custom_tmux_dir = std::env::var_os("TMUX_TMPDIR").is_some_and(|v| !v.is_empty());

    if let Some(cur) = crate::api::current_socket() {
        let s = cur.to_string_lossy().into_owned();
        if !sockets.contains(&s) {
            sockets.push(s);
        }
    }
    if !isolated_temp || has_custom_tmux_dir {
        for srv in crate::remote::discovery::discover_masil_servers() {
            if !sockets.contains(&srv.socket) {
                sockets.push(srv.socket);
            }
        }
    }
    if sockets.is_empty() {
        return;
    }

    for sock in sockets {
        sync_theme_to_socket(&masil_bin, &sock, catalog, false);
    }
}

pub fn sync_theme_to_socket(
    masil_bin: &std::path::Path,
    sock: &str,
    catalog: &manifest::PluginCatalog,
    only_if_plugin_theme: bool,
) {
    let resolved = catalog.active_resolved_theme();
    if only_if_plugin_theme && resolved.is_none() {
        return;
    }
    let active_builtin = catalog.active_theme.as_deref().and_then(|t| match t {
        "dark" | "light" | "terminal" => Some(t),
        "system" => Some("dark"),
        _ => None,
    });
    let read_opt = |opt: &str| -> Option<String> {
        let out = std::process::Command::new(masil_bin)
            .arg("-S")
            .arg(sock)
            .args(["show-options", "-gqv", opt])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let val = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        (!val.is_empty()).then_some(val)
    };
    if read_opt("@masil-ui").as_deref() == Some("off") {
        return;
    }
    let server_theme = read_opt("@masil-theme");
    if server_theme.as_deref() == Some("tmux") {
        return;
    }

    let colors = if let Some(ref t) = resolved {
        MasilBarColors {
            panel: &t.panel,
            raised: &t.raised,
            text: &t.text,
            muted: &t.muted,
            accent: &t.accent,
            accent_text: &t.accent_text,
            warning: &t.warning,
            track: &t.track,
        }
    } else {
        let fallback = active_builtin.or(server_theme.as_deref()).unwrap_or("dark");
        builtin_bar_colors(fallback)
    };

    let status_style = format!("bg={},fg={}", colors.panel, colors.muted);
    let win_style = format!("fg={}", colors.muted);
    let win_cur_style = format!("bg={},fg={},bold", colors.raised, colors.text);
    let win_act_style = format!("fg={}", colors.warning);
    let win_bell_style = format!("fg={},bold", colors.warning);
    let msg_style = format!("bg={},fg={}", colors.raised, colors.text);
    let msg_cmd_style = format!("bg={},fg={}", colors.raised, colors.accent);
    let mode_style = format!("bg={},fg={}", colors.accent, colors.accent_text);
    let border_style = format!("fg={}", colors.track);
    let active_border_style = format!("fg={}", colors.accent);
    let menu_style = format!("bg={},fg={}", colors.panel, colors.text);
    let menu_sel_style = format!("bg={},fg={}", colors.accent, colors.accent_text);
    let menu_border_style = format!("fg={}", colors.accent);

    let mut args: Vec<&str> = Vec::new();
    let push_set = |k: &'static str, v: &str, buf: &mut Vec<String>| {
        if !buf.is_empty() {
            buf.push(";".into());
        }
        buf.push("set-option".into());
        buf.push("-g".into());
        buf.push(k.into());
        buf.push(v.to_owned());
    };
    let mut cmd_buf: Vec<String> = Vec::new();
    if let Some(builtin) = active_builtin {
        push_set("@masil-theme", builtin, &mut cmd_buf);
    }
    push_set("@masil-c-panel", colors.panel, &mut cmd_buf);
    push_set("@masil-c-raised", colors.raised, &mut cmd_buf);
    push_set("@masil-c-text", colors.text, &mut cmd_buf);
    push_set("@masil-c-muted", colors.muted, &mut cmd_buf);
    push_set("@masil-c-accent", colors.accent, &mut cmd_buf);
    push_set("@masil-c-accent-text", colors.accent_text, &mut cmd_buf);
    push_set("@masil-c-warning", colors.warning, &mut cmd_buf);
    push_set("@masil-c-track", colors.track, &mut cmd_buf);
    push_set("status-style", &status_style, &mut cmd_buf);
    push_set("window-status-style", &win_style, &mut cmd_buf);
    push_set("window-status-current-style", &win_cur_style, &mut cmd_buf);
    push_set("window-status-activity-style", &win_act_style, &mut cmd_buf);
    push_set("window-status-bell-style", &win_bell_style, &mut cmd_buf);
    push_set("message-style", &msg_style, &mut cmd_buf);
    push_set("message-command-style", &msg_cmd_style, &mut cmd_buf);
    push_set("mode-style", &mode_style, &mut cmd_buf);
    push_set("pane-border-style", &border_style, &mut cmd_buf);
    push_set(
        "pane-active-border-style",
        &active_border_style,
        &mut cmd_buf,
    );
    push_set("menu-style", &menu_style, &mut cmd_buf);
    push_set("menu-selected-style", &menu_sel_style, &mut cmd_buf);
    push_set("menu-border-style", &menu_border_style, &mut cmd_buf);
    push_set("clock-mode-colour", colors.accent, &mut cmd_buf);
    args.extend(cmd_buf.iter().map(String::as_str));

    let _ = std::process::Command::new(masil_bin)
        .arg("-S")
        .arg(sock)
        .args(&args)
        .output();
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
        "sync-theme" => cmd_sync_theme(&args[1..]),
        "open" => cmd_open(&args[1..]),
        "run-cmd" => cmd_run_cmd(&args[1..]),
        "menu" => cmd_menu(&args[1..]),
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
            match rt.block_on(runner::invoke_panel_mode(
                &plugin,
                &panel.id,
                Some(sample_agent.clone()),
                None,
                None,
                Map::new(),
                None,
                false,
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
            match rt.block_on(runner::invoke_command_full(
                &plugin,
                &cmd.id,
                String::new(),
                Some(sample_agent.clone()),
                None,
                None,
                Map::new(),
                None,
                false,
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
            let out = rt.block_on(runner::invoke_command_full(
                &plugin,
                pos[2],
                cmd_args,
                Some(agent),
                action,
                None,
                Map::new(),
                socket,
                true,
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
    if let Ok(updated) = load_catalog() {
        sync_theme_to_masil_servers(&updated);
    }
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
    sync_theme_to_masil_servers(&catalog);
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
    let existing = load_catalog().ok();
    let existing_plugin = existing.as_ref().and_then(|c| c.find_plugin(id)).cloned();
    let mut cfg = PluginsConfig::load()?;
    let was_linked = cfg.linked.remove(id).is_some();
    cfg.disabled.retain(|x| x != id);
    let owned_active_theme = cfg.active_theme.as_deref().is_some_and(|active_id| {
        active_id == id
            || existing_plugin
                .as_ref()
                .is_some_and(|p| p.themes.iter().any(|t| t.id == active_id))
    });
    if owned_active_theme {
        cfg.active_theme = None;
    }
    let managed_dir = existing_plugin
        .as_ref()
        .filter(|p| p.source == "installed")
        .map(|p| p.dir.clone())
        .unwrap_or_else(|| plugins_dir().unwrap_or_default().join(id));
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
    if owned_active_theme && let Ok(updated) = load_catalog() {
        sync_theme_to_masil_servers(&updated);
    }
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

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn cmd_sync_theme(args: &[String]) -> Result<i32, String> {
    let mut socket: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--socket" => {
                socket = Some(args.get(i + 1).ok_or("--socket requires a path")?.clone());
                i += 2;
            }
            _ => i += 1,
        }
    }
    let catalog = load_catalog()?;
    if let Some(sock) = socket {
        if let Ok(masil_bin) = crate::native_ui::native_executable()
            && masil_bin.is_file()
        {
            sync_theme_to_socket(&masil_bin, &sock, &catalog, true);
        }
    } else if catalog.active_resolved_theme().is_some() {
        sync_theme_to_masil_servers(&catalog);
    }
    Ok(0)
}

fn cmd_open(args: &[String]) -> Result<i32, String> {
    let mut socket: Option<String> = None;
    let mut pos: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--socket" => {
                socket = Some(args.get(i + 1).ok_or("--socket requires a path")?.clone());
                i += 2;
            }
            val if !val.starts_with('-') => {
                pos.push(val);
                i += 1;
            }
            other => return Err(format!("unknown option '{other}'")),
        }
    }
    let plugin_id = pos
        .first()
        .copied()
        .ok_or("usage: masil-agent plugin open <plugin-id> [panel-id] [--socket PATH]")?;
    let panel_id = pos.get(1).copied();
    crate::ui::plugin_view::run_standalone_panel(plugin_id, panel_id, socket.as_deref())
}

fn cmd_run_cmd(args: &[String]) -> Result<i32, String> {
    let mut socket: Option<String> = None;
    let mut cmd_args = String::new();
    let mut pos: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--socket" => {
                socket = Some(args.get(i + 1).ok_or("--socket requires a path")?.clone());
                i += 2;
            }
            "--args" => {
                cmd_args = args.get(i + 1).ok_or("--args requires text")?.clone();
                i += 2;
            }
            val if !val.starts_with('-') => {
                pos.push(val);
                i += 1;
            }
            other => return Err(format!("unknown option '{other}'")),
        }
    }
    if pos.len() < 2 {
        return Err(
            "usage: masil-agent plugin run-cmd <plugin-id> <command-id> [--args TEXT] [--socket PATH]"
                .into(),
        );
    }
    crate::ui::plugin_view::run_standalone_command(pos[0], pos[1], &cmd_args, socket.as_deref())
}

fn cmd_menu(args: &[String]) -> Result<i32, String> {
    let mut socket: Option<String> = None;
    let mut client: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--socket" => {
                socket = Some(args.get(i + 1).ok_or("--socket requires a path")?.clone());
                i += 2;
            }
            "--client" => {
                client = Some(args.get(i + 1).ok_or("--client requires a name")?.clone());
                i += 2;
            }
            _ => i += 1,
        }
    }
    let sock = socket
        .or_else(|| crate::api::current_socket().map(|p| p.to_string_lossy().into_owned()))
        .ok_or("usage: masil-agent plugin menu --socket PATH [--client NAME]")?;
    let masil_bin = crate::native_ui::native_executable()?;
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate masil-agent: {e}"))?;
    let exe_str = exe.to_string_lossy().into_owned();

    let ko = std::process::Command::new(&masil_bin)
        .arg("-S")
        .arg(&sock)
        .args(["show-options", "-gqv", "@masil-lang"])
        .output()
        .ok()
        .is_some_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "ko");
    let tr = |en: &'static str, k: &'static str| if ko { k } else { en };

    let catalog = load_catalog()?;
    let mut menu_args: Vec<String> = vec!["display-menu".into(), "-M".into()];
    if let Some(c) = client.as_deref().filter(|s| !s.is_empty()) {
        menu_args.push("-c".into());
        menu_args.push(c.to_owned());
        menu_args.push("-t".into());
        menu_args.push(c.to_owned());
    }
    menu_args.extend([
        "-x".into(),
        "C".into(),
        "-y".into(),
        "C".into(),
        "-T".into(),
        format!("#[align=centre]{}", tr("Plugins", "플러그인")),
    ]);

    let mut hotkey_n = 1usize;
    let mut has_panels = false;
    for plugin in catalog.active_plugins() {
        let Some(m) = &plugin.manifest else { continue };
        for panel in &m.panels {
            has_panels = true;
            let label = format!("{}: {} ({})", tr("Panel", "패널"), panel.title, plugin.id)
                .replace('#', "##");
            let key = if hotkey_n <= 9 {
                let k = hotkey_n.to_string();
                hotkey_n += 1;
                k
            } else {
                String::new()
            };
            let shell_cmd = format!(
                "{} plugin open {} {} --socket {}",
                sh_quote(&exe_str),
                sh_quote(&plugin.id),
                sh_quote(&panel.id),
                sh_quote(&sock)
            );
            let title_q = sh_quote(&panel.title);
            let cmd = format!(
                "new-pane -x 52% -y 58% -T {title_q} {}",
                sh_quote(&shell_cmd)
            );
            menu_args.push(label);
            menu_args.push(key);
            menu_args.push(cmd);
        }
    }

    let mut has_cmds = false;
    for plugin in catalog.active_plugins() {
        let Some(m) = &plugin.manifest else { continue };
        for c in &m.commands {
            if !has_cmds && has_panels {
                menu_args.push(String::new());
            }
            has_cmds = true;
            let slash = format!("/{}", c.slash.trim_start_matches('/'));
            let label = format!("{slash} — {} ({})", c.title, plugin.id).replace('#', "##");
            let shell_cmd = format!(
                "{} plugin run-cmd {} {} --socket {}",
                sh_quote(&exe_str),
                sh_quote(&plugin.id),
                sh_quote(&c.id),
                sh_quote(&sock)
            );
            let cmd = format!(
                "new-pane -O -K -x 66% -y 66% -X 17% -Y 17% {}",
                sh_quote(&shell_cmd)
            );
            menu_args.push(label);
            menu_args.push(String::new());
            menu_args.push(cmd);
        }
    }

    let themes = catalog.all_themes();
    if !themes.is_empty() || catalog.active_resolved_theme().is_some() {
        if has_panels || has_cmds {
            menu_args.push(String::new());
        }
        for t in &themes {
            let active = catalog.active_theme.as_deref() == Some(t.id.as_str());
            let mark = if active { "● " } else { "" };
            let label = format!("{mark}{}: {}", tr("Theme", "테마"), t.name).replace('#', "##");
            let cmd = format!(
                "run-shell -b {}",
                sh_quote(&format!(
                    "{} plugin theme set {}",
                    sh_quote(&exe_str),
                    sh_quote(&t.id)
                ))
            );
            menu_args.push(label);
            menu_args.push(String::new());
            menu_args.push(cmd);
        }
        if catalog.active_resolved_theme().is_some() {
            let label = tr("Theme: Reset default", "테마: 기본값 복원").to_owned();
            let cmd = format!(
                "run-shell -b {}",
                sh_quote(&format!("{} plugin theme reset", sh_quote(&exe_str)))
            );
            menu_args.push(label);
            menu_args.push(String::new());
            menu_args.push(cmd);
        }
    }

    if !has_panels && !has_cmds && themes.is_empty() {
        menu_args.push(format!(
            "-{}",
            tr("No active plugins", "활성 플러그인 없음")
        ));
        menu_args.push(String::new());
        menu_args.push(String::new());
    }

    menu_args.push(String::new());
    let settings_shell = format!("{} settings --group plugins", sh_quote(&exe_str));
    menu_args.push(tr("Manage plugins (Settings)", "플러그인 관리 (설정)").to_owned());
    menu_args.push("s".into());
    menu_args.push(format!(
        "new-pane -O -K -x 90% -y 90% -X 5% -Y 5% {}",
        sh_quote(&settings_shell)
    ));

    let reload_shell = format!("{} plugin reload", sh_quote(&exe_str));
    menu_args.push(tr("Reload plugins", "플러그인 다시 불러오기").to_owned());
    menu_args.push("r".into());
    menu_args.push(format!("run-shell -b {}", sh_quote(&reload_shell)));

    let out = std::process::Command::new(&masil_bin)
        .arg("-S")
        .arg(&sock)
        .args(&menu_args)
        .output()
        .map_err(|e| format!("failed opening plugin menu: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_owned());
    }
    Ok(0)
}
