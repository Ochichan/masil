/**
 * E2EE Noise NK WebSocket client for connecting to `masil-agent remote` either
 * directly (LAN / Tailscale) or through `masil-relay`.
 */
import {
  InitiatorHandshake,
  NoiseTransport,
  createHandshakeParams,
} from "../crypto/noise-nk";
import { base64ToBytes, bytesToBase64 } from "../crypto/pairing";
import { RecordKind, openRecord, sealRecord } from "../crypto/records";
import {
  BinaryFrame,
  BinaryOpcode,
  decodeBinaryFrame,
  encodeBinaryFrame,
} from "./binary-frames";

export type ConnectionState = "disconnected" | "connecting" | "connected";

export interface MasilClientOptions {
  hostId: string;
  hostPub: Uint8Array;
  relayUrl?: string;
  directAddr?: string;
  deviceId?: string;
  deviceSecret?: string;
  pairToken?: string;
  deviceName?: string;
  platform?: "ios" | "android" | "web";
  onStateChange?: (state: ConnectionState, error?: string) => void;
  onBinaryFrame?: (frame: BinaryFrame) => void;
  onEvent?: (event: Record<string, unknown>) => void;
}

interface PendingRpc {
  resolve: (value: unknown) => void;
  reject: (err: Error) => void;
  timer: ReturnType<typeof setTimeout>;
}

const RPC_TIMEOUT_MS = 60_000;
const PING_INTERVAL_MS = 10_000;

export class MasilRemoteClient {
  private readonly opts: MasilClientOptions;
  private ws: WebSocket | null = null;
  private transport: NoiseTransport | null = null;
  private pending = new Map<string, PendingRpc>();
  private nextReqId = 1;
  private state: ConnectionState = "disconnected";
  private pingTimer: ReturnType<typeof setInterval> | null = null;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private lastInboundMs = 0;
  private missedPings = 0;
  private intentionallyClosed = false;
  private backoffMs = 1000;

  constructor(opts: MasilClientOptions) {
    this.opts = opts;
  }

  get connectionState(): ConnectionState {
    return this.state;
  }

  private setState(next: ConnectionState, error?: string): void {
    this.state = next;
    this.opts.onStateChange?.(next, error);
  }

  private resolveEndpointUrls(): string[] {
    const urls: string[] = [];
    if (this.opts.directAddr) {
      const addr = this.opts.directAddr.trim();
      urls.push(
        addr.startsWith("ws://") || addr.startsWith("wss://")
          ? addr
          : `ws://${addr}`
      );
    }
    if (this.opts.relayUrl) {
      const base = this.opts.relayUrl.trim().replace(/\/+$/, "");
      const prefix = base.endsWith("/v1") ? base : `${base}/v1`;
      urls.push(`${prefix}/client/${encodeURIComponent(this.opts.hostId)}`);
    }
    return urls;
  }

  async connect(): Promise<Record<string, unknown>> {
    this.intentionallyClosed = false;
    this.clearTimers();
    this.setState("connecting");

    const endpoints = this.resolveEndpointUrls();
    if (endpoints.length === 0) {
      this.setState("disconnected", "No relay or direct endpoint configured");
      throw new Error("No relay or direct endpoint configured");
    }

    let lastErr: Error = new Error("Unable to reach host");
    for (const url of endpoints) {
      try {
        const authResult = await this.connectOne(url);
        this.backoffMs = 1000;
        this.setState("connected");
        this.startLivenessLoop();
        return authResult;
      } catch (err) {
        lastErr = err instanceof Error ? err : new Error(String(err));
      }
    }

    this.setState("disconnected", lastErr.message);
    this.scheduleReconnect();
    throw lastErr;
  }

  /**
   * Called when mobile app returns from OS background suspension: runs a fresh
   * liveness probe and reconnects immediately without backoff if stale.
   */
  async onAppForeground(): Promise<void> {
    if (this.intentionallyClosed) return;
    if (this.state !== "connected" || !this.ws || !this.transport) {
      this.backoffMs = 1000;
      void this.connect().catch(() => {});
      return;
    }
    const before = this.lastInboundMs;
    this.sendPing();
    setTimeout(() => {
      if (this.lastInboundMs === before && !this.intentionallyClosed) {
        this.teardownSocket();
        this.backoffMs = 1000;
        void this.connect().catch(() => {});
      }
    }, 5_000);
  }

  private connectOne(url: string): Promise<Record<string, unknown>> {
    return new Promise((resolve, reject) => {
      const ws = new WebSocket(url);
      ws.binaryType = "arraybuffer";
      const { handshake, msg1 } = InitiatorHandshake.start(
        this.opts.hostPub,
        createHandshakeParams(true)
      );
      let handshakeDone = false;

      const connectDeadline = setTimeout(() => {
        if (!handshakeDone) {
          ws.close();
          reject(new Error(`Connection timed out (${url})`));
        }
      }, 10_000);

      ws.onopen = () => {
        ws.send(
          JSON.stringify({
            type: "e2ee_hello",
            v: 1,
            message: bytesToBase64(msg1),
          })
        );
      };

      ws.onerror = () => {
        if (!handshakeDone) {
          clearTimeout(connectDeadline);
          reject(new Error(`WebSocket error (${url})`));
        }
      };

      ws.onclose = () => {
        clearTimeout(connectDeadline);
        if (!handshakeDone) {
          reject(new Error(`Closed before handshake completed (${url})`));
          return;
        }
        this.handleDisconnect();
      };

      ws.onmessage = (event: MessageEvent) => {
        this.lastInboundMs = Date.now();
        this.missedPings = 0;

        if (!handshakeDone) {
          try {
            const raw =
              typeof event.data === "string"
                ? event.data
                : new TextDecoder().decode(new Uint8Array(event.data as ArrayBuffer));
            const env = JSON.parse(raw) as {
              type?: string;
              v?: number;
              message?: string;
            };
            if (env.type !== "e2ee_ready" || env.v !== 1 || !env.message) {
              throw new Error("Unexpected handshake response");
            }
            const transport = handshake.finish(base64ToBytes(env.message));
            handshakeDone = true;
            clearTimeout(connectDeadline);
            this.ws = ws;
            this.transport = transport;

            const authPromise = this.opts.pairToken
              ? this.request("auth.pair.request", {
                  pair_token: this.opts.pairToken,
                  device_name: this.opts.deviceName ?? "masil mobile",
                  platform: this.opts.platform ?? "ios",
                })
              : this.request("auth.hello.request", {
                  device_id: this.opts.deviceId ?? "",
                  device_secret: this.opts.deviceSecret ?? "",
                });

            authPromise
              .then((res) => resolve(res as Record<string, unknown>))
              .catch((err) => {
                ws.close();
                reject(err);
              });
          } catch (err) {
            clearTimeout(connectDeadline);
            ws.close();
            reject(err instanceof Error ? err : new Error(String(err)));
          }
          return;
        }

        this.handleEncryptedInbound(event.data);
      };
    });
  }

  private handleEncryptedInbound(data: unknown): void {
    if (!this.transport) return;
    try {
      let wire: Uint8Array;
      if (typeof data === "string") {
        wire = base64ToBytes(data);
      } else if (data instanceof ArrayBuffer) {
        wire = new Uint8Array(data);
      } else if (ArrayBuffer.isView(data)) {
        wire = new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
      } else {
        return;
      }

      const { kind, payload } = openRecord(this.transport, wire);
      if (kind === RecordKind.Binary) {
        const frame = decodeBinaryFrame(payload);
        this.opts.onBinaryFrame?.(frame);
        return;
      }

      const msg = JSON.parse(new TextDecoder().decode(payload)) as Record<
        string,
        unknown
      >;
      if (msg.type === "pong") {
        return;
      }
      if (typeof msg.id === "string" && this.pending.has(msg.id)) {
        const entry = this.pending.get(msg.id)!;
        this.pending.delete(msg.id);
        clearTimeout(entry.timer);
        if (msg.ok === true) {
          entry.resolve(msg.value);
        } else {
          const errObj = msg.error as
            | { code?: string; message?: string }
            | undefined;
          entry.reject(
            new Error(
              errObj?.message
                ? `[${errObj.code ?? "error"}] ${errObj.message}`
                : "RPC failed"
            )
          );
        }
        return;
      }
      this.opts.onEvent?.(msg);
    } catch {
      // Any record authentication or framing violation tears down immediately.
      this.teardownSocket();
      this.handleDisconnect();
    }
  }

  request<T = Record<string, unknown>>(
    type: string,
    params: Record<string, unknown> = {}
  ): Promise<T> {
    if (!this.ws || !this.transport || this.ws.readyState !== WebSocket.OPEN) {
      return Promise.reject(new Error("Host is not connected"));
    }
    const id = `r-${this.nextReqId++}`;
    const body = new TextEncoder().encode(
      JSON.stringify({ v: 1, id, type, params })
    );
    const wire = sealRecord(this.transport, RecordKind.Text, body);
    this.ws.send(bytesToBase64(wire));

    return new Promise<T>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`RPC timeout: ${type}`));
      }, RPC_TIMEOUT_MS);
      this.pending.set(id, {
        resolve: (v) => resolve(v as T),
        reject,
        timer,
      });
    });
  }

  sendBinaryFrame(frame: BinaryFrame): void {
    if (!this.ws || !this.transport || this.ws.readyState !== WebSocket.OPEN) {
      return;
    }
    const encoded = encodeBinaryFrame(frame);
    const wire = sealRecord(this.transport, RecordKind.Binary, encoded);
    if (this.transport.params.flags & 0x01) {
      this.ws.send(wire);
    } else {
      this.ws.send(bytesToBase64(wire));
    }
  }

  sendTerminalInput(channelId: number, bytes: Uint8Array): void {
    this.sendBinaryFrame({
      opcode: BinaryOpcode.TerminalInput,
      channelId,
      payload: bytes,
    });
  }

  sendTerminalResize(
    channelId: number,
    cols: number,
    rows: number,
    mode: "desktop" | "fit"
  ): void {
    const payload = new TextEncoder().encode(
      JSON.stringify({ cols, rows, mode })
    );
    this.sendBinaryFrame({
      opcode: BinaryOpcode.TerminalResize,
      channelId,
      payload,
    });
  }

  private sendPing(): void {
    if (!this.ws || !this.transport || this.ws.readyState !== WebSocket.OPEN) {
      return;
    }
    const body = new TextEncoder().encode(
      JSON.stringify({ v: 1, type: "ping", ts: Date.now() })
    );
    const wire = sealRecord(this.transport, RecordKind.Text, body);
    this.ws.send(bytesToBase64(wire));
  }

  private startLivenessLoop(): void {
    this.clearTimers();
    this.lastInboundMs = Date.now();
    this.missedPings = 0;
    this.pingTimer = setInterval(() => {
      if (Date.now() - this.lastInboundMs >= PING_INTERVAL_MS) {
        this.missedPings += 1;
        if (this.missedPings >= 2) {
          this.teardownSocket();
          this.handleDisconnect();
          return;
        }
      }
      this.sendPing();
    }, PING_INTERVAL_MS);
  }

  private handleDisconnect(): void {
    this.clearTimers();
    for (const [, p] of this.pending) {
      clearTimeout(p.timer);
      p.reject(new Error("Connection closed"));
    }
    this.pending.clear();
    this.transport?.close();
    this.transport = null;
    this.ws = null;
    this.setState("disconnected");
    if (!this.intentionallyClosed) {
      this.scheduleReconnect();
    }
  }

  private scheduleReconnect(): void {
    if (this.intentionallyClosed || this.reconnectTimer) return;
    const delay = this.backoffMs;
    this.backoffMs = Math.min(this.backoffMs * 2, 30_000);
    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = null;
      void this.connect().catch(() => {});
    }, delay);
  }

  private teardownSocket(): void {
    try {
      this.ws?.close();
    } catch {
      // ignore
    }
    this.ws = null;
  }

  private clearTimers(): void {
    if (this.pingTimer) {
      clearInterval(this.pingTimer);
      this.pingTimer = null;
    }
    if (this.reconnectTimer) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
  }

  disconnect(): void {
    this.intentionallyClosed = true;
    this.clearTimers();
    this.teardownSocket();
    this.transport?.close();
    this.transport = null;
    this.setState("disconnected");
  }
}
