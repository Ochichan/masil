/**
 * Chunked replay-protected record framing matching `crates/masil-wire/src/record.rs`.
 */
import { NoiseTransport, TAG_LEN } from "./noise-nk";

export const MAX_CHUNK_DATA = 65_513;
export const CHUNK_HEADER_LEN = 6;
export const MAX_RECORD_BYTES = 4 * 1024 * 1024;
const FLAG_FIRST = 0x01;
const FLAG_LAST = 0x02;
const KNOWN_FLAGS = FLAG_FIRST | FLAG_LAST;

export const RecordKind = {
  Text: 0x00,
  Binary: 0x01,
} as const;

export type RecordKindValue = (typeof RecordKind)[keyof typeof RecordKind];

export function sealRecord(
  transport: NoiseTransport,
  kind: RecordKindValue,
  payload: Uint8Array
): Uint8Array {
  if (payload.length > MAX_RECORD_BYTES) {
    throw new Error(`record payload ${payload.length} exceeds limit`);
  }
  const totalLen = payload.length;
  const chunkCount =
    totalLen === 0 ? 1 : Math.ceil(totalLen / MAX_CHUNK_DATA);
  const parts: Uint8Array[] = [];
  let wireTotal = 0;

  for (let idx = 0; idx < chunkCount; idx++) {
    const start = idx * MAX_CHUNK_DATA;
    const end = Math.min(start + MAX_CHUNK_DATA, totalLen);
    const slice = payload.subarray(start, end);
    let flags = 0;
    if (idx === 0) flags |= FLAG_FIRST;
    if (idx + 1 === chunkCount) flags |= FLAG_LAST;

    const plain = new Uint8Array(CHUNK_HEADER_LEN + slice.length);
    const view = new DataView(plain.buffer);
    plain[0] = kind;
    plain[1] = flags;
    view.setUint32(2, totalLen, false);
    plain.set(slice, CHUNK_HEADER_LEN);

    const ct = transport.encrypt(plain);
    const framed = new Uint8Array(2 + ct.length);
    new DataView(framed.buffer).setUint16(0, ct.length, false);
    framed.set(ct, 2);
    parts.push(framed);
    wireTotal += framed.length;
  }

  const out = new Uint8Array(wireTotal);
  let off = 0;
  for (const p of parts) {
    out.set(p, off);
    off += p.length;
  }
  return out;
}

export function openRecord(
  transport: NoiseTransport,
  wire: Uint8Array
): { kind: RecordKindValue; payload: Uint8Array } {
  if (wire.length === 0) {
    throw new Error("empty wire record");
  }
  const chunks: Uint8Array[] = [];
  let scan = 0;
  while (scan < wire.length) {
    if (wire.length - scan < 2) {
      throw new Error("truncated wire record header");
    }
    const ctLen = (wire[scan]! << 8) | wire[scan + 1]!;
    scan += 2;
    if (ctLen < CHUNK_HEADER_LEN + TAG_LEN || wire.length - scan < ctLen) {
      throw new Error("truncated wire record chunk");
    }
    chunks.push(wire.subarray(scan, scan + ctLen));
    scan += ctLen;
  }

  let expectedKind: RecordKindValue | null = null;
  let expectedTotal: number | null = null;
  let out: Uint8Array | null = null;
  let written = 0;
  let sawLast = false;

  for (let idx = 0; idx < chunks.length; idx++) {
    if (sawLast) {
      throw new Error("trailing bytes after final chunk");
    }
    const plain = transport.decrypt(chunks[idx]!);
    if (plain.length < CHUNK_HEADER_LEN) {
      throw new Error("chunk shorter than header");
    }
    const kindByte = plain[0]!;
    if (kindByte !== RecordKind.Text && kindByte !== RecordKind.Binary) {
      throw new Error(`invalid record kind ${kindByte}`);
    }
    const flags = plain[1]!;
    if ((flags & ~KNOWN_FLAGS) !== 0) {
      throw new Error(`invalid record flags ${flags}`);
    }
    const isFirst = (flags & FLAG_FIRST) !== 0;
    const isLast = (flags & FLAG_LAST) !== 0;
    if ((idx === 0) !== isFirst) {
      throw new Error("misplaced first-chunk flag");
    }
    const view = new DataView(plain.buffer, plain.byteOffset, plain.byteLength);
    const total = view.getUint32(2, false);
    if (total > MAX_RECORD_BYTES) {
      throw new Error("declared record length exceeds limit");
    }
    const data = plain.subarray(CHUNK_HEADER_LEN);
    if (!isLast && data.length !== MAX_CHUNK_DATA) {
      throw new Error("non-final chunk not full");
    }

    if (expectedKind === null || expectedTotal === null || out === null) {
      if (total > wire.length) {
        throw new Error("declared record length exceeds wire size");
      }
      expectedKind = kindByte;
      expectedTotal = total;
      out = new Uint8Array(total);
    } else if (expectedKind !== kindByte || expectedTotal !== total) {
      throw new Error("chunk metadata mismatch");
    }

    if (written + data.length > expectedTotal) {
      throw new Error("chunk overflowed declared total length");
    }
    out.set(data, written);
    written += data.length;
    if (isLast) sawLast = true;
  }

  if (!sawLast || expectedKind === null || out === null || written !== expectedTotal) {
    throw new Error("incomplete wire record");
  }
  return { kind: expectedKind, payload: out };
}
