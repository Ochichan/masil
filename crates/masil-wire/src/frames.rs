//! Binary application frames transported inside encrypted `RecordKind::Binary` records.
//!
//! Wire layout: `opcode(1 byte) || channel_id(u16 BE) || payload`.

use serde::{Deserialize, Serialize};
use std::fmt;

pub const TUNNEL_MAX_CHUNK_BYTES: usize = 32 * 1024;
pub const TUNNEL_INITIAL_WINDOW_BYTES: u32 = 256 * 1024;
pub const ASSET_MAX_UPLOAD_BYTES: usize = 10 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum BinaryOpcode {
    TerminalOutput = 0x01,
    TerminalInput = 0x02,
    TerminalResize = 0x03,
    TerminalSnapshot = 0x04,
    AudioChunk = 0x10,
    AudioEnd = 0x11,
    TunnelOpen = 0x20,
    TunnelOpenAck = 0x21,
    TunnelData = 0x22,
    TunnelClose = 0x23,
    TunnelWindow = 0x24,
    AssetUploadChunk = 0x30,
    AssetUploadEnd = 0x31,
}

impl BinaryOpcode {
    pub fn decode(byte: u8) -> Result<Self, FrameError> {
        match byte {
            0x01 => Ok(Self::TerminalOutput),
            0x02 => Ok(Self::TerminalInput),
            0x03 => Ok(Self::TerminalResize),
            0x04 => Ok(Self::TerminalSnapshot),
            0x10 => Ok(Self::AudioChunk),
            0x11 => Ok(Self::AudioEnd),
            0x20 => Ok(Self::TunnelOpen),
            0x21 => Ok(Self::TunnelOpenAck),
            0x22 => Ok(Self::TunnelData),
            0x23 => Ok(Self::TunnelClose),
            0x24 => Ok(Self::TunnelWindow),
            0x30 => Ok(Self::AssetUploadChunk),
            0x31 => Ok(Self::AssetUploadEnd),
            _ => Err(FrameError::UnknownOpcode(byte)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    TooShort,
    UnknownOpcode(u8),
    InvalidHeader(String),
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooShort => f.write_str("binary frame shorter than 3-byte header"),
            Self::UnknownOpcode(op) => write!(f, "unknown binary opcode {op:#04x}"),
            Self::InvalidHeader(msg) => write!(f, "invalid binary frame payload header: {msg}"),
        }
    }
}

impl std::error::Error for FrameError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryFrame {
    pub opcode: BinaryOpcode,
    pub channel_id: u16,
    pub payload: Vec<u8>,
}

impl BinaryFrame {
    pub fn new(opcode: BinaryOpcode, channel_id: u16, payload: Vec<u8>) -> Self {
        Self {
            opcode,
            channel_id,
            payload,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(3 + self.payload.len());
        out.push(self.opcode as u8);
        out.extend_from_slice(&self.channel_id.to_be_bytes());
        out.extend_from_slice(&self.payload);
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, FrameError> {
        if bytes.len() < 3 {
            return Err(FrameError::TooShort);
        }
        let opcode = BinaryOpcode::decode(bytes[0])?;
        let channel_id = u16::from_be_bytes([bytes[1], bytes[2]]);
        Ok(Self {
            opcode,
            channel_id,
            payload: bytes[3..].to_vec(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResizeMode {
    Desktop,
    Fit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalResizePayload {
    pub cols: u16,
    pub rows: u16,
    pub mode: ResizeMode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalSnapshotMeta {
    pub cols: u16,
    pub rows: u16,
    pub cursor_x: u16,
    pub cursor_y: u16,
    pub desktop_cols: u16,
    pub desktop_rows: u16,
    pub fit_active: bool,
}

impl TerminalSnapshotMeta {
    pub fn encode_with_ansi(&self, ansi: &[u8]) -> Result<Vec<u8>, FrameError> {
        let meta_bytes =
            serde_json::to_vec(self).map_err(|e| FrameError::InvalidHeader(e.to_string()))?;
        let meta_len = u32::try_from(meta_bytes.len())
            .map_err(|_| FrameError::InvalidHeader("snapshot meta exceeds u32".into()))?;
        let mut out = Vec::with_capacity(4 + meta_bytes.len() + ansi.len());
        out.extend_from_slice(&meta_len.to_be_bytes());
        out.extend_from_slice(&meta_bytes);
        out.extend_from_slice(ansi);
        Ok(out)
    }

    pub fn decode_with_ansi(payload: &[u8]) -> Result<(Self, &[u8]), FrameError> {
        if payload.len() < 4 {
            return Err(FrameError::TooShort);
        }
        let meta_len =
            u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]) as usize;
        if payload.len() < 4 + meta_len {
            return Err(FrameError::InvalidHeader("truncated snapshot meta".into()));
        }
        let meta: Self = serde_json::from_slice(&payload[4..4 + meta_len])
            .map_err(|e| FrameError::InvalidHeader(e.to_string()))?;
        Ok((meta, &payload[4 + meta_len..]))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioEndPayload {
    pub mime: String,
    pub target_agent: Option<String>,
    pub mode: String,
}

/// Phase 2A (`0x20`): Mobile requests an E2EE loopback TCP stream to `127.0.0.1:<target_port>`
/// (or `[::1]:<target_port>`). Hostnames and non-loopback targets are impossible by construction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TunnelOpenPayload {
    pub target_port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
}

/// Phase 2A (`0x21`): Host acknowledges a loopback tunnel stream open with initial flow-control window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TunnelOpenAckPayload {
    pub ok: bool,
    pub initial_window: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Phase 2A (`0x23`): Either side gracefully closes a loopback tunnel stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TunnelClosePayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Phase 2A (`0x24`): Receive window credit replenishment (`u32 BE`, 4 bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TunnelWindowPayload {
    pub credit: u32,
}

impl TunnelWindowPayload {
    pub fn encode(self) -> [u8; 4] {
        self.credit.to_be_bytes()
    }

    pub fn decode(payload: &[u8]) -> Result<Self, FrameError> {
        if payload.len() != 4 {
            return Err(FrameError::InvalidHeader(format!(
                "tunnel window must be 4 bytes, got {}",
                payload.len()
            )));
        }
        Ok(Self {
            credit: u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]),
        })
    }
}

/// Delivery policy for annotated mobile screenshots (D-08).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AssetDeliveryMode {
    Send,
    #[default]
    Queue,
    Prefill,
}

/// Optional browser/viewport diagnostics attached to a visual markup upload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AssetUploadMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub viewport: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_socket: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub console_errors: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub network_errors: Vec<String>,
    #[serde(default)]
    pub annotations_count: u32,
}

/// Phase 2A (`0x31`): Completes a chunked asset upload (`0x30`) and triggers host save + delivery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetUploadEndPayload {
    pub agent_id: String,
    pub mime: String,
    pub filename: String,
    #[serde(default)]
    pub delivery_mode: AssetDeliveryMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default)]
    pub meta: AssetUploadMeta,
}
