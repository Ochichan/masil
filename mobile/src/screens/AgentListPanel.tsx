import React, { useMemo, useState } from "react";
import {
  FlatList,
  Pressable,
  RefreshControl,
  ScrollView,
  SectionList,
  StyleSheet,
  Text,
  TextInput,
  View,
} from "react-native";
import { useRouter } from "expo-router";
import {
  ChevronDown,
  ChevronRight,
  Download,
  Flame,
  Inbox,
  Laptop,
  Moon,
  Plus,
  QrCode,
  Search,
  Settings,
  Shield,
  Sun,
  Terminal,
  Trophy,
  X,
} from "lucide-react-native";
import { useTranslation } from "react-i18next";
import { SafeAreaView, useSafeAreaInsets } from "react-native-safe-area-context";
import {
  collisionSeverityTone,
  formatCacheWarmthCompact,
  sortAgentsWarmestBlockedFirst,
} from "../agents/phase1-helpers";
import { formatFocusRemaining } from "../agents/phase2-helpers";
import {
  type BatchPromptCandidate,
  filterBatchSafePrompts,
  parseSandboxBadge,
} from "../agents/phase3-helpers";
import {
  type AgentStatusCategory,
  type AgentStatusFilter,
  type BadgeTone,
  buildAgentStatusSections,
  countAgentsByCategory,
  getAgentBadgeDescriptor,
  resolveInboxKindDisplay,
} from "../agents/status-classification";
import { ArenaScorecardModal } from "../components/ArenaScorecardModal";
import { BatchApproveBar } from "../components/BatchApproveBar";
import { BriefingModal } from "../components/BriefingModal";
import { FocusShieldModal } from "../components/FocusShieldModal";
import { PolicyFirewallModal } from "../components/PolicyFirewallModal";
import { SegmentedTabs } from "../components/SegmentedTabs";
import {
  getHostClient,
  useHostRuntimeStore,
} from "../hosts/HostRuntimeController";
import { useMobilePanelsStore } from "../mobile-panels/store";
import type { AgentRow } from "../protocol/schemas";
import { matchesMasilQuery } from "../search/korean-choseong";
import { useMasilTheme } from "../theme/ThemeProvider";
import { FONTS, TABULAR_NUMS, shortenHomePath } from "../theme/typography";
import { ImportSessionSheet } from "./ImportSessionSheet";
import { NewAgentSheet } from "./NewAgentSheet";

type Palette = ReturnType<typeof useMasilTheme>["palette"];

function toneColors(tone: BadgeTone, palette: Palette) {
  switch (tone) {
    case "warning":
      return { color: palette.warning, bg: palette.warningSoft };
    case "accent":
      return { color: palette.accent, bg: palette.accentSoft };
    case "success":
      return { color: palette.success, bg: palette.successSoft };
    case "danger":
      return { color: palette.danger, bg: palette.dangerSoft };
    default:
      return { color: palette.muted, bg: palette.raised };
  }
}

function categoryMeta(
  category: AgentStatusCategory,
  palette: Palette,
  t: (k: string) => string
) {
  switch (category) {
    case "needs_input":
      return {
        label: t("filterNeedsInput"),
        color: palette.warning,
        bg: palette.warningSoft,
      };
    case "working":
      return {
        label: t("filterWorking"),
        color: palette.success,
        bg: palette.successSoft,
      };
    case "idle":
      return {
        label: t("filterIdle"),
        color: palette.muted,
        bg: palette.raised,
      };
    case "unavailable":
      return {
        label: t("filterUnavailable"),
        color: palette.danger,
        bg: palette.dangerSoft,
      };
  }
}

function stateBadge(agent: AgentRow, palette: Palette, t: (k: string) => string) {
  const desc = getAgentBadgeDescriptor(agent);
  const colors = toneColors(desc.tone, palette);
  const baseLabel = t(desc.key);
  return {
    label: desc.count ? `${baseLabel} ${desc.count}` : baseLabel,
    color: colors.color,
    bg: colors.bg,
    category: desc.category,
  };
}

export function AgentListPanel() {
  const { palette } = useMasilTheme();
  const { t, i18n } = useTranslation();
  const insets = useSafeAreaInsets();
  const router = useRouter();
  const hostOrder = useHostRuntimeStore((s) => s.hostOrder);
  const hosts = useHostRuntimeStore((s) => s.hosts);
  const timelines = useHostRuntimeStore((s) => s.timelines);
  const activeHostId = useHostRuntimeStore((s) => s.activeHostId);
  const activeAgentRemoteId = useHostRuntimeStore((s) => s.activeAgentRemoteId);
  const selectHost = useHostRuntimeStore((s) => s.selectHost);
  const selectAgent = useHostRuntimeStore((s) => s.selectAgent);
  const refreshHost = useHostRuntimeStore((s) => s.refreshHost);
  const selectPanel = useMobilePanelsStore((s) => s.selectPanel);

  const [tab, setTab] = useState<"agents" | "inbox">("agents");
  const [statusFilter, setStatusFilter] = useState<AgentStatusFilter>("all");
  const [collapsedCategories, setCollapsedCategories] = useState<
    Partial<Record<AgentStatusCategory, boolean>>
  >({});
  const [query, setQuery] = useState("");
  const [warmestFirst, setWarmestFirst] = useState(false);
  const [refreshing, setRefreshing] = useState(false);
  const [showNew, setShowNew] = useState(false);
  const [showImport, setShowImport] = useState(false);
  const [showBriefing, setShowBriefing] = useState(false);
  const [showFocusModal, setShowFocusModal] = useState(false);
  const [showPolicyModal, setShowPolicyModal] = useState(false);
  const [showArenaModal, setShowArenaModal] = useState(false);

  const activeHost = activeHostId ? hosts[activeHostId] : undefined;
  const focusStatus = activeHost?.focusStatus ?? null;
  const isKo = i18n.language.startsWith("ko");

  const safeBatchCandidates = useMemo(() => {
    if (!activeHost || !activeHostId) return [];
    const raw: BatchPromptCandidate[] = [];
    for (const agent of activeHost.agents) {
      const tl = timelines[`${activeHostId}::${agent.remote_id}`];
      const prompts = tl?.active_prompts ?? [];
      for (const prompt of prompts) {
        raw.push({
          agentName: agent.name || agent.pane,
          remoteId: agent.remote_id,
          serverSocket: agent.server_socket,
          pane: agent.pane,
          prompt,
        });
      }
    }
    return filterBatchSafePrompts(raw);
  }, [activeHost, activeHostId, timelines]);

  const queryMatchedAgents = useMemo(() => {
    const list = activeHost?.agents ?? [];
    if (!query.trim()) return list;
    return list.filter((a) =>
      matchesMasilQuery(
        `${a.name} ${a.provider} ${a.cwd} ${a.server_name} ${a.pane}`,
        query
      )
    );
  }, [activeHost?.agents, query]);

  const statusCounts = useMemo(
    () => countAgentsByCategory(queryMatchedAgents),
    [queryMatchedAgents]
  );

  const sections = useMemo(() => {
    const built = buildAgentStatusSections(queryMatchedAgents, statusFilter).map(
      (sec) => ({
        ...sec,
        data: warmestFirst
          ? sortAgentsWarmestBlockedFirst(sec.data)
          : sec.data,
      })
    );
    if (statusFilter !== "all") {
      return built;
    }
    return built.map((sec) => ({
      ...sec,
      data: collapsedCategories[sec.category] ? [] : sec.data,
    }));
  }, [queryMatchedAgents, statusFilter, collapsedCategories, warmestFirst]);

  const toggleCategoryCollapse = (category: AgentStatusCategory) => {
    setCollapsedCategories((prev) => ({
      ...prev,
      [category]: !prev[category],
    }));
  };

  const toggleStatusFilter = (target: AgentStatusFilter) => {
    setStatusFilter((prev) =>
      target === "all" ? "all" : prev === target ? "all" : target
    );
  };

  const onRefresh = async () => {
    if (!activeHostId) return;
    setRefreshing(true);
    try {
      await refreshHost(activeHostId, true);
    } finally {
      setRefreshing(false);
    }
  };

  const ackAllInbox = async () => {
    if (!activeHostId) return;
    const client = getHostClient(activeHostId);
    if (!client) return;
    await client.request("inbox.ack.request", {});
    await refreshHost(activeHostId, false);
  };

  const connColor =
    activeHost?.connection === "connected"
      ? palette.success
      : activeHost?.connection === "connecting"
        ? palette.warning
        : palette.danger;

  const connBg =
    activeHost?.connection === "connected"
      ? palette.successSoft
      : activeHost?.connection === "connecting"
        ? palette.warningSoft
        : palette.dangerSoft;

  const connLabel =
    activeHost?.connection === "connected"
      ? t("connected")
      : activeHost?.connection === "connecting"
        ? t("connecting")
        : t("lastKnown");

  const summaryTiles: Array<{
    id: AgentStatusFilter;
    label: string;
    count: number;
    dotColor: string;
    activeBg: string;
    activeBorder: string;
    highlightWhenNonZero?: boolean;
  }> = [
    {
      id: "all",
      label: t("filterAll"),
      count: statusCounts.all,
      dotColor: palette.accent,
      activeBg: palette.accentSoft,
      activeBorder: palette.accent,
    },
    {
      id: "needs_input",
      label: t("filterNeedsInput"),
      count: statusCounts.needs_input,
      dotColor: palette.warning,
      activeBg: palette.warningSoft,
      activeBorder: palette.warning,
      highlightWhenNonZero: true,
    },
    {
      id: "working",
      label: t("filterWorking"),
      count: statusCounts.working,
      dotColor: palette.success,
      activeBg: palette.successSoft,
      activeBorder: palette.success,
    },
    {
      id: "idle",
      label: t("filterIdle"),
      count: statusCounts.idle,
      dotColor: palette.muted,
      activeBg: palette.raised,
      activeBorder: palette.borderSubtle,
    },
  ];

  if (statusCounts.unavailable > 0 || statusFilter === "unavailable") {
    summaryTiles.push({
      id: "unavailable",
      label: t("filterUnavailable"),
      count: statusCounts.unavailable,
      dotColor: palette.danger,
      activeBg: palette.dangerSoft,
      activeBorder: palette.danger,
    });
  }

  return (
    <SafeAreaView
      style={[styles.root, { backgroundColor: palette.canvas }]}
      edges={["top"]}
    >
      {/* Unified top chrome: brand, host switcher, segmented tabs with one bottom hairline */}
      <View
        style={[
          styles.topChrome,
          { backgroundColor: palette.panel, borderBottomColor: palette.hairline },
        ]}
      >
        <View style={styles.header}>
          <View style={styles.brandGroup}>
            <Text style={[styles.brand, { color: palette.text }]}>masil</Text>
            <View style={[styles.connPill, { backgroundColor: connBg }]}>
              <View style={[styles.dot, { backgroundColor: connColor }]} />
              <Text style={[styles.connBadge, { color: connColor }]}>
                {connLabel}
              </Text>
            </View>
          </View>
          <View style={styles.headerIcons}>
            {activeHost ? (
              <>
                <Pressable
                  onPress={() => setShowFocusModal(true)}
                  hitSlop={6}
                  style={[
                    styles.headerBtn,
                    {
                      width: focusStatus?.active ? "auto" : 34,
                      paddingHorizontal: focusStatus?.active ? 8 : 0,
                      flexDirection: "row",
                      gap: 4,
                      backgroundColor: focusStatus?.active
                        ? palette.accentSoft
                        : palette.raised,
                      borderColor: focusStatus?.active
                        ? palette.accent
                        : "transparent",
                    },
                  ]}
                  accessibilityLabel={t("focusShield")}
                >
                  <Moon
                    size={15}
                    color={
                      focusStatus?.active
                        ? palette.accent
                        : palette.textSecondary
                    }
                    strokeWidth={1.9}
                  />
                  {focusStatus?.active ? (
                    <Text
                      style={{
                        color: palette.accent,
                        fontFamily: FONTS.semi,
                        fontSize: 11,
                      }}
                    >
                      {formatFocusRemaining(focusStatus.remaining_ms, isKo)}
                    </Text>
                  ) : null}
                </Pressable>
                <Pressable
                  onPress={() => setShowBriefing(true)}
                  hitSlop={6}
                  style={[
                    styles.headerBtn,
                    {
                      backgroundColor: palette.raised,
                      borderColor: "transparent",
                    },
                  ]}
                  accessibilityLabel={t("briefingTitle")}
                >
                  <Sun size={16} color={palette.warning} strokeWidth={1.9} />
                </Pressable>
                <Pressable
                  onPress={() => setShowPolicyModal(true)}
                  hitSlop={6}
                  style={[
                    styles.headerBtn,
                    {
                      backgroundColor: palette.raised,
                      borderColor: "transparent",
                    },
                  ]}
                  accessibilityLabel={t("policyFirewallTitle")}
                >
                  <Shield size={16} color={palette.accent} strokeWidth={1.9} />
                </Pressable>
                <Pressable
                  onPress={() => setShowArenaModal(true)}
                  hitSlop={6}
                  style={[
                    styles.headerBtn,
                    {
                      backgroundColor: palette.raised,
                      borderColor: "transparent",
                    },
                  ]}
                  accessibilityLabel={t("arenaTitle")}
                >
                  <Trophy size={16} color={palette.warning} strokeWidth={1.9} />
                </Pressable>
              </>
            ) : null}
            <Pressable
              onPress={() => router.push("/pair")}
              hitSlop={6}
              style={[
                styles.headerBtn,
                {
                  backgroundColor: palette.raised,
                  borderColor: "transparent",
                },
              ]}
              accessibilityLabel={t("pairHost")}
            >
              <QrCode size={17} color={palette.textSecondary} strokeWidth={1.85} />
            </Pressable>
            <Pressable
              onPress={() => router.push("/settings")}
              hitSlop={6}
              style={[
                styles.headerBtn,
                {
                  backgroundColor: palette.raised,
                  borderColor: "transparent",
                },
              ]}
              accessibilityLabel={t("settings")}
            >
              <Settings size={17} color={palette.textSecondary} strokeWidth={1.85} />
            </Pressable>
          </View>
        </View>

        {hostOrder.length > 0 ? (
          <ScrollView
            horizontal
            showsHorizontalScrollIndicator={false}
            style={styles.hostBar}
            contentContainerStyle={{
              paddingHorizontal: 14,
              paddingBottom: 8,
              gap: 8,
              alignItems: "center",
            }}
          >
            {hostOrder.map((hid) => {
              const h = hosts[hid];
              if (!h) return null;
              const active = hid === activeHostId;
              const hostCounts = countAgentsByCategory(h.agents);
              const alertBadge = hostCounts.needs_input || h.inbox.length;
              return (
                <Pressable
                  key={hid}
                  onPress={() => selectHost(hid)}
                  style={[
                    styles.hostChip,
                    {
                      backgroundColor: active
                        ? palette.raised
                        : palette.segmentTrack,
                      borderColor: "transparent",
                    },
                  ]}
                >
                  <Laptop
                    size={13}
                    color={active ? palette.textSecondary : palette.muted}
                  />
                  <Text
                    style={[
                      styles.hostChipText,
                      { color: active ? palette.text : palette.muted },
                    ]}
                  >
                    {h.host.hostName}
                  </Text>
                  {alertBadge > 0 ? (
                    <View
                      style={[
                        styles.hostAlertPill,
                        { backgroundColor: palette.warningSoft },
                      ]}
                    >
                      <Text
                        style={[
                          styles.hostAlertText,
                          { color: palette.warning },
                        ]}
                      >
                        {alertBadge}
                      </Text>
                    </View>
                  ) : null}
                  <ChevronDown size={13} color={palette.muted} />
                </Pressable>
              );
            })}
          </ScrollView>
        ) : null}

        <View style={styles.subBar}>
          <SegmentedTabs
            value={tab}
            onChange={setTab}
            options={[
              {
                id: "agents",
                label: t("agents"),
                count: activeHost?.agents.length ?? 0,
                icon: (c) => <Terminal size={13} color={c} />,
              },
              {
                id: "inbox",
                label: t("inbox"),
                count: activeHost?.inbox.length ?? 0,
                icon: (c) => <Inbox size={13} color={c} />,
              },
            ]}
          />
        </View>
      </View>

      {tab === "agents" ? (
        <>
          {/* At-a-glance status summary & one-tap filter strip */}
          <View style={styles.summaryStrip}>
            {summaryTiles.map((tile) => {
              const active = statusFilter === tile.id;
              const hasAttention =
                tile.highlightWhenNonZero && tile.count > 0 && !active;
              const dimmed = tile.id !== "all" && tile.count === 0 && !active;
              return (
                <Pressable
                  key={tile.id}
                  onPress={() => toggleStatusFilter(tile.id)}
                  style={[
                    styles.summaryTile,
                    {
                      backgroundColor: active
                        ? tile.activeBg
                        : hasAttention
                          ? palette.warningSoft
                          : palette.panel,
                      borderColor: active
                        ? tile.activeBorder
                        : hasAttention
                          ? palette.warning
                          : palette.hairline,
                    },
                  ]}
                  accessibilityRole="button"
                  accessibilityState={{ selected: active }}
                >
                  <View style={styles.summaryTopRow}>
                    <View
                      style={[
                        styles.summaryDot,
                        {
                          backgroundColor: tile.dotColor,
                          opacity: dimmed ? 0.4 : 1,
                        },
                      ]}
                    />
                    <Text
                      style={[
                        styles.summaryCount,
                        {
                          color: active
                            ? tile.id === "idle"
                              ? palette.text
                              : tile.dotColor
                            : hasAttention
                              ? palette.warning
                              : dimmed
                                ? palette.muted
                                : palette.text,
                        },
                      ]}
                    >
                      {tile.count}
                    </Text>
                  </View>
                  <Text
                    numberOfLines={1}
                    style={[
                      styles.summaryLabel,
                      {
                        color: active
                          ? palette.text
                          : hasAttention
                            ? palette.warning
                            : palette.muted,
                        fontFamily: active || hasAttention ? FONTS.semi : FONTS.regular,
                      },
                    ]}
                  >
                    {tile.label}
                  </Text>
                </Pressable>
              );
            })}
          </View>

          <View style={styles.searchRow}>
            <View
              style={[
                styles.searchBox,
                {
                  flex: 1,
                  backgroundColor: palette.panel,
                  borderColor: palette.hairline,
                },
              ]}
            >
              <Search size={15} color={palette.muted} />
              <View style={styles.searchInputWrap}>
                {query.length === 0 ? (
                  <Text
                    pointerEvents="none"
                    numberOfLines={1}
                    style={[styles.searchPlaceholder, { color: palette.muted }]}
                  >
                    {t("searchPlaceholder")}
                  </Text>
                ) : null}
                <TextInput
                  value={query}
                  onChangeText={setQuery}
                  style={[styles.searchInput, { color: palette.text }]}
                />
              </View>
              {query.length > 0 ? (
                <Pressable onPress={() => setQuery("")} hitSlop={8}>
                  <X size={15} color={palette.muted} />
                </Pressable>
              ) : null}
            </View>
            <Pressable
              onPress={() => setWarmestFirst((v) => !v)}
              style={[
                styles.warmSortBtn,
                {
                  backgroundColor: warmestFirst
                    ? palette.warningSoft
                    : palette.panel,
                  borderColor: warmestFirst
                    ? palette.warning
                    : palette.hairline,
                },
              ]}
              accessibilityRole="button"
              accessibilityState={{ selected: warmestFirst }}
            >
              <Flame
                size={13}
                color={warmestFirst ? palette.warning : palette.muted}
              />
              <Text
                style={[
                  styles.warmSortText,
                  { color: warmestFirst ? palette.warning : palette.muted },
                ]}
              >
                {t("sortWarmestFirst")}
              </Text>
            </Pressable>
          </View>

          <SectionList
            sections={sections}
            keyExtractor={(a) => a.remote_id}
            stickySectionHeadersEnabled={false}
            refreshControl={
              <RefreshControl
                refreshing={refreshing}
                onRefresh={() => void onRefresh()}
                tintColor={palette.accent}
              />
            }
            contentContainerStyle={styles.listContent}
            ListEmptyComponent={
              <View style={styles.emptyWrap}>
                <Text style={[styles.emptyText, { color: palette.muted }]}>
                  {statusFilter !== "all" && (activeHost?.agents.length ?? 0) > 0
                    ? t("emptyFilteredAgents")
                    : t("emptyAgents")}
                </Text>
                {statusFilter !== "all" ? (
                  <Pressable
                    onPress={() => setStatusFilter("all")}
                    style={[
                      styles.resetFilterBtn,
                      {
                        backgroundColor: palette.raised,
                        borderColor: palette.borderSubtle,
                      },
                    ]}
                  >
                    <Text
                      style={{
                        color: palette.accent,
                        fontFamily: FONTS.semi,
                        fontSize: 12.5,
                      }}
                    >
                      {t("showAllAgents")}
                    </Text>
                  </Pressable>
                ) : null}
              </View>
            }
            renderSectionHeader={({ section }) => {
              const meta = categoryMeta(section.category, palette, t);
              const collapsed =
                statusFilter === "all" &&
                Boolean(collapsedCategories[section.category]);
              return (
                <Pressable
                  onPress={() =>
                    statusFilter === "all"
                      ? toggleCategoryCollapse(section.category)
                      : undefined
                  }
                  style={styles.sectionHeader}
                >
                  <View style={styles.sectionHeaderLeft}>
                    <View
                      style={[
                        styles.sectionDot,
                        { backgroundColor: meta.color },
                      ]}
                    />
                    <Text
                      style={[
                        styles.sectionTitle,
                        {
                          color:
                            section.category === "needs_input"
                              ? palette.warning
                              : section.category === "working"
                                ? palette.success
                                : section.category === "unavailable"
                                  ? palette.danger
                                  : palette.textSecondary,
                        },
                      ]}
                    >
                      {meta.label}
                    </Text>
                    <View
                      style={[
                        styles.sectionCountBadge,
                        { backgroundColor: meta.bg },
                      ]}
                    >
                      <Text
                        style={[
                          styles.sectionCountText,
                          { color: meta.color },
                        ]}
                      >
                        {section.count}
                      </Text>
                    </View>
                  </View>
                  <View
                    style={[
                      styles.sectionLine,
                      { backgroundColor: palette.hairline },
                    ]}
                  />
                  {statusFilter === "all" ? (
                    collapsed ? (
                      <ChevronRight size={14} color={palette.muted} />
                    ) : (
                      <ChevronDown size={14} color={palette.muted} />
                    )
                  ) : null}
                </Pressable>
              );
            }}
            renderItem={({ item }: { item: AgentRow }) => {
              const selected = item.remote_id === activeAgentRemoteId;
              const badge = stateBadge(item, palette, t);
              const shortCwd = shortenHomePath(item.cwd);
              const leftRailColor = selected
                ? palette.accent
                : badge.category === "needs_input"
                  ? palette.warning
                  : badge.category === "working"
                    ? palette.success
                    : "transparent";
              return (
                <Pressable
                  onPress={() => {
                    if (activeHostId) {
                      selectAgent(activeHostId, item.remote_id);
                      selectPanel("agent");
                    }
                  }}
                  style={[
                    styles.agentCard,
                    {
                      backgroundColor: palette.panel,
                      borderColor:
                        badge.category === "needs_input" && !selected
                          ? palette.warningSoft
                          : palette.hairline,
                      borderLeftWidth: 3,
                      borderLeftColor: leftRailColor,
                    },
                  ]}
                >
                  <View style={styles.cardTop}>
                    <View style={styles.nameRow}>
                      <Text
                        numberOfLines={1}
                        style={[styles.agentName, { color: palette.text }]}
                      >
                        {item.name || item.pane}
                      </Text>
                    </View>
                    <View
                      style={[styles.statePill, { backgroundColor: badge.bg }]}
                    >
                      <View
                        style={[styles.dot, { backgroundColor: badge.color }]}
                      />
                      <Text style={[styles.stateText, { color: badge.color }]}>
                        {badge.label}
                      </Text>
                    </View>
                  </View>
                  {item.summary ? (
                    <Text
                      numberOfLines={1}
                      style={[styles.summarySnippet, { color: palette.muted }]}
                    >
                      {item.summary}
                    </Text>
                  ) : null}
                  {(() => {
                    const warmthLabel = formatCacheWarmthCompact(
                      item.cache_warmth
                    );
                    const critCount = item.audit_summary?.critical_count ?? 0;
                    const highCount = item.audit_summary?.high_count ?? 0;
                    const hasCollision =
                      Boolean(item.collision_severity) &&
                      item.collision_severity !== "clean";
                    const breakerTripped = Boolean(item.breaker?.tripped);
                    const sandboxInfo = parseSandboxBadge(item.sandbox);
                    if (
                      !warmthLabel &&
                      critCount === 0 &&
                      highCount === 0 &&
                      !hasCollision &&
                      !breakerTripped &&
                      !sandboxInfo
                    ) {
                      return null;
                    }
                    const colTone = collisionSeverityTone(
                      item.collision_severity
                    );
                    const colColor =
                      colTone === "danger"
                        ? palette.danger
                        : colTone === "warning"
                          ? palette.warning
                          : palette.accent;
                    const colBg =
                      colTone === "danger"
                        ? palette.dangerSoft
                        : colTone === "warning"
                          ? palette.warningSoft
                          : palette.accentSoft;
                    return (
                      <View style={styles.phase1Strip}>
                        {breakerTripped ? (
                          <View
                            style={[
                              styles.phase1Pill,
                              { backgroundColor: palette.dangerSoft },
                            ]}
                          >
                            <Text
                              style={[
                                styles.phase1PillText,
                                { color: palette.danger },
                              ]}
                            >
                              {t("breakerPausedBadge")}
                            </Text>
                          </View>
                        ) : null}
                        {critCount > 0 ? (
                          <View
                            style={[
                              styles.phase1Pill,
                              { backgroundColor: palette.dangerSoft },
                            ]}
                          >
                            <Text
                              style={[
                                styles.phase1PillText,
                                { color: palette.danger },
                              ]}
                            >
                              {t("auditBadgeCritical", { count: critCount })}
                            </Text>
                          </View>
                        ) : highCount > 0 ? (
                          <View
                            style={[
                              styles.phase1Pill,
                              { backgroundColor: palette.warningSoft },
                            ]}
                          >
                            <Text
                              style={[
                                styles.phase1PillText,
                                { color: palette.warning },
                              ]}
                            >
                              {t("auditBadgeHigh", { count: highCount })}
                            </Text>
                          </View>
                        ) : null}
                        {hasCollision ? (
                          <View
                            style={[
                              styles.phase1Pill,
                              { backgroundColor: colBg },
                            ]}
                          >
                            <Text
                              style={[
                                styles.phase1PillText,
                                { color: colColor },
                              ]}
                            >
                              {t("collisionBadge")}
                            </Text>
                          </View>
                        ) : null}
                        {warmthLabel ? (
                          <View
                            style={[
                              styles.phase1Pill,
                              { backgroundColor: palette.raised },
                            ]}
                          >
                            <Text
                              style={[
                                styles.phase1PillText,
                                { color: palette.textSecondary },
                              ]}
                            >
                              {warmthLabel}
                            </Text>
                          </View>
                        ) : null}
                        {sandboxInfo ? (
                          <View
                            style={[
                              styles.phase1Pill,
                              {
                                backgroundColor: sandboxInfo.sandboxed
                                  ? palette.successSoft
                                  : palette.warningSoft,
                              },
                            ]}
                          >
                            <Text
                              style={[
                                styles.phase1PillText,
                                {
                                  color: sandboxInfo.sandboxed
                                    ? palette.success
                                    : palette.warning,
                                },
                              ]}
                            >
                              {sandboxInfo.sandboxed
                                ? `🛡 ${t("sandboxBadgeOn")}`
                                : `⚠ ${t("sandboxBadgeOff")}`}
                            </Text>
                          </View>
                        ) : null}
                      </View>
                    );
                  })()}
                  <View style={styles.cardBottom}>
                    <View
                      style={[
                        styles.providerBadge,
                        { backgroundColor: palette.canvas },
                      ]}
                    >
                      <Text
                        style={[styles.providerTag, { color: palette.accent }]}
                      >
                        {item.provider}
                      </Text>
                    </View>
                    <Text
                      numberOfLines={1}
                      style={[styles.cwdText, { color: palette.textSecondary }]}
                    >
                      {shortCwd}
                    </Text>
                  </View>
                </Pressable>
              );
            }}
          />

          {activeHostId ? (
            <View
              style={[
                styles.bottomActions,
                {
                  backgroundColor: palette.panel,
                  borderTopColor: palette.hairline,
                  paddingBottom: Math.max(insets.bottom, 20) + 12,
                },
              ]}
            >
              <Pressable
                onPress={() => setShowNew(true)}
                style={[styles.actionBtn, { backgroundColor: palette.accent }]}
              >
                <Plus size={16} color={palette.accentText} strokeWidth={2.4} />
                <Text
                  style={[styles.primaryCtaText, { color: palette.accentText }]}
                >
                  {t("newAgent")}
                </Text>
              </Pressable>
              <Pressable
                onPress={() => setShowImport(true)}
                style={[
                  styles.actionBtn,
                  {
                    backgroundColor: palette.raised,
                    borderWidth: 1,
                    borderColor: palette.borderSubtle,
                  },
                ]}
              >
                <Download size={15} color={palette.text} />
                <Text style={[styles.secondaryCtaText, { color: palette.text }]}>
                  {t("importSession")}
                </Text>
              </Pressable>
            </View>
          ) : null}
        </>
      ) : (
        <View style={{ flex: 1 }}>
          {activeHostId ? (
            <BatchApproveBar
              hostId={activeHostId}
              safeCandidates={safeBatchCandidates}
            />
          ) : null}
          {(activeHost?.inbox.length ?? 0) > 0 ? (
            <Pressable
              onPress={() => void ackAllInbox()}
              style={[
                styles.ackAllBtn,
                {
                  backgroundColor: palette.raised,
                  borderColor: palette.borderSubtle,
                },
              ]}
            >
              <Text
                style={{
                  color: palette.accent,
                  fontFamily: FONTS.semi,
                  fontSize: 12,
                }}
              >
                {t("ackAll")}
              </Text>
            </Pressable>
          ) : null}
          <FlatList
            data={activeHost?.inbox ?? []}
            keyExtractor={(item, idx) => `${item.id ?? item.seq ?? idx}`}
            contentContainerStyle={{ padding: 14, gap: 8 }}
            ListEmptyComponent={
              <View style={styles.emptyWrap}>
                <Text style={[styles.emptyText, { color: palette.muted }]}>
                  {t("emptyInbox")}
                </Text>
              </View>
            }
            renderItem={({ item }) => {
              const kindDisplay = resolveInboxKindDisplay(item.kind);
              const kindColors = toneColors(kindDisplay.tone, palette);
              return (
                <Pressable
                  onPress={() => {
                    if (!activeHostId || !activeHost) return;
                    const match = activeHost.agents.find(
                      (a) => a.pane === item.pane || a.name === item.agent
                    );
                    if (match) {
                      selectAgent(activeHostId, match.remote_id);
                      selectPanel("agent");
                    }
                  }}
                  style={[
                    styles.agentCard,
                    {
                      backgroundColor: palette.panel,
                      borderColor: palette.hairline,
                      borderLeftWidth: 3,
                      borderLeftColor: kindColors.color,
                    },
                  ]}
                >
                  <View style={styles.cardTop}>
                    <Text style={[styles.agentName, { color: palette.text }]}>
                      {item.agent || item.pane}
                    </Text>
                    <View
                      style={[
                        styles.statePill,
                        { backgroundColor: kindColors.bg },
                      ]}
                    >
                      <View
                        style={[
                          styles.dot,
                          { backgroundColor: kindColors.color },
                        ]}
                      />
                      <Text
                        style={[
                          styles.stateText,
                          { color: kindColors.color },
                        ]}
                      >
                        {t(kindDisplay.key)}
                      </Text>
                    </View>
                  </View>
                  {item.summary ? (
                    <Text
                      numberOfLines={2}
                      style={{
                        color: palette.muted,
                        fontFamily: FONTS.regular,
                        fontSize: 12.5,
                      }}
                    >
                      {item.summary}
                    </Text>
                  ) : null}
                </Pressable>
              );
            }}
          />
        </View>
      )}

      {activeHostId ? (
        <>
          <NewAgentSheet
            visible={showNew}
            hostId={activeHostId}
            onClose={() => setShowNew(false)}
          />
          <ImportSessionSheet
            visible={showImport}
            hostId={activeHostId}
            onClose={() => setShowImport(false)}
          />
          <BriefingModal
            visible={showBriefing}
            hostId={activeHostId}
            onClose={() => setShowBriefing(false)}
          />
          <FocusShieldModal
            visible={showFocusModal}
            hostId={activeHostId}
            status={focusStatus}
            onClose={() => setShowFocusModal(false)}
            onEndedOpenBriefing={() => setShowBriefing(true)}
          />
          <PolicyFirewallModal
            visible={showPolicyModal}
            hostId={activeHostId}
            onClose={() => setShowPolicyModal(false)}
          />
          <ArenaScorecardModal
            visible={showArenaModal}
            hostId={activeHostId}
            defaultRepoPath={activeHost?.agents[0]?.cwd}
            onClose={() => setShowArenaModal(false)}
          />
        </>
      ) : null}
    </SafeAreaView>
  );
}

const styles = StyleSheet.create({
  root: { flex: 1 },
  topChrome: {
    borderBottomWidth: 1,
  },
  header: {
    flexDirection: "row",
    alignItems: "center",
    justifyContent: "space-between",
    paddingHorizontal: 14,
    paddingTop: 11,
    paddingBottom: 9,
  },
  brandGroup: {
    flexDirection: "row",
    alignItems: "center",
    gap: 10,
  },
  brand: {
    fontFamily: FONTS.bold,
    fontSize: 19,
    letterSpacing: -0.3,
  },
  connPill: {
    flexDirection: "row",
    alignItems: "center",
    gap: 5,
    paddingHorizontal: 8,
    paddingVertical: 3,
    borderRadius: 999,
  },
  dot: { width: 6, height: 6, borderRadius: 3 },
  connBadge: {
    fontFamily: FONTS.semi,
    fontSize: 11.5,
  },
  headerIcons: { flexDirection: "row", gap: 7, alignItems: "center" },
  headerBtn: {
    width: 34,
    height: 34,
    borderRadius: 9,
    borderWidth: 1,
    alignItems: "center",
    justifyContent: "center",
  },
  hostBar: { maxHeight: 42 },
  hostChip: {
    flexDirection: "row",
    alignItems: "center",
    gap: 6,
    paddingHorizontal: 11,
    paddingVertical: 6,
    borderRadius: 9,
    borderWidth: 1,
  },
  hostChipText: {
    fontFamily: FONTS.semi,
    fontSize: 12.5,
  },
  hostAlertPill: {
    paddingHorizontal: 5,
    paddingVertical: 1,
    borderRadius: 999,
  },
  hostAlertText: {
    fontFamily: FONTS.bold,
    fontVariant: TABULAR_NUMS,
    fontSize: 10.5,
  },
  subBar: {
    paddingHorizontal: 14,
    paddingBottom: 10,
  },
  summaryStrip: {
    flexDirection: "row",
    paddingHorizontal: 14,
    paddingTop: 11,
    gap: 6,
  },
  summaryTile: {
    flex: 1,
    borderWidth: 1,
    borderRadius: 10,
    paddingVertical: 7,
    paddingHorizontal: 6,
    alignItems: "center",
    gap: 2,
  },
  summaryTopRow: {
    flexDirection: "row",
    alignItems: "center",
    gap: 5,
  },
  summaryDot: {
    width: 6,
    height: 6,
    borderRadius: 3,
  },
  summaryCount: {
    fontFamily: FONTS.bold,
    fontVariant: TABULAR_NUMS,
    fontSize: 15,
    letterSpacing: -0.2,
  },
  summaryLabel: {
    fontSize: 11,
    letterSpacing: -0.1,
  },
  searchRow: { paddingHorizontal: 14, paddingTop: 9, paddingBottom: 6 },
  searchBox: {
    flexDirection: "row",
    alignItems: "center",
    borderWidth: 1,
    borderRadius: 10,
    paddingHorizontal: 11,
    gap: 8,
    height: 38,
  },
  searchInputWrap: {
    flex: 1,
    justifyContent: "center",
    position: "relative",
  },
  searchPlaceholder: {
    position: "absolute",
    left: 0,
    right: 0,
    fontFamily: FONTS.regular,
    fontSize: 13.5,
  },
  searchInput: {
    fontFamily: FONTS.regular,
    fontSize: 13.5,
    paddingVertical: 0,
  },
  listContent: {
    paddingHorizontal: 14,
    paddingBottom: 18,
  },
  sectionHeader: {
    flexDirection: "row",
    alignItems: "center",
    gap: 8,
    paddingTop: 10,
    paddingBottom: 7,
  },
  sectionHeaderLeft: {
    flexDirection: "row",
    alignItems: "center",
    gap: 6,
  },
  sectionDot: {
    width: 7,
    height: 7,
    borderRadius: 3.5,
  },
  sectionTitle: {
    fontFamily: FONTS.bold,
    fontSize: 12,
    letterSpacing: 0.1,
  },
  sectionCountBadge: {
    paddingHorizontal: 6,
    paddingVertical: 1,
    borderRadius: 999,
  },
  sectionCountText: {
    fontFamily: FONTS.semi,
    fontVariant: TABULAR_NUMS,
    fontSize: 11,
  },
  sectionLine: {
    flex: 1,
    height: 1,
  },
  agentCard: {
    borderWidth: 1,
    borderRadius: 12,
    paddingHorizontal: 14,
    paddingVertical: 11,
    gap: 7,
    marginBottom: 8,
  },
  cardTop: {
    flexDirection: "row",
    justifyContent: "space-between",
    alignItems: "center",
    gap: 8,
  },
  nameRow: {
    flexDirection: "row",
    alignItems: "center",
    gap: 7,
    flex: 1,
    minWidth: 0,
  },
  agentName: {
    fontFamily: FONTS.bold,
    fontSize: 14.5,
    letterSpacing: -0.1,
    flexShrink: 1,
  },
  statePill: {
    flexDirection: "row",
    alignItems: "center",
    gap: 5,
    paddingHorizontal: 8,
    paddingVertical: 3,
    borderRadius: 999,
  },
  stateText: {
    fontFamily: FONTS.semi,
    fontSize: 11.5,
  },
  summarySnippet: {
    fontFamily: FONTS.regular,
    fontSize: 12,
  },
  cardBottom: {
    flexDirection: "row",
    alignItems: "center",
    gap: 8,
  },
  providerBadge: {
    paddingHorizontal: 6,
    paddingVertical: 2,
    borderRadius: 5,
  },
  providerTag: {
    fontFamily: FONTS.monoSemi,
    fontSize: 10.5,
    textTransform: "lowercase",
  },
  cwdText: {
    fontFamily: FONTS.mono,
    fontVariant: TABULAR_NUMS,
    fontSize: 11.5,
    flex: 1,
  },
  emptyWrap: {
    paddingVertical: 48,
    alignItems: "center",
    gap: 12,
  },
  emptyText: {
    fontFamily: FONTS.regular,
    fontSize: 13.5,
  },
  resetFilterBtn: {
    paddingHorizontal: 14,
    paddingVertical: 8,
    borderRadius: 8,
    borderWidth: 1,
  },
  bottomActions: {
    flexDirection: "row",
    paddingHorizontal: 14,
    paddingTop: 12,
    gap: 8,
    borderTopWidth: 1,
  },
  actionBtn: {
    flex: 1,
    flexDirection: "row",
    alignItems: "center",
    justifyContent: "center",
    gap: 6,
    height: 44,
    borderRadius: 10,
  },
  primaryCtaText: {
    fontFamily: FONTS.bold,
    fontSize: 13.5,
  },
  secondaryCtaText: {
    fontFamily: FONTS.semi,
    fontSize: 13,
  },
  ackAllBtn: {
    alignSelf: "flex-end",
    margin: 14,
    marginBottom: 0,
    paddingHorizontal: 10,
    paddingVertical: 6,
    borderRadius: 7,
    borderWidth: 1,
  },
  warmSortBtn: {
    flexDirection: "row",
    alignItems: "center",
    gap: 4,
    paddingHorizontal: 10,
    height: 36,
    borderRadius: 10,
    borderWidth: 1,
  },
  warmSortText: {
    fontFamily: FONTS.semi,
    fontSize: 11.5,
  },
  phase1Strip: {
    flexDirection: "row",
    flexWrap: "wrap",
    gap: 5,
  },
  phase1Pill: {
    paddingHorizontal: 6,
    paddingVertical: 2,
    borderRadius: 6,
  },
  phase1PillText: {
    fontFamily: FONTS.monoSemi,
    fontVariant: TABULAR_NUMS,
    fontSize: 10,
  },
});
