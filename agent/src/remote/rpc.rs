//! JSON RPC dispatcher for authenticated mobile clients. Bridges existing
//! `masil-agent` CLI/durable operations, workspace files/git diff, live
//! transcripts, checkpoints, worktrees, schedules, and external imports.

use crate::native_ui::native_executable;
use crate::remote::config::{DeviceStore, HostIdentity, RemoteConfig};
use crate::remote::discovery::{
    default_masil_socket, discover_importable_sessions, discover_masil_servers,
    list_all_host_agents, run_self_agent_json,
};
use crate::remote::files::{
    browse_host_directories, create_host_directory, git_changes_diff, git_changes_list,
    list_directory, read_file_preview,
};
use crate::remote::hooks;
use crate::remote::transcripts::{candidate_sessions_for_agent, fetch_timeline};
use masil_wire::rpc::{RpcRequest, RpcResponse};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

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
    json!({
        "live_terminal": true,
        "transcript_providers": ["claude", "codex", "agy", "opencode"],
        "desktop_size_preserved": true,
        "voice_host_stt": config.stt_command.as_ref().is_some_and(|v| !v.is_empty()),
        "relay_configured": config.relay_url.is_some(),
        "push_enabled": config.push_enabled && config.relay_url.is_some(),
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
            let target = params["target"].as_str().unwrap_or("").to_owned();
            match run_blocking_agent(socket, vec!["get".into(), target]).await {
                Ok(val) => RpcResponse::ok(id, &kind, val),
                Err(e) => RpcResponse::err(id, &kind, "agent_get_failed", e),
            }
        }
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
            if params["answers"]
                .as_bool()
                .unwrap_or(matches!(provider.as_str(), "opencode" | "kilo"))
            {
                args.push("--answers".into());
            }
            let extra = build_provider_start_flags(
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
            if !extra.is_empty() {
                args.push("--".into());
                args.extend(extra);
            }
            match run_blocking_agent(socket, args).await {
                Ok(val) => RpcResponse::ok(id, &kind, val),
                Err(e) => RpcResponse::err(id, &kind, "agent_start_failed", e),
            }
        }
        "agent.configure.request" => {
            let socket = resolve_socket_param(&params);
            let target = params["target"].as_str().unwrap_or("").to_owned();
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
                    send_human_keys_to_pane(socket.as_deref(), &target, &format!("{cmd}\r")).await
                {
                    return RpcResponse::err(id, &kind, "configure_failed", e);
                }
                applied.push(cmd.clone());
            }
            RpcResponse::ok(id, &kind, json!({"applied": applied}))
        }
        "agent.prompt.request" => {
            let socket = resolve_socket_param(&params);
            let target = params["target"].as_str().unwrap_or("").to_owned();
            let text = params["text"].as_str().unwrap_or("").to_owned();
            let allow_raw_slash =
                text.trim_start().starts_with('/') || text.trim_start().starts_with('!');
            if allow_raw_slash {
                // Human mobile control explicitly allows slash/shell commands directly into live TUI.
                match send_human_keys_to_pane(socket.as_deref(), &target, &format!("{text}\r"))
                    .await
                {
                    Ok(()) => RpcResponse::ok(
                        id,
                        &kind,
                        json!({"delivered": true, "mode": "direct_human_keys"}),
                    ),
                    Err(e) => RpcResponse::err(id, &kind, "prompt_failed", e),
                }
            } else {
                let mut args = vec!["prompt".into(), target.clone(), text.clone()];
                if let Some(run) = params["run"].as_str().filter(|s| !s.is_empty()) {
                    args.push("--run".into());
                    args.push(run.to_owned());
                }
                match run_blocking_agent(socket.clone(), args).await {
                    Ok(val) => RpcResponse::ok(id, &kind, val),
                    Err(err) if params["force_keys"].as_bool().unwrap_or(false) => {
                        match send_human_keys_to_pane(
                            socket.as_deref(),
                            &target,
                            &format!("{text}\r"),
                        )
                        .await
                        {
                            Ok(()) => RpcResponse::ok(
                                id,
                                &kind,
                                json!({"delivered": true, "mode": "fallback_keys", "note": err}),
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
            let target = params["target"].as_str().unwrap_or("").to_owned();
            match run_blocking_agent(socket.clone(), vec!["interrupt".into(), target.clone()]).await
            {
                Ok(val) => RpcResponse::ok(id, &kind, val),
                Err(_) => {
                    // Human explicit interrupt fallback sends Esc.
                    match send_human_keys_to_pane(socket.as_deref(), &target, "\x1b").await {
                        Ok(()) => RpcResponse::ok(
                            id,
                            &kind,
                            json!({"interrupted": true, "mode": "direct_esc"}),
                        ),
                        Err(e) => RpcResponse::err(id, &kind, "interrupt_failed", e),
                    }
                }
            }
        }
        "agent.approve.request" => {
            let socket = resolve_socket_param(&params);
            let target = params["target"].as_str().unwrap_or("").to_owned();
            if let Some(req_id) = params["request_id"].as_str().filter(|s| !s.is_empty()) {
                let decision = params["decision"].as_str().unwrap_or("once").to_owned();
                let args = vec!["answer".into(), target, req_id.to_owned(), decision];
                match run_blocking_agent(socket, args).await {
                    Ok(val) => RpcResponse::ok(id, &kind, val),
                    Err(e) => RpcResponse::err(id, &kind, "approve_failed", e),
                }
            } else {
                let keys = params["key_sequence"].as_str().unwrap_or("\r");
                match send_human_keys_to_pane(socket.as_deref(), &target, keys).await {
                    Ok(()) => RpcResponse::ok(
                        id,
                        &kind,
                        json!({"sent": true, "mode": "human_confirmed_keys"}),
                    ),
                    Err(e) => RpcResponse::err(id, &kind, "approve_failed", e),
                }
            }
        }
        "agent.close.request" => {
            let socket = resolve_socket_param(&params);
            let target = params["target"].as_str().unwrap_or("").to_owned();
            match run_blocking_agent(socket, vec!["close".into(), target]).await {
                Ok(val) => RpcResponse::ok(id, &kind, val),
                Err(e) => RpcResponse::err(id, &kind, "close_failed", e),
            }
        }
        "agent.queue.list.request" => {
            let socket = resolve_socket_param(&params);
            let target = params["target"].as_str().unwrap_or("").to_owned();
            match run_blocking_agent(socket, vec!["queue".into(), target]).await {
                Ok(val) => RpcResponse::ok(id, &kind, val),
                Err(e) => RpcResponse::err(id, &kind, "queue_failed", e),
            }
        }
        "agent.queue.add.request" => {
            let socket = resolve_socket_param(&params);
            let target = params["target"].as_str().unwrap_or("").to_owned();
            let text = params["text"].as_str().unwrap_or("").to_owned();
            match run_blocking_agent(socket, vec!["queue".into(), target, "add".into(), text]).await
            {
                Ok(val) => RpcResponse::ok(id, &kind, val),
                Err(e) => RpcResponse::err(id, &kind, "queue_add_failed", e),
            }
        }
        "agent.queue.send.request" => {
            let socket = resolve_socket_param(&params);
            let target = params["target"].as_str().unwrap_or("").to_owned();
            match run_blocking_agent(socket, vec!["queue".into(), target, "send".into()]).await {
                Ok(val) => RpcResponse::ok(id, &kind, val),
                Err(e) => RpcResponse::err(id, &kind, "queue_send_failed", e),
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
            let target = params["target"].as_str().unwrap_or("").to_owned();
            match run_blocking_agent(socket, vec!["checkpoint".into(), target, "list".into()]).await
            {
                Ok(val) => RpcResponse::ok(id, &kind, val),
                Err(e) => RpcResponse::err(id, &kind, "checkpoint_failed", e),
            }
        }
        "checkpoint.make.request" => {
            let socket = resolve_socket_param(&params);
            let target = params["target"].as_str().unwrap_or("").to_owned();
            match run_blocking_agent(socket, vec!["checkpoint".into(), target, "make".into()]).await
            {
                Ok(val) => RpcResponse::ok(id, &kind, val),
                Err(e) => RpcResponse::err(id, &kind, "checkpoint_failed", e),
            }
        }
        "checkpoint.restore.request" => {
            let socket = resolve_socket_param(&params);
            let target = params["target"].as_str().unwrap_or("").to_owned();
            let cp_id = params["checkpoint_id"].as_str().unwrap_or("").to_owned();
            let path = params["path"].as_str().unwrap_or("").to_owned();
            let mut args = vec!["checkpoint".into(), target, "restore".into(), cp_id];
            if !path.is_empty() {
                args.push("--path".into());
                args.push(path);
            }
            args.push("--yes".into());
            match run_blocking_agent(socket, args).await {
                Ok(val) => RpcResponse::ok(id, &kind, val),
                Err(e) => RpcResponse::err(id, &kind, "checkpoint_restore_failed", e),
            }
        }
        "worktree.list.request" => {
            match tokio::task::spawn_blocking(|| crate::worktree::listing(&[])).await {
                Ok(Ok(val)) => RpcResponse::ok(id, &kind, val),
                Ok(Err(e)) => RpcResponse::err(id, &kind, "worktree_list_failed", e),
                Err(e) => RpcResponse::err(id, &kind, "worktree_list_failed", e.to_string()),
            }
        }
        "worktree.create.request" => {
            let repo = params["repo"].as_str().unwrap_or("").to_owned();
            let branch = params["branch"].as_str().unwrap_or("").to_owned();
            let res = tokio::task::spawn_blocking(move || {
                let exe = std::env::current_exe().map_err(|e| e.to_string())?;
                let out = Command::new(exe)
                    .args(["worktree", "create", &repo, &branch])
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
                Ok(Err(e)) => RpcResponse::err(id, &kind, "worktree_create_failed", e),
                Err(e) => RpcResponse::err(id, &kind, "worktree_create_failed", e.to_string()),
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
        "schedule.remove.request" => {
            let socket = resolve_socket_param(&params);
            let name = params["name"].as_str().unwrap_or("").to_owned();
            match run_blocking_agent(socket, vec!["schedule".into(), "remove".into(), name]).await {
                Ok(val) => RpcResponse::ok(id, &kind, val),
                Err(e) => RpcResponse::err(id, &kind, "schedule_remove_failed", e),
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

async fn send_human_keys_to_pane(
    socket: Option<&str>,
    target: &str,
    raw: &str,
) -> Result<(), String> {
    let sock = socket
        .map(str::to_owned)
        .or_else(|| {
            discover_masil_servers()
                .into_iter()
                .next()
                .map(|s| s.socket)
        })
        .ok_or("no active masil server found")?;
    let masil = native_executable()?;
    let mut cmd = tokio::process::Command::new(masil);
    cmd.arg("-S")
        .arg(&sock)
        .args(["send-keys", "-H", "-t", target]);
    for b in raw.as_bytes() {
        cmd.arg(format!("{b:02x}"));
    }
    let out = cmd
        .output()
        .await
        .map_err(|e| format!("send-keys failed: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_owned());
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
