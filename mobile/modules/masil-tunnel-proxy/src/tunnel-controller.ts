/**
 * Bridges phone-local `MasilTunnelProxy` TCP listeners (127.0.0.1:<ephemeral>)
 * to the host's Noise NK binary tunnel multiplexer (`0x20..0x24`), including
 * 32 KiB chunking, 256 KiB receive-window credit replenishment, and automatic
 * AppState foreground rebinding within 100ms (D-07).
 */
import { AppState, type AppStateStatus } from "react-native";
import { base64ToBytes, bytesToBase64 } from "../../../src/crypto/pairing";
import {
  getHostClient,
  subscribeHostBinary,
} from "../../../src/hosts/HostRuntimeController";
import {
  type BinaryFrame,
  BinaryOpcode,
  TUNNEL_MAX_CHUNK_BYTES,
  decodeJsonPayload,
  encodeJsonPayload,
  encodeTunnelWindowPayload,
  type TunnelOpenAckPayload,
} from "../../../src/protocol/binary-frames";
import {
  getNativeTunnelProxy,
  isNativeTunnelProxyAvailable,
  type TunnelListenerHandle,
} from "./index";

export interface ActiveTunnelSession {
  hostId: string;
  targetPort: number;
  localPort: number;
  localOrigin: string;
  rebind: () => Promise<number>;
  stop: () => Promise<void>;
}

/**
 * Starts a native phone-local loopback listener bound to `127.0.0.1:<ephemeral>`
 * that transparently proxies HTTP & WebSocket streams over E2EE Noise NK to the
 * host's `127.0.0.1:<targetPort>`.
 */
export async function startE2eeLocalhostTunnel(params: {
  hostId: string;
  targetPort: number;
  agentId?: string;
  onRebound?: (newLocalPort: number) => void;
  onStreamError?: (message: string) => void;
}): Promise<ActiveTunnelSession> {
  if (!isNativeTunnelProxyAvailable()) {
    throw new Error(
      "Native MasilTunnelProxy module is unavailable in this build. Rebuild the iOS/Android dev client with Expo prebuild."
    );
  }
  const proxy = getNativeTunnelProxy()!;
  const handle: TunnelListenerHandle = await proxy.startListener(
    params.targetPort
  );
  let currentLocalPort = handle.localPort;
  let stopped = false;

  const ownedStreams = new Set<number>();
  const pendingInbound = new Map<number, Uint8Array[]>();
  const openReady = new Set<number>();

  const flushPending = (streamId: number) => {
    const client = getHostClient(params.hostId);
    const queued = pendingInbound.get(streamId);
    if (!client || !queued) return;
    pendingInbound.delete(streamId);
    for (const buf of queued) {
      sendChunkedTunnelData(client, streamId, buf);
    }
  };

  const subs: Array<{ remove: () => void }> = [];

  if (proxy.addListener) {
    subs.push(
      proxy.addListener("onStreamOpen", (ev) => {
        if (stopped || ev.listenerId !== handle.listenerId) return;
        const streamId = Number(ev.streamId);
        if (!streamId) return;
        const client = getHostClient(params.hostId);
        if (!client || client.connectionState !== "connected") {
          proxy.closeStream(streamId);
          return;
        }
        ownedStreams.add(streamId);
        client.sendBinaryFrame({
          opcode: BinaryOpcode.TunnelOpen,
          channelId: streamId,
          payload: encodeJsonPayload({
            target_port: params.targetPort,
            ...(params.agentId ? { agent_id: params.agentId } : {}),
          }),
        });
      })
    );

    subs.push(
      proxy.addListener("onStreamData", (ev) => {
        if (stopped) return;
        const streamId = Number(ev.streamId);
        if (!ownedStreams.has(streamId)) return;
        const dataBase64 = String(ev.dataBase64 ?? "");
        if (!dataBase64) return;
        const bytes = base64ToBytes(dataBase64);
        if (!openReady.has(streamId)) {
          const list = pendingInbound.get(streamId) ?? [];
          list.push(bytes);
          pendingInbound.set(streamId, list);
          return;
        }
        const client = getHostClient(params.hostId);
        if (client) {
          sendChunkedTunnelData(client, streamId, bytes);
        }
      })
    );

    subs.push(
      proxy.addListener("onStreamClose", (ev) => {
        const streamId = Number(ev.streamId);
        if (!ownedStreams.delete(streamId)) return;
        openReady.delete(streamId);
        pendingInbound.delete(streamId);
        const client = getHostClient(params.hostId);
        client?.sendBinaryFrame({
          opcode: BinaryOpcode.TunnelClose,
          channelId: streamId,
          payload: encodeJsonPayload({ reason: String(ev.reason ?? "local_eof") }),
        });
      })
    );

    subs.push(
      proxy.addListener("onListenerRebound", (ev) => {
        if (stopped || ev.listenerId !== handle.listenerId) return;
        const nextPort = Number(ev.localPort);
        if (nextPort > 0 && nextPort !== currentLocalPort) {
          currentLocalPort = nextPort;
          params.onRebound?.(nextPort);
        }
      })
    );
  }

  const unsubBinary = subscribeHostBinary(params.hostId, (frame: BinaryFrame) => {
    if (stopped || !ownedStreams.has(frame.channelId)) return;
    switch (frame.opcode) {
      case BinaryOpcode.TunnelOpenAck: {
        try {
          const ack = decodeJsonPayload<TunnelOpenAckPayload>(frame.payload);
          if (!ack.ok) {
            ownedStreams.delete(frame.channelId);
            pendingInbound.delete(frame.channelId);
            proxy.closeStream(frame.channelId);
            if (ack.error) {
              params.onStreamError?.(ack.error);
            }
            return;
          }
          openReady.add(frame.channelId);
          flushPending(frame.channelId);
        } catch {
          ownedStreams.delete(frame.channelId);
          proxy.closeStream(frame.channelId);
        }
        break;
      }
      case BinaryOpcode.TunnelData: {
        if (frame.payload.length > 0) {
          proxy.writeStream(frame.channelId, bytesToBase64(frame.payload));
          // Immediately replenish host send window credit after handing off bytes.
          const client = getHostClient(params.hostId);
          client?.sendBinaryFrame({
            opcode: BinaryOpcode.TunnelWindow,
            channelId: frame.channelId,
            payload: encodeTunnelWindowPayload(frame.payload.length),
          });
        }
        break;
      }
      case BinaryOpcode.TunnelClose: {
        ownedStreams.delete(frame.channelId);
        openReady.delete(frame.channelId);
        pendingInbound.delete(frame.channelId);
        proxy.closeStream(frame.channelId);
        break;
      }
      default:
        break;
    }
  });

  // D-07: Auto-rebind reclaimed loopback listener within 100ms of returning to foreground.
  let rebindTimer: ReturnType<typeof setTimeout> | null = null;
  const appStateSub = AppState.addEventListener("change", (next: AppStateStatus) => {
    if (stopped || next !== "active") return;
    if (rebindTimer) clearTimeout(rebindTimer);
    rebindTimer = setTimeout(() => {
      if (stopped) return;
      void proxy
        .rebindListener(handle.listenerId)
        .then((res) => {
          if (!stopped && res.localPort > 0 && res.localPort !== currentLocalPort) {
            currentLocalPort = res.localPort;
            params.onRebound?.(res.localPort);
          }
        })
        .catch(() => {});
    }, 50);
  });

  const rebind = async (): Promise<number> => {
    if (stopped) return currentLocalPort;
    const res = await proxy.rebindListener(handle.listenerId);
    currentLocalPort = res.localPort;
    params.onRebound?.(res.localPort);
    return currentLocalPort;
  };

  const stop = async (): Promise<void> => {
    if (stopped) return;
    stopped = true;
    if (rebindTimer) clearTimeout(rebindTimer);
    appStateSub.remove();
    unsubBinary();
    for (const s of subs) s.remove();
    const client = getHostClient(params.hostId);
    for (const sid of ownedStreams) {
      proxy.closeStream(sid);
      client?.sendBinaryFrame({
        opcode: BinaryOpcode.TunnelClose,
        channelId: sid,
        payload: encodeJsonPayload({ reason: "tunnel_stopped" }),
      });
    }
    ownedStreams.clear();
    openReady.clear();
    pendingInbound.clear();
    await proxy.stopListener(handle.listenerId).catch(() => {});
  };

  return {
    hostId: params.hostId,
    targetPort: params.targetPort,
    get localPort() {
      return currentLocalPort;
    },
    get localOrigin() {
      return `http://127.0.0.1:${currentLocalPort}`;
    },
    rebind,
    stop,
  };
}

function sendChunkedTunnelData(
  client: NonNullable<ReturnType<typeof getHostClient>>,
  streamId: number,
  bytes: Uint8Array
): void {
  for (let offset = 0; offset < bytes.length; offset += TUNNEL_MAX_CHUNK_BYTES) {
    const end = Math.min(offset + TUNNEL_MAX_CHUNK_BYTES, bytes.length);
    client.sendBinaryFrame({
      opcode: BinaryOpcode.TunnelData,
      channelId: streamId,
      payload: bytes.subarray(offset, end),
    });
  }
}
