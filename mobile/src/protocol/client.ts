/**
 * E2EE Noise NK WebSocket client for connecting to `masil-agent remote` either
 * directly (LAN -> Tailscale automatic staggered fallback) or through `masil-relay`.
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
import {
  buildEndpointUrls,
  isLoopbackHost,
  normalizeDirectAddrList,
} from "./endpoints";

export type ConnectionState = "disconnected" | "connecting" | "connected";

export interface MasilClientOptions {
  hostId: string;
  hostPub: Uint8Array;
  relayUrl?: string;
  directAddr?: string;
  directAddrs?: string[];
  deviceId?: string;
  deviceSecret?: string;
  pairToken?: string;
  deviceName?: string;
  platform?: "ios" | "android" | "web";
  /** Delay before launching the next fallback endpoint while an earlier attempt is still pending. */
  staggerMs?: number;
  /** Shorter stagger delay when the next endpoint was the last one that succeeded. */
  fastStaggerMs?: number;
  /** Per-endpoint connect + auth timeout in ms. */
  connectTimeoutMs?: number;
  onStateChange?: (
    state: ConnectionState,
    error?: string,
    activeEndpoint?: string | null
  ) => void;
  onAuthResult?: (
    authResult: Record<string, unknown>,
    activeEndpoint: string
  ) => void;
  onBinaryFrame?: (frame: BinaryFrame) => void;
  onEvent?: (event: Record<string, unknown>) => void;
}

interface PendingRpc {
  resolve: (value: unknown) => void;
  reject: (err: Error) => void;
  timer: ReturnType<typeof setTimeout>;
}

interface ActiveAttempt {
  url: string;
  abort: () => void;
}

const RPC_TIMEOUT_MS = 60_000;
const PING_INTERVAL_MS = 10_000;
const DEFAULT_CONNECT_TIMEOUT_MS = 10_000;
const DEFAULT_STAGGER_MS = 1_200;
const DEFAULT_FAST_STAGGER_MS = 350;
const LOOPBACK_STAGGER_MS = 150;

export class MasilRemoteClient {
  private readonly opts: MasilClientOptions;
  private relayUrl?: string;
  private directAddr?: string;
  private directAddrs: string[];
  private ws: WebSocket | null = null;
  private transport: NoiseTransport | null = null;
  private pending = new Map<string, PendingRpc>();
  private nextReqId = 1;
  private state: ConnectionState = "disconnected";
  private activeEndpointUrl: string | null = null;
  private lastSucceededUrl: string | null = null;
  private pingTimer: ReturnType<typeof setInterval> | null = null;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private activeAttempts = new Set<ActiveAttempt>();
  private connectRaceSeq = 0;
  private lastInboundMs = 0;
  private missedPings = 0;
  private intentionallyClosed = false;
  private backoffMs = 1000;

  constructor(opts: MasilClientOptions) {
    this.opts = opts;
    this.relayUrl = opts.relayUrl;
    this.directAddr = opts.directAddr;
    this.directAddrs = normalizeDirectAddrList([
      opts.directAddr,
      ...(opts.directAddrs ?? []),
    ]);
  }

  get connectionState(): ConnectionState {
    return this.state;
  }

  get activeEndpoint(): string | null {
    return this.activeEndpointUrl;
  }

  /**
   * Updates saved endpoints (e.g. when the host advertises fresh LAN + Tailscale
   * addresses after connecting) without interrupting an active session.
   */
  updateEndpoints(patch: {
    directAddr?: string;
    directAddrs?: string[];
    relayUrl?: string;
  }): void {
    if (patch.directAddr !== undefined) {
      this.directAddr = patch.directAddr;
    }
    if (patch.directAddrs !== undefined) {
      this.directAddrs = normalizeDirectAddrList([
        ...(patch.directAddrs ?? []),
        patch.directAddr,
      ]);
      this.directAddr = this.directAddrs[0];
    } else if (patch.directAddr !== undefined) {
      this.directAddrs = normalizeDirectAddrList([
        patch.directAddr,
        ...this.directAddrs,
      ]);
    }
    if (patch.relayUrl !== undefined) {
      this.relayUrl = patch.relayUrl || undefined;
    }
  }

  private setState(next: ConnectionState, error?: string): void {
    this.state = next;
    if (next !== "connected") {
      this.activeEndpointUrl = null;
    }
    this.opts.onStateChange?.(next, error, this.activeEndpointUrl);
  }

  private resolveEndpointUrls(): string[] {
    return buildEndpointUrls({
      hostId: this.opts.hostId,
      directAddr: this.directAddr,
      directAddrs: this.directAddrs,
      relayUrl: this.relayUrl,
    });
  }

  async connect(): Promise<Record<string, unknown>> {
    this.intentionallyClosed = false;
    this.abortInFlightAttempts();
    this.clearTimers();
    this.teardownSocket();
    this.transport?.close();
    this.transport = null;
    this.setState("connecting");

    const endpoints = this.resolveEndpointUrls();
    if (endpoints.length === 0) {
      this.setState("disconnected", "No relay or direct endpoint configured");
      throw new Error("No relay or direct endpoint configured");
    }

    const raceId = ++this.connectRaceSeq;

    try {
      const winner = await this.connectWithStaggeredFallback(endpoints, raceId);
      if (this.intentionallyClosed || raceId !== this.connectRaceSeq) {
        this.teardownSocket();
        this.closeTransport();
        throw new Error("Connection cancelled");
      }
      this.backoffMs = 1000;
      this.lastSucceededUrl = winner.url;
      this.activeEndpointUrl = winner.url;
      this.setState("connected");
      this.opts.onAuthResult?.(winner.authResult, winner.url);
      this.startLivenessLoop();
      return winner.authResult;
    } catch (err) {
      const lastErr = err instanceof Error ? err : new Error(String(err));
      if (!this.intentionallyClosed && raceId === this.connectRaceSeq) {
        this.setState("disconnected", lastErr.message);
        this.scheduleReconnect();
      }
      throw lastErr;
    }
  }

  /**
   * Happy-Eyeballs style staggered fallback across ordered endpoints:
   * tries primary LAN immediately, starts Tailscale fallback quickly if LAN
   * blackholes (or immediately if LAN refuses), then relay if configured.
   */
  private connectWithStaggeredFallback(
    endpoints: string[],
    raceId: number
  ): Promise<{ url: string; authResult: Record<string, unknown> }> {
    const staggerMs = this.opts.staggerMs ?? DEFAULT_STAGGER_MS;
    const fastStaggerMs = this.opts.fastStaggerMs ?? DEFAULT_FAST_STAGGER_MS;
    const timeoutMs = this.opts.connectTimeoutMs ?? DEFAULT_CONNECT_TIMEOUT_MS;

    return new Promise((resolve, reject) => {
      let settled = false;
      let nextIdx = 0;
      let failures = 0;
      let lastErr: Error = new Error("Unable to reach host");
      let staggerTimer: ReturnType<typeof setTimeout> | null = null;
      const raceAttempts = new Set<ActiveAttempt>();

      const clearStagger = () => {
        if (staggerTimer) {
          clearTimeout(staggerTimer);
          staggerTimer = null;
        }
      };

      const abortOthers = (winner?: ActiveAttempt) => {
        clearStagger();
        for (const att of raceAttempts) {
          if (att !== winner) {
            att.abort();
            this.activeAttempts.delete(att);
          }
        }
        raceAttempts.clear();
      };

      const scheduleNext = (prevUrl: string) => {
        clearStagger();
        if (settled || nextIdx >= endpoints.length) return;
        const upcomingUrl = endpoints[nextIdx]!;
        let delay = staggerMs;
        if (isLoopbackHost(prevUrl)) {
          delay = Math.min(delay, LOOPBACK_STAGGER_MS);
        } else if (this.lastSucceededUrl && upcomingUrl === this.lastSucceededUrl) {
          delay = Math.min(delay, fastStaggerMs);
        }
        staggerTimer = setTimeout(() => {
          staggerTimer = null;
          launchNext();
        }, delay);
      };

      const launchNext = () => {
        if (settled || nextIdx >= endpoints.length) return;
        if (this.intentionallyClosed || raceId !== this.connectRaceSeq) {
          settled = true;
          abortOthers();
          reject(new Error("Connection cancelled"));
          return;
        }

        const url = endpoints[nextIdx++]!;
        const attempt = this.startSingleAttempt(
          url,
          timeoutMs,
          (ws, transport, authResult) => {
            if (settled || this.intentionallyClosed || raceId !== this.connectRaceSeq) {
              try {
                ws.close();
              } catch {
                // ignore
              }
              transport.close();
              return false;
            }
            settled = true;
            this.activeAttempts.delete(attempt);
            abortOthers(attempt);

            // Adopt winning socket & transport synchronously inside ws.onmessage
            // so zero subsequent frames can be dropped or reordered.
            this.ws = ws;
            this.transport = transport;
            this.lastInboundMs = Date.now();
            this.missedPings = 0;
            ws.onmessage = (ev: MessageEvent) => {
              if (this.ws !== ws) return;
              this.lastInboundMs = Date.now();
              this.missedPings = 0;
              this.handleEncryptedInbound(ev.data);
            };
            ws.onclose = () => {
              if (this.ws !== ws) return;
              this.handleDisconnect();
            };
            ws.onerror = () => {
              // onclose handles teardown and reconnect.
            };

            resolve({ url, authResult });
            return true;
          },
          (err) => {
            raceAttempts.delete(attempt);
            this.activeAttempts.delete(attempt);
            if (settled) return;
            lastErr = err;
            failures += 1;
            if (nextIdx < endpoints.length) {
              // Fast failure: immediately launch the next fallback endpoint.
              clearStagger();
              launchNext();
            } else if (failures >= endpoints.length) {
              settled = true;
              clearStagger();
              reject(lastErr);
            }
          }
        );

        raceAttempts.add(attempt);
        this.activeAttempts.add(attempt);

        if (nextIdx < endpoints.length) {
          scheduleNext(url);
        }
      };

      launchNext();
    });
  }

  private startSingleAttempt(
    url: string,
    timeoutMs: number,
    onWin: (
      ws: WebSocket,
      transport: NoiseTransport,
      authResult: Record<string, unknown>
    ) => boolean,
    onFail: (err: Error) => void
  ): ActiveAttempt {
    let closed = false;
    let handshakeDone = false;
    let ws: WebSocket | null = null;
    let transport: NoiseTransport | null = null;
    const authReqId = `auth-${this.nextReqId++}`;

    const cleanup = (closeSocket: boolean) => {
      clearTimeout(deadline);
      if (closeSocket && ws) {
        try {
          ws.onopen = null;
          ws.onmessage = null;
          ws.onerror = null;
          ws.onclose = null;
          ws.close();
        } catch {
          // ignore
        }
      }
      ws = null;
      if (closeSocket && transport) {
        transport.close();
      }
      transport = null;
    };

    const fail = (err: Error) => {
      if (closed) return;
      closed = true;
      cleanup(true);
      onFail(err);
    };

    const deadline = setTimeout(() => {
      fail(new Error(`Connection timed out (${url})`));
    }, timeoutMs);

    try {
      ws = new WebSocket(url);
      ws.binaryType = "arraybuffer";
    } catch (err) {
      fail(err instanceof Error ? err : new Error(String(err)));
      return { url, abort: () => {} };
    }

    const { handshake, msg1 } = InitiatorHandshake.start(
      this.opts.hostPub,
      createHandshakeParams(true)
    );

    ws.onopen = () => {
      if (closed || !ws) return;
      try {
        ws.send(
          JSON.stringify({
            type: "e2ee_hello",
            v: 1,
            message: bytesToBase64(msg1),
          })
        );
      } catch (err) {
        fail(err instanceof Error ? err : new Error(String(err)));
      }
    };

    ws.onerror = () => {
      fail(new Error(`WebSocket error (${url})`));
    };

    ws.onclose = () => {
      fail(new Error(`Closed before authentication completed (${url})`));
    };

    ws.onmessage = (event: MessageEvent) => {
      if (closed || !ws) return;

      if (!handshakeDone) {
        try {
          const raw =
            typeof event.data === "string"
              ? event.data
              : new TextDecoder().decode(
                  new Uint8Array(event.data as ArrayBuffer)
                );
          const env = JSON.parse(raw) as {
            type?: string;
            v?: number;
            message?: string;
          };
          if (env.type !== "e2ee_ready" || env.v !== 1 || !env.message) {
            throw new Error("Unexpected handshake response");
          }
          transport = handshake.finish(base64ToBytes(env.message));
          handshakeDone = true;

          const authPayload = this.opts.pairToken
            ? {
                v: 1,
                id: authReqId,
                type: "auth.pair.request",
                params: {
                  pair_token: this.opts.pairToken,
                  device_name: this.opts.deviceName ?? "masil mobile",
                  platform: this.opts.platform ?? "ios",
                },
              }
            : {
                v: 1,
                id: authReqId,
                type: "auth.hello.request",
                params: {
                  device_id: this.opts.deviceId ?? "",
                  device_secret: this.opts.deviceSecret ?? "",
                },
              };

          const sealed = sealRecord(
            transport,
            RecordKind.Text,
            new TextEncoder().encode(JSON.stringify(authPayload))
          );
          ws.send(bytesToBase64(sealed));
        } catch (err) {
          fail(err instanceof Error ? err : new Error(String(err)));
        }
        return;
      }

      // Awaiting encrypted auth response on this candidate transport.
      if (!transport) return;
      try {
        const wire = this.decodeWireBytes(event.data);
        if (!wire) return;
        const { kind, payload } = openRecord(transport, wire);
        if (kind !== RecordKind.Text) return;
        const msg = JSON.parse(new TextDecoder().decode(payload)) as Record<
          string,
          unknown
        >;
        if (msg.id !== authReqId) return;

        if (msg.ok !== true) {
          const errObj = msg.error as
            | { code?: string; message?: string }
            | undefined;
          throw new Error(
            errObj?.message
              ? `[${errObj.code ?? "error"}] ${errObj.message}`
              : "Authentication failed"
          );
        }

        const winningWs = ws;
        const winningTransport = transport;
        closed = true;
        clearTimeout(deadline);
        ws = null;
        transport = null;

        const adopted = onWin(
          winningWs,
          winningTransport,
          (msg.value as Record<string, unknown>) ?? {}
        );
        if (!adopted) {
          try {
            winningWs.close();
          } catch {
            // ignore
          }
          winningTransport.close();
        }
      } catch (err) {
        fail(err instanceof Error ? err : new Error(String(err)));
      }
    };

    return {
      url,
      abort: () => {
        if (closed) return;
        closed = true;
        cleanup(true);
      },
    };
  }

  private abortInFlightAttempts(): void {
    for (const att of this.activeAttempts) {
      att.abort();
    }
    this.activeAttempts.clear();
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
    }, 3_000);
  }

  private decodeWireBytes(data: unknown): Uint8Array | null {
    if (typeof data === "string") {
      return base64ToBytes(data);
    }
    if (data instanceof ArrayBuffer) {
      return new Uint8Array(data);
    }
    if (ArrayBuffer.isView(data)) {
      return new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
    }
    return null;
  }

  private handleEncryptedInbound(data: unknown): void {
    if (!this.transport) return;
    try {
      const wire = this.decodeWireBytes(data);
      if (!wire) return;

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

  private closeTransport(): void {
    const tx = this.transport;
    this.transport = null;
    tx?.close();
  }

  private teardownSocket(): void {
    const sock = this.ws;
    this.ws = null;
    try {
      if (sock) {
        sock.onmessage = null;
        sock.onclose = null;
        sock.onerror = null;
        sock.close();
      }
    } catch {
      // ignore
    }
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
    this.connectRaceSeq += 1;
    this.abortInFlightAttempts();
    this.clearTimers();
    this.teardownSocket();
    this.transport?.close();
    this.transport = null;
    this.setState("disconnected");
  }
}
