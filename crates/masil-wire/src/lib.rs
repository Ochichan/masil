//! Shared wire protocol, Noise NK E2EE, chunked record framing, binary frames,
//! RPC envelopes, and QR pairing URI codecs for `masil-agent`, `masil-relay`,
//! and cross-language test vectors consumed by `mobile/`.

pub mod frames;
pub mod noise;
pub mod pairing;
pub mod record;
pub mod rpc;

pub const PROTOCOL_VERSION: u8 = 1;
pub const NOISE_PROTOCOL_NAME: &[u8] = b"Noise_NK_25519_ChaChaPoly_SHA256";
pub const NOISE_PROLOGUE: &[u8] = b"masil-remote-e2ee/1";
