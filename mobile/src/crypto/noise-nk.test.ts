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
