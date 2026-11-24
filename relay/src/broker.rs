use crate::config::RelayConfig;
use axum::Json;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use futures_util::{SinkExt, StreamExt};
use masil_wire::rpc::RelayControlMessage;
use rand_core::{OsRng, RngCore};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};

const OUTBOUND_QUEUE_DEPTH: usize = 128;

#[derive(Debug, Clone)]
pub enum OutboundClientMsg {
    Text(String),
    Binary(Vec<u8>),
    Close(u16, String),
}

struct HostEntry {
    generation: u64,
    token_hash: [u8; 32],
    host_tx: mpsc::Sender<String>,
    clients: HashMap<String, mpsc::Sender<OutboundClientMsg>>,
}

pub struct RelayState {
    pub config: RelayConfig,
    pub http: reqwest::Client,
    hosts: Mutex<HashMap<String, HostEntry>>,
    next_gen: Mutex<u64>,
}

impl RelayState {
    pub fn new(config: RelayConfig) -> Self {
        Self {
            config,
            http: reqwest::Client::new(),
            hosts: Mutex::new(HashMap::new()),
            next_gen: Mutex::new(1),
        }
    }

    pub async fn verify_host_token(&self, host_id: &str, token: &str) -> bool {
        let hash = sha256_bytes(token.as_bytes());
        let guard = self.hosts.lock().await;
        guard
            .get(host_id)
            .is_some_and(|entry| entry.token_hash == hash)
    }

    async fn alloc_gen(&self) -> u64 {
        let mut g = self.next_gen.lock().await;
        let cur = *g;
        *g += 1;
        cur
    }
}

#[derive(Deserialize)]
pub struct HostQuery {
    token: Option<String>,
}

pub async fn healthz(State(state): State<Arc<RelayState>>) -> impl IntoResponse {
    let guard = state.hosts.lock().await;
    let hosts_online = guard.len();
    let active_sessions: usize = guard.values().map(|h| h.clients.len()).sum();
    Json(json!({
        "status": "ok",
        "hosts_online": hosts_online,
        "active_sessions": active_sessions,
        "push_apns_configured": state.config.apns.is_some(),
        "push_fcm_configured": state.config.fcm.is_some(),
    }))
}

pub fn extract_bearer(headers: &HeaderMap) -> Option<String> {
    let val = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    val.strip_prefix("Bearer ").map(str::to_owned)
}

fn sha256_bytes(input: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(input);
    let d = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&d);
    out
}

fn random_session_id() -> String {
    let mut buf = [0u8; 16];
    OsRng.fill_bytes(&mut buf);
    let mut s = String::with_capacity(32);
    for b in buf {
        use std::fmt::Write;
        let _ = write!(&mut s, "{b:02x}");
    }
    s
}

fn valid_host_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

pub async fn host_ws(
    Path(host_id): Path<String>,
    Query(query): Query<HostQuery>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
    State(state): State<Arc<RelayState>>,
) -> Response {
    if !valid_host_id(&host_id) {
        return (StatusCode::BAD_REQUEST, "invalid host_id").into_response();
    }
    let token = extract_bearer(&headers).or(query.token);
    let Some(token) = token.filter(|t| t.len() >= 16) else {
        return (StatusCode::UNAUTHORIZED, "missing host token").into_response();
    };
    let token_hash = sha256_bytes(token.as_bytes());
    {
        let guard = state.hosts.lock().await;
        if let Some(existing) = guard.get(&host_id) {
            if existing.token_hash != token_hash {
                return (StatusCode::FORBIDDEN, "host_id owned by another token").into_response();
            }
        } else if guard.len() >= state.config.max_hosts {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "relay host capacity reached",
            )
                .into_response();
        }
    }
    let max_frame = state.config.max_frame_bytes;
    ws.max_message_size(max_frame)
        .on_upgrade(move |socket| run_host_socket(socket, host_id, token_hash, state))
}

async fn run_host_socket(
    socket: WebSocket,
    host_id: String,
    token_hash: [u8; 32],
    state: Arc<RelayState>,
) {
    let generation = state.alloc_gen().await;
    let (host_tx, mut host_rx) = mpsc::channel::<String>(OUTBOUND_QUEUE_DEPTH);

    {
        let mut guard = state.hosts.lock().await;
        if let Some(prev) = guard.insert(
            host_id.clone(),
            HostEntry {
                generation,
                token_hash,
                host_tx: host_tx.clone(),
                clients: HashMap::new(),
            },
        ) {
            for (_, client_tx) in prev.clients {
                let _ =
                    client_tx.try_send(OutboundClientMsg::Close(1012, "host_reconnected".into()));
            }
        }
    }

    let (mut ws_sink, mut ws_stream) = socket.split();
    let writer = tokio::spawn(async move {
        while let Some(text) = host_rx.recv().await {
            if ws_sink.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
        let _ = ws_sink.close().await;
    });

    while let Some(Ok(msg)) = ws_stream.next().await {
        match msg {
            Message::Text(text) => {
                let Ok(ctrl) = serde_json::from_str::<RelayControlMessage>(&text) else {
                    continue;
                };
                match ctrl {
                    RelayControlMessage::Frame {
                        session_id,
                        kind,
                        data,
                    } => {
                        let client_tx = {
                            let guard = state.hosts.lock().await;
                            guard
                                .get(&host_id)
                                .and_then(|h| h.clients.get(&session_id).cloned())
                        };
                        if let Some(tx) = client_tx {
                            let outbound = if kind == "binary" {
                                match STANDARD.decode(data.as_bytes()) {
                                    Ok(bytes) => OutboundClientMsg::Binary(bytes),
                                    Err(_) => continue,
                                }
                            } else {
                                OutboundClientMsg::Text(data)
                            };
                            if tx.try_send(outbound).is_err() {
                                // Drop slow client without blocking host.
                                let _ = tx.try_send(OutboundClientMsg::Close(
                                    1011,
                                    "backpressure_limit".into(),
                                ));
                            }
                        }
                    }
                    RelayControlMessage::SessionClose { session_id, reason } => {
                        let client_tx = {
                            let mut guard = state.hosts.lock().await;
                            guard
                                .get_mut(&host_id)
                                .and_then(|h| h.clients.remove(&session_id))
                        };
                        if let Some(tx) = client_tx {
                            let _ = tx.try_send(OutboundClientMsg::Close(
                                1000,
                                reason.unwrap_or_else(|| "host_closed".into()),
                            ));
                        }
                    }
                    RelayControlMessage::Push { mut request } => {
                        request.host_id = host_id.clone();
                        let state_push = state.clone();
                        tokio::spawn(async move {
                            let _ = crate::push::dispatch_push(&state_push, &request).await;
                        });
                    }
                    RelayControlMessage::Ping { ts } => {
                        if let Ok(pong) = serde_json::to_string(&RelayControlMessage::Pong { ts }) {
                            let _ = host_tx.try_send(pong);
                        }
                    }
                    RelayControlMessage::Pong { .. } | RelayControlMessage::SessionOpen { .. } => {}
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    writer.abort();

    let mut guard = state.hosts.lock().await;
    if guard
        .get(&host_id)
        .is_some_and(|entry| entry.generation == generation)
        && let Some(removed) = guard.remove(&host_id)
    {
        for (_, client_tx) in removed.clients {
            let _ = client_tx.try_send(OutboundClientMsg::Close(1012, "host_disconnected".into()));
        }
    }
}

pub async fn client_ws(
    Path(host_id): Path<String>,
    ws: WebSocketUpgrade,
    State(state): State<Arc<RelayState>>,
) -> Response {
    if !valid_host_id(&host_id) {
        return (StatusCode::BAD_REQUEST, "invalid host_id").into_response();
    }
    {
        let guard = state.hosts.lock().await;
        let Some(host) = guard.get(&host_id) else {
            return (StatusCode::NOT_FOUND, "host_offline").into_response();
        };
        if host.clients.len() >= state.config.max_clients_per_host {
            return (StatusCode::TOO_MANY_REQUESTS, "too_many_clients").into_response();
        }
    }
    let max_frame = state.config.max_frame_bytes;
    ws.max_message_size(max_frame)
        .on_upgrade(move |socket| run_client_socket(socket, host_id, state))
}

async fn run_client_socket(socket: WebSocket, host_id: String, state: Arc<RelayState>) {
    let session_id = random_session_id();
    let (client_tx, mut client_rx) = mpsc::channel::<OutboundClientMsg>(OUTBOUND_QUEUE_DEPTH);

    let host_tx = {
        let mut guard = state.hosts.lock().await;
        let Some(host) = guard.get_mut(&host_id) else {
            return;
        };
        if host.clients.len() >= state.config.max_clients_per_host {
            return;
        }
        host.clients.insert(session_id.clone(), client_tx);
        host.host_tx.clone()
    };

    if let Ok(open_msg) = serde_json::to_string(&RelayControlMessage::SessionOpen {
        session_id: session_id.clone(),
    }) {
        let _ = host_tx.send(open_msg).await;
    }

    let (mut ws_sink, mut ws_stream) = socket.split();
    let writer = tokio::spawn(async move {
        while let Some(msg) = client_rx.recv().await {
            let res = match msg {
                OutboundClientMsg::Text(t) => ws_sink.send(Message::Text(t.into())).await,
                OutboundClientMsg::Binary(b) => ws_sink.send(Message::Binary(b.into())).await,
                OutboundClientMsg::Close(code, reason) => {
                    let _ = ws_sink
                        .send(Message::Close(Some(CloseFrame {
                            code,
                            reason: reason.into(),
                        })))
                        .await;
                    break;
                }
            };
            if res.is_err() {
                break;
            }
        }
        let _ = ws_sink.close().await;
    });

    while let Some(Ok(msg)) = ws_stream.next().await {
        let ctrl = match msg {
            Message::Text(text) => RelayControlMessage::Frame {
                session_id: session_id.clone(),
                kind: "text".into(),
                data: text.to_string(),
            },
            Message::Binary(bytes) => RelayControlMessage::Frame {
                session_id: session_id.clone(),
                kind: "binary".into(),
                data: STANDARD.encode(&bytes),
            },
            Message::Close(_) => break,
            _ => continue,
        };
        if let Ok(encoded) = serde_json::to_string(&ctrl)
            && host_tx.send(encoded).await.is_err()
        {
            break;
        }
    }

    writer.abort();
    {
        let mut guard = state.hosts.lock().await;
        if let Some(host) = guard.get_mut(&host_id) {
            host.clients.remove(&session_id);
        }
    }
    if let Ok(close_msg) = serde_json::to_string(&RelayControlMessage::SessionClose {
        session_id,
        reason: Some("client_disconnected".into()),
    }) {
        let _ = host_tx.try_send(close_msg);
    }
}
