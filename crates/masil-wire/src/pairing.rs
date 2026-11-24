//! QR pairing URI (`masil://pair?v=1&...`) encoding and strict validation.

use crate::noise::KEY_LEN;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingOffer {
    pub version: u8,
    pub host_id: String,
    pub host_pub: [u8; KEY_LEN],
    pub pair_token: String,
    pub relay_url: Option<String>,
    pub direct_addr: Option<String>,
    pub host_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairingError {
    InvalidScheme,
    UnsupportedVersion(String),
    MissingField(&'static str),
    InvalidPublicKey,
    NoEndpoint,
    InvalidPercentEncoding,
}

impl fmt::Display for PairingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidScheme => f.write_str("pairing URI must start with masil://pair?"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported pairing URI version: {v}"),
            Self::MissingField(field) => write!(f, "missing pairing field: {field}"),
            Self::InvalidPublicKey => f.write_str("host public key must be 32-byte base64url"),
            Self::NoEndpoint => f.write_str("pairing URI requires relay or direct endpoint"),
            Self::InvalidPercentEncoding => f.write_str("invalid percent encoding in pairing URI"),
        }
    }
}

impl std::error::Error for PairingError {}

fn pct_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        let safe = byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~');
        if safe {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push(hex_digit(byte >> 4));
            out.push(hex_digit(byte & 0x0f));
        }
    }
    out
}

fn hex_digit(v: u8) -> char {
    match v {
        0..=9 => (b'0' + v) as char,
        10..=15 => (b'A' + (v - 10)) as char,
        _ => unreachable!(),
    }
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn pct_decode(input: &str) -> Result<String, PairingError> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return Err(PairingError::InvalidPercentEncoding);
            }
            let hi = hex_val(bytes[i + 1]).ok_or(PairingError::InvalidPercentEncoding)?;
            let lo = hex_val(bytes[i + 2]).ok_or(PairingError::InvalidPercentEncoding)?;
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| PairingError::InvalidPercentEncoding)
}

impl PairingOffer {
    pub fn to_uri(&self) -> Result<String, PairingError> {
        if self.relay_url.is_none() && self.direct_addr.is_none() {
            return Err(PairingError::NoEndpoint);
        }
        let pub_b64 = URL_SAFE_NO_PAD.encode(self.host_pub);
        let mut parts = vec![
            format!("v={}", self.version),
            format!("host={}", pct_encode(&self.host_id)),
            format!("pub={pub_b64}"),
            format!("pair={}", pct_encode(&self.pair_token)),
            format!("name={}", pct_encode(&self.host_name)),
        ];
        if let Some(ref relay) = self.relay_url {
            parts.push(format!("relay={}", pct_encode(relay)));
        }
        if let Some(ref direct) = self.direct_addr {
            parts.push(format!("direct={}", pct_encode(direct)));
        }
        Ok(format!("masil://pair?{}", parts.join("&")))
    }

    pub fn parse_uri(uri: &str) -> Result<Self, PairingError> {
        let query = uri
            .trim()
            .strip_prefix("masil://pair?")
            .ok_or(PairingError::InvalidScheme)?;
        let mut version: Option<u8> = None;
        let mut host_id: Option<String> = None;
        let mut host_pub: Option<[u8; KEY_LEN]> = None;
        let mut pair_token: Option<String> = None;
        let mut relay_url: Option<String> = None;
        let mut direct_addr: Option<String> = None;
        let mut host_name: Option<String> = None;

        for pair in query.split('&') {
            if pair.is_empty() {
                continue;
            }
            let Some((k, v)) = pair.split_once('=') else {
                continue;
            };
            let decoded = pct_decode(v)?;
            match k {
                "v" => {
                    let parsed = decoded
                        .parse::<u8>()
                        .map_err(|_| PairingError::UnsupportedVersion(decoded.clone()))?;
                    if parsed != crate::PROTOCOL_VERSION {
                        return Err(PairingError::UnsupportedVersion(decoded));
                    }
                    version = Some(parsed);
                }
                "host" if !decoded.is_empty() => host_id = Some(decoded),
                "pub" => {
                    let raw = URL_SAFE_NO_PAD
                        .decode(decoded.as_bytes())
                        .map_err(|_| PairingError::InvalidPublicKey)?;
                    if raw.len() != KEY_LEN {
                        return Err(PairingError::InvalidPublicKey);
                    }
                    let mut arr = [0u8; KEY_LEN];
                    arr.copy_from_slice(&raw);
                    host_pub = Some(arr);
                }
                "pair" if !decoded.is_empty() => pair_token = Some(decoded),
                "relay" if !decoded.is_empty() => relay_url = Some(decoded),
                "direct" if !decoded.is_empty() => direct_addr = Some(decoded),
                "name" if !decoded.is_empty() => host_name = Some(decoded),
                _ => {}
            }
        }

        let version = version.ok_or(PairingError::MissingField("v"))?;
        let host_id = host_id.ok_or(PairingError::MissingField("host"))?;
        let host_pub = host_pub.ok_or(PairingError::MissingField("pub"))?;
        let pair_token = pair_token.ok_or(PairingError::MissingField("pair"))?;
        let host_name = host_name.unwrap_or_else(|| host_id.clone());
        if relay_url.is_none() && direct_addr.is_none() {
            return Err(PairingError::NoEndpoint);
        }
        Ok(Self {
            version,
            host_id,
            host_pub,
            pair_token,
            relay_url,
            direct_addr,
            host_name,
        })
    }
}
