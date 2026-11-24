//! Textbook Noise Revision 34 `Noise_NK_25519_ChaChaPoly_SHA256` handshake and
//! transport cipher states with monotonic 64-bit nonce replay protection.

use crate::{NOISE_PROLOGUE, NOISE_PROTOCOL_NAME};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use hmac::{Hmac, Mac};
use rand_core::{OsRng, RngCore};
use sha2::{Digest, Sha256};
use std::fmt;
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::{Zeroize, ZeroizeOnDrop};

type HmacSha256 = Hmac<Sha256>;

pub const KEY_LEN: usize = 32;
pub const TAG_LEN: usize = 16;
pub const HANDSHAKE_PARAM_VERSION: u8 = 1;
pub const FLAG_BINARY_CIPHERTEXT: u8 = 0x01;
pub const KNOWN_PARAM_FLAGS: u8 = FLAG_BINARY_CIPHERTEXT;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoiseError {
    InvalidLength,
    InvalidSharedSecret,
    CryptoFailure,
    NonceExhausted,
    UnsupportedParams(u8, u8),
    UnexpectedSelection,
}

impl fmt::Display for NoiseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLength => f.write_str("invalid Noise message or key length"),
            Self::InvalidSharedSecret => f.write_str("low-order X25519 public key rejected"),
            Self::CryptoFailure => f.write_str("AEAD authentication or encryption failed"),
            Self::NonceExhausted => f.write_str("transport nonce counter exhausted"),
            Self::UnsupportedParams(v, flags) => {
                write!(f, "unsupported Noise params version={v} flags={flags:#04x}")
            }
            Self::UnexpectedSelection => f.write_str("responder selected unoffered flags"),
        }
    }
}

impl std::error::Error for NoiseError {}

#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct Keypair {
    secret: [u8; KEY_LEN],
    #[zeroize(skip)]
    public: [u8; KEY_LEN],
}

impl Keypair {
    pub fn generate() -> Self {
        let mut secret = [0u8; KEY_LEN];
        OsRng.fill_bytes(&mut secret);
        Self::from_secret_bytes(secret)
    }

    pub fn from_secret_bytes(secret: [u8; KEY_LEN]) -> Self {
        let sk = StaticSecret::from(secret);
        let pk = PublicKey::from(&sk);
        Self {
            secret: sk.to_bytes(),
            public: pk.to_bytes(),
        }
    }

    pub fn secret_bytes(&self) -> &[u8; KEY_LEN] {
        &self.secret
    }

    pub fn public_bytes(&self) -> &[u8; KEY_LEN] {
        &self.public
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HandshakeParams {
    pub version: u8,
    pub flags: u8,
}

impl HandshakeParams {
    pub const fn new(binary_ciphertext: bool) -> Self {
        Self {
            version: HANDSHAKE_PARAM_VERSION,
            flags: if binary_ciphertext {
                FLAG_BINARY_CIPHERTEXT
            } else {
                0
            },
        }
    }

    pub const fn encode(self) -> [u8; 2] {
        [self.version, self.flags]
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, NoiseError> {
        if bytes.len() != 2 {
            return Err(NoiseError::InvalidLength);
        }
        let version = bytes[0];
        let flags = bytes[1];
        if version != HANDSHAKE_PARAM_VERSION || (flags & !KNOWN_PARAM_FLAGS) != 0 {
            return Err(NoiseError::UnsupportedParams(version, flags));
        }
        Ok(Self { version, flags })
    }

    pub const fn binary_ciphertext(self) -> bool {
        (self.flags & FLAG_BINARY_CIPHERTEXT) != 0
    }
}

#[derive(Zeroize, ZeroizeOnDrop)]
pub struct CipherState {
    key: [u8; KEY_LEN],
    nonce: u64,
}

impl CipherState {
    fn new(key: [u8; KEY_LEN]) -> Self {
        Self { key, nonce: 0 }
    }

    pub fn nonce(&self) -> u64 {
        self.nonce
    }

    fn make_nonce(n: u64) -> [u8; 12] {
        let mut out = [0u8; 12];
        out[4..12].copy_from_slice(&n.to_le_bytes());
        out
    }

    pub fn encrypt_with_ad(&mut self, ad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, NoiseError> {
        if self.nonce == u64::MAX {
            return Err(NoiseError::NonceExhausted);
        }
        let cipher = ChaCha20Poly1305::new((&self.key).into());
        let nonce_bytes = Self::make_nonce(self.nonce);
        let ct = cipher
            .encrypt(
                Nonce::from_slice(&nonce_bytes),
                Payload {
                    msg: plaintext,
                    aad: ad,
                },
            )
            .map_err(|_| NoiseError::CryptoFailure)?;
        self.nonce += 1;
        Ok(ct)
    }

    pub fn decrypt_with_ad(&mut self, ad: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, NoiseError> {
        if self.nonce == u64::MAX {
            return Err(NoiseError::NonceExhausted);
        }
        if ciphertext.len() < TAG_LEN {
            return Err(NoiseError::InvalidLength);
        }
        let cipher = ChaCha20Poly1305::new((&self.key).into());
        let nonce_bytes = Self::make_nonce(self.nonce);
        let pt = cipher
            .decrypt(
                Nonce::from_slice(&nonce_bytes),
                Payload {
                    msg: ciphertext,
                    aad: ad,
                },
            )
            .map_err(|_| NoiseError::CryptoFailure)?;
        // Advance nonce only after authentication succeeds.
        self.nonce += 1;
        Ok(pt)
    }
}

fn sha256(parts: &[&[u8]]) -> [u8; KEY_LEN] {
    let mut hasher = Sha256::new();
    for p in parts {
        hasher.update(p);
    }
    let digest = hasher.finalize();
    let mut out = [0u8; KEY_LEN];
    out.copy_from_slice(&digest);
    out
}

fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> [u8; KEY_LEN] {
    let mut mac = <HmacSha256 as Mac>::new_from_slice(key).expect("HMAC accepts any key length");
    for p in parts {
        mac.update(p);
    }
    let result = mac.finalize().into_bytes();
    let mut out = [0u8; KEY_LEN];
    out.copy_from_slice(&result);
    out
}

fn hkdf2(chaining_key: &[u8; KEY_LEN], ikm: &[u8]) -> ([u8; KEY_LEN], [u8; KEY_LEN]) {
    let temp_key = hmac_sha256(chaining_key, &[ikm]);
    let output1 = hmac_sha256(&temp_key, &[&[0x01]]);
    let output2 = hmac_sha256(&temp_key, &[&output1, &[0x02]]);
    (output1, output2)
}

fn dh(secret: &[u8; KEY_LEN], public: &[u8; KEY_LEN]) -> Result<[u8; KEY_LEN], NoiseError> {
    let sk = StaticSecret::from(*secret);
    let pk = PublicKey::from(*public);
    let shared = sk.diffie_hellman(&pk).to_bytes();
    if shared.iter().all(|&b| b == 0) {
        return Err(NoiseError::InvalidSharedSecret);
    }
    Ok(shared)
}

#[derive(Zeroize, ZeroizeOnDrop)]
struct SymmetricState {
    ck: [u8; KEY_LEN],
    h: [u8; KEY_LEN],
    cipher: Option<CipherState>,
}

impl SymmetricState {
    fn initialize(
        protocol_name: &[u8],
        prologue: &[u8],
        responder_static_pub: &[u8; KEY_LEN],
    ) -> Self {
        let mut h = [0u8; KEY_LEN];
        if protocol_name.len() <= KEY_LEN {
            h[..protocol_name.len()].copy_from_slice(protocol_name);
        } else {
            h = sha256(&[protocol_name]);
        }
        let mut state = Self {
            ck: h,
            h,
            cipher: None,
        };
        state.mix_hash(prologue);
        // NK pre-message: <- s
        state.mix_hash(responder_static_pub);
        state
    }

    fn mix_hash(&mut self, data: &[u8]) {
        self.h = sha256(&[&self.h, data]);
    }

    fn mix_key(&mut self, ikm: &[u8]) {
        let (ck, temp_k) = hkdf2(&self.ck, ikm);
        self.ck = ck;
        self.cipher = Some(CipherState::new(temp_k));
    }

    fn encrypt_and_hash(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, NoiseError> {
        let out = match self.cipher.as_mut() {
            Some(c) => c.encrypt_with_ad(&self.h, plaintext)?,
            None => plaintext.to_vec(),
        };
        self.mix_hash(&out);
        Ok(out)
    }

    fn decrypt_and_hash(&mut self, ciphertext: &[u8]) -> Result<Vec<u8>, NoiseError> {
        let out = match self.cipher.as_mut() {
            Some(c) => c.decrypt_with_ad(&self.h, ciphertext)?,
            None => ciphertext.to_vec(),
        };
        self.mix_hash(ciphertext);
        Ok(out)
    }

    fn split(self) -> (CipherState, CipherState, [u8; KEY_LEN]) {
        let (k1, k2) = hkdf2(&self.ck, &[]);
        (CipherState::new(k1), CipherState::new(k2), self.h)
    }
}

pub struct TransportState {
    pub send: CipherState,
    pub recv: CipherState,
    pub handshake_hash: [u8; KEY_LEN],
    pub params: HandshakeParams,
}

impl TransportState {
    pub fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, NoiseError> {
        self.send.encrypt_with_ad(&[], plaintext)
    }

    pub fn decrypt(&mut self, ciphertext: &[u8]) -> Result<Vec<u8>, NoiseError> {
        self.recv.decrypt_with_ad(&[], ciphertext)
    }
}

pub struct InitiatorHandshake {
    sym: SymmetricState,
    ephemeral: Keypair,
    offered: HandshakeParams,
}

impl InitiatorHandshake {
    pub fn start(
        responder_static_pub: &[u8; KEY_LEN],
        offered: HandshakeParams,
    ) -> Result<(Self, Vec<u8>), NoiseError> {
        Self::start_with_ephemeral(responder_static_pub, Keypair::generate(), offered)
    }

    pub fn start_with_ephemeral(
        responder_static_pub: &[u8; KEY_LEN],
        ephemeral: Keypair,
        offered: HandshakeParams,
    ) -> Result<(Self, Vec<u8>), NoiseError> {
        let mut sym =
            SymmetricState::initialize(NOISE_PROTOCOL_NAME, NOISE_PROLOGUE, responder_static_pub);
        // -> e
        let e_pub = *ephemeral.public_bytes();
        sym.mix_hash(&e_pub);
        // -> es
        let es = dh(ephemeral.secret_bytes(), responder_static_pub)?;
        sym.mix_key(&es);
        let ct = sym.encrypt_and_hash(&offered.encode())?;
        let mut msg1 = Vec::with_capacity(KEY_LEN + ct.len());
        msg1.extend_from_slice(&e_pub);
        msg1.extend_from_slice(&ct);
        Ok((
            Self {
                sym,
                ephemeral,
                offered,
            },
            msg1,
        ))
    }

    pub fn finish(mut self, msg2: &[u8]) -> Result<TransportState, NoiseError> {
        if msg2.len() < KEY_LEN + TAG_LEN {
            return Err(NoiseError::InvalidLength);
        }
        let mut re_pub = [0u8; KEY_LEN];
        re_pub.copy_from_slice(&msg2[..KEY_LEN]);
        // <- e
        self.sym.mix_hash(&re_pub);
        // <- ee
        let ee = dh(self.ephemeral.secret_bytes(), &re_pub)?;
        self.sym.mix_key(&ee);
        let payload = self.sym.decrypt_and_hash(&msg2[KEY_LEN..])?;
        let selected = HandshakeParams::decode(&payload)?;
        if (selected.flags & !self.offered.flags) != 0 {
            return Err(NoiseError::UnexpectedSelection);
        }
        let (send, recv, handshake_hash) = self.sym.split();
        Ok(TransportState {
            send,
            recv,
            handshake_hash,
            params: selected,
        })
    }
}

pub struct ResponderHandshake;

impl ResponderHandshake {
    pub fn accept(
        responder_static: &Keypair,
        msg1: &[u8],
        allow_binary: bool,
    ) -> Result<(TransportState, Vec<u8>), NoiseError> {
        Self::accept_with_ephemeral(responder_static, Keypair::generate(), msg1, allow_binary)
    }

    pub fn accept_with_ephemeral(
        responder_static: &Keypair,
        ephemeral: Keypair,
        msg1: &[u8],
        allow_binary: bool,
    ) -> Result<(TransportState, Vec<u8>), NoiseError> {
        if msg1.len() < KEY_LEN + TAG_LEN {
            return Err(NoiseError::InvalidLength);
        }
        let mut sym = SymmetricState::initialize(
            NOISE_PROTOCOL_NAME,
            NOISE_PROLOGUE,
            responder_static.public_bytes(),
        );
        let mut ie_pub = [0u8; KEY_LEN];
        ie_pub.copy_from_slice(&msg1[..KEY_LEN]);
        // -> e
        sym.mix_hash(&ie_pub);
        // -> es
        let es = dh(responder_static.secret_bytes(), &ie_pub)?;
        sym.mix_key(&es);
        let payload = sym.decrypt_and_hash(&msg1[KEY_LEN..])?;
        let offered = HandshakeParams::decode(&payload)?;

        let selected = HandshakeParams {
            version: HANDSHAKE_PARAM_VERSION,
            flags: if allow_binary && offered.binary_ciphertext() {
                FLAG_BINARY_CIPHERTEXT
            } else {
                0
            },
        };

        // <- e
        let re_pub = *ephemeral.public_bytes();
        sym.mix_hash(&re_pub);
        // <- ee
        let ee = dh(ephemeral.secret_bytes(), &ie_pub)?;
        sym.mix_key(&ee);
        let ct = sym.encrypt_and_hash(&selected.encode())?;
        let mut msg2 = Vec::with_capacity(KEY_LEN + ct.len());
        msg2.extend_from_slice(&re_pub);
        msg2.extend_from_slice(&ct);

        let (recv, send, handshake_hash) = sym.split();
        Ok((
            TransportState {
                send,
                recv,
                handshake_hash,
                params: selected,
            },
            msg2,
        ))
    }
}
