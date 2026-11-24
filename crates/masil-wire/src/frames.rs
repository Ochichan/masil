//! Binary application frames transported inside encrypted `RecordKind::Binary` records.
//!
//! Wire layout: `opcode(1 byte) || channel_id(u16 BE) || payload`.

use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum BinaryOpcode {
    TerminalOutput = 0x01,
    TerminalInput = 0x02,
    TerminalResize = 0x03,
    TerminalSnapshot = 0x04,
    AudioChunk = 0x10,
    AudioEnd = 0x11,
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
