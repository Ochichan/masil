import React, { useState } from "react";
import { ActivityIndicator, Pressable, StyleSheet, Text, View } from "react-native";
import { ShieldCheck, Zap } from "lucide-react-native";
import { useTranslation } from "react-i18next";
import type { BatchPromptCandidate } from "../agents/phase3-helpers";
import { getHostClient, useHostRuntimeStore } from "../hosts/HostRuntimeController";
import { useMasilTheme } from "../theme/ThemeProvider";
import { FONTS } from "../theme/typography";

interface BatchApproveBarProps {
  hostId: string;
  safeCandidates: BatchPromptCandidate[];
  onApproved?: (approvedCount: number) => void;
}

/**
 * D-10 Mobile Safe Batch-Approve Bar:
 * Displayed at the top of the Inbox when >= 2 verified `low`-risk permission
 * prompts are waiting. Medium/High/Critical prompts are strictly excluded on
 * both client and host and remain isolated as individual confirmation cards.
 */
export function BatchApproveBar({
  hostId,
  safeCandidates,
  onApproved,
}: BatchApproveBarProps) {
  const { palette } = useMasilTheme();
  const { t } = useTranslation();
  const refreshHost = useHostRuntimeStore((s) => s.refreshHost);
  const [busy, setBusy] = useState(false);
  const [resultMsg, setResultMsg] = useState<string | null>(null);

  if (safeCandidates.length < 2) {
    return null;
  }

  const handleApproveAll = async () => {
    const client = getHostClient(hostId);
    if (!client || busy) return;
    setBusy(true);
    setResultMsg(null);
    try {
      const res = await client.request<{
        approved_count: number;
        rejected_count: number;
      }>("prompt.batch_approve.request", {
        items: safeCandidates.map((c) => ({
          server_socket: c.serverSocket,
          pane: c.pane,
          prompt_id: c.prompt.id,
        })),
      });
      setResultMsg(t("batchApproveDone", { approved: res.approved_count }));
      onApproved?.(res.approved_count);
      await refreshHost(hostId, false);
    } catch (e) {
      setResultMsg(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <View
      style={[
        styles.bar,
        {
          backgroundColor: palette.successSoft,
          borderColor: palette.success,
        },
      ]}
    >
      <View style={styles.left}>
        <View style={styles.titleRow}>
          <Zap size={14} color={palette.success} />
          <Text style={[styles.title, { color: palette.text }]} numberOfLines={1}>
            {t("batchApproveTitle", { count: safeCandidates.length })}
          </Text>
        </View>
        <Text style={[styles.sub, { color: palette.textSecondary }]} numberOfLines={2}>
          {resultMsg ?? t("batchApproveSubtitle")}
        </Text>
      </View>
      <Pressable
        onPress={() => void handleApproveAll()}
        disabled={busy}
        style={[
          styles.btn,
          { backgroundColor: palette.success, opacity: busy ? 0.65 : 1 },
        ]}
      >
        {busy ? (
          <ActivityIndicator size="small" color="#ffffff" />
        ) : (
          <>
            <ShieldCheck size={14} color="#ffffff" />
            <Text style={styles.btnText}>
              {t("batchApproveAction", { count: safeCandidates.length })}
            </Text>
          </>
        )}
      </Pressable>
    </View>
  );
}

const styles = StyleSheet.create({
  bar: {
    flexDirection: "row",
    alignItems: "center",
    justifyContent: "space-between",
    gap: 10,
    marginHorizontal: 12,
    marginBottom: 8,
    paddingHorizontal: 12,
    paddingVertical: 10,
    borderRadius: 12,
    borderWidth: 1,
  },
  left: { flex: 1, gap: 2 },
  titleRow: { flexDirection: "row", alignItems: "center", gap: 6 },
  title: { fontFamily: FONTS.semi, fontSize: 13 },
  sub: { fontFamily: FONTS.regular, fontSize: 11.5 },
  btn: {
    flexDirection: "row",
    alignItems: "center",
    gap: 5,
    paddingHorizontal: 11,
    paddingVertical: 8,
    borderRadius: 9,
  },
  btnText: {
    color: "#ffffff",
    fontFamily: FONTS.semi,
    fontSize: 12,
  },
});
