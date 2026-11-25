import React, { useMemo, useState } from "react";
import { Pressable, StyleSheet, Text, View } from "react-native";
import { ChevronLeft, Sparkles } from "lucide-react-native";
import { useTranslation } from "react-i18next";
import { SafeAreaView } from "react-native-safe-area-context";
import { CheckpointsView } from "../components/CheckpointsView";
import { FileExplorerView } from "../components/FileExplorerView";
import { GitDiffView } from "../components/GitDiffView";
import { PluginCreatorSheet } from "../components/PluginCreatorSheet";
import { PluginPanelView } from "../components/PluginPanelView";
import { SchedulesView } from "../components/SchedulesView";
import { SegmentedTabs } from "../components/SegmentedTabs";
import { useHostRuntimeStore } from "../hosts/HostRuntimeController";
import { useMobilePanelsStore } from "../mobile-panels/store";
import type { AgentRow, PluginPanelDef } from "../protocol/schemas";
import { useMasilTheme } from "../theme/ThemeProvider";
import { FONTS, shortenHomePath } from "../theme/typography";

interface WorkspaceToolsPanelProps {
  hostId: string | null;
  agent: AgentRow | null;
}

export function WorkspaceToolsPanel({
  hostId,
  agent,
}: WorkspaceToolsPanelProps) {
  const { palette } = useMasilTheme();
  const { t } = useTranslation();
  const [tab, setTab] = useState<string>("changes");
  const [showCreator, setShowCreator] = useState(false);
  const selectPanel = useMobilePanelsStore((s) => s.selectPanel);
  const plugins = useHostRuntimeStore((s) =>
    hostId ? s.hosts[hostId]?.plugins ?? null : null
  );

  const pluginPanels = useMemo<PluginPanelDef[]>(
    () => plugins?.panels ?? [],
    [plugins]
  );

  const tabs = useMemo(() => {
    const base: Array<{ id: string; label: string }> = [
      { id: "changes", label: t("changes") },
      { id: "files", label: t("files") },
      { id: "checkpoints", label: t("checkpoints") },
      { id: "schedules", label: t("schedules") },
    ];
    for (const p of pluginPanels) {
      base.push({ id: `plugin:${p.plugin_id}:${p.id}`, label: p.title });
    }
    return base;
  }, [pluginPanels, t]);

  const activePluginPanel = useMemo(() => {
    if (!tab.startsWith("plugin:")) return null;
    return (
      pluginPanels.find((p) => `plugin:${p.plugin_id}:${p.id}` === tab) ?? null
    );
  }, [tab, pluginPanels]);

  const effectiveTab =
    tab.startsWith("plugin:") && !activePluginPanel ? "changes" : tab;

  const shortCwd = shortenHomePath(agent?.cwd);

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
            onPress={() => selectPanel("agent")}
            style={styles.backBtn}
            hitSlop={8}
          >
            <ChevronLeft size={19} color={palette.accent} />
            <Text style={[styles.backTitle, { color: palette.text }]}>
              {agent?.name ?? t("agents")}
            </Text>
          </Pressable>
          <Text
            numberOfLines={1}
            ellipsizeMode="middle"
            style={[styles.cwdText, { color: palette.textSecondary }]}
          >
            {shortCwd}
          </Text>
          {hostId ? (
            <Pressable
              onPress={() => setShowCreator(true)}
              hitSlop={8}
              style={[
                styles.creatorBtn,
                {
                  backgroundColor: palette.accentSoft,
                  borderColor: palette.accentBorder,
                },
              ]}
              accessibilityLabel={t("createPluginWithAi")}
            >
              <Sparkles size={13} color={palette.accent} />
            </Pressable>
          ) : null}
        </View>

        <View style={styles.segmentRow}>
          <SegmentedTabs
            options={tabs}
            value={effectiveTab}
            onChange={setTab}
          />
        </View>
      </View>

      {activePluginPanel && hostId ? (
        <PluginPanelView
          hostId={hostId}
          agent={agent}
          panel={activePluginPanel}
        />
      ) : !hostId || !agent ? (
        <View style={styles.emptyRoot}>
          <Text style={[styles.emptyText, { color: palette.muted }]}>
            에이전트를 선택하면 변경 내역, 파일, 체크포인트, 예약을 확인할 수 있습니다.
          </Text>
        </View>
      ) : effectiveTab === "changes" ? (
        <GitDiffView
          hostId={hostId}
          cwd={agent.cwd}
          onOpenFiles={() => setTab("files")}
        />
      ) : effectiveTab === "files" ? (
        <FileExplorerView hostId={hostId} cwd={agent.cwd} />
      ) : effectiveTab === "checkpoints" ? (
        <CheckpointsView hostId={hostId} agent={agent} />
      ) : (
        <SchedulesView hostId={hostId} serverSocket={agent.server_socket} />
      )}

      {hostId ? (
        <PluginCreatorSheet
          visible={showCreator}
          hostId={hostId}
          onClose={() => setShowCreator(false)}
        />
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
    paddingHorizontal: 14,
    paddingTop: 11,
    paddingBottom: 8,
    gap: 8,
  },
  backBtn: {
    flexDirection: "row",
    alignItems: "center",
    gap: 3,
  },
  backTitle: {
    fontFamily: FONTS.bold,
    fontSize: 15.5,
    letterSpacing: -0.2,
  },
  cwdText: {
    fontFamily: FONTS.mono,
    fontSize: 11.5,
    flex: 1,
    textAlign: "right",
  },
  creatorBtn: {
    width: 28,
    height: 28,
    borderRadius: 8,
    borderWidth: 1,
    alignItems: "center",
    justifyContent: "center",
  },
  segmentRow: {
    paddingHorizontal: 14,
    paddingBottom: 10,
  },
  emptyRoot: {
    flex: 1,
    alignItems: "center",
    justifyContent: "center",
    padding: 28,
  },
  emptyText: {
    fontFamily: FONTS.regular,
    fontSize: 13.5,
    textAlign: "center",
    lineHeight: 20,
  },
});
