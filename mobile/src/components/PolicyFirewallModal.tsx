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
import { Plus, Shield, ShieldOff, Trash2, X } from "lucide-react-native";
import { useTranslation } from "react-i18next";
import { useSafeAreaInsets } from "react-native-safe-area-context";
import { getHostClient } from "../hosts/HostRuntimeController";
import type {
  PermissionAuditEntry,
  PermissionPolicyRule,
} from "../protocol/schemas";
import { useMasilTheme } from "../theme/ThemeProvider";
import { FONTS } from "../theme/typography";
import { SegmentedTabs } from "./SegmentedTabs";

interface PolicyFirewallModalProps {
  visible: boolean;
  hostId: string;
  onClose: () => void;
}

export function PolicyFirewallModal({
  visible,
  hostId,
  onClose,
}: PolicyFirewallModalProps) {
  const { palette } = useMasilTheme();
  const { t } = useTranslation();
  const insets = useSafeAreaInsets();
  const [tab, setTab] = useState<"rules" | "audit">("rules");
  const [rules, setRules] = useState<PermissionPolicyRule[]>([]);
  const [auditEntries, setAuditEntries] = useState<PermissionAuditEntry[]>([]);
  const [loading, setLoading] = useState(false);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const [ruleId, setRuleId] = useState("");
  const [cmdPattern, setCmdPattern] = useState("");

  const loadAll = useCallback(async () => {
    const client = getHostClient(hostId);
    if (!client) return;
    setLoading(true);
    setError(null);
    try {
      const [rulesRes, auditRes] = await Promise.all([
        client.request<{ rules?: PermissionPolicyRule[] }>("policy.list.request", {}),
        client.request<{ entries?: PermissionAuditEntry[] }>("policy.audit_log.request", {
          limit: 40,
        }),
      ]);
      setRules(rulesRes.rules ?? []);
      setAuditEntries(auditRes.entries ?? []);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }, [hostId]);

  useEffect(() => {
    if (visible) {
      void loadAll();
    }
  }, [visible, loadAll]);

  const handleAddRule = async () => {
    const client = getHostClient(hostId);
    if (!client || !ruleId.trim() || !cmdPattern.trim()) return;
    setBusyId("add");
    setError(null);
    try {
      await client.request("policy.upsert.request", {
        id: ruleId.trim(),
        scope: "global",
        match_command: [cmdPattern.trim()],
        max_risk: "low",
        action: "auto_approve",
        enabled: true,
      });
      setRuleId("");
      setCmdPattern("");
      await loadAll();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusyId(null);
    }
  };

  const handleRevoke = async (id: string) => {
    const client = getHostClient(hostId);
    if (!client) return;
    setBusyId(id);
    setError(null);
    try {
      await client.request("policy.revoke.request", { rule_id: id });
      await loadAll();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusyId(null);
    }
  };

  const handleRemove = async (id: string) => {
    const client = getHostClient(hostId);
    if (!client) return;
    setBusyId(id);
    setError(null);
    try {
      await client.request("policy.remove.request", { rule_id: id });
      await loadAll();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusyId(null);
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
              <Shield size={18} color={palette.accent} />
              <View>
                <Text style={[styles.title, { color: palette.text }]}>
                  {t("policyFirewallTitle")}
                </Text>
                <Text style={[styles.sub, { color: palette.muted }]}>
                  {t("policyFirewallSubtitle")}
                </Text>
              </View>
            </View>
            <Pressable onPress={onClose} hitSlop={8} style={styles.closeBtn}>
              <X size={19} color={palette.muted} />
            </Pressable>
          </View>

          <View style={styles.tabWrap}>
            <SegmentedTabs
              options={[
                { id: "rules", label: `${t("policyRulesTab")} (${rules.length})` },
                { id: "audit", label: `${t("policyAuditTab")} (${auditEntries.length})` },
              ]}
              value={tab}
              onChange={(v) => setTab(v === "audit" ? "audit" : "rules")}
            />
          </View>

          {error ? (
            <Text style={[styles.error, { color: palette.danger }]}>{error}</Text>
          ) : null}

          {loading ? (
            <View style={styles.center}>
              <ActivityIndicator color={palette.accent} />
            </View>
          ) : tab === "rules" ? (
            <ScrollView style={styles.scroll} contentContainerStyle={styles.content}>
              <View
                style={[
                  styles.addCard,
                  { backgroundColor: palette.canvas, borderColor: palette.hairline },
                ]}
              >
                <Text style={[styles.sectionLabel, { color: palette.text }]}>
                  {t("policyAddRule")}
                </Text>
                <TextInput
                  value={ruleId}
                  onChangeText={setRuleId}
                  placeholder={t("policyRuleIdPlaceholder")}
                  placeholderTextColor={palette.muted}
                  autoCapitalize="none"
                  style={[
                    styles.input,
                    {
                      backgroundColor: palette.panel,
                      borderColor: palette.hairline,
                      color: palette.text,
                    },
                  ]}
                />
                <TextInput
                  value={cmdPattern}
                  onChangeText={setCmdPattern}
                  placeholder={t("policyCommandPlaceholder")}
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
                  onPress={() => void handleAddRule()}
                  disabled={busyId === "add" || !ruleId.trim() || !cmdPattern.trim()}
                  style={[
                    styles.addBtn,
                    {
                      backgroundColor: palette.accent,
                      opacity:
                        busyId === "add" || !ruleId.trim() || !cmdPattern.trim()
                          ? 0.5
                          : 1,
                    },
                  ]}
                >
                  <Plus size={14} color={palette.accentText} />
                  <Text style={[styles.addBtnText, { color: palette.accentText }]}>
                    {t("policyAddRule")}
                  </Text>
                </Pressable>
              </View>

              {rules.length === 0 ? (
                <Text style={[styles.empty, { color: palette.muted }]}>
                  {t("policyEmptyRules")}
                </Text>
              ) : (
                rules.map((r) => (
                  <View
                    key={r.id}
                    style={[
                      styles.card,
                      {
                        backgroundColor: palette.canvas,
                        borderColor: palette.hairline,
                        opacity: r.enabled ? 1 : 0.65,
                      },
                    ]}
                  >
                    <View style={styles.cardTop}>
                      <Text style={[styles.cardTitle, { color: palette.text }]}>
                        {r.id}
                      </Text>
                      <View
                        style={[
                          styles.badge,
                          {
                            backgroundColor: !r.enabled
                              ? palette.raised
                              : r.action === "deny"
                                ? palette.dangerSoft
                                : palette.successSoft,
                          },
                        ]}
                      >
                        <Text
                          style={[
                            styles.badgeText,
                            {
                              color: !r.enabled
                                ? palette.muted
                                : r.action === "deny"
                                  ? palette.danger
                                  : palette.success,
                            },
                          ]}
                        >
                          {r.enabled ? r.action : t("policyRevokedTag")}
                        </Text>
                      </View>
                    </View>
                    <Text style={[styles.monoLine, { color: palette.textSecondary }]}>
                      {[...(r.match_command ?? []), ...(r.match_path ?? [])].join(", ")}
                    </Text>
                    <View style={styles.cardActions}>
                      {r.enabled ? (
                        <Pressable
                          onPress={() => void handleRevoke(r.id)}
                          disabled={busyId === r.id}
                          style={[
                            styles.smallBtn,
                            { backgroundColor: palette.dangerSoft },
                          ]}
                        >
                          <ShieldOff size={12} color={palette.danger} />
                          <Text
                            style={[styles.smallBtnText, { color: palette.danger }]}
                          >
                            {t("policyRevokeRule")}
                          </Text>
                        </Pressable>
                      ) : null}
                      <Pressable
                        onPress={() => void handleRemove(r.id)}
                        disabled={busyId === r.id}
                        style={[styles.smallBtn, { backgroundColor: palette.raised }]}
                      >
                        <Trash2 size={12} color={palette.muted} />
                      </Pressable>
                    </View>
                  </View>
                ))
              )}
            </ScrollView>
          ) : (
            <ScrollView style={styles.scroll} contentContainerStyle={styles.content}>
              {auditEntries.length === 0 ? (
                <Text style={[styles.empty, { color: palette.muted }]}>
                  {t("policyEmptyAudit")}
                </Text>
              ) : (
                auditEntries.map((entry) => (
                  <View
                    key={entry.id}
                    style={[
                      styles.card,
                      { backgroundColor: palette.canvas, borderColor: palette.hairline },
                    ]}
                  >
                    <View style={styles.cardTop}>
                      <Text style={[styles.cardTitle, { color: palette.text }]}>
                        {entry.decision} · {entry.provider}
                      </Text>
                      <Text
                        style={[
                          styles.badgeText,
                          {
                            color:
                              entry.risk_tier === "low"
                                ? palette.success
                                : palette.danger,
                          },
                        ]}
                      >
                        {entry.risk_tier.toUpperCase()}
                      </Text>
                    </View>
                    <Text style={[styles.monoLine, { color: palette.textSecondary }]}>
                      {entry.command}
                    </Text>
                    {entry.rule_id ? (
                      <View style={styles.cardActions}>
                        <Text style={[styles.sub, { color: palette.muted }]}>
                          {entry.rule_id}
                          {entry.revoked_ms ? ` · ${t("policyRevokedTag")}` : ""}
                        </Text>
                        {!entry.revoked_ms ? (
                          <Pressable
                            onPress={() => void handleRevoke(entry.rule_id!)}
                            style={[
                              styles.smallBtn,
                              { backgroundColor: palette.dangerSoft },
                            ]}
                          >
                            <Text
                              style={[styles.smallBtnText, { color: palette.danger }]}
                            >
                              {t("policyRevokeRule")}
                            </Text>
                          </Pressable>
                        ) : null}
                      </View>
                    ) : null}
                  </View>
                ))
              )}
            </ScrollView>
          )}
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
    maxHeight: "88%",
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
  tabWrap: { paddingHorizontal: 16, paddingBottom: 10 },
  error: { paddingHorizontal: 16, paddingBottom: 8, fontSize: 12 },
  center: { padding: 32, alignItems: "center" },
  scroll: { maxHeight: 520 },
  content: { paddingHorizontal: 16, paddingBottom: 24, gap: 10 },
  addCard: {
    borderWidth: 1,
    borderRadius: 12,
    padding: 12,
    gap: 8,
  },
  sectionLabel: { fontFamily: FONTS.semi, fontSize: 13 },
  input: {
    borderWidth: 1,
    borderRadius: 9,
    paddingHorizontal: 10,
    paddingVertical: 8,
    fontSize: 13,
  },
  addBtn: {
    flexDirection: "row",
    alignItems: "center",
    justifyContent: "center",
    gap: 6,
    paddingVertical: 9,
    borderRadius: 9,
  },
  addBtnText: { fontFamily: FONTS.semi, fontSize: 13 },
  card: {
    borderWidth: 1,
    borderRadius: 12,
    padding: 12,
    gap: 6,
  },
  cardTop: {
    flexDirection: "row",
    justifyContent: "space-between",
    alignItems: "center",
    gap: 8,
  },
  cardTitle: { fontFamily: FONTS.semi, fontSize: 13.5 },
  badge: { paddingHorizontal: 8, paddingVertical: 3, borderRadius: 999 },
  badgeText: { fontFamily: FONTS.semi, fontSize: 11 },
  monoLine: { fontFamily: FONTS.mono, fontSize: 12 },
  cardActions: {
    flexDirection: "row",
    alignItems: "center",
    justifyContent: "space-between",
    gap: 8,
    marginTop: 4,
  },
  smallBtn: {
    flexDirection: "row",
    alignItems: "center",
    gap: 4,
    paddingHorizontal: 9,
    paddingVertical: 5,
    borderRadius: 7,
  },
  smallBtnText: { fontFamily: FONTS.semi, fontSize: 11.5 },
  empty: { textAlign: "center", paddingVertical: 24, fontSize: 13 },
});
