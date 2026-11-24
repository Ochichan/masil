//! Live terminal byte streaming over `masil -C` (pipe-safe tmux control mode) without
//! modifying the C core. Preserves desktop PTY size by default via
//! `refresh-client -f ignore-size`, resizing only when the mobile user
//! explicitly enables `fit` mode.

use crate::native_ui::native_executable;
use masil_wire::frames::{
    BinaryFrame, BinaryOpcode, ResizeMode, TerminalResizePayload, TerminalSnapshotMeta,
};
use std::collections::HashMap;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;

pub struct TerminalSubscription {
    pub pane_id: String,
    stdin: ChildStdin,
    child: Child,
    fit_active: bool,
}

impl TerminalSubscription {
    pub async fn resize(&mut self, req: &TerminalResizePayload) -> Result<(), String> {
        let cmd = match req.mode {
            ResizeMode::Fit => {
                let cols = req.cols.clamp(24, 300);
                let rows = req.rows.clamp(8, 120);
                self.fit_active = true;
                format!("refresh-client -f !ignore-size\nrefresh-client -C {cols}x{rows}\n")
            }
            ResizeMode::Desktop => {
                self.fit_active = false;
                "refresh-client -f ignore-size\n".to_owned()
            }
        };
        self.stdin
            .write_all(cmd.as_bytes())
            .await
            .map_err(|e| format!("control-mode resize write failed: {e}"))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| format!("control-mode resize flush failed: {e}"))
    }

    pub async fn send_input(&mut self, bytes: &[u8]) -> Result<(), String> {
        if bytes.is_empty() {
            return Ok(());
        }
        // Send in 1 KiB chunks via hex-encoded `send-keys -H` so UTF-8, Esc,
        // Ctrl-C, and control sequences cross cleanly without shell quoting.
        for chunk in bytes.chunks(1024) {
            let mut line = format!("send-keys -H -t {}", self.pane_id);
            for b in chunk {
                use std::fmt::Write;
                let _ = write!(&mut line, " {b:02x}");
            }
            line.push('\n');
            self.stdin
                .write_all(line.as_bytes())
                .await
                .map_err(|e| format!("send-keys write failed: {e}"))?;
        }
        self.stdin
            .flush()
            .await
            .map_err(|e| format!("send-keys flush failed: {e}"))
    }

    pub async fn close(mut self) {
        if self.fit_active {
            let _ = self
                .stdin
                .write_all(b"refresh-client -f ignore-size\n")
                .await;
        }
        let _ = self.stdin.write_all(b"detach-client\n").await;
        let _ = self.stdin.flush().await;
        let _ = self.child.kill().await;
    }
}

pub struct TerminalMux {
    subs: HashMap<u16, TerminalSubscription>,
}

impl TerminalMux {
    pub fn new() -> Self {
        Self {
            subs: HashMap::new(),
        }
    }

    pub async fn attach(
        &mut self,
        channel_id: u16,
        server_socket: String,
        pane_id: String,
        outbound_tx: mpsc::Sender<BinaryFrame>,
    ) -> Result<TerminalSnapshotMeta, String> {
        if let Some(prev) = self.subs.remove(&channel_id) {
            prev.close().await;
        }
        let masil = native_executable()?;
        let (session_name, cols, rows, cursor_x, cursor_y) =
            query_pane_geometry(&masil, &server_socket, &pane_id).await?;
        let ansi = capture_pane_ansi(&masil, &server_socket, &pane_id).await?;

        let meta = TerminalSnapshotMeta {
            cols,
            rows,
            cursor_x,
            cursor_y,
            desktop_cols: cols,
            desktop_rows: rows,
            fit_active: false,
        };
        let snap_payload = meta.encode_with_ansi(&ansi).map_err(|e| e.to_string())?;
        let _ = outbound_tx
            .send(BinaryFrame::new(
                BinaryOpcode::TerminalSnapshot,
                channel_id,
                snap_payload,
            ))
            .await;

        let mut child = Command::new(&masil)
            .env_remove("TMUX")
            .arg("-u")
            .arg("-S")
            .arg(&server_socket)
            .arg("-C")
            .arg("attach-session")
            .arg("-f")
            .arg("ignore-size,pause-after=2")
            .arg("-t")
            .arg(&session_name)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("failed launching masil -C: {e}"))?;

        let stdin = child.stdin.take().ok_or("masil -C stdin unavailable")?;
        let stdout = child.stdout.take().ok_or("masil -C stdout unavailable")?;

        let target_pane = pane_id.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(escaped) = extract_control_output(&line, &target_pane) {
                    let bytes = unescape_tmux_octal(escaped);
                    if !bytes.is_empty()
                        && outbound_tx
                            .send(BinaryFrame::new(
                                BinaryOpcode::TerminalOutput,
                                channel_id,
                                bytes,
                            ))
                            .await
                            .is_err()
                    {
                        break;
                    }
                }
            }
        });

        self.subs.insert(
            channel_id,
            TerminalSubscription {
                pane_id,
                stdin,
                child,
                fit_active: false,
            },
        );
        Ok(meta)
    }

    pub async fn detach(&mut self, channel_id: u16) {
        if let Some(sub) = self.subs.remove(&channel_id) {
            sub.close().await;
        }
    }

    pub async fn handle_input(&mut self, channel_id: u16, bytes: &[u8]) -> Result<(), String> {
        let sub = self
            .subs
            .get_mut(&channel_id)
            .ok_or("terminal channel not attached")?;
        sub.send_input(bytes).await
    }

    pub async fn handle_resize(
        &mut self,
        channel_id: u16,
        req: &TerminalResizePayload,
    ) -> Result<(), String> {
        let sub = self
            .subs
            .get_mut(&channel_id)
            .ok_or("terminal channel not attached")?;
        sub.resize(req).await
    }

    pub async fn close_all(&mut self) {
        let all: Vec<_> = self.subs.drain().map(|(_, s)| s).collect();
        for sub in all {
            sub.close().await;
        }
    }
}

async fn query_pane_geometry(
    masil: &std::path::Path,
    socket: &str,
    pane_id: &str,
) -> Result<(String, u16, u16, u16, u16), String> {
    let out = Command::new(masil)
        .arg("-S")
        .arg(socket)
        .args([
            "display-message",
            "-p",
            "-t",
            pane_id,
            "#{session_name}\t#{pane_width}\t#{pane_height}\t#{cursor_x}\t#{cursor_y}",
        ])
        .output()
        .await
        .map_err(|e| format!("failed querying pane geometry: {e}"))?;
    if !out.status.success() {
        return Err(format!("pane {pane_id} not found on server"));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let parts: Vec<&str> = text.trim().split('\t').collect();
    if parts.len() != 5 {
        return Err("unexpected pane geometry format".into());
    }
    let session = parts[0].to_owned();
    let cols = parts[1].parse::<u16>().unwrap_or(80);
    let rows = parts[2].parse::<u16>().unwrap_or(24);
    let cx = parts[3].parse::<u16>().unwrap_or(0);
    let cy = parts[4].parse::<u16>().unwrap_or(0);
    Ok((session, cols, rows, cx, cy))
}

async fn capture_pane_ansi(
    masil: &std::path::Path,
    socket: &str,
    pane_id: &str,
) -> Result<Vec<u8>, String> {
    let out = Command::new(masil)
        .arg("-S")
        .arg(socket)
        .args([
            "capture-pane",
            "-e",
            "-p",
            "-J",
            "-S",
            "-500",
            "-t",
            pane_id,
        ])
        .output()
        .await
        .map_err(|e| format!("capture-pane failed: {e}"))?;
    if !out.status.success() {
        return Err("capture-pane returned non-zero".into());
    }
    // Convert bare LF to CRLF for xterm replay fidelity.
    let mut normalized = Vec::with_capacity(out.stdout.len() + 256);
    for &b in &out.stdout {
        if b == b'\n' {
            normalized.push(b'\r');
        }
        normalized.push(b);
    }
    Ok(normalized)
}

/// Extracts the octal-escaped payload from either `%output <pane> <value>` or
/// `%extended-output <pane> <age> [extra...] : <value>`.
pub fn extract_control_output<'a>(line: &'a str, target_pane: &str) -> Option<&'a str> {
    if let Some(rest) = line.strip_prefix("%output ") {
        let (pane, payload) = rest.split_once(' ')?;
        return (pane == target_pane).then_some(payload);
    }
    if let Some(rest) = line.strip_prefix("%extended-output ") {
        let (pane, after_pane) = rest.split_once(' ')?;
        if pane != target_pane {
            return None;
        }
        let (_, payload) = after_pane.split_once(" : ")?;
        return Some(payload);
    }
    None
}

/// Decodes tmux control mode `%output` octal escapes (`\015`, `\033`, `\\`, etc.).
pub fn unescape_tmux_octal(input: &str) -> Vec<u8> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() {
            let (a, b, c) = (bytes[i + 1], bytes[i + 2], bytes[i + 3]);
            if (b'0'..=b'3').contains(&a)
                && (b'0'..=b'7').contains(&b)
                && (b'0'..=b'7').contains(&c)
            {
                let val = ((a - b'0') << 6) | ((b - b'0') << 3) | (c - b'0');
                out.push(val);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unescapes_tmux_octal_sequences() {
        let decoded = unescape_tmux_octal(r"hello\033[32m\015\012\134world");
        assert_eq!(decoded, b"hello\x1b[32m\r\n\\world");
    }

    #[test]
    fn parses_output_and_extended_output_lines() {
        assert_eq!(
            extract_control_output(r"%output %3 hi\015\012", "%3"),
            Some(r"hi\015\012")
        );
        assert_eq!(
            extract_control_output(r"%extended-output %3 12 : OK\015\012", "%3"),
            Some(r"OK\015\012")
        );
        assert_eq!(
            extract_control_output(r"%extended-output %9 0 : other", "%3"),
            None
        );
    }
}
