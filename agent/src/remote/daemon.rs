//! Single per-user `masil-agent remote` daemon managing direct LAN/Tailscale
//! WebSocket ingress, outbound E2EE relay transport, Noise NK session handling,
//! live `-C` terminal multiplexing, voice STT upload, and minimal push
//! forwarding.

use crate::remote::config::{
    DeviceStore, HostIdentity, RemoteConfig, ensure_private_dir, state_dir,
};
use crate::remote::discovery::{discover_masil_servers, list_all_host_agents, run_self_agent_json};
use crate::remote::rpc::{SessionAuth, dispatch_authenticated_rpc, handle_rpc};
use crate::remote::terminal_stream::TerminalMux;
use crate::remote::voice::{AudioUploadBuffer, transcribe_audio};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use futures_util::{SinkExt, StreamExt};
use masil_wire::frames::{AudioEndPayload, BinaryFrame, BinaryOpcode, TerminalResizePayload};
use masil_wire::noise::{ResponderHandshake, TransportState};
use masil_wire::record::{RecordKind, open_record, seal_record};
use masil_wire::rpc::{
    HandshakeEnvelope, PushDeviceToken, PushNotificationRequest, RelayControlMessage, RpcRequest,
    RpcResponse,
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
    let shared = SharedDaemon {
        identity: identity.clone(),
        config: config.clone(),
        active_sessions: Arc::new(Mutex::new(HashMap::new())),
        connected_sessions: Arc::new(AtomicUsize::new(0)),
        agents_tx,
        refresh_notify: Arc::new(Notify::new()),
        plugins_tx,
        plugins_notify: Arc::new(Notify::new()),
        relay_push_tx,
    };

    // 0. Live agent state watcher: pushes `agents.changed` within 1s to all
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

    // 3. Background inbox watcher forwarding minimal push metadata over the authenticated relay socket.
    if config.push_enabled && config.relay_url.is_some() {
        let shared_push = shared.clone();
        tokio::spawn(async move {
            run_push_watcher(shared_push).await;
        });
    }

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
                                && let Ok(encoded) = serde_json::to_string(&RelayControlMessage::Push { request })
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
    let mut audio_buffers: HashMap<u16, AudioUploadBuffer> = HashMap::new();

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
                                    | "agent.interrupt.request"
                                    | "agent.approve.request"
                                    | "agent.configure.request"
                                    | "agent.resume.request"
                                    | "plugin.creator.start.request"
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
                    BinaryOpcode::TerminalOutput | BinaryOpcode::TerminalSnapshot => {}
                }
            }
        }
    }

    terminal_mux.close_all().await;
    bin_forwarder.abort();
    let _ = out_tx.send(OutboundWire::Close).await;
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

fn agents_fingerprint(agents: &[Value]) -> String {
    let mut parts = Vec::with_capacity(agents.len());
    for a in agents {
        parts.push(format!(
            "{}|{}|{}|{}|{}|{}|{}|{}|{}",
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
        ));
    }
    parts.join("\n")
}

async fn run_agents_watcher(shared: SharedDaemon) {
    let mut last_fp = String::new();
    let mut last_states: HashMap<String, String> = HashMap::new();
    let mut tick = tokio::time::interval(Duration::from_millis(1000));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = shared.refresh_notify.notified() => {}
        }
        if shared.connected_sessions.load(Ordering::Relaxed) == 0 {
            continue;
        }
        let agents = tokio::task::spawn_blocking(list_all_host_agents)
            .await
            .unwrap_or_default();
        let fp = agents_fingerprint(&agents);
        if fp != last_fp {
            last_fp = fp;
            let mut next_states = HashMap::with_capacity(agents.len());
            for agent in &agents {
                let key = agent
                    .get("remote_id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let st = agent
                    .get("state")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_owned();
                if let Some(prev) = last_states.get(&key)
                    && prev != &st
                {
                    let prev_state = prev.clone();
                    let next_state = st.clone();
                    let agent_val = agent.clone();
                    let sock = agent
                        .get("server_socket")
                        .and_then(Value::as_str)
                        .map(std::path::PathBuf::from);
                    tokio::spawn(async move {
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
                next_states.insert(key, st);
            }
            last_states = next_states;
            let _ = shared.agents_tx.send(Arc::new(agents));
        }
    }
}

async fn run_plugins_watcher(shared: SharedDaemon) {
    let mut last_fp = String::new();
    let mut tick = tokio::time::interval(Duration::from_millis(1500));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = shared.plugins_notify.notified() => {}
        }
        if shared.connected_sessions.load(Ordering::Relaxed) == 0 {
            continue;
        }
        let fp = tokio::task::spawn_blocking(crate::plugin::manifest::catalog_fingerprint)
            .await
            .unwrap_or_default();
        if fp != last_fp {
            last_fp = fp;
            if let Ok(Ok(catalog)) = tokio::task::spawn_blocking(crate::plugin::catalog_json).await
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
        let tokens: Vec<PushDeviceToken> = DeviceStore::load()
            .ok()
            .map(|d| {
                d.devices
                    .into_iter()
                    .filter_map(|dev| {
                        dev.push_token.map(|tok| PushDeviceToken {
                            platform: dev.platform,
                            token: tok,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        if tokens.is_empty() {
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
                        found.push((server.name.clone(), row.clone()));
                    }
                }
            }
            found
        })
        .await
        .unwrap_or_default();

        for (server_name, item) in items {
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
            let req = PushNotificationRequest {
                host_id: shared.identity.host_id.clone(),
                host_name: shared.config.effective_host_name(),
                agent_id: item
                    .get("pane")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                agent_name: item
                    .get("agent")
                    .and_then(Value::as_str)
                    .unwrap_or("agent")
                    .to_owned(),
                event_kind: item
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or("blocked")
                    .to_owned(),
                occurred_ms: crate::remote::config::now_ms(),
                tokens: tokens.clone(),
            };
            let _ = shared.relay_push_tx.try_send(req);
        }
        initialized = true;
    }
}
