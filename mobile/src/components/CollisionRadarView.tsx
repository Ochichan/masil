import React, { useCallback, useEffect, useState } from "react";
import {
  ActivityIndicator,
  Pressable,
  RefreshControl,
  ScrollView,
  StyleSheet,
  Text,
  TextInput,
  View,
} from "react-native";
import {
  AlertTriangle,
  BellRing,
  GitMerge,
  Play,
  RefreshCw,
} from "lucide-react-native";
import { useTranslation } from "react-i18next";
import { collisionSeverityTone } from "../agents/phase1-helpers";
import { getHostClient } from "../hosts/HostRuntimeController";
import type {
  AgentRow,
  CollisionPair,
  UiDocument,
} from "../protocol/schemas";
import { useMasilTheme } from "../theme/ThemeProvider";
import { FONTS } from "../theme/typography";
import { PluginUiRenderer } from "./PluginUiRenderer";

interface CollisionRadarViewProps {
  hostId: string;
  agent: AgentRow;
}

export function CollisionRadarView({
  hostId,
  agent,
}: CollisionRadarViewProps) {
  const { palette } = useMasilTheme();
  const { t } = useTranslation();

  const [pairs, setPairs] = useState<CollisionPair[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [feedback, setFeedback] = useState<string | null>(null);
  const [shadowCmd, setShadowCmd] = useState("npm test");
  const [shadowDoc, setShadowDoc] = useState<UiDocument | null>(null);
  const [busyKey, setBusyKey] = useState<string | null>(null);

  const loadMatrix = useCallback(async () => {
    const client = getHostClient(hostId);
    if (!client) {
      setError(t("hostNotConnected"));
      return;
    }
    setLoading(true);
    setError(null);
    try {
      const res = await client.request<{
        pairs?: CollisionPair[];
        collisions?: CollisionPair[];
      }>("collision.matrix.request", {
        server_socket: agent.server_socket,
        repo: agent.cwd,
      });
      const list = res.pairs ?? res.collisions ?? [];
      setPairs(Array.isArray(list) ? list : []);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }, [hostId, agent.server_socket, agent.cwd, t]);

  useEffect(() => {
    void loadMatrix();
  }, [loadMatrix]);

  const warnSibling = async (target: string, sibling: string, key: string) => {
    const client = getHostClient(hostId);
    if (!client) return;
    setBusyKey(key);
    setError(null);
    setFeedback(null);
    try {
      await client.request("collision.warn_agent.request", {
        server_socket: agent.server_socket,
        target,
        sibling,
      });
      setFeedback(t("collisionWarnQueued", { target, sibling }));
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusyKey(null);
    }
  };

  const runShadowCheck = async (pair: CollisionPair, key: string) => {
    const cmd = shadowCmd.trim();
    if (!cmd) return;
    const client = getHostClient(hostId);
    if (!client) return;
    setBusyKey(key);
    setError(null);
    setFeedback(null);
    try {
      const res = await client.request<{
        ui?: UiDocument;
        passed?: boolean;
        summary?: string;
      }>("collision.shadow_check.request", {
        worktree_a: pair.worktree_a || pair.agent_a,
        worktree_b: pair.worktree_b || pair.agent_b,
        cmd,
        repo: agent.cwd,
      });
      if (res.ui) {
        setShadowDoc(res.ui);
      } else {
        setFeedback(
          res.summary ||
            (res.passed ? t("shadowCheckPassed") : t("shadowCheckFailed"))
        );
      }
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusyKey(null);
    }
  };

  const toneColors = (sev: CollisionPair["severity"]) => {
    const tone = collisionSeverityTone(sev);
    if (tone === "danger") {
      return { fg: palette.danger, bg: palette.dangerSoft };
    }
    if (tone === "warning") {
      return { fg: palette.warning, bg: palette.warningSoft };
    }
    if (tone === "accent") {
      return { fg: palette.accent, bg: palette.accentSoft };
    }
    return { fg: palette.muted, bg: palette.raised };
  };

  return (
    <ScrollView
      style={styles.root}
      contentContainerStyle={styles.content}
      refreshControl={
        <RefreshControl
          refreshing={loading}
          onRefresh={() => void loadMatrix()}
          tintColor={palette.accent}
        />
      }
    >
      <View style={styles.toolbar}>
        <View style={styles.toolbarLeft}>
          <GitMerge size={15} color={palette.accent} />
          <Text style={[styles.heading, { color: palette.text }]}>
            {t("collisionRadarTitle")}
          </Text>
        </View>
        <Pressable
          onPress={() => void loadMatrix()}
          style={[
            styles.refreshBtn,
            { backgroundColor: palette.raised, borderColor: palette.hairline },
          ]}
        >
          <RefreshCw size={13} color={palette.textSecondary} />
        </Pressable>
      </View>

      <View
        style={[
          styles.cmdBox,
          { backgroundColor: palette.panel, borderColor: palette.hairline },
        ]}
      >
        <Text style={[styles.cmdLabel, { color: palette.muted }]}>
          {t("shadowCheckCommandLabel")}
        </Text>
        <TextInput
          value={shadowCmd}
          onChangeText={setShadowCmd}
          placeholder="cargo test / npm test"
          placeholderTextColor={palette.muted}
          autoCapitalize="none"
          autoCorrect={false}
          style={[
            styles.cmdInput,
            {
              color: palette.text,
              backgroundColor: palette.canvas,
              borderColor: palette.hairline,
            },
          ]}
        />
      </View>

      {feedback ? (
        <Text style={[styles.feedbackText, { color: palette.success }]}>
          {feedback}
        </Text>
      ) : null}
      {error ? (
        <Text style={[styles.feedbackText, { color: palette.danger }]}>
          {error}
        </Text>
      ) : null}

      {loading && pairs.length === 0 ? (
        <View style={styles.emptyWrap}>
          <ActivityIndicator size="small" color={palette.accent} />
        </View>
      ) : pairs.length === 0 ? (
        <View
          style={[
            styles.emptyCard,
            { backgroundColor: palette.panel, borderColor: palette.hairline },
          ]}
        >
          <Text style={[styles.emptyText, { color: palette.muted }]}>
            {t("collisionNoneFound")}
          </Text>
        </View>
      ) : (
        pairs.map((pair, idx) => {
          const colors = toneColors(pair.severity);
          const pairKey = `${pair.agent_a}::${pair.agent_b}::${idx}`;
          const warnKeyA = `warnA:${pairKey}`;
          const warnKeyB = `warnB:${pairKey}`;
          const shadowKey = `shadow:${pairKey}`;
          return (
            <View
              key={pairKey}
              style={[
                styles.pairCard,
                {
                  backgroundColor: palette.panel,
                  borderColor:
                    pair.severity === "clean" ? palette.hairline : colors.fg,
                },
              ]}
            >
              <View style={styles.pairHeader}>
                <View style={styles.pairNames}>
                  <AlertTriangle size={14} color={colors.fg} />
                  <Text style={[styles.pairTitle, { color: palette.text }]}>
                    {pair.agent_a} ↔ {pair.agent_b}
                  </Text>
                </View>
                <View
                  style={[styles.sevPill, { backgroundColor: colors.bg }]}
                >
                  <Text style={[styles.sevText, { color: colors.fg }]}>
                    {pair.severity.replace(/_/g, " ").toUpperCase()}
                  </Text>
                </View>
              </View>

              {pair.files.length > 0 ? (
                <Text
                  style={[styles.filesText, { color: palette.textSecondary }]}
                >
                  {pair.files.join(", ")}
                </Text>
              ) : null}

              {pair.details.map((d, dIdx) => (
                <Text
                  key={dIdx}
                  style={[styles.detailText, { color: palette.muted }]}
                >
                  • {d}
                </Text>
              ))}

              <View style={styles.actionRow}>
                <Pressable
                  disabled={busyKey !== null}
                  onPress={() =>
                    void warnSibling(
                      pair.pane_a || pair.agent_a,
                      pair.agent_b,
                      warnKeyA
                    )
                  }
                  style={[
                    styles.actionBtn,
                    {
                      backgroundColor: palette.warningSoft,
                      borderColor: palette.warning,
                    },
                  ]}
                >
                  <BellRing size={12} color={palette.warning} />
                  <Text
                    style={[styles.actionBtnText, { color: palette.warning }]}
                  >
                    {t("collisionWarnBtn", { name: pair.agent_a })}
                  </Text>
                </Pressable>

                <Pressable
                  disabled={busyKey !== null}
                  onPress={() =>
                    void warnSibling(
                      pair.pane_b || pair.agent_b,
                      pair.agent_a,
                      warnKeyB
                    )
                  }
                  style={[
                    styles.actionBtn,
                    {
                      backgroundColor: palette.warningSoft,
                      borderColor: palette.warning,
                    },
                  ]}
                >
                  <BellRing size={12} color={palette.warning} />
                  <Text
                    style={[styles.actionBtnText, { color: palette.warning }]}
                  >
                    {t("collisionWarnBtn", { name: pair.agent_b })}
                  </Text>
                </Pressable>

                {pair.worktree_a &&
                pair.worktree_b &&
                pair.worktree_a !== pair.worktree_b ? (
                  <Pressable
                    disabled={busyKey !== null}
                    onPress={() => void runShadowCheck(pair, shadowKey)}
                    style={[
                      styles.actionBtn,
                      {
                        backgroundColor: palette.accentSoft,
                        borderColor: palette.accent,
                      },
                    ]}
                  >
                    {busyKey === shadowKey ? (
                      <ActivityIndicator size="small" color={palette.accent} />
                    ) : (
                      <Play size={12} color={palette.accent} />
                    )}
                    <Text
                      style={[styles.actionBtnText, { color: palette.accent }]}
                    >
                      {t("shadowCheckRun")}
                    </Text>
                  </Pressable>
                ) : null}
              </View>
            </View>
          );
        })
      )}

      {shadowDoc ? (
        <View
          style={[
            styles.shadowPanel,
            { backgroundColor: palette.panel, borderColor: palette.accentBorder },
          ]}
        >
          <PluginUiRenderer blocks={shadowDoc.blocks ?? []} />
        </View>
      ) : null}
    </ScrollView>
  );
}

const styles = StyleSheet.create({
  root: { flex: 1 },
  content: { padding: 14, gap: 10, paddingBottom: 28 },
  toolbar: {
    flexDirection: "row",
    alignItems: "center",
    justifyContent: "space-between",
  },
  toolbarLeft: {
    flexDirection: "row",
    alignItems: "center",
    gap: 7,
  },
  heading: {
    fontFamily: FONTS.bold,
    fontSize: 14.5,
  },
  refreshBtn: {
    width: 30,
    height: 30,
    borderRadius: 8,
    borderWidth: 1,
    alignItems: "center",
    justifyContent: "center",
  },
  cmdBox: {
    borderWidth: 1,
    borderRadius: 10,
    padding: 10,
    gap: 6,
  },
  cmdLabel: {
    fontFamily: FONTS.semi,
    fontSize: 11.5,
  },
  cmdInput: {
    borderWidth: 1,
    borderRadius: 8,
    paddingHorizontal: 10,
    paddingVertical: 7,
    fontFamily: FONTS.mono,
    fontSize: 12,
  },
  feedbackText: {
    fontFamily: FONTS.semi,
    fontSize: 12,
  },
  emptyWrap: {
    paddingVertical: 28,
    alignItems: "center",
  },
  emptyCard: {
    borderWidth: 1,
    borderRadius: 12,
    padding: 18,
    alignItems: "center",
  },
  emptyText: {
    fontFamily: FONTS.regular,
    fontSize: 13,
    textAlign: "center",
  },
  pairCard: {
    borderWidth: 1,
    borderRadius: 12,
    padding: 12,
    gap: 7,
  },
  pairHeader: {
    flexDirection: "row",
    alignItems: "center",
    justifyContent: "space-between",
    gap: 8,
  },
  pairNames: {
    flexDirection: "row",
    alignItems: "center",
    gap: 6,
    flex: 1,
  },
  pairTitle: {
    fontFamily: FONTS.bold,
    fontSize: 13.5,
  },
  sevPill: {
    paddingHorizontal: 7,
    paddingVertical: 2,
    borderRadius: 999,
  },
  sevText: {
    fontFamily: FONTS.bold,
    fontSize: 10,
  },
  filesText: {
    fontFamily: FONTS.mono,
    fontSize: 11.5,
  },
  detailText: {
    fontFamily: FONTS.regular,
    fontSize: 12,
    lineHeight: 17,
  },
  actionRow: {
    flexDirection: "row",
    flexWrap: "wrap",
    gap: 7,
    marginTop: 4,
  },
  actionBtn: {
    flexDirection: "row",
    alignItems: "center",
    gap: 5,
    borderWidth: 1,
    borderRadius: 8,
    paddingHorizontal: 9,
    paddingVertical: 6,
  },
  actionBtnText: {
    fontFamily: FONTS.semi,
    fontSize: 11.5,
  },
  shadowPanel: {
    borderWidth: 1,
    borderRadius: 12,
    padding: 10,
  },
});
