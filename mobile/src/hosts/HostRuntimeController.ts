/**
 * Multi-host runtime controller owning saved hosts, E2EE clients, offline
 * SQLite cache hydration, unified attention counts, binary terminal streams,
 * and voice transcription callbacks.
 */
import { AppState, Platform } from "react-native";
import i18next from "i18next";
import { create } from "zustand";
import { base64ToBytes, bytesToBase64, parsePairingUri } from "../crypto/pairing";
import { useMobilePanelsStore } from "../mobile-panels/store";
import {
  type AgentNotificationEvent,
  DEFAULT_NOTIFICATION_PREFERENCES,
  NotificationDeduplicator,
  type NotificationPreferences,
  countAgentsNeedingAttention,
  detectAgentStateTransitions,
  detectNewInboxNotifications,
  detectNewPromptNotifications,
  inboxItemKey,
  normalizeNotificationKind,
  shouldDeliverNotification,
} from "../notifications/agent-notifications";
import {
  deliverLocalAgentNotification,
  flushPendingNotificationDeepLink,
  registerPushTokenWithHost,
  updateAppBadgeCount,
} from "../notifications/push-manager";
import { BinaryFrame } from "../protocol/binary-frames";
import { ConnectionState, MasilRemoteClient } from "../protocol/client";
import {
  mergeAdvertisedDirectAddrs,
  normalizeDirectAddrList,
} from "../protocol/endpoints";
import {
  DEFAULT_LIVE_PULSE_PREFERENCES,
  projectLivePulseForDevice,
} from "../agents/phase2-helpers";
import {
  endNativeLivePulse,
  setLivePulsePushToStartObserver,
  subscribeLivePulseTokens,
  updateNativeLivePulse,
} from "../../modules/masil-live-pulse/src";
import {
  AgentRow,
  AssetUploadResultEvent,
  BindingConfidence,
  FocusShieldStatus,
  InboxItem,
  InteractivePrompt,
  LivePulsePayload,
  LivePulsePreferences,
  PluginCatalog,
  SavedHost,
  TimelineItem,
  TimelinePage,
} from "../protocol/schemas";
import {
  loadCachedAgents,
  loadCachedTimeline,
  saveCachedAgents,
  saveCachedTimeline,
} from "../storage/replica-db";
import {
  loadLivePulsePreferences,
  loadNotificationPreferences,
  loadSavedHosts,
  removeHostRecord,
  saveHostRecord,
  saveLivePulsePreferences,
  saveNotificationPreferences,
} from "../storage/secure-store";

export interface HostRuntimeState {
  host: SavedHost;
  connection: ConnectionState;
  activeEndpoint?: string | null;
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
  plugins: PluginCatalog | null;
  focusStatus?: FocusShieldStatus | null;
}

type BinaryListener = (frame: BinaryFrame) => void;
type VoiceListener = (event: {
  channelId: number;
  ok: boolean;
  text?: string;
  error?: string;
}) => void;
type AssetUploadListener = (event: AssetUploadResultEvent) => void;

const clientPool = new Map<string, MasilRemoteClient>();
const pollTimers = new Map<string, ReturnType<typeof setInterval>>();
const inFlightRefresh = new Set<string>();
const inFlightTimeline = new Set<string>();
const binaryListeners = new Map<string, Set<BinaryListener>>();
const voiceListeners = new Set<VoiceListener>();
const assetUploadListeners = new Set<AssetUploadListener>();
const baselineSeededHosts = new Set<string>();
const inboxSeededHosts = new Set<string>();
const seenInboxKeys = new Set<string>();
const notificationDeduper = new NotificationDeduplicator();
let appStateWired = false;
let livePulseTokensWired = false;
let latestPushToStartToken: string | null = null;
const latestActivityTokens = new Map<string, string>();
let appInForeground = true;
let permissionPrompted = false;

function dispatchAgentNotification(event: AgentNotificationEvent): void {
  const state = useHostRuntimeStore.getState();
  const activePanel = useMobilePanelsStore.getState().target;
  if (
    !shouldDeliverNotification(event, state.notificationPrefs, {
      appInForeground,
      activePanel,
      activeHostId: state.activeHostId,
      activeAgentRemoteId: state.activeAgentRemoteId,
    })
  ) {
    return;
  }
  if (!notificationDeduper.shouldEmit(event)) {
    return;
  }
  const lang = i18next.language || "ko";
  void deliverLocalAgentNotification(event, lang);
}

function syncAttentionBadge(): void {
  const hosts = useHostRuntimeStore.getState().hosts;
  let total = 0;
  for (const h of Object.values(hosts)) {
    total += countAgentsNeedingAttention(h.agents);
  }
  void updateAppBadgeCount(total);
}

function sameStringArray(a: string[] | undefined, b: string[] | undefined): boolean {
  const la = a ?? [];
  const lb = b ?? [];
  if (la.length !== lb.length) return false;
  for (let i = 0; i < la.length; i += 1) {
    if (la[i] !== lb[i]) return false;
  }
  return true;
}

function syncHostEndpointsFromServer(
  hostId: string,
  advertisedDirectAddrs: unknown,
  advertisedRelayUrl: unknown
): void {
  const rawDirect = Array.isArray(advertisedDirectAddrs)
    ? advertisedDirectAddrs.filter((v): v is string => typeof v === "string")
    : [];
  const rawRelay =
    typeof advertisedRelayUrl === "string" && advertisedRelayUrl.trim().length > 0
      ? advertisedRelayUrl.trim()
      : undefined;
  if (rawDirect.length === 0 && !rawRelay) return;

  const prevState = useHostRuntimeStore.getState().hosts[hostId];
  if (!prevState) return;
  const prevHost = prevState.host;
  const existingDirect = normalizeDirectAddrList([
    prevHost.directAddr,
    ...(prevHost.directAddrs ?? []),
  ]);
  const nextDirectAddrs =
    rawDirect.length > 0
      ? mergeAdvertisedDirectAddrs(existingDirect, rawDirect)
      : existingDirect;
  const nextDirectAddr = nextDirectAddrs[0];
  const nextRelayUrl = rawRelay ?? prevHost.relayUrl;

  if (
    prevHost.directAddr === nextDirectAddr &&
    sameStringArray(prevHost.directAddrs, nextDirectAddrs) &&
    prevHost.relayUrl === nextRelayUrl
  ) {
    return;
  }

  const updatedHost: SavedHost = {
    ...prevHost,
    directAddr: nextDirectAddr,
    directAddrs: nextDirectAddrs,
    relayUrl: nextRelayUrl,
  };

  useHostRuntimeStore.setState((s) => {
    const cur = s.hosts[hostId];
    if (!cur) return s;
    return {
      hosts: {
        ...s.hosts,
        [hostId]: {
          ...cur,
          host: updatedHost,
        },
      },
    };
  });
  void saveHostRecord(updatedHost);
  clientPool.get(hostId)?.updateEndpoints({
    directAddr: nextDirectAddr,
    directAddrs: nextDirectAddrs,
    relayUrl: nextRelayUrl,
  });
}

function startHostPolling(hostId: string): void {
  stopHostPolling(hostId);
  const id = setInterval(() => {
    if (!appInForeground) return;
    void useHostRuntimeStore.getState().refreshHost(hostId, false);
  }, 2500);
  pollTimers.set(hostId, id);
}

function promptsSignature(prompts?: InteractivePrompt[]): string {
  if (!prompts || prompts.length === 0) return "";
  return prompts
    .map(
      (p) =>
        `${p.id}:${p.kind}:${p.mode}:${p.selected_index}:${p.multi_select ? 1 : 0}:${p.free_text_only ? 1 : 0}:${p.body.length}:${p.screen_excerpt?.length ?? 0}:${p.options
          .map(
            (o) =>
              `${o.index}=${o.selected ? 1 : 0}/${o.checked ? 1 : 0}:${o.label.length}`
          )
          .join(",")}`
    )
    .join(";");
}

export function timelineSignature(page: TimelinePage): string {
  const last = page.items[page.items.length - 1];
  const pluginCardsSig = page.items
    .filter((it) => it.kind === "plugin_card")
    .map((it) => {
      const card = it as Extract<TimelinePage["items"][number], { kind: "plugin_card" }>;
      return `${card.id}:${card.ts_ms}:${card.title.length}:${card.blocks.length}`;
    })
    .join(",");
  return [
    page.binding_confidence,
    page.session_id ?? "",
    page.source_label,
    page.detected_model ?? "",
    page.items.length,
    last?.id ?? "",
    last?.seq ?? 0,
    last?.ts_ms ?? 0,
    last && "markdown" in last ? last.markdown.length : "",
    last && "text" in last ? last.text.length : "",
    last && "output" in last ? (last.output?.length ?? 0) : "",
    last && "status" in last ? last.status : "",
    pluginCardsSig,
    promptsSignature(page.active_prompts),
  ].join("|");
}

import {
  applyTimelineDeltaToPage,
  type TimelineDeltaEvent,
} from "./timeline-delta";
export { applyTimelineDeltaToPage, type TimelineDeltaEvent };

function applyAgentsSnapshot(
  hostId: string,
  rawAgents: AgentRow[],
  extra?: {
    inbox?: InboxItem[];
    capabilities?: Record<string, unknown>;
    hooks?: HostRuntimeState["hooks"];
    plugins?: PluginCatalog | null;
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

  // Detect live state transitions & new inbox events only after establishing
  // the host's first live baseline snapshot (never on offline cache hydration).
  const pendingNotifications: AgentNotificationEvent[] = [];
  const now = Date.now();
  if (!baselineSeededHosts.has(hostId)) {
    baselineSeededHosts.add(hostId);
  } else {
    pendingNotifications.push(
      ...detectAgentStateTransitions(
        hostId,
        prevHost.host.hostName,
        prevHost.agents,
        agents,
        now
      )
    );
  }
  if (extra?.inbox) {
    if (!inboxSeededHosts.has(hostId)) {
      inboxSeededHosts.add(hostId);
      for (const item of extra.inbox) {
        seenInboxKeys.add(inboxItemKey(hostId, item));
      }
    } else {
      pendingNotifications.push(
        ...detectNewInboxNotifications(
          hostId,
          prevHost.host.hostName,
          seenInboxKeys,
          extra.inbox,
          agents,
          now
        )
      );
    }
  }

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
          plugins:
            extra?.plugins !== undefined ? extra.plugins : prev.plugins,
        },
      },
      activeAgentRemoteId: nextActiveAgent,
    };
  });

  for (const evt of pendingNotifications) {
    dispatchAgentNotification(evt);
  }
  syncAttentionBadge();
  flushPendingNotificationDeepLink();

  // Whenever the active agent changes state, cwd, session binding, or pending
  // request counts, refresh its timeline immediately instead of waiting for the next poll tick.
  if (
    nextActive &&
    (!prevActive ||
      prevActive.state !== nextActive.state ||
      prevActive.session_id !== nextActive.session_id ||
      prevActive.cwd !== nextActive.cwd ||
      (prevActive.permission_count ?? 0) !== (nextActive.permission_count ?? 0) ||
      (prevActive.question_count ?? 0) !== (nextActive.question_count ?? 0))
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
  composerDraftPrefill: string | null;
  notificationPrefs: NotificationPreferences;
  livePulsePrefs: LivePulsePreferences;
  bootstrap: () => Promise<void>;
  pairFromUri: (uri: string, deviceName?: string) => Promise<SavedHost>;
  removeHost: (hostId: string) => Promise<void>;
  selectHost: (hostId: string) => void;
  selectAgent: (hostId: string, remoteId: string) => void;
  refreshHost: (hostId: string, full?: boolean) => Promise<void>;
  refreshTimeline: (hostId: string, agent: AgentRow) => Promise<void>;
  subscribeTimeline: (hostId: string, agent: AgentRow) => Promise<() => void>;
  patchTimelinePrompts: (
    hostId: string,
    remoteId: string,
    prompts: InteractivePrompt[]
  ) => void;
  setVoiceMode: (mode: VoiceSttMode) => void;
  setComposerAttachment: (path: string | null) => void;
  setComposerDraftPrefill: (text: string | null) => void;
  setNotificationPrefs: (patch: Partial<NotificationPreferences>) => void;
  setLivePulsePrefs: (patch: Partial<LivePulsePreferences>) => Promise<void>;
  refreshFocusStatus: (hostId: string) => Promise<FocusShieldStatus | null>;
  setFocusShield: (
    hostId: string,
    active: boolean,
    duration?: string,
    reason?: string
  ) => Promise<FocusShieldStatus | null>;
  setHostPlugins: (hostId: string, plugins: PluginCatalog) => void;
  refreshPlugins: (hostId: string) => Promise<void>;
}

export function subscribeAssetUploadResults(
  listener: AssetUploadListener
): () => void {
  assetUploadListeners.add(listener);
  return () => {
    assetUploadListeners.delete(listener);
  };
}

async function syncHostLivePulseRegistration(
  hostId: string,
  prefs: LivePulsePreferences
): Promise<void> {
  const client = clientPool.get(hostId);
  if (!client || client.connectionState !== "connected") return;
  const platform =
    Platform.OS === "ios"
      ? "ios"
      : Platform.OS === "android"
        ? "android"
        : "web";
  try {
    await client.request("host.live_pulse.register.request", {
      platform,
      enabled: prefs.enabled,
      summary_enabled: prefs.enabled && prefs.summaryEnabled,
      push_to_start_token: prefs.enabled ? latestPushToStartToken : undefined,
      push_token: prefs.enabled ? latestActivityTokens.get(hostId) : undefined,
    });
  } catch {
    // Host may not advertise Phase 2B live pulse yet.
  }
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
  composerDraftPrefill: null,
  notificationPrefs: { ...DEFAULT_NOTIFICATION_PREFERENCES },
  livePulsePrefs: { ...DEFAULT_LIVE_PULSE_PREFERENCES },

  bootstrap: async () => {
    if (get().initialized) return;
    const [saved, savedNotifPrefs, savedLivePulsePrefs] = await Promise.all([
      loadSavedHosts(),
      loadNotificationPreferences(),
      loadLivePulsePreferences(),
    ]);
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
        plugins: null,
        focusStatus: null,
      };
      order.push(host.hostId);
    }

    set({
      initialized: true,
      hosts: initialHosts,
      hostOrder: order,
      activeHostId: order[0] ?? null,
      notificationPrefs: savedNotifPrefs,
      livePulsePrefs: savedLivePulsePrefs,
    });
    flushPendingNotificationDeepLink();

    if (!livePulseTokensWired) {
      livePulseTokensWired = true;
      subscribeLivePulseTokens((ev) => {
        const curPrefs = useHostRuntimeStore.getState().livePulsePrefs;
        if (!curPrefs.enabled) return;
        if (ev.kind === "push_to_start") {
          latestPushToStartToken = ev.token;
          for (const hid of useHostRuntimeStore.getState().hostOrder) {
            void syncHostLivePulseRegistration(hid, curPrefs);
          }
        } else if (ev.hostId) {
          latestActivityTokens.set(ev.hostId, ev.token);
          void syncHostLivePulseRegistration(ev.hostId, curPrefs);
        }
      });
    }
    if (savedLivePulsePrefs.enabled) {
      void setLivePulsePushToStartObserver(true);
    }

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
      directAddrs: offer.directAddrs,
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

    const advertisedDirect = Array.isArray(authRes.direct_addrs)
      ? authRes.direct_addrs.filter((v): v is string => typeof v === "string")
      : [];
    const directAddrs = mergeAdvertisedDirectAddrs(
      offer.directAddrs,
      advertisedDirect
    );
    const advertisedRelay =
      typeof authRes.relay_url === "string" && authRes.relay_url.trim().length > 0
        ? authRes.relay_url.trim()
        : undefined;

    const record: SavedHost = {
      hostId: offer.hostId,
      hostName: String(authRes.host_name ?? offer.hostName),
      hostPubB64: bytesToBase64(offer.hostPub),
      deviceId,
      deviceSecret,
      relayUrl: advertisedRelay ?? offer.relayUrl,
      directAddr: directAddrs[0] ?? offer.directAddr,
      directAddrs,
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
          plugins: null,
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
    baselineSeededHosts.delete(hostId);
    inboxSeededHosts.delete(hostId);
    notificationDeduper.clearHost(hostId);
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
    syncAttentionBadge();
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
            direct_addrs?: string[];
            relay_url?: string | null;
          }>("host.info.request")
        : Promise.resolve(null);
      const needsPlugins = full || !get().hosts[hostId]?.plugins;
      const pluginsPromise = needsPlugins
        ? client
            .request<PluginCatalog>("plugin.list.request")
            .catch(() => undefined)
        : Promise.resolve(undefined);

      const [infoRes, agentsRes, inboxRes, pluginsRes] = await Promise.all([
        infoPromise,
        agentsPromise,
        inboxPromise,
        pluginsPromise,
      ]);
      if (infoRes) {
        syncHostEndpointsFromServer(
          hostId,
          infoRes.direct_addrs,
          infoRes.relay_url
        );
      }
      applyAgentsSnapshot(hostId, agentsRes.agents ?? [], {
        inbox: inboxRes.items ?? [],
        capabilities: infoRes?.capabilities,
        hooks: infoRes?.hooks,
        plugins: pluginsRes,
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
      if (prev && baselineSeededHosts.has(hostId)) {
        const hostName = get().hosts[hostId]?.host.hostName ?? "";
        const promptEvents = detectNewPromptNotifications(
          hostId,
          hostName,
          agent,
          prev.active_prompts,
          page.active_prompts
        );
        for (const evt of promptEvents) {
          dispatchAgentNotification(evt);
        }
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

  subscribeTimeline: async (hostId: string, agent: AgentRow) => {
    const client = clientPool.get(hostId);
    if (!client || client.connectionState !== "connected") {
      return () => {};
    }
    const subId = `tl::${agent.remote_id}`;
    const key = `${hostId}::${agent.remote_id}`;
    try {
      const res = await client.request<{ sub_id?: string; page?: TimelinePage }>(
        "agent.timeline.subscribe.request",
        {
          sub_id: subId,
          remote_id: agent.remote_id,
          server_socket: agent.server_socket,
          pane: agent.pane,
          provider: agent.provider,
          cwd: agent.cwd,
          session_id: agent.session_id ?? null,
          state: agent.state,
          limit: 200,
        }
      );
      if (res.page) {
        const prev = get().timelines[key];
        if (!prev || timelineSignature(prev) !== timelineSignature(res.page)) {
          void saveCachedTimeline(hostId, agent.remote_id, res.page);
          set((s) => ({
            timelines: {
              ...s.timelines,
              [key]: res.page!,
            },
          }));
        }
      }
      const activeSubId = res.sub_id ?? subId;
      return () => {
        const currentClient = clientPool.get(hostId);
        if (currentClient && currentClient.connectionState === "connected") {
          void currentClient
            .request("agent.timeline.unsubscribe.request", { sub_id: activeSubId })
            .catch(() => {});
        }
      };
    } catch {
      return () => {};
    }
  },

  patchTimelinePrompts: (hostId, remoteId, prompts) => {
    const key = `${hostId}::${remoteId}`;
    set((s) => {
      const prev = s.timelines[key];
      if (!prev) return s;
      return {
        timelines: {
          ...s.timelines,
          [key]: {
            ...prev,
            active_prompts: prompts,
          },
        },
      };
    });
  },

  setVoiceMode: (mode) => set({ voiceMode: mode }),
  setComposerAttachment: (path) => set({ composerAttachment: path }),
  setComposerDraftPrefill: (text) => set({ composerDraftPrefill: text }),
  setNotificationPrefs: (patch) => {
    const next: NotificationPreferences = {
      ...get().notificationPrefs,
      ...patch,
    };
    set({ notificationPrefs: next });
    void saveNotificationPreferences(next);
  },
  setLivePulsePrefs: async (patch) => {
    const prev = get().livePulsePrefs;
    const enabled =
      patch.enabled !== undefined ? patch.enabled : prev.enabled;
    const summaryEnabled =
      enabled &&
      (patch.summaryEnabled !== undefined
        ? patch.summaryEnabled
        : prev.summaryEnabled);
    const next: LivePulsePreferences = { enabled, summaryEnabled };
    set({ livePulsePrefs: next });
    await saveLivePulsePreferences(next);
    await setLivePulsePushToStartObserver(enabled);
    if (!enabled) {
      await endNativeLivePulse();
    }
    for (const hid of get().hostOrder) {
      await syncHostLivePulseRegistration(hid, next);
    }
  },
  refreshFocusStatus: async (hostId) => {
    const client = clientPool.get(hostId);
    if (!client || client.connectionState !== "connected") return null;
    try {
      const res = await client.request<{ focus?: FocusShieldStatus }>(
        "focus.status.request"
      );
      const focusStatus = res.focus ?? null;
      set((s) => {
        const prev = s.hosts[hostId];
        if (!prev) return s;
        return {
          hosts: {
            ...s.hosts,
            [hostId]: { ...prev, focusStatus },
          },
        };
      });
      return focusStatus;
    } catch {
      return null;
    }
  },
  setFocusShield: async (hostId, active, duration = "25m", reason) => {
    const client = clientPool.get(hostId);
    if (!client || client.connectionState !== "connected") return null;
    const res = await client.request<{ focus?: FocusShieldStatus }>(
      "focus.set.request",
      { active, duration, reason }
    );
    const focusStatus = res.focus ?? null;
    set((s) => {
      const prev = s.hosts[hostId];
      if (!prev) return s;
      return {
        hosts: {
          ...s.hosts,
          [hostId]: { ...prev, focusStatus },
        },
      };
    });
    return focusStatus;
  },
  setHostPlugins: (hostId, plugins) =>
    set((s) => {
      const prev = s.hosts[hostId];
      if (!prev) return s;
      return {
        hosts: {
          ...s.hosts,
          [hostId]: { ...prev, plugins },
        },
      };
    }),
  refreshPlugins: async (hostId) => {
    const client = clientPool.get(hostId);
    if (!client || client.connectionState !== "connected") return;
    try {
      const catalog = await client.request<PluginCatalog>("plugin.list.request");
      get().setHostPlugins(hostId, catalog);
    } catch {
      // Keep last known catalog.
    }
  },
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
    directAddrs: host.directAddrs,
    deviceId: host.deviceId,
    deviceSecret: host.deviceSecret,
    platform,
    onAuthResult: (authRes) => {
      syncHostEndpointsFromServer(
        host.hostId,
        authRes.direct_addrs,
        authRes.relay_url
      );
    },
    onStateChange: (connection, lastError, activeEndpoint) => {
      useHostRuntimeStore.setState((s) => {
        const prev = s.hosts[host.hostId];
        if (!prev) return s;
        return {
          hosts: {
            ...s.hosts,
            [host.hostId]: {
              ...prev,
              connection,
              lastError,
              activeEndpoint: activeEndpoint ?? null,
            },
          },
        };
      });
      if (connection === "connected") {
        startHostPolling(host.hostId);
        void useHostRuntimeStore.getState().refreshHost(host.hostId);
        void useHostRuntimeStore.getState().refreshFocusStatus(host.hostId);
        const curLivePrefs = useHostRuntimeStore.getState().livePulsePrefs;
        if (curLivePrefs.enabled) {
          void syncHostLivePulseRegistration(host.hostId, curLivePrefs);
        }
        const promptIfUndetermined =
          !permissionPrompted &&
          useHostRuntimeStore.getState().notificationPrefs.enabled;
        if (promptIfUndetermined) {
          permissionPrompted = true;
        }
        void registerPushTokenWithHost(host.hostId, { promptIfUndetermined });
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
      if (event.event === "agent.notification") {
        const kind = normalizeNotificationKind(
          typeof event.event_kind === "string" ? event.event_kind : undefined
        );
        if (kind) {
          const paneId =
            typeof event.agent_id === "string" ? event.agent_id : "";
          const remoteId =
            typeof event.remote_id === "string" && event.remote_id.length > 0
              ? event.remote_id
              : `default::${paneId}`;
          const agentName =
            typeof event.agent_name === "string" && event.agent_name.length > 0
              ? event.agent_name
              : paneId || "agent";
          const hostName =
            typeof event.host_name === "string" && event.host_name.length > 0
              ? event.host_name
              : host.hostName;
          dispatchAgentNotification({
            hostId: host.hostId,
            hostName,
            remoteId,
            paneId,
            agentName,
            provider:
              typeof event.provider === "string" ? event.provider : undefined,
            kind,
            permissionCount:
              typeof event.permission_count === "number"
                ? event.permission_count
                : undefined,
            questionCount:
              typeof event.question_count === "number"
                ? event.question_count
                : undefined,
            summary:
              typeof event.summary === "string" ? event.summary : null,
            occurredMs:
              typeof event.occurred_ms === "number"
                ? event.occurred_ms
                : Date.now(),
          });
        }
        return;
      }
      if (event.event === "plugins.changed" && event.catalog) {
        useHostRuntimeStore
          .getState()
          .setHostPlugins(host.hostId, event.catalog as PluginCatalog);
        return;
      }
      if (
        event.event === "audit.completed" ||
        event.event === "collision.changed" ||
        event.event === "breaker.tripped"
      ) {
        const store = useHostRuntimeStore.getState();
        void store.refreshHost(host.hostId, false);
        if (event.event === "audit.completed" && store.activeHostId === host.hostId) {
          const activeId = store.activeAgentRemoteId;
          const activeAgent = activeId
            ? store.hosts[host.hostId]?.agents.find((a) => a.remote_id === activeId)
            : undefined;
          if (activeAgent) {
            void store.refreshTimeline(host.hostId, activeAgent);
          }
        }
        return;
      }
      if (event.event === "timeline.delta") {
        const remoteId =
          typeof event.remote_id === "string" && event.remote_id.length > 0
            ? event.remote_id
            : null;
        if (remoteId) {
          const store = useHostRuntimeStore.getState();
          const key = `${host.hostId}::${remoteId}`;
          const prev = store.timelines[key];
          const next = applyTimelineDeltaToPage(prev, {
            mode: event.mode === "patch" ? "patch" : "replace",
            from_index:
              typeof event.from_index === "number" ? event.from_index : undefined,
            items: Array.isArray(event.items)
              ? (event.items as TimelineItem[])
              : undefined,
            page:
              event.page && typeof event.page === "object"
                ? (event.page as TimelinePage)
                : undefined,
            binding_confidence:
              typeof event.binding_confidence === "string"
                ? (event.binding_confidence as BindingConfidence)
                : undefined,
            session_id:
              event.session_id !== undefined
                ? (event.session_id as string | null)
                : undefined,
            source_label:
              typeof event.source_label === "string"
                ? event.source_label
                : undefined,
            detected_model:
              event.detected_model !== undefined
                ? (event.detected_model as string | null)
                : undefined,
            has_more:
              typeof event.has_more === "boolean" ? event.has_more : undefined,
            active_prompts: Array.isArray(event.active_prompts)
              ? (event.active_prompts as InteractivePrompt[])
              : undefined,
          });
          if (next && (!prev || timelineSignature(prev) !== timelineSignature(next))) {
            const matchedAgent = store.hosts[host.hostId]?.agents.find(
              (a) => a.remote_id === remoteId
            );
            if (prev && matchedAgent && baselineSeededHosts.has(host.hostId)) {
              const promptEvents = detectNewPromptNotifications(
                host.hostId,
                store.hosts[host.hostId]?.host.hostName ?? host.hostName,
                matchedAgent,
                prev.active_prompts,
                next.active_prompts
              );
              for (const evt of promptEvents) {
                dispatchAgentNotification(evt);
              }
            }
            void saveCachedTimeline(host.hostId, remoteId, next);
            useHostRuntimeStore.setState((s) => ({
              timelines: {
                ...s.timelines,
                [key]: next,
              },
            }));
          }
        }
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
        return;
      }
      if (event.event === "asset.upload.result") {
        const res: AssetUploadResultEvent = {
          upload_id: Number(event.upload_id ?? 0),
          ok: event.ok === true,
          agent_id:
            typeof event.agent_id === "string" ? event.agent_id : undefined,
          delivery_mode:
            typeof event.delivery_mode === "string"
              ? event.delivery_mode
              : undefined,
          saved_path:
            typeof event.saved_path === "string" ? event.saved_path : undefined,
          prompt_path:
            typeof event.prompt_path === "string"
              ? event.prompt_path
              : undefined,
          in_git_worktree:
            typeof event.in_git_worktree === "boolean"
              ? event.in_git_worktree
              : undefined,
          size_bytes:
            typeof event.size_bytes === "number" ? event.size_bytes : undefined,
          structured_prompt:
            typeof event.structured_prompt === "string"
              ? event.structured_prompt
              : undefined,
          error: typeof event.error === "string" ? event.error : undefined,
        };
        for (const fn of assetUploadListeners) fn(res);
        return;
      }
      if (event.event === "live_pulse.updated") {
        const prefs = useHostRuntimeStore.getState().livePulsePrefs;
        if (!prefs.enabled) return;
        const rawPulse = (
          prefs.summaryEnabled && event.pulse_with_summary
            ? event.pulse_with_summary
            : event.pulse
        ) as LivePulsePayload | undefined;
        if (rawPulse) {
          const projected = projectLivePulseForDevice(rawPulse, prefs);
          if (projected) {
            void updateNativeLivePulse({
              enabled: true,
              hostId: projected.host_id,
              hostName: projected.host_name,
              workingCount: projected.working_count,
              blockedCount: projected.blocked_count,
              doneCount: projected.done_count,
              alertCount: projected.alert_count,
              topAgentAlias: projected.top_agent_alias,
              topEventKind: projected.top_event_kind,
              topSummaryLine: projected.top_summary_line,
              deepLink: projected.deep_link,
              updatedAt: projected.updated_at,
            });
          }
        }
      }
    },
  });

  clientPool.set(host.hostId, client);
  void client.connect().catch(() => {});
}
