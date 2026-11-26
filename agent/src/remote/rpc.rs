//! JSON RPC dispatcher for authenticated mobile clients. Bridges existing
//! `masil-agent` CLI/durable operations, workspace files/git diff, live
//! transcripts, checkpoints, worktrees, schedules, and external imports.

use crate::native_ui::native_executable;
use crate::remote::config::{
    DeviceStore, HostIdentity, RemoteConfig, resolve_advertised_direct_addrs,
};
use crate::remote::discovery::{
    default_masil_socket, discover_importable_sessions, discover_masil_servers,
    list_all_host_agents, run_self_agent_json, run_self_agent_json_env,
};
use crate::remote::files::{
    browse_host_directories, create_host_directory, git_changes_diff, git_changes_list,
    list_directory, read_file_preview,
};
use crate::remote::hooks;
use crate::remote::prompts;
use crate::remote::transcripts::{candidate_sessions_for_agent, fetch_timeline};
use masil_wire::rpc::{InteractivePrompt, RpcRequest, RpcResponse};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;

#[derive(Debug, Clone)]
pub struct SessionAuth {
    pub device_id: String,
    pub platform: String,
}

pub async fn handle_rpc(
    req: RpcRequest,
    auth: &mut Option<SessionAuth>,
    identity: &HostIdentity,
    config: &RemoteConfig,
) -> RpcResponse {
    match req.kind.as_str() {
        "auth.pair.request" => {
            let pair_token = req.params["pair_token"].as_str().unwrap_or("");
            let device_name = req.params["device_name"].as_str().unwrap_or("Mobile");
            let platform = req.params["platform"].as_str().unwrap_or("unknown");
            match DeviceStore::redeem_pair_token(pair_token, device_name, platform) {
                Ok((dev, device_secret)) => {
                    *auth = Some(SessionAuth {
                        device_id: dev.device_id.clone(),
                        platform: dev.platform.clone(),
                    });
                    RpcResponse::ok(
                        req.id,
                        &req.kind,
                        json!({
                            "device_id": dev.device_id,
                            "device_secret": device_secret,
                            "host_id": identity.host_id,
                            "host_name": config.effective_host_name(),
                            "direct_addrs": resolve_advertised_direct_addrs(config),
                            "relay_url": config.relay_url,
                        }),
                    )
                }
                Err(err) => RpcResponse::err(req.id, &req.kind, "pair_rejected", err),
            }
        }
        "auth.hello.request" => {
            let device_id = req.params["device_id"].as_str().unwrap_or("");
            let device_secret = req.params["device_secret"].as_str().unwrap_or("");
            match DeviceStore::authenticate_device(device_id, device_secret) {
                Ok(dev) => {
                    *auth = Some(SessionAuth {
                        device_id: dev.device_id.clone(),
                        platform: dev.platform.clone(),
                    });
                    RpcResponse::ok(
                        req.id,
                        &req.kind,
                        json!({
                            "authenticated": true,
                            "host_id": identity.host_id,
                            "host_name": config.effective_host_name(),
                            "version": env!("CARGO_PKG_VERSION"),
                            "direct_addrs": resolve_advertised_direct_addrs(config),
                            "relay_url": config.relay_url,
                            "capabilities": host_capabilities(config),
                        }),
                    )
                }
                Err(err) => RpcResponse::err(req.id, &req.kind, "unauthorized", err),
            }
        }
        _ => {
            let Some(session) = auth.as_ref() else {
                return RpcResponse::err(
                    req.id,
                    &req.kind,
                    "unauthorized",
                    "authenticate with auth.hello.request or auth.pair.request first",
                );
            };
            dispatch_authenticated(req, session, identity, config).await
        }
    }
}

fn host_capabilities(config: &RemoteConfig) -> Value {
    let sb = crate::sandbox::status();
    json!({
        "live_terminal": true,
        "interactive_prompts": true,
        "structured_answers": true,
        "transcript_providers": ["claude", "codex", "agy", "opencode"],
        "desktop_size_preserved": true,
        "voice_host_stt": config.stt_command.as_ref().is_some_and(|v| !v.is_empty()),
        "relay_configured": config.relay_url.is_some(),
        "push_enabled": config.push_enabled && config.relay_url.is_some(),
        "plugins": true,
        "plugin_ui_spec": crate::plugin::ui_spec::UI_SPEC_VERSION,
        "checkpoint_preview": true,
        "worktree_management": true,
        "timeline_streaming": true,
        "turn_audit": true,
        "bs_detector": true,
        "collision_radar": true,
        "circuit_breaker": true,
        "cache_warmth": true,
        "localhost_tunnel": true,
        "visual_markup": true,
        "live_pulse_push": true,
        "focus_briefing": true,
        "permission_firewall": true,
        "os_sandbox": {
            "available": sb.available,
            "default_enabled": sb.default_enabled,
            "backend": sb.backend,
        },
        "agent_arena": true,
        "time_travel_dvr": true,
    })
}

pub async fn dispatch_authenticated_rpc(
    req: RpcRequest,
    session: &SessionAuth,
    identity: &HostIdentity,
    config: &RemoteConfig,
) -> RpcResponse {
    dispatch_authenticated(req, session, identity, config).await
}

async fn dispatch_authenticated(
    req: RpcRequest,
    session: &SessionAuth,
    identity: &HostIdentity,
    config: &RemoteConfig,
) -> RpcResponse {
    let id = req.id.clone();
    let kind = req.kind.clone();
    let params = req.params.clone();

    match kind.as_str() {
        "host.info.request" => {
            let servers = discover_masil_servers();
            let hook_status = hooks::inspect_all();
            RpcResponse::ok(
                id,
                &kind,
                json!({
                    "host_id": identity.host_id,
                    "host_name": config.effective_host_name(),
                    "version": env!("CARGO_PKG_VERSION"),
                    "direct_addrs": resolve_advertised_direct_addrs(config),
                    "relay_url": config.relay_url,
                    "servers": servers,
                    "hooks": hook_status,
                    "capabilities": host_capabilities(config),
                }),
            )
        }
        "host.push.register.request" => {
            let platform = params["platform"].as_str().unwrap_or(&session.platform);
            let token = params["token"].as_str().unwrap_or("");
            match DeviceStore::register_push_token(&session.device_id, platform, token) {
                Ok(()) => RpcResponse::ok(id, &kind, json!({"registered": true})),
                Err(e) => RpcResponse::err(id, &kind, "push_register_failed", e),
            }
        }
        "host.live_pulse.register.request" => {
            let platform = params["platform"].as_str().unwrap_or(&session.platform);
            let enabled = params["enabled"].as_bool().unwrap_or(false);
            let summary_enabled = params["summary_enabled"]
                .as_bool()
                .or_else(|| params["live_activity_summary"].as_bool())
                .unwrap_or(false);
            let push_to_start = params["push_to_start_token"].as_str();
            let push_token = params["push_token"]
                .as_str()
                .or_else(|| params["activity_token"].as_str());
            match DeviceStore::register_live_pulse(
                &session.device_id,
                platform,
                enabled,
                summary_enabled,
                push_to_start,
                push_token,
            ) {
                Ok(dev) => RpcResponse::ok(
                    id,
                    &kind,
                    json!({
                        "registered": true,
                        "enabled": dev.live_activity_enabled,
                        "summary_enabled": dev.live_activity_summary,
                        "has_push_to_start_token": dev.live_activity_push_to_start_token.is_some(),
                        "has_push_token": dev.live_activity_push_token.is_some(),
                    }),
                ),
                Err(e) => RpcResponse::err(id, &kind, "live_pulse_register_failed", e),
            }
        }
        "host.models.list.request" => {
            let provider = params["provider"].as_str().unwrap_or("agy").to_owned();
            let result =
                tokio::task::spawn_blocking(move || list_provider_models_and_options(&provider))
                    .await
                    .unwrap_or_else(|_| json!({"provider": "unknown", "models": []}));
            RpcResponse::ok(id, &kind, result)
        }
        "host.dirs.list.request" => {
            let path = params["path"].as_str().unwrap_or("").to_owned();
            let show_hidden = params["show_hidden"].as_bool().unwrap_or(false);
            let include_quick = params["include_quick"].as_bool().unwrap_or(true);
            match tokio::task::spawn_blocking(move || {
                browse_host_directories(&path, show_hidden, include_quick)
            })
            .await
            {
                Ok(Ok(val)) => RpcResponse::ok(id, &kind, val),
                Ok(Err(e)) => RpcResponse::err(id, &kind, "dirs_list_failed", e),
                Err(e) => RpcResponse::err(id, &kind, "dirs_list_failed", e.to_string()),
            }
        }
        "host.dirs.mkdir.request" => {
            let parent = params["parent"].as_str().unwrap_or("").to_owned();
            let name = params["name"].as_str().unwrap_or("").to_owned();
            match tokio::task::spawn_blocking(move || create_host_directory(&parent, &name)).await {
                Ok(Ok(val)) => RpcResponse::ok(id, &kind, val),
                Ok(Err(e)) => RpcResponse::err(id, &kind, "dirs_mkdir_failed", e),
                Err(e) => RpcResponse::err(id, &kind, "dirs_mkdir_failed", e.to_string()),
            }
        }
        "host.hooks.set.request" => {
            let provider = params["provider"].as_str().unwrap_or("");
            let enable = params["enabled"].as_bool().unwrap_or(true);
            let res = if enable {
                hooks::install_provider(provider)
            } else {
                hooks::uninstall_provider(provider)
            };
            match res {
                Ok(msg) => RpcResponse::ok(
                    id,
                    &kind,
                    json!({"ok": true, "message": msg, "hooks": hooks::inspect_all()}),
                ),
                Err(e) => RpcResponse::err(id, &kind, "hooks_failed", e),
            }
        }
        "agent.list.request" => {
            let agents = tokio::task::spawn_blocking(list_all_host_agents)
                .await
                .unwrap_or_default();
            let servers = discover_masil_servers();
            RpcResponse::ok(id, &kind, json!({"agents": agents, "servers": servers}))
        }
        "agent.get.request" => {
            let socket = resolve_socket_param(&params);
            let target = preferred_agent_target(&params);
            match run_blocking_agent(socket, vec!["get".into(), target]).await {
                Ok(val) => RpcResponse::ok(id, &kind, val),
                Err(e) => RpcResponse::err(id, &kind, "agent_get_failed", e),
            }
        }
        "agent.requests.list.request" => {
            let socket = resolve_socket_param(&params);
            let pane_id = match resolve_pane_target(socket.as_deref(), &params).await {
                Ok(p) => p,
                Err(e) => return RpcResponse::err(id, &kind, "requests_failed", e),
            };
            let socket_str = socket.unwrap_or_default();
            let (provider, cwd, reported, state) =
                resolve_live_agent_meta(&socket_str, &pane_id, &params).await;
            let agent_key = format!("{socket_str}::{pane_id}");
            let page = tokio::task::spawn_blocking(move || {
                fetch_timeline(crate::remote::transcripts::TimelineQuery {
                    server_socket: &socket_str,
                    pane_id: &pane_id,
                    agent_key: &agent_key,
                    provider: &provider,
                    cwd: &cwd,
                    reported_session_id: reported.as_deref(),
                    state: &state,
                    limit: Some(60),
                })
            })
            .await;
            match page {
                Ok(p) => RpcResponse::ok(
                    id,
                    &kind,
                    json!({
                        "prompts": p.active_prompts,
                    }),
                ),
                Err(e) => RpcResponse::err(id, &kind, "requests_failed", e.to_string()),
            }
        }
        "agent.prompt.respond.request" => handle_prompt_respond(id, &kind, &params).await,
        "agent.timeline.fetch.request" => {
            let socket = resolve_socket_param(&params).unwrap_or_default();
            let pane_id = params["pane"].as_str().unwrap_or("").to_owned();
            let provider = params["provider"].as_str().unwrap_or("").to_owned();
            let cwd = params["cwd"].as_str().unwrap_or("").to_owned();
            let reported = params["session_id"].as_str().map(str::to_owned);
            let state = params["state"].as_str().unwrap_or("unknown").to_owned();
            let limit = params["limit"].as_u64().map(|n| n as usize);
            let agent_key = format!("{socket}::{pane_id}");
            let page = tokio::task::spawn_blocking(move || {
                fetch_timeline(crate::remote::transcripts::TimelineQuery {
                    server_socket: &socket,
                    pane_id: &pane_id,
                    agent_key: &agent_key,
                    provider: &provider,
                    cwd: &cwd,
                    reported_session_id: reported.as_deref(),
                    state: &state,
                    limit,
                })
            })
            .await;
            match page {
                Ok(p) => RpcResponse::ok(id, &kind, serde_json::to_value(p).unwrap_or(Value::Null)),
                Err(e) => RpcResponse::err(id, &kind, "timeline_failed", e.to_string()),
            }
        }
        "agent.session.candidates.request" => {
            let provider = params["provider"].as_str().unwrap_or("").to_owned();
            let cwd = params["cwd"].as_str().unwrap_or("").to_owned();
            let limit = params["limit"].as_u64().unwrap_or(20) as usize;
            let list = tokio::task::spawn_blocking(move || {
                candidate_sessions_for_agent(&provider, &cwd, limit)
            })
            .await
            .unwrap_or_default();
            RpcResponse::ok(id, &kind, json!({"candidates": list}))
        }
        "agent.session.bind.request" => {
            let socket = resolve_socket_param(&params).unwrap_or_default();
            let pane_id = params["pane"].as_str().unwrap_or("");
            let session_id = params["session_id"].as_str().unwrap_or("");
            let key = format!("{socket}::{pane_id}");
            match DeviceStore::set_manual_binding(&key, session_id) {
                Ok(()) => {
                    crate::remote::transcripts::invalidate_agent_cache(&key);
                    RpcResponse::ok(id, &kind, json!({"bound": true, "session_id": session_id}))
                }
                Err(e) => RpcResponse::err(id, &kind, "bind_failed", e),
            }
        }
        "agent.importable.list.request" => {
            let limit = params["limit"].as_u64().unwrap_or(40) as usize;
            let sessions = tokio::task::spawn_blocking(move || discover_importable_sessions(limit))
                .await
                .unwrap_or_default();
            RpcResponse::ok(id, &kind, json!({"sessions": sessions}))
        }
        "agent.import.request" => {
            let socket = ensure_target_socket(&params);
            let provider = params["provider"].as_str().unwrap_or("").to_owned();
            let session_id = params["session_id"].as_str().unwrap_or("").to_owned();
            let cwd = params["cwd"].as_str().unwrap_or("").to_owned();
            let name = params["name"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| {
                    format!("{}-{}", provider, &session_id[..6.min(session_id.len())])
                });
            let mut args = vec!["start".into(), name, provider.clone()];
            if !cwd.is_empty() {
                args.push("--cwd".into());
                args.push(cwd);
            }
            if !session_id.is_empty() {
                args.push("--session".into());
                args.push(session_id);
            }
            if matches!(provider.as_str(), "opencode" | "kilo") {
                args.push("--answers".into());
            }
            match run_blocking_agent(socket, args).await {
                Ok(val) => RpcResponse::ok(id, &kind, val),
                Err(e) => RpcResponse::err(id, &kind, "import_failed", e),
            }
        }
        "agent.start.request" => {
            let socket = ensure_target_socket(&params);
            let name = params["name"].as_str().unwrap_or("agent").to_owned();
            let provider = params["provider"].as_str().unwrap_or("claude").to_owned();
            let mut args = vec!["start".into(), name, provider.clone()];
            if let Some(cwd) = params["cwd"].as_str().filter(|s| !s.is_empty()) {
                args.push("--cwd".into());
                args.push(cwd.to_owned());
            }
            if let Some(wt) = params["worktree"].as_str().filter(|s| !s.is_empty()) {
                args.push("--worktree".into());
                args.push(wt.to_owned());
            }
            if params["no_sandbox"].as_bool() == Some(true)
                || params["sandbox"].as_str() == Some("off")
                || params["sandbox"].as_bool() == Some(false)
            {
                args.push("--no-sandbox".into());
            } else if let Some(sb_profile) = params["sandbox"].as_str().filter(|s| !s.is_empty()) {
                args.push("--sandbox".into());
                args.push(sb_profile.to_owned());
            }
            if params["answers"]
                .as_bool()
                .unwrap_or(matches!(provider.as_str(), "opencode" | "kilo"))
            {
                args.push("--answers".into());
            }
            let mut extra = build_provider_start_flags(
                &provider,
                params["model"]
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty()),
                params["effort"]
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty()),
                params["mode"]
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty()),
            );
            let initial_prompt = params["initial_prompt"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            let mut deferred_prompt: Option<String> = None;
            if let Some(ref prompt_text) = initial_prompt {
                if let Some(prompt_args) =
                    crate::plugin::skill::creator_initial_prompt_args(&provider, prompt_text)
                {
                    extra.extend(prompt_args);
                } else {
                    deferred_prompt = Some(prompt_text.clone());
                }
            }
            if !extra.is_empty() {
                args.push("--".into());
                args.extend(extra);
            }
            match run_blocking_agent(socket.clone(), args).await {
                Ok(val) => {
                    if let Some(prompt_text) = deferred_prompt {
                        let pane_target = val
                            .get("pane_id")
                            .or_else(|| val.get("pane"))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned();
                        if !pane_target.is_empty() {
                            tokio::spawn(deliver_initial_prompt_when_idle(
                                socket.clone(),
                                pane_target,
                                prompt_text,
                            ));
                        }
                    }
                    RpcResponse::ok(id, &kind, val)
                }
                Err(e) => RpcResponse::err(id, &kind, "agent_start_failed", e),
            }
        }
        "agent.configure.request" => {
            let socket = resolve_socket_param(&params);
            let pane_id = match resolve_pane_target(socket.as_deref(), &params).await {
                Ok(p) => p,
                Err(e) => return RpcResponse::err(id, &kind, "configure_failed", e),
            };
            let provider = params["provider"].as_str().unwrap_or("").to_owned();
            let model = params["model"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            let effort = params["effort"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            let mode = params["mode"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            let cmds = build_live_configure_commands(
                &provider,
                model.as_deref(),
                effort.as_deref(),
                mode.as_deref(),
            );
            if cmds.is_empty() {
                return RpcResponse::err(
                    id,
                    &kind,
                    "configure_unsupported",
                    "no supported live options selected for this provider",
                );
            }
            let mut applied = Vec::new();
            for (idx, cmd) in cmds.iter().enumerate() {
                if idx > 0 {
                    tokio::time::sleep(Duration::from_millis(350)).await;
                }
                if let Err(e) =
                    send_text_and_enter_to_pane(socket.as_deref(), &pane_id, cmd, true).await
                {
                    return RpcResponse::err(id, &kind, "configure_failed", e);
                }
                applied.push(cmd.clone());
            }
            RpcResponse::ok(id, &kind, json!({"applied": applied}))
        }
        "agent.prompt.request" => {
            let socket = resolve_socket_param(&params);
            let target = preferred_agent_target(&params);
            let raw_text = params["text"].as_str().unwrap_or("").to_owned();
            let hook_agent = json!({
                "name": params["target"].as_str().unwrap_or(&target),
                "pane": params["pane"].as_str().unwrap_or(&target),
                "server_socket": socket.clone().unwrap_or_default(),
            });
            let text = crate::plugin::runner::run_prompt_before_hooks(
                &raw_text,
                Some(hook_agent),
                socket.as_deref().map(std::path::PathBuf::from),
            )
            .await;
            let allow_raw_slash =
                text.trim_start().starts_with('/') || text.trim_start().starts_with('!');
            if allow_raw_slash {
                // Human mobile control explicitly allows slash/shell commands directly into live TUI.
                let pane_id = match resolve_pane_target(socket.as_deref(), &params).await {
                    Ok(p) => p,
                    Err(e) => return RpcResponse::err(id, &kind, "prompt_failed", e),
                };
                match send_text_and_enter_to_pane(socket.as_deref(), &pane_id, &text, true).await {
                    Ok(()) => RpcResponse::ok(
                        id,
                        &kind,
                        json!({"delivered": true, "mode": "direct_human_keys", "pane": pane_id}),
                    ),
                    Err(e) => RpcResponse::err(id, &kind, "prompt_failed", e),
                }
            } else {
                let mut args = vec!["prompt".into(), target.clone(), text.clone()];
                if let Some(run) = params["run"].as_str().filter(|s| !s.is_empty()) {
                    args.push("--run".into());
                    args.push(run.to_owned());
                }
                match run_blocking_agent_env(
                    socket.clone(),
                    args,
                    vec![("MASIL_SKIP_PROMPT_HOOK", "1")],
                )
                .await
                {
                    Ok(val) => RpcResponse::ok(id, &kind, val),
                    Err(err) if params["force_keys"].as_bool().unwrap_or(false) => {
                        let pane_id = match resolve_pane_target(socket.as_deref(), &params).await {
                            Ok(p) => p,
                            Err(e) => return RpcResponse::err(id, &kind, "prompt_failed", e),
                        };
                        match send_text_and_enter_to_pane(socket.as_deref(), &pane_id, &text, true)
                            .await
                        {
                            Ok(()) => RpcResponse::ok(
                                id,
                                &kind,
                                json!({"delivered": true, "mode": "fallback_keys", "pane": pane_id, "note": err}),
                            ),
                            Err(e) => RpcResponse::err(id, &kind, "prompt_failed", e),
                        }
                    }
                    Err(e) => RpcResponse::err(id, &kind, "prompt_failed", e),
                }
            }
        }
        "agent.interrupt.request" => {
            let socket = resolve_socket_param(&params);
            let target = preferred_agent_target(&params);
            let args = vec![
                "interrupt".into(),
                target.clone(),
                "--any-state".into(),
                "--confirm-seconds".into(),
                "1".into(),
            ];
            match run_blocking_agent(socket.clone(), args).await {
                Ok(val) => RpcResponse::ok(id, &kind, val),
                Err(_) => {
                    let pane_id = match resolve_pane_target(socket.as_deref(), &params).await {
                        Ok(p) => p,
                        Err(e) => return RpcResponse::err(id, &kind, "interrupt_failed", e),
                    };
                    let provider = params["provider"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned);
                    let provider = match provider {
                        Some(p) => p,
                        None => {
                            run_blocking_agent(socket.clone(), vec!["get".into(), pane_id.clone()])
                                .await
                                .ok()
                                .and_then(|v| {
                                    v.get("provider").and_then(Value::as_str).map(str::to_owned)
                                })
                                .unwrap_or_default()
                        }
                    };
                    let (steps, delay_ms, mode_name): (&[&str], u64, &str) = match provider.as_str()
                    {
                        "opencode" | "kilo" => (&["\x1b", "\x1b"], 160, "direct_double_esc"),
                        "cursor" => (&["\x03"], 45, "direct_ctrl_c"),
                        _ => (&["\x1b"], 45, "direct_esc"),
                    };
                    match send_paced_keys_to_pane(socket.as_deref(), &pane_id, steps, delay_ms)
                        .await
                    {
                        Ok(()) => RpcResponse::ok(
                            id,
                            &kind,
                            json!({"interrupted": true, "mode": mode_name, "pane": pane_id}),
                        ),
                        Err(e) => RpcResponse::err(id, &kind, "interrupt_failed", e),
                    }
                }
            }
        }
        "agent.approve.request" => {
            let socket = resolve_socket_param(&params);
            let target = preferred_agent_target(&params);
            if let Some(req_id) = params["request_id"].as_str().filter(|s| !s.is_empty()) {
                let raw_decision = params["decision"].as_str().unwrap_or("once");
                let choice = normalize_permission_choice(raw_decision);
                let mut args = vec![
                    "answer".into(),
                    target,
                    req_id.to_owned(),
                    "--choice".into(),
                    choice.to_owned(),
                ];
                if choice == "reject"
                    && let Some(msg) = params["message"]
                        .as_str()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                {
                    args.push("--message".into());
                    args.push(msg.to_owned());
                }
                match run_blocking_agent(socket, args).await {
                    Ok(val) => RpcResponse::ok(id, &kind, val),
                    Err(e) => RpcResponse::err(id, &kind, "approve_failed", e),
                }
            } else {
                let pane_id = match resolve_pane_target(socket.as_deref(), &params).await {
                    Ok(p) => p,
                    Err(e) => return RpcResponse::err(id, &kind, "approve_failed", e),
                };
                let keys = params["key_sequence"].as_str().unwrap_or("\r");
                let res = send_key_sequence_paced(socket.as_deref(), &pane_id, keys).await;
                match res {
                    Ok(()) => RpcResponse::ok(
                        id,
                        &kind,
                        json!({"sent": true, "mode": "human_confirmed_keys", "pane": pane_id}),
                    ),
                    Err(e) => RpcResponse::err(id, &kind, "approve_failed", e),
                }
            }
        }
        "agent.close.request" => {
            let socket = resolve_socket_param(&params);
            let target = preferred_agent_target(&params);
            match run_blocking_agent(socket, vec!["close".into(), target]).await {
                Ok(val) => RpcResponse::ok(id, &kind, val),
                Err(e) => RpcResponse::err(id, &kind, "close_failed", e),
            }
        }
        "agent.queue.list.request" => {
            let socket = resolve_socket_param(&params);
            let target = preferred_agent_target(&params);
            match run_blocking_agent(socket, vec!["queue".into(), target]).await {
                Ok(val) => RpcResponse::ok(id, &kind, val),
                Err(e) => RpcResponse::err(id, &kind, "queue_failed", e),
            }
        }
        "agent.queue.add.request" => {
            let socket = resolve_socket_param(&params);
            let target = preferred_agent_target(&params);
            let text = params["text"].as_str().unwrap_or("").to_owned();
            match run_blocking_agent(socket, vec!["queue".into(), target, "add".into(), text]).await
            {
                Ok(val) => RpcResponse::ok(id, &kind, val),
                Err(e) => RpcResponse::err(id, &kind, "queue_add_failed", e),
            }
        }
        "agent.queue.send.request" => {
            let socket = resolve_socket_param(&params);
            let target = preferred_agent_target(&params);
            let mut args = vec!["queue".into(), target, "send".into()];
            if let Some(item_id) = json_id_to_string(params.get("item_id")) {
                args.push(item_id);
            }
            match run_blocking_agent(socket, args).await {
                Ok(val) => RpcResponse::ok(id, &kind, val),
                Err(e) => RpcResponse::err(id, &kind, "queue_send_failed", e),
            }
        }
        "agent.queue.remove.request" => {
            let socket = resolve_socket_param(&params);
            let target = preferred_agent_target(&params);
            let Some(item_id) = json_id_to_string(params.get("item_id")) else {
                return RpcResponse::err(id, &kind, "queue_remove_failed", "missing item_id");
            };
            match run_blocking_agent(
                socket,
                vec!["queue".into(), target, "remove".into(), item_id],
            )
            .await
            {
                Ok(val) => RpcResponse::ok(id, &kind, val),
                Err(e) => RpcResponse::err(id, &kind, "queue_remove_failed", e),
            }
        }
        "inbox.list.request" => {
            let all = params["all"].as_bool().unwrap_or(false);
            let items = tokio::task::spawn_blocking(move || {
                let mut combined = Vec::new();
                for server in discover_masil_servers() {
                    let mut args = vec!["inbox".into()];
                    if all {
                        args.push("--all".into());
                    }
                    if let Ok(val) = run_self_agent_json(Some(&server.socket), &args)
                        && let Some(arr) = val
                            .get("events")
                            .or_else(|| val.get("items"))
                            .and_then(Value::as_array)
                    {
                        for item in arr {
                            let mut row = item.clone();
                            if let Some(obj) = row.as_object_mut() {
                                obj.insert(
                                    "server_name".into(),
                                    Value::String(server.name.clone()),
                                );
                                obj.insert(
                                    "server_socket".into(),
                                    Value::String(server.socket.clone()),
                                );
                            }
                            combined.push(row);
                        }
                    }
                }
                combined
            })
            .await
            .unwrap_or_default();
            RpcResponse::ok(id, &kind, json!({"items": items}))
        }
        "inbox.ack.request" => {
            let socket = resolve_socket_param(&params);
            let item_id = params["item_id"].as_str().unwrap_or("").to_owned();
            let args = if item_id.is_empty() {
                vec!["inbox".into(), "read-all".into()]
            } else {
                vec!["inbox".into(), "ack".into(), item_id]
            };
            match run_blocking_agent(socket, args).await {
                Ok(val) => RpcResponse::ok(id, &kind, val),
                Err(e) => RpcResponse::err(id, &kind, "inbox_ack_failed", e),
            }
        }
        "workspace.files.list.request" => {
            let cwd = params["cwd"].as_str().unwrap_or("").to_owned();
            let path = params["path"].as_str().unwrap_or("").to_owned();
            match tokio::task::spawn_blocking(move || list_directory(&cwd, &path)).await {
                Ok(Ok(val)) => RpcResponse::ok(id, &kind, val),
                Ok(Err(e)) => RpcResponse::err(id, &kind, "files_list_failed", e),
                Err(e) => RpcResponse::err(id, &kind, "files_list_failed", e.to_string()),
            }
        }
        "workspace.files.read.request" => {
            let cwd = params["cwd"].as_str().unwrap_or("").to_owned();
            let path = params["path"].as_str().unwrap_or("").to_owned();
            match tokio::task::spawn_blocking(move || read_file_preview(&cwd, &path)).await {
                Ok(Ok(val)) => RpcResponse::ok(id, &kind, val),
                Ok(Err(e)) => RpcResponse::err(id, &kind, "files_read_failed", e),
                Err(e) => RpcResponse::err(id, &kind, "files_read_failed", e.to_string()),
            }
        }
        "workspace.changes.list.request" => {
            let cwd = params["cwd"].as_str().unwrap_or("").to_owned();
            match tokio::task::spawn_blocking(move || git_changes_list(&cwd)).await {
                Ok(Ok(val)) => RpcResponse::ok(id, &kind, val),
                Ok(Err(e)) => RpcResponse::err(id, &kind, "changes_failed", e),
                Err(e) => RpcResponse::err(id, &kind, "changes_failed", e.to_string()),
            }
        }
        "workspace.changes.diff.request" => {
            let cwd = params["cwd"].as_str().unwrap_or("").to_owned();
            let path = params["path"].as_str().unwrap_or("").to_owned();
            match tokio::task::spawn_blocking(move || git_changes_diff(&cwd, &path)).await {
                Ok(Ok(val)) => RpcResponse::ok(id, &kind, val),
                Ok(Err(e)) => RpcResponse::err(id, &kind, "diff_failed", e),
                Err(e) => RpcResponse::err(id, &kind, "diff_failed", e.to_string()),
            }
        }
        "checkpoint.list.request" => {
            let socket = resolve_socket_param(&params);
            let target = preferred_agent_target(&params);
            match run_blocking_agent(socket, vec!["checkpoint".into(), target, "list".into()]).await
            {
                Ok(val) => RpcResponse::ok(id, &kind, val),
                Err(e) => RpcResponse::err(id, &kind, "checkpoint_failed", e),
            }
        }
        "checkpoint.make.request" => {
            let socket = resolve_socket_param(&params);
            let target = preferred_agent_target(&params);
            match run_blocking_agent(socket, vec!["checkpoint".into(), target, "make".into()]).await
            {
                Ok(val) => RpcResponse::ok(id, &kind, val),
                Err(e) => RpcResponse::err(id, &kind, "checkpoint_failed", e),
            }
        }
        "checkpoint.preview.request" => {
            let socket = resolve_socket_param(&params);
            let target = preferred_agent_target(&params);
            let Some(cp_id) = json_id_to_string(params.get("checkpoint_id")) else {
                return RpcResponse::err(
                    id,
                    &kind,
                    "checkpoint_preview_failed",
                    "missing checkpoint_id",
                );
            };
            let mut paths = collect_checkpoint_paths(&params);
            if paths.is_empty() {
                let show_val = match run_blocking_agent(
                    socket.clone(),
                    vec![
                        "checkpoint".into(),
                        target.clone(),
                        "show".into(),
                        cp_id.clone(),
                    ],
                )
                .await
                {
                    Ok(v) => v,
                    Err(e) => return RpcResponse::err(id, &kind, "checkpoint_preview_failed", e),
                };
                extract_restorable_paths_from_show(&show_val, &mut paths);
                if paths.is_empty() {
                    return RpcResponse::ok(
                        id,
                        &kind,
                        json!({
                            "stage": "unchanged",
                            "id": cp_id,
                            "token": Value::Null,
                            "files": [],
                            "diff": "",
                            "message": "Working tree already matches this checkpoint.",
                        }),
                    );
                }
            }
            let mut preview_args = vec!["checkpoint".into(), target, "restore".into(), cp_id];
            preview_args.extend(paths.iter().cloned());
            match run_blocking_agent(socket, preview_args).await {
                Ok(mut preview) => {
                    let single = preview
                        .get("files")
                        .and_then(Value::as_array)
                        .filter(|files| files.len() == 1)
                        .and_then(|files| files.first())
                        .map(|first| {
                            (
                                first.get("path").cloned(),
                                first.get("diff").cloned(),
                                first.get("action").cloned(),
                            )
                        });
                    if let Some((path_val, diff_val, action_val)) = single {
                        if let Some(p) = path_val {
                            preview["path"] = p;
                        }
                        if let Some(d) = diff_val {
                            preview["diff"] = d;
                        }
                        if let Some(a) = action_val {
                            preview["action"] = a;
                        }
                    }
                    RpcResponse::ok(id, &kind, preview)
                }
                Err(e) => RpcResponse::err(id, &kind, "checkpoint_preview_failed", e),
            }
        }
        "checkpoint.restore.request" => {
            let socket = resolve_socket_param(&params);
            let target = preferred_agent_target(&params);
            let Some(cp_id) = json_id_to_string(params.get("checkpoint_id")) else {
                return RpcResponse::err(
                    id,
                    &kind,
                    "checkpoint_restore_failed",
                    "missing checkpoint_id",
                );
            };
            let mut paths = collect_checkpoint_paths(&params);
            if paths.is_empty() {
                let show_val = match run_blocking_agent(
                    socket.clone(),
                    vec![
                        "checkpoint".into(),
                        target.clone(),
                        "show".into(),
                        cp_id.clone(),
                    ],
                )
                .await
                {
                    Ok(v) => v,
                    Err(e) => return RpcResponse::err(id, &kind, "checkpoint_restore_failed", e),
                };
                extract_restorable_paths_from_show(&show_val, &mut paths);
                if paths.is_empty() {
                    return RpcResponse::ok(
                        id,
                        &kind,
                        json!({
                            "stage": "unchanged",
                            "id": cp_id,
                            "message": "Working tree already matches this checkpoint.",
                        }),
                    );
                }
            }
            let explicit_token = params["confirm_token"]
                .as_str()
                .or_else(|| params["token"].as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            let token = match explicit_token {
                Some(tok) => tok,
                None => {
                    let mut preview_args = vec![
                        "checkpoint".into(),
                        target.clone(),
                        "restore".into(),
                        cp_id.clone(),
                    ];
                    preview_args.extend(paths.iter().cloned());
                    let preview = match run_blocking_agent(socket.clone(), preview_args).await {
                        Ok(v) => v,
                        Err(e) => {
                            return RpcResponse::err(id, &kind, "checkpoint_restore_failed", e);
                        }
                    };
                    let Some(tok) = preview
                        .get("token")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                    else {
                        return RpcResponse::ok(id, &kind, preview);
                    };
                    tok.to_owned()
                }
            };
            let mut confirm_args = vec!["checkpoint".into(), target, "restore".into(), cp_id];
            confirm_args.extend(paths);
            confirm_args.push("--confirm".into());
            confirm_args.push(token);
            match run_blocking_agent(socket, confirm_args).await {
                Ok(val) => RpcResponse::ok(id, &kind, val),
                Err(e) => RpcResponse::err(id, &kind, "checkpoint_restore_failed", e),
            }
        }
        "worktree.list.request" => {
            let mut list_args: Vec<String> = Vec::new();
            if let Some(repo) = params["repo"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                list_args.push(repo.to_owned());
            }
            if params["all"].as_bool().unwrap_or(false) {
                list_args.push("--all".into());
            }
            match tokio::task::spawn_blocking(move || crate::worktree::listing(&list_args)).await {
                Ok(Ok(val)) => RpcResponse::ok(id, &kind, val),
                Ok(Err(e)) => RpcResponse::err(id, &kind, "worktree_list_failed", e),
                Err(e) => RpcResponse::err(id, &kind, "worktree_list_failed", e.to_string()),
            }
        }
        "worktree.create.request" => {
            let repo = params["repo"].as_str().unwrap_or("").to_owned();
            let branch = params["branch"].as_str().unwrap_or("").to_owned();
            let from_ref = params["from"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            let name = params["name"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            let res = tokio::task::spawn_blocking(move || {
                let exe = std::env::current_exe().map_err(|e| e.to_string())?;
                let mut args = vec!["worktree".into(), "create".into(), repo, branch];
                if let Some(from) = from_ref {
                    args.push("--from".into());
                    args.push(from);
                }
                if let Some(n) = name {
                    args.push("--name".into());
                    args.push(n);
                }
                let out = Command::new(exe)
                    .args(&args)
                    .output()
                    .map_err(|e| e.to_string())?;
                let stdout = String::from_utf8_lossy(&out.stdout).trim().to_owned();
                if out.status.success() {
                    let mut parsed: Value =
                        serde_json::from_str(&stdout).unwrap_or_else(|_| json!({"raw": stdout}));
                    let hoisted = parsed.get("result").and_then(Value::as_object).map(|r| {
                        (
                            r.get("path").cloned(),
                            r.get("name").cloned(),
                            r.get("branch").cloned(),
                        )
                    });
                    if let (Some(obj), Some((path, name, branch))) =
                        (parsed.as_object_mut(), hoisted)
                    {
                        if let Some(v) = path {
                            obj.entry("path".to_owned()).or_insert(v);
                        }
                        if let Some(v) = name {
                            obj.entry("name".to_owned()).or_insert(v);
                        }
                        if let Some(v) = branch {
                            obj.entry("branch".to_owned()).or_insert(v);
                        }
                    }
                    Ok(parsed)
                } else {
                    Err(String::from_utf8_lossy(&out.stderr).trim().to_owned())
                }
            })
            .await;
            match res {
                Ok(Ok(val)) => RpcResponse::ok(id, &kind, val),
                Ok(Err(e)) => RpcResponse::err(id, &kind, "worktree_create_failed", e),
                Err(e) => RpcResponse::err(id, &kind, "worktree_create_failed", e.to_string()),
            }
        }
        "worktree.remove.request" => {
            let target = params["worktree"]
                .as_str()
                .or_else(|| params["target"].as_str())
                .or_else(|| params["path"].as_str())
                .or_else(|| params["name"].as_str())
                .map(str::trim)
                .unwrap_or("")
                .to_owned();
            if target.is_empty() {
                return RpcResponse::err(
                    id,
                    &kind,
                    "worktree_remove_failed",
                    "missing worktree target",
                );
            }
            let repo = params["repo"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            let confirm = params["confirm"]
                .as_str()
                .or_else(|| params["confirm_token"].as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            let key = params["key"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            let force = params["force"].as_bool().unwrap_or(false);
            let delete_branch = params["delete_branch"].as_bool().unwrap_or(false);
            let no_wait = params["no_wait"].as_bool().unwrap_or(false);
            let res = tokio::task::spawn_blocking(move || {
                let exe = std::env::current_exe().map_err(|e| e.to_string())?;
                let mut args = vec!["worktree".into(), "remove".into(), target];
                if let Some(r) = repo {
                    args.push("--repo".into());
                    args.push(r);
                }
                if force {
                    args.push("--force".into());
                }
                if let Some(c) = confirm {
                    args.push("--confirm".into());
                    args.push(c);
                }
                if delete_branch {
                    args.push("--delete-branch".into());
                }
                if let Some(k) = key {
                    args.push("--key".into());
                    args.push(k);
                }
                if no_wait {
                    args.push("--no-wait".into());
                }
                let out = Command::new(exe)
                    .args(&args)
                    .output()
                    .map_err(|e| e.to_string())?;
                let stdout = String::from_utf8_lossy(&out.stdout).trim().to_owned();
                let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
                if out.status.success() {
                    serde_json::from_str(&stdout).or_else(|_| Ok(json!({"raw": stdout})))
                } else if !stdout.is_empty()
                    && let Ok(mut parsed) = serde_json::from_str::<Value>(&stdout)
                {
                    let hoisted_token = parsed.get("result").and_then(|r| r.get("token")).cloned();
                    if let Some(obj) = parsed.as_object_mut() {
                        obj.insert("needs_confirmation".into(), json!(true));
                        obj.insert("error".into(), json!(stderr));
                        if let Some(tok) = hoisted_token {
                            obj.entry("token".to_owned()).or_insert(tok);
                        }
                    }
                    Ok(parsed)
                } else {
                    Err(if stderr.is_empty() { stdout } else { stderr })
                }
            })
            .await;
            match res {
                Ok(Ok(val)) => RpcResponse::ok(id, &kind, val),
                Ok(Err(e)) => RpcResponse::err(id, &kind, "worktree_remove_failed", e),
                Err(e) => RpcResponse::err(id, &kind, "worktree_remove_failed", e.to_string()),
            }
        }
        "schedule.list.request" => {
            let socket = resolve_socket_param(&params);
            match run_blocking_agent(socket, vec!["schedule".into(), "list".into()]).await {
                Ok(val) => RpcResponse::ok(id, &kind, val),
                Err(e) => RpcResponse::err(id, &kind, "schedule_list_failed", e),
            }
        }
        "schedule.toggle.request" => {
            let socket = resolve_socket_param(&params);
            let name = params["name"].as_str().unwrap_or("").to_owned();
            let verb = if params["enabled"].as_bool().unwrap_or(true) {
                "enable"
            } else {
                "disable"
            };
            match run_blocking_agent(socket, vec!["schedule".into(), verb.into(), name]).await {
                Ok(val) => RpcResponse::ok(id, &kind, val),
                Err(e) => RpcResponse::err(id, &kind, "schedule_toggle_failed", e),
            }
        }
        "audit.latest.request" => {
            let socket = resolve_socket_param(&params);
            let target = preferred_agent_target(&params);
            let mut args = vec!["audit".into(), target, "--json".into()];
            if let Some(turn_id) = json_id_to_string(params.get("turn_id"))
                .or_else(|| json_id_to_string(params.get("audit_id")))
            {
                args.push("--turn".into());
                args.push(turn_id);
            }
            match run_blocking_agent(socket, args).await {
                Ok(val) => RpcResponse::ok(id, &kind, val),
                Err(e) => RpcResponse::err(id, &kind, "audit_latest_failed", e),
            }
        }
        "audit.history.request" => {
            let socket = resolve_socket_param(&params);
            let target = preferred_agent_target(&params);
            match run_blocking_agent(
                socket,
                vec!["audit".into(), target, "--history".into(), "--json".into()],
            )
            .await
            {
                Ok(val) => RpcResponse::ok(id, &kind, val),
                Err(e) => RpcResponse::err(id, &kind, "audit_history_failed", e),
            }
        }
        "audit.challenge.request" => {
            let socket = resolve_socket_param(&params);
            let target = preferred_agent_target(&params);
            let finding_id = params["finding_id"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            let file_hint = params["file"]
                .as_str()
                .or_else(|| params["path"].as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            let custom_prompt = params["prompt"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            let audit_id = json_id_to_string(params.get("audit_id"));

            let prompt_text = if let Some(custom) = custom_prompt {
                custom
            } else {
                let mut fetch_args = vec!["audit".into(), target.clone(), "--json".into()];
                if let Some(aid) = audit_id {
                    fetch_args.push("--turn".into());
                    fetch_args.push(aid);
                }
                match run_blocking_agent(socket.clone(), fetch_args).await {
                    Ok(val) => {
                        if let Ok(report) = serde_json::from_value::<crate::audit::AuditReport>(
                            val["audit"].clone(),
                        ) {
                            crate::audit::ui_card::build_challenge_prompt(
                                &report,
                                finding_id.as_deref(),
                                file_hint.as_deref(),
                            )
                        } else {
                            format!(
                                "[masil Audit] Please review finding {} and fix the root cause before re-running tests.",
                                finding_id.as_deref().unwrap_or("latest")
                            )
                        }
                    }
                    Err(e) => return RpcResponse::err(id, &kind, "audit_challenge_failed", e),
                }
            };

            let delivery = params["delivery"].as_str().unwrap_or("queue");
            let send_res = if delivery == "prompt" {
                run_blocking_agent(
                    socket,
                    vec!["prompt".into(), target.clone(), prompt_text.clone()],
                )
                .await
            } else {
                run_blocking_agent(
                    socket,
                    vec![
                        "queue".into(),
                        target.clone(),
                        "add".into(),
                        prompt_text.clone(),
                    ],
                )
                .await
            };
            match send_res {
                Ok(res) => RpcResponse::ok(
                    id,
                    &kind,
                    json!({
                        "challenged": true,
                        "delivery": delivery,
                        "prompt": prompt_text,
                        "result": res,
                    }),
                ),
                Err(e) => RpcResponse::err(id, &kind, "audit_challenge_failed", e),
            }
        }
        "audit.rollback_file.request" => {
            let socket = resolve_socket_param(&params);
            let target = preferred_agent_target(&params);
            let Some(file_path) = params["path"]
                .as_str()
                .or_else(|| params["file"].as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
            else {
                return RpcResponse::err(id, &kind, "audit_rollback_failed", "missing path");
            };
            let cp_id = match json_id_to_string(params.get("checkpoint_id"))
                .or_else(|| json_id_to_string(params.get("prev_checkpoint_id")))
            {
                Some(c) => c,
                None => {
                    // Look up latest audit's prev_checkpoint_id if caller omitted it.
                    let fetched = run_blocking_agent(
                        socket.clone(),
                        vec!["audit".into(), target.clone(), "--json".into()],
                    )
                    .await;
                    match fetched.ok().and_then(|v| {
                        v["audit"]["prev_checkpoint_id"]
                            .as_str()
                            .or_else(|| v["audit"]["checkpoint_id"].as_str())
                            .map(str::to_owned)
                    }) {
                        Some(c) => c,
                        None => {
                            return RpcResponse::err(
                                id,
                                &kind,
                                "audit_rollback_failed",
                                "no pre-turn checkpoint available for rollback",
                            );
                        }
                    }
                }
            };
            let explicit_token = params["confirm_token"]
                .as_str()
                .or_else(|| params["token"].as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            let token = match explicit_token {
                Some(tok) => tok,
                None => {
                    let preview = match run_blocking_agent(
                        socket.clone(),
                        vec![
                            "checkpoint".into(),
                            target.clone(),
                            "restore".into(),
                            cp_id.clone(),
                            file_path.clone(),
                        ],
                    )
                    .await
                    {
                        Ok(v) => v,
                        Err(e) => return RpcResponse::err(id, &kind, "audit_rollback_failed", e),
                    };
                    let Some(tok) = preview
                        .get("token")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                    else {
                        return RpcResponse::ok(id, &kind, preview);
                    };
                    tok.to_owned()
                }
            };
            match run_blocking_agent(
                socket,
                vec![
                    "checkpoint".into(),
                    target,
                    "restore".into(),
                    cp_id,
                    file_path,
                    "--confirm".into(),
                    token,
                ],
            )
            .await
            {
                Ok(val) => RpcResponse::ok(id, &kind, val),
                Err(e) => RpcResponse::err(id, &kind, "audit_rollback_failed", e),
            }
        }
        "collision.matrix.request" => {
            let repo = params["repo"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            let socket = resolve_socket_param(&params);
            let res = tokio::task::spawn_blocking(move || {
                let exe = std::env::current_exe().map_err(|e| e.to_string())?;
                let mut args = vec!["worktree".into(), "conflicts".into()];
                if let Some(r) = repo {
                    args.push(r);
                }
                if let Some(s) = socket {
                    args.push("--socket".into());
                    args.push(s);
                }
                args.push("--json".into());
                let out = Command::new(exe)
                    .args(&args)
                    .output()
                    .map_err(|e| e.to_string())?;
                let stdout = String::from_utf8_lossy(&out.stdout).trim().to_owned();
                if out.status.success() {
                    serde_json::from_str(&stdout).or_else(|_| Ok(json!({"raw": stdout})))
                } else {
                    Err(String::from_utf8_lossy(&out.stderr).trim().to_owned())
                }
            })
            .await;
            match res {
                Ok(Ok(val)) => RpcResponse::ok(id, &kind, val),
                Ok(Err(e)) => RpcResponse::err(id, &kind, "collision_matrix_failed", e),
                Err(e) => RpcResponse::err(id, &kind, "collision_matrix_failed", e.to_string()),
            }
        }
        "collision.shadow_check.request" => {
            let wt_a = params["worktree_a"]
                .as_str()
                .or_else(|| params["agent_a"].as_str())
                .unwrap_or("")
                .trim()
                .to_owned();
            let wt_b = params["worktree_b"]
                .as_str()
                .or_else(|| params["agent_b"].as_str())
                .unwrap_or("")
                .trim()
                .to_owned();
            let cmd_str = params["cmd"]
                .as_str()
                .or_else(|| params["command"].as_str())
                .unwrap_or("")
                .trim()
                .to_owned();
            let repo = params["repo"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            if wt_a.is_empty() || wt_b.is_empty() || cmd_str.is_empty() {
                return RpcResponse::err(
                    id,
                    &kind,
                    "collision_shadow_failed",
                    "worktree_a, worktree_b, and cmd are required",
                );
            }
            let res = tokio::task::spawn_blocking(move || {
                let exe = std::env::current_exe().map_err(|e| e.to_string())?;
                let mut args = vec![
                    "worktree".into(),
                    "shadow-check".into(),
                    wt_a,
                    wt_b,
                    "--cmd".into(),
                    cmd_str,
                ];
                if let Some(r) = repo {
                    args.push("--repo".into());
                    args.push(r);
                }
                let out = Command::new(exe)
                    .args(&args)
                    .output()
                    .map_err(|e| e.to_string())?;
                let stdout = String::from_utf8_lossy(&out.stdout).trim().to_owned();
                if out.status.success() {
                    serde_json::from_str(&stdout).or_else(|_| Ok(json!({"raw": stdout})))
                } else {
                    Err(String::from_utf8_lossy(&out.stderr).trim().to_owned())
                }
            })
            .await;
            match res {
                Ok(Ok(val)) => RpcResponse::ok(id, &kind, val),
                Ok(Err(e)) => RpcResponse::err(id, &kind, "collision_shadow_failed", e),
                Err(e) => RpcResponse::err(id, &kind, "collision_shadow_failed", e.to_string()),
            }
        }
        "collision.warn_agent.request" => {
            let target = preferred_agent_target(&params);
            let sibling = params["sibling"]
                .as_str()
                .or_else(|| params["about"].as_str())
                .unwrap_or("")
                .trim()
                .to_owned();
            if target.is_empty() || sibling.is_empty() {
                return RpcResponse::err(
                    id,
                    &kind,
                    "collision_warn_failed",
                    "target and sibling are required",
                );
            }
            let socket = resolve_socket_param(&params);
            let res = tokio::task::spawn_blocking(move || {
                let exe = std::env::current_exe().map_err(|e| e.to_string())?;
                let mut args = vec![
                    "worktree".into(),
                    "warn".into(),
                    target,
                    "--about".into(),
                    sibling,
                ];
                if let Some(s) = socket {
                    args.push("--socket".into());
                    args.push(s);
                }
                let out = Command::new(exe)
                    .args(&args)
                    .output()
                    .map_err(|e| e.to_string())?;
                let stdout = String::from_utf8_lossy(&out.stdout).trim().to_owned();
                if out.status.success() {
                    serde_json::from_str(&stdout).or_else(|_| Ok(json!({"raw": stdout})))
                } else {
                    Err(String::from_utf8_lossy(&out.stderr).trim().to_owned())
                }
            })
            .await;
            match res {
                Ok(Ok(val)) => RpcResponse::ok(id, &kind, val),
                Ok(Err(e)) => RpcResponse::err(id, &kind, "collision_warn_failed", e),
                Err(e) => RpcResponse::err(id, &kind, "collision_warn_failed", e.to_string()),
            }
        }
        "guard.status.request" => {
            let socket = resolve_socket_param(&params);
            let target = preferred_agent_target(&params);
            if !target.is_empty() {
                match run_blocking_agent(socket, vec!["guard".into(), target, "status".into()])
                    .await
                {
                    Ok(val) => RpcResponse::ok(id, &kind, val),
                    Err(e) => RpcResponse::err(id, &kind, "guard_status_failed", e),
                }
            } else {
                let sock_path = socket
                    .map(std::path::PathBuf::from)
                    .or_else(|| default_masil_socket().ok());
                let Some(sp) = sock_path else {
                    return RpcResponse::ok(id, &kind, json!({"breakers": [], "cache_warmth": []}));
                };
                let res = tokio::task::spawn_blocking(move || -> Result<Value, String> {
                    let store = crate::managed::Store::open(&sp)?;
                    let now = crate::observation::now_ms();
                    let breakers: Vec<Value> = store
                        .list_circuit_breakers()?
                        .iter()
                        .map(|r| crate::guard::breaker::record_to_json(r, None, false, None))
                        .collect();
                    let warmth: Vec<Value> = store
                        .list_cache_warmth()?
                        .iter()
                        .map(|r| {
                            crate::guard::cache_warmth::CacheWarmthSnapshot::from_record(r, now)
                                .to_json()
                        })
                        .collect();
                    Ok(json!({
                        "breakers": breakers,
                        "cache_warmth": warmth,
                    }))
                })
                .await;
                match res {
                    Ok(Ok(val)) => RpcResponse::ok(id, &kind, val),
                    Ok(Err(e)) => RpcResponse::err(id, &kind, "guard_status_failed", e),
                    Err(e) => RpcResponse::err(id, &kind, "guard_status_failed", e.to_string()),
                }
            }
        }
        "guard.breaker.action.request" => {
            let socket = resolve_socket_param(&params);
            let target = preferred_agent_target(&params);
            let action = params["action"].as_str().unwrap_or("").trim().to_owned();
            if target.is_empty() || action.is_empty() {
                return RpcResponse::err(
                    id,
                    &kind,
                    "guard_action_failed",
                    "target and action are required",
                );
            }
            match action.as_str() {
                "extend" => {
                    let turns = params["turns"].as_u64().unwrap_or(10).to_string();
                    match run_blocking_agent(
                        socket,
                        vec![
                            "guard".into(),
                            target,
                            "extend".into(),
                            "--turns".into(),
                            turns,
                        ],
                    )
                    .await
                    {
                        Ok(val) => RpcResponse::ok(id, &kind, val),
                        Err(e) => RpcResponse::err(id, &kind, "guard_action_failed", e),
                    }
                }
                "dismiss" => {
                    match run_blocking_agent(socket, vec!["guard".into(), target, "dismiss".into()])
                        .await
                    {
                        Ok(val) => RpcResponse::ok(id, &kind, val),
                        Err(e) => RpcResponse::err(id, &kind, "guard_action_failed", e),
                    }
                }
                "resume_hint" => {
                    let hint = params["hint"]
                        .as_str()
                        .or_else(|| params["text"].as_str())
                        .unwrap_or("")
                        .trim()
                        .to_owned();
                    if hint.is_empty() {
                        return RpcResponse::err(
                            id,
                            &kind,
                            "guard_action_failed",
                            "hint text is required",
                        );
                    }
                    let _ = run_blocking_agent(
                        socket.clone(),
                        vec![
                            "guard".into(),
                            target.clone(),
                            "extend".into(),
                            "--turns".into(),
                            "10".into(),
                        ],
                    )
                    .await;
                    match run_blocking_agent(
                        socket,
                        vec!["queue".into(), target, "add".into(), hint.clone()],
                    )
                    .await
                    {
                        Ok(res) => RpcResponse::ok(
                            id,
                            &kind,
                            json!({"action": "resume_hint", "hint": hint, "result": res}),
                        ),
                        Err(e) => RpcResponse::err(id, &kind, "guard_action_failed", e),
                    }
                }
                "rollback" => {
                    let cp_id = match json_id_to_string(params.get("checkpoint_id")) {
                        Some(c) => c,
                        None => {
                            let status = run_blocking_agent(
                                socket.clone(),
                                vec!["guard".into(), target.clone(), "status".into()],
                            )
                            .await;
                            match status.ok().and_then(|v| {
                                v["breaker"]["pre_oscillation_checkpoint_id"]
                                    .as_u64()
                                    .map(|n| n.to_string())
                            }) {
                                Some(c) => c,
                                None => {
                                    return RpcResponse::err(
                                        id,
                                        &kind,
                                        "guard_action_failed",
                                        "no pre-oscillation checkpoint recorded",
                                    );
                                }
                            }
                        }
                    };
                    let show_val = match run_blocking_agent(
                        socket.clone(),
                        vec![
                            "checkpoint".into(),
                            target.clone(),
                            "show".into(),
                            cp_id.clone(),
                        ],
                    )
                    .await
                    {
                        Ok(v) => v,
                        Err(e) => return RpcResponse::err(id, &kind, "guard_action_failed", e),
                    };
                    let mut paths = Vec::new();
                    extract_restorable_paths_from_show(&show_val, &mut paths);
                    let _ = run_blocking_agent(
                        socket.clone(),
                        vec!["guard".into(), target.clone(), "dismiss".into()],
                    )
                    .await;
                    if paths.is_empty() {
                        return RpcResponse::ok(
                            id,
                            &kind,
                            json!({"action": "rollback", "checkpoint_id": cp_id, "stage": "unchanged"}),
                        );
                    }
                    let mut preview_args = vec![
                        "checkpoint".into(),
                        target.clone(),
                        "restore".into(),
                        cp_id.clone(),
                    ];
                    preview_args.extend(paths.iter().cloned());
                    let preview = match run_blocking_agent(socket.clone(), preview_args).await {
                        Ok(v) => v,
                        Err(e) => return RpcResponse::err(id, &kind, "guard_action_failed", e),
                    };
                    let Some(tok) = preview
                        .get("token")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                    else {
                        return RpcResponse::ok(id, &kind, preview);
                    };
                    let mut confirm_args =
                        vec!["checkpoint".into(), target, "restore".into(), cp_id];
                    confirm_args.extend(paths);
                    confirm_args.push("--confirm".into());
                    confirm_args.push(tok);
                    match run_blocking_agent(socket, confirm_args).await {
                        Ok(val) => RpcResponse::ok(id, &kind, val),
                        Err(e) => RpcResponse::err(id, &kind, "guard_action_failed", e),
                    }
                }
                "handoff" => {
                    let to_provider = params["provider"]
                        .as_str()
                        .unwrap_or("codex")
                        .trim()
                        .to_owned();
                    let name = params["name"]
                        .as_str()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                        .unwrap_or_else(|| format!("handoff-{to_provider}"));
                    let cwd = params["cwd"]
                        .as_str()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned);
                    let summary = params["summary"]
                        .as_str()
                        .unwrap_or("Continue task after circuit breaker pause on previous agent.")
                        .trim()
                        .to_owned();
                    let mut start_args = vec!["start".into(), name.clone(), to_provider.clone()];
                    if let Some(dir) = cwd {
                        start_args.push("--cwd".into());
                        start_args.push(dir);
                    }
                    let started = match run_blocking_agent(socket.clone(), start_args).await {
                        Ok(v) => v,
                        Err(e) => return RpcResponse::err(id, &kind, "guard_action_failed", e),
                    };
                    let new_pane = started["pane_id"].as_str().unwrap_or(&name).to_owned();
                    let handoff_msg =
                        format!("[masil Circuit Breaker Handoff from `{target}`] {summary}");
                    let _ = run_blocking_agent(
                        socket.clone(),
                        vec!["queue".into(), new_pane.clone(), "add".into(), handoff_msg],
                    )
                    .await;
                    let _ =
                        run_blocking_agent(socket, vec!["guard".into(), target, "dismiss".into()])
                            .await;
                    RpcResponse::ok(
                        id,
                        &kind,
                        json!({
                            "action": "handoff",
                            "new_agent": name,
                            "pane_id": new_pane,
                            "provider": to_provider,
                            "started": started,
                        }),
                    )
                }
                other => RpcResponse::err(
                    id,
                    &kind,
                    "guard_action_failed",
                    format!("unknown guard action `{other}`"),
                ),
            }
        }
        "tunnel.ports.list.request" => {
            let socket = resolve_socket_param(&params);
            let pane = params["pane"]
                .as_str()
                .or_else(|| params["pane_id"].as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            let tunnel_cfg = config.tunnel.clone();
            let res = tokio::task::spawn_blocking(move || {
                crate::remote::ports::discover_tunnel_ports(
                    socket.as_deref(),
                    pane.as_deref(),
                    &tunnel_cfg,
                )
            })
            .await;
            match res {
                Ok(discovery) => RpcResponse::ok(
                    id,
                    &kind,
                    serde_json::to_value(discovery).unwrap_or(Value::Null),
                ),
                Err(e) => RpcResponse::err(id, &kind, "tunnel_ports_failed", e.to_string()),
            }
        }
        "focus.status.request" => {
            let socket = resolve_socket_param(&params);
            let res = tokio::task::spawn_blocking(move || -> Result<Value, String> {
                let sp = match socket {
                    Some(s) => std::path::PathBuf::from(s),
                    None => default_masil_socket()?,
                };
                let status = crate::briefing::shield::get_status(&sp)?;
                Ok(json!({ "focus": status }))
            })
            .await;
            match res {
                Ok(Ok(val)) => RpcResponse::ok(id, &kind, val),
                Ok(Err(e)) => RpcResponse::err(id, &kind, "focus_status_failed", e),
                Err(e) => RpcResponse::err(id, &kind, "focus_status_failed", e.to_string()),
            }
        }
        "focus.set.request" => {
            let socket = resolve_socket_param(&params);
            let active = params["active"].as_bool().unwrap_or(true);
            let duration = params["duration"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("25m")
                .to_owned();
            let reason = params["reason"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            let res = tokio::task::spawn_blocking(move || -> Result<Value, String> {
                let sp = match socket {
                    Some(s) => std::path::PathBuf::from(s),
                    None => default_masil_socket()?,
                };
                if active {
                    let status = crate::briefing::shield::enable(&sp, &duration, reason)?;
                    Ok(json!({ "focus": status, "held_events": [] }))
                } else {
                    let (status, held) = crate::briefing::shield::disable(&sp)?;
                    Ok(json!({ "focus": status, "held_events": held }))
                }
            })
            .await;
            match res {
                Ok(Ok(val)) => RpcResponse::ok(id, &kind, val),
                Ok(Err(e)) => RpcResponse::err(id, &kind, "focus_set_failed", e),
                Err(e) => RpcResponse::err(id, &kind, "focus_set_failed", e.to_string()),
            }
        }
        "briefing.generate.request" => {
            let socket = resolve_socket_param(&params).map(std::path::PathBuf::from);
            let since = params["since"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("8h")
                .to_owned();
            let res = tokio::task::spawn_blocking(move || {
                crate::briefing::digest::generate_briefing(socket.as_deref(), &since)
            })
            .await;
            match res {
                Ok(Ok(report)) => RpcResponse::ok(
                    id,
                    &kind,
                    serde_json::to_value(report).unwrap_or(Value::Null),
                ),
                Ok(Err(e)) => RpcResponse::err(id, &kind, "briefing_failed", e),
                Err(e) => RpcResponse::err(id, &kind, "briefing_failed", e.to_string()),
            }
        }
        // ------------------------------------------------------------------
        // Phase 3A: Zero-Trust Permission Firewall, Batch Approve & OS Sandbox
        // ------------------------------------------------------------------
        "sandbox.status.request" => {
            let st = crate::sandbox::status();
            RpcResponse::ok(id, &kind, serde_json::to_value(st).unwrap_or(Value::Null))
        }
        "policy.list.request" => {
            let socket = resolve_socket_param(&params).map(std::path::PathBuf::from);
            let (allow_tui, rules) = tokio::task::spawn_blocking(move || {
                crate::policy::load_effective_rules(socket.as_deref())
            })
            .await
            .unwrap_or_else(|_| (true, Vec::new()));
            RpcResponse::ok(
                id,
                &kind,
                json!({
                    "allow_tui_rule_auto_approve": allow_tui,
                    "rules": rules,
                }),
            )
        }
        "policy.check.request" => {
            let socket = resolve_socket_param(&params).map(std::path::PathBuf::from);
            let command = params["command"].as_str().unwrap_or("").to_owned();
            let cwd = params["cwd"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            let res = tokio::task::spawn_blocking(move || {
                let (allow_tui, rules) = crate::policy::load_effective_rules(socket.as_deref());
                let mut assessment =
                    crate::policy::risk::assess_command_risk(&command, cwd.as_deref());
                let matched =
                    crate::policy::rules::evaluate_rules(&rules, &assessment, cwd.as_deref());
                if let Some(ref m) = matched {
                    assessment.matched_rule_id = Some(m.rule.id.clone());
                    assessment.matched_action = Some(m.rule.action.as_str().to_owned());
                    if matches!(
                        m.rule.action,
                        crate::policy::rules::PolicyAction::Deny
                            | crate::policy::rules::PolicyAction::AlwaysConfirm
                    ) {
                        assessment.batch_eligible = false;
                    }
                }
                let would_auto_approve = allow_tui
                    && assessment.tier == masil_wire::rpc::PromptRiskTier::Low
                    && !assessment.truncated
                    && matched.as_ref().is_some_and(|m| {
                        m.rule.action == crate::policy::rules::PolicyAction::AutoApprove
                    });
                json!({
                    "command": command,
                    "assessment": assessment,
                    "matched_rule": matched.map(|m| m.rule),
                    "would_auto_approve": would_auto_approve,
                })
            })
            .await;
            match res {
                Ok(val) => RpcResponse::ok(id, &kind, val),
                Err(e) => RpcResponse::err(id, &kind, "policy_check_failed", e.to_string()),
            }
        }
        "policy.upsert.request" => {
            let socket = resolve_socket_param(&params).map(std::path::PathBuf::from);
            let rule_id = params["id"].as_str().unwrap_or("").to_owned();
            let scope = params["scope"].as_str().unwrap_or("global").to_owned();
            let max_risk = params["max_risk"].as_str().unwrap_or("low").to_owned();
            let action_str = params["action"]
                .as_str()
                .unwrap_or("auto_approve")
                .to_owned();
            let match_command: Vec<String> = params["match_command"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect();
            let match_path: Vec<String> = params["match_path"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect();
            let enabled = params["enabled"].as_bool().unwrap_or(true);
            let res = tokio::task::spawn_blocking(move || -> Result<Value, String> {
                let action = crate::policy::rules::PolicyAction::parse(&action_str)?;
                let saved = crate::policy::upsert_rule(
                    socket.as_deref(),
                    crate::policy::rules::PolicyRule {
                        id: rule_id,
                        scope,
                        match_command,
                        match_path,
                        max_risk,
                        action,
                        enabled,
                        source: "mobile".into(),
                    },
                )?;
                Ok(json!({ "saved": true, "rule": saved }))
            })
            .await;
            match res {
                Ok(Ok(val)) => RpcResponse::ok(id, &kind, val),
                Ok(Err(e)) => RpcResponse::err(id, &kind, "policy_upsert_failed", e),
                Err(e) => RpcResponse::err(id, &kind, "policy_upsert_failed", e.to_string()),
            }
        }
        "policy.remove.request" => {
            let socket = resolve_socket_param(&params).map(std::path::PathBuf::from);
            let rule_id = params["rule_id"]
                .as_str()
                .or_else(|| params["id"].as_str())
                .unwrap_or("")
                .to_owned();
            let res = tokio::task::spawn_blocking(move || {
                let removed = crate::policy::remove_rule(socket.as_deref(), &rule_id)?;
                Ok::<_, String>(json!({ "rule_id": rule_id, "removed": removed }))
            })
            .await;
            match res {
                Ok(Ok(val)) => RpcResponse::ok(id, &kind, val),
                Ok(Err(e)) => RpcResponse::err(id, &kind, "policy_remove_failed", e),
                Err(e) => RpcResponse::err(id, &kind, "policy_remove_failed", e.to_string()),
            }
        }
        "policy.revoke.request" => {
            let socket = resolve_socket_param(&params).map(std::path::PathBuf::from);
            let rule_id = params["rule_id"]
                .as_str()
                .or_else(|| params["id"].as_str())
                .unwrap_or("")
                .to_owned();
            let res = tokio::task::spawn_blocking(move || {
                crate::policy::revoke_rule(socket.as_deref(), &rule_id)
            })
            .await;
            match res {
                Ok(Ok(val)) => RpcResponse::ok(id, &kind, val),
                Ok(Err(e)) => RpcResponse::err(id, &kind, "policy_revoke_failed", e),
                Err(e) => RpcResponse::err(id, &kind, "policy_revoke_failed", e.to_string()),
            }
        }
        "policy.audit_log.request" => {
            let socket = resolve_socket_param(&params).map(std::path::PathBuf::from);
            let run_id = params["run_id"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            let limit = params["limit"].as_u64().unwrap_or(50) as usize;
            let res = tokio::task::spawn_blocking(move || -> Result<Value, String> {
                let sp = match socket {
                    Some(s) => s,
                    None => default_masil_socket()?,
                };
                let store = crate::managed::Store::open(&sp)?;
                let entries = store.list_permission_audit_log(run_id.as_deref(), limit)?;
                Ok(json!({ "entries": entries }))
            })
            .await;
            match res {
                Ok(Ok(val)) => RpcResponse::ok(id, &kind, val),
                Ok(Err(e)) => RpcResponse::err(id, &kind, "policy_audit_failed", e),
                Err(e) => RpcResponse::err(id, &kind, "policy_audit_failed", e.to_string()),
            }
        }
        "prompt.batch_approve.request" => {
            // D-10: strictly allows ONLY verified `Low`-risk prompts; High/Critical/Medium
            // are unconditionally rejected from batch approval.
            let items = params["items"].as_array().cloned().unwrap_or_default();
            let mut approved = Vec::new();
            let mut rejected = Vec::new();
            for item in items {
                let sock = item["server_socket"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .or_else(|| resolve_socket_param(&params));
                let pane = item["pane"]
                    .as_str()
                    .or_else(|| item["pane_id"].as_str())
                    .unwrap_or("")
                    .to_owned();
                let expected_prompt_id = item["prompt_id"].as_str().unwrap_or("").to_owned();
                let Some(sock_str) = sock else {
                    rejected.push(json!({"pane": pane, "reason": "missing server socket"}));
                    continue;
                };
                let agent_info =
                    run_blocking_agent(Some(sock_str.clone()), vec!["get".into(), pane.clone()])
                        .await;
                let Ok(info) = agent_info else {
                    rejected.push(json!({"pane": pane, "reason": "agent unavailable"}));
                    continue;
                };
                let provider = info["provider"].as_str().unwrap_or("").to_owned();
                let cwd = info["cwd"].as_str().unwrap_or("").to_owned();
                let run_id = info["run"].as_str().unwrap_or("").to_owned();
                let state = info["state"].as_str().unwrap_or("blocked").to_owned();
                let sock_c = sock_str.clone();
                let pane_c = pane.clone();
                let prov_c = provider.clone();
                let cwd_c = cwd.clone();
                let live = tokio::task::spawn_blocking(move || {
                    prompts::collect_live_prompts_with_cwd(
                        &sock_c,
                        &pane_c,
                        &prov_c,
                        &state,
                        Some(&cwd_c),
                        &[],
                    )
                })
                .await
                .unwrap_or_default();
                let matched_prompt = if expected_prompt_id.is_empty() {
                    live.first()
                } else {
                    live.iter().find(|p| p.id == expected_prompt_id)
                };
                let Some(p) = matched_prompt else {
                    rejected.push(json!({"pane": pane, "reason": "prompt changed or gone"}));
                    continue;
                };
                if !crate::policy::guard::is_batch_approvable(p) {
                    let tier = p
                        .risk
                        .as_ref()
                        .map(|r| r.tier.as_str())
                        .unwrap_or("unknown");
                    rejected.push(json!({
                        "pane": pane,
                        "prompt_id": p.id,
                        "reason": format!("excluded from batch approval (risk={tier})"),
                    }));
                    continue;
                }
                let Some(allow_opt) = crate::policy::guard::find_verified_allow_option(p) else {
                    rejected.push(
                        json!({"pane": pane, "reason": "no verified non-destructive allow option"}),
                    );
                    continue;
                };
                let delivery = if p.mode == masil_wire::rpc::InteractivePromptMode::StructuredApi {
                    if let Some(req_id) = p.request_id.as_deref() {
                        run_blocking_agent(
                            Some(sock_str.clone()),
                            vec![
                                "answer".into(),
                                pane.clone(),
                                req_id.to_owned(),
                                "--choice".into(),
                                "once".into(),
                            ],
                        )
                        .await
                        .map(|_| ())
                    } else {
                        Err("missing request_id".into())
                    }
                } else if let Some(hk) = allow_opt
                    .hotkey
                    .as_deref()
                    .filter(|h| !h.is_empty() && *h != "\r" && *h != "\n")
                {
                    send_raw_bytes_to_pane(Some(&sock_str), &pane, hk.as_bytes()).await
                } else {
                    Err("no verified hotkey".into())
                };
                match delivery {
                    Ok(()) => {
                        if let Some(risk) = p.risk.clone() {
                            let sock_log = std::path::PathBuf::from(&sock_str);
                            let run_log = run_id.clone();
                            let pane_log = pane.clone();
                            let prov_log = provider.clone();
                            let excerpt = p.screen_excerpt.clone();
                            let _ = tokio::task::spawn_blocking(move || {
                                if let Ok(mut st) = crate::managed::Store::open(&sock_log) {
                                    let _ = crate::policy::guard::record_decision_audit(
                                        &mut st,
                                        &run_log,
                                        &pane_log,
                                        risk.matched_rule_id.as_deref(),
                                        &prov_log,
                                        &risk,
                                        "batch_approved",
                                        excerpt.as_deref(),
                                        crate::observation::now_ms(),
                                    );
                                }
                            })
                            .await;
                        }
                        approved.push(json!({"pane": pane, "prompt_id": p.id}));
                    }
                    Err(e) => {
                        rejected.push(json!({"pane": pane, "prompt_id": p.id, "reason": e}));
                    }
                }
            }
            RpcResponse::ok(
                id,
                &kind,
                json!({
                    "approved_count": approved.len(),
                    "rejected_count": rejected.len(),
                    "approved": approved,
                    "rejected": rejected,
                }),
            )
        }
        // ------------------------------------------------------------------
        // Phase 3B: Agent Arena (Best-of-N) & Time-Travel DVR / Trace Export
        // ------------------------------------------------------------------
        "arena.list.request" => {
            let socket = resolve_socket_param(&params).map(std::path::PathBuf::from);
            let limit = params["limit"].as_u64().unwrap_or(20) as usize;
            let res = tokio::task::spawn_blocking(move || {
                crate::arena::list_arenas(socket.as_deref(), limit)
            })
            .await;
            match res {
                Ok(Ok(val)) => RpcResponse::ok(id, &kind, val),
                Ok(Err(e)) => RpcResponse::err(id, &kind, "arena_list_failed", e),
                Err(e) => RpcResponse::err(id, &kind, "arena_list_failed", e.to_string()),
            }
        }
        "arena.start.request" => {
            let socket = resolve_socket_param(&params).map(std::path::PathBuf::from);
            let prompt = params["prompt"].as_str().unwrap_or("").to_owned();
            let repo_path = std::path::PathBuf::from(
                params["repo_path"]
                    .as_str()
                    .or_else(|| params["cwd"].as_str())
                    .unwrap_or("."),
            );
            let racers: Vec<String> = params["racers"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect();
            let verify_cmd = params["verify_cmd"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .map(str::to_owned);
            let no_sandbox = params["no_sandbox"].as_bool().unwrap_or(false);
            let launch_agents = params["launch_agents"].as_bool().unwrap_or(true);
            let res = tokio::task::spawn_blocking(move || {
                crate::arena::start_arena(
                    socket.as_deref(),
                    &crate::arena::StartArenaRequest {
                        prompt,
                        repo_path,
                        racers,
                        verify_cmd,
                        no_sandbox,
                        launch_agents,
                    },
                )
            })
            .await;
            match res {
                Ok(Ok(val)) => RpcResponse::ok(id, &kind, val),
                Ok(Err(e)) => RpcResponse::err(id, &kind, "arena_start_failed", e),
                Err(e) => RpcResponse::err(id, &kind, "arena_start_failed", e.to_string()),
            }
        }
        "arena.status.request" => {
            let socket = resolve_socket_param(&params).map(std::path::PathBuf::from);
            let arena_id = params["arena_id"].as_str().unwrap_or("").to_owned();
            let res = tokio::task::spawn_blocking(move || {
                crate::arena::status_arena(socket.as_deref(), &arena_id)
            })
            .await;
            match res {
                Ok(Ok(val)) => RpcResponse::ok(id, &kind, val),
                Ok(Err(e)) => RpcResponse::err(id, &kind, "arena_status_failed", e),
                Err(e) => RpcResponse::err(id, &kind, "arena_status_failed", e.to_string()),
            }
        }
        "arena.verify.request" => {
            let socket = resolve_socket_param(&params).map(std::path::PathBuf::from);
            let arena_id = params["arena_id"].as_str().unwrap_or("").to_owned();
            let res = tokio::task::spawn_blocking(move || {
                crate::arena::verify_arena(socket.as_deref(), &arena_id)
            })
            .await;
            match res {
                Ok(Ok(val)) => RpcResponse::ok(id, &kind, val),
                Ok(Err(e)) => RpcResponse::err(id, &kind, "arena_verify_failed", e),
                Err(e) => RpcResponse::err(id, &kind, "arena_verify_failed", e.to_string()),
            }
        }
        "arena.promote.request" => {
            let socket = resolve_socket_param(&params).map(std::path::PathBuf::from);
            let arena_id = params["arena_id"].as_str().unwrap_or("").to_owned();
            let winner = params["winner"].as_str().unwrap_or("").to_owned();
            let mode_str = params["mode"].as_str().unwrap_or("branch").to_owned();
            let losers_str = params["losers"].as_str().unwrap_or("archive").to_owned();
            let res = tokio::task::spawn_blocking(move || -> Result<Value, String> {
                let mode = crate::arena::PromoteMode::parse(&mode_str)?;
                let losers = crate::arena::LoserDisposition::parse(&losers_str)?;
                crate::arena::promote_arena(socket.as_deref(), &arena_id, &winner, mode, losers)
            })
            .await;
            match res {
                Ok(Ok(val)) => RpcResponse::ok(id, &kind, val),
                Ok(Err(e)) => RpcResponse::err(id, &kind, "arena_promote_failed", e),
                Err(e) => RpcResponse::err(id, &kind, "arena_promote_failed", e.to_string()),
            }
        }
        "arena.synthesize.request" => {
            let socket = resolve_socket_param(&params).map(std::path::PathBuf::from);
            let arena_id = params["arena_id"].as_str().unwrap_or("").to_owned();
            let judge = params["judge"].as_str().unwrap_or("claude").to_owned();
            let winner = params["winner"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            let launch = params["launch_agent"].as_bool().unwrap_or(true);
            let res = tokio::task::spawn_blocking(move || {
                crate::arena::synthesize_arena(
                    socket.as_deref(),
                    &arena_id,
                    &judge,
                    winner.as_deref(),
                    launch,
                )
            })
            .await;
            match res {
                Ok(Ok(val)) => RpcResponse::ok(id, &kind, val),
                Ok(Err(e)) => RpcResponse::err(id, &kind, "arena_synthesize_failed", e),
                Err(e) => RpcResponse::err(id, &kind, "arena_synthesize_failed", e.to_string()),
            }
        }
        "dvr.list.request" => {
            let socket = resolve_socket_param(&params);
            let run_id = params["run_id"]
                .as_str()
                .or_else(|| params["run"].as_str())
                .unwrap_or("")
                .to_owned();
            let limit = params["limit"].as_u64().unwrap_or(100) as usize;
            let res = tokio::task::spawn_blocking(move || -> Result<Value, String> {
                let sp = match socket {
                    Some(s) => std::path::PathBuf::from(s),
                    None => default_masil_socket()?,
                };
                let turns = crate::dvr::list_turns(&sp, &run_id, limit)?;
                Ok(json!({ "run_id": run_id, "turns": turns }))
            })
            .await;
            match res {
                Ok(Ok(val)) => RpcResponse::ok(id, &kind, val),
                Ok(Err(e)) => RpcResponse::err(id, &kind, "dvr_list_failed", e),
                Err(e) => RpcResponse::err(id, &kind, "dvr_list_failed", e.to_string()),
            }
        }
        "dvr.inspect.request" => {
            let socket = resolve_socket_param(&params);
            let run_id = params["run_id"]
                .as_str()
                .or_else(|| params["run"].as_str())
                .unwrap_or("")
                .to_owned();
            let cwd = std::path::PathBuf::from(params["cwd"].as_str().unwrap_or("."));
            let turn_seq = params["turn_seq"].as_u64();
            let res = tokio::task::spawn_blocking(move || -> Result<Value, String> {
                let sp = match socket {
                    Some(s) => std::path::PathBuf::from(s),
                    None => default_masil_socket()?,
                };
                crate::dvr::inspect_turn(&sp, &cwd, &run_id, turn_seq)
            })
            .await;
            match res {
                Ok(Ok(val)) => RpcResponse::ok(id, &kind, val),
                Ok(Err(e)) => RpcResponse::err(id, &kind, "dvr_inspect_failed", e),
                Err(e) => RpcResponse::err(id, &kind, "dvr_inspect_failed", e.to_string()),
            }
        }
        "dvr.snapshot.request" => {
            let socket = resolve_socket_param(&params);
            let pane_id = params["pane"]
                .as_str()
                .or_else(|| params["pane_id"].as_str())
                .unwrap_or("")
                .to_owned();
            let run_id = params["run_id"]
                .as_str()
                .or_else(|| params["run"].as_str())
                .unwrap_or("")
                .to_owned();
            let agent_name = params["agent_name"].as_str().unwrap_or("agent").to_owned();
            let provider = params["provider"].as_str().unwrap_or("unknown").to_owned();
            let cwd = std::path::PathBuf::from(params["cwd"].as_str().unwrap_or("."));
            let note = params["note"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            let res = tokio::task::spawn_blocking(move || -> Result<Value, String> {
                let sp = match socket {
                    Some(s) => std::path::PathBuf::from(s),
                    None => default_masil_socket()?,
                };
                let excerpt = prompts::capture_visible_pane(&sp.to_string_lossy(), &pane_id);
                let rec = crate::dvr::capture_turn_snapshot(
                    &sp,
                    &cwd,
                    &agent_name,
                    &run_id,
                    &pane_id,
                    &provider,
                    note.as_deref(),
                    excerpt.as_deref(),
                )?;
                Ok(json!({ "turn": rec }))
            })
            .await;
            match res {
                Ok(Ok(val)) => RpcResponse::ok(id, &kind, val),
                Ok(Err(e)) => RpcResponse::err(id, &kind, "dvr_snapshot_failed", e),
                Err(e) => RpcResponse::err(id, &kind, "dvr_snapshot_failed", e.to_string()),
            }
        }
        "dvr.pin.request" => {
            let socket = resolve_socket_param(&params);
            let run_id = params["run_id"]
                .as_str()
                .or_else(|| params["run"].as_str())
                .unwrap_or("")
                .to_owned();
            let cwd = std::path::PathBuf::from(params["cwd"].as_str().unwrap_or("."));
            let turn_seq = params["turn_seq"].as_u64().unwrap_or(0);
            let pinned = params["pinned"].as_bool().unwrap_or(true);
            let note = params["note"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            let res = tokio::task::spawn_blocking(move || -> Result<Value, String> {
                let sp = match socket {
                    Some(s) => std::path::PathBuf::from(s),
                    None => default_masil_socket()?,
                };
                crate::dvr::set_pin(&sp, &cwd, &run_id, turn_seq, pinned, note.as_deref())
            })
            .await;
            match res {
                Ok(Ok(val)) => RpcResponse::ok(id, &kind, val),
                Ok(Err(e)) => RpcResponse::err(id, &kind, "dvr_pin_failed", e),
                Err(e) => RpcResponse::err(id, &kind, "dvr_pin_failed", e.to_string()),
            }
        }
        "dvr.restore.request" => {
            let socket = resolve_socket_param(&params);
            let run_id = params["run_id"]
                .as_str()
                .or_else(|| params["run"].as_str())
                .unwrap_or("")
                .to_owned();
            let cwd = std::path::PathBuf::from(params["cwd"].as_str().unwrap_or("."));
            let turn_seq = params["turn_seq"].as_u64().unwrap_or(0);
            let res = tokio::task::spawn_blocking(move || -> Result<Value, String> {
                let sp = match socket {
                    Some(s) => std::path::PathBuf::from(s),
                    None => default_masil_socket()?,
                };
                crate::dvr::restore_turn(&sp, &cwd, &run_id, turn_seq)
            })
            .await;
            match res {
                Ok(Ok(val)) => RpcResponse::ok(id, &kind, val),
                Ok(Err(e)) => RpcResponse::err(id, &kind, "dvr_restore_failed", e),
                Err(e) => RpcResponse::err(id, &kind, "dvr_restore_failed", e.to_string()),
            }
        }
        "dvr.fork.request" => {
            let socket = resolve_socket_param(&params);
            let run_id = params["run_id"]
                .as_str()
                .or_else(|| params["run"].as_str())
                .unwrap_or("")
                .to_owned();
            let cwd = std::path::PathBuf::from(params["cwd"].as_str().unwrap_or("."));
            let turn_seq = params["turn_seq"].as_u64().unwrap_or(0);
            let branch = params["branch"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            let res = tokio::task::spawn_blocking(move || -> Result<Value, String> {
                let sp = match socket {
                    Some(s) => std::path::PathBuf::from(s),
                    None => default_masil_socket()?,
                };
                crate::dvr::fork_turn(&sp, &cwd, &run_id, turn_seq, branch.as_deref(), None)
            })
            .await;
            match res {
                Ok(Ok(val)) => RpcResponse::ok(id, &kind, val),
                Ok(Err(e)) => RpcResponse::err(id, &kind, "dvr_fork_failed", e),
                Err(e) => RpcResponse::err(id, &kind, "dvr_fork_failed", e.to_string()),
            }
        }
        "dvr.export_trace.request" => {
            let socket = resolve_socket_param(&params);
            let run_id = params["run_id"]
                .as_str()
                .or_else(|| params["run"].as_str())
                .unwrap_or("")
                .to_owned();
            let cwd = std::path::PathBuf::from(params["cwd"].as_str().unwrap_or("."));
            let agent_name = params["agent_name"].as_str().unwrap_or("agent").to_owned();
            let provider = params["provider"].as_str().unwrap_or("unknown").to_owned();
            let strict = params["redact_strict"].as_bool().unwrap_or(true);
            let res = tokio::task::spawn_blocking(move || -> Result<Value, String> {
                let sp = match socket {
                    Some(s) => std::path::PathBuf::from(s),
                    None => default_masil_socket()?,
                };
                let (exported, _) = crate::dvr::export_trace(
                    &sp,
                    &cwd,
                    &agent_name,
                    &provider,
                    &run_id,
                    strict,
                    None,
                )?;
                Ok(json!({
                    "turn_count": exported.turn_count,
                    "total_redactions": exported.total_redactions,
                    "redaction_kinds": exported.redaction_kinds,
                    "html": exported.html,
                }))
            })
            .await;
            match res {
                Ok(Ok(val)) => RpcResponse::ok(id, &kind, val),
                Ok(Err(e)) => RpcResponse::err(id, &kind, "dvr_export_failed", e),
                Err(e) => RpcResponse::err(id, &kind, "dvr_export_failed", e.to_string()),
            }
        }
        "schedule.remove.request" => {
            let socket = resolve_socket_param(&params);
            let name = params["name"].as_str().unwrap_or("").to_owned();
            match run_blocking_agent(socket, vec!["schedule".into(), "remove".into(), name]).await {
                Ok(val) => RpcResponse::ok(id, &kind, val),
                Err(e) => RpcResponse::err(id, &kind, "schedule_remove_failed", e),
            }
        }
        "plugin.list.request" => {
            match tokio::task::spawn_blocking(crate::plugin::catalog_json).await {
                Ok(Ok(val)) => RpcResponse::ok(id, &kind, val),
                Ok(Err(e)) => RpcResponse::err(id, &kind, "plugin_list_failed", e),
                Err(e) => RpcResponse::err(id, &kind, "plugin_list_failed", e.to_string()),
            }
        }
        "plugin.toggle.request" => {
            let plugin_id = params["plugin_id"].as_str().unwrap_or("").to_owned();
            let enabled = params["enabled"].as_bool().unwrap_or(true);
            let res = tokio::task::spawn_blocking(move || -> Result<Value, String> {
                let mut cfg = crate::plugin::manifest::PluginsConfig::load()?;
                cfg.set_plugin_enabled(&plugin_id, enabled);
                cfg.save()?;
                if let Ok(cat) = crate::plugin::manifest::load_catalog() {
                    crate::plugin::sync_theme_to_masil_servers(&cat);
                }
                crate::plugin::catalog_json()
            })
            .await;
            match res {
                Ok(Ok(val)) => RpcResponse::ok(id, &kind, val),
                Ok(Err(e)) => RpcResponse::err(id, &kind, "plugin_toggle_failed", e),
                Err(e) => RpcResponse::err(id, &kind, "plugin_toggle_failed", e.to_string()),
            }
        }
        "plugin.reload.request" => {
            let res = tokio::task::spawn_blocking(|| -> Result<Value, String> {
                let mut cfg = crate::plugin::manifest::PluginsConfig::load()?;
                cfg.bump_reload();
                cfg.save()?;
                if let Ok(cat) = crate::plugin::manifest::load_catalog() {
                    crate::plugin::sync_theme_to_masil_servers(&cat);
                }
                crate::plugin::catalog_json()
            })
            .await;
            match res {
                Ok(Ok(val)) => RpcResponse::ok(id, &kind, val),
                Ok(Err(e)) => RpcResponse::err(id, &kind, "plugin_reload_failed", e),
                Err(e) => RpcResponse::err(id, &kind, "plugin_reload_failed", e.to_string()),
            }
        }
        "plugin.theme.set.request" => {
            let theme_id = params["theme_id"].as_str().map(str::to_owned);
            let res = tokio::task::spawn_blocking(move || -> Result<Value, String> {
                crate::plugin::set_active_theme(theme_id.as_deref())?;
                crate::plugin::catalog_json()
            })
            .await;
            match res {
                Ok(Ok(val)) => RpcResponse::ok(id, &kind, val),
                Ok(Err(e)) => RpcResponse::err(id, &kind, "plugin_theme_failed", e),
                Err(e) => RpcResponse::err(id, &kind, "plugin_theme_failed", e.to_string()),
            }
        }
        "plugin.panel.render.request" => {
            let plugin_id = params["plugin_id"].as_str().unwrap_or("").to_owned();
            let panel_id = params["panel_id"].as_str().unwrap_or("").to_owned();
            let action = params["action"].as_str().map(str::to_owned);
            let payload = params.get("payload").cloned().filter(|v| !v.is_null());
            let form_values = params
                .get("form_values")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let socket = resolve_socket_param(&params).map(std::path::PathBuf::from);
            let mut agent = params.get("agent").cloned().filter(|v| !v.is_null());
            if let (Some(Value::Object(obj)), Some(sock)) = (&mut agent, &socket)
                && obj
                    .get("server_socket")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
            {
                obj.insert(
                    "server_socket".into(),
                    Value::String(sock.to_string_lossy().into_owned()),
                );
            }
            let loaded = tokio::task::spawn_blocking(move || {
                let catalog = crate::plugin::manifest::load_catalog()?;
                let p = catalog
                    .find_plugin(&plugin_id)
                    .cloned()
                    .ok_or_else(|| format!("plugin '{plugin_id}' not found"))?;
                if !p.is_active(catalog.enabled) {
                    return Err(format!("plugin '{plugin_id}' is disabled or has errors"));
                }
                Ok::<_, String>(p)
            })
            .await;
            match loaded {
                Ok(Ok(plugin)) => match crate::plugin::runner::invoke_panel(
                    &plugin,
                    &panel_id,
                    agent,
                    action,
                    payload,
                    form_values,
                    socket,
                )
                .await
                {
                    Ok(doc) => {
                        RpcResponse::ok(id, &kind, serde_json::to_value(doc).unwrap_or(Value::Null))
                    }
                    Err(e) => RpcResponse::err(id, &kind, "plugin_panel_failed", e),
                },
                Ok(Err(e)) => RpcResponse::err(id, &kind, "plugin_panel_failed", e),
                Err(e) => RpcResponse::err(id, &kind, "plugin_panel_failed", e.to_string()),
            }
        }
        "plugin.command.run.request" => {
            let plugin_id = params["plugin_id"].as_str().unwrap_or("").to_owned();
            let command_id = params["command_id"].as_str().unwrap_or("").to_owned();
            let args = params["args"].as_str().unwrap_or("").to_owned();
            let action = params["action"].as_str().map(str::to_owned);
            let payload = params.get("payload").cloned().filter(|v| !v.is_null());
            let form_values = params
                .get("form_values")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let socket = resolve_socket_param(&params).map(std::path::PathBuf::from);
            let mut agent = params.get("agent").cloned().filter(|v| !v.is_null());
            if let (Some(Value::Object(obj)), Some(sock)) = (&mut agent, &socket)
                && obj
                    .get("server_socket")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
            {
                obj.insert(
                    "server_socket".into(),
                    Value::String(sock.to_string_lossy().into_owned()),
                );
            }
            let loaded = tokio::task::spawn_blocking(move || {
                let catalog = crate::plugin::manifest::load_catalog()?;
                let p = catalog
                    .find_plugin(&plugin_id)
                    .cloned()
                    .ok_or_else(|| format!("plugin '{plugin_id}' not found"))?;
                if !p.is_active(catalog.enabled) {
                    return Err(format!("plugin '{plugin_id}' is disabled or has errors"));
                }
                Ok::<_, String>(p)
            })
            .await;
            match loaded {
                Ok(Ok(plugin)) => match crate::plugin::runner::invoke_command_full(
                    &plugin,
                    &command_id,
                    args,
                    agent,
                    action,
                    payload,
                    form_values,
                    socket,
                    true,
                )
                .await
                {
                    Ok(out) => {
                        RpcResponse::ok(id, &kind, serde_json::to_value(out).unwrap_or(Value::Null))
                    }
                    Err(e) => RpcResponse::err(id, &kind, "plugin_command_failed", e),
                },
                Ok(Err(e)) => RpcResponse::err(id, &kind, "plugin_command_failed", e),
                Err(e) => RpcResponse::err(id, &kind, "plugin_command_failed", e.to_string()),
            }
        }
        "plugin.logs.request" => {
            let plugin_id = params["plugin_id"].as_str().unwrap_or("").to_owned();
            let tail = params["tail"].as_u64().unwrap_or(80) as usize;
            match tokio::task::spawn_blocking(move || {
                crate::plugin::runner::read_plugin_logs(&plugin_id, tail)
            })
            .await
            {
                Ok(Ok(lines)) => RpcResponse::ok(id, &kind, json!({ "lines": lines })),
                Ok(Err(e)) => RpcResponse::err(id, &kind, "plugin_logs_failed", e),
                Err(e) => RpcResponse::err(id, &kind, "plugin_logs_failed", e.to_string()),
            }
        }
        "plugin.skill.set.request" => {
            let target = params["target"].as_str().unwrap_or("all").to_owned();
            let enabled = params["enabled"].as_bool().unwrap_or(true);
            let force = params["force"].as_bool().unwrap_or(false);
            let res = tokio::task::spawn_blocking(move || -> Result<Value, String> {
                let paths = if enabled {
                    crate::plugin::skill::install_target(&target, force)?
                } else {
                    crate::plugin::skill::uninstall_target(&target)?
                };
                Ok(json!({
                    "ok": true,
                    "paths": paths,
                    "skills": crate::plugin::skill::inspect_all(),
                }))
            })
            .await;
            match res {
                Ok(Ok(val)) => RpcResponse::ok(id, &kind, val),
                Ok(Err(e)) => RpcResponse::err(id, &kind, "plugin_skill_failed", e),
                Err(e) => RpcResponse::err(id, &kind, "plugin_skill_failed", e.to_string()),
            }
        }
        "plugin.creator.start.request" => {
            let goal = params["goal"].as_str().unwrap_or("").trim().to_owned();
            if goal.is_empty() {
                return RpcResponse::err(
                    id,
                    &kind,
                    "creator_invalid",
                    "describe the plugin you want to create",
                );
            }
            let provider = match params["provider"].as_str().unwrap_or("agy") {
                "claude" | "claude-code" => "claude",
                "codex" => "codex",
                "agy" | "antigravity" => "agy",
                other => {
                    return RpcResponse::err(
                        id,
                        &kind,
                        "creator_invalid",
                        format!(
                            "unsupported creator provider '{other}' (use agy, claude, or codex)"
                        ),
                    );
                }
            }
            .to_owned();
            let plugin_id = params["plugin_id"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            let model = params["model"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            let effort = params["effort"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);

            let plugins_root = match crate::plugin::manifest::plugins_dir() {
                Ok(d) => {
                    let _ = std::fs::create_dir_all(&d);
                    d
                }
                Err(e) => return RpcResponse::err(id, &kind, "creator_failed", e),
            };
            // Ensure the selected CLI has the official `masil-plugin` skill installed.
            let skill_target = provider.clone();
            let _ = tokio::task::spawn_blocking(move || {
                crate::plugin::skill::install_target(&skill_target, false)
            })
            .await;

            let cwd = params["cwd"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| plugins_root.to_string_lossy().into_owned());
            let slug = plugin_id.as_deref().map(str::to_owned).unwrap_or_else(|| {
                let s: String = goal
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
                    .take(14)
                    .collect::<String>()
                    .to_ascii_lowercase();
                if s.is_empty() {
                    format!("custom-{}", crate::remote::config::now_ms() % 10000)
                } else {
                    s
                }
            });
            let agent_name = format!("plugin-{slug}")
                .chars()
                .take(30)
                .collect::<String>();
            let creator_prompt = crate::plugin::skill::build_creator_prompt(
                &goal,
                plugin_id.as_deref(),
                &plugins_root,
            );
            let socket = ensure_target_socket(&params);
            let mut args = vec![
                "start".into(),
                agent_name.clone(),
                provider.clone(),
                "--cwd".into(),
                cwd,
            ];
            let mut extra =
                build_provider_start_flags(&provider, model.as_deref(), effort.as_deref(), None);
            let native_prompt_args =
                crate::plugin::skill::creator_initial_prompt_args(&provider, &creator_prompt);
            let used_native_prompt = native_prompt_args.is_some();
            if let Some(prompt_args) = native_prompt_args {
                extra.extend(prompt_args);
            }
            if !extra.is_empty() {
                args.push("--".into());
                args.extend(extra);
            }
            match run_blocking_agent(socket.clone(), args).await {
                Ok(val) => {
                    if !used_native_prompt {
                        let pane_target = val
                            .get("pane_id")
                            .or_else(|| val.get("pane"))
                            .and_then(Value::as_str)
                            .unwrap_or(&agent_name)
                            .to_owned();
                        tokio::spawn(deliver_initial_prompt_when_idle(
                            socket.clone(),
                            pane_target,
                            creator_prompt.clone(),
                        ));
                    }
                    RpcResponse::ok(
                        id,
                        &kind,
                        json!({
                            "started": val,
                            "agent_name": agent_name,
                            "provider": provider,
                            "prompt": creator_prompt,
                        }),
                    )
                }
                Err(e) => RpcResponse::err(id, &kind, "creator_start_failed", e),
            }
        }
        _ => RpcResponse::err(id, &kind, "unknown_rpc", format!("unsupported RPC: {kind}")),
    }
}

fn resolve_socket_param(params: &Value) -> Option<String> {
    if let Some(s) = params["server_socket"].as_str().filter(|s| !s.is_empty()) {
        return Some(s.to_owned());
    }
    discover_masil_servers()
        .into_iter()
        .next()
        .map(|s| s.socket)
}

fn ensure_target_socket(params: &Value) -> Option<String> {
    if let Some(s) = resolve_socket_param(params) {
        return Some(s);
    }
    let default_sock = default_masil_socket().ok()?;
    let masil = native_executable().ok()?;
    let _ = Command::new(masil)
        .arg("-S")
        .arg(&default_sock)
        .args(["new-session", "-d", "-s", "main"])
        .output();
    Some(default_sock.to_string_lossy().into_owned())
}

async fn run_blocking_agent(socket: Option<String>, args: Vec<String>) -> Result<Value, String> {
    tokio::task::spawn_blocking(move || run_self_agent_json(socket.as_deref(), &args))
        .await
        .map_err(|e| e.to_string())?
}

async fn run_blocking_agent_env(
    socket: Option<String>,
    args: Vec<String>,
    envs: Vec<(&'static str, &'static str)>,
) -> Result<Value, String> {
    tokio::task::spawn_blocking(move || run_self_agent_json_env(socket.as_deref(), &args, &envs))
        .await
        .map_err(|e| e.to_string())?
}

async fn deliver_initial_prompt_when_idle(
    socket: Option<String>,
    pane_target: String,
    prompt: String,
) {
    let deadline = Instant::now() + Duration::from_secs(30);
    tokio::time::sleep(Duration::from_millis(800)).await;
    while Instant::now() < deadline {
        if let Ok(info) =
            run_blocking_agent(socket.clone(), vec!["get".into(), pane_target.clone()]).await
        {
            let state = info.get("state").and_then(Value::as_str).unwrap_or("");
            if state == "idle" {
                if run_blocking_agent(
                    socket.clone(),
                    vec!["prompt".into(), pane_target.clone(), prompt.clone()],
                )
                .await
                .is_ok()
                {
                    return;
                }
            } else if state == "exited" {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(600)).await;
    }
}

fn collect_checkpoint_paths(params: &Value) -> Vec<String> {
    let mut paths: Vec<String> = params["paths"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    if paths.is_empty()
        && let Some(p) = params["path"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
    {
        paths.push(p.to_owned());
    }
    paths
}

fn extract_restorable_paths_from_show(show_val: &Value, paths: &mut Vec<String>) {
    if let Some(files) = show_val
        .get("since")
        .and_then(|s| s.get("files"))
        .and_then(Value::as_array)
    {
        for f in files {
            if f.get("kind").and_then(Value::as_str) == Some("excluded_now") {
                continue;
            }
            if let Some(p) = f.get("path").and_then(Value::as_str)
                && !p.trim().is_empty()
            {
                paths.push(p.to_owned());
            }
        }
    }
}

fn preferred_agent_target(params: &Value) -> String {
    if let Some(p) = params["pane"]
        .as_str()
        .map(str::trim)
        .filter(|s| s.starts_with('%'))
    {
        return p.to_owned();
    }
    params["target"]
        .as_str()
        .or_else(|| params["pane"].as_str())
        .or_else(|| params["agent_id"].as_str())
        .unwrap_or("")
        .trim()
        .to_owned()
}

fn json_id_to_string(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) if !s.trim().is_empty() => Some(s.trim().to_owned()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

fn normalize_permission_choice(raw: &str) -> &'static str {
    match raw.trim().to_ascii_lowercase().as_str() {
        "always" => "always",
        "reject" | "deny" | "cancel" | "no" => "reject",
        _ => "once",
    }
}

fn map_key_token(token: &str) -> &str {
    match token {
        "Escape" | "Esc" | "esc" => "\x1b",
        "Enter" | "Return" | "CR" => "\r",
        "Tab" => "\t",
        "Shift-Tab" | "BTab" | "BackTab" => "\x1b[Z",
        "Space" => " ",
        "Up" => "\x1b[A",
        "Down" => "\x1b[B",
        "Right" => "\x1b[C",
        "Left" => "\x1b[D",
        "Backspace" | "BSpace" => "\x7f",
        "C-c" | "Ctrl-C" => "\x03",
        "C-d" | "Ctrl-D" => "\x04",
        "C-g" | "Ctrl-G" => "\x07",
        "C-u" | "Ctrl-U" => "\x15",
        other => other,
    }
}

async fn resolve_pane_target(socket: Option<&str>, params: &Value) -> Result<String, String> {
    if let Some(p) = params["pane"]
        .as_str()
        .map(str::trim)
        .filter(|s| s.starts_with('%'))
    {
        return Ok(p.to_owned());
    }
    if let Some(t) = params["target"]
        .as_str()
        .map(str::trim)
        .filter(|s| s.starts_with('%'))
    {
        return Ok(t.to_owned());
    }
    let lookup = params["target"]
        .as_str()
        .or_else(|| params["pane"].as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "missing agent target or pane id".to_owned())?
        .to_owned();
    let val = run_blocking_agent(
        socket.map(str::to_owned),
        vec!["get".into(), lookup.clone()],
    )
    .await?;
    val.get("pane_id")
        .or_else(|| val.get("pane"))
        .and_then(Value::as_str)
        .filter(|s| s.starts_with('%'))
        .map(str::to_owned)
        .ok_or_else(|| format!("cannot resolve pane_id for target '{lookup}'"))
}

async fn resolve_live_agent_meta(
    socket: &str,
    pane_id: &str,
    params: &Value,
) -> (String, String, Option<String>, String) {
    let mut provider = params["provider"]
        .as_str()
        .map(str::trim)
        .unwrap_or("")
        .to_owned();
    let mut cwd = params["cwd"]
        .as_str()
        .map(str::trim)
        .unwrap_or("")
        .to_owned();
    let mut session_id = params["session_id"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let mut state = params["state"]
        .as_str()
        .map(str::trim)
        .unwrap_or("")
        .to_owned();

    if provider.is_empty() || state.is_empty() {
        let sock = (!socket.is_empty()).then(|| socket.to_owned());
        if let Ok(val) = run_blocking_agent(sock, vec!["get".into(), pane_id.to_owned()]).await {
            if provider.is_empty()
                && let Some(p) = val.get("provider").and_then(Value::as_str)
            {
                provider = p.to_owned();
            }
            if cwd.is_empty()
                && let Some(c) = val.get("cwd").and_then(Value::as_str)
            {
                cwd = c.to_owned();
            }
            if session_id.is_none() {
                session_id = val
                    .get("session_id")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .or_else(|| {
                        val.get("binding")
                            .and_then(|b| b.get("session_id"))
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                            .map(str::to_owned)
                    });
            }
            if let Some(s) = val.get("state").and_then(Value::as_str) {
                state = s.to_owned();
            }
        }
    }
    if state.is_empty() {
        state = "unknown".to_owned();
    }
    (provider, cwd, session_id, state)
}

async fn handle_prompt_respond(id: String, kind: &str, params: &Value) -> RpcResponse {
    let socket = resolve_socket_param(params);
    let action = params["action"].as_str().unwrap_or("").trim().to_owned();
    if action.is_empty() {
        return RpcResponse::err(id, kind, "prompt_respond_failed", "missing action");
    }

    let pane_id = match resolve_pane_target(socket.as_deref(), params).await {
        Ok(p) => p,
        Err(e) => return RpcResponse::err(id, kind, "prompt_respond_failed", e),
    };
    let socket_str = socket.clone().unwrap_or_default();
    let (provider, cwd, reported_sid, initial_state) =
        resolve_live_agent_meta(&socket_str, &pane_id, params).await;

    let mut is_api_action = false;
    let action_result: Result<Value, String> = match action.as_str() {
        "api_permission" => {
            is_api_action = true;
            let req_id = params["request_id"].as_str().unwrap_or("").trim();
            if req_id.is_empty() {
                Err("missing request_id for api_permission".into())
            } else {
                let choice =
                    normalize_permission_choice(params["decision"].as_str().unwrap_or("once"));
                let mut args = vec![
                    "answer".into(),
                    pane_id.clone(),
                    req_id.to_owned(),
                    "--choice".into(),
                    choice.to_owned(),
                ];
                if choice == "reject"
                    && let Some(msg) = params["message"]
                        .as_str()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                {
                    args.push("--message".into());
                    args.push(msg.to_owned());
                }
                run_blocking_agent(socket.clone(), args).await
            }
        }
        "api_question" => {
            is_api_action = true;
            let req_id = params["request_id"].as_str().unwrap_or("").trim();
            if req_id.is_empty() {
                Err("missing request_id for api_question".into())
            } else if params["reject"].as_bool() == Some(true) {
                run_blocking_agent(
                    socket.clone(),
                    vec![
                        "answer".into(),
                        pane_id.clone(),
                        req_id.to_owned(),
                        "--reject".into(),
                    ],
                )
                .await
            } else if let Some(multi) = params.get("multi_answers").and_then(Value::as_array) {
                let mut normalized: Vec<Vec<String>> = Vec::with_capacity(multi.len());
                for group in multi {
                    let Some(arr) = group.as_array() else {
                        return RpcResponse::err(
                            id,
                            kind,
                            "prompt_respond_failed",
                            "multi_answers must be an array of string arrays",
                        );
                    };
                    let labels: Vec<String> = arr
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                        .collect();
                    normalized.push(labels);
                }
                let encoded = serde_json::to_string(&normalized).map_err(|e| e.to_string());
                match encoded {
                    Ok(json_str) => {
                        run_blocking_agent(
                            socket.clone(),
                            vec![
                                "answer".into(),
                                pane_id.clone(),
                                req_id.to_owned(),
                                "--answers-json".into(),
                                json_str,
                            ],
                        )
                        .await
                    }
                    Err(e) => Err(e),
                }
            } else if let Some(answers) = params.get("answers").and_then(Value::as_array) {
                let mut args = vec!["answer".into(), pane_id.clone(), req_id.to_owned()];
                for a in answers {
                    if let Some(s) = a.as_str() {
                        args.push("--answer".into());
                        args.push(s.to_owned());
                    }
                }
                if args.len() == 3 {
                    Err("answers must contain at least one answer".into())
                } else {
                    run_blocking_agent(socket.clone(), args).await
                }
            } else {
                Err("api_question requires answers, multi_answers, or reject".into())
            }
        }
        "tui_select_option" => {
            let option_index = params["option_index"].as_u64().map(|n| n as usize);
            let submit = params["submit"].as_bool().unwrap_or(true);
            let caller_hotkey = params["hotkey"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            let fallback_selected = params["current_selected_index"].as_u64().unwrap_or(0) as usize;

            let fresh =
                capture_fresh_tui_prompt(&socket_str, &pane_id, &provider, &initial_state).await;

            let fresh_opt = option_index.and_then(|idx| {
                fresh.as_ref().and_then(|p| {
                    p.options
                        .iter()
                        .find(|o| o.index == idx)
                        .or_else(|| p.options.get(idx))
                })
            });

            let resolved_hotkey = fresh_opt.and_then(|o| o.hotkey.clone()).or(caller_hotkey);

            // Use direct hotkey ONLY for non-digit hotkeys (e.g., [y] Approve, [n] Deny, y\r, \x1b, \r)
            // so arrow-navigated numbered menus (1., 2., 3.) move reliably across all providers.
            let is_direct_non_digit_hotkey = resolved_hotkey
                .as_deref()
                .is_some_and(|hk| !hk.is_empty() && !hk.chars().all(|c| c.is_ascii_digit()));

            if is_direct_non_digit_hotkey {
                let hk = resolved_hotkey.as_deref().unwrap_or("\r");
                send_key_sequence_paced(socket.as_deref(), &pane_id, hk)
                    .await
                    .map(|()| json!({"mode": "tui_hotkey", "hotkey": hk}))
            } else if let Some(target_idx) = option_index {
                let current_idx = fresh
                    .as_ref()
                    .map(|p| p.selected_index)
                    .unwrap_or(fallback_selected);
                let steps: Vec<&str> = if target_idx > current_idx {
                    vec!["\x1b[B"; target_idx - current_idx]
                } else if target_idx < current_idx {
                    vec!["\x1b[A"; current_idx - target_idx]
                } else {
                    Vec::new()
                };
                async {
                    if !steps.is_empty() {
                        send_paced_keys_to_pane(socket.as_deref(), &pane_id, &steps, 45).await?;
                    }
                    if submit {
                        if !steps.is_empty() {
                            tokio::time::sleep(Duration::from_millis(75)).await;
                        }
                        send_raw_bytes_to_pane(socket.as_deref(), &pane_id, b"\r").await?;
                    }
                    Ok::<_, String>(json!({
                        "mode": "tui_arrows",
                        "from": current_idx,
                        "to": target_idx,
                        "submitted": submit,
                    }))
                }
                .await
            } else {
                Err("tui_select_option requires option_index or hotkey".into())
            }
        }
        "tui_toggle_options" => {
            let desired: HashSet<usize> = params["indices"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(Value::as_u64)
                        .map(|n| n as usize)
                        .collect()
                })
                .unwrap_or_default();
            let submit = params["submit"].as_bool().unwrap_or(true);

            let fresh =
                capture_fresh_tui_prompt(&socket_str, &pane_id, &provider, &initial_state).await;

            async {
                let mut cur = fresh.as_ref().map(|p| p.selected_index).unwrap_or(0);
                let mut toggled = Vec::new();

                if let Some(prompt) = fresh.as_ref()
                    && !prompt.options.is_empty()
                {
                    for opt in &prompt.options {
                        // Skip non-checkbox trailing submit/cancel rows when toggling.
                        if opt.checked.is_none() && is_submit_action_label(&opt.label) {
                            continue;
                        }
                        let currently_checked = opt.checked.unwrap_or(false);
                        let want_checked = desired.contains(&opt.index);
                        if currently_checked != want_checked {
                            move_cursor_paced(socket.as_deref(), &pane_id, cur, opt.index).await?;
                            cur = opt.index;
                            tokio::time::sleep(Duration::from_millis(55)).await;
                            send_raw_bytes_to_pane(socket.as_deref(), &pane_id, b" ").await?;
                            tokio::time::sleep(Duration::from_millis(55)).await;
                            toggled.push(opt.index);
                        }
                    }
                    if submit {
                        if let Some(submit_opt) = prompt
                            .options
                            .iter()
                            .rev()
                            .find(|o| o.checked.is_none() && is_submit_action_label(&o.label))
                        {
                            move_cursor_paced(socket.as_deref(), &pane_id, cur, submit_opt.index)
                                .await?;
                            tokio::time::sleep(Duration::from_millis(75)).await;
                        } else {
                            tokio::time::sleep(Duration::from_millis(80)).await;
                        }
                        send_raw_bytes_to_pane(socket.as_deref(), &pane_id, b"\r").await?;
                    }
                } else {
                    let mut sorted: Vec<usize> = desired.iter().copied().collect();
                    sorted.sort_unstable();
                    for idx in sorted {
                        move_cursor_paced(socket.as_deref(), &pane_id, cur, idx).await?;
                        cur = idx;
                        tokio::time::sleep(Duration::from_millis(55)).await;
                        send_raw_bytes_to_pane(socket.as_deref(), &pane_id, b" ").await?;
                        tokio::time::sleep(Duration::from_millis(55)).await;
                        toggled.push(idx);
                    }
                    if submit {
                        tokio::time::sleep(Duration::from_millis(80)).await;
                        send_raw_bytes_to_pane(socket.as_deref(), &pane_id, b"\r").await?;
                    }
                }
                Ok::<_, String>(json!({
                    "mode": "tui_multi_select",
                    "toggled": toggled,
                    "submitted": submit,
                }))
            }
            .await
        }
        "tui_custom_text" => {
            let text = params["text"].as_str().unwrap_or("").trim().to_owned();
            if text.is_empty() {
                Err("tui_custom_text requires non-empty text".into())
            } else {
                let submit = params["submit"].as_bool().unwrap_or(true);
                let caller_option_index = params["option_index"].as_u64().map(|n| n as usize);
                let activate_override = params["activate_with_enter"].as_bool();

                let fresh =
                    capture_fresh_tui_prompt(&socket_str, &pane_id, &provider, &initial_state)
                        .await;

                async {
                    let is_free_text_only = fresh.as_ref().is_some_and(|p| p.free_text_only);
                    if !is_free_text_only {
                        let target_opt = caller_option_index
                            .and_then(|idx| {
                                fresh
                                    .as_ref()
                                    .and_then(|p| p.options.iter().find(|o| o.index == idx))
                                    .map(|o| (o.index, o.label.clone()))
                                    .or_else(|| Some((idx, String::new())))
                            })
                            .or_else(|| {
                                fresh
                                    .as_ref()
                                    .and_then(|p| p.options.iter().find(|o| o.is_custom_input))
                                    .map(|o| (o.index, o.label.clone()))
                            });

                        if let Some((target_idx, label)) = target_opt {
                            let cur = fresh.as_ref().map(|p| p.selected_index).unwrap_or(0);
                            move_cursor_paced(socket.as_deref(), &pane_id, cur, target_idx).await?;
                            tokio::time::sleep(Duration::from_millis(80)).await;

                            let lower = label.to_ascii_lowercase();
                            let inline_typing_row = lower.contains("type something")
                                || lower.contains("tell claude")
                                || lower.contains("type to ")
                                || matches!(provider.as_str(), "claude" | "codex");
                            let should_press_enter =
                                activate_override.unwrap_or(!inline_typing_row);
                            if should_press_enter {
                                send_raw_bytes_to_pane(socket.as_deref(), &pane_id, b"\r").await?;
                                tokio::time::sleep(Duration::from_millis(140)).await;
                            }
                        }
                    }
                    send_text_and_enter_to_pane(socket.as_deref(), &pane_id, &text, submit).await?;
                    Ok::<_, String>(json!({
                        "mode": "tui_custom_text",
                        "submitted": submit,
                    }))
                }
                .await
            }
        }
        "tui_keys" => {
            let mut key_tokens: Vec<String> = Vec::new();
            if let Some(arr) = params.get("keys").and_then(Value::as_array) {
                for item in arr {
                    if let Some(s) = item.as_str()
                        && !s.is_empty()
                    {
                        key_tokens.push(s.to_owned());
                    }
                }
            } else if let Some(s) = params["key"].as_str().filter(|s| !s.is_empty()) {
                key_tokens.push(s.to_owned());
            }
            if key_tokens.is_empty() {
                Err("tui_keys requires keys array or key string".into())
            } else {
                let mapped: Vec<&str> = key_tokens.iter().map(|t| map_key_token(t)).collect();
                let delay = params["delay_ms"].as_u64().unwrap_or(45).clamp(10, 500);
                send_paced_keys_to_pane(socket.as_deref(), &pane_id, &mapped, delay)
                    .await
                    .map(|()| json!({"mode": "tui_keys", "count": mapped.len()}))
            }
        }
        other => Err(format!("unsupported prompt respond action: {other}")),
    };

    let result_val = match action_result {
        Ok(v) => v,
        Err(e) => return RpcResponse::err(id, kind, "prompt_respond_failed", e),
    };

    // Wait briefly so the provider redraws or advances to the next question step,
    // then return fresh live state + active_prompts for immediate mobile update.
    tokio::time::sleep(Duration::from_millis(if is_api_action { 120 } else { 260 })).await;

    let live_state = run_blocking_agent(socket.clone(), vec!["get".into(), pane_id.clone()])
        .await
        .ok()
        .and_then(|v| v.get("state").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or(initial_state);

    let sock_for_page = socket_str.clone();
    let pane_for_page = pane_id.clone();
    let prov_for_page = provider.clone();
    let cwd_for_page = cwd.clone();
    let sid_for_page = reported_sid.clone();
    let state_for_page = live_state.clone();
    let agent_key = format!("{socket_str}::{pane_id}");

    let fresh_prompts = tokio::task::spawn_blocking(move || {
        fetch_timeline(crate::remote::transcripts::TimelineQuery {
            server_socket: &sock_for_page,
            pane_id: &pane_for_page,
            agent_key: &agent_key,
            provider: &prov_for_page,
            cwd: &cwd_for_page,
            reported_session_id: sid_for_page.as_deref(),
            state: &state_for_page,
            limit: Some(60),
        })
        .active_prompts
    })
    .await
    .unwrap_or_default();

    RpcResponse::ok(
        id,
        kind,
        json!({
            "ok": true,
            "action": action,
            "pane": pane_id,
            "state": live_state,
            "active_prompts": fresh_prompts,
            "result": result_val,
        }),
    )
}

fn is_submit_action_label(label: &str) -> bool {
    let lower = label.trim().to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "submit" | "confirm" | "done" | "continue" | "제출" | "확인" | "완료"
    ) || lower.starts_with("submit ")
        || lower.starts_with("confirm ")
}

async fn capture_fresh_tui_prompt(
    socket: &str,
    pane_id: &str,
    provider: &str,
    state: &str,
) -> Option<InteractivePrompt> {
    let sock = socket.to_owned();
    let pane = pane_id.to_owned();
    let prov = provider.to_owned();
    let st = state.to_owned();
    tokio::task::spawn_blocking(move || {
        let screen = prompts::capture_visible_pane(&sock, &pane)?;
        prompts::parse_live_tui_screen(&prov, &st, &screen, &[])
    })
    .await
    .ok()
    .flatten()
}

async fn move_cursor_paced(
    socket: Option<&str>,
    pane_id: &str,
    from: usize,
    to: usize,
) -> Result<(), String> {
    if from == to {
        return Ok(());
    }
    let steps: Vec<&str> = if to > from {
        vec!["\x1b[B"; to - from]
    } else {
        vec!["\x1b[A"; from - to]
    };
    send_paced_keys_to_pane(socket, pane_id, &steps, 45).await
}

fn resolve_socket_or_err(socket: Option<&str>) -> Result<String, String> {
    socket
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            discover_masil_servers()
                .into_iter()
                .next()
                .map(|s| s.socket)
        })
        .ok_or_else(|| "no active masil server found".to_owned())
}

async fn send_raw_bytes_to_pane(
    socket: Option<&str>,
    pane_id: &str,
    bytes: &[u8],
) -> Result<(), String> {
    if bytes.is_empty() {
        return Ok(());
    }
    let sock = resolve_socket_or_err(socket)?;
    let masil = native_executable()?;
    for chunk in bytes.chunks(128) {
        let mut cmd = tokio::process::Command::new(&masil);
        cmd.arg("-S")
            .arg(&sock)
            .args(["send-keys", "-H", "-t", pane_id]);
        for b in chunk {
            cmd.arg(format!("{b:02x}"));
        }
        let out = cmd
            .output()
            .await
            .map_err(|e| format!("send-keys failed: {e}"))?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).trim().to_owned());
        }
    }
    Ok(())
}

async fn send_paced_keys_to_pane(
    socket: Option<&str>,
    pane_id: &str,
    steps: &[&str],
    delay_ms: u64,
) -> Result<(), String> {
    for (idx, step) in steps.iter().enumerate() {
        if idx > 0 && delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }
        send_raw_bytes_to_pane(socket, pane_id, step.as_bytes()).await?;
    }
    Ok(())
}

async fn send_key_sequence_paced(
    socket: Option<&str>,
    pane_id: &str,
    keys: &str,
) -> Result<(), String> {
    if let Some(prefix) = keys.strip_suffix('\r')
        && !prefix.is_empty()
    {
        send_raw_bytes_to_pane(socket, pane_id, prefix.as_bytes()).await?;
        tokio::time::sleep(Duration::from_millis(80)).await;
        return send_raw_bytes_to_pane(socket, pane_id, b"\r").await;
    }
    send_raw_bytes_to_pane(socket, pane_id, keys.as_bytes()).await
}

async fn send_text_and_enter_to_pane(
    socket: Option<&str>,
    pane_id: &str,
    text: &str,
    submit: bool,
) -> Result<(), String> {
    if !text.is_empty() {
        let use_bracketed_paste = text.contains('\n') || text.contains('\r') || text.len() > 200;
        if use_bracketed_paste {
            paste_text_to_pane(socket, pane_id, text).await?;
        } else {
            send_raw_bytes_to_pane(socket, pane_id, text.as_bytes()).await?;
        }
    }
    if submit {
        if !text.is_empty() {
            tokio::time::sleep(Duration::from_millis(120)).await;
        }
        send_raw_bytes_to_pane(socket, pane_id, b"\r").await?;
    }
    Ok(())
}

async fn paste_text_to_pane(socket: Option<&str>, pane_id: &str, text: &str) -> Result<(), String> {
    let sock = resolve_socket_or_err(socket)?;
    let masil = native_executable()?;
    let buf_name = format!(
        "masil-mob-{}-{}",
        std::process::id(),
        crate::remote::config::now_ms()
    );
    let mut load = tokio::process::Command::new(&masil)
        .arg("-S")
        .arg(&sock)
        .args(["load-buffer", "-b", &buf_name, "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("load-buffer spawn failed: {e}"))?;
    if let Some(mut stdin) = load.stdin.take() {
        stdin
            .write_all(text.as_bytes())
            .await
            .map_err(|e| format!("load-buffer write failed: {e}"))?;
        drop(stdin);
    }
    let load_out = load
        .wait_with_output()
        .await
        .map_err(|e| format!("load-buffer failed: {e}"))?;
    if !load_out.status.success() {
        return Err(String::from_utf8_lossy(&load_out.stderr).trim().to_owned());
    }
    let paste_out = tokio::process::Command::new(&masil)
        .arg("-S")
        .arg(&sock)
        .args([
            "paste-buffer",
            "-p",
            "-r",
            "-d",
            "-b",
            &buf_name,
            "-t",
            pane_id,
        ])
        .output()
        .await
        .map_err(|e| format!("paste-buffer failed: {e}"))?;
    if !paste_out.status.success() {
        let _ = tokio::process::Command::new(&masil)
            .arg("-S")
            .arg(&sock)
            .args(["delete-buffer", "-b", &buf_name])
            .output()
            .await;
        return Err(String::from_utf8_lossy(&paste_out.stderr).trim().to_owned());
    }
    Ok(())
}

fn build_provider_start_flags(
    provider: &str,
    model: Option<&str>,
    effort: Option<&str>,
    mode: Option<&str>,
) -> Vec<String> {
    let mut out = Vec::new();
    match provider {
        "agy" | "antigravity" => {
            if let Some(m) = model {
                out.push("--model".into());
                out.push(m.to_owned());
            }
            if let Some(e) = effort {
                out.push("--effort".into());
                out.push(e.to_owned());
            }
            if let Some(md) = mode {
                out.push("--mode".into());
                out.push(md.to_owned());
            }
        }
        "claude" => {
            if let Some(m) = model {
                out.push("--model".into());
                out.push(m.to_owned());
            }
            if let Some(e) = effort {
                out.push("--effort".into());
                out.push(e.to_owned());
            }
            if let Some(md) = mode {
                out.push("--permission-mode".into());
                out.push(md.to_owned());
            }
        }
        "codex" => {
            if let Some(m) = model {
                out.push("--model".into());
                out.push(m.to_owned());
            }
            if let Some(e) = effort {
                out.push("-c".into());
                out.push(format!("model_reasoning_effort={e}"));
            }
            if let Some(md) = mode {
                out.push("-a".into());
                out.push(md.to_owned());
            }
        }
        "opencode" | "kilo" | "gemini" => {
            if let Some(m) = model {
                out.push("--model".into());
                out.push(m.to_owned());
            }
        }
        _ => {
            if let Some(m) = model {
                out.push("--model".into());
                out.push(m.to_owned());
            }
        }
    }
    out
}

fn build_live_configure_commands(
    provider: &str,
    model: Option<&str>,
    effort: Option<&str>,
    mode: Option<&str>,
) -> Vec<String> {
    let mut cmds = Vec::new();
    match provider {
        "agy" | "antigravity" => {
            if let Some(m) = model {
                cmds.push(format!("/model {m}"));
            }
            if let Some(e) = effort {
                cmds.push(format!("/effort {e}"));
            }
            if let Some(md) = mode {
                cmds.push(format!("/mode {md}"));
            }
        }
        "claude" => {
            if let Some(m) = model {
                cmds.push(format!("/model {m}"));
            }
            if let Some(e) = effort {
                cmds.push(format!("/effort {e}"));
            }
        }
        "codex" | "opencode" | "kilo" | "gemini" => {
            if let Some(m) = model {
                cmds.push(format!("/model {m}"));
            }
        }
        _ => {
            if let Some(m) = model {
                cmds.push(format!("/model {m}"));
            }
        }
    }
    cmds
}

fn models_cache() -> &'static Mutex<HashMap<String, (Instant, Value)>> {
    static CACHE: OnceLock<Mutex<HashMap<String, (Instant, Value)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn list_provider_models_and_options(provider: &str) -> Value {
    let key = provider.to_ascii_lowercase();
    if let Ok(guard) = models_cache().lock()
        && let Some((ts, cached)) = guard.get(&key)
        && ts.elapsed() < Duration::from_secs(300)
    {
        return cached.clone();
    }

    let result = match key.as_str() {
        "agy" | "antigravity" => {
            let mut models = query_agy_models();
            if models.is_empty() {
                models = vec![
                    json!({"id": "gemini-4-neon-xhigh", "label": "Gemini 4 Neon (XHigh)"}),
                    json!({"id": "gemini-4-neon-high", "label": "Gemini 4 Neon (High)"}),
                    json!({"id": "gemini-4-carbon-high", "label": "Gemini 4 Carbon (High)"}),
                    json!({"id": "gemini-4-argon-high", "label": "Gemini 4 Argon (High)"}),
                    json!({"id": "claude-opus-5-5-high", "label": "Claude Opus 5.5 (High)"}),
                    json!({"id": "claude-sonnet-5-5-high", "label": "Claude Sonnet 5.5 (High)"}),
                ];
            }
            json!({
                "provider": "agy",
                "models": models,
                "efforts": ["low", "medium", "high", "xhigh", "max"],
                "modes": ["auto", "accept-edits", "plan"],
                "supports_live_model": true,
                "supports_live_effort": true,
                "supports_live_mode": true,
            })
        }
        "claude" => json!({
            "provider": "claude",
            "models": [
                {"id": "opus", "label": "Claude Opus (Latest)"},
                {"id": "sonnet", "label": "Claude Sonnet (Latest)"},
                {"id": "haiku", "label": "Claude Haiku (Latest)"},
                {"id": "claude-opus-5-5", "label": "Claude Opus 5.5"},
                {"id": "claude-sonnet-5-5", "label": "Claude Sonnet 5.5"},
            ],
            "efforts": ["low", "medium", "high", "max"],
            "modes": ["default", "auto", "acceptEdits", "plan", "bypassPermissions"],
            "supports_live_model": true,
            "supports_live_effort": true,
            "supports_live_mode": false,
        }),
        "codex" => json!({
            "provider": "codex",
            "models": [
                {"id": "gpt-5.4", "label": "GPT-5.4"},
                {"id": "gpt-5.3-codex", "label": "GPT-5.3 Codex"},
                {"id": "o3", "label": "o3"},
            ],
            "efforts": ["low", "medium", "high", "xhigh"],
            "modes": ["on-request", "never", "untrusted"],
            "supports_live_model": true,
            "supports_live_effort": false,
            "supports_live_mode": false,
        }),
        "opencode" => {
            let mut models = query_opencode_models();
            if models.is_empty() {
                models = vec![
                    json!({"id": "openrouter/~anthropic/claude-opus-latest", "label": "Claude Opus (OpenRouter)"}),
                    json!({"id": "openrouter/~anthropic/claude-sonnet-latest", "label": "Claude Sonnet (OpenRouter)"}),
                    json!({"id": "openrouter/~openai/gpt-terra-latest", "label": "GPT Terra (OpenRouter)"}),
                ];
            }
            json!({
                "provider": "opencode",
                "models": models,
                "efforts": [],
                "modes": [],
                "supports_live_model": true,
                "supports_live_effort": false,
                "supports_live_mode": false,
            })
        }
        "gemini" => json!({
            "provider": "gemini",
            "models": [
                {"id": "gemini-2.5-pro", "label": "Gemini 2.5 Pro"},
                {"id": "gemini-2.5-flash", "label": "Gemini 2.5 Flash"},
            ],
            "efforts": [],
            "modes": [],
            "supports_live_model": true,
            "supports_live_effort": false,
            "supports_live_mode": false,
        }),
        other => json!({
            "provider": other,
            "models": [],
            "efforts": [],
            "modes": [],
            "supports_live_model": true,
            "supports_live_effort": false,
            "supports_live_mode": false,
        }),
    };

    if let Ok(mut guard) = models_cache().lock() {
        guard.insert(key, (Instant::now(), result.clone()));
    }
    result
}

fn query_agy_models() -> Vec<Value> {
    let Ok(out) = Command::new("agy").arg("models").output() else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut models = Vec::new();
    for raw_line in text.lines() {
        // Spinner prefixes use '\r'; take the segment after the last '\r'.
        let line = raw_line.rsplit('\r').next().unwrap_or(raw_line).trim();
        if line.is_empty() || line.starts_with("Fetching") {
            continue;
        }
        let mut parts = line.splitn(2, char::is_whitespace);
        let id = parts.next().unwrap_or("").trim();
        let label = parts.next().unwrap_or("").trim();
        if id.is_empty()
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'/'))
        {
            continue;
        }
        models.push(json!({
            "id": id,
            "label": if label.is_empty() { id } else { label },
        }));
    }
    models
}

fn query_opencode_models() -> Vec<Value> {
    let Ok(out) = Command::new("opencode").arg("models").output() else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && l.contains('/'))
        .take(50)
        .map(|id| json!({ "id": id, "label": id }))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_permission_choices_and_key_tokens() {
        assert_eq!(normalize_permission_choice("once"), "once");
        assert_eq!(normalize_permission_choice("allow"), "once");
        assert_eq!(normalize_permission_choice("always"), "always");
        assert_eq!(normalize_permission_choice("reject"), "reject");
        assert_eq!(normalize_permission_choice("deny"), "reject");
        assert_eq!(normalize_permission_choice("cancel"), "reject");

        assert_eq!(map_key_token("Escape"), "\x1b");
        assert_eq!(map_key_token("Enter"), "\r");
        assert_eq!(map_key_token("Tab"), "\t");
        assert_eq!(map_key_token("Shift-Tab"), "\x1b[Z");
        assert_eq!(map_key_token("Space"), " ");
        assert_eq!(map_key_token("Up"), "\x1b[A");
        assert_eq!(map_key_token("Down"), "\x1b[B");
        assert_eq!(map_key_token("C-c"), "\x03");
        assert_eq!(map_key_token("y"), "y");
    }

    #[test]
    fn prefers_pane_id_over_agent_name() {
        assert_eq!(
            preferred_agent_target(&json!({"pane": "%4", "target": "agy-4"})),
            "%4"
        );
        assert_eq!(preferred_agent_target(&json!({"target": "agy-4"})), "agy-4");
        assert_eq!(json_id_to_string(Some(&json!(42))).as_deref(), Some("42"));
        assert_eq!(
            json_id_to_string(Some(&json!("cp-7"))).as_deref(),
            Some("cp-7")
        );
        assert!(is_submit_action_label("Submit"));
        assert!(is_submit_action_label("제출"));
        assert!(!is_submit_action_label("Option A"));
    }
}
