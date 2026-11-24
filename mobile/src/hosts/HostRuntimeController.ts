/**
 * Multi-host runtime controller owning saved hosts, E2EE clients, offline
 * SQLite cache hydration, unified attention counts, binary terminal streams,
 * and voice transcription callbacks.
 */
import { AppState, Platform } from "react-native";
import { create } from "zustand";
import { base64ToBytes, bytesToBase64, parsePairingUri } from "../crypto/pairing";
import { BinaryFrame } from "../protocol/binary-frames";
import { ConnectionState, MasilRemoteClient } from "../protocol/client";
import {
  AgentRow,
  InboxItem,
  SavedHost,
  TimelinePage,
} from "../protocol/schemas";
import {
  loadCachedAgents,
  loadCachedTimeline,
  saveCachedAgents,
  saveCachedTimeline,
} from "../storage/replica-db";
import {
  loadSavedHosts,
  removeHostRecord,
  saveHostRecord,
} from "../storage/secure-store";

export interface HostRuntimeState {
  host: SavedHost;
  connection: ConnectionState;
  lastError?: string;
  agents: AgentRow[];
  inbox: InboxItem[];
  capabilities: Record<string, unknown>;
  hooks: Array<{
    id: string;
    label: string;
    cli_installed: boolean;
    hook_installed: boolean;
    config_path: string;
  }>;
}

type BinaryListener = (frame: BinaryFrame) => void;
type VoiceListener = (event: {
  channelId: number;
  ok: boolean;
  text?: string;
  error?: string;
}) => void;

const clientPool = new Map<string, MasilRemoteClient>();
const pollTimers = new Map<string, ReturnType<typeof setInterval>>();
const inFlightRefresh = new Set<string>();
const inFlightTimeline = new Set<string>();
const binaryListeners = new Map<string, Set<BinaryListener>>();
const voiceListeners = new Set<VoiceListener>();
let appStateWired = false;
let appInForeground = true;

function startHostPolling(hostId: string): void {
  stopHostPolling(hostId);
  const id = setInterval(() => {
    if (!appInForeground) return;
    void useHostRuntimeStore.getState().refreshHost(hostId, false);
  }, 2500);
  pollTimers.set(hostId, id);
}

function timelineSignature(page: TimelinePage): string {
  const last = page.items[page.items.length - 1];
  return [
    page.binding_confidence,
    page.session_id ?? "",
    page.source_label,
    page.detected_model ?? "",
    page.items.length,
    last?.id ?? "",
    last?.seq ?? 0,
    last && "markdown" in last ? last.markdown.length : "",
    last && "text" in last ? last.text.length : "",
    last && "output" in last ? (last.output?.length ?? 0) : "",
    last && "status" in last ? last.status : "",
  ].join("|");
}

function applyAgentsSnapshot(
  hostId: string,
  rawAgents: AgentRow[],
  extra?: {
    inbox?: InboxItem[];
    capabilities?: Record<string, unknown>;
    hooks?: HostRuntimeState["hooks"];
  }
): void {
  const agents = rawAgents.map(normalizeAgentRow);
  const prevState = useHostRuntimeStore.getState();
  const prevHost = prevState.hosts[hostId];
  if (!prevHost) return;

  const activeId = prevState.activeAgentRemoteId;
  const prevActive = activeId
    ? prevHost.agents.find((a) => a.remote_id === activeId)
    : undefined;
  const nextActive = activeId
    ? agents.find((a) => a.remote_id === activeId)
    : undefined;

  void saveCachedAgents(hostId, agents);
  useHostRuntimeStore.setState((s) => {
    const prev = s.hosts[hostId];
    if (!prev) return s;
    const stillExists =
      s.activeAgentRemoteId !== null &&
      agents.some((a) => a.remote_id === s.activeAgentRemoteId);
    const nextActiveAgent =
      s.activeHostId === hostId
        ? stillExists
          ? s.activeAgentRemoteId
          : agents[0]?.remote_id ?? null
        : s.activeAgentRemoteId;
    return {
      hosts: {
        ...s.hosts,
        [hostId]: {
          ...prev,
          agents,
          inbox: extra?.inbox ?? prev.inbox,
          capabilities: extra?.capabilities ?? prev.capabilities,
          hooks: extra?.hooks ?? prev.hooks,
        },
      },
      activeAgentRemoteId: nextActiveAgent,
    };
  });

  // Whenever the active agent changes state, cwd, or session binding, refresh
  // its timeline immediately instead of waiting for the next poll tick.
  if (
    nextActive &&
    (!prevActive ||
      prevActive.state !== nextActive.state ||
      prevActive.session_id !== nextActive.session_id ||
      prevActive.cwd !== nextActive.cwd)
  ) {
    void useHostRuntimeStore.getState().refreshTimeline(hostId, nextActive);
  }
}

function stopHostPolling(hostId: string): void {
  const existing = pollTimers.get(hostId);
  if (existing) {
    clearInterval(existing);
    pollTimers.delete(hostId);
  }
}

function normalizeAgentRow(raw: AgentRow): AgentRow {
  const pane = raw.pane || raw.pane_id || "";
  const serverName = raw.server_name || "default";
  const remoteId =
    raw.remote_id && !raw.remote_id.endsWith("::")
      ? raw.remote_id
      : `${serverName}::${pane || raw.name}`;
  return {
    ...raw,
    pane,
    pane_id: pane,
    remote_id: remoteId,
  };
}

export type VoiceSttMode = "os" | "host";

interface RuntimeStore {
  initialized: boolean;
  hosts: Record<string, HostRuntimeState>;
  hostOrder: string[];
  activeHostId: string | null;
  activeAgentRemoteId: string | null;
  timelines: Record<string, TimelinePage>;
  voiceMode: VoiceSttMode;
  composerAttachment: string | null;
  bootstrap: () => Promise<void>;
  pairFromUri: (uri: string, deviceName?: string) => Promise<SavedHost>;
  removeHost: (hostId: string) => Promise<void>;
  selectHost: (hostId: string) => void;
  selectAgent: (hostId: string, remoteId: string) => void;
  refreshHost: (hostId: string, full?: boolean) => Promise<void>;
  refreshTimeline: (hostId: string, agent: AgentRow) => Promise<void>;
  setVoiceMode: (mode: VoiceSttMode) => void;
  setComposerAttachment: (path: string | null) => void;
}

export function getHostClient(hostId: string): MasilRemoteClient | null {
  return clientPool.get(hostId) ?? null;
}

export function subscribeHostBinary(
  hostId: string,
  listener: BinaryListener
): () => void {
  let set = binaryListeners.get(hostId);
  if (!set) {
    set = new Set();
    binaryListeners.set(hostId, set);
  }
  set.add(listener);
  return () => {
    set?.delete(listener);
  };
}

export function subscribeVoiceTranscriptions(
  listener: VoiceListener
): () => void {
  voiceListeners.add(listener);
  return () => {
    voiceListeners.delete(listener);
  };
}

export const useHostRuntimeStore = create<RuntimeStore>((set, get) => ({
  initialized: false,
  hosts: {},
  hostOrder: [],
  activeHostId: null,
  activeAgentRemoteId: null,
  timelines: {},
  voiceMode: "os",
  composerAttachment: null,

  bootstrap: async () => {
    if (get().initialized) return;
    const saved = await loadSavedHosts();
    const initialHosts: Record<string, HostRuntimeState> = {};
    const order: string[] = [];

    for (const host of saved) {
      const cachedAgents = await loadCachedAgents(host.hostId);
      initialHosts[host.hostId] = {
        host,
        connection: "disconnected",
        agents: cachedAgents,
        inbox: [],
        capabilities: {},
        hooks: [],
      };
      order.push(host.hostId);
    }

    set({
      initialized: true,
      hosts: initialHosts,
      hostOrder: order,
      activeHostId: order[0] ?? null,
    });

    for (const host of saved) {
      connectManagedHost(host);
    }

    if (!appStateWired) {
      appStateWired = true;
      AppState.addEventListener("change", (next) => {
        appInForeground = next === "active";
        if (next === "active") {
          for (const [hid, client] of clientPool.entries()) {
            void client.onAppForeground();
            void useHostRuntimeStore.getState().refreshHost(hid);
          }
        }
      });
    }
  },

  pairFromUri: async (uri: string, deviceName?: string) => {
    const offer = parsePairingUri(uri);
    const platform =
      Platform.OS === "ios"
        ? "ios"
        : Platform.OS === "android"
          ? "android"
          : "web";

    const tempClient = new MasilRemoteClient({
      hostId: offer.hostId,
      hostPub: offer.hostPub,
      relayUrl: offer.relayUrl,
      directAddr: offer.directAddr,
      pairToken: offer.pairToken,
      deviceName: deviceName ?? `masil ${platform}`,
      platform,
    });

    const authRes = await tempClient.connect();
    tempClient.disconnect();

    const deviceId = String(authRes.device_id ?? "");
    const deviceSecret = String(authRes.device_secret ?? "");
    if (!deviceId || !deviceSecret) {
      throw new Error("Host returned incomplete pairing credentials");
    }

    const record: SavedHost = {
      hostId: offer.hostId,
      hostName: String(authRes.host_name ?? offer.hostName),
      hostPubB64: bytesToBase64(offer.hostPub),
      deviceId,
      deviceSecret,
      relayUrl: offer.relayUrl,
      directAddr: offer.directAddr,
    };

    const nextList = await saveHostRecord(record);
    set((s) => ({
      hosts: {
        ...s.hosts,
        [record.hostId]: {
          host: record,
          connection: "disconnected",
          agents: [],
          inbox: [],
          capabilities: {},
          hooks: [],
        },
      },
      hostOrder: nextList.map((h) => h.hostId),
      activeHostId: record.hostId,
    }));

    connectManagedHost(record);
    return record;
  },

  removeHost: async (hostId: string) => {
    stopHostPolling(hostId);
    clientPool.get(hostId)?.disconnect();
    clientPool.delete(hostId);
    const nextList = await removeHostRecord(hostId);
    set((s) => {
      const nextHosts = { ...s.hosts };
      delete nextHosts[hostId];
      const nextActive =
        s.activeHostId === hostId ? nextList[0]?.hostId ?? null : s.activeHostId;
      return {
        hosts: nextHosts,
        hostOrder: nextList.map((h) => h.hostId),
        activeHostId: nextActive,
        activeAgentRemoteId:
          s.activeHostId === hostId ? null : s.activeAgentRemoteId,
      };
    });
  },

  selectHost: (hostId: string) => {
    set({ activeHostId: hostId, activeAgentRemoteId: null });
    void get().refreshHost(hostId);
  },

  selectAgent: (hostId: string, remoteId: string) => {
    set({ activeHostId: hostId, activeAgentRemoteId: remoteId });
    const hostState = get().hosts[hostId];
    const agent = hostState?.agents.find((a) => a.remote_id === remoteId);
    if (agent) {
      void loadCachedTimeline(hostId, remoteId).then((cached) => {
        if (cached) {
          set((s) => ({
            timelines: {
              ...s.timelines,
              [`${hostId}::${remoteId}`]: cached,
            },
          }));
        }
      });
      void get().refreshTimeline(hostId, agent);
    }
  },

  refreshHost: async (hostId: string, full = true) => {
    const client = clientPool.get(hostId);
    if (!client || client.connectionState !== "connected") return;
    if (inFlightRefresh.has(hostId)) return;
    inFlightRefresh.add(hostId);
    try {
      const agentsPromise = client.request<{ agents?: AgentRow[] }>(
        "agent.list.request"
      );
      const inboxPromise = client.request<{ items?: InboxItem[] }>(
        "inbox.list.request",
        { all: false }
      );
      const infoPromise = full
        ? client.request<{
            capabilities?: Record<string, unknown>;
            hooks?: HostRuntimeState["hooks"];
          }>("host.info.request")
        : Promise.resolve(null);

      const [infoRes, agentsRes, inboxRes] = await Promise.all([
        infoPromise,
        agentsPromise,
        inboxPromise,
      ]);
      applyAgentsSnapshot(hostId, agentsRes.agents ?? [], {
        inbox: inboxRes.items ?? [],
        capabilities: infoRes?.capabilities,
        hooks: infoRes?.hooks,
      });
    } catch {
      // Retain last-known evidence on transient RPC error.
    } finally {
      inFlightRefresh.delete(hostId);
    }
  },

  refreshTimeline: async (hostId: string, agent: AgentRow) => {
    const client = clientPool.get(hostId);
    if (!client || client.connectionState !== "connected") return;
    const key = `${hostId}::${agent.remote_id}`;
    if (inFlightTimeline.has(key)) return;
    inFlightTimeline.add(key);
    try {
      const page = await client.request<TimelinePage>(
        "agent.timeline.fetch.request",
        {
          server_socket: agent.server_socket,
          pane: agent.pane,
          provider: agent.provider,
          cwd: agent.cwd,
          session_id: agent.session_id ?? null,
          state: agent.state,
          limit: 200,
        }
      );
      const prev = get().timelines[key];
      if (prev && timelineSignature(prev) === timelineSignature(page)) {
        return;
      }
      void saveCachedTimeline(hostId, agent.remote_id, page);
      set((s) => ({
        timelines: {
          ...s.timelines,
          [key]: page,
        },
      }));
    } catch {
      // Keep last cached timeline.
    } finally {
      inFlightTimeline.delete(key);
    }
  },

  setVoiceMode: (mode) => set({ voiceMode: mode }),
  setComposerAttachment: (path) => set({ composerAttachment: path }),
}));

function connectManagedHost(host: SavedHost): void {
  clientPool.get(host.hostId)?.disconnect();
  const platform =
    Platform.OS === "ios"
      ? "ios"
      : Platform.OS === "android"
        ? "android"
        : "web";

  const client = new MasilRemoteClient({
    hostId: host.hostId,
    hostPub: base64ToBytes(host.hostPubB64),
    relayUrl: host.relayUrl,
    directAddr: host.directAddr,
    deviceId: host.deviceId,
    deviceSecret: host.deviceSecret,
    platform,
    onStateChange: (connection, lastError) => {
      useHostRuntimeStore.setState((s) => {
        const prev = s.hosts[host.hostId];
        if (!prev) return s;
        return {
          hosts: {
            ...s.hosts,
            [host.hostId]: { ...prev, connection, lastError },
          },
        };
      });
      if (connection === "connected") {
        startHostPolling(host.hostId);
        void useHostRuntimeStore.getState().refreshHost(host.hostId);
      } else if (connection === "disconnected") {
        stopHostPolling(host.hostId);
      }
    },
    onBinaryFrame: (frame) => {
      const listeners = binaryListeners.get(host.hostId);
      if (!listeners) return;
      for (const fn of listeners) fn(frame);
    },
    onEvent: (event) => {
      if (event.event === "agents.changed" && Array.isArray(event.agents)) {
        applyAgentsSnapshot(host.hostId, event.agents as AgentRow[]);
        return;
      }
      if (event.event === "voice.transcription") {
        for (const fn of voiceListeners) {
          fn({
            channelId: Number(event.channel_id ?? 0),
            ok: event.ok === true,
            text: typeof event.text === "string" ? event.text : undefined,
            error: typeof event.error === "string" ? event.error : undefined,
          });
        }
      }
    },
  });

  clientPool.set(host.hostId, client);
  void client.connect().catch(() => {});
}
