/**
 * Binary application frame codec matching `crates/masil-wire/src/frames.rs`.
 */
export const TUNNEL_MAX_CHUNK_BYTES = 32 * 1024;
export const TUNNEL_INITIAL_WINDOW_BYTES = 256 * 1024;
export const ASSET_MAX_UPLOAD_BYTES = 10 * 1024 * 1024;

export const BinaryOpcode = {
  TerminalOutput: 0x01,
  TerminalInput: 0x02,
  TerminalResize: 0x03,
  TerminalSnapshot: 0x04,
  AudioChunk: 0x10,
  AudioEnd: 0x11,
  TunnelOpen: 0x20,
  TunnelOpenAck: 0x21,
  TunnelData: 0x22,
  TunnelClose: 0x23,
  TunnelWindow: 0x24,
  AssetUploadChunk: 0x30,
  AssetUploadEnd: 0x31,
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

export interface TunnelOpenPayload {
  target_port: number;
  agent_id?: string;
}

export interface TunnelOpenAckPayload {
  ok: boolean;
  initial_window: number;
  error?: string;
}

export interface TunnelClosePayload {
  reason?: string;
}

export type AssetDeliveryMode = "send" | "queue" | "prefill";

export interface AssetUploadMeta {
  url?: string;
  viewport?: string;
  theme?: string;
  server_socket?: string;
  cwd?: string;
  console_errors?: string[];
  network_errors?: string[];
  annotations_count?: number;
}

export interface AssetUploadEndPayload {
  agent_id: string;
  mime: string;
  filename: string;
  delivery_mode: AssetDeliveryMode;
  prompt?: string;
  meta: AssetUploadMeta;
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

export function encodeJsonPayload(value: unknown): Uint8Array {
  return new TextEncoder().encode(JSON.stringify(value));
}

export function decodeJsonPayload<T>(payload: Uint8Array): T {
  return JSON.parse(new TextDecoder().decode(payload)) as T;
}

export function encodeTunnelWindowPayload(credit: number): Uint8Array {
  const out = new Uint8Array(4);
  const view = new DataView(out.buffer);
  view.setUint32(0, credit >>> 0, false);
  return out;
}

export function decodeTunnelWindowPayload(payload: Uint8Array): number {
  if (payload.length !== 4) {
    throw new Error(`tunnel window must be 4 bytes, got ${payload.length}`);
  }
  const view = new DataView(
    payload.buffer,
    payload.byteOffset,
    payload.byteLength
  );
  return view.getUint32(0, false);
}

export function encodeTunnelOpenFrame(
  streamId: number,
  payload: TunnelOpenPayload
): Uint8Array {
  return encodeBinaryFrame({
    opcode: BinaryOpcode.TunnelOpen,
    channelId: streamId,
    payload: encodeJsonPayload(payload),
  });
}

export function encodeTunnelWindowFrame(
  streamId: number,
  credit: number
): Uint8Array {
  return encodeBinaryFrame({
    opcode: BinaryOpcode.TunnelWindow,
    channelId: streamId,
    payload: encodeTunnelWindowPayload(credit),
  });
}

export function encodeTunnelCloseFrame(
  streamId: number,
  reason?: string
): Uint8Array {
  const body: TunnelClosePayload = reason ? { reason } : {};
  return encodeBinaryFrame({
    opcode: BinaryOpcode.TunnelClose,
    channelId: streamId,
    payload: encodeJsonPayload(body),
  });
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
