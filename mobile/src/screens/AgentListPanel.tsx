import React, { useMemo, useState } from "react";
import {
  FlatList,
  Pressable,
  RefreshControl,
  ScrollView,
  StyleSheet,
  Text,
  TextInput,
  View,
} from "react-native";
import { useRouter } from "expo-router";
import {
  ChevronDown,
  Download,
  Inbox,
  Laptop,
  Plus,
  QrCode,
  Search,
  Settings,
  Terminal,
  X,
} from "lucide-react-native";
import { useTranslation } from "react-i18next";
import { SafeAreaView, useSafeAreaInsets } from "react-native-safe-area-context";
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

function stateBadge(
  state: string,
  palette: ReturnType<typeof useMasilTheme>["palette"],
  t: (k: string) => string
) {
  switch (state) {
    case "working":
    case "retrying":
      return { label: t("working"), color: palette.success, bg: palette.successSoft };
    case "blocked":
      return { label: t("needsInput"), color: palette.warning, bg: palette.warningSoft };
    case "idle":
      return { label: t("idle"), color: palette.muted, bg: palette.raised };
    default:
      return { label: t("stateUnavailable"), color: palette.danger, bg: palette.dangerSoft };
  }
}

export function AgentListPanel() {
  const { palette } = useMasilTheme();
  const { t } = useTranslation();
  const insets = useSafeAreaInsets();
  const router = useRouter();
  const hostOrder = useHostRuntimeStore((s) => s.hostOrder);
  const hosts = useHostRuntimeStore((s) => s.hosts);
  const activeHostId = useHostRuntimeStore((s) => s.activeHostId);
  const activeAgentRemoteId = useHostRuntimeStore((s) => s.activeAgentRemoteId);
  const selectHost = useHostRuntimeStore((s) => s.selectHost);
  const selectAgent = useHostRuntimeStore((s) => s.selectAgent);
  const refreshHost = useHostRuntimeStore((s) => s.refreshHost);
  const selectPanel = useMobilePanelsStore((s) => s.selectPanel);

  const [tab, setTab] = useState<"agents" | "inbox">("agents");
  const [query, setQuery] = useState("");
  const [refreshing, setRefreshing] = useState(false);
  const [showNew, setShowNew] = useState(false);
  const [showImport, setShowImport] = useState(false);

  const activeHost = activeHostId ? hosts[activeHostId] : undefined;

  const filteredAgents = useMemo(() => {
    const list = activeHost?.agents ?? [];
    const matched = list.filter((a) =>
      matchesMasilQuery(
        `${a.name} ${a.provider} ${a.cwd} ${a.server_name} ${a.pane}`,
        query
      )
    );
    return [...matched].sort((a, b) => {
      const pa = a.state === "blocked" ? 0 : a.state === "working" ? 1 : 2;
      const pb = b.state === "blocked" ? 0 : b.state === "working" ? 1 : 2;
      return pa - pb || a.name.localeCompare(b.name);
    });
  }, [activeHost?.agents, query]);

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
              const unread = h.inbox.length;
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
                    {unread > 0 ? ` (${unread})` : ""}
                  </Text>
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
          <View style={styles.searchRow}>
            <View
              style={[
                styles.searchBox,
                {
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
          </View>

          <FlatList
            data={filteredAgents}
            keyExtractor={(a) => a.remote_id}
            refreshControl={
              <RefreshControl
                refreshing={refreshing}
                onRefresh={() => void onRefresh()}
                tintColor={palette.accent}
              />
            }
            contentContainerStyle={{
              paddingHorizontal: 14,
              paddingBottom: 18,
              gap: 8,
            }}
            ListEmptyComponent={
              <View style={styles.emptyWrap}>
                <Text style={[styles.emptyText, { color: palette.muted }]}>
                  {t("emptyAgents")}
                </Text>
              </View>
            }
            renderItem={({ item }: { item: AgentRow }) => {
              const selected = item.remote_id === activeAgentRemoteId;
              const badge = stateBadge(item.state, palette, t);
              const shortCwd = shortenHomePath(item.cwd);
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
                        borderColor: palette.hairline,
                        borderLeftWidth: 2.5,
                        borderLeftColor: selected
                          ? palette.accent
                          : "transparent",
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
            renderItem={({ item }) => (
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
                    borderColor: palette.warning,
                  },
                ]}
              >
                <View style={styles.cardTop}>
                  <Text style={[styles.agentName, { color: palette.text }]}>
                    {item.agent || item.pane}
                  </Text>
                  <Text
                    style={{
                      color: palette.warning,
                      fontFamily: FONTS.semi,
                      fontSize: 11.5,
                    }}
                  >
                    {item.kind}
                  </Text>
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
            )}
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
  subBar: {
    paddingHorizontal: 14,
    paddingBottom: 10,
  },
  searchRow: { paddingHorizontal: 14, paddingTop: 12, paddingBottom: 10 },
  searchBox: {
    flexDirection: "row",
    alignItems: "center",
    borderWidth: 1,
    borderRadius: 10,
    paddingHorizontal: 11,
    gap: 8,
    height: 40,
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
  agentCard: {
    borderWidth: 1,
    borderRadius: 12,
    paddingHorizontal: 14,
    paddingVertical: 12,
    gap: 8,
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
  activeDot: {
    width: 6,
    height: 6,
    borderRadius: 3,
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
  },
  emptyText: {
    fontFamily: FONTS.regular,
    fontSize: 13.5,
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
});
