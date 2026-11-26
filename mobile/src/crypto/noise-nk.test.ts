import fs from "node:fs";
import path from "node:path";
import { describe, expect, it } from "vitest";
import {
  InitiatorHandshake,
  createHandshakeParams,
  keypairFromSecret,
} from "./noise-nk";
import { RecordKind, openRecord, sealRecord } from "./records";
import { base64ToBytes, bytesToBase64, parsePairingUri } from "./pairing";
import {
  BinaryOpcode,
  TUNNEL_INITIAL_WINDOW_BYTES,
  decodeBinaryFrame,
  decodeJsonPayload,
  decodeTunnelWindowPayload,
  encodeBinaryFrame,
  encodeJsonPayload,
  encodeTunnelCloseFrame,
  encodeTunnelOpenFrame,
  encodeTunnelWindowFrame,
} from "../protocol/binary-frames";

describe("Cross-language Noise NK & record parity with crates/masil-wire", () => {
  it("matches crates/masil-wire/tests/vectors.json bit-for-bit", () => {
    const vectorPath = path.resolve(
      __dirname,
      "../../../crates/masil-wire/tests/vectors.json"
    );
    const v = JSON.parse(fs.readFileSync(vectorPath, "utf8")) as Record<
      string,
      string
    >;

    const rsPub = base64ToBytes(v.responder_static_public_b64!);
    const ie = keypairFromSecret(base64ToBytes(v.initiator_ephemeral_secret_b64!));

    const { handshake, msg1 } = InitiatorHandshake.start(
      rsPub,
      createHandshakeParams(true),
      ie
    );
    expect(bytesToBase64(msg1)).toBe(v.msg1_b64);

    const transport = handshake.finish(base64ToBytes(v.msg2_b64!));
    expect(bytesToBase64(transport.handshakeHash)).toBe(v.handshake_hash_b64);

    // Initiator -> Responder text record must produce identical ciphertext.
    const textPlain = base64ToBytes(v.initiator_to_responder_text_plain_b64!);
    const textWire = sealRecord(transport, RecordKind.Text, textPlain);
    expect(bytesToBase64(textWire)).toBe(v.initiator_to_responder_text_wire_b64);

    // Responder -> Initiator binary record must decrypt to identical binary frame.
    const binWire = base64ToBytes(v.responder_to_initiator_binary_wire_b64!);
    const opened = openRecord(transport, binWire);
    expect(opened.kind).toBe(RecordKind.Binary);
    expect(bytesToBase64(opened.payload)).toBe(
      v.responder_to_initiator_binary_plain_b64
    );

    // Replay of binWire must throw.
    expect(() => openRecord(transport, binWire)).toThrow();
  });

  it("matches crates/masil-wire Phase 2 binary opcodes 0x20..0x31 bit-for-bit", () => {
    const vectorPath = path.resolve(
      __dirname,
      "../../../crates/masil-wire/tests/vectors.json"
    );
    const v = JSON.parse(fs.readFileSync(vectorPath, "utf8")) as {
      phase2_frames: Record<string, string>;
    };
    const p2 = v.phase2_frames;

    const openBytes = encodeTunnelOpenFrame(42, {
      target_port: 5173,
      agent_id: "default::%1",
    });
    expect(bytesToBase64(openBytes)).toBe(p2.tunnel_open_b64);

    const openAckFrame = decodeBinaryFrame(base64ToBytes(p2.tunnel_open_ack_b64!));
    expect(openAckFrame.opcode).toBe(BinaryOpcode.TunnelOpenAck);
    expect(openAckFrame.channelId).toBe(42);
    const openAck = decodeJsonPayload<{
      ok: boolean;
      initial_window: number;
    }>(openAckFrame.payload);
    expect(openAck.ok).toBe(true);
    expect(openAck.initial_window).toBe(TUNNEL_INITIAL_WINDOW_BYTES);

    const dataBytes = encodeBinaryFrame({
      opcode: BinaryOpcode.TunnelData,
      channelId: 42,
      payload: new TextEncoder().encode(
        "GET / HTTP/1.1\r\nHost: localhost:5173\r\n\r\n"
      ),
    });
    expect(bytesToBase64(dataBytes)).toBe(p2.tunnel_data_b64);

    const closeBytes = encodeTunnelCloseFrame(42, "eof");
    expect(bytesToBase64(closeBytes)).toBe(p2.tunnel_close_b64);

    const winBytes = encodeTunnelWindowFrame(42, 65_536);
    expect(bytesToBase64(winBytes)).toBe(p2.tunnel_window_b64);
    const winDecoded = decodeBinaryFrame(winBytes);
    expect(winDecoded.opcode).toBe(BinaryOpcode.TunnelWindow);
    expect(decodeTunnelWindowPayload(winDecoded.payload)).toBe(65_536);

    const chunkBytes = encodeBinaryFrame({
      opcode: BinaryOpcode.AssetUploadChunk,
      channelId: 9,
      payload: new Uint8Array([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    });
    expect(bytesToBase64(chunkBytes)).toBe(p2.asset_chunk_b64);

    const endFrame = decodeBinaryFrame(base64ToBytes(p2.asset_end_b64!));
    expect(endFrame.opcode).toBe(BinaryOpcode.AssetUploadEnd);
    expect(endFrame.channelId).toBe(9);
    const endPayload = decodeJsonPayload<{
      agent_id: string;
      mime: string;
      delivery_mode: string;
      meta: { url: string; annotations_count: number };
    }>(endFrame.payload);
    expect(endPayload.agent_id).toBe("%1");
    expect(endPayload.mime).toBe("image/png");
    expect(endPayload.delivery_mode).toBe("queue");
    expect(endPayload.meta.url).toBe("http://localhost:5173/dashboard");
    expect(endPayload.meta.annotations_count).toBe(2);

    // Re-encoding the exact same JSON matches byte-for-byte.
    const reencodedEnd = encodeBinaryFrame({
      opcode: BinaryOpcode.AssetUploadEnd,
      channelId: 9,
      payload: encodeJsonPayload({
        agent_id: "%1",
        mime: "image/png",
        filename: "markup.png",
        delivery_mode: "queue",
        prompt: "Fix padding",
        meta: {
          url: "http://localhost:5173/dashboard",
          viewport: "390x844",
          theme: "dark",
          console_errors: ["TypeError: boom"],
          network_errors: ["GET /api -> 500"],
          annotations_count: 2,
        },
      }),
    });
    expect(bytesToBase64(reencodedEnd)).toBe(p2.asset_end_b64);
  });

  it("parses masil://pair URIs strictly", () => {
    const pub = Buffer.alloc(32, 0x42).toString("base64url");
    const offer = parsePairingUri(
      `masil://pair?v=1&host=macbook&pub=${pub}&pair=tok123&relay=wss%3A%2F%2Frelay.masil.dev%2Fv1&name=Ochi%20Mac`
    );
    expect(offer.hostId).toBe("macbook");
    expect(offer.hostName).toBe("Ochi Mac");
    expect(offer.relayUrl).toBe("wss://relay.masil.dev/v1");
    expect(offer.hostPub.length).toBe(32);
  });
});
