import React, { useCallback, useEffect, useMemo, useState } from "react";
import {
  ActivityIndicator,
  Modal,
  Pressable,
  ScrollView,
  StyleSheet,
  Text,
  View,
} from "react-native";
import {
  AlertOctagon,
  AlertTriangle,
  Check,
  ChevronDown,
  ChevronRight,
  MessageSquareWarning,
  RotateCcw,
  ShieldCheck,
  X,
} from "lucide-react-native";
import { useTranslation } from "react-i18next";
import { useSafeAreaInsets } from "react-native-safe-area-context";
import {
  buildHybridTriageStack,
  type HybridTriageCard,
} from "../agents/phase1-helpers";
import { getHostClient } from "../hosts/HostRuntimeController";
import type {
  AgentRow,
  TurnAuditReport,
} from "../protocol/schemas";
import { useMasilTheme } from "../theme/ThemeProvider";
import { FONTS, TABULAR_NUMS } from "../theme/typography";
import { ConfirmModal } from "./ConfirmModal";

interface AuditTriageSheetProps {
  visible: boolean;
  hostId: string;
  agent: AgentRow;
  onClose: () => void;
  onActionDone?: (toast: string) => void;
}

export function AuditTriageSheet({
  visible,
  hostId,
  agent,
  onClose,
  onActionDone,
}: AuditTriageSheetProps) {
  const { palette } = useMasilTheme();
  const { t } = useTranslation();
  const insets = useSafeAreaInsets();

  const [report, setReport] = useState<TurnAuditReport | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [passedIds, setPassedIds] = useState<Record<string, boolean>>({});
  const [showSecondary, setShowSecondary] = useState(false);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [pendingRollback, setPendingRollback] = useState<{
    file: string;
    checkpointId: string;
    previewToken?: string;
    previewSummary: string;
  } | null>(null);

  const loadLatest = useCallback(async () => {
    const client = getHostClient(hostId);
    if (!client) {
      setError(t("hostNotConnected"));
      return;
    }
    setLoading(true);
    setError(null);
    try {
      const res = await client.request<{ audit?: TurnAuditReport | null }>(
        "audit.latest.request",
        {
          server_socket: agent.server_socket,
          pane: agent.pane,
          target: agent.pane || agent.name,
        }
      );
      setReport(res.audit ?? null);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }, [hostId, agent.server_socket, agent.pane, agent.name, t]);

  useEffect(() => {
    if (!visible) return;
    setPassedIds({});
    setShowSecondary(false);
    void loadLatest();
  }, [visible, loadLatest]);

  const stack = useMemo(() => buildHybridTriageStack(report), [report]);
  const rollbackCheckpoint =
    report?.prev_checkpoint_id || report?.checkpoint_id || null;

  const challengeCard = async (card: HybridTriageCard) => {
    const client = getHostClient(hostId);
    if (!client) return;
    setBusyId(card.id);
    setError(null);
    try {
      const findingId =
        card.kind === "finding"
          ? card.finding.id || card.finding.rule_id
          : undefined;
      await client.request("audit.challenge.request", {
        server_socket: agent.server_socket,
        pane: agent.pane,
        target: agent.pane || agent.name,
        finding_id: findingId,
      });
      setPassedIds((prev) => ({ ...prev, [card.id]: true }));
      onActionDone?.(t("auditChallengeSent"));
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusyId(null);
    }
  };

  const prepareRollback = async (file: string, cardId: string) => {
    if (!rollbackCheckpoint) {
      setError(t("auditNoCheckpoint"));
      return;
    }
    const client = getHostClient(hostId);
    if (!client) return;
    setBusyId(cardId);
    setError(null);
    try {
      const preview = await client.request<{
        token?: string;
        files?: Array<{ action?: string; path?: string }>;
      }>("checkpoint.preview.request", {
        server_socket: agent.server_socket,
        pane: agent.pane,
        target: agent.pane || agent.name,
        checkpoint_id: rollbackCheckpoint,
        paths: [file],
      });
      const action = preview.files?.[0]?.action ?? "restore";
      setPendingRollback({
        file,
        checkpointId: rollbackCheckpoint,
        previewToken: preview.token,
        previewSummary: t("auditRollbackConfirmBody", {
          file,
          checkpoint: rollbackCheckpoint,
          action,
        }),
      });
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusyId(null);
    }
  };

  const executeRollback = async () => {
    if (!pendingRollback) return;
    const req = pendingRollback;
    setPendingRollback(null);
    const client = getHostClient(hostId);
    if (!client) return;
    setBusyId(`rb:${req.file}`);
    setError(null);
    try {
      await client.request("audit.rollback_file.request", {
        server_socket: agent.server_socket,
        pane: agent.pane,
        target: agent.pane || agent.name,
        path: req.file,
        checkpoint_id: req.checkpointId,
        confirm_token: req.previewToken,
      });
      onActionDone?.(t("auditRollbackDone", { file: req.file }));
      await loadLatest();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusyId(null);
    }
  };

  const verdictTone =
    report?.verdict === "flagged"
      ? { color: palette.danger, bg: palette.dangerSoft }
      : report?.verdict === "warnings" || report?.verdict === "caution"
        ? { color: palette.warning, bg: palette.warningSoft }
        : { color: palette.success, bg: palette.successSoft };

  return (
    <Modal
      visible={visible}
      transparent
      animationType="slide"
      onRequestClose={onClose}
    >
      <View style={styles.backdrop}>
        <View
          style={[
            styles.sheet,
            {
              backgroundColor: palette.panel,
              borderColor: palette.borderSubtle,
              paddingBottom: Math.max(insets.bottom, 16) + 8,
            },
          ]}
        >
          <View style={[styles.grabBar, { backgroundColor: palette.track }]} />

          <View style={styles.header}>
            <View style={{ flex: 1, gap: 4 }}>
              <View style={styles.titleRow}>
                <ShieldCheck size={16} color={palette.accent} />
                <Text style={[styles.title, { color: palette.text }]}>
                  {t("auditTitle")}
                </Text>
                {report ? (
                  <View
                    style={[
                      styles.verdictPill,
                      { backgroundColor: verdictTone.bg },
                    ]}
                  >
                    <Text
                      style={[styles.verdictText, { color: verdictTone.color }]}
                    >
                      {report.verdict.toUpperCase()} · {report.risk_score}/100
                    </Text>
                  </View>
                ) : null}
              </View>
              <Text style={[styles.subtitle, { color: palette.muted }]}>
                {agent.name || agent.pane}
                {report?.claim_verified === false
                  ? ` · ${t("auditClaimUnverified")}`
                  : report?.claim_verified === true
                    ? ` · ${t("auditClaimVerified")}`
                    : ""}
              </Text>
            </View>
            <Pressable onPress={onClose} hitSlop={8} style={styles.closeBtn}>
              <X size={19} color={palette.muted} />
            </Pressable>
          </View>

          <View
            style={[styles.divider, { backgroundColor: palette.hairline }]}
          />

          <ScrollView contentContainerStyle={styles.body}>
            {loading ? (
              <View style={styles.centerBox}>
                <ActivityIndicator size="small" color={palette.accent} />
              </View>
            ) : error ? (
              <Text style={[styles.errorText, { color: palette.danger }]}>
                {error}
              </Text>
            ) : !report ||
              (stack.primaryCards.length === 0 &&
                stack.secondaryFindings.length === 0) ? (
              <View style={styles.centerBox}>
                <Text style={[styles.emptyText, { color: palette.muted }]}>
                  {t("auditCleanEmpty")}
                </Text>
              </View>
            ) : (
              <>
                {stack.primaryCards.map((card) => {
                  const isPassed = Boolean(passedIds[card.id]);
                  const isBusy = busyId === card.id;
                  const file =
                    card.kind === "finding"
                      ? card.finding.file ?? null
                      : card.hunk.file;
                  const isCrit =
                    card.kind === "finding" &&
                    card.finding.severity.toLowerCase() === "critical";
                  const borderColor = isPassed
                    ? palette.hairline
                    : card.kind === "finding"
                      ? isCrit
                        ? palette.danger
                        : palette.warning
                      : palette.accentBorder;

                  return (
                    <View
                      key={card.id}
                      style={[
                        styles.card,
                        {
                          backgroundColor: palette.canvas,
                          borderColor,
                          opacity: isPassed ? 0.55 : 1,
                        },
                      ]}
                    >
                      {card.kind === "finding" ? (
                        <>
                          <View style={styles.cardHeader}>
                            {isCrit ? (
                              <AlertOctagon size={14} color={palette.danger} />
                            ) : (
                              <AlertTriangle
                                size={14}
                                color={palette.warning}
                              />
                            )}
                            <Text
                              style={[
                                styles.ruleBadge,
                                {
                                  color: isCrit
                                    ? palette.danger
                                    : palette.warning,
                                },
                              ]}
                            >
                              {card.finding.severity.toUpperCase()} ·{" "}
                              {card.finding.rule_id}
                            </Text>
                            {file ? (
                              <Text
                                numberOfLines={1}
                                style={[
                                  styles.fileTag,
                                  { color: palette.textSecondary },
                                ]}
                              >
                                {file}
                                {card.finding.line ? `:${card.finding.line}` : ""}
                              </Text>
                            ) : null}
                          </View>
                          <Text
                            style={[styles.cardTitle, { color: palette.text }]}
                          >
                            {card.finding.title}
                          </Text>
                          <Text
                            style={[
                              styles.cardDetail,
                              { color: palette.textSecondary },
                            ]}
                          >
                            {card.finding.detail}
                          </Text>
                          {card.finding.evidence ? (
                            <Text
                              style={[
                                styles.codeSnippet,
                                {
                                  backgroundColor: palette.panel,
                                  color: palette.text,
                                },
                              ]}
                            >
                              {card.finding.evidence}
                            </Text>
                          ) : null}
                        </>
                      ) : (
                        <>
                          <View style={styles.cardHeader}>
                            <Text
                              style={[
                                styles.ruleBadge,
                                { color: palette.accent },
                              ]}
                            >
                              RISK {card.hunk.risk_score}
                            </Text>
                            <Text
                              numberOfLines={1}
                              style={[
                                styles.fileTag,
                                { color: palette.textSecondary },
                              ]}
                            >
                              {card.hunk.file} (+{card.hunk.added} -
                              {card.hunk.removed})
                            </Text>
                          </View>
                          {card.hunk.reason ? (
                            <Text
                              style={[
                                styles.cardDetail,
                                { color: palette.muted },
                              ]}
                            >
                              {card.hunk.reason}
                            </Text>
                          ) : null}
                          <Text
                            numberOfLines={8}
                            style={[
                              styles.codeSnippet,
                              {
                                backgroundColor: palette.panel,
                                color: palette.text,
                              },
                            ]}
                          >
                            {card.hunk.diff}
                          </Text>
                        </>
                      )}

                      <View style={styles.actionRow}>
                        <Pressable
                          disabled={isBusy}
                          onPress={() =>
                            setPassedIds((prev) => ({
                              ...prev,
                              [card.id]: !prev[card.id],
                            }))
                          }
                          style={[
                            styles.actionBtn,
                            {
                              backgroundColor: isPassed
                                ? palette.successSoft
                                : palette.raised,
                              borderColor: palette.hairline,
                            },
                          ]}
                        >
                          <Check size={13} color={palette.success} />
                          <Text
                            style={[
                              styles.actionBtnText,
                              { color: palette.text },
                            ]}
                          >
                            {t("auditPass")}
                          </Text>
                        </Pressable>

                        <Pressable
                          disabled={isBusy}
                          onPress={() => void challengeCard(card)}
                          style={[
                            styles.actionBtn,
                            {
                              backgroundColor: palette.warningSoft,
                              borderColor: palette.warning,
                            },
                          ]}
                        >
                          <MessageSquareWarning
                            size={13}
                            color={palette.warning}
                          />
                          <Text
                            style={[
                              styles.actionBtnText,
                              { color: palette.warning },
                            ]}
                          >
                            {t("auditChallenge")}
                          </Text>
                        </Pressable>

                        {file && rollbackCheckpoint ? (
                          <Pressable
                            disabled={isBusy}
                            onPress={() => void prepareRollback(file, card.id)}
                            style={[
                              styles.actionBtn,
                              {
                                backgroundColor: palette.dangerSoft,
                                borderColor: palette.danger,
                              },
                            ]}
                          >
                            <RotateCcw size={13} color={palette.danger} />
                            <Text
                              style={[
                                styles.actionBtnText,
                                { color: palette.danger },
                              ]}
                            >
                              {t("auditRollbackFile")}
                            </Text>
                          </Pressable>
                        ) : null}
                      </View>
                    </View>
                  );
                })}

                {stack.secondaryFindings.length > 0 ? (
                  <View
                    style={[
                      styles.drawer,
                      {
                        backgroundColor: palette.canvas,
                        borderColor: palette.hairline,
                      },
                    ]}
                  >
                    <Pressable
                      onPress={() => setShowSecondary((v) => !v)}
                      style={styles.drawerHeader}
                    >
                      {showSecondary ? (
                        <ChevronDown size={14} color={palette.muted} />
                      ) : (
                        <ChevronRight size={14} color={palette.muted} />
                      )}
                      <Text
                        style={[
                          styles.drawerTitle,
                          { color: palette.textSecondary },
                        ]}
                      >
                        {t("auditMoreFindings", {
                          count: stack.secondaryFindings.length,
                        })}
                      </Text>
                    </Pressable>
                    {showSecondary
                      ? stack.secondaryFindings.map((f, idx) => (
                          <View
                            key={`${f.rule_id}-${idx}`}
                            style={[
                              styles.secondaryRow,
                              { borderTopColor: palette.hairline },
                            ]}
                          >
                            <Text
                              style={[
                                styles.ruleBadge,
                                { color: palette.muted },
                              ]}
                            >
                              {f.severity.toUpperCase()} · {f.rule_id}
                              {f.file ? ` · ${f.file}` : ""}
                            </Text>
                            <Text
                              style={[
                                styles.cardDetail,
                                { color: palette.text },
                              ]}
                            >
                              {f.title} — {f.detail}
                            </Text>
                          </View>
                        ))
                      : null}
                  </View>
                ) : null}
              </>
            )}
          </ScrollView>
        </View>
      </View>

      <ConfirmModal
        visible={Boolean(pendingRollback)}
        title={t("auditRollbackConfirmTitle")}
        body={pendingRollback?.previewSummary ?? ""}
        destructive
        onCancel={() => setPendingRollback(null)}
        onConfirm={() => void executeRollback()}
      />
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
    borderLeftWidth: 1,
    borderRightWidth: 1,
    paddingHorizontal: 16,
    paddingTop: 10,
    maxHeight: "88%",
  },
  grabBar: {
    width: 36,
    height: 4,
    borderRadius: 2,
    alignSelf: "center",
    marginBottom: 12,
  },
  header: {
    flexDirection: "row",
    alignItems: "flex-start",
    gap: 10,
    marginBottom: 10,
  },
  titleRow: {
    flexDirection: "row",
    alignItems: "center",
    flexWrap: "wrap",
    gap: 7,
  },
  title: {
    fontFamily: FONTS.bold,
    fontSize: 16,
  },
  verdictPill: {
    paddingHorizontal: 7,
    paddingVertical: 2,
    borderRadius: 999,
  },
  verdictText: {
    fontFamily: FONTS.bold,
    fontVariant: TABULAR_NUMS,
    fontSize: 10.5,
  },
  subtitle: {
    fontFamily: FONTS.regular,
    fontSize: 12,
  },
  closeBtn: {
    width: 32,
    height: 32,
    alignItems: "center",
    justifyContent: "center",
  },
  divider: {
    height: 1,
    marginHorizontal: -16,
    marginBottom: 12,
  },
  body: {
    gap: 10,
    paddingBottom: 18,
  },
  centerBox: {
    paddingVertical: 28,
    alignItems: "center",
  },
  emptyText: {
    fontFamily: FONTS.regular,
    fontSize: 13.5,
    textAlign: "center",
  },
  errorText: {
    fontFamily: FONTS.semi,
    fontSize: 12.5,
  },
  card: {
    borderWidth: 1,
    borderRadius: 12,
    padding: 12,
    gap: 7,
  },
  cardHeader: {
    flexDirection: "row",
    alignItems: "center",
    gap: 6,
  },
  ruleBadge: {
    fontFamily: FONTS.bold,
    fontVariant: TABULAR_NUMS,
    fontSize: 11,
  },
  fileTag: {
    fontFamily: FONTS.mono,
    fontSize: 11,
    flex: 1,
    textAlign: "right",
  },
  cardTitle: {
    fontFamily: FONTS.bold,
    fontSize: 13.5,
  },
  cardDetail: {
    fontFamily: FONTS.regular,
    fontSize: 12.5,
    lineHeight: 18,
  },
  codeSnippet: {
    fontFamily: FONTS.mono,
    fontSize: 11,
    lineHeight: 16,
    padding: 8,
    borderRadius: 8,
  },
  actionRow: {
    flexDirection: "row",
    flexWrap: "wrap",
    gap: 8,
    marginTop: 4,
  },
  actionBtn: {
    flexDirection: "row",
    alignItems: "center",
    gap: 5,
    borderWidth: 1,
    borderRadius: 8,
    paddingHorizontal: 10,
    paddingVertical: 6,
  },
  actionBtnText: {
    fontFamily: FONTS.semi,
    fontSize: 12,
  },
  drawer: {
    borderWidth: 1,
    borderRadius: 10,
    overflow: "hidden",
  },
  drawerHeader: {
    flexDirection: "row",
    alignItems: "center",
    gap: 6,
    padding: 11,
  },
  drawerTitle: {
    fontFamily: FONTS.semi,
    fontSize: 12.5,
  },
  secondaryRow: {
    borderTopWidth: 1,
    padding: 10,
    gap: 4,
  },
});
