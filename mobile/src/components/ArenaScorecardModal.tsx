import React, { useCallback, useEffect, useState } from "react";
import {
  ActivityIndicator,
  Modal,
  Pressable,
  ScrollView,
  StyleSheet,
  Text,
  TextInput,
  View,
} from "react-native";
import { CheckCircle2, GitBranch, Play, Sparkles, Trophy, X } from "lucide-react-native";
import { useTranslation } from "react-i18next";
import { useSafeAreaInsets } from "react-native-safe-area-context";
import { rankArenaRacers } from "../agents/phase3-helpers";
import { getHostClient, useHostRuntimeStore } from "../hosts/HostRuntimeController";
import type { ArenaListItem } from "../protocol/schemas";
import { useMasilTheme } from "../theme/ThemeProvider";
import { FONTS } from "../theme/typography";
import { SelectableChip } from "./SelectableChip";

interface ArenaScorecardModalProps {
  visible: boolean;
  hostId: string;
  defaultRepoPath?: string;
  onClose: () => void;
}

const RACER_CHOICES = ["claude", "codex", "agy", "gemini", "opencode"];

export function ArenaScorecardModal({
  visible,
  hostId,
  defaultRepoPath,
  onClose,
}: ArenaScorecardModalProps) {
  const { palette } = useMasilTheme();
  const { t } = useTranslation();
  const insets = useSafeAreaInsets();
  const refreshHost = useHostRuntimeStore((s) => s.refreshHost);

  const [arenas, setArenas] = useState<ArenaListItem[]>([]);
  const [loading, setLoading] = useState(false);
  const [busyKey, setBusyKey] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [toast, setToast] = useState<string | null>(null);

  const [prompt, setPrompt] = useState("");
  const [verifyCmd, setVerifyCmd] = useState("");
  const [selectedRacers, setSelectedRacers] = useState<string[]>(["claude", "codex"]);
  const [loserMode, setLoserMode] = useState<"archive" | "clean">("archive");

  const loadArenas = useCallback(async () => {
    const client = getHostClient(hostId);
    if (!client) return;
    setLoading(true);
    setError(null);
    try {
      const res = await client.request<{ arenas?: ArenaListItem[] }>(
        "arena.list.request",
        { limit: 15 }
      );
      setArenas(res.arenas ?? []);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }, [hostId]);

  useEffect(() => {
    if (visible) {
      setToast(null);
      void loadArenas();
    }
  }, [visible, loadArenas]);

  const toggleRacer = (id: string) => {
    setSelectedRacers((prev) =>
      prev.includes(id)
        ? prev.length > 2
          ? prev.filter((r) => r !== id)
          : prev
        : [...prev, id]
    );
  };

  const handleStartArena = async () => {
    const client = getHostClient(hostId);
    if (!client || !prompt.trim() || selectedRacers.length < 2) return;
    setBusyKey("start");
    setError(null);
    try {
      await client.request("arena.start.request", {
        prompt: prompt.trim(),
        repo_path: defaultRepoPath || ".",
        racers: selectedRacers,
        verify_cmd: verifyCmd.trim() || undefined,
        launch_agents: true,
      });
      setPrompt("");
      setVerifyCmd("");
      await loadArenas();
      await refreshHost(hostId, false);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusyKey(null);
    }
  };

  const handleVerify = async (arenaId: string) => {
    const client = getHostClient(hostId);
    if (!client) return;
    setBusyKey(`verify:${arenaId}`);
    setError(null);
    try {
      await client.request("arena.verify.request", { arena_id: arenaId });
      await loadArenas();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusyKey(null);
    }
  };

  const handlePromote = async (
    arenaId: string,
    winner: string,
    mode: "branch" | "squash"
  ) => {
    const client = getHostClient(hostId);
    if (!client) return;
    setBusyKey(`promote:${arenaId}:${winner}:${mode}`);
    setError(null);
    try {
      const res = await client.request<{
        promoted_branch?: string | null;
        safety_checkpoint_id?: number | null;
      }>("arena.promote.request", {
        arena_id: arenaId,
        winner,
        mode,
        losers: loserMode,
      });
      setToast(
        res.promoted_branch
          ? `Promoted ${winner} → ${res.promoted_branch}`
          : `Squashed ${winner}${res.safety_checkpoint_id ? ` (safety ckpt #${res.safety_checkpoint_id})` : ""}`
      );
      await loadArenas();
      await refreshHost(hostId, false);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusyKey(null);
    }
  };

  const handleSynthesize = async (arenaId: string, judge: string) => {
    const client = getHostClient(hostId);
    if (!client) return;
    setBusyKey(`synth:${arenaId}`);
    setError(null);
    try {
      await client.request("arena.synthesize.request", {
        arena_id: arenaId,
        judge,
        launch_agent: true,
      });
      setToast(`Synthesis judge (${judge}) started for ${arenaId}`);
      await refreshHost(hostId, false);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusyKey(null);
    }
  };

  return (
    <Modal visible={visible} transparent animationType="slide" onRequestClose={onClose}>
      <View style={styles.backdrop}>
        <View
          style={[
            styles.sheet,
            {
              backgroundColor: palette.panel,
              borderColor: palette.borderSubtle,
              paddingBottom: Math.max(insets.bottom, 14),
            },
          ]}
        >
          <View style={styles.header}>
            <View style={styles.headerLeft}>
              <Trophy size={18} color={palette.warning} />
              <View>
                <Text style={[styles.title, { color: palette.text }]}>
                  {t("arenaTitle")}
                </Text>
                <Text style={[styles.sub, { color: palette.muted }]}>
                  {t("arenaSubtitle")}
                </Text>
              </View>
            </View>
            <Pressable onPress={onClose} hitSlop={8} style={styles.closeBtn}>
              <X size={19} color={palette.muted} />
            </Pressable>
          </View>

          {error ? (
            <Text style={[styles.msg, { color: palette.danger }]}>{error}</Text>
          ) : null}
          {toast ? (
            <Text style={[styles.msg, { color: palette.success }]}>{toast}</Text>
          ) : null}

          <ScrollView style={styles.scroll} contentContainerStyle={styles.content}>
            {/* Launch New Arena */}
            <View
              style={[
                styles.card,
                { backgroundColor: palette.canvas, borderColor: palette.hairline },
              ]}
            >
              <Text style={[styles.cardTitle, { color: palette.text }]}>
                {t("arenaStartNew")}
              </Text>
              <View style={styles.chips}>
                {RACER_CHOICES.map((r) => (
                  <SelectableChip
                    key={r}
                    label={r}
                    selected={selectedRacers.includes(r)}
                    onPress={() => toggleRacer(r)}
                  />
                ))}
              </View>
              <TextInput
                value={prompt}
                onChangeText={setPrompt}
                placeholder={t("arenaPromptPlaceholder")}
                placeholderTextColor={palette.muted}
                multiline
                style={[
                  styles.input,
                  {
                    backgroundColor: palette.panel,
                    borderColor: palette.hairline,
                    color: palette.text,
                    minHeight: 56,
                  },
                ]}
              />
              <TextInput
                value={verifyCmd}
                onChangeText={setVerifyCmd}
                placeholder={t("arenaVerifyPlaceholder")}
                placeholderTextColor={palette.muted}
                autoCapitalize="none"
                style={[
                  styles.input,
                  {
                    backgroundColor: palette.panel,
                    borderColor: palette.hairline,
                    color: palette.text,
                    fontFamily: FONTS.mono,
                  },
                ]}
              />
              <Pressable
                onPress={() => void handleStartArena()}
                disabled={busyKey === "start" || !prompt.trim()}
                style={[
                  styles.primaryBtn,
                  {
                    backgroundColor: palette.accent,
                    opacity: busyKey === "start" || !prompt.trim() ? 0.5 : 1,
                  },
                ]}
              >
                {busyKey === "start" ? (
                  <ActivityIndicator size="small" color={palette.accentText} />
                ) : (
                  <>
                    <Play size={14} color={palette.accentText} />
                    <Text style={[styles.primaryBtnText, { color: palette.accentText }]}>
                      {t("arenaLaunchBtn")}
                    </Text>
                  </>
                )}
              </Pressable>
            </View>

            {/* Losers disposition preference (D-12) */}
            <View style={styles.chips}>
              <SelectableChip
                label={t("arenaLosersArchive")}
                selected={loserMode === "archive"}
                onPress={() => setLoserMode("archive")}
              />
              <SelectableChip
                label={t("arenaLosersClean")}
                selected={loserMode === "clean"}
                onPress={() => setLoserMode("clean")}
              />
            </View>

            {loading ? (
              <ActivityIndicator color={palette.accent} />
            ) : arenas.length === 0 ? (
              <Text style={[styles.empty, { color: palette.muted }]}>
                {t("arenaEmpty")}
              </Text>
            ) : (
              arenas.map(({ arena, racers }) => {
                const ranked = rankArenaRacers(racers);
                return (
                  <View
                    key={arena.id}
                    style={[
                      styles.card,
                      { backgroundColor: palette.canvas, borderColor: palette.hairline },
                    ]}
                  >
                    <View style={styles.cardTop}>
                      <Text style={[styles.cardTitle, { color: palette.text }]}>
                        {arena.id}
                      </Text>
                      <Text style={[styles.badgeText, { color: palette.accent }]}>
                        {arena.status.toUpperCase()}
                      </Text>
                    </View>
                    <Text
                      numberOfLines={2}
                      style={[styles.sub, { color: palette.textSecondary }]}
                    >
                      {arena.prompt}
                    </Text>
                    <View style={styles.rowActions}>
                      <Pressable
                        onPress={() => void handleVerify(arena.id)}
                        disabled={busyKey === `verify:${arena.id}`}
                        style={[styles.smallBtn, { backgroundColor: palette.raised }]}
                      >
                        <CheckCircle2 size={12} color={palette.accent} />
                        <Text style={[styles.smallBtnText, { color: palette.text }]}>
                          {t("arenaVerifyBtn")}
                        </Text>
                      </Pressable>
                      <Pressable
                        onPress={() => void handleSynthesize(arena.id, "claude")}
                        disabled={busyKey === `synth:${arena.id}`}
                        style={[styles.smallBtn, { backgroundColor: palette.accentSoft }]}
                      >
                        <Sparkles size={12} color={palette.accent} />
                        <Text style={[styles.smallBtnText, { color: palette.accent }]}>
                          {t("arenaSynthesizeBtn")}
                        </Text>
                      </Pressable>
                    </View>

                    {ranked.map((r) => (
                      <View
                        key={r.racer_name}
                        style={[
                          styles.racerRow,
                          { borderColor: palette.hairline, backgroundColor: palette.panel },
                        ]}
                      >
                        <View style={styles.cardTop}>
                          <Text style={[styles.cardTitle, { color: palette.text }]}>
                            {r.racer_name}
                            {arena.winner_racer === r.racer_name ? " 🏆" : ""}
                          </Text>
                          <Text
                            style={[
                              styles.badgeText,
                              {
                                color:
                                  r.verify_exit_code === 0
                                    ? palette.success
                                    : r.verify_exit_code != null
                                      ? palette.danger
                                      : palette.muted,
                              },
                            ]}
                          >
                            {r.verify_exit_code === 0
                              ? "PASS"
                              : r.verify_exit_code != null
                                ? `EXIT ${r.verify_exit_code}`
                                : r.status}
                            {r.audit_verdict ? ` · ${r.audit_verdict}` : ""}
                          </Text>
                        </View>
                        {arena.status !== "promoted" ? (
                          <View style={styles.rowActions}>
                            <Pressable
                              onPress={() =>
                                void handlePromote(arena.id, r.racer_name, "branch")
                              }
                              style={[
                                styles.smallBtn,
                                { backgroundColor: palette.successSoft },
                              ]}
                            >
                              <GitBranch size={12} color={palette.success} />
                              <Text
                                style={[
                                  styles.smallBtnText,
                                  { color: palette.success },
                                ]}
                              >
                                {t("arenaPromoteBranch")}
                              </Text>
                            </Pressable>
                            <Pressable
                              onPress={() =>
                                void handlePromote(arena.id, r.racer_name, "squash")
                              }
                              style={[
                                styles.smallBtn,
                                { backgroundColor: palette.accentSoft },
                              ]}
                            >
                              <Text
                                style={[
                                  styles.smallBtnText,
                                  { color: palette.accent },
                                ]}
                              >
                                {t("arenaPromoteSquash")}
                              </Text>
                            </Pressable>
                          </View>
                        ) : null}
                      </View>
                    ))}
                  </View>
                );
              })
            )}
          </ScrollView>
        </View>
      </View>
    </Modal>
  );
}

const styles = StyleSheet.create({
  backdrop: {
    flex: 1,
    backgroundColor: "rgba(4,7,11,0.72)",
    justifyContent: "flex-end",
  },
  sheet: {
    borderTopLeftRadius: 20,
    borderTopRightRadius: 20,
    borderTopWidth: 1,
    maxHeight: "90%",
    paddingTop: 14,
  },
  header: {
    flexDirection: "row",
    alignItems: "center",
    justifyContent: "space-between",
    paddingHorizontal: 16,
    paddingBottom: 10,
  },
  headerLeft: { flexDirection: "row", alignItems: "center", gap: 10 },
  title: { fontFamily: FONTS.bold, fontSize: 16 },
  sub: { fontFamily: FONTS.regular, fontSize: 11.5 },
  closeBtn: { padding: 6 },
  msg: { paddingHorizontal: 16, paddingBottom: 8, fontSize: 12 },
  scroll: { maxHeight: 580 },
  content: { paddingHorizontal: 16, paddingBottom: 24, gap: 12 },
  card: { borderWidth: 1, borderRadius: 12, padding: 12, gap: 8 },
  cardTop: {
    flexDirection: "row",
    justifyContent: "space-between",
    alignItems: "center",
    gap: 8,
  },
  cardTitle: { fontFamily: FONTS.semi, fontSize: 13.5 },
  badgeText: { fontFamily: FONTS.semi, fontSize: 11 },
  chips: { flexDirection: "row", flexWrap: "wrap", gap: 6 },
  input: {
    borderWidth: 1,
    borderRadius: 9,
    paddingHorizontal: 10,
    paddingVertical: 8,
    fontSize: 13,
  },
  primaryBtn: {
    flexDirection: "row",
    alignItems: "center",
    justifyContent: "center",
    gap: 6,
    paddingVertical: 10,
    borderRadius: 9,
  },
  primaryBtnText: { fontFamily: FONTS.semi, fontSize: 13 },
  rowActions: { flexDirection: "row", flexWrap: "wrap", gap: 8 },
  racerRow: { borderWidth: 1, borderRadius: 9, padding: 10, gap: 6 },
  smallBtn: {
    flexDirection: "row",
    alignItems: "center",
    gap: 5,
    paddingHorizontal: 10,
    paddingVertical: 6,
    borderRadius: 8,
  },
  smallBtnText: { fontFamily: FONTS.semi, fontSize: 11.5 },
  empty: { textAlign: "center", paddingVertical: 20, fontSize: 13 },
});
