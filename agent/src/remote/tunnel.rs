//! Phase 2A (D-07): E2EE loopback-only TCP tunnel multiplexer for direct mobile
//! `localhost` preview (HTTP, HMR WebSockets, SSE, ES modules).
//!
//! Security & resource invariants (D-07):
//! - Connections are hardcoded exclusively to `127.0.0.1:<port>` (with `[::1]:<port>`
//!   fallback when IPv4 is refused). External or LAN SSRF is impossible by construction.
//! - Port `0` and every port in `TunnelConfig::denied_ports` (default SSH/DB/masil remote
//!   ports) are rejected with `TunnelOpenAck { ok: false, error: Some("port_denied: ...") }`.
//! - Concurrent stream cap (`max_streams_per_client`, default 32) and idle timeout
//!   (`idle_timeout_secs`, default 300s) prevent socket exhaustion.
//! - Bidirectional receive-window flow control (`TUNNEL_INITIAL_WINDOW_BYTES = 256 KiB`,
//!   chunk size `<= TUNNEL_MAX_CHUNK_BYTES = 32 KiB`) bounds memory usage on slow links.

use crate::remote::config::TunnelConfig;
use masil_wire::frames::{
    BinaryFrame, BinaryOpcode, TUNNEL_INITIAL_WINDOW_BYTES, TUNNEL_MAX_CHUNK_BYTES,
    TunnelClosePayload, TunnelOpenAckPayload, TunnelOpenPayload, TunnelWindowPayload,
};
use std::collections::HashMap;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, Notify, mpsc};

const WRITE_QUEUE_DEPTH: usize = 64;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

struct ActiveStream {
    write_tx: mpsc::Sender<Vec<u8>>,
    send_window: Arc<AtomicU64>,
    window_notify: Arc<Notify>,
    last_active: Arc<Mutex<Instant>>,
    reader_task: tokio::task::JoinHandle<()>,
    writer_task: tokio::task::JoinHandle<()>,
    idle_task: tokio::task::JoinHandle<()>,
}

impl ActiveStream {
    fn abort(&self) {
        self.reader_task.abort();
        self.writer_task.abort();
        self.idle_task.abort();
    }
}

pub struct TunnelMux {
    config: TunnelConfig,
    streams: Arc<Mutex<HashMap<u16, ActiveStream>>>,
}

impl TunnelMux {
    pub fn new(config: TunnelConfig) -> Self {
        Self {
            config,
            streams: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    #[cfg(test)]
    pub async fn active_count(&self) -> usize {
        self.streams.lock().await.len()
    }

    /// Handles `BinaryOpcode::TunnelOpen (0x20)`.
    pub async fn handle_open(
        &self,
        stream_id: u16,
        req: TunnelOpenPayload,
        bin_tx: mpsc::Sender<BinaryFrame>,
    ) {
        if self.config.is_port_denied(req.target_port) {
            send_open_ack(
                &bin_tx,
                stream_id,
                TunnelOpenAckPayload {
                    ok: false,
                    initial_window: 0,
                    error: Some(format!(
                        "port_denied: port {} is blocked by tunnel policy",
                        req.target_port
                    )),
                },
            )
            .await;
            return;
        }

        {
            let guard = self.streams.lock().await;
            if guard.len() >= self.config.max_streams_per_client && !guard.contains_key(&stream_id)
            {
                drop(guard);
                send_open_ack(
                    &bin_tx,
                    stream_id,
                    TunnelOpenAckPayload {
                        ok: false,
                        initial_window: 0,
                        error: Some(format!(
                            "max_streams_exceeded: limit {}",
                            self.config.max_streams_per_client
                        )),
                    },
                )
                .await;
                return;
            }
        }

        // Connect strictly to loopback (`127.0.0.1` first, `[::1]` fallback).
        let tcp = match connect_loopback(req.target_port).await {
            Ok(s) => s,
            Err(err) => {
                send_open_ack(
                    &bin_tx,
                    stream_id,
                    TunnelOpenAckPayload {
                        ok: false,
                        initial_window: 0,
                        error: Some(format!("connect_failed: {err}")),
                    },
                )
                .await;
                return;
            }
        };
        let _ = tcp.set_nodelay(true);

        // Close any stale stream sharing `stream_id`.
        if let Some(prev) = self.streams.lock().await.remove(&stream_id) {
            prev.abort();
        }

        let (mut reader, mut writer) = tcp.into_split();
        let (write_tx, mut write_rx) = mpsc::channel::<Vec<u8>>(WRITE_QUEUE_DEPTH);
        let send_window = Arc::new(AtomicU64::new(u64::from(TUNNEL_INITIAL_WINDOW_BYTES)));
        let window_notify = Arc::new(Notify::new());
        let last_active = Arc::new(Mutex::new(Instant::now()));

        // Acknowledge open BEFORE spawning reader so mobile always sees `TunnelOpenAck`
        // ahead of the first `TunnelData` frame.
        send_open_ack(
            &bin_tx,
            stream_id,
            TunnelOpenAckPayload {
                ok: true,
                initial_window: TUNNEL_INITIAL_WINDOW_BYTES,
                error: None,
            },
        )
        .await;

        let streams_reader = self.streams.clone();
        let bin_reader = bin_tx.clone();
        let win_reader = send_window.clone();
        let notify_reader = window_notify.clone();
        let active_reader = last_active.clone();
        let reader_task = tokio::spawn(async move {
            let mut buf = vec![0u8; TUNNEL_MAX_CHUNK_BYTES];
            let close_reason = loop {
                // Respect mobile's receive window before reading more bytes from loopback.
                loop {
                    let avail = win_reader.load(Ordering::Acquire);
                    if avail > 0 {
                        break;
                    }
                    notify_reader.notified().await;
                }
                let max_read =
                    (win_reader.load(Ordering::Acquire) as usize).clamp(1, TUNNEL_MAX_CHUNK_BYTES);
                match reader.read(&mut buf[..max_read]).await {
                    Ok(0) => break Some("eof".to_owned()),
                    Ok(n) => {
                        *active_reader.lock().await = Instant::now();
                        win_reader.fetch_sub(n as u64, Ordering::AcqRel);
                        let frame = BinaryFrame::new(
                            BinaryOpcode::TunnelData,
                            stream_id,
                            buf[..n].to_vec(),
                        );
                        if bin_reader.send(frame).await.is_err() {
                            break Some("session_closed".to_owned());
                        }
                    }
                    Err(e) => break Some(format!("read_error: {e}")),
                }
            };
            send_close(&bin_reader, stream_id, close_reason).await;
            if let Some(removed) = streams_reader.lock().await.remove(&stream_id) {
                removed.writer_task.abort();
                removed.idle_task.abort();
            }
        });

        let streams_writer = self.streams.clone();
        let bin_writer = bin_tx.clone();
        let active_writer = last_active.clone();
        let writer_task = tokio::spawn(async move {
            let mut pending_credit: u32 = 0;
            while let Some(chunk) = write_rx.recv().await {
                *active_writer.lock().await = Instant::now();
                if let Err(e) = writer.write_all(&chunk).await {
                    send_close(&bin_writer, stream_id, Some(format!("write_error: {e}"))).await;
                    if let Some(removed) = streams_writer.lock().await.remove(&stream_id) {
                        removed.reader_task.abort();
                        removed.idle_task.abort();
                    }
                    return;
                }
                pending_credit = pending_credit.saturating_add(chunk.len() as u32);
                // Batch window updates once >= 32 KiB drained or channel emptied.
                if pending_credit >= TUNNEL_MAX_CHUNK_BYTES as u32 || write_rx.is_empty() {
                    let frame = BinaryFrame::new(
                        BinaryOpcode::TunnelWindow,
                        stream_id,
                        TunnelWindowPayload {
                            credit: pending_credit,
                        }
                        .encode()
                        .to_vec(),
                    );
                    let _ = bin_writer.send(frame).await;
                    pending_credit = 0;
                }
            }
            let _ = writer.shutdown().await;
        });

        let idle_timeout = Duration::from_secs(self.config.idle_timeout_secs.max(5));
        let streams_idle = self.streams.clone();
        let bin_idle = bin_tx;
        let active_idle = last_active.clone();
        let idle_task = tokio::spawn(async move {
            let check_every = Duration::from_secs(10)
                .min(idle_timeout / 2)
                .max(Duration::from_secs(1));
            loop {
                tokio::time::sleep(check_every).await;
                let elapsed = active_idle.lock().await.elapsed();
                if elapsed >= idle_timeout {
                    send_close(&bin_idle, stream_id, Some("idle_timeout".into())).await;
                    if let Some(removed) = streams_idle.lock().await.remove(&stream_id) {
                        removed.reader_task.abort();
                        removed.writer_task.abort();
                    }
                    break;
                }
            }
        });

        self.streams.lock().await.insert(
            stream_id,
            ActiveStream {
                write_tx,
                send_window,
                window_notify,
                last_active,
                reader_task,
                writer_task,
                idle_task,
            },
        );
    }

    /// Handles `BinaryOpcode::TunnelData (0x22)` from mobile -> host loopback socket.
    pub async fn handle_data(&self, stream_id: u16, bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }
        let target = {
            let guard = self.streams.lock().await;
            guard
                .get(&stream_id)
                .map(|s| (s.write_tx.clone(), s.last_active.clone()))
        };
        if let Some((tx, active)) = target {
            *active.lock().await = Instant::now();
            let _ = tx.send(bytes).await;
        }
    }

    /// Handles `BinaryOpcode::TunnelWindow (0x24)` credit replenishment from mobile.
    pub async fn handle_window(&self, stream_id: u16, credit: u32) {
        if credit == 0 {
            return;
        }
        let target = {
            let guard = self.streams.lock().await;
            guard
                .get(&stream_id)
                .map(|s| (s.send_window.clone(), s.window_notify.clone()))
        };
        if let Some((win, notify)) = target {
            win.fetch_add(u64::from(credit), Ordering::AcqRel);
            notify.notify_waiters();
        }
    }

    /// Handles `BinaryOpcode::TunnelClose (0x23)` from mobile.
    pub async fn handle_close(&self, stream_id: u16) {
        if let Some(removed) = self.streams.lock().await.remove(&stream_id) {
            removed.abort();
        }
    }

    pub async fn close_all(&self) {
        let mut guard = self.streams.lock().await;
        for (_, stream) in guard.drain() {
            stream.abort();
        }
    }
}

/// Strictly dials loopback (`127.0.0.1` then `[::1]`) — never DNS, never external IPs.
async fn connect_loopback(port: u16) -> Result<TcpStream, String> {
    let v4 = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port));
    match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(v4)).await {
        Ok(Ok(stream)) => Ok(stream),
        Ok(Err(e4)) => {
            let v6 = SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::LOCALHOST, port, 0, 0));
            match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(v6)).await {
                Ok(Ok(stream)) => Ok(stream),
                Ok(Err(_)) => Err(e4.to_string()),
                Err(_) => Err("loopback connect timed out".into()),
            }
        }
        Err(_) => Err("loopback connect timed out".into()),
    }
}

async fn send_open_ack(
    bin_tx: &mpsc::Sender<BinaryFrame>,
    stream_id: u16,
    ack: TunnelOpenAckPayload,
) {
    if let Ok(payload) = serde_json::to_vec(&ack) {
        let _ = bin_tx
            .send(BinaryFrame::new(
                BinaryOpcode::TunnelOpenAck,
                stream_id,
                payload,
            ))
            .await;
    }
}

async fn send_close(bin_tx: &mpsc::Sender<BinaryFrame>, stream_id: u16, reason: Option<String>) {
    if let Ok(payload) = serde_json::to_vec(&TunnelClosePayload { reason }) {
        let _ = bin_tx
            .send(BinaryFrame::new(
                BinaryOpcode::TunnelClose,
                stream_id,
                payload,
            ))
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn denies_sensitive_ports_and_zero_port() {
        let mux = TunnelMux::new(TunnelConfig::default());
        let (tx, mut rx) = mpsc::channel(16);

        for &port in &[0u16, 22, 5432, 6379, 7767, 27017] {
            mux.handle_open(
                1,
                TunnelOpenPayload {
                    target_port: port,
                    agent_id: None,
                },
                tx.clone(),
            )
            .await;
            let frame = rx.recv().await.unwrap();
            assert_eq!(frame.opcode, BinaryOpcode::TunnelOpenAck);
            let ack: TunnelOpenAckPayload = serde_json::from_slice(&frame.payload).unwrap();
            assert!(!ack.ok, "port {port} must be denied");
            assert!(
                ack.error
                    .as_deref()
                    .unwrap_or("")
                    .starts_with("port_denied"),
                "expected port_denied for {port}, got {:?}",
                ack.error
            );
        }
        assert_eq!(mux.active_count().await, 0);
    }

    #[tokio::test]
    async fn tunnels_http_request_and_response_over_loopback() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut req_buf = [0u8; 512];
            let n = sock.read(&mut req_buf).await.unwrap();
            let req = String::from_utf8_lossy(&req_buf[..n]);
            assert!(req.starts_with("GET /hello HTTP/1.1"));
            let body = "hello-from-masil-loopback";
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            let _ = sock.shutdown().await;
        });

        let mux = TunnelMux::new(TunnelConfig::default());
        let (tx, mut rx) = mpsc::channel(32);

        mux.handle_open(
            11,
            TunnelOpenPayload {
                target_port: port,
                agent_id: Some("default::%1".into()),
            },
            tx.clone(),
        )
        .await;

        let ack_frame = rx.recv().await.unwrap();
        assert_eq!(ack_frame.opcode, BinaryOpcode::TunnelOpenAck);
        assert_eq!(ack_frame.channel_id, 11);
        let ack: TunnelOpenAckPayload = serde_json::from_slice(&ack_frame.payload).unwrap();
        assert!(ack.ok);
        assert_eq!(ack.initial_window, TUNNEL_INITIAL_WINDOW_BYTES);

        mux.handle_data(
            11,
            b"GET /hello HTTP/1.1\r\nHost: localhost\r\n\r\n".to_vec(),
        )
        .await;

        let mut received = Vec::new();
        let mut saw_window = false;
        let mut saw_close = false;
        while let Ok(Some(frame)) = tokio::time::timeout(Duration::from_secs(3), rx.recv()).await {
            match frame.opcode {
                BinaryOpcode::TunnelData => received.extend_from_slice(&frame.payload),
                BinaryOpcode::TunnelWindow => {
                    let w = TunnelWindowPayload::decode(&frame.payload).unwrap();
                    assert!(w.credit > 0);
                    saw_window = true;
                }
                BinaryOpcode::TunnelClose => {
                    saw_close = true;
                    break;
                }
                _ => {}
            }
        }
        let text = String::from_utf8_lossy(&received);
        assert!(text.contains("HTTP/1.1 200 OK"), "got {text}");
        assert!(text.contains("hello-from-masil-loopback"), "got {text}");
        assert!(saw_window);
        assert!(saw_close);
        assert_eq!(mux.active_count().await, 0);
    }

    #[tokio::test]
    async fn enforces_max_concurrent_streams_cap() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let accept_task = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((sock, _)) = listener.accept().await {
                held.push(sock);
            }
        });

        let cfg = TunnelConfig {
            max_streams_per_client: 2,
            ..TunnelConfig::default()
        };
        let mux = TunnelMux::new(cfg);
        let (tx, mut rx) = mpsc::channel(16);

        for sid in [1u16, 2u16] {
            mux.handle_open(
                sid,
                TunnelOpenPayload {
                    target_port: port,
                    agent_id: None,
                },
                tx.clone(),
            )
            .await;
            let ack: TunnelOpenAckPayload =
                serde_json::from_slice(&rx.recv().await.unwrap().payload).unwrap();
            assert!(ack.ok);
        }

        // Third concurrent stream must be rejected.
        mux.handle_open(
            3,
            TunnelOpenPayload {
                target_port: port,
                agent_id: None,
            },
            tx.clone(),
        )
        .await;
        let ack: TunnelOpenAckPayload =
            serde_json::from_slice(&rx.recv().await.unwrap().payload).unwrap();
        assert!(!ack.ok);
        assert!(
            ack.error
                .as_deref()
                .unwrap_or("")
                .starts_with("max_streams_exceeded")
        );

        mux.close_all().await;
        accept_task.abort();
    }
}
