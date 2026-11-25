import React, { useCallback, useEffect, useState } from "react";
import { Modal, Pressable, ScrollView, StyleSheet, Text, View } from "react-native";
import { useRouter } from "expo-router";
import {
  ChevronDown,
  FolderGit2,
  Link2,
  Menu,
  MessageSquare,
  QrCode,
  SlidersHorizontal,
  Terminal,
  Trash2,
  X,
} from "lucide-react-native";
import { useTranslation } from "react-i18next";
import { KeyboardAvoidingView } from "react-native-keyboard-controller";
import { SafeAreaView } from "react-native-safe-area-context";
import { AgentOptionsSheet } from "../components/AgentOptionsSheet";
import { BindingBadgeSheet } from "../components/BindingBadgeSheet";
import { ConfirmModal } from "../components/ConfirmModal";
import { ConversationView } from "../components/ConversationView";
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
  PluginCommandDef,
  PluginCommandOutcome,
  UiDocument,
} from "../protocol/schemas";
import { useMasilTheme } from "../theme/ThemeProvider";
import { FONTS } from "../theme/typography";

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

export function AgentDetailScreen({ hostId, agent }: AgentDetailScreenProps) {
  const { palette } = useMasilTheme();
  const { t } = useTranslation();
  const router = useRouter();
  const selectPanel = useMobilePanelsStore((s) => s.selectPanel);
  const timelines = useHostRuntimeStore((s) => s.timelines);
  const refreshTimeline = useHostRuntimeStore((s) => s.refreshTimeline);
  const refreshHost = useHostRuntimeStore((s) => s.refreshHost);
  const hostOrder = useHostRuntimeStore((s) => s.hostOrder);
  const pluginCommands = useHostRuntimeStore((s) =>
    hostId
      ? s.hosts[hostId]?.plugins?.commands ?? EMPTY_PLUGIN_COMMANDS
      : EMPTY_PLUGIN_COMMANDS
  );

  const [mode, setMode] = useState<"conversation" | "terminal">("conversation");
  const [refreshing, setRefreshing] = useState(false);
  const [showBindSheet, setShowBindSheet] = useState(false);
  const [showOptionsSheet, setShowOptionsSheet] = useState(false);
  const [confirmClose, setConfirmClose] = useState(false);
  const [pluginToast, setPluginToast] = useState<string | null>(null);
  const [pluginResultUi, setPluginResultUi] = useState<{
    title: string;
    pluginId: string;
    doc: UiDocument;
  } | null>(null);
  const [composerDraftOverride, setComposerDraftOverride] = useState<
    string | null
  >(null);
  const clearComposerDraftOverride = useCallback(
    () => setComposerDraftOverride(null),
    []
  );

  const page =
    hostId && agent ? timelines[`${hostId}::${agent.remote_id}`] ?? null : null;

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
    fetchLatest();
    const id = setInterval(fetchLatest, 1500);
    return () => clearInterval(id);
  }, [hostId, agent?.remote_id, agent?.state, agent?.session_id, mode, refreshTimeline]);

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

  const stateInfo =
    agent.state === "working" || agent.state === "retrying"
      ? { label: t("working"), color: palette.success, bg: palette.successSoft }
      : agent.state === "blocked"
        ? { label: t("needsInput"), color: palette.warning, bg: palette.warningSoft }
        : agent.state === "idle"
          ? { label: t("idle"), color: palette.muted, bg: palette.raised }
          : { label: t("stateUnavailable"), color: palette.danger, bg: palette.dangerSoft };

  const nudgeSync = () => {
    void refreshHost(hostId, false);
    void refreshTimeline(hostId, agent);
    setTimeout(() => {
      const latest = useHostRuntimeStore
        .getState()
        .hosts[hostId]?.agents.find((a) => a.remote_id === agent.remote_id);
      void refreshHost(hostId, false);
      if (latest) void refreshTimeline(hostId, latest);
    }, 900);
  };

  const sendPrompt = async (text: string) => {
    const client = getHostClient(hostId);
    if (!client) return;
    await client.request("agent.prompt.request", {
      server_socket: agent.server_socket,
      target: agent.name || agent.pane,
      run: agent.run,
      text,
      force_keys: true,
    });
    nudgeSync();
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
        setPluginToast(outcome.toast);
        setTimeout(() => setPluginToast(null), 3500);
      }
      if (outcome.ui) {
        setPluginResultUi({
          title: outcome.ui.title || cmd.title,
          pluginId: cmd.plugin_id,
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
      setPluginToast(`/${cmd.slash}: ${msg}`);
      setTimeout(() => setPluginToast(null), 4000);
    }
  };

  const queuePrompt = async (text: string) => {
    const client = getHostClient(hostId);
    if (!client) return;
    await client.request("agent.queue.add.request", {
      server_socket: agent.server_socket,
      target: agent.name || agent.pane,
      text,
    });
  };

  const interruptAgent = async () => {
    const client = getHostClient(hostId);
    if (!client) return;
    await client.request("agent.interrupt.request", {
      server_socket: agent.server_socket,
      target: agent.name || agent.pane,
    });
    nudgeSync();
  };

  const approveAgent = async (
    requestId: string | null | undefined,
    decision: string,
    keySequence?: string | null
  ) => {
    const client = getHostClient(hostId);
    if (!client) return;
    await client.request("agent.approve.request", {
      server_socket: agent.server_socket,
      target: agent.name || agent.pane,
      request_id: requestId ?? undefined,
      decision,
      key_sequence: keySequence ?? "\r",
    });
    nudgeSync();
  };

  const closeAgent = async () => {
    setConfirmClose(false);
    const client = getHostClient(hostId);
    if (!client) return;
    await client.request("agent.close.request", {
      server_socket: agent.server_socket,
      target: agent.name || agent.pane,
    });
    await refreshHost(hostId);
    selectPanel("agent-list");
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
            </View>
          </View>

          <View style={styles.headerActions}>
            <Pressable
              onPress={() => setShowOptionsSheet(true)}
              hitSlop={4}
              style={[
                styles.iconBtn,
                showOptionsSheet && { backgroundColor: palette.accentSoft },
              ]}
              accessibilityLabel="모델 및 옵션 설정"
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
              accessibilityLabel="세션 닫기"
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
                ]);
                setRefreshing(false);
              }}
              onApprove={(reqId, decision, keys) =>
                void approveAgent(reqId, decision, keys)
              }
              onOpenTerminal={() => setMode("terminal")}
            />
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

      {pluginToast ? (
        <View
          pointerEvents="none"
          style={[
            styles.toastBar,
            {
              backgroundColor: palette.raised,
              borderColor: palette.accentBorder,
            },
          ]}
        >
          <Text
            numberOfLines={2}
            style={[styles.toastText, { color: palette.text }]}
          >
            {pluginToast}
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
                <PluginUiRenderer blocks={pluginResultUi.doc.blocks ?? []} />
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
  modeBar: {
    paddingHorizontal: 14,
    paddingBottom: 8,
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
