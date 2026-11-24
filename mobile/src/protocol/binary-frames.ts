/**
 * Binary application frame codec matching `crates/masil-wire/src/frames.rs`.
 */
export const BinaryOpcode = {
  TerminalOutput: 0x01,
  TerminalInput: 0x02,
  TerminalResize: 0x03,
  TerminalSnapshot: 0x04,
  AudioChunk: 0x10,
  AudioEnd: 0x11,
} as const;

export type BinaryOpcodeValue =
  (typeof BinaryOpcode)[keyof typeof BinaryOpcode];

export interface BinaryFrame {
  opcode: BinaryOpcodeValue;
  channelId: number;
  payload: Uint8Array;
}

export interface TerminalSnapshotMeta {
  cols: number;
  rows: number;
  cursor_x: number;
  cursor_y: number;
  desktop_cols: number;
  desktop_rows: number;
  fit_active: boolean;
}

export function encodeBinaryFrame(frame: BinaryFrame): Uint8Array {
  const out = new Uint8Array(3 + frame.payload.length);
  out[0] = frame.opcode;
  out[1] = (frame.channelId >> 8) & 0xff;
  out[2] = frame.channelId & 0xff;
  out.set(frame.payload, 3);
  return out;
}

export function decodeBinaryFrame(bytes: Uint8Array): BinaryFrame {
  if (bytes.length < 3) {
    throw new Error("binary frame shorter than 3 bytes");
  }
  const opcode = bytes[0]! as BinaryOpcodeValue;
  const channelId = (bytes[1]! << 8) | bytes[2]!;
  return {
    opcode,
    channelId,
    payload: bytes.subarray(3),
  };
}

export function decodeTerminalSnapshot(payload: Uint8Array): {
  meta: TerminalSnapshotMeta;
  ansi: Uint8Array;
} {
  if (payload.length < 4) {
    throw new Error("terminal snapshot payload too short");
  }
  const view = new DataView(
    payload.buffer,
    payload.byteOffset,
    payload.byteLength
  );
  const metaLen = view.getUint32(0, false);
  if (payload.length < 4 + metaLen) {
    throw new Error("truncated terminal snapshot header");
  }
  const metaJson = new TextDecoder().decode(payload.subarray(4, 4 + metaLen));
  const meta = JSON.parse(metaJson) as TerminalSnapshotMeta;
  return {
    meta,
    ansi: payload.subarray(4 + metaLen),
  };
}
