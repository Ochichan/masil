import React, { useCallback, useEffect, useState } from "react";
import {
  ActivityIndicator,
  Pressable,
  ScrollView,
  StyleSheet,
  Text,
  View,
} from "react-native";
import {
  Camera,
  ChevronLeft,
  ChevronRight,
  FileCode2,
  GitFork,
  Pin,
  PinOff,
  RotateCcw,
} from "lucide-react-native";
import { useTranslation } from "react-i18next";
import { clampTurnIndex, countPinnedTurns } from "../agents/phase3-helpers";
import { getHostClient } from "../hosts/HostRuntimeController";
import type { AgentRow, DvrInspectResult, DvrTurnRecord } from "../protocol/schemas";
import { useMasilTheme } from "../theme/ThemeProvider";
import { FONTS } from "../theme/typography";

interface DvrScrubberViewProps {
  hostId: string;
  agent: AgentRow;
}

/**
 * D-12 Time-Travel DVR Scrubber embedded inside Workspace Tools:
 * Navigate recorded turns (Turn 1 .. Turn N), inspect diffs/audit/terminal
 * excerpts, pin/unpin milestones (permanently exempt from pruning), restore
 * full working tree (including undoing Bash side effects), fork to a new
 * worktree, or export a zero-CDN secret-redacted HTML PR trace.
 */
export function DvrScrubberView({ hostId, agent }: DvrScrubberViewProps) {
  const { palette } = useMasilTheme();
  const { t } = useTranslation();

  const [turns, setTurns] = useState<DvrTurnRecord[]>([]);
  const [selectedIdx, setSelectedIdx] = useState(0);
  const [detail, setDetail] = useState<DvrInspectResult | null>(null);
  const [loading, setLoading] = useState(false);
  const [busyAction, setBusyAction] = useState<string | null>(null);
  const [statusMsg, setStatusMsg] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const runId = agent.run ?? "";

  const loadTurns = useCallback(async () => {
    const client = getHostClient(hostId);
    if (!client || !runId) return;
    setLoading(true);
    setError(null);
    try {
      const res = await client.request<{ turns?: DvrTurnRecord[] }>("dvr.list.request", {
        server_socket: agent.server_socket,
        run_id: runId,
        limit: 100,
      });
      const list = res.turns ?? [];
      setTurns(list);
      setSelectedIdx(Math.max(0, list.length - 1));
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }, [hostId, agent.server_socket, runId]);

  useEffect(() => {
    void loadTurns();
  }, [loadTurns]);

  const activeTurn = turns[clampTurnIndex(selectedIdx, turns.length)] ?? null;

  useEffect(() => {
    const client = getHostClient(hostId);
    if (!client || !runId || !activeTurn) {
      setDetail(null);
      return;
    }
    let cancelled = false;
    void client
      .request<DvrInspectResult>("dvr.inspect.request", {
        server_socket: agent.server_socket,
        run_id: runId,
        cwd: agent.cwd,
        turn_seq: activeTurn.turn_seq,
      })
      .then((res) => {
        if (!cancelled) setDetail(res);
      })
      .catch(() => {
        if (!cancelled) setDetail(null);
      });
    return () => {
      cancelled = true;
    };
  }, [hostId, agent.server_socket, agent.cwd, runId, activeTurn?.turn_seq]);

  const handleCapture = async () => {
    const client = getHostClient(hostId);
    if (!client || !runId) return;
    setBusyAction("capture");
    setError(null);
    try {
      await client.request("dvr.snapshot.request", {
        server_socket: agent.server_socket,
        pane: agent.pane,
        run_id: runId,
        agent_name: agent.name,
        provider: agent.provider,
        cwd: agent.cwd,
      });
      await loadTurns();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusyAction(null);
    }
  };

  const handleTogglePin = async () => {
    const client = getHostClient(hostId);
    if (!client || !activeTurn) return;
    setBusyAction("pin");
    setError(null);
    try {
      await client.request("dvr.pin.request", {
        server_socket: agent.server_socket,
        run_id: runId,
        cwd: agent.cwd,
        turn_seq: activeTurn.turn_seq,
        pinned: !activeTurn.pinned,
      });
      await loadTurns();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusyAction(null);
    }
  };

  const handleRestore = async () => {
    const client = getHostClient(hostId);
    if (!client || !activeTurn) return;
    setBusyAction("restore");
    setError(null);
    try {
      const res = await client.request<{ safety_checkpoint_id?: number }>(
        "dvr.restore.request",
        {
          server_socket: agent.server_socket,
          run_id: runId,
          cwd: agent.cwd,
          turn_seq: activeTurn.turn_seq,
        }
      );
      setStatusMsg(
        `Restored Turn #${activeTurn.turn_seq} (safety ckpt #${res.safety_checkpoint_id ?? "?"})`
      );
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusyAction(null);
    }
  };

  const handleFork = async () => {
    const client = getHostClient(hostId);
    if (!client || !activeTurn) return;
    setBusyAction("fork");
    setError(null);
    try {
      const res = await client.request<{ branch?: string; worktree_path?: string }>(
        "dvr.fork.request",
        {
          server_socket: agent.server_socket,
          run_id: runId,
          cwd: agent.cwd,
          turn_seq: activeTurn.turn_seq,
        }
      );
      setStatusMsg(`Forked Turn #${activeTurn.turn_seq} → ${res.branch ?? "worktree"}`);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusyAction(null);
    }
  };

  const handleExportTrace = async () => {
    const client = getHostClient(hostId);
    if (!client || !runId) return;
    setBusyAction("export");
    setError(null);
    try {
      const res = await client.request<{
        turn_count: number;
        total_redactions: number;
      }>("dvr.export_trace.request", {
        server_socket: agent.server_socket,
        run_id: runId,
        cwd: agent.cwd,
        agent_name: agent.name,
        provider: agent.provider,
        redact_strict: true,
      });
      setStatusMsg(
        t("dvrTraceExported", {
          turns: res.turn_count,
          redactions: res.total_redactions,
        })
      );
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusyAction(null);
    }
  };

  return (
    <ScrollView style={styles.root} contentContainerStyle={styles.content}>
      <View style={styles.topRow}>
        <View style={{ flex: 1 }}>
          <Text style={[styles.title, { color: palette.text }]}>{t("dvrTitle")}</Text>
          <Text style={[styles.sub, { color: palette.muted }]}>
            {t("dvrSubtitle")} · {turns.length} ({countPinnedTurns(turns)} pinned)
          </Text>
        </View>
        <Pressable
          onPress={() => void handleCapture()}
          disabled={busyAction === "capture"}
          style={[styles.chipBtn, { backgroundColor: palette.accentSoft }]}
        >
          <Camera size={13} color={palette.accent} />
          <Text style={[styles.chipBtnText, { color: palette.accent }]}>
            {t("dvrSnapshotBtn")}
          </Text>
        </Pressable>
      </View>

      {error ? (
        <Text style={[styles.msg, { color: palette.danger }]}>{error}</Text>
      ) : null}
      {statusMsg ? (
        <Text style={[styles.msg, { color: palette.success }]}>{statusMsg}</Text>
      ) : null}

      {loading ? (
        <ActivityIndicator color={palette.accent} style={{ marginTop: 24 }} />
      ) : !activeTurn ? (
        <Text style={[styles.empty, { color: palette.muted }]}>{t("dvrEmpty")}</Text>
      ) : (
        <>
          {/* Turn Scrubber Bar */}
          <View
            style={[
              styles.card,
              { backgroundColor: palette.panel, borderColor: palette.hairline },
            ]}
          >
            <View style={styles.scrubberHeader}>
              <Pressable
                onPress={() =>
                  setSelectedIdx((i) => clampTurnIndex(i - 1, turns.length))
                }
                disabled={selectedIdx <= 0}
                style={[styles.navBtn, { backgroundColor: palette.raised }]}
              >
                <ChevronLeft size={16} color={palette.text} />
              </Pressable>
              <Text style={[styles.turnTitle, { color: palette.text }]}>
                Turn #{activeTurn.turn_seq} ({selectedIdx + 1}/{turns.length})
              </Text>
              <Pressable
                onPress={() =>
                  setSelectedIdx((i) => clampTurnIndex(i + 1, turns.length))
                }
                disabled={selectedIdx >= turns.length - 1}
                style={[styles.navBtn, { backgroundColor: palette.raised }]}
              >
                <ChevronRight size={16} color={palette.text} />
              </Pressable>
            </View>

            {/* Visual step rail */}
            <ScrollView horizontal showsHorizontalScrollIndicator={false} style={{ marginVertical: 4 }}>
              <View style={styles.rail}>
                {turns.map((turn, idx) => {
                  const active = idx === selectedIdx;
                  return (
                    <Pressable
                      key={turn.turn_seq}
                      onPress={() => setSelectedIdx(idx)}
                      style={[
                        styles.dotStep,
                        {
                          backgroundColor: active
                            ? palette.accent
                            : turn.pinned
                              ? palette.warningSoft
                              : palette.raised,
                          borderColor: turn.pinned ? palette.warning : palette.hairline,
                        },
                      ]}
                    >
                      <Text
                        style={[
                          styles.dotLabel,
                          { color: active ? palette.accentText : palette.text },
                        ]}
                      >
                        {turn.pinned ? "★" : ""}
                        {turn.turn_seq}
                      </Text>
                    </Pressable>
                  );
                })}
              </View>
            </ScrollView>

            <View style={styles.actionsRow}>
              <Pressable
                onPress={() => void handleTogglePin()}
                disabled={busyAction === "pin"}
                style={[styles.chipBtn, { backgroundColor: palette.raised }]}
              >
                {activeTurn.pinned ? (
                  <PinOff size={13} color={palette.warning} />
                ) : (
                  <Pin size={13} color={palette.accent} />
                )}
                <Text style={[styles.chipBtnText, { color: palette.text }]}>
                  {activeTurn.pinned ? t("dvrUnpinBtn") : t("dvrPinBtn")}
                </Text>
              </Pressable>
              <Pressable
                onPress={() => void handleRestore()}
                disabled={busyAction === "restore"}
                style={[styles.chipBtn, { backgroundColor: palette.warningSoft }]}
              >
                <RotateCcw size={13} color={palette.warning} />
                <Text style={[styles.chipBtnText, { color: palette.warning }]}>
                  {t("dvrRestoreBtn")}
                </Text>
              </Pressable>
              <Pressable
                onPress={() => void handleFork()}
                disabled={busyAction === "fork"}
                style={[styles.chipBtn, { backgroundColor: palette.accentSoft }]}
              >
                <GitFork size={13} color={palette.accent} />
                <Text style={[styles.chipBtnText, { color: palette.accent }]}>
                  {t("dvrForkBtn")}
                </Text>
              </Pressable>
              <Pressable
                onPress={() => void handleExportTrace()}
                disabled={busyAction === "export"}
                style={[styles.chipBtn, { backgroundColor: palette.successSoft }]}
              >
                <FileCode2 size={13} color={palette.success} />
                <Text style={[styles.chipBtnText, { color: palette.success }]}>
                  {t("dvrExportTraceBtn")}
                </Text>
              </Pressable>
            </View>
          </View>

          {/* Diff & Evidence Card */}
          <View
            style={[
              styles.card,
              { backgroundColor: palette.panel, borderColor: palette.hairline },
            ]}
          >
            <Text style={[styles.sub, { color: palette.muted }]}>
              ckpt #{activeTurn.checkpoint_id} · {activeTurn.tree_oid.slice(0, 10)} ·{" "}
              {(activeTurn.audit_verdict ?? "verified").toUpperCase()}
            </Text>
            {activeTurn.prompt_preview ? (
              <Text style={[styles.promptText, { color: palette.text }]}>
                {activeTurn.prompt_preview}
              </Text>
            ) : null}
            <Text
              style={[
                styles.codeBox,
                {
                  backgroundColor: palette.canvas,
                  color: palette.textSecondary,
                  borderColor: palette.hairline,
                },
              ]}
            >
              {detail?.diff?.trim() || "(no file diff in this turn)"}
            </Text>
          </View>
        </>
      )}
    </ScrollView>
  );
}

const styles = StyleSheet.create({
  root: { flex: 1 },
  content: { padding: 14, gap: 12 },
  topRow: { flexDirection: "row", alignItems: "center", gap: 10 },
  title: { fontFamily: FONTS.bold, fontSize: 15.5 },
  sub: { fontFamily: FONTS.regular, fontSize: 11.5 },
  msg: { fontSize: 12 },
  empty: { textAlign: "center", paddingVertical: 32, fontSize: 13 },
  card: { borderWidth: 1, borderRadius: 12, padding: 12, gap: 10 },
  scrubberHeader: {
    flexDirection: "row",
    alignItems: "center",
    justifyContent: "space-between",
  },
  navBtn: {
    width: 32,
    height: 32,
    borderRadius: 8,
    alignItems: "center",
    justifyContent: "center",
  },
  turnTitle: { fontFamily: FONTS.semi, fontSize: 14 },
  rail: { flexDirection: "row", gap: 6, alignItems: "center" },
  dotStep: {
    minWidth: 32,
    height: 28,
    paddingHorizontal: 8,
    borderRadius: 999,
    borderWidth: 1,
    alignItems: "center",
    justifyContent: "center",
  },
  dotLabel: { fontFamily: FONTS.semi, fontSize: 11.5 },
  actionsRow: { flexDirection: "row", flexWrap: "wrap", gap: 7 },
  chipBtn: {
    flexDirection: "row",
    alignItems: "center",
    gap: 5,
    paddingHorizontal: 10,
    paddingVertical: 7,
    borderRadius: 8,
  },
  chipBtnText: { fontFamily: FONTS.semi, fontSize: 11.5 },
  promptText: { fontFamily: FONTS.regular, fontSize: 13 },
  codeBox: {
    fontFamily: FONTS.mono,
    fontSize: 11.5,
    padding: 10,
    borderRadius: 8,
    borderWidth: 1,
  },
});
