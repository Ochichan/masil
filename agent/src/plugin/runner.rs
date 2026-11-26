//! Sandboxed-scope host runner for Masil plugin handlers, KV storage,
//! persistent timeline cards, lifecycle hooks, build steps, and log tailing.

use super::manifest::{LoadedPlugin, PluginCatalog, load_catalog, resolve_entry_argv, state_root};
use super::ui_spec::{UiDocument, validate_ui_document};
use crate::api::Session;
use crate::api::scope::Scope;
use masil_wire::rpc::TimelineItem;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

const MAX_STDOUT_BYTES: usize = 512 * 1024;
const MAX_STDERR_BYTES: usize = 64 * 1024;
const MAX_STORAGE_BYTES: usize = 64 * 1024;
const LOG_ROTATE_BYTES: u64 = 256 * 1024;
const MAX_TIMELINE_CARDS_PER_AGENT: usize = 50;
const DEFAULT_HANDLER_TIMEOUT_SECS: u64 = 20;
const DEFAULT_HOOK_TIMEOUT_SECS: u64 = 5;
const BUILD_STEP_TIMEOUT_SECS: u64 = 120;

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn ensure_private_dir(dir: &Path) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
    Ok(())
}

pub fn plugin_state_dir(plugin_id: &str) -> Result<PathBuf, String> {
    let dir = state_root()?.join(plugin_id);
    ensure_private_dir(&dir)?;
    Ok(dir)
}

fn append_log(plugin_id: &str, text: &str) {
    let Ok(dir) = plugin_state_dir(plugin_id) else {
        return;
    };
    let path = dir.join("plugin.log");
    if fs::metadata(&path).is_ok_and(|m| m.len() > LOG_ROTATE_BYTES) {
        let _ = fs::rename(&path, dir.join("plugin.log.1"));
    }
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&path)
    {
        let _ = writeln!(file, "[{}] {}", now_ms(), text.trim_end());
    }
}

pub fn read_plugin_logs(plugin_id: &str, tail_lines: usize) -> Result<Vec<String>, String> {
    let path = state_root()?.join(plugin_id).join("plugin.log");
    if !path.exists() {
        return Ok(Vec::new());
    }
    let body =
        fs::read_to_string(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let lines: Vec<String> = body.lines().map(str::to_owned).collect();
    let keep = tail_lines.clamp(1, 500);
    if lines.len() <= keep {
        Ok(lines)
    } else {
        Ok(lines[lines.len() - keep..].to_vec())
    }
}

pub fn load_storage(plugin_id: &str) -> Value {
    let Ok(dir) = plugin_state_dir(plugin_id) else {
        return json!({});
    };
    let path = dir.join("storage.json");
    let Ok(body) = fs::read_to_string(&path) else {
        return json!({});
    };
    match serde_json::from_str::<Value>(&body) {
        Ok(Value::Object(map)) => Value::Object(map),
        _ => json!({}),
    }
}

pub fn apply_storage_patch(plugin_id: &str, patch: &Value) -> Result<(), String> {
    let Some(patch_obj) = patch.as_object() else {
        return Ok(());
    };
    if patch_obj.is_empty() {
        return Ok(());
    }
    let mut current = match load_storage(plugin_id) {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    for (k, v) in patch_obj {
        if v.is_null() {
            current.remove(k);
        } else {
            current.insert(k.clone(), v.clone());
        }
    }
    let encoded = serde_json::to_vec_pretty(&Value::Object(current)).map_err(|e| e.to_string())?;
    if encoded.len() > MAX_STORAGE_BYTES {
        return Err(format!(
            "plugin '{plugin_id}' storage exceeds {} KiB",
            MAX_STORAGE_BYTES / 1024
        ));
    }
    let dir = plugin_state_dir(plugin_id)?;
    let path = dir.join("storage.json");
    let tmp = dir.join(format!("storage.{}.tmp", std::process::id()));
    let mut f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|e| format!("cannot open {}: {e}", tmp.display()))?;
    f.write_all(&encoded).map_err(|e| e.to_string())?;
    let _ = f.sync_all();
    fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredTimelineCard {
    pub id: String,
    pub plugin_id: String,
    pub card_id: String,
    pub ts_ms: i64,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    pub blocks: Vec<Value>,
}

fn timeline_file_for_agent(agent_key: &str) -> Result<PathBuf, String> {
    let dir = state_root()?.join("timeline");
    ensure_private_dir(&dir)?;
    let mut hasher = Sha256::new();
    hasher.update(agent_key.as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(24);
    for b in &digest[..12] {
        use std::fmt::Write;
        let _ = write!(&mut hex, "{b:02x}");
    }
    Ok(dir.join(format!("{hex}.json")))
}

pub fn upsert_timeline_card(
    agent_key: &str,
    plugin_id: &str,
    card: &TimelineCardInput,
) -> Result<(), String> {
    // Validate blocks against Masil UI Spec v1 before storing.
    let doc_val = json!({
        "v": 1,
        "title": card.title,
        "subtitle": card.subtitle,
        "blocks": card.blocks,
    });
    if let Err(errs) = validate_ui_document(&doc_val) {
        return Err(format!("invalid timeline_card: {}", errs.join("; ")));
    }
    let path = timeline_file_for_agent(agent_key)?;
    let mut cards: Vec<StoredTimelineCard> = fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let card_id = if card.id.trim().is_empty() {
        format!("card-{}", now_ms())
    } else {
        card.id.trim().to_owned()
    };
    let full_id = format!("plugin:{plugin_id}:{card_id}");
    cards.retain(|c| c.id != full_id);
    cards.push(StoredTimelineCard {
        id: full_id,
        plugin_id: plugin_id.to_owned(),
        card_id,
        ts_ms: now_ms(),
        title: card.title.clone(),
        subtitle: card.subtitle.clone(),
        blocks: card.blocks.clone(),
    });
    if cards.len() > MAX_TIMELINE_CARDS_PER_AGENT {
        let drop_n = cards.len() - MAX_TIMELINE_CARDS_PER_AGENT;
        cards.drain(0..drop_n);
    }
    let body = serde_json::to_vec_pretty(&cards).map_err(|e| e.to_string())?;
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    let mut f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|e| e.to_string())?;
    f.write_all(&body).map_err(|e| e.to_string())?;
    fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

pub fn load_timeline_cards(agent_key: &str) -> Vec<TimelineItem> {
    let Ok(path) = timeline_file_for_agent(agent_key) else {
        return Vec::new();
    };
    let Ok(body) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    let Ok(cards) = serde_json::from_str::<Vec<StoredTimelineCard>>(&body) else {
        return Vec::new();
    };
    cards
        .into_iter()
        .enumerate()
        .map(|(idx, c)| TimelineItem::PluginCard {
            id: c.id,
            seq: 900_000 + idx as u64,
            ts_ms: c.ts_ms,
            plugin_id: c.plugin_id,
            card_id: c.card_id,
            title: c.title,
            subtitle: c.subtitle,
            blocks: c.blocks,
        })
        .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimelineCardInput {
    #[serde(default)]
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub subtitle: Option<String>,
    #[serde(default)]
    pub blocks: Vec<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginRequest {
    pub v: u32,
    pub kind: String,
    pub handler: String,
    pub plugin_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<Value>,
    #[serde(default)]
    pub form_values: Map<String, Value>,
    #[serde(default)]
    pub args: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<Value>,
    pub storage: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandOutcome {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub toast: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ui: Option<UiDocument>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub draft: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeline_card_saved: Option<bool>,
    #[serde(default)]
    pub refresh_agents: bool,
}

async fn drain_bounded(reader: &mut (impl AsyncRead + Unpin), limit: usize) -> (Vec<u8>, bool) {
    let mut kept = Vec::new();
    let mut over = false;
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => return (kept, over),
            Ok(n) => {
                let room = limit.saturating_sub(kept.len());
                kept.extend_from_slice(&buf[..n.min(room)]);
                over |= n > room;
            }
        }
    }
}

pub async fn invoke_raw(
    plugin: &LoadedPlugin,
    req: &PluginRequest,
    socket: Option<PathBuf>,
    timeout_override: Option<u64>,
) -> Result<Value, String> {
    invoke_raw_mode(plugin, req, socket, timeout_override, true).await
}

pub async fn invoke_raw_mode(
    plugin: &LoadedPlugin,
    req: &PluginRequest,
    socket: Option<PathBuf>,
    timeout_override: Option<u64>,
    persist: bool,
) -> Result<Value, String> {
    let manifest = plugin
        .manifest
        .as_ref()
        .ok_or_else(|| format!("plugin '{}' has no valid manifest", plugin.id))?;
    let argv = resolve_entry_argv(&plugin.dir, &manifest.entry)?;
    let scope = Scope::parse(&manifest.scope)?;
    let state_dir = plugin_state_dir(&plugin.id)?;
    let limit = Duration::from_secs(
        timeout_override
            .or(manifest.timeout_secs)
            .unwrap_or(DEFAULT_HANDLER_TIMEOUT_SECS)
            .clamp(1, 300),
    );

    let (ours, theirs) =
        std::os::unix::net::UnixStream::pair().map_err(|e| format!("plugin api socket: {e}"))?;
    let mut cmd = tokio::process::Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .current_dir(&plugin.dir)
        .env("MASIL_PLUGIN_ID", &plugin.id)
        .env("MASIL_PLUGIN_DIR", &plugin.dir)
        .env("MASIL_PLUGIN_STATE_DIR", &state_dir)
        .env("MASIL_API_VERSION", crate::api::VERSION.to_string())
        .env("MASIL_API_FD", "3")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);

    let fd = theirs.as_raw_fd();
    // SAFETY: only async-signal-safe calls between fork and exec; dup2 clears FD_CLOEXEC.
    unsafe {
        cmd.pre_exec(move || {
            let moved = if fd == 3 {
                libc::fcntl(3, libc::F_SETFD, 0)
            } else {
                libc::dup2(fd, 3)
            };
            if moved < 0 {
                return Err(std::io::Error::last_os_error());
            }
            for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP, libc::SIGUSR1] {
                libc::signal(sig, libc::SIG_DFL);
            }
            Ok(())
        });
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed spawning plugin '{}' ({}): {e}", plugin.id, argv[0]))?;
    drop(theirs);
    ours.set_nonblocking(true).map_err(|e| e.to_string())?;
    let ours = tokio::net::UnixStream::from_std(ours).map_err(|e| e.to_string())?;
    let (api_reader, api_writer) = ours.into_split();
    let api_session = Session::new(socket, scope);
    let api_task = tokio::spawn(crate::api::serve(api_reader, api_writer, api_session));

    let pgid = child.id().and_then(|p| i32::try_from(p).ok());
    let mut stdin = child.stdin.take();
    let mut stdout = child.stdout.take().ok_or("no stdout pipe")?;
    let mut stderr = child.stderr.take().ok_or("no stderr pipe")?;
    let stdin_bytes = serde_json::to_vec(req).map_err(|e| e.to_string())?;

    let io_future = async {
        let write_fut = async {
            if let Some(mut s) = stdin.take() {
                let _ = s.write_all(&stdin_bytes).await;
                let _ = s.write_all(b"\n").await;
                let _ = s.shutdown().await;
            }
        };
        let ((), (out_bytes, out_over), (err_bytes, _)) = tokio::join!(
            write_fut,
            drain_bounded(&mut stdout, MAX_STDOUT_BYTES),
            drain_bounded(&mut stderr, MAX_STDERR_BYTES),
        );
        let status = child.wait().await;
        (out_bytes, out_over, err_bytes, status)
    };

    let (out_bytes, out_over, err_bytes, status) =
        match tokio::time::timeout(limit, io_future).await {
            Ok(res) => res,
            Err(_) => {
                if let Some(group) = pgid {
                    crate::api::end_group(group, &mut child).await;
                }
                api_task.abort();
                let msg = format!(
                    "plugin '{}' handler '{}' timed out after {}s",
                    plugin.id,
                    req.handler,
                    limit.as_secs()
                );
                append_log(&plugin.id, &msg);
                return Err(msg);
            }
        };

    let _ = tokio::time::timeout(Duration::from_secs(1), api_task).await;

    let err_text = String::from_utf8_lossy(&err_bytes).trim().to_owned();
    if !err_text.is_empty() {
        append_log(&plugin.id, &format!("[{}] {err_text}", req.handler));
    }

    let status = status.map_err(|e| format!("wait failed: {e}"))?;
    if !status.success() {
        let detail = if err_text.is_empty() {
            format!("exited with {status}")
        } else {
            format!("exited with {status}: {err_text}")
        };
        append_log(&plugin.id, &format!("[{}] ERROR {detail}", req.handler));
        return Err(format!("plugin '{}' failed: {detail}", plugin.id));
    }
    if out_over {
        return Err(format!(
            "plugin '{}' stdout exceeded {} KiB",
            plugin.id,
            MAX_STDOUT_BYTES / 1024
        ));
    }
    let out_text = String::from_utf8_lossy(&out_bytes).trim().to_owned();
    if out_text.is_empty() {
        return Ok(json!({}));
    }
    let parsed: Value = serde_json::from_str(&out_text).map_err(|e| {
        append_log(
            &plugin.id,
            &format!("[{}] invalid JSON stdout: {e}", req.handler),
        );
        format!("plugin '{}' returned invalid JSON: {e}", plugin.id)
    })?;
    if persist
        && let Some(patch) = parsed.get("storage_patch")
        && let Err(e) = apply_storage_patch(&plugin.id, patch)
    {
        append_log(&plugin.id, &format!("storage error: {e}"));
    }
    Ok(parsed)
}

#[allow(clippy::too_many_arguments)]
pub async fn invoke_panel(
    plugin: &LoadedPlugin,
    panel_id: &str,
    agent: Option<Value>,
    action: Option<String>,
    payload: Option<Value>,
    form_values: Map<String, Value>,
    socket: Option<PathBuf>,
) -> Result<UiDocument, String> {
    invoke_panel_mode(
        plugin,
        panel_id,
        agent,
        action,
        payload,
        form_values,
        socket,
        true,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn invoke_panel_mode(
    plugin: &LoadedPlugin,
    panel_id: &str,
    agent: Option<Value>,
    action: Option<String>,
    payload: Option<Value>,
    form_values: Map<String, Value>,
    socket: Option<PathBuf>,
    persist: bool,
) -> Result<UiDocument, String> {
    let manifest = plugin
        .manifest
        .as_ref()
        .ok_or_else(|| format!("plugin '{}' has no manifest", plugin.id))?;
    let panel = manifest
        .panels
        .iter()
        .find(|p| p.id == panel_id)
        .ok_or_else(|| format!("plugin '{}' has no panel '{panel_id}'", plugin.id))?;

    let req = PluginRequest {
        v: 1,
        kind: "panel".into(),
        handler: panel.handler.clone(),
        plugin_id: plugin.id.clone(),
        action,
        payload,
        form_values,
        args: String::new(),
        event: None,
        event_data: None,
        agent,
        storage: if persist {
            load_storage(&plugin.id)
        } else {
            json!({})
        },
    };
    let raw = invoke_raw_mode(plugin, &req, socket, None, persist).await?;
    validate_ui_document(&raw).map_err(|errs| {
        format!(
            "plugin '{}' panel '{}' returned invalid UI: {}",
            plugin.id,
            panel_id,
            errs.join("; ")
        )
    })
}

pub async fn invoke_command(
    plugin: &LoadedPlugin,
    command_id: &str,
    args: String,
    agent: Option<Value>,
    socket: Option<PathBuf>,
) -> Result<CommandOutcome, String> {
    invoke_command_full(
        plugin,
        command_id,
        args,
        agent,
        None,
        None,
        Map::new(),
        socket,
        true,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn invoke_command_full(
    plugin: &LoadedPlugin,
    command_id: &str,
    args: String,
    agent: Option<Value>,
    action: Option<String>,
    payload: Option<Value>,
    form_values: Map<String, Value>,
    socket: Option<PathBuf>,
    persist: bool,
) -> Result<CommandOutcome, String> {
    let manifest = plugin
        .manifest
        .as_ref()
        .ok_or_else(|| format!("plugin '{}' has no manifest", plugin.id))?;
    let cmd = manifest
        .commands
        .iter()
        .find(|c| c.id == command_id || c.slash.trim_start_matches('/') == command_id)
        .ok_or_else(|| format!("plugin '{}' has no command '{command_id}'", plugin.id))?;

    let req = PluginRequest {
        v: 1,
        kind: "command".into(),
        handler: cmd.handler.clone(),
        plugin_id: plugin.id.clone(),
        action,
        payload,
        form_values,
        args,
        event: None,
        event_data: None,
        agent: agent.clone(),
        storage: if persist {
            load_storage(&plugin.id)
        } else {
            json!({})
        },
    };
    let raw = invoke_raw_mode(plugin, &req, socket.clone(), None, persist).await?;

    let toast = raw.get("toast").and_then(Value::as_str).map(str::to_owned);
    let draft = raw.get("draft").and_then(Value::as_str).map(str::to_owned);
    let refresh_agents = raw
        .get("refresh_agents")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let ui = match raw.get("ui") {
        Some(v) if !v.is_null() => Some(validate_ui_document(v).map_err(|errs| {
            format!(
                "plugin '{}' command '{}' returned invalid UI: {}",
                plugin.id,
                cmd.id,
                errs.join("; ")
            )
        })?),
        _ => None,
    };

    let mut timeline_card_saved = None;
    if let Some(card_val) = raw.get("timeline_card").filter(|v| !v.is_null()) {
        let card: TimelineCardInput = serde_json::from_value(card_val.clone())
            .map_err(|e| format!("invalid timeline_card: {e}"))?;
        let doc_val = json!({
            "v": 1,
            "title": card.title,
            "subtitle": card.subtitle,
            "blocks": card.blocks,
        });
        if let Err(errs) = validate_ui_document(&doc_val) {
            return Err(format!("invalid timeline_card: {}", errs.join("; ")));
        }
        if persist && let Some(key) = extract_agent_key(agent.as_ref(), socket.as_deref()) {
            upsert_timeline_card(&key, &plugin.id, &card)?;
            timeline_card_saved = Some(true);
        }
    }

    Ok(CommandOutcome {
        toast,
        ui,
        draft,
        timeline_card_saved,
        refresh_agents,
    })
}

fn extract_agent_key(agent: Option<&Value>, socket_fallback: Option<&Path>) -> Option<String> {
    let a = agent?;
    let sock = a
        .get("server_socket")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .or_else(|| socket_fallback.map(|p| p.to_string_lossy().into_owned()))
        .unwrap_or_default();
    let pane = a
        .get("pane")
        .or_else(|| a.get("pane_id"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if pane.is_empty() {
        return None;
    }
    Some(format!("{sock}::{pane}"))
}

pub async fn dispatch_hook_event(
    event: &str,
    event_data: Value,
    agent: Option<Value>,
    socket: Option<PathBuf>,
) {
    let Ok(catalog) = load_catalog() else {
        return;
    };
    dispatch_hook_event_with_catalog(&catalog, event, event_data, agent, socket).await;
}

pub async fn dispatch_hook_event_with_catalog(
    catalog: &PluginCatalog,
    event: &str,
    event_data: Value,
    agent: Option<Value>,
    socket: Option<PathBuf>,
) {
    for plugin in catalog.active_plugins() {
        let Some(manifest) = &plugin.manifest else {
            continue;
        };
        for hook in manifest.hooks.iter().filter(|h| h.event == event) {
            let req = PluginRequest {
                v: 1,
                kind: "hook".into(),
                handler: hook.handler.clone(),
                plugin_id: plugin.id.clone(),
                action: None,
                payload: None,
                form_values: Map::new(),
                args: String::new(),
                event: Some(event.to_owned()),
                event_data: Some(event_data.clone()),
                agent: agent.clone(),
                storage: load_storage(&plugin.id),
            };
            let timeout = Some(hook.timeout_secs.unwrap_or(DEFAULT_HOOK_TIMEOUT_SECS));
            match invoke_raw(plugin, &req, socket.clone(), timeout).await {
                Ok(raw) => {
                    if let Some(card_val) = raw.get("timeline_card").filter(|v| !v.is_null())
                        && let Ok(card) =
                            serde_json::from_value::<TimelineCardInput>(card_val.clone())
                        && let Some(key) = extract_agent_key(agent.as_ref(), socket.as_deref())
                    {
                        let _ = upsert_timeline_card(&key, &plugin.id, &card);
                    }
                }
                Err(e) => {
                    append_log(&plugin.id, &format!("hook '{event}' error: {e}"));
                }
            }
        }
    }
}

/// Runs any registered `prompt.before` hooks sequentially. Returns the
/// potentially transformed prompt text, refusing empty overrides or raw
/// command injection when the original text was a plain prompt.
pub async fn run_prompt_before_hooks(
    original: &str,
    agent: Option<Value>,
    socket: Option<PathBuf>,
) -> String {
    let Ok(catalog) = load_catalog() else {
        return original.to_owned();
    };
    let mut current = original.to_owned();
    let orig_is_cmd = crate::api::scope::begins_like_command(original);
    for plugin in catalog.active_plugins() {
        let Some(manifest) = &plugin.manifest else {
            continue;
        };
        for hook in manifest.hooks.iter().filter(|h| h.event == "prompt.before") {
            let req = PluginRequest {
                v: 1,
                kind: "hook".into(),
                handler: hook.handler.clone(),
                plugin_id: plugin.id.clone(),
                action: None,
                payload: None,
                form_values: Map::new(),
                args: current.clone(),
                event: Some("prompt.before".into()),
                event_data: Some(json!({ "text": current })),
                agent: agent.clone(),
                storage: load_storage(&plugin.id),
            };
            let timeout = Some(hook.timeout_secs.unwrap_or(DEFAULT_HOOK_TIMEOUT_SECS));
            if let Ok(raw) = invoke_raw(plugin, &req, socket.clone(), timeout).await
                && let Some(next) = raw.get("prompt_text").and_then(Value::as_str)
                && !next.trim().is_empty()
                && (orig_is_cmd || !crate::api::scope::begins_like_command(next))
            {
                current = next.to_owned();
            }
        }
    }
    current
}

/// Runs manifest `build` argv commands without a shell.
pub fn run_build_steps(dir: &Path, steps: &[Vec<String>]) -> Result<(), String> {
    for (idx, argv) in steps.iter().enumerate() {
        let Some((program, args)) = argv.split_first() else {
            return Err(format!("build[{idx}] is empty"));
        };
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        rt.block_on(async {
            let child = tokio::process::Command::new(program)
                .args(args)
                .current_dir(dir)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .process_group(0)
                .spawn()
                .map_err(|e| format!("build[{idx}] ({program}) failed to spawn: {e}"))?;
            let pgid = child.id().and_then(|p| i32::try_from(p).ok());
            match tokio::time::timeout(
                Duration::from_secs(BUILD_STEP_TIMEOUT_SECS),
                child.wait_with_output(),
            )
            .await
            {
                Ok(Ok(out)) if out.status.success() => Ok(()),
                Ok(Ok(out)) => Err(format!(
                    "build[{idx}] ({program}) exited with {}: {}",
                    out.status,
                    String::from_utf8_lossy(&out.stderr).trim()
                )),
                Ok(Err(e)) => Err(format!("build[{idx}] ({program}) error: {e}")),
                Err(_) => {
                    if let Some(group) = pgid {
                        // SAFETY: child leads its own process group.
                        unsafe { libc::killpg(group, libc::SIGKILL) };
                    }
                    Err(format!(
                        "build[{idx}] ({program}) timed out after {BUILD_STEP_TIMEOUT_SECS}s"
                    ))
                }
            }
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::manifest::load_plugin_dir;
    use std::collections::HashSet;

    #[tokio::test]
    async fn invokes_python_panel_persists_storage_and_timeline_card() {
        let dir = std::env::temp_dir().join(format!("masil-run-test-{}", std::process::id()));
        let state = dir.join("state");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // SAFETY: isolated test uses unique temp directory for XDG_STATE_HOME.
        unsafe { std::env::set_var("XDG_STATE_HOME", &state) };

        fs::write(
            dir.join("masil-plugin.toml"),
            r#"
schema_version = 1
id = "py-test"
name = "Py Test"
scope = "read"
entry = ["python3", "plugin.py"]

[[panels]]
id = "main"
title = "Main"
handler = "panel.main"

[[commands]]
id = "ping"
slash = "ping"
title = "Ping"
handler = "command.ping"
"#,
        )
        .unwrap();
        fs::write(
            dir.join("plugin.py"),
            r#"import json, sys
req = json.load(sys.stdin)
count = int((req.get("storage") or {}).get("count", 0)) + 1
if req.get("kind") == "command":
    print(json.dumps({
        "toast": f"pong {req.get('args', '')} #{count}".strip(),
        "storage_patch": {"count": count},
        "timeline_card": {
            "id": "pong-card",
            "title": "Pong",
            "blocks": [{"type": "text", "text": f"count={count}"}]
        }
    }))
else:
    print(json.dumps({
        "v": 1,
        "title": "Main",
        "blocks": [{"type": "metric_row", "items": [{"label": "Runs", "value": str(count)}]}],
        "storage_patch": {"count": count}
    }))
"#,
        )
        .unwrap();

        let loaded = load_plugin_dir(&dir, None, "test", &HashSet::new());
        assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);

        let doc = invoke_panel(&loaded, "main", None, None, None, Map::new(), None)
            .await
            .unwrap();
        assert_eq!(doc.blocks.len(), 1);

        let agent = json!({"server_socket": "/tmp/sock", "pane": "%9"});
        let outcome = invoke_command(&loaded, "ping", "hi".into(), Some(agent), None)
            .await
            .unwrap();
        assert_eq!(outcome.toast.as_deref(), Some("pong hi #2"));
        assert_eq!(outcome.timeline_card_saved, Some(true));

        let cards = load_timeline_cards("/tmp/sock::%9");
        assert_eq!(cards.len(), 1);

        // Dry-run panel/command (`persist = false`) must not mutate storage or timeline cards.
        let dry_doc = invoke_panel_mode(&loaded, "main", None, None, None, Map::new(), None, false)
            .await
            .unwrap();
        assert_eq!(dry_doc.blocks.len(), 1);
        let dry_cmd = invoke_command_full(
            &loaded,
            "ping",
            "dry".into(),
            Some(json!({"pane": "%10"})),
            Some("btn_click".into()),
            Some(json!({"k": 1})),
            Map::new(),
            Some(PathBuf::from("/tmp/sock2")),
            false,
        )
        .await
        .unwrap();
        assert_eq!(dry_cmd.toast.as_deref(), Some("pong dry #1"));
        assert_eq!(dry_cmd.timeline_card_saved, None);
        assert_eq!(load_timeline_cards("/tmp/sock2::%10").len(), 0);
        // Storage stayed at 2 because dry-run ran isolated without persisting.
        assert_eq!(load_storage("py-test").get("count"), Some(&json!(2)));

        // Live command with socket fallback (no server_socket in agent JSON) saves card under socket::pane.
        let live_cmd = invoke_command_full(
            &loaded,
            "ping",
            "live".into(),
            Some(json!({"pane": "%10"})),
            None,
            None,
            Map::new(),
            Some(PathBuf::from("/tmp/sock2")),
            true,
        )
        .await
        .unwrap();
        assert_eq!(live_cmd.timeline_card_saved, Some(true));
        assert_eq!(load_timeline_cards("/tmp/sock2::%10").len(), 1);

        let _ = fs::remove_dir_all(&dir);
    }
}
