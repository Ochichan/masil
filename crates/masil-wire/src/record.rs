//! Chunked, replay-protected application record framing over Noise transport.
//!
//! Each application message becomes one wire record made of one or more
//! length-prefixed Noise transport ciphertexts:
//!
//! ```text
//! chunk_plaintext = kind(1) || flags(1) || total_len(u32 BE) || chunk_data
//! wire_record     = (u16 BE ciphertext_len || ciphertext)+
//! ```

use crate::noise::{NoiseError, TAG_LEN, TransportState};
use std::fmt;

pub const MAX_CHUNK_DATA: usize = 65_513;
pub const CHUNK_HEADER_LEN: usize = 6;
pub const MAX_RECORD_BYTES: usize = 4 * 1024 * 1024;
const FLAG_FIRST: u8 = 0x01;
const FLAG_LAST: u8 = 0x02;
const KNOWN_FLAGS: u8 = FLAG_FIRST | FLAG_LAST;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RecordKind {
    Text = 0x00,
    Binary = 0x01,
}

impl RecordKind {
    fn decode(byte: u8) -> Result<Self, RecordError> {
        match byte {
            0x00 => Ok(Self::Text),
            0x01 => Ok(Self::Binary),
            _ => Err(RecordError::InvalidKind(byte)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordError {
    PayloadTooLarge(usize),
    EmptyRecord,
    TruncatedFrame,
    TrailingBytes,
    InvalidKind(u8),
    InvalidFlags(u8),
    MetadataMismatch,
    NonFinalShortChunk,
    LengthMismatch,
    Noise(NoiseError),
}

impl fmt::Display for RecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PayloadTooLarge(n) => write!(f, "record payload {n} exceeds {MAX_RECORD_BYTES}"),
            Self::EmptyRecord => f.write_str("wire record contains zero chunks"),
            Self::TruncatedFrame => f.write_str("wire record truncated before declared length"),
            Self::TrailingBytes => f.write_str("wire record has bytes after final chunk"),
            Self::InvalidKind(k) => write!(f, "invalid record kind {k:#04x}"),
            Self::InvalidFlags(fl) => write!(f, "invalid record flags {fl:#04x}"),
            Self::MetadataMismatch => f.write_str("record chunk metadata disagreed"),
            Self::NonFinalShortChunk => f.write_str("non-final record chunk was not full"),
            Self::LengthMismatch => f.write_str("assembled record length disagreed with header"),
            Self::Noise(e) => write!(f, "record crypto error: {e}"),
        }
    }
}

impl std::error::Error for RecordError {}

impl From<NoiseError> for RecordError {
    fn from(value: NoiseError) -> Self {
        Self::Noise(value)
    }
}

pub fn seal_record(
    transport: &mut TransportState,
    kind: RecordKind,
    payload: &[u8],
) -> Result<Vec<u8>, RecordError> {
    if payload.len() > MAX_RECORD_BYTES {
        return Err(RecordError::PayloadTooLarge(payload.len()));
    }
    let total_len = payload.len() as u32;
    let chunk_count = if payload.is_empty() {
        1
    } else {
        payload.len().div_ceil(MAX_CHUNK_DATA)
    };

    let mut wire =
        Vec::with_capacity(payload.len() + chunk_count * (2 + CHUNK_HEADER_LEN + TAG_LEN));
    for idx in 0..chunk_count {
        let start = idx * MAX_CHUNK_DATA;
        let end = ((idx + 1) * MAX_CHUNK_DATA).min(payload.len());
        let data = &payload[start..end];
        let mut flags = 0u8;
        if idx == 0 {
            flags |= FLAG_FIRST;
        }
        if idx + 1 == chunk_count {
            flags |= FLAG_LAST;
        }
        let mut plain = Vec::with_capacity(CHUNK_HEADER_LEN + data.len());
        plain.push(kind as u8);
        plain.push(flags);
        plain.extend_from_slice(&total_len.to_be_bytes());
        plain.extend_from_slice(data);

        let ct = transport.encrypt(&plain)?;
        let ct_len = u16::try_from(ct.len()).map_err(|_| RecordError::PayloadTooLarge(ct.len()))?;
        wire.extend_from_slice(&ct_len.to_be_bytes());
        wire.extend_from_slice(&ct);
    }
    Ok(wire)
}

pub fn open_record(
    transport: &mut TransportState,
    wire: &[u8],
) -> Result<(RecordKind, Vec<u8>), RecordError> {
    if wire.is_empty() {
        return Err(RecordError::EmptyRecord);
    }
    // Pre-scan frame lengths before allocating output buffer.
    let mut scan = 0usize;
    let mut chunks = Vec::new();
    while scan < wire.len() {
        if wire.len() - scan < 2 {
            return Err(RecordError::TruncatedFrame);
        }
        let ct_len = u16::from_be_bytes([wire[scan], wire[scan + 1]]) as usize;
        scan += 2;
        if ct_len < CHUNK_HEADER_LEN + TAG_LEN || wire.len() - scan < ct_len {
            return Err(RecordError::TruncatedFrame);
        }
        chunks.push(&wire[scan..scan + ct_len]);
        scan += ct_len;
    }
    if chunks.is_empty() {
        return Err(RecordError::EmptyRecord);
    }

    let mut expected_kind: Option<RecordKind> = None;
    let mut expected_total: Option<usize> = None;
    let mut output: Vec<u8> = Vec::new();
    let mut saw_last = false;

    for (idx, ct) in chunks.iter().enumerate() {
        if saw_last {
            return Err(RecordError::TrailingBytes);
        }
        let plain = transport.decrypt(ct)?;
        if plain.len() < CHUNK_HEADER_LEN {
            return Err(RecordError::TruncatedFrame);
        }
        let kind = RecordKind::decode(plain[0])?;
        let flags = plain[1];
        if (flags & !KNOWN_FLAGS) != 0 {
            return Err(RecordError::InvalidFlags(flags));
        }
        let is_first = (flags & FLAG_FIRST) != 0;
        let is_last = (flags & FLAG_LAST) != 0;
        if (idx == 0) != is_first {
            return Err(RecordError::InvalidFlags(flags));
        }
        let total = u32::from_be_bytes([plain[2], plain[3], plain[4], plain[5]]) as usize;
        if total > MAX_RECORD_BYTES {
            return Err(RecordError::PayloadTooLarge(total));
        }
        let data = &plain[CHUNK_HEADER_LEN..];
        if data.len() > MAX_CHUNK_DATA {
            return Err(RecordError::PayloadTooLarge(data.len()));
        }
        if !is_last && data.len() != MAX_CHUNK_DATA {
            return Err(RecordError::NonFinalShortChunk);
        }

        match (expected_kind, expected_total) {
            (None, None) => {
                // Validate declared total against remaining received bytes before reserving.
                if total > wire.len() {
                    return Err(RecordError::LengthMismatch);
                }
                expected_kind = Some(kind);
                expected_total = Some(total);
                output = Vec::with_capacity(total);
            }
            (Some(ek), Some(et)) => {
                if ek != kind || et != total {
                    return Err(RecordError::MetadataMismatch);
                }
            }
            _ => unreachable!(),
        }
        if output.len().saturating_add(data.len()) > expected_total.unwrap_or(0) {
            return Err(RecordError::LengthMismatch);
        }
        output.extend_from_slice(data);
        if is_last {
            saw_last = true;
        }
    }

    if !saw_last {
        return Err(RecordError::TruncatedFrame);
    }
    let Some(kind) = expected_kind else {
        return Err(RecordError::EmptyRecord);
    };
    if output.len() != expected_total.unwrap_or(0) {
        return Err(RecordError::LengthMismatch);
    }
    Ok((kind, output))
}
