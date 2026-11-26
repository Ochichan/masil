//! Single per-user `masil-agent remote` daemon managing direct LAN/Tailscale
//! WebSocket ingress, outbound E2EE relay transport, Noise NK session handling,
//! live `-C` terminal multiplexing, voice STT upload, and minimal push
//! forwarding.

use crate::remote::assets::{
    AssetUploadBuffer, MARKUP_RETENTION_SECS, cleanup_expired_markups, save_markup_asset,
};
use crate::remote::config::{
    DeviceStore, HostIdentity, RemoteConfig, ensure_private_dir, state_dir,
};
use crate::remote::discovery::{
    default_masil_socket, discover_masil_servers, list_all_host_agents, run_self_agent_json,
};
use crate::remote::rpc::{SessionAuth, dispatch_authenticated_rpc, handle_rpc};
use crate::remote::terminal_stream::TerminalMux;
use crate::remote::transcripts::{TimelinePage, TimelineQuery, fetch_timeline};
use crate::remote::tunnel::TunnelMux;
use crate::remote::voice::{AudioUploadBuffer, transcribe_audio};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use futures_util::{SinkExt, StreamExt};
use masil_wire::frames::{
    AssetDeliveryMode, AssetUploadEndPayload, AudioEndPayload, BinaryFrame, BinaryOpcode,
    TerminalResizePayload, TunnelOpenPayload, TunnelWindowPayload,
};
use masil_wire::noise::{ResponderHandshake, TransportState};
use masil_wire::record::{RecordKind, open_record, seal_record};
use masil_wire::rpc::{
    HandshakeEnvelope, LIVE_ACTIVITY_MAX_SUMMARY_CHARS, LiveActivityEventKind, LiveActivityPayload,
    PushDeviceToken, PushNotificationRequest, RelayControlMessage, RpcRequest, RpcResponse,
};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, Notify, broadcast, mpsc};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{accept_async, connect_async};

const MAX_SESSIONS: usize = 32;

#[derive(Clone)]
struct SharedDaemon {
    identity: Arc<HostIdentity>,
    config: Arc<RemoteConfig>,
    active_sessions: Arc<Mutex<HashMap<String, mpsc::Sender<SessionControl>>>>,
    connected_sessions: Arc<AtomicUsize>,
    agents_tx: broadcast::Sender<Arc<Vec<Value>>>,
    refresh_notify: Arc<Notify>,
    plugins_tx: broadcast::Sender<Arc<Value>>,
    plugins_notify: Arc<Notify>,
    notify_tx: broadcast::Sender<Arc<Value>>,
    notify_dedup: Arc<Mutex<NotificationDeduper>>,
    relay_push_tx: mpsc::Sender<PushNotificationRequest>,
}

struct SessionCountGuard(Arc<AtomicUsize>);

impl SessionCountGuard {
    fn enter(counter: Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        Self(counter)
    }
}

impl Drop for SessionCountGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

enum SessionControl {
    InboundText(String),
    InboundBinary(Vec<u8>),
    ForceClose,
}

enum OutboundWire {
    Text(String),
    Binary(Vec<u8>),
    Close,
}

pub fn run_daemon() -> Result<i32, String> {
    let config = RemoteConfig::load()?;
    let identity = HostIdentity::load_or_create()?;
    let sdir = state_dir()?;
    ensure_private_dir(&sdir)?;

    let pid_path = sdir.join("daemon.pid");
    {
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&pid_path)
            .map_err(|e| format!("cannot write daemon.pid: {e}"))?;
        let _ = writeln!(f, "{}", std::process::id());
    }

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("cannot build tokio runtime: {e}"))?;

    let result = rt.block_on(daemon_main(Arc::new(identity), Arc::new(config)));
    let _ = fs::remove_file(&pid_path);
    result
}

async fn daemon_main(
    identity: Arc<HostIdentity>,
    config: Arc<RemoteConfig>,
) -> Result<i32, String> {
    let (relay_push_tx, relay_push_rx) = mpsc::channel::<PushNotificationRequest>(64);
    let (agents_tx, _) = broadcast::channel::<Arc<Vec<Value>>>(16);
    let (plugins_tx, _) = broadcast::channel::<Arc<Value>>(16);
    let (notify_tx, _) = broadcast::channel::<Arc<Value>>(64);
    let shared = SharedDaemon {
        identity: identity.clone(),
        config: config.clone(),
        active_sessions: Arc::new(Mutex::new(HashMap::new())),
        connected_sessions: Arc::new(AtomicUsize::new(0)),
        agents_tx,
        refresh_notify: Arc::new(Notify::new()),
        plugins_tx,
        plugins_notify: Arc::new(Notify::new()),
        notify_tx,
        notify_dedup: Arc::new(Mutex::new(NotificationDeduper::default())),
        relay_push_tx,
    };

    // 0. D-01: entering `masil-agent remote` ensures default intelligence
    //    features are active on discovered local servers without blocking startup.
    tokio::task::spawn_blocking(|| {
        for server in discover_masil_servers() {
            crate::coordinator::ensure_default_intelligence_features(std::path::Path::new(
                &server.socket,
            ));
        }
    });

    // 0a. Live agent state watcher: pushes `agents.changed` within 1s to all
    //    connected mobile sessions whenever any agent's state/cwd/session flips.
    {
        let shared_watch = shared.clone();
        tokio::spawn(async move {
            run_agents_watcher(shared_watch).await;
        });
    }

    // 0b. Live plugin & theme watcher: pushes `plugins.changed` to all connected
    //     mobile sessions whenever plugins.toml or any plugin manifest/theme changes.
    {
        let shared_plugins = shared.clone();
        tokio::spawn(async move {
            run_plugins_watcher(shared_plugins).await;
        });
    }

    // 1. Direct LAN / Tailscale WebSocket listener.
    if let Some(listen) = config.listen_addr.clone().filter(|s| !s.trim().is_empty()) {
        let listener = TcpListener::bind(&listen)
            .await
            .map_err(|e| format!("remote daemon failed binding {listen}: {e}"))?;
        eprintln!(
            "masil-agent remote listening directly on {listen} (host_id={})",
            identity.host_id
        );
        let shared_direct = shared.clone();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _peer)) => {
                        let s = shared_direct.clone();
                        tokio::spawn(async move {
                            handle_direct_tcp(stream, s).await;
                        });
                    }
                    Err(e) => {
                        eprintln!("masil-agent remote accept error: {e}");
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                }
            }
        });
    }

    // 2. Outbound E2EE relay transport loop (reconnects automatically on wake / drop).
    if let Some(relay_ws_url) = config.relay_host_ws_url(&identity.host_id, &identity.host_token) {
        let shared_relay = shared.clone();
        tokio::spawn(async move {
            run_relay_loop(relay_ws_url, relay_push_rx, shared_relay).await;
        });
    }

    // 3. Background inbox watcher dispatching `inbox.event` plugin hooks and
    //    forwarding minimal push metadata over the authenticated relay socket.
    {
        let shared_push = shared.clone();
        tokio::spawn(async move {
            run_push_watcher(shared_push).await;
        });
    }

    // 4. Phase 2A (D-08): Periodic 24-hour cleanup of temporary visual markup PNGs.
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(3600));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            let _ = tokio::task::spawn_blocking(|| {
                let mut roots: Vec<std::path::PathBuf> = Vec::new();
                for agent in list_all_host_agents() {
                    if let Some(cwd) = agent.get("cwd").and_then(Value::as_str)
                        && !cwd.trim().is_empty()
                    {
                        let p = std::path::PathBuf::from(cwd);
                        if !roots.contains(&p) {
                            roots.push(p);
                        }
                    }
                }
                let _ = cleanup_expired_markups(&roots, Duration::from_secs(MARKUP_RETENTION_SECS));
            })
            .await;
        }
    });

    let _ = tokio::signal::ctrl_c().await;
    eprintln!("masil-agent remote stopping");
    Ok(0)
}

async fn handle_direct_tcp(stream: TcpStream, shared: SharedDaemon) {
    let Ok(ws) = accept_async(stream).await else {
        return;
    };
    let (mut ws_sink, mut ws_stream) = ws.split();
    let (out_tx, mut out_rx) = mpsc::channel::<OutboundWire>(128);
    let (in_tx, in_rx) = mpsc::channel::<SessionControl>(128);

    let session_key = format!("direct-{}", crate::remote::config::random_token_b64(8));
    {
        let mut guard = shared.active_sessions.lock().await;
        if guard.len() >= MAX_SESSIONS {
            let _ = ws_sink.close().await;
            return;
        }
        guard.insert(session_key.clone(), in_tx.clone());
    }

    let writer = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            let res = match msg {
                OutboundWire::Text(t) => ws_sink.send(Message::Text(t.into())).await,
                OutboundWire::Binary(b) => ws_sink.send(Message::Binary(b.into())).await,
                OutboundWire::Close => {
                    let _ = ws_sink.close().await;
                    break;
                }
            };
            if res.is_err() {
                break;
            }
        }
    });

    let processor = tokio::spawn(run_encrypted_session(in_rx, out_tx, shared.clone()));

    while let Some(Ok(msg)) = ws_stream.next().await {
        match msg {
            Message::Text(t) => {
                if in_tx
                    .send(SessionControl::InboundText(t.to_string()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Message::Binary(b) => {
                if in_tx
                    .send(SessionControl::InboundBinary(b.to_vec()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    let _ = in_tx.try_send(SessionControl::ForceClose);
    let _ = processor.await;
    writer.abort();
    shared.active_sessions.lock().await.remove(&session_key);
}

async fn run_relay_loop(
    url: String,
    mut push_rx: mpsc::Receiver<PushNotificationRequest>,
    shared: SharedDaemon,
) {
    let mut backoff = Duration::from_secs(1);
    loop {
        match connect_async(&url).await {
            Ok((ws, _)) => {
                backoff = Duration::from_secs(1);
                eprintln!("masil-agent remote connected to relay");
                let (mut ws_sink, mut ws_stream) = ws.split();
                let (relay_out_tx, mut relay_out_rx) = mpsc::channel::<String>(256);
                let mut sessions: HashMap<String, mpsc::Sender<SessionControl>> = HashMap::new();

                let writer = tokio::spawn(async move {
                    while let Some(line) = relay_out_rx.recv().await {
                        if ws_sink.send(Message::Text(line.into())).await.is_err() {
                            break;
                        }
                    }
                    let _ = ws_sink.close().await;
                });

                loop {
                    tokio::select! {
                        maybe_push = push_rx.recv() => {
                            if let Some(request) = maybe_push
                                && let Ok(encoded) = serde_json::to_string(&RelayControlMessage::Push { request: Box::new(request) })
                            {
                                let _ = relay_out_tx.try_send(encoded);
                            }
                        }
                        maybe_msg = ws_stream.next() => {
                            let Some(Ok(msg)) = maybe_msg else { break; };
                            let Message::Text(text) = msg else {
                                if matches!(msg, Message::Close(_)) {
                                    break;
                                }
                                continue;
                            };
                            let Ok(ctrl) = serde_json::from_str::<RelayControlMessage>(&text) else {
                                continue;
                            };
                            match ctrl {
                                RelayControlMessage::SessionOpen { session_id } => {
                                    if sessions.len() >= MAX_SESSIONS {
                                        continue;
                                    }
                                    let (in_tx, in_rx) = mpsc::channel::<SessionControl>(128);
                                    let (out_tx, mut out_rx) = mpsc::channel::<OutboundWire>(128);
                                    sessions.insert(session_id.clone(), in_tx);

                                    let sid_fwd = session_id.clone();
                                    let relay_tx_fwd = relay_out_tx.clone();
                                    tokio::spawn(async move {
                                        while let Some(wire) = out_rx.recv().await {
                                            let ctrl_msg = match wire {
                                                OutboundWire::Text(t) => RelayControlMessage::Frame {
                                                    session_id: sid_fwd.clone(),
                                                    kind: "text".into(),
                                                    data: t,
                                                },
                                                OutboundWire::Binary(b) => RelayControlMessage::Frame {
                                                    session_id: sid_fwd.clone(),
                                                    kind: "binary".into(),
                                                    data: STANDARD.encode(b),
                                                },
                                                OutboundWire::Close => RelayControlMessage::SessionClose {
                                                    session_id: sid_fwd.clone(),
                                                    reason: Some("session_closed".into()),
                                                },
                                            };
                                            if let Ok(encoded) = serde_json::to_string(&ctrl_msg)
                                                && relay_tx_fwd.send(encoded).await.is_err()
                                            {
                                                break;
                                            }
                                            if matches!(ctrl_msg, RelayControlMessage::SessionClose { .. })
                                            {
                                                break;
                                            }
                                        }
                                    });

                                    tokio::spawn(run_encrypted_session(in_rx, out_tx, shared.clone()));
                                }
                                RelayControlMessage::Frame {
                                    session_id,
                                    kind,
                                    data,
                                } => {
                                    if let Some(tx) = sessions.get(&session_id) {
                                        let item = if kind == "binary" {
                                            match STANDARD.decode(data.as_bytes()) {
                                                Ok(b) => SessionControl::InboundBinary(b),
                                                Err(_) => continue,
                                            }
                                        } else {
                                            SessionControl::InboundText(data)
                                        };
                                        let _ = tx.try_send(item);
                                    }
                                }
                                RelayControlMessage::SessionClose { session_id, .. } => {
                                    if let Some(tx) = sessions.remove(&session_id) {
                                        let _ = tx.try_send(SessionControl::ForceClose);
                                    }
                                }
                                RelayControlMessage::Ping { ts } => {
                                    if let Ok(pong) =
                                        serde_json::to_string(&RelayControlMessage::Pong { ts })
                                    {
                                        let _ = relay_out_tx.try_send(pong);
                                    }
                                }
                                RelayControlMessage::Pong { .. } | RelayControlMessage::Push { .. } => {}
                            }
                        }
                    }
                }

                for (_, tx) in sessions.drain() {
                    let _ = tx.try_send(SessionControl::ForceClose);
                }
                writer.abort();
            }
            Err(e) => {
                eprintln!("masil-agent remote relay reconnect in {:?}: {e}", backoff);
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

async fn run_encrypted_session(
    mut in_rx: mpsc::Receiver<SessionControl>,
    out_tx: mpsc::Sender<OutboundWire>,
    shared: SharedDaemon,
) {
    let _session_guard = SessionCountGuard::enter(shared.connected_sessions.clone());
    // 1. Wait for Noise NK E2eeHello handshake.
    let transport: Option<TransportState> = match in_rx.recv().await {
        Some(SessionControl::InboundText(raw)) => {
            let Ok(HandshakeEnvelope::E2eeHello { v: 1, message }) = serde_json::from_str(&raw)
            else {
                let _ = out_tx.send(OutboundWire::Close).await;
                return;
            };
            let Ok(msg1) = STANDARD.decode(message.as_bytes()) else {
                let _ = out_tx.send(OutboundWire::Close).await;
                return;
            };
            match ResponderHandshake::accept(&shared.identity.keypair, &msg1, true) {
                Ok((tx_state, msg2)) => {
                    let ready = serde_json::to_string(&HandshakeEnvelope::E2eeReady {
                        v: 1,
                        message: STANDARD.encode(msg2),
                    })
                    .unwrap_or_default();
                    if out_tx.send(OutboundWire::Text(ready)).await.is_err() {
                        return;
                    }
                    Some(tx_state)
                }
                Err(_) => {
                    let _ = out_tx.send(OutboundWire::Close).await;
                    return;
                }
            }
        }
        Some(SessionControl::InboundBinary(_)) => {
            let _ = out_tx.send(OutboundWire::Close).await;
            return;
        }
        Some(SessionControl::ForceClose) | None => return,
    };

    let Some(tx_state) = transport else {
        return;
    };
    let transport = Arc::new(Mutex::new(tx_state));
    let mut auth: Option<SessionAuth> = None;
    let mut terminal_mux = TerminalMux::new();
    let tunnel_mux = TunnelMux::new(shared.config.tunnel.clone());
    let mut audio_buffers: HashMap<u16, AudioUploadBuffer> = HashMap::new();
    let mut asset_buffers: HashMap<u16, AssetUploadBuffer> = HashMap::new();
    let mut timeline_subs: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();

    // Forwarder for binary terminal output frames emitted asynchronously by `-C`.
    // Keep the Noise transport lock across `out_bin_fwd.send` so ciphertexts
    // enter the WebSocket queue in strict monotonic nonce order.
    let (bin_tx, mut bin_rx) = mpsc::channel::<BinaryFrame>(128);
    let tx_bin_fwd = transport.clone();
    let out_bin_fwd = out_tx.clone();
    let bin_forwarder = tokio::spawn(async move {
        while let Some(frame) = bin_rx.recv().await {
            let encoded = frame.encode();
            let mut guard = tx_bin_fwd.lock().await;
            let use_bin = guard.params.binary_ciphertext();
            let sealed = seal_record(&mut guard, RecordKind::Binary, &encoded);
            let sent = match sealed {
                Ok(wire) if use_bin => out_bin_fwd.send(OutboundWire::Binary(wire)).await.is_ok(),
                Ok(wire) => out_bin_fwd
                    .send(OutboundWire::Text(STANDARD.encode(wire)))
                    .await
                    .is_ok(),
                Err(_) => false,
            };
            drop(guard);
            if !sent {
                break;
            }
        }
    });

    let mut agents_rx = shared.agents_tx.subscribe();
    let mut plugins_rx = shared.plugins_tx.subscribe();
    let mut notify_rx = shared.notify_tx.subscribe();
    let mut revoke_tick = tokio::time::interval(Duration::from_secs(2));
    revoke_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        let ctrl = tokio::select! {
            maybe = in_rx.recv() => match maybe {
                Some(c) => c,
                None => break,
            },
            pushed = agents_rx.recv(), if auth.is_some() => {
                if let Ok(agents) = pushed {
                    let evt = json!({
                        "v": 1,
                        "event": "agents.changed",
                        "agents": agents.as_ref(),
                    });
                    if send_encrypted_json(&transport, &out_tx, &evt).await.is_err() {
                        break;
                    }
                }
                continue;
            }
            pushed_plugins = plugins_rx.recv(), if auth.is_some() => {
                if let Ok(catalog) = pushed_plugins {
                    let evt = json!({
                        "v": 1,
                        "event": "plugins.changed",
                        "catalog": catalog.as_ref(),
                    });
                    if send_encrypted_json(&transport, &out_tx, &evt).await.is_err() {
                        break;
                    }
                }
                continue;
            }
            pushed_notify = notify_rx.recv(), if auth.is_some() => {
                if let Ok(evt) = pushed_notify
                    && send_encrypted_json(&transport, &out_tx, evt.as_ref()).await.is_err()
                {
                    break;
                }
                continue;
            }
            _ = revoke_tick.tick() => {
                if let Some(ref sess) = auth {
                    let still_valid = DeviceStore::load()
                        .ok()
                        .is_some_and(|d| d.devices.iter().any(|dev| dev.device_id == sess.device_id));
                    if !still_valid {
                        break;
                    }
                }
                continue;
            }
        };
        let wire_bytes = match ctrl {
            SessionControl::InboundBinary(b) => b,
            SessionControl::InboundText(t) => match STANDARD.decode(t.as_bytes()) {
                Ok(b) => b,
                Err(_) => break,
            },
            SessionControl::ForceClose => break,
        };

        let opened = {
            let mut guard = transport.lock().await;
            open_record(&mut guard, &wire_bytes)
        };
        let Ok((kind, payload)) = opened else {
            // Replay, tamper, or framing violation terminates channel immediately.
            break;
        };

        // Re-verify authenticated device was not revoked.
        if let Some(ref sess) = auth {
            let still_valid = DeviceStore::load()
                .ok()
                .is_some_and(|d| d.devices.iter().any(|dev| dev.device_id == sess.device_id));
            if !still_valid {
                break;
            }
        }

        match kind {
            RecordKind::Text => {
                let Ok(val) = serde_json::from_slice::<Value>(&payload) else {
                    continue;
                };
                if val.get("type").and_then(Value::as_str) == Some("ping") {
                    let pong = json!({"v": 1, "type": "pong", "ts": val.get("ts").cloned().unwrap_or(json!(0))});
                    let _ = send_encrypted_json(&transport, &out_tx, &pong).await;
                    continue;
                }
                let Ok(req) = serde_json::from_value::<RpcRequest>(val) else {
                    continue;
                };
                match req.kind.as_str() {
                    "auth.pair.request" | "auth.hello.request" => {
                        let resp =
                            handle_rpc(req, &mut auth, &shared.identity, &shared.config).await;
                        if auth.is_some() {
                            shared.refresh_notify.notify_one();
                        }
                        if let Ok(v) = serde_json::to_value(&resp)
                            && send_encrypted_json(&transport, &out_tx, &v).await.is_err()
                        {
                            break;
                        }
                    }
                    "terminal.attach.request" => {
                        let resp = if auth.is_none() {
                            RpcResponse::err(
                                req.id,
                                &req.kind,
                                "unauthorized",
                                "authenticate first",
                            )
                        } else {
                            let channel_id = req.params["channel_id"].as_u64().unwrap_or(1) as u16;
                            let socket = req.params["server_socket"]
                                .as_str()
                                .map(str::to_owned)
                                .or_else(|| {
                                    discover_masil_servers()
                                        .into_iter()
                                        .next()
                                        .map(|s| s.socket)
                                })
                                .unwrap_or_default();
                            let pane = req.params["pane"].as_str().unwrap_or("").to_owned();
                            match terminal_mux
                                .attach(channel_id, socket, pane, bin_tx.clone())
                                .await
                            {
                                Ok(meta) => RpcResponse::ok(
                                    req.id,
                                    &req.kind,
                                    serde_json::to_value(meta).unwrap_or(Value::Null),
                                ),
                                Err(e) => RpcResponse::err(req.id, &req.kind, "attach_failed", e),
                            }
                        };
                        if let Ok(v) = serde_json::to_value(&resp)
                            && send_encrypted_json(&transport, &out_tx, &v).await.is_err()
                        {
                            break;
                        }
                    }
                    "terminal.detach.request" => {
                        let channel_id = req.params["channel_id"].as_u64().unwrap_or(1) as u16;
                        terminal_mux.detach(channel_id).await;
                        let resp = RpcResponse::ok(req.id, &req.kind, json!({"detached": true}));
                        if let Ok(v) = serde_json::to_value(&resp)
                            && send_encrypted_json(&transport, &out_tx, &v).await.is_err()
                        {
                            break;
                        }
                    }
                    "agent.timeline.subscribe.request" => {
                        let resp = if auth.is_none() {
                            RpcResponse::err(
                                req.id,
                                &req.kind,
                                "unauthorized",
                                "authenticate first",
                            )
                        } else {
                            let socket = req.params["server_socket"]
                                .as_str()
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                                .map(str::to_owned)
                                .or_else(|| {
                                    default_masil_socket()
                                        .ok()
                                        .map(|p| p.to_string_lossy().into_owned())
                                })
                                .unwrap_or_default();
                            let pane = req.params["pane"].as_str().unwrap_or("").to_owned();
                            let provider = req.params["provider"].as_str().unwrap_or("").to_owned();
                            let cwd = req.params["cwd"].as_str().unwrap_or("").to_owned();
                            let reported = req.params["session_id"].as_str().map(str::to_owned);
                            let state =
                                req.params["state"].as_str().unwrap_or("unknown").to_owned();
                            let limit = req.params["limit"].as_u64().map(|n| n as usize);
                            let remote_id = req.params["remote_id"]
                                .as_str()
                                .filter(|s| !s.is_empty())
                                .map(str::to_owned)
                                .unwrap_or_else(|| format!("default::{pane}"));
                            let sub_id = req.params["sub_id"]
                                .as_str()
                                .filter(|s| !s.is_empty())
                                .map(str::to_owned)
                                .unwrap_or_else(|| format!("tl::{remote_id}"));
                            let agent_key = format!("{socket}::{pane}");
                            if let Some(prev) = timeline_subs.remove(&sub_id) {
                                prev.abort();
                            }
                            if timeline_subs.len() >= 4
                                && let Some(evict_key) = timeline_subs.keys().next().cloned()
                                && let Some(evicted) = timeline_subs.remove(&evict_key)
                            {
                                evicted.abort();
                            }
                            let init_socket = socket.clone();
                            let init_pane = pane.clone();
                            let init_key = agent_key.clone();
                            let init_provider = provider.clone();
                            let init_cwd = cwd.clone();
                            let init_reported = reported.clone();
                            let init_state = state.clone();
                            let initial_page = tokio::task::spawn_blocking(move || {
                                fetch_timeline(TimelineQuery {
                                    server_socket: &init_socket,
                                    pane_id: &init_pane,
                                    agent_key: &init_key,
                                    provider: &init_provider,
                                    cwd: &init_cwd,
                                    reported_session_id: init_reported.as_deref(),
                                    state: &init_state,
                                    limit,
                                })
                            })
                            .await;
                            match initial_page {
                                Ok(page) => {
                                    let sub_handle = tokio::spawn(run_timeline_subscription(
                                        TimelineSubParams {
                                            sub_id: sub_id.clone(),
                                            remote_id,
                                            socket,
                                            pane,
                                            agent_key,
                                            provider,
                                            cwd,
                                            reported_session_id: reported,
                                            state,
                                            limit,
                                        },
                                        page.clone(),
                                        transport.clone(),
                                        out_tx.clone(),
                                        shared.clone(),
                                    ));
                                    timeline_subs.insert(sub_id.clone(), sub_handle);
                                    RpcResponse::ok(
                                        req.id,
                                        &req.kind,
                                        json!({
                                            "sub_id": sub_id,
                                            "subscribed": true,
                                            "page": page,
                                        }),
                                    )
                                }
                                Err(e) => RpcResponse::err(
                                    req.id,
                                    &req.kind,
                                    "timeline_subscribe_failed",
                                    e.to_string(),
                                ),
                            }
                        };
                        if let Ok(v) = serde_json::to_value(&resp)
                            && send_encrypted_json(&transport, &out_tx, &v).await.is_err()
                        {
                            break;
                        }
                    }
                    "agent.timeline.unsubscribe.request" => {
                        if let Some(sub_id) =
                            req.params["sub_id"].as_str().filter(|s| !s.is_empty())
                        {
                            if let Some(handle) = timeline_subs.remove(sub_id) {
                                handle.abort();
                            }
                        } else {
                            for (_, handle) in timeline_subs.drain() {
                                handle.abort();
                            }
                        }
                        let resp =
                            RpcResponse::ok(req.id, &req.kind, json!({"unsubscribed": true}));
                        if let Ok(v) = serde_json::to_value(&resp)
                            && send_encrypted_json(&transport, &out_tx, &v).await.is_err()
                        {
                            break;
                        }
                    }
                    _ => {
                        let Some(session) = auth.clone() else {
                            let resp = RpcResponse::err(
                                req.id,
                                &req.kind,
                                "unauthorized",
                                "authenticate with auth.hello.request or auth.pair.request first",
                            );
                            if let Ok(v) = serde_json::to_value(&resp)
                                && send_encrypted_json(&transport, &out_tx, &v).await.is_err()
                            {
                                break;
                            }
                            continue;
                        };
                        // Dispatch authenticated RPCs concurrently so heavy timeline or
                        // model queries never head-of-line block live state updates or prompts.
                        let shared_rpc = shared.clone();
                        let tx_rpc = transport.clone();
                        let out_rpc = out_tx.clone();
                        tokio::spawn(async move {
                            let mutates_state = matches!(
                                req.kind.as_str(),
                                "agent.start.request"
                                    | "agent.close.request"
                                    | "agent.prompt.request"
                                    | "agent.prompt.respond.request"
                                    | "agent.interrupt.request"
                                    | "agent.approve.request"
                                    | "agent.configure.request"
                                    | "agent.resume.request"
                                    | "agent.queue.add.request"
                                    | "agent.queue.send.request"
                                    | "agent.queue.remove.request"
                                    | "plugin.creator.start.request"
                                    | "focus.set.request"
                                    | "prompt.batch_approve.request"
                                    | "policy.upsert.request"
                                    | "policy.remove.request"
                                    | "policy.revoke.request"
                                    | "arena.start.request"
                                    | "arena.promote.request"
                                    | "dvr.restore.request"
                                    | "dvr.fork.request"
                                    | "dvr.pin.request"
                            );
                            let mutates_plugins = matches!(
                                req.kind.as_str(),
                                "plugin.toggle.request"
                                    | "plugin.reload.request"
                                    | "plugin.theme.set.request"
                                    | "plugin.skill.set.request"
                            );
                            let resp = dispatch_authenticated_rpc(
                                req,
                                &session,
                                &shared_rpc.identity,
                                &shared_rpc.config,
                            )
                            .await;
                            if let Ok(v) = serde_json::to_value(&resp) {
                                let _ = send_encrypted_json(&tx_rpc, &out_rpc, &v).await;
                            }
                            if mutates_plugins {
                                shared_rpc.plugins_notify.notify_one();
                            }
                            if mutates_state {
                                shared_rpc.refresh_notify.notify_one();
                                let notify_later = shared_rpc.refresh_notify.clone();
                                tokio::spawn(async move {
                                    tokio::time::sleep(Duration::from_millis(700)).await;
                                    notify_later.notify_one();
                                });
                            }
                        });
                    }
                }
            }
            RecordKind::Binary => {
                if auth.is_none() {
                    continue;
                }
                let Ok(frame) = BinaryFrame::decode(&payload) else {
                    continue;
                };
                match frame.opcode {
                    BinaryOpcode::TerminalInput => {
                        let _ = terminal_mux
                            .handle_input(frame.channel_id, &frame.payload)
                            .await;
                    }
                    BinaryOpcode::TerminalResize => {
                        if let Ok(req) =
                            serde_json::from_slice::<TerminalResizePayload>(&frame.payload)
                        {
                            let _ = terminal_mux.handle_resize(frame.channel_id, &req).await;
                        }
                    }
                    BinaryOpcode::AudioChunk => {
                        let buf = audio_buffers
                            .entry(frame.channel_id)
                            .or_insert_with(|| AudioUploadBuffer::new(frame.channel_id));
                        if buf.push(&frame.payload).is_err() {
                            audio_buffers.remove(&frame.channel_id);
                        }
                    }
                    BinaryOpcode::AudioEnd => {
                        if let Some(buf) = audio_buffers.remove(&frame.channel_id) {
                            let meta = serde_json::from_slice::<AudioEndPayload>(&frame.payload)
                                .unwrap_or(AudioEndPayload {
                                    mime: "audio/wav".into(),
                                    target_agent: None,
                                    mode: "composer".into(),
                                });
                            let audio_bytes = buf.into_bytes();
                            let event =
                                match transcribe_audio(&shared.config, &audio_bytes, &meta.mime)
                                    .await
                                {
                                    Ok(text) => json!({
                                        "v": 1,
                                        "event": "voice.transcription",
                                        "channel_id": frame.channel_id,
                                        "ok": true,
                                        "text": text,
                                        "mode": meta.mode,
                                        "target_agent": meta.target_agent,
                                    }),
                                    Err(err) => json!({
                                        "v": 1,
                                        "event": "voice.transcription",
                                        "channel_id": frame.channel_id,
                                        "ok": false,
                                        "error": err,
                                    }),
                                };
                            let _ = send_encrypted_json(&transport, &out_tx, &event).await;
                        }
                    }
                    BinaryOpcode::TunnelOpen => {
                        if let Ok(open_req) =
                            serde_json::from_slice::<TunnelOpenPayload>(&frame.payload)
                        {
                            tunnel_mux
                                .handle_open(frame.channel_id, open_req, bin_tx.clone())
                                .await;
                        }
                    }
                    BinaryOpcode::TunnelData => {
                        tunnel_mux
                            .handle_data(frame.channel_id, frame.payload)
                            .await;
                    }
                    BinaryOpcode::TunnelClose => {
                        tunnel_mux.handle_close(frame.channel_id).await;
                    }
                    BinaryOpcode::TunnelWindow => {
                        if let Ok(win) = TunnelWindowPayload::decode(&frame.payload) {
                            tunnel_mux.handle_window(frame.channel_id, win.credit).await;
                        }
                    }
                    BinaryOpcode::AssetUploadChunk => {
                        let buf = asset_buffers
                            .entry(frame.channel_id)
                            .or_insert_with(AssetUploadBuffer::new);
                        if let Err(err) = buf.push(&frame.payload) {
                            asset_buffers.remove(&frame.channel_id);
                            let evt = json!({
                                "v": 1,
                                "event": "asset.upload.result",
                                "upload_id": frame.channel_id,
                                "ok": false,
                                "error": err,
                            });
                            let _ = send_encrypted_json(&transport, &out_tx, &evt).await;
                        }
                    }
                    BinaryOpcode::AssetUploadEnd => {
                        let upload_id = frame.channel_id;
                        let buf = asset_buffers.remove(&upload_id);
                        let end_parsed =
                            serde_json::from_slice::<AssetUploadEndPayload>(&frame.payload);
                        let evt = match (buf, end_parsed) {
                            (Some(buffer), Ok(end_meta)) => {
                                let raw_bytes = buffer.into_bytes();
                                let tx_shared = shared.clone();
                                handle_asset_upload_end(upload_id, raw_bytes, end_meta, &tx_shared)
                                    .await
                            }
                            (None, _) => json!({
                                "v": 1,
                                "event": "asset.upload.result",
                                "upload_id": upload_id,
                                "ok": false,
                                "error": "no chunks received for upload_id",
                            }),
                            (_, Err(e)) => json!({
                                "v": 1,
                                "event": "asset.upload.result",
                                "upload_id": upload_id,
                                "ok": false,
                                "error": format!("invalid asset upload metadata: {e}"),
                            }),
                        };
                        let _ = send_encrypted_json(&transport, &out_tx, &evt).await;
                    }
                    BinaryOpcode::TerminalOutput
                    | BinaryOpcode::TerminalSnapshot
                    | BinaryOpcode::TunnelOpenAck => {}
                }
            }
        }
    }

    for (_, handle) in timeline_subs.drain() {
        handle.abort();
    }
    tunnel_mux.close_all().await;
    terminal_mux.close_all().await;
    bin_forwarder.abort();
    let _ = out_tx.send(OutboundWire::Close).await;
}

struct TimelineSubParams {
    sub_id: String,
    remote_id: String,
    socket: String,
    pane: String,
    agent_key: String,
    provider: String,
    cwd: String,
    reported_session_id: Option<String>,
    state: String,
    limit: Option<usize>,
}

/// Computes a minimal `timeline.delta` event between `prev` and `next` timeline
/// pages. Returns `None` if both pages are identical.
fn build_timeline_delta_event(
    sub_id: &str,
    remote_id: &str,
    agent_key: &str,
    seq: u64,
    prev: &TimelinePage,
    next: &TimelinePage,
) -> Option<Value> {
    if prev == next {
        return None;
    }
    // Find longest unchanged prefix of timeline items so streaming responses
    // only ship newly added or updated tail items rather than the full history.
    let mut common = 0usize;
    let max_common = prev.items.len().min(next.items.len());
    while common < max_common && prev.items[common] == next.items[common] {
        common += 1;
    }
    let can_patch = prev.session_id == next.session_id
        && prev.source_label == next.source_label
        && common > 0
        && next.items.len() >= common;
    if can_patch {
        let delta_items = &next.items[common..];
        Some(json!({
            "v": 1,
            "sub": sub_id,
            "seq": seq,
            "event": "timeline.delta",
            "remote_id": remote_id,
            "agent_key": agent_key,
            "mode": "patch",
            "from_index": common,
            "items": delta_items,
            "binding_confidence": next.binding_confidence,
            "session_id": next.session_id,
            "source_label": next.source_label,
            "detected_model": next.detected_model,
            "has_more": next.has_more,
            "active_prompts": next.active_prompts,
        }))
    } else {
        Some(json!({
            "v": 1,
            "sub": sub_id,
            "seq": seq,
            "event": "timeline.delta",
            "remote_id": remote_id,
            "agent_key": agent_key,
            "mode": "replace",
            "page": next,
        }))
    }
}

async fn run_timeline_subscription(
    mut params: TimelineSubParams,
    mut prev: TimelinePage,
    transport: Arc<Mutex<TransportState>>,
    out_tx: mpsc::Sender<OutboundWire>,
    shared: SharedDaemon,
) {
    let mut seq: u64 = 0;
    let mut agents_rx = shared.agents_tx.subscribe();
    loop {
        let poll_ms = if matches!(params.state.as_str(), "working" | "blocked" | "retrying")
            || !prev.active_prompts.is_empty()
        {
            500
        } else {
            1500
        };
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(poll_ms)) => {}
            _ = shared.refresh_notify.notified() => {}
            pushed = agents_rx.recv() => {
                if let Ok(agents) = pushed
                    && let Some(found) = agents.iter().find(|a| {
                        a.get("remote_id").and_then(Value::as_str) == Some(params.remote_id.as_str())
                            || (a.get("pane").and_then(Value::as_str) == Some(params.pane.as_str())
                                && a.get("server_socket").and_then(Value::as_str)
                                    == Some(params.socket.as_str()))
                    })
                {
                    if let Some(s) = found.get("state").and_then(Value::as_str) {
                        params.state = s.to_owned();
                    }
                    if let Some(c) = found.get("cwd").and_then(Value::as_str) {
                        params.cwd = c.to_owned();
                    }
                    params.reported_session_id = found
                        .get("session_id")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                }
            }
        }
        let socket = params.socket.clone();
        let pane = params.pane.clone();
        let agent_key = params.agent_key.clone();
        let provider = params.provider.clone();
        let cwd = params.cwd.clone();
        let reported = params.reported_session_id.clone();
        let state = params.state.clone();
        let limit = params.limit;
        let Ok(next) = tokio::task::spawn_blocking(move || {
            fetch_timeline(TimelineQuery {
                server_socket: &socket,
                pane_id: &pane,
                agent_key: &agent_key,
                provider: &provider,
                cwd: &cwd,
                reported_session_id: reported.as_deref(),
                state: &state,
                limit,
            })
        })
        .await
        else {
            continue;
        };
        if let Some(delta) = build_timeline_delta_event(
            &params.sub_id,
            &params.remote_id,
            &params.agent_key,
            seq + 1,
            &prev,
            &next,
        ) {
            seq += 1;
            prev = next;
            if send_encrypted_json(&transport, &out_tx, &delta)
                .await
                .is_err()
            {
                break;
            }
        }
    }
}

async fn send_encrypted_json(
    transport: &Arc<Mutex<TransportState>>,
    out_tx: &mpsc::Sender<OutboundWire>,
    value: &Value,
) -> Result<(), String> {
    let plain = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    // Hold the Noise transport mutex across `out_tx.send` so concurrent tasks
    // enqueue ciphertexts in strictly monotonic nonce order.
    let mut guard = transport.lock().await;
    let wire = seal_record(&mut guard, RecordKind::Text, &plain).map_err(|e| e.to_string())?;
    out_tx
        .send(OutboundWire::Text(STANDARD.encode(wire)))
        .await
        .map_err(|_| "session outbound channel closed".to_owned())
}

const NOTIFY_DEDUP_WINDOW_MS: i64 = 12_000;

#[derive(Clone, Debug, PartialEq, Eq)]
struct AgentSnapshot {
    state: String,
    permission_count: u64,
    question_count: u64,
    returned_idle: bool,
    run: String,
}

impl AgentSnapshot {
    fn from_value(agent: &Value) -> Self {
        Self {
            state: agent
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_owned(),
            permission_count: agent
                .get("permission_count")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            question_count: agent
                .get("question_count")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            returned_idle: agent
                .get("returned_idle")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            run: agent
                .get("run")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
        }
    }
}

#[derive(Default)]
struct NotificationDeduper {
    recent: HashMap<String, i64>,
}

impl NotificationDeduper {
    fn normalize_kind(kind: &str) -> &str {
        match kind {
            "returned_idle" => "turn_completed",
            other => other,
        }
    }

    fn should_emit(&mut self, agent_key: &str, raw_kind: &str, now_ms: i64) -> bool {
        self.recent
            .retain(|_, ts| now_ms.saturating_sub(*ts) < NOTIFY_DEDUP_WINDOW_MS * 4);
        let kind = Self::normalize_kind(raw_kind);
        if kind == "blocked" {
            for specific in ["approval_requested", "question_asked"] {
                let specific_key = format!("{agent_key}::{specific}");
                if let Some(&ts) = self.recent.get(&specific_key)
                    && now_ms.saturating_sub(ts) < NOTIFY_DEDUP_WINDOW_MS
                {
                    return false;
                }
            }
        }
        let key = format!("{agent_key}::{kind}");
        if let Some(&last_ts) = self.recent.get(&key)
            && now_ms.saturating_sub(last_ts) < NOTIFY_DEDUP_WINDOW_MS
        {
            return false;
        }
        self.recent.insert(key, now_ms);
        true
    }
}

fn detect_agent_notification_kind(
    prev: Option<&AgentSnapshot>,
    next: &AgentSnapshot,
) -> Option<&'static str> {
    if next.state == "exited" {
        return None;
    }
    match prev {
        None => {
            if next.permission_count > 0 {
                Some("approval_requested")
            } else if next.question_count > 0 {
                Some("question_asked")
            } else if next.state == "blocked" {
                Some("blocked")
            } else {
                None
            }
        }
        Some(p) => {
            if next.permission_count > p.permission_count
                || (p.state != "blocked" && next.state == "blocked" && next.permission_count > 0)
            {
                return Some("approval_requested");
            }
            if next.question_count > p.question_count
                || (p.state != "blocked" && next.state == "blocked" && next.question_count > 0)
            {
                return Some("question_asked");
            }
            if p.state != "blocked" && next.state == "blocked" {
                return Some("blocked");
            }
            let same_run = p.run.is_empty() || next.run.is_empty() || p.run == next.run;
            if same_run
                && next.state == "idle"
                && (matches!(p.state.as_str(), "working" | "retrying")
                    || (!p.returned_idle && next.returned_idle))
            {
                return Some("turn_completed");
            }
            None
        }
    }
}

fn load_registered_push_tokens(config: &RemoteConfig) -> Vec<PushDeviceToken> {
    if !(config.push_enabled && config.relay_url.is_some()) {
        return Vec::new();
    }
    DeviceStore::load()
        .ok()
        .map(|d| {
            d.devices
                .into_iter()
                .filter_map(|dev| {
                    dev.push_token.map(|tok| PushDeviceToken {
                        platform: dev.platform,
                        token: tok,
                        push_type: None,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// D-09: Returns Live Activity / Ongoing Pulse tokens ONLY for devices that
/// explicitly opted in (`live_activity_enabled == true`), split by whether
/// `live_activity_summary` opt-in is also active.
fn load_opt_in_live_pulse_tokens(
    config: &RemoteConfig,
) -> (Vec<PushDeviceToken>, Vec<PushDeviceToken>) {
    if !(config.push_enabled && config.relay_url.is_some()) {
        return (Vec::new(), Vec::new());
    }
    let Ok(data) = DeviceStore::load() else {
        return (Vec::new(), Vec::new());
    };
    let mut minimal_tokens = Vec::new();
    let mut summary_tokens = Vec::new();
    for dev in data.devices {
        if !dev.live_activity_enabled {
            continue;
        }
        let allow_summary = dev.live_activity_summary || config.live_activity_summary;
        let bucket = if allow_summary {
            &mut summary_tokens
        } else {
            &mut minimal_tokens
        };
        if dev.platform == "ios" {
            if let Some(tok) = dev.live_activity_push_token {
                bucket.push(PushDeviceToken {
                    platform: "ios".into(),
                    token: tok,
                    push_type: Some("liveactivity".into()),
                });
            } else if let Some(tok) = dev.live_activity_push_to_start_token {
                bucket.push(PushDeviceToken {
                    platform: "ios".into(),
                    token: tok,
                    push_type: Some("liveactivity_start".into()),
                });
            }
        } else if dev.platform == "android"
            && let Some(tok) = dev.push_token
        {
            bucket.push(PushDeviceToken {
                platform: "android".into(),
                token: tok,
                push_type: Some("android_ongoing".into()),
            });
        }
    }
    (minimal_tokens, summary_tokens)
}

/// Computes a D-09 minimal-metadata fleet pulse payload from live host agents.
/// `include_summary` is ONLY true for explicitly opted-in summary recipients and
/// every summary string is passed through `redact_live_activity_summary`.
fn compute_live_pulse_payload(
    host_id: &str,
    host_name: &str,
    agents: &[Value],
    include_summary: bool,
    now_ms: i64,
) -> LiveActivityPayload {
    let mut working_count = 0u32;
    let mut blocked_count = 0u32;
    let mut done_count = 0u32;
    let mut alert_count = 0u32;

    // Track highest-priority spotlight agent (breaker > hard conflict > approval > question > bs_flagged > finished).
    let mut top_priority: u8 = 0;
    let mut top_alias: Option<String> = None;
    let mut top_kind: Option<LiveActivityEventKind> = None;
    let mut top_summary: Option<String> = None;
    let mut top_pane: Option<String> = None;

    for agent in agents {
        let state = agent
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        if state == "exited" {
            continue;
        }
        let pane = agent
            .get("pane")
            .or_else(|| agent.get("pane_id"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let name = agent
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(pane);
        let perm = agent
            .get("permission_count")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let q = agent
            .get("question_count")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let tripped = agent
            .get("breaker")
            .and_then(|b| b.get("tripped"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let coll = agent
            .get("collision_severity")
            .and_then(Value::as_str)
            .unwrap_or("clean");
        let audit_crit = agent
            .get("audit_summary")
            .and_then(|a| a.get("critical_count"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let raw_summary = agent.get("summary").and_then(Value::as_str);

        if matches!(state, "working" | "retrying") {
            working_count += 1;
        } else if state == "blocked" || perm > 0 || q > 0 {
            blocked_count += 1;
        } else if state == "idle" {
            done_count += 1;
        }

        let is_hard_coll = matches!(coll, "hard_conflict" | "same_directory_race");
        if tripped || is_hard_coll || audit_crit > 0 {
            alert_count += 1;
        }

        let candidate = if tripped {
            Some((6u8, LiveActivityEventKind::LoopPaused))
        } else if is_hard_coll {
            Some((5, LiveActivityEventKind::HardConflict))
        } else if perm > 0 {
            Some((4, LiveActivityEventKind::ApprovalRequested))
        } else if q > 0 || state == "blocked" {
            Some((3, LiveActivityEventKind::QuestionAsked))
        } else if audit_crit > 0 {
            Some((2, LiveActivityEventKind::BsFlagged))
        } else if state == "idle" {
            Some((1, LiveActivityEventKind::TurnFinished))
        } else {
            None
        };

        if let Some((prio, kind)) = candidate
            && prio > top_priority
        {
            top_priority = prio;
            top_alias = Some(name.to_owned());
            top_kind = Some(kind);
            top_pane = if pane.is_empty() {
                None
            } else {
                Some(pane.to_owned())
            };
            top_summary = if include_summary {
                raw_summary.and_then(|s| {
                    crate::briefing::redact::redact_live_activity_summary(
                        s,
                        LIVE_ACTIVITY_MAX_SUMMARY_CHARS,
                    )
                })
            } else {
                None
            };
        }
    }

    let deep_link = match top_pane {
        Some(p) => format!("masil://host/{host_id}/agent/{}", p.replace('%', "%25")),
        None => format!("masil://host/{host_id}"),
    };

    LiveActivityPayload {
        host_id: host_id.to_owned(),
        host_name: host_name.to_owned(),
        working_count,
        blocked_count,
        done_count,
        alert_count,
        top_agent_alias: top_alias,
        top_event_kind: top_kind,
        top_summary_line: top_summary,
        deep_link,
        updated_at: (now_ms.max(0) as u64) / 1000,
    }
    .sanitized()
}

/// Completes a Phase 2A (`0x31`) visual markup upload: resolves agent cwd/socket,
/// saves the image cleanly, optionally delivers to agent (`send` | `queue`), or
/// returns structured prompt for mobile composer `prefill`.
async fn handle_asset_upload_end(
    upload_id: u16,
    bytes: Vec<u8>,
    end: AssetUploadEndPayload,
    shared: &SharedDaemon,
) -> Value {
    let target_agent = end.agent_id.clone();
    let explicit_socket = end.meta.server_socket.clone();
    let explicit_cwd = end.meta.cwd.clone();

    let (resolved_socket, resolved_cwd, resolved_target) = tokio::task::spawn_blocking(move || {
        let agents = list_all_host_agents();
        let matched = agents.iter().find(|a| {
            a.get("remote_id").and_then(Value::as_str) == Some(target_agent.as_str())
                || a.get("pane").and_then(Value::as_str) == Some(target_agent.as_str())
                || a.get("name").and_then(Value::as_str) == Some(target_agent.as_str())
        });
        let sock = explicit_socket
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                matched
                    .and_then(|a| a.get("server_socket").and_then(Value::as_str))
                    .map(str::to_owned)
            })
            .or_else(|| {
                default_masil_socket()
                    .ok()
                    .map(|p| p.to_string_lossy().into_owned())
            });
        let cwd = explicit_cwd.filter(|s| !s.trim().is_empty()).or_else(|| {
            matched
                .and_then(|a| a.get("cwd").and_then(Value::as_str))
                .map(str::to_owned)
        });
        let target = matched
            .and_then(|a| a.get("pane").and_then(Value::as_str))
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or(target_agent);
        (sock, cwd, target)
    })
    .await
    .unwrap_or((None, None, end.agent_id.clone()));

    let save_end = end.clone();
    let cwd_path = resolved_cwd.as_deref().map(std::path::PathBuf::from);
    let saved = match tokio::task::spawn_blocking(move || {
        save_markup_asset(cwd_path.as_deref(), &bytes, &save_end)
    })
    .await
    {
        Ok(Ok(s)) => s,
        Ok(Err(err)) => {
            return json!({
                "v": 1,
                "event": "asset.upload.result",
                "upload_id": upload_id,
                "ok": false,
                "error": err,
            });
        }
        Err(e) => {
            return json!({
                "v": 1,
                "event": "asset.upload.result",
                "upload_id": upload_id,
                "ok": false,
                "error": e.to_string(),
            });
        }
    };

    let delivery_mode_str = match end.delivery_mode {
        AssetDeliveryMode::Send => "send",
        AssetDeliveryMode::Queue => "queue",
        AssetDeliveryMode::Prefill => "prefill",
    };

    let delivery_res = match end.delivery_mode {
        AssetDeliveryMode::Prefill => Ok(Value::Null),
        AssetDeliveryMode::Send => {
            let sock = resolved_socket.clone();
            let tgt = resolved_target.clone();
            let prompt = saved.structured_prompt.clone();
            tokio::task::spawn_blocking(move || {
                run_self_agent_json(sock.as_deref(), &["prompt".into(), tgt, prompt])
            })
            .await
            .map_err(|e| e.to_string())
            .and_then(|r| r)
        }
        AssetDeliveryMode::Queue => {
            let sock = resolved_socket.clone();
            let tgt = resolved_target.clone();
            let prompt = saved.structured_prompt.clone();
            tokio::task::spawn_blocking(move || {
                run_self_agent_json(
                    sock.as_deref(),
                    &["queue".into(), tgt, "add".into(), prompt],
                )
            })
            .await
            .map_err(|e| e.to_string())
            .and_then(|r| r)
        }
    };

    match delivery_res {
        Ok(res) => {
            if !matches!(end.delivery_mode, AssetDeliveryMode::Prefill) {
                shared.refresh_notify.notify_one();
            }
            json!({
                "v": 1,
                "event": "asset.upload.result",
                "upload_id": upload_id,
                "ok": true,
                "agent_id": end.agent_id,
                "delivery_mode": delivery_mode_str,
                "saved_path": saved.absolute_path.to_string_lossy(),
                "prompt_path": saved.prompt_path,
                "in_git_worktree": saved.in_git_worktree,
                "size_bytes": saved.size_bytes,
                "structured_prompt": saved.structured_prompt,
                "result": res,
            })
        }
        Err(err) => json!({
            "v": 1,
            "event": "asset.upload.result",
            "upload_id": upload_id,
            "ok": false,
            "saved_path": saved.absolute_path.to_string_lossy(),
            "prompt_path": saved.prompt_path,
            "structured_prompt": saved.structured_prompt,
            "error": err,
        }),
    }
}

fn agents_fingerprint(agents: &[Value]) -> String {
    let mut parts = Vec::with_capacity(agents.len());
    for a in agents {
        parts.push(format!(
            "{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
            a.get("remote_id").and_then(Value::as_str).unwrap_or(""),
            a.get("state").and_then(Value::as_str).unwrap_or(""),
            a.get("cwd").and_then(Value::as_str).unwrap_or(""),
            a.get("session_id").and_then(Value::as_str).unwrap_or(""),
            a.get("name").and_then(Value::as_str).unwrap_or(""),
            a.get("process").and_then(Value::as_str).unwrap_or(""),
            a.get("run").and_then(Value::as_str).unwrap_or(""),
            a.get("permission_count")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            a.get("question_count").and_then(Value::as_u64).unwrap_or(0),
            a.get("returned_idle")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            a.get("seen").and_then(Value::as_bool).unwrap_or(false),
            a.get("stale").and_then(Value::as_bool).unwrap_or(false),
            a.get("revision").and_then(Value::as_str).unwrap_or(""),
            a.get("audit_summary")
                .and_then(|v| v.get("id"))
                .and_then(Value::as_i64)
                .unwrap_or(0),
            a.get("breaker")
                .and_then(|v| v.get("tripped"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
            a.get("collision_severity")
                .and_then(Value::as_str)
                .unwrap_or(""),
            a.get("cache_warmth")
                .and_then(|v| v.get("tier"))
                .and_then(Value::as_str)
                .unwrap_or(""),
        ));
    }
    parts.join("\n")
}

async fn run_agents_watcher(shared: SharedDaemon) {
    let mut last_fp = String::new();
    let mut last_snapshots: HashMap<String, AgentSnapshot> = HashMap::new();
    let mut seen_audit_ids: HashMap<String, i64> = HashMap::new();
    let mut seen_tripped: HashMap<String, bool> = HashMap::new();
    let mut seen_collisions: HashMap<String, String> = HashMap::new();
    let mut initialized = false;
    let mut idle_tick: u32 = 0;
    let mut tick = tokio::time::interval(Duration::from_millis(1000));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = shared.refresh_notify.notified() => {}
        }
        let sessions = shared.connected_sessions.load(Ordering::Relaxed);
        let push_tokens = load_registered_push_tokens(&shared.config);
        if sessions == 0 {
            idle_tick = idle_tick.wrapping_add(1);
            if !idle_tick.is_multiple_of(3) {
                continue;
            }
            let has_hooks = tokio::task::spawn_blocking(|| {
                crate::plugin::manifest::load_catalog()
                    .is_ok_and(|c| c.has_hooks_for("agent.state_changed"))
            })
            .await
            .unwrap_or(false);
            if !has_hooks && push_tokens.is_empty() {
                continue;
            }
        } else {
            idle_tick = 0;
        }
        let agents = tokio::task::spawn_blocking(list_all_host_agents)
            .await
            .unwrap_or_default();
        let fp = agents_fingerprint(&agents);
        if fp != last_fp {
            last_fp = fp;
            let now = crate::remote::config::now_ms();
            let mut next_snapshots = HashMap::with_capacity(agents.len());
            for agent in &agents {
                let key = agent
                    .get("remote_id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let snap = AgentSnapshot::from_value(agent);
                let prev = last_snapshots.get(&key);
                if let Some(prev_snap) = prev
                    && prev_snap.state != snap.state
                {
                    let prev_state = prev_snap.state.clone();
                    let next_state = snap.state.clone();
                    let agent_val = agent.clone();
                    let sock = agent
                        .get("server_socket")
                        .and_then(Value::as_str)
                        .map(std::path::PathBuf::from);
                    tokio::spawn(async move {
                        // Avoid duplicate hook dispatch when the server's Gen 6
                        // coordinator already watches and fires local hooks.
                        if let Some(ref sp) = sock {
                            let check = sp.clone();
                            let handled = tokio::task::spawn_blocking(move || {
                                crate::coordinator::is_watching(&check)
                            })
                            .await
                            .unwrap_or(false);
                            if handled {
                                return;
                            }
                        }
                        crate::plugin::runner::dispatch_hook_event(
                            "agent.state_changed",
                            json!({
                                "previous_state": prev_state,
                                "state": next_state,
                                "name": agent_val.get("name"),
                                "pane": agent_val.get("pane"),
                                "provider": agent_val.get("provider"),
                            }),
                            Some(agent_val),
                            sock,
                        )
                        .await;
                    });
                }

                if initialized && let Some(event_kind) = detect_agent_notification_kind(prev, &snap)
                {
                    let should_emit = {
                        let mut dedup = shared.notify_dedup.lock().await;
                        dedup.should_emit(&key, event_kind, now)
                    };
                    if should_emit {
                        let pane = agent
                            .get("pane")
                            .or_else(|| agent.get("pane_id"))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned();
                        let name = agent
                            .get("name")
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                            .unwrap_or(&pane)
                            .to_owned();
                        let provider = agent
                            .get("provider")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned();
                        let sock_for_shield = agent
                            .get("server_socket")
                            .and_then(Value::as_str)
                            .map(std::path::PathBuf::from);
                        let run_for_shield = snap.run.clone();
                        let pane_for_shield = pane.clone();
                        let kind_for_shield = event_kind.to_owned();
                        let held_by_focus = if let Some(sp) = sock_for_shield {
                            let summary_json = json!({
                                "agent": name,
                                "provider": provider,
                                "kind": event_kind,
                            })
                            .to_string();
                            tokio::task::spawn_blocking(move || {
                                crate::briefing::shield::suppress_or_record_event(
                                    &sp,
                                    &pane_for_shield,
                                    &run_for_shield,
                                    &kind_for_shield,
                                    Some(&summary_json),
                                )
                            })
                            .await
                            .unwrap_or(false)
                        } else {
                            false
                        };
                        if !held_by_focus {
                            if sessions > 0 {
                                let evt = json!({
                                    "v": 1,
                                    "event": "agent.notification",
                                    "host_id": shared.identity.host_id,
                                    "host_name": shared.config.effective_host_name(),
                                    "remote_id": key,
                                    "agent_id": pane,
                                    "agent_name": name,
                                    "provider": provider,
                                    "event_kind": event_kind,
                                    "permission_count": snap.permission_count,
                                    "question_count": snap.question_count,
                                    "occurred_ms": now,
                                });
                                let _ = shared.notify_tx.send(Arc::new(evt));
                            }
                            if !push_tokens.is_empty() {
                                let req = PushNotificationRequest {
                                    host_id: shared.identity.host_id.clone(),
                                    host_name: shared.config.effective_host_name(),
                                    agent_id: pane,
                                    agent_name: name,
                                    event_kind: event_kind.to_owned(),
                                    occurred_ms: now,
                                    tokens: push_tokens.clone(),
                                    live_activity: None,
                                };
                                let _ = shared.relay_push_tx.try_send(req);
                            }
                        }
                    }
                }
                next_snapshots.insert(key.clone(), snap);

                // Phase 1 real-time push events (`audit.completed`, `collision.changed`, `breaker.tripped`).
                let audit_id = agent
                    .get("audit_summary")
                    .and_then(|v| v.get("id"))
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                let prev_audit_id = seen_audit_ids.insert(key.clone(), audit_id).unwrap_or(0);
                if initialized && audit_id > prev_audit_id && sessions > 0 {
                    let evt = json!({
                        "v": 1,
                        "event": "audit.completed",
                        "host_id": shared.identity.host_id,
                        "remote_id": key,
                        "audit_summary": agent.get("audit_summary"),
                        "occurred_ms": now,
                    });
                    let _ = shared.notify_tx.send(Arc::new(evt));
                }

                let is_tripped = agent
                    .get("breaker")
                    .and_then(|v| v.get("tripped"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let was_tripped = seen_tripped
                    .insert(key.clone(), is_tripped)
                    .unwrap_or(false);
                if initialized && is_tripped && !was_tripped {
                    if sessions > 0 {
                        let evt = json!({
                            "v": 1,
                            "event": "breaker.tripped",
                            "host_id": shared.identity.host_id,
                            "remote_id": key,
                            "breaker": agent.get("breaker"),
                            "occurred_ms": now,
                        });
                        let _ = shared.notify_tx.send(Arc::new(evt));
                    }
                    if !push_tokens.is_empty() {
                        let pane = agent
                            .get("pane")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned();
                        let name = agent
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or(&pane)
                            .to_owned();
                        let req = PushNotificationRequest {
                            host_id: shared.identity.host_id.clone(),
                            host_name: shared.config.effective_host_name(),
                            agent_id: pane,
                            agent_name: name,
                            event_kind: "circuit_breaker_tripped".to_owned(),
                            occurred_ms: now,
                            tokens: push_tokens.clone(),
                            live_activity: None,
                        };
                        let _ = shared.relay_push_tx.try_send(req);
                    }
                }

                let coll_sev = agent
                    .get("collision_severity")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let prev_coll = seen_collisions
                    .insert(key.clone(), coll_sev.clone())
                    .unwrap_or_default();
                if initialized && coll_sev != prev_coll && sessions > 0 {
                    let evt = json!({
                        "v": 1,
                        "event": "collision.changed",
                        "host_id": shared.identity.host_id,
                        "remote_id": key,
                        "severity": coll_sev,
                        "peers": agent.get("collision_peers"),
                        "occurred_ms": now,
                    });
                    let _ = shared.notify_tx.send(Arc::new(evt));
                }
                // Breakthrough push for `hard_conflict` / `same_directory_race` even during Focus Shield.
                if initialized
                    && coll_sev != prev_coll
                    && matches!(coll_sev.as_str(), "hard_conflict" | "same_directory_race")
                    && !push_tokens.is_empty()
                {
                    let pane = agent
                        .get("pane")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned();
                    let name = agent
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or(&pane)
                        .to_owned();
                    let req = PushNotificationRequest {
                        host_id: shared.identity.host_id.clone(),
                        host_name: shared.config.effective_host_name(),
                        agent_id: pane,
                        agent_name: name,
                        event_kind: coll_sev.clone(),
                        occurred_ms: now,
                        tokens: push_tokens.clone(),
                        live_activity: None,
                    };
                    let _ = shared.relay_push_tx.try_send(req);
                }
            }
            last_snapshots = next_snapshots;
            initialized = true;
            // Phase 2B (D-09): Emit fleet pulse to connected sessions and opted-in Live Activity devices.
            let host_name = shared.config.effective_host_name();
            let pulse_minimal = compute_live_pulse_payload(
                &shared.identity.host_id,
                &host_name,
                &agents,
                false,
                now,
            );
            let pulse_with_summary = compute_live_pulse_payload(
                &shared.identity.host_id,
                &host_name,
                &agents,
                true,
                now,
            );
            if sessions > 0 {
                let evt = json!({
                    "v": 1,
                    "event": "live_pulse.updated",
                    "host_id": shared.identity.host_id,
                    "pulse": pulse_minimal,
                    "pulse_with_summary": pulse_with_summary,
                });
                let _ = shared.notify_tx.send(Arc::new(evt));
                let _ = shared.agents_tx.send(Arc::new(agents));
            }
            let (minimal_la_tokens, summary_la_tokens) =
                load_opt_in_live_pulse_tokens(&shared.config);
            if !minimal_la_tokens.is_empty() {
                let req = PushNotificationRequest {
                    host_id: shared.identity.host_id.clone(),
                    host_name: host_name.clone(),
                    agent_id: String::new(),
                    agent_name: host_name.clone(),
                    event_kind: pulse_minimal
                        .top_event_kind
                        .map(|k| k.as_str().to_owned())
                        .unwrap_or_else(|| "turn_finished".into()),
                    occurred_ms: now,
                    tokens: minimal_la_tokens,
                    live_activity: Some(pulse_minimal),
                };
                let _ = shared.relay_push_tx.try_send(req);
            }
            if !summary_la_tokens.is_empty() {
                let req = PushNotificationRequest {
                    host_id: shared.identity.host_id.clone(),
                    host_name: host_name.clone(),
                    agent_id: String::new(),
                    agent_name: host_name,
                    event_kind: pulse_with_summary
                        .top_event_kind
                        .map(|k| k.as_str().to_owned())
                        .unwrap_or_else(|| "turn_finished".into()),
                    occurred_ms: now,
                    tokens: summary_la_tokens,
                    live_activity: Some(pulse_with_summary),
                };
                let _ = shared.relay_push_tx.try_send(req);
            }
        }
    }
}

async fn run_plugins_watcher(shared: SharedDaemon) {
    let mut last_fp = String::new();
    let mut initialized = false;
    let mut idle_tick: u32 = 0;
    let mut tick = tokio::time::interval(Duration::from_millis(1500));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = shared.plugins_notify.notified() => {}
        }
        let sessions = shared.connected_sessions.load(Ordering::Relaxed);
        if sessions == 0 {
            idle_tick = idle_tick.wrapping_add(1);
            if !idle_tick.is_multiple_of(2) {
                continue;
            }
        } else {
            idle_tick = 0;
        }
        let fp = tokio::task::spawn_blocking(crate::plugin::manifest::catalog_fingerprint)
            .await
            .unwrap_or_default();
        if fp != last_fp {
            let sync_servers = initialized;
            last_fp = fp;
            initialized = true;
            if let Ok(Ok(catalog)) = tokio::task::spawn_blocking(move || {
                if sync_servers && let Ok(cat) = crate::plugin::manifest::load_catalog() {
                    crate::plugin::sync_theme_to_masil_servers(&cat);
                }
                crate::plugin::catalog_json()
            })
            .await
                && sessions > 0
            {
                let _ = shared.plugins_tx.send(Arc::new(catalog));
            }
        }
    }
}

async fn run_push_watcher(shared: SharedDaemon) {
    let mut seen: HashSet<String> = HashSet::new();
    let mut initialized = false;
    let mut interval = tokio::time::interval(Duration::from_secs(5));
    loop {
        interval.tick().await;
        let sessions = shared.connected_sessions.load(Ordering::Relaxed);
        let tokens = load_registered_push_tokens(&shared.config);
        let has_inbox_hooks = tokio::task::spawn_blocking(|| {
            crate::plugin::manifest::load_catalog().is_ok_and(|c| c.has_hooks_for("inbox.event"))
        })
        .await
        .unwrap_or(false);
        if tokens.is_empty() && !has_inbox_hooks && sessions == 0 {
            continue;
        }
        let items = tokio::task::spawn_blocking(|| {
            let mut found = Vec::new();
            for server in discover_masil_servers() {
                if let Ok(val) = run_self_agent_json(Some(&server.socket), &["inbox".into()])
                    && let Some(arr) = val
                        .get("events")
                        .or_else(|| val.get("items"))
                        .and_then(Value::as_array)
                {
                    for row in arr {
                        found.push((server.name.clone(), server.socket.clone(), row.clone()));
                    }
                }
            }
            found
        })
        .await
        .unwrap_or_default();

        for (server_name, server_socket, item) in items {
            let key = format!(
                "{server_name}:{}",
                item.get("id")
                    .and_then(Value::as_i64)
                    .or_else(|| item.get("seq").and_then(Value::as_i64))
                    .unwrap_or(0)
            );
            if !seen.insert(key) {
                continue;
            }
            if !initialized {
                continue;
            }
            let pane = item
                .get("pane")
                .or_else(|| item.get("pane_id"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let name = item
                .get("agent")
                .or_else(|| item.get("name"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or(if pane.is_empty() { "agent" } else { &pane })
                .to_owned();
            let provider = item
                .get("provider")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let event_kind = item
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("blocked")
                .to_owned();
            let summary = item.get("summary").and_then(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .or_else(|| v.get("text").and_then(Value::as_str).map(str::to_owned))
            });

            if has_inbox_hooks {
                let mut event_val = item.clone();
                if let Some(obj) = event_val.as_object_mut() {
                    obj.insert("server_name".into(), Value::String(server_name.clone()));
                    obj.insert("server_socket".into(), Value::String(server_socket.clone()));
                }
                let hook_agent = json!({
                    "name": name,
                    "pane": pane,
                    "pane_id": pane,
                    "provider": provider,
                    "server_socket": server_socket,
                });
                let sock_path = std::path::PathBuf::from(&server_socket);
                tokio::spawn(async move {
                    let check = sock_path.clone();
                    let handled = tokio::task::spawn_blocking(move || {
                        crate::coordinator::is_watching(&check)
                    })
                    .await
                    .unwrap_or(false);
                    if handled {
                        return;
                    }
                    crate::plugin::runner::dispatch_hook_event(
                        "inbox.event",
                        event_val,
                        Some(hook_agent),
                        Some(sock_path),
                    )
                    .await;
                });
            }

            let now = crate::remote::config::now_ms();
            let remote_id = format!("{server_name}::{pane}");
            let should_emit = {
                let mut dedup = shared.notify_dedup.lock().await;
                dedup.should_emit(&remote_id, &event_kind, now)
            };
            if should_emit {
                let sp = std::path::PathBuf::from(&server_socket);
                let run_str = item
                    .get("run")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let pane_str = pane.clone();
                let kind_str = event_kind.clone();
                let summary_json = json!({
                    "agent": name,
                    "provider": provider,
                    "kind": event_kind,
                    "summary": summary,
                })
                .to_string();
                let held_by_focus = tokio::task::spawn_blocking(move || {
                    crate::briefing::shield::suppress_or_record_event(
                        &sp,
                        &pane_str,
                        &run_str,
                        &kind_str,
                        Some(&summary_json),
                    )
                })
                .await
                .unwrap_or(false);
                if held_by_focus {
                    continue;
                }
                if sessions > 0 {
                    let evt = json!({
                        "v": 1,
                        "event": "agent.notification",
                        "host_id": shared.identity.host_id,
                        "host_name": shared.config.effective_host_name(),
                        "remote_id": remote_id,
                        "agent_id": pane,
                        "agent_name": name,
                        "provider": provider,
                        "event_kind": event_kind,
                        "summary": summary,
                        "occurred_ms": now,
                    });
                    let _ = shared.notify_tx.send(Arc::new(evt));
                }
                if !tokens.is_empty() {
                    let req = PushNotificationRequest {
                        host_id: shared.identity.host_id.clone(),
                        host_name: shared.config.effective_host_name(),
                        agent_id: pane,
                        agent_name: name,
                        event_kind,
                        occurred_ms: now,
                        tokens: tokens.clone(),
                        live_activity: None,
                    };
                    let _ = shared.relay_push_tx.try_send(req);
                }
            }
        }
        initialized = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(state: &str, perm: u64, q: u64, returned_idle: bool, run: &str) -> AgentSnapshot {
        AgentSnapshot {
            state: state.to_owned(),
            permission_count: perm,
            question_count: q,
            returned_idle,
            run: run.to_owned(),
        }
    }

    #[test]
    fn detects_completion_and_input_needed_transitions() {
        let working = snap("working", 0, 0, false, "r1");
        let idle_done = snap("idle", 0, 0, true, "r1");
        assert_eq!(
            detect_agent_notification_kind(Some(&working), &idle_done),
            Some("turn_completed")
        );

        let blocked_perm = snap("blocked", 1, 0, false, "r1");
        assert_eq!(
            detect_agent_notification_kind(Some(&working), &blocked_perm),
            Some("approval_requested")
        );

        let blocked_q = snap("blocked", 0, 1, false, "r1");
        assert_eq!(
            detect_agent_notification_kind(Some(&working), &blocked_q),
            Some("question_asked")
        );

        let blocked_plain = snap("blocked", 0, 0, false, "r1");
        assert_eq!(
            detect_agent_notification_kind(Some(&working), &blocked_plain),
            Some("blocked")
        );

        // Steady idle or working -> working does not notify.
        assert_eq!(
            detect_agent_notification_kind(Some(&idle_done), &idle_done),
            None
        );
        assert_eq!(
            detect_agent_notification_kind(Some(&working), &working),
            None
        );

        // Different run resetting to idle does not falsely claim completion.
        let new_run_idle = snap("idle", 0, 0, false, "r2");
        assert_eq!(
            detect_agent_notification_kind(Some(&working), &new_run_idle),
            None
        );
    }

    #[test]
    fn deduper_coalesces_returned_idle_and_specific_blockers() {
        let mut dedup = NotificationDeduper::default();
        assert!(dedup.should_emit("default::%1", "turn_completed", 10_000));
        assert!(!dedup.should_emit("default::%1", "returned_idle", 15_000));
        assert!(dedup.should_emit("default::%1", "turn_completed", 25_000));

        assert!(dedup.should_emit("default::%2", "question_asked", 10_000));
        assert!(!dedup.should_emit("default::%2", "blocked", 11_000));
    }

    #[test]
    fn fingerprint_reflects_returned_idle_and_seen() {
        let a = json!({
            "remote_id": "default::%1",
            "state": "idle",
            "returned_idle": false,
            "seen": false,
            "revision": "1"
        });
        let b = json!({
            "remote_id": "default::%1",
            "state": "idle",
            "returned_idle": true,
            "seen": false,
            "revision": "2"
        });
        assert_ne!(agents_fingerprint(&[a]), agents_fingerprint(&[b]));
    }

    #[test]
    fn timeline_delta_emits_patch_on_append_and_replace_on_session_switch() {
        use masil_wire::rpc::{BindingConfidence, TimelineItem};

        let base = TimelinePage {
            binding_confidence: BindingConfidence::ReportedByHook,
            session_id: Some("ses-1".into()),
            source_label: "claude".into(),
            detected_model: Some("claude-sonnet".into()),
            items: vec![TimelineItem::UserMessage {
                id: "u1".into(),
                seq: 1,
                ts_ms: 100,
                text: "hello".into(),
            }],
            active_prompts: Vec::new(),
            has_more: false,
        };
        // Unchanged page produces no delta.
        assert!(build_timeline_delta_event("sub1", "d::%1", "s::%1", 1, &base, &base).is_none());

        // Appending a second item emits a minimal `patch` starting at index 1.
        let mut appended = base.clone();
        appended.items.push(TimelineItem::AssistantMessage {
            id: "a1".into(),
            seq: 2,
            ts_ms: 200,
            markdown: "world".into(),
            model: Some("claude-sonnet".into()),
        });
        let delta =
            build_timeline_delta_event("sub1", "d::%1", "s::%1", 1, &base, &appended).unwrap();
        assert_eq!(delta["event"], "timeline.delta");
        assert_eq!(delta["mode"], "patch");
        assert_eq!(delta["from_index"], 1);
        assert_eq!(delta["items"].as_array().unwrap().len(), 1);
        assert_eq!(delta["items"][0]["id"], "a1");

        // Switching sessions emits a `replace` event with the full page.
        let mut switched = appended.clone();
        switched.session_id = Some("ses-2".into());
        let rep =
            build_timeline_delta_event("sub1", "d::%1", "s::%1", 2, &appended, &switched).unwrap();
        assert_eq!(rep["mode"], "replace");
        assert_eq!(rep["page"]["session_id"], "ses-2");
    }
}
