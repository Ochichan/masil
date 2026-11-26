import React, { useCallback, useEffect, useRef, useState } from "react";
import { Modal, Pressable, ScrollView, StyleSheet, Text, View } from "react-native";
import { useRouter } from "expo-router";
import {
  ChevronDown,
  ChevronUp,
  FolderGit2,
  Link2,
  ListOrdered,
  Menu,
  MessageSquare,
  Play,
  QrCode,
  ShieldCheck,
  SlidersHorizontal,
  Terminal,
  Trash2,
  X,
} from "lucide-react-native";
import { useTranslation } from "react-i18next";
import { KeyboardAvoidingView } from "react-native-keyboard-controller";
import { SafeAreaView } from "react-native-safe-area-context";
import {
  countAgentsByCategory,
  getAgentBadgeDescriptor,
} from "../agents/status-classification";
import { formatCacheWarmthCompact } from "../agents/phase1-helpers";
import { AgentOptionsSheet } from "../components/AgentOptionsSheet";
import { AuditTriageSheet } from "../components/AuditTriageSheet";
import { BindingBadgeSheet } from "../components/BindingBadgeSheet";
import { CircuitBreakerCard } from "../components/CircuitBreakerCard";
import { ConfirmModal } from "../components/ConfirmModal";
import { ConversationView } from "../components/ConversationView";
import {
  InteractivePromptCard,
  type PromptRespondPayload,
} from "../components/InteractivePromptCard";
import { LiveTerminalTab } from "../components/LiveTerminalTab";
import { MobileComposer } from "../components/MobileComposer";
import { PluginUiRenderer } from "../components/PluginUiRenderer";
import { SegmentedTabs } from "../components/SegmentedTabs";
import {
  getHostClient,
  useHostRuntimeStore,
} from "../hosts/HostRuntimeController";
import { useMobilePanelsStore } from "../mobile-panels/store";
import type {
  AgentRow,
  BindingConfidence,
  InteractivePrompt,
  PluginCommandDef,
  PluginCommandOutcome,
  QueuedPromptItem,
  UiDocument,
} from "../protocol/schemas";
import { useMasilTheme } from "../theme/ThemeProvider";
import { FONTS, TABULAR_NUMS } from "../theme/typography";

interface AgentDetailScreenProps {
  hostId: string | null;
  agent: AgentRow | null;
}

function bindingLabel(
  conf: BindingConfidence,
  t: (k: string) => string,
  palette: ReturnType<typeof useMasilTheme>["palette"]
) {
  switch (conf) {
    case "reported_by_hook":
      return { text: t("bindingHook"), color: palette.muted, showLinkIcon: true };
    case "manually_selected":
      return { text: t("bindingManual"), color: palette.accent, showLinkIcon: true };
    case "unverified_heuristic":
      return { text: t("bindingUnverified"), color: palette.muted, showLinkIcon: true };
    default:
      return { text: t("bindingPaneOnly"), color: palette.muted, showLinkIcon: false };
  }
}

const EMPTY_PLUGIN_COMMANDS: PluginCommandDef[] = [];
const EMPTY_AGENTS: AgentRow[] = [];

export function AgentDetailScreen({ hostId, agent }: AgentDetailScreenProps) {
  const { palette } = useMasilTheme();
  const { t } = useTranslation();
  const router = useRouter();
  const selectPanel = useMobilePanelsStore((s) => s.selectPanel);
  const timelines = useHostRuntimeStore((s) => s.timelines);
  const refreshTimeline = useHostRuntimeStore((s) => s.refreshTimeline);
  const subscribeTimeline = useHostRuntimeStore((s) => s.subscribeTimeline);
  const patchTimelinePrompts = useHostRuntimeStore(
    (s) => s.patchTimelinePrompts
  );
  const refreshHost = useHostRuntimeStore((s) => s.refreshHost);
  const hostOrder = useHostRuntimeStore((s) => s.hostOrder);
  const hostAgents = useHostRuntimeStore((s) =>
    hostId ? s.hosts[hostId]?.agents ?? EMPTY_AGENTS : EMPTY_AGENTS
  );
  const hostConnection = useHostRuntimeStore((s) =>
    hostId ? s.hosts[hostId]?.connection ?? "disconnected" : "disconnected"
  );
  const supportsTimelineStreaming = useHostRuntimeStore((s) =>
    hostId ? s.hosts[hostId]?.capabilities?.timeline_streaming === true : false
  );
  const pluginCommands = useHostRuntimeStore((s) =>
    hostId
      ? s.hosts[hostId]?.plugins?.commands ?? EMPTY_PLUGIN_COMMANDS
      : EMPTY_PLUGIN_COMMANDS
  );

  const [mode, setMode] = useState<"conversation" | "terminal">("conversation");
  const [refreshing, setRefreshing] = useState(false);
  const [showBindSheet, setShowBindSheet] = useState(false);
  const [showOptionsSheet, setShowOptionsSheet] = useState(false);
  const [showAuditSheet, setShowAuditSheet] = useState(false);
  const [confirmClose, setConfirmClose] = useState(false);
  const [toast, setToast] = useState<{ text: string; isError?: boolean } | null>(
    null
  );
  const toastTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const [queuedItems, setQueuedItems] = useState<QueuedPromptItem[]>([]);
  const [queueExpanded, setQueueExpanded] = useState(false);
  const [pluginResultUi, setPluginResultUi] = useState<{
    title: string;
    pluginId: string;
    commandId: string;
    args: string;
    doc: UiDocument;
  } | null>(null);
  const [pluginResultBusy, setPluginResultBusy] = useState(false);
  const [composerDraftOverride, setComposerDraftOverride] = useState<
    string | null
  >(null);
  const storeDraftPrefill = useHostRuntimeStore((s) => s.composerDraftPrefill);
  const setStoreDraftPrefill = useHostRuntimeStore(
    (s) => s.setComposerDraftPrefill
  );
  useEffect(() => {
    if (storeDraftPrefill) {
      setComposerDraftOverride(storeDraftPrefill);
      setStoreDraftPrefill(null);
    }
  }, [storeDraftPrefill, setStoreDraftPrefill]);
  const clearComposerDraftOverride = useCallback(
    () => setComposerDraftOverride(null),
    []
  );

  const showToast = useCallback((text: string, isError = false) => {
    if (toastTimerRef.current) clearTimeout(toastTimerRef.current);
    setToast({ text, isError });
    toastTimerRef.current = setTimeout(
      () => setToast(null),
      isError ? 4200 : 3200
    );
  }, []);

  useEffect(() => {
    return () => {
      if (toastTimerRef.current) clearTimeout(toastTimerRef.current);
    };
  }, []);

  const page =
    hostId && agent ? timelines[`${hostId}::${agent.remote_id}`] ?? null : null;
  const activePrompts = page?.active_prompts ?? [];
  const hasActivePrompt = activePrompts.length > 0;

  const loadQueue = useCallback(async () => {
    if (!hostId || !agent) {
      setQueuedItems([]);
      return;
    }
    const client = getHostClient(hostId);
    if (!client || client.connectionState !== "connected") return;
    try {
      const res = await client.request<{
        items?: QueuedPromptItem[];
        prompts?: QueuedPromptItem[];
        queue?: QueuedPromptItem[];
      }>("agent.queue.list.request", {
        server_socket: agent.server_socket,
        pane: agent.pane,
        target: agent.pane || agent.name,
      });
      const list = res.items ?? res.prompts ?? res.queue ?? [];
      setQueuedItems(Array.isArray(list) ? list : []);
    } catch {
      // Queue may be empty or unsupported for unmanaged pane.
    }
  }, [hostId, agent?.server_socket, agent?.pane, agent?.name]);

  useEffect(() => {
    setQueueExpanded(false);
    void loadQueue();
  }, [agent?.remote_id, loadQueue]);

  useEffect(() => {
    if (!hostId || !agent || mode !== "conversation") return;
    const remoteId = agent.remote_id;
    const fetchLatest = () => {
      const latest = useHostRuntimeStore
        .getState()
        .hosts[hostId]?.agents.find((a) => a.remote_id === remoteId);
      if (latest) {
        void refreshTimeline(hostId, latest);
      }
    };

    if (supportsTimelineStreaming && hostConnection === "connected") {
      let active = true;
      let unsubscribe: (() => void) | null = null;
      void subscribeTimeline(hostId, agent).then((cleanup) => {
        if (!active) {
          cleanup();
        } else {
          unsubscribe = cleanup;
        }
      });
      // Slow safety-net resync only; live changes arrive over `timeline.delta`.
      const safetyId = setInterval(fetchLatest, 15_000);
      return () => {
        active = false;
        clearInterval(safetyId);
        unsubscribe?.();
      };
    }

    fetchLatest();
    const pollMs = agent.state === "blocked" || hasActivePrompt ? 800 : 1500;
    const id = setInterval(fetchLatest, pollMs);
    return () => clearInterval(id);
  }, [
    hostId,
    hostConnection,
    supportsTimelineStreaming,
    agent?.remote_id,
    agent?.state,
    agent?.session_id,
    hasActivePrompt,
    mode,
    refreshTimeline,
    subscribeTimeline,
  ]);

  if (hostOrder.length === 0) {
    return (
      <SafeAreaView
        style={[styles.emptyRoot, { backgroundColor: palette.canvas }]}
      >
        <Text style={[styles.emptyTitle, { color: palette.text }]}>
          {t("noHostsTitle")}
        </Text>
        <Text style={[styles.emptyBody, { color: palette.muted }]}>
          {t("noHostsBody")}
        </Text>
        <Pressable
          onPress={() => router.push("/pair")}
          style={[styles.pairBtn, { backgroundColor: palette.accent }]}
        >
          <QrCode size={18} color={palette.accentText} />
          <Text
            style={{
              color: palette.accentText,
              fontFamily: FONTS.bold,
              fontSize: 14,
            }}
          >
            {t("pairHost")}
          </Text>
        </Pressable>
      </SafeAreaView>
    );
  }

  if (!hostId || !agent) {
    return (
      <SafeAreaView
        style={[styles.emptyRoot, { backgroundColor: palette.canvas }]}
      >
        <Text style={[styles.emptyTitle, { color: palette.text }]}>
          {t("emptyAgents")}
        </Text>
        <Pressable
          onPress={() => selectPanel("agent-list")}
          style={[styles.pairBtn, { backgroundColor: palette.accent }]}
        >
          <Menu size={18} color={palette.accentText} />
          <Text
            style={{
              color: palette.accentText,
              fontFamily: FONTS.bold,
              fontSize: 14,
            }}
          >
            {t("agents")}
          </Text>
        </Pressable>
      </SafeAreaView>
    );
  }

  const badge = bindingLabel(
    page?.binding_confidence ?? "none",
    t,
    palette
  );

  const topPromptKind = activePrompts[0]?.kind;
  const badgeDesc = getAgentBadgeDescriptor(agent, topPromptKind);
  const stateInfo = (() => {
    const baseLabel = t(badgeDesc.key);
    const label = badgeDesc.count
      ? `${baseLabel} ${badgeDesc.count}`
      : baseLabel;
    switch (badgeDesc.tone) {
      case "warning":
        return { label, color: palette.warning, bg: palette.warningSoft };
      case "accent":
        return { label, color: palette.accent, bg: palette.accentSoft };
      case "success":
        return { label, color: palette.success, bg: palette.successSoft };
      case "danger":
        return { label, color: palette.danger, bg: palette.dangerSoft };
      default:
        return { label, color: palette.muted, bg: palette.raised };
    }
  })();

  const otherAgents = hostAgents.filter((a) => a.remote_id !== agent.remote_id);
  const otherCounts = countAgentsByCategory(otherAgents);
  const menuNeedsInputCount = otherCounts.needs_input;
  const menuWorkingCount = otherCounts.working;

  const nudgeSync = () => {
    void refreshHost(hostId, false);
    void refreshTimeline(hostId, agent);
    setTimeout(() => {
      const latest = useHostRuntimeStore
        .getState()
        .hosts[hostId]?.agents.find((a) => a.remote_id === agent.remote_id);
      void refreshHost(hostId, false);
      if (latest) void refreshTimeline(hostId, latest);
    }, 850);
  };

  const sendPrompt = async (text: string) => {
    const client = getHostClient(hostId);
    if (!client) {
      const err = new Error(t("disconnected"));
      showToast(t("sendFailed", { error: err.message }), true);
      throw err;
    }
    try {
      await client.request("agent.prompt.request", {
        server_socket: agent.server_socket,
        pane: agent.pane,
        target: agent.pane || agent.name,
        run: agent.run,
        text,
        force_keys: true,
      });
      nudgeSync();
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      showToast(t("sendFailed", { error: msg }), true);
      throw e;
    }
  };

  const runPluginCommand = async (cmd: PluginCommandDef, args: string) => {
    const client = getHostClient(hostId);
    if (!client) return;
    try {
      const outcome = await client.request<PluginCommandOutcome>(
        "plugin.command.run.request",
        {
          plugin_id: cmd.plugin_id,
          command_id: cmd.id,
          args,
          server_socket: agent.server_socket,
          agent: {
            remote_id: agent.remote_id,
            name: agent.name,
            provider: agent.provider,
            state: agent.state,
            cwd: agent.cwd,
            pane: agent.pane,
            server_socket: agent.server_socket,
            session_id: agent.session_id ?? null,
          },
        }
      );
      if (outcome.toast) {
        showToast(outcome.toast);
      }
      if (outcome.ui) {
        setPluginResultUi({
          title: outcome.ui.title || cmd.title,
          pluginId: cmd.plugin_id,
          commandId: cmd.id,
          args,
          doc: outcome.ui,
        });
      }
      if (outcome.draft) {
        setComposerDraftOverride(outcome.draft);
      }
      if (outcome.timeline_card_saved || outcome.refresh_agents) {
        nudgeSync();
      }
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      showToast(`/${cmd.slash}: ${msg}`, true);
    }
  };

  const runPluginCommandAction = async (evt: {
    action: string;
    payload?: unknown;
    formValues?: Record<string, string>;
  }) => {
    if (!pluginResultUi || pluginResultBusy) return;
    const client = getHostClient(hostId);
    if (!client) return;
    setPluginResultBusy(true);
    try {
      const outcome = await client.request<PluginCommandOutcome>(
        "plugin.command.run.request",
        {
          plugin_id: pluginResultUi.pluginId,
          command_id: pluginResultUi.commandId,
          args: pluginResultUi.args,
          action: evt.action,
          payload: evt.payload ?? null,
          form_values: evt.formValues ?? {},
          server_socket: agent.server_socket,
          agent: {
            remote_id: agent.remote_id,
            name: agent.name,
            provider: agent.provider,
            state: agent.state,
            cwd: agent.cwd,
            pane: agent.pane,
            server_socket: agent.server_socket,
            session_id: agent.session_id ?? null,
          },
        }
      );
      if (outcome.toast) {
        showToast(outcome.toast);
      }
      if (outcome.ui) {
        setPluginResultUi((prev) =>
          prev
            ? {
                ...prev,
                title: outcome.ui?.title || prev.title,
                doc: outcome.ui!,
              }
            : null
        );
      }
      if (outcome.draft) {
        setComposerDraftOverride(outcome.draft);
      }
      if (outcome.timeline_card_saved || outcome.refresh_agents) {
        nudgeSync();
      }
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      showToast(`${pluginResultUi.title}: ${msg}`, true);
    } finally {
      setPluginResultBusy(false);
    }
  };

  const queuePrompt = async (text: string) => {
    const client = getHostClient(hostId);
    if (!client) {
      const err = new Error(t("disconnected"));
      showToast(t("sendFailed", { error: err.message }), true);
      throw err;
    }
    try {
      await client.request("agent.queue.add.request", {
        server_socket: agent.server_socket,
        pane: agent.pane,
        target: agent.pane || agent.name,
        text,
      });
      showToast(t("queueAdded"));
      void loadQueue();
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      showToast(t("sendFailed", { error: msg }), true);
      throw e;
    }
  };

  const sendQueuedItem = async (itemId: string | number) => {
    const client = getHostClient(hostId);
    if (!client) return;
    try {
      await client.request("agent.queue.send.request", {
        server_socket: agent.server_socket,
        pane: agent.pane,
        target: agent.pane || agent.name,
        item_id: itemId,
      });
      void loadQueue();
      nudgeSync();
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      showToast(t("sendFailed", { error: msg }), true);
    }
  };

  const removeQueuedItem = async (itemId: string | number) => {
    const client = getHostClient(hostId);
    if (!client) return;
    try {
      await client.request("agent.queue.remove.request", {
        server_socket: agent.server_socket,
        pane: agent.pane,
        target: agent.pane || agent.name,
        item_id: itemId,
      });
      void loadQueue();
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      showToast(t("responseFailed", { error: msg }), true);
    }
  };

  const interruptAgent = async () => {
    const client = getHostClient(hostId);
    if (!client) {
      showToast(t("interruptFailed", { error: t("disconnected") }), true);
      return;
    }
    try {
      await client.request("agent.interrupt.request", {
        server_socket: agent.server_socket,
        pane: agent.pane,
        target: agent.pane || agent.name,
        provider: agent.provider,
      });
      showToast(t("interruptSent"));
      nudgeSync();
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      showToast(t("interruptFailed", { error: msg }), true);
    }
  };

  const respondToPrompt = async (payload: PromptRespondPayload) => {
    const client = getHostClient(hostId);
    if (!client) {
      const err = new Error(t("disconnected"));
      showToast(t("responseFailed", { error: err.message }), true);
      throw err;
    }
    try {
      const res = await client.request<{
        ok?: boolean;
        active_prompts?: InteractivePrompt[];
      }>("agent.prompt.respond.request", {
        server_socket: agent.server_socket,
        pane: agent.pane,
        target: agent.pane || agent.name,
        provider: agent.provider,
        cwd: agent.cwd,
        session_id: agent.session_id ?? null,
        state: agent.state,
        ...payload,
      });
      if (Array.isArray(res.active_prompts)) {
        patchTimelinePrompts(hostId, agent.remote_id, res.active_prompts);
      }
      nudgeSync();
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      showToast(t("responseFailed", { error: msg }), true);
      throw e;
    }
  };

  const approveAgent = async (
    requestId: string | null | undefined,
    decision: string,
    keySequence?: string | null
  ) => {
    const client = getHostClient(hostId);
    if (!client) {
      showToast(t("responseFailed", { error: t("disconnected") }), true);
      return;
    }
    try {
      await client.request("agent.approve.request", {
        server_socket: agent.server_socket,
        pane: agent.pane,
        target: agent.pane || agent.name,
        request_id: requestId ?? undefined,
        decision,
        key_sequence: keySequence ?? "\r",
      });
      nudgeSync();
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      showToast(t("responseFailed", { error: msg }), true);
    }
  };

  const closeAgent = async () => {
    setConfirmClose(false);
    const client = getHostClient(hostId);
    if (!client) return;
    try {
      await client.request("agent.close.request", {
        server_socket: agent.server_socket,
        pane: agent.pane,
        target: agent.pane || agent.name,
      });
      await refreshHost(hostId);
      selectPanel("agent-list");
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      showToast(t("closeFailed", { error: msg }), true);
    }
  };

  return (
    <SafeAreaView
      style={[styles.root, { backgroundColor: palette.canvas }]}
      edges={["top", "bottom"]}
    >
      <View
        style={[
          styles.topChrome,
          { backgroundColor: palette.panel, borderBottomColor: palette.hairline },
        ]}
      >
        <View style={styles.header}>
          <Pressable
            onPress={() => selectPanel("agent-list")}
            hitSlop={6}
            style={styles.iconBtn}
            accessibilityLabel={t("agents")}
          >
            <Menu size={19} color={palette.textSecondary} strokeWidth={1.9} />
            {menuNeedsInputCount > 0 ? (
              <View
                style={[
                  styles.menuBadge,
                  {
                    backgroundColor: palette.warning,
                    borderColor: palette.panel,
                  },
                ]}
              >
                <Text
                  style={[styles.menuBadgeText, { color: palette.accentText }]}
                >
                  {menuNeedsInputCount > 9 ? "9+" : menuNeedsInputCount}
                </Text>
              </View>
            ) : menuWorkingCount > 0 ? (
              <View
                style={[
                  styles.menuWorkingDot,
                  {
                    backgroundColor: palette.success,
                    borderColor: palette.panel,
                  },
                ]}
              />
            ) : null}
          </Pressable>

          <View style={styles.titleCol}>
            <View style={styles.titleRow}>
              <Text
                numberOfLines={1}
                style={[styles.agentTitle, { color: palette.text }]}
              >
                {agent.name || agent.pane}
              </Text>
              <View style={[styles.statePill, { backgroundColor: stateInfo.bg }]}>
                <View style={[styles.stateDot, { backgroundColor: stateInfo.color }]} />
                <Text style={[styles.stateChip, { color: stateInfo.color }]}>
                  {stateInfo.label}
                </Text>
              </View>
            </View>
            <View style={styles.metaRow}>
              <Pressable
                onPress={() => setShowOptionsSheet(true)}
                style={[
                  styles.modelPill,
                  {
                    backgroundColor: palette.raised,
                    borderColor: palette.hairline,
                  },
                ]}
                hitSlop={6}
              >
                <Text
                  numberOfLines={1}
                  style={[styles.providerText, { color: palette.textSecondary }]}
                >
                  {page?.detected_model || agent.provider}
                </Text>
                <ChevronDown size={11} color={palette.muted} />
              </Pressable>
              <Pressable
                onPress={() => setShowBindSheet(true)}
                hitSlop={6}
                style={styles.bindTrigger}
              >
                {badge.showLinkIcon ? (
                  <Link2 size={11} color={badge.color} strokeWidth={1.8} />
                ) : null}
                <Text style={[styles.badgeText, { color: badge.color }]}>
                  {badge.text}
                </Text>
              </Pressable>
              {(() => {
                const warmthLabel = formatCacheWarmthCompact(agent.cache_warmth);
                return warmthLabel ? (
                  <Text
                    style={[styles.warmthText, { color: palette.muted }]}
                    numberOfLines={1}
                  >
                    {warmthLabel}
                  </Text>
                ) : null;
              })()}
            </View>
          </View>

          <View style={styles.headerActions}>
            <Pressable
              onPress={() => setShowAuditSheet(true)}
              hitSlop={4}
              style={[
                styles.iconBtn,
                showAuditSheet && { backgroundColor: palette.accentSoft },
              ]}
              accessibilityLabel={t("auditTitle")}
            >
              <ShieldCheck
                size={18}
                color={
                  (agent.audit_summary?.critical_count ?? 0) > 0
                    ? palette.danger
                    : (agent.audit_summary?.high_count ?? 0) > 0
                      ? palette.warning
                      : showAuditSheet
                        ? palette.accent
                        : palette.textSecondary
                }
                strokeWidth={1.85}
              />
              {((agent.audit_summary?.critical_count ?? 0) > 0 ||
                (agent.audit_summary?.high_count ?? 0) > 0) ? (
                <View
                  style={[
                    styles.menuWorkingDot,
                    {
                      backgroundColor:
                        (agent.audit_summary?.critical_count ?? 0) > 0
                          ? palette.danger
                          : palette.warning,
                      borderColor: palette.panel,
                    },
                  ]}
                />
              ) : null}
            </Pressable>
            <Pressable
              onPress={() => setShowOptionsSheet(true)}
              hitSlop={4}
              style={[
                styles.iconBtn,
                showOptionsSheet && { backgroundColor: palette.accentSoft },
              ]}
              accessibilityLabel={t("modelAndOptions")}
            >
              <SlidersHorizontal
                size={18}
                color={showOptionsSheet ? palette.accent : palette.textSecondary}
                strokeWidth={1.85}
              />
            </Pressable>
            <Pressable
              onPress={() => selectPanel("workspace-tools")}
              hitSlop={4}
              style={styles.iconBtn}
              accessibilityLabel={t("changes")}
            >
              <FolderGit2
                size={18}
                color={palette.textSecondary}
                strokeWidth={1.85}
              />
            </Pressable>
            <View
              style={[
                styles.actionDivider,
                { backgroundColor: palette.hairline },
              ]}
            />
            <Pressable
              onPress={() => setConfirmClose(true)}
              hitSlop={4}
              style={styles.iconBtn}
              accessibilityLabel={t("closeSession")}
            >
              <Trash2 size={18} color={palette.muted} strokeWidth={1.85} />
            </Pressable>
          </View>
        </View>

        {/* Compact recessed mode switcher */}
        <View style={styles.modeBar}>
          <SegmentedTabs
            value={mode}
            onChange={setMode}
            options={[
              {
                id: "conversation",
                label: t("conversation"),
                icon: (c) => <MessageSquare size={14} color={c} />,
              },
              {
                id: "terminal",
                label: t("terminal"),
                icon: (c) => <Terminal size={14} color={c} />,
              },
            ]}
          />
        </View>
      </View>

      <KeyboardAvoidingView behavior="padding" style={{ flex: 1 }}>
        {mode === "conversation" ? (
          <>
            <ConversationView
              agent={agent}
              page={page}
              refreshing={refreshing}
              onRefresh={async () => {
                setRefreshing(true);
                await Promise.all([
                  refreshHost(hostId, true),
                  refreshTimeline(hostId, agent),
                  loadQueue(),
                ]);
                setRefreshing(false);
              }}
              onApprove={(reqId, decision, keys) =>
                void approveAgent(reqId, decision, keys)
              }
              onOpenTerminal={() => setMode("terminal")}
            />
            {agent.breaker?.tripped ? (
              <CircuitBreakerCard
                hostId={hostId}
                agent={agent}
                breaker={agent.breaker}
                onResolved={(msg) => {
                  showToast(msg);
                  nudgeSync();
                }}
                onError={(msg) => showToast(msg, true)}
              />
            ) : null}
            {hasActivePrompt ? (
              <InteractivePromptCard
                agentName={agent.name || agent.pane}
                prompts={activePrompts}
                onRespond={respondToPrompt}
                onOpenTerminal={() => setMode("terminal")}
              />
            ) : null}
            {queuedItems.length > 0 ? (
              <View
                style={[
                  styles.queueBar,
                  {
                    backgroundColor: palette.panel,
                    borderTopColor: palette.hairline,
                  },
                ]}
              >
                <Pressable
                  onPress={() => setQueueExpanded((v) => !v)}
                  style={styles.queueHeader}
                >
                  <View style={styles.queueHeaderLeft}>
                    <ListOrdered size={13} color={palette.accent} />
                    <Text style={[styles.queueTitle, { color: palette.text }]}>
                      {t("queueCount", { count: queuedItems.length })}
                    </Text>
                  </View>
                  {queueExpanded ? (
                    <ChevronDown size={14} color={palette.muted} />
                  ) : (
                    <ChevronUp size={14} color={palette.muted} />
                  )}
                </Pressable>
                {queueExpanded ? (
                  <ScrollView
                    style={styles.queueList}
                    contentContainerStyle={{ gap: 6, paddingBottom: 6 }}
                  >
                    {queuedItems.map((item) => {
                      const label = item.body || item.text || "";
                      return (
                        <View
                          key={String(item.id)}
                          style={[
                            styles.queueRow,
                            {
                              backgroundColor: palette.canvas,
                              borderColor: palette.hairline,
                            },
                          ]}
                        >
                          <Text
                            numberOfLines={2}
                            style={[styles.queueItemText, { color: palette.text }]}
                          >
                            {label}
                          </Text>
                          <Pressable
                            onPress={() => void sendQueuedItem(item.id)}
                            hitSlop={6}
                            style={[
                              styles.queueActionBtn,
                              { backgroundColor: palette.accentSoft },
                            ]}
                            accessibilityLabel={t("queueSendNow")}
                          >
                            <Play size={12} color={palette.accent} />
                          </Pressable>
                          <Pressable
                            onPress={() => void removeQueuedItem(item.id)}
                            hitSlop={6}
                            style={[
                              styles.queueActionBtn,
                              { backgroundColor: palette.raised },
                            ]}
                            accessibilityLabel={t("queueRemove")}
                          >
                            <X size={13} color={palette.muted} />
                          </Pressable>
                        </View>
                      );
                    })}
                  </ScrollView>
                ) : null}
              </View>
            ) : null}
            <MobileComposer
              hostId={hostId}
              agentName={agent.name || agent.pane}
              isWorking={agent.state === "working" || agent.state === "retrying"}
              pluginCommands={pluginCommands}
              draftOverride={composerDraftOverride}
              onDraftOverrideConsumed={clearComposerDraftOverride}
              onSend={sendPrompt}
              onPluginCommand={runPluginCommand}
              onQueueAdd={queuePrompt}
              onInterrupt={interruptAgent}
            />
          </>
        ) : (
          <LiveTerminalTab
            hostId={hostId}
            agent={agent}
            active={mode === "terminal"}
          />
        )}
      </KeyboardAvoidingView>

      <AgentOptionsSheet
        visible={showOptionsSheet}
        hostId={hostId}
        agent={agent}
        detectedModel={page?.detected_model ?? null}
        onClose={() => setShowOptionsSheet(false)}
        onApplied={nudgeSync}
      />

      <AuditTriageSheet
        visible={showAuditSheet}
        hostId={hostId}
        agent={agent}
        onClose={() => setShowAuditSheet(false)}
        onActionDone={(msg) => {
          showToast(msg);
          nudgeSync();
        }}
      />

      <BindingBadgeSheet
        visible={showBindSheet}
        hostId={hostId}
        agent={agent}
        confidence={page?.binding_confidence ?? "none"}
        currentSessionId={page?.session_id ?? null}
        onClose={() => setShowBindSheet(false)}
        onBound={() => void refreshTimeline(hostId, agent)}
      />

      <ConfirmModal
        visible={confirmClose}
        title={t("confirmActionTitle")}
        body={t("confirmCloseBody", { name: agent.name || agent.pane })}
        destructive
        onCancel={() => setConfirmClose(false)}
        onConfirm={() => void closeAgent()}
      />

      {toast ? (
        <View
          pointerEvents="none"
          style={[
            styles.toastBar,
            {
              backgroundColor: toast.isError ? palette.dangerSoft : palette.raised,
              borderColor: toast.isError ? palette.danger : palette.accentBorder,
            },
          ]}
        >
          <Text
            numberOfLines={2}
            style={[
              styles.toastText,
              { color: toast.isError ? palette.danger : palette.text },
            ]}
          >
            {toast.text}
          </Text>
        </View>
      ) : null}

      <Modal
        visible={Boolean(pluginResultUi)}
        transparent
        animationType="slide"
        onRequestClose={() => setPluginResultUi(null)}
      >
        <View style={styles.resultBackdrop}>
          <View
            style={[
              styles.resultSheet,
              {
                backgroundColor: palette.panel,
                borderColor: palette.borderSubtle,
              },
            ]}
          >
            <View style={styles.resultHeader}>
              <View style={{ flex: 1 }}>
                <Text style={[styles.resultTitle, { color: palette.text }]}>
                  {pluginResultUi?.title}
                </Text>
                <Text style={[styles.resultSub, { color: palette.muted }]}>
                  {pluginResultUi?.pluginId}
                </Text>
              </View>
              <Pressable onPress={() => setPluginResultUi(null)} hitSlop={8}>
                <X size={19} color={palette.muted} />
              </Pressable>
            </View>
            <ScrollView contentContainerStyle={styles.resultBody}>
              {pluginResultUi ? (
                <PluginUiRenderer
                  blocks={pluginResultUi.doc.blocks ?? []}
                  busy={pluginResultBusy}
                  onAction={(evt) => void runPluginCommandAction(evt)}
                />
              ) : null}
            </ScrollView>
          </View>
        </View>
      </Modal>
    </SafeAreaView>
  );
}

const styles = StyleSheet.create({
  root: { flex: 1 },
  emptyRoot: {
    flex: 1,
    alignItems: "center",
    justifyContent: "center",
    padding: 28,
    gap: 12,
  },
  emptyTitle: {
    fontFamily: FONTS.bold,
    fontSize: 17,
    textAlign: "center",
  },
  emptyBody: {
    fontFamily: FONTS.regular,
    fontSize: 14,
    lineHeight: 21,
    textAlign: "center",
  },
  pairBtn: {
    flexDirection: "row",
    alignItems: "center",
    gap: 8,
    paddingHorizontal: 18,
    paddingVertical: 12,
    borderRadius: 10,
    marginTop: 6,
  },
  topChrome: {
    borderBottomWidth: 1,
  },
  header: {
    flexDirection: "row",
    alignItems: "center",
    paddingHorizontal: 10,
    paddingTop: 8,
    paddingBottom: 7,
    gap: 8,
  },
  iconBtn: {
    width: 34,
    height: 34,
    borderRadius: 9,
    alignItems: "center",
    justifyContent: "center",
    position: "relative",
  },
  menuBadge: {
    position: "absolute",
    top: 1,
    right: 0,
    minWidth: 15,
    height: 15,
    borderRadius: 8,
    borderWidth: 1.5,
    paddingHorizontal: 3,
    alignItems: "center",
    justifyContent: "center",
  },
  menuBadgeText: {
    fontFamily: FONTS.bold,
    fontVariant: TABULAR_NUMS,
    fontSize: 9,
    lineHeight: 11,
  },
  menuWorkingDot: {
    position: "absolute",
    top: 5,
    right: 4,
    width: 8,
    height: 8,
    borderRadius: 4,
    borderWidth: 1.5,
  },
  actionDivider: {
    width: 1,
    height: 16,
    marginHorizontal: 2,
  },
  headerActions: {
    flexDirection: "row",
    alignItems: "center",
    gap: 2,
  },
  titleCol: { flex: 1, minWidth: 0, gap: 4 },
  titleRow: { flexDirection: "row", alignItems: "center", gap: 7 },
  agentTitle: {
    fontFamily: FONTS.bold,
    fontSize: 16,
    letterSpacing: -0.2,
    flexShrink: 1,
  },
  statePill: {
    flexDirection: "row",
    alignItems: "center",
    gap: 5,
    paddingHorizontal: 7,
    paddingVertical: 2,
    borderRadius: 999,
  },
  stateDot: { width: 6, height: 6, borderRadius: 3 },
  stateChip: {
    fontFamily: FONTS.semi,
    fontSize: 11,
  },
  metaRow: {
    flexDirection: "row",
    alignItems: "center",
    gap: 8,
  },
  modelPill: {
    flexDirection: "row",
    alignItems: "center",
    gap: 4,
    height: 23,
    paddingHorizontal: 8,
    borderRadius: 6,
    borderWidth: 1,
    flexShrink: 1,
  },
  bindTrigger: {
    flexDirection: "row",
    alignItems: "center",
    gap: 3,
    opacity: 0.78,
  },
  providerText: {
    fontFamily: FONTS.mono,
    fontSize: 11,
    flexShrink: 1,
  },
  badgeText: {
    fontFamily: FONTS.regular,
    fontSize: 11.5,
  },
  warmthText: {
    fontFamily: FONTS.mono,
    fontSize: 10.5,
  },
  modeBar: {
    paddingHorizontal: 14,
    paddingBottom: 8,
  },
  queueBar: {
    borderTopWidth: 1,
    paddingHorizontal: 12,
    paddingVertical: 6,
    gap: 6,
  },
  queueHeader: {
    flexDirection: "row",
    alignItems: "center",
    justifyContent: "space-between",
  },
  queueHeaderLeft: {
    flexDirection: "row",
    alignItems: "center",
    gap: 6,
  },
  queueTitle: {
    fontFamily: FONTS.semi,
    fontSize: 12,
  },
  queueList: {
    maxHeight: 130,
  },
  queueRow: {
    flexDirection: "row",
    alignItems: "center",
    gap: 6,
    borderWidth: 1,
    borderRadius: 8,
    paddingHorizontal: 9,
    paddingVertical: 6,
  },
  queueItemText: {
    flex: 1,
    fontFamily: FONTS.regular,
    fontSize: 12,
    lineHeight: 17,
  },
  queueActionBtn: {
    width: 26,
    height: 26,
    borderRadius: 7,
    alignItems: "center",
    justifyContent: "center",
  },
  toastBar: {
    position: "absolute",
    top: 112,
    left: 16,
    right: 16,
    borderWidth: 1,
    borderRadius: 10,
    paddingHorizontal: 13,
    paddingVertical: 9,
  },
  toastText: {
    fontFamily: FONTS.semi,
    fontSize: 12.5,
  },
  resultBackdrop: {
    flex: 1,
    backgroundColor: "rgba(0,0,0,0.55)",
    justifyContent: "flex-end",
  },
  resultSheet: {
    maxHeight: "82%",
    borderTopLeftRadius: 18,
    borderTopRightRadius: 18,
    borderTopWidth: 1,
  },
  resultHeader: {
    flexDirection: "row",
    alignItems: "center",
    paddingHorizontal: 16,
    paddingVertical: 13,
    gap: 10,
  },
  resultTitle: { fontFamily: FONTS.bold, fontSize: 15.5 },
  resultSub: { fontFamily: FONTS.mono, fontSize: 11 },
  resultBody: { padding: 16, paddingTop: 4, paddingBottom: 32 },
});
