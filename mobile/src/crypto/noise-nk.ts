/**
 * Textbook Noise Revision 34 `Noise_NK_25519_ChaChaPoly_SHA256` client
 * matching `crates/masil-wire/src/noise.rs` bit-for-bit.
 */
import { x25519 } from "@noble/curves/ed25519.js";
import { chacha20poly1305 } from "@noble/ciphers/chacha.js";
import { hmac } from "@noble/hashes/hmac.js";
import { sha256 } from "@noble/hashes/sha2.js";

export const KEY_LEN = 32;
export const TAG_LEN = 16;
export const NOISE_PROTOCOL_NAME = new TextEncoder().encode(
  "Noise_NK_25519_ChaChaPoly_SHA256"
);
export const NOISE_PROLOGUE = new TextEncoder().encode("masil-remote-e2ee/1");
export const HANDSHAKE_PARAM_VERSION = 1;
export const FLAG_BINARY_CIPHERTEXT = 0x01;
const KNOWN_PARAM_FLAGS = FLAG_BINARY_CIPHERTEXT;
const MAX_NONCE = (1n << 64n) - 1n;

export interface Keypair {
  secret: Uint8Array;
  public: Uint8Array;
}

export interface HandshakeParams {
  version: number;
  flags: number;
}

export function createHandshakeParams(binaryCiphertext: boolean): HandshakeParams {
  return {
    version: HANDSHAKE_PARAM_VERSION,
    flags: binaryCiphertext ? FLAG_BINARY_CIPHERTEXT : 0,
  };
}

export function decodeHandshakeParams(bytes: Uint8Array): HandshakeParams {
  if (bytes.length !== 2) {
    throw new Error("invalid Noise handshake params length");
  }
  const version = bytes[0]!;
  const flags = bytes[1]!;
  if (version !== HANDSHAKE_PARAM_VERSION || (flags & ~KNOWN_PARAM_FLAGS) !== 0) {
    throw new Error(`unsupported Noise params version=${version} flags=${flags}`);
  }
  return { version, flags };
}

export function generateKeypair(): Keypair {
  const secret = x25519.utils.randomSecretKey();
  return keypairFromSecret(secret);
}

export function keypairFromSecret(secret: Uint8Array): Keypair {
  if (secret.length !== KEY_LEN) {
    throw new Error("X25519 secret must be 32 bytes");
  }
  const pub = x25519.getPublicKey(secret);
  return { secret: new Uint8Array(secret), public: new Uint8Array(pub) };
}

function concat(...parts: Uint8Array[]): Uint8Array {
  let total = 0;
  for (const p of parts) total += p.length;
  const out = new Uint8Array(total);
  let off = 0;
  for (const p of parts) {
    out.set(p, off);
    off += p.length;
  }
  return out;
}

function hkdf2(ck: Uint8Array, ikm: Uint8Array): [Uint8Array, Uint8Array] {
  const tempKey = hmac(sha256, ck, ikm);
  const out1 = hmac(sha256, tempKey, new Uint8Array([0x01]));
  const out2 = hmac(sha256, tempKey, concat(out1, new Uint8Array([0x02])));
  return [out1, out2];
}

function dh(secret: Uint8Array, pub: Uint8Array): Uint8Array {
  const shared = x25519.getSharedSecret(secret, pub);
  let allZero = true;
  for (let i = 0; i < shared.length; i++) {
    if (shared[i] !== 0) {
      allZero = false;
      break;
    }
  }
  if (allZero) {
    throw new Error("low-order X25519 public key rejected");
  }
  return shared;
}

export class CipherState {
  private readonly key: Uint8Array;
  private nonceValue = 0n;

  constructor(key: Uint8Array) {
    this.key = new Uint8Array(key);
  }

  get nonce(): bigint {
    return this.nonceValue;
  }

  private makeNonce(n: bigint): Uint8Array {
    const buf = new Uint8Array(12);
    const view = new DataView(buf.buffer);
    view.setBigUint64(4, n, true);
    return buf;
  }

  encryptWithAd(ad: Uint8Array, plaintext: Uint8Array): Uint8Array {
    if (this.nonceValue === MAX_NONCE) {
      throw new Error("transport nonce exhausted");
    }
    const nonce = this.makeNonce(this.nonceValue);
    const ct = chacha20poly1305(this.key, nonce, ad).encrypt(plaintext);
    this.nonceValue += 1n;
    return ct;
  }

  decryptWithAd(ad: Uint8Array, ciphertext: Uint8Array): Uint8Array {
    if (this.nonceValue === MAX_NONCE) {
      throw new Error("transport nonce exhausted");
    }
    if (ciphertext.length < TAG_LEN) {
      throw new Error("ciphertext shorter than AEAD tag");
    }
    const nonce = this.makeNonce(this.nonceValue);
    const pt = chacha20poly1305(this.key, nonce, ad).decrypt(ciphertext);
    // Advance only after AEAD authentication succeeds.
    this.nonceValue += 1n;
    return pt;
  }

  zeroize(): void {
    this.key.fill(0);
  }
}

class SymmetricState {
  private ck: Uint8Array;
  private h: Uint8Array;
  private cipher: CipherState | null = null;

  constructor(responderStaticPub: Uint8Array) {
    if (responderStaticPub.length !== KEY_LEN) {
      throw new Error("responder static public key must be 32 bytes");
    }
    let h: Uint8Array;
    if (NOISE_PROTOCOL_NAME.length <= KEY_LEN) {
      h = new Uint8Array(KEY_LEN);
      h.set(NOISE_PROTOCOL_NAME, 0);
    } else {
      h = sha256(NOISE_PROTOCOL_NAME);
    }
    this.h = h;
    this.ck = new Uint8Array(h);
    this.mixHash(NOISE_PROLOGUE);
    // NK pre-message: <- s
    this.mixHash(responderStaticPub);
  }

  mixHash(data: Uint8Array): void {
    this.h = sha256(concat(this.h, data));
  }

  mixKey(ikm: Uint8Array): void {
    const [ck, tempK] = hkdf2(this.ck, ikm);
    this.ck = ck;
    this.cipher = new CipherState(tempK);
  }

  encryptAndHash(plaintext: Uint8Array): Uint8Array {
    const out = this.cipher
      ? this.cipher.encryptWithAd(this.h, plaintext)
      : new Uint8Array(plaintext);
    this.mixHash(out);
    return out;
  }

  decryptAndHash(ciphertext: Uint8Array): Uint8Array {
    const out = this.cipher
      ? this.cipher.decryptWithAd(this.h, ciphertext)
      : new Uint8Array(ciphertext);
    this.mixHash(ciphertext);
    return out;
  }

  split(): { k1: CipherState; k2: CipherState; hash: Uint8Array } {
    const [raw1, raw2] = hkdf2(this.ck, new Uint8Array(0));
    return {
      k1: new CipherState(raw1),
      k2: new CipherState(raw2),
      hash: new Uint8Array(this.h),
    };
  }
}

export class NoiseTransport {
  readonly send: CipherState;
  readonly recv: CipherState;
  readonly handshakeHash: Uint8Array;
  readonly params: HandshakeParams;

  constructor(
    send: CipherState,
    recv: CipherState,
    handshakeHash: Uint8Array,
    params: HandshakeParams
  ) {
    this.send = send;
    this.recv = recv;
    this.handshakeHash = handshakeHash;
    this.params = params;
  }

  encrypt(plaintext: Uint8Array): Uint8Array {
    return this.send.encryptWithAd(new Uint8Array(0), plaintext);
  }

  decrypt(ciphertext: Uint8Array): Uint8Array {
    return this.recv.decryptWithAd(new Uint8Array(0), ciphertext);
  }

  close(): void {
    this.send.zeroize();
    this.recv.zeroize();
  }
}

export class InitiatorHandshake {
  private readonly sym: SymmetricState;
  private readonly ephemeral: Keypair;
  private readonly offered: HandshakeParams;

  private constructor(
    sym: SymmetricState,
    ephemeral: Keypair,
    offered: HandshakeParams
  ) {
    this.sym = sym;
    this.ephemeral = ephemeral;
    this.offered = offered;
  }

  static start(
    responderStaticPub: Uint8Array,
    offered: HandshakeParams = createHandshakeParams(true),
    ephemeral: Keypair = generateKeypair()
  ): { handshake: InitiatorHandshake; msg1: Uint8Array } {
    const sym = new SymmetricState(responderStaticPub);
    sym.mixHash(ephemeral.public);
    const es = dh(ephemeral.secret, responderStaticPub);
    sym.mixKey(es);
    const ct = sym.encryptAndHash(new Uint8Array([offered.version, offered.flags]));
    const msg1 = concat(ephemeral.public, ct);
    return {
      handshake: new InitiatorHandshake(sym, ephemeral, offered),
      msg1,
    };
  }

  finish(msg2: Uint8Array): NoiseTransport {
    if (msg2.length < KEY_LEN + TAG_LEN) {
      throw new Error("Noise msg2 too short");
    }
    const rePub = msg2.subarray(0, KEY_LEN);
    this.sym.mixHash(rePub);
    const ee = dh(this.ephemeral.secret, rePub);
    this.sym.mixKey(ee);
    const payload = this.sym.decryptAndHash(msg2.subarray(KEY_LEN));
    const selected = decodeHandshakeParams(payload);
    if ((selected.flags & ~this.offered.flags) !== 0) {
      throw new Error("responder selected unoffered Noise flags");
    }
    const { k1, k2, hash } = this.sym.split();
    return new NoiseTransport(k1, k2, hash, selected);
  }
}
