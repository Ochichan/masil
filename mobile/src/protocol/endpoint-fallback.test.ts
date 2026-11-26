import { chacha20poly1305 } from "@noble/ciphers/chacha.js";
import { x25519 } from "@noble/curves/ed25519.js";
import { hmac } from "@noble/hashes/hmac.js";
import { sha256 } from "@noble/hashes/sha2.js";
import { WebSocketServer } from "ws";
import { describe, expect, it } from "vitest";
import {
  CipherState,
  KEY_LEN,
  NOISE_PROLOGUE,
  NOISE_PROTOCOL_NAME,
  NoiseTransport,
  createHandshakeParams,
  decodeHandshakeParams,
} from "../crypto/noise-nk";
import { base64ToBytes, bytesToBase64, parsePairingUri } from "../crypto/pairing";
import { RecordKind, openRecord, sealRecord } from "../crypto/records";
import { MasilRemoteClient } from "./client";
import {
  buildEndpointUrls,
  classifyEndpointRoute,
  isPrivateLanHost,
  isTailscaleHost,
  mergeAdvertisedDirectAddrs,
} from "./endpoints";

const encoder = new TextEncoder();
const decoder = new TextDecoder();

function concatBytes(...chunks: Uint8Array[]): Uint8Array {
  const total = chunks.reduce((acc, c) => acc + c.length, 0);
  const out = new Uint8Array(total);
  let offset = 0;
  for (const c of chunks) {
    out.set(c, offset);
    offset += c.length;
  }
  return out;
}

function nonce12(n: number): Uint8Array {
  const out = new Uint8Array(12);
  const view = new DataView(out.buffer);
  view.setUint32(4, n >>> 0, true);
  view.setUint32(8, Math.floor(n / 0x1_0000_0000) >>> 0, true);
  return out;
}

function hkdf2(chainingKey: Uint8Array, ikm: Uint8Array): [Uint8Array, Uint8Array] {
  const tempKey = hmac(sha256, chainingKey, ikm);
  const out1 = hmac(sha256, tempKey, new Uint8Array([0x01]));
  const out2 = hmac(sha256, tempKey, concatBytes(out1, new Uint8Array([0x02])));
  return [out1, out2];
}

/**
 * Minimal Noise NK responder for verifying live multi-endpoint fallback in Node.
 */
function acceptNoiseNkResponder(
  responderPriv: Uint8Array,
  responderPub: Uint8Array,
  msg1: Uint8Array
): { transport: NoiseTransport; msg2: Uint8Array } {
  let ck: Uint8Array =
    NOISE_PROTOCOL_NAME.length <= 32
      ? new Uint8Array(32)
      : sha256(NOISE_PROTOCOL_NAME);
  if (NOISE_PROTOCOL_NAME.length <= 32) ck.set(NOISE_PROTOCOL_NAME);
  let h = sha256(concatBytes(ck, NOISE_PROLOGUE));
  h = sha256(concatBytes(h, responderPub));

  const re = msg1.slice(0, KEY_LEN);
  h = sha256(concatBytes(h, re));
  const es = x25519.getSharedSecret(responderPriv, re);
  let k: Uint8Array;
  [ck, k] = hkdf2(ck, es);
  const ct1 = msg1.slice(KEY_LEN);
  const clientParams = decodeHandshakeParams(
    chacha20poly1305(k, nonce12(0), h).decrypt(ct1)
  );
  h = sha256(concatBytes(h, ct1));

  const ePriv = x25519.utils.randomSecretKey();
  const ePub = x25519.getPublicKey(ePriv);
  h = sha256(concatBytes(h, ePub));
  const ee = x25519.getSharedSecret(ePriv, re);
  [ck, k] = hkdf2(ck, ee);

  const serverParams = createHandshakeParams((clientParams.flags & 0x01) !== 0);
  const serverParamBytes = new Uint8Array([
    serverParams.version,
    serverParams.flags,
  ]);
  const ct2 = chacha20poly1305(k, nonce12(0), h).encrypt(serverParamBytes);
  h = sha256(concatBytes(h, ct2));
  const msg2 = concatBytes(ePub, ct2);

  const [c1, c2] = hkdf2(ck, new Uint8Array(0));
  const transport = new NoiseTransport(
    new CipherState(c2),
    new CipherState(c1),
    h,
    serverParams
  );
  return { transport, msg2 };
}

describe("pairing URI & endpoint fallback helpers", () => {
  it("parses primary direct and direct_alt (Tailscale) endpoints from pairing URI", () => {
    const pub = Buffer.alloc(32, 0x33).toString("base64url");
    const offer = parsePairingUri(
      `masil://pair?v=1&host=mac-1&pub=${pub}&pair=tok1&name=Mac&direct=192.168.0.15%3A7767&direct_alt=100.97.190.15%3A7767`
    );
    expect(offer.directAddr).toBe("192.168.0.15:7767");
    expect(offer.directAddrs).toEqual([
      "192.168.0.15:7767",
      "100.97.190.15:7767",
    ]);
  });

  it("classifies LAN, Tailscale, and Relay routes and builds ordered URLs", () => {
    expect(isPrivateLanHost("192.168.0.15:7767")).toBe(true);
    expect(isPrivateLanHost("10.249.169.162:7767")).toBe(true);
    expect(isTailscaleHost("100.97.190.15:7767")).toBe(true);
    expect(isTailscaleHost("macbook.tail1234.ts.net:7767")).toBe(true);
    expect(isTailscaleHost("192.168.0.15:7767")).toBe(false);

    const urls = buildEndpointUrls({
      hostId: "mac-1",
      directAddr: "192.168.0.15:7767",
      directAddrs: ["192.168.0.15:7767", "100.97.190.15:7767"],
      relayUrl: "wss://relay.example.com",
    });
    expect(urls).toEqual([
      "ws://192.168.0.15:7767",
      "ws://100.97.190.15:7767",
      "wss://relay.example.com/v1/client/mac-1",
    ]);
    expect(classifyEndpointRoute(urls[0]!, "wss://relay.example.com", "mac-1")).toBe("lan");
    expect(classifyEndpointRoute(urls[1]!, "wss://relay.example.com", "mac-1")).toBe("tailscale");
    expect(classifyEndpointRoute(urls[2]!, "wss://relay.example.com", "mac-1")).toBe("relay");
  });

  it("merges host-advertised direct addresses while updating stale LAN DHCP IPs and retaining Tailscale", () => {
    expect(
      mergeAdvertisedDirectAddrs(
        ["192.168.0.15:7767", "100.97.190.15:7767"],
        ["192.168.0.29:7767", "100.97.190.15:7767"]
      )
    ).toEqual(["192.168.0.29:7767", "100.97.190.15:7767"]);

    expect(
      mergeAdvertisedDirectAddrs(
        ["192.168.0.15:7767", "100.97.190.15:7767"],
        ["192.168.0.29:7767"]
      )
    ).toEqual(["192.168.0.29:7767", "100.97.190.15:7767"]);
  });
});

describe("MasilRemoteClient staggered LAN -> Tailscale fallback", () => {
  it("falls back quickly when primary LAN endpoint stalls/blackholes and completes E2EE RPC on fallback", async () => {
    const hostPriv = x25519.utils.randomSecretKey();
    const hostPub = x25519.getPublicKey(hostPriv);

    // Server A simulates a blackholed home LAN IP: accepts WS but never responds.
    const stalledServer = new WebSocketServer({ host: "127.0.0.1", port: 0 });
    await new Promise<void>((r) => stalledServer.once("listening", r));
    const stalledPort = (stalledServer.address() as { port: number }).port;

    // Server B simulates the reachable Tailscale endpoint with real Noise NK.
    const fallbackServer = new WebSocketServer({ host: "127.0.0.1", port: 0 });
    await new Promise<void>((r) => fallbackServer.once("listening", r));
    const fallbackPort = (fallbackServer.address() as { port: number }).port;

    fallbackServer.on("connection", (socket) => {
      let tx: NoiseTransport | null = null;
      socket.on("message", (raw) => {
        const text = raw.toString();
        if (!tx) {
          const env = JSON.parse(text) as { message: string };
          const accepted = acceptNoiseNkResponder(
            hostPriv,
            hostPub,
            base64ToBytes(env.message)
          );
          tx = accepted.transport;
          socket.send(
            JSON.stringify({
              type: "e2ee_ready",
              v: 1,
              message: bytesToBase64(accepted.msg2),
            })
          );
          return;
        }
        const { payload } = openRecord(tx, base64ToBytes(text));
        const req = JSON.parse(decoder.decode(payload)) as {
          id: string;
          type: string;
        };
        const resp =
          req.type === "auth.hello.request"
            ? {
                v: 1,
                id: req.id,
                type: "auth.hello.response",
                ok: true,
                value: {
                  authenticated: true,
                  direct_addrs: [
                    `127.0.0.1:${stalledPort}`,
                    `127.0.0.1:${fallbackPort}`,
                  ],
                },
              }
            : {
                v: 1,
                id: req.id,
                type: `${req.type}.response`,
                ok: true,
                value: { pong: true },
              };
        const wire = sealRecord(tx, RecordKind.Text, encoder.encode(JSON.stringify(resp)));
        socket.send(bytesToBase64(wire));
      });
    });

    const startedAt = Date.now();
    const client = new MasilRemoteClient({
      hostId: "mac-test",
      hostPub,
      directAddr: `127.0.0.1:${stalledPort}`,
      directAddrs: [`127.0.0.1:${stalledPort}`, `127.0.0.1:${fallbackPort}`],
      deviceId: "dev-1",
      deviceSecret: "sec-1",
      staggerMs: 80,
      connectTimeoutMs: 5_000,
    });

    try {
      const auth = await client.connect();
      const elapsed = Date.now() - startedAt;
      expect(auth.authenticated).toBe(true);
      expect(client.activeEndpoint).toBe(`ws://127.0.0.1:${fallbackPort}`);
      // Must NOT wait for the 5s timeout on the stalled primary endpoint!
      expect(elapsed).toBeLessThan(1_500);

      // Verify encrypted RPC works immediately over the adopted fallback socket.
      const res = await client.request<{ pong: boolean }>("host.info.request");
      expect(res.pong).toBe(true);
    } finally {
      client.disconnect();
      stalledServer.close();
      fallbackServer.close();
    }
  });
});
