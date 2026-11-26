import React, { useCallback, useEffect, useState } from "react";
import {
  ActivityIndicator,
  Modal,
  Pressable,
  ScrollView,
  StyleSheet,
  Text,
  View,
} from "react-native";
import { RefreshCw, Sun, X } from "lucide-react-native";
import { useTranslation } from "react-i18next";
import { SafeAreaView } from "react-native-safe-area-context";
import {
  getHostClient,
  useHostRuntimeStore,
} from "../hosts/HostRuntimeController";
import { useMobilePanelsStore } from "../mobile-panels/store";
import type { BriefingReport } from "../protocol/schemas";
import { useMasilTheme } from "../theme/ThemeProvider";
import { FONTS } from "../theme/typography";
import { type PluginActionEvent, PluginUiRenderer } from "./PluginUiRenderer";

interface BriefingModalProps {
  visible: boolean;
  hostId: string | null;
  onClose: () => void;
}

const WINDOWS = ["2h", "8h", "24h"] as const;

export function BriefingModal({
  visible,
  hostId,
  onClose,
}: BriefingModalProps) {
  const { palette } = useMasilTheme();
  const { t } = useTranslation();
  const selectAgent = useHostRuntimeStore((s) => s.selectAgent);
  const selectPanel = useMobilePanelsStore((s) => s.selectPanel);

  const [windowSpec, setWindowSpec] = useState<(typeof WINDOWS)[number]>("8h");
  const [report, setReport] = useState<BriefingReport | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const loadBriefing = useCallback(async () => {
    if (!hostId) return;
    const client = getHostClient(hostId);
    if (!client || client.connectionState !== "connected") {
      setError(t("disconnected"));
      return;
    }
    setLoading(true);
    setError(null);
    try {
      const res = await client.request<BriefingReport>(
        "briefing.generate.request",
        { since: windowSpec }
      );
      setReport(res);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }, [hostId, windowSpec, t]);

  useEffect(() => {
    if (visible) {
      void loadBriefing();
    }
  }, [visible, loadBriefing]);

  const handleAction = (ev: PluginActionEvent) => {
    if (!hostId) return;
    const payload = (ev.payload ?? {}) as Record<string, unknown>;
    const remoteId =
      typeof payload.remote_id === "string" ? payload.remote_id : "";
    const pane = typeof payload.pane === "string" ? payload.pane : "";
    const agents = useHostRuntimeStore.getState().hosts[hostId]?.agents ?? [];
    const matched =
      (remoteId ? agents.find((a) => a.remote_id === remoteId) : undefined) ??
      (pane ? agents.find((a) => a.pane === pane) : undefined);

    if (matched) {
      selectAgent(hostId, matched.remote_id);
      if (ev.action === "view_diff" || ev.action === "resolve_collision") {
        selectPanel("workspace-tools");
      } else {
        selectPanel("agent");
      }
      onClose();
    }
  };

  return (
    <Modal visible={visible} animationType="slide" onRequestClose={onClose}>
      <SafeAreaView
        style={[styles.root, { backgroundColor: palette.canvas }]}
        edges={["top", "bottom"]}
      >
        <View
          style={[
            styles.header,
            { backgroundColor: palette.panel, borderBottomColor: palette.hairline },
          ]}
        >
          <View style={styles.titleRow}>
            <Sun size={18} color={palette.warning} />
            <Text style={[styles.title, { color: palette.text }]}>
              {t("briefingTitle")}
            </Text>
          </View>
          <View style={styles.headerActions}>
            {WINDOWS.map((w) => {
              const active = windowSpec === w;
              return (
                <Pressable
                  key={w}
                  onPress={() => setWindowSpec(w)}
                  style={[
                    styles.winChip,
                    {
                      backgroundColor: active ? palette.accent : palette.raised,
                    },
                  ]}
                >
                  <Text
                    style={{
                      color: active ? palette.accentText : palette.textSecondary,
                      fontFamily: FONTS.semi,
                      fontSize: 11.5,
                    }}
                  >
                    {w}
                  </Text>
                </Pressable>
              );
            })}
            <Pressable
              onPress={() => void loadBriefing()}
              hitSlop={8}
              style={[styles.iconBtn, { backgroundColor: palette.raised }]}
            >
              <RefreshCw size={14} color={palette.textSecondary} />
            </Pressable>
            <Pressable onPress={onClose} hitSlop={8} style={styles.iconBtn}>
              <X size={18} color={palette.textSecondary} />
            </Pressable>
          </View>
        </View>

        <ScrollView contentContainerStyle={styles.body}>
          {loading && !report ? (
            <View style={styles.center}>
              <ActivityIndicator size="large" color={palette.accent} />
            </View>
          ) : error ? (
            <Text style={{ color: palette.danger, fontSize: 13 }}>{error}</Text>
          ) : report?.ui ? (
            <PluginUiRenderer
              blocks={report.ui.blocks}
              busy={loading}
              onAction={handleAction}
            />
          ) : (
            <Text style={{ color: palette.muted, fontSize: 13.5 }}>
              {t("briefingEmpty")}
            </Text>
          )}
        </ScrollView>
      </SafeAreaView>
    </Modal>
  );
}

const styles = StyleSheet.create({
  root: { flex: 1 },
  header: {
    flexDirection: "row",
    alignItems: "center",
    justifyContent: "space-between",
    paddingHorizontal: 14,
    paddingVertical: 11,
    borderBottomWidth: 1,
  },
  titleRow: {
    flexDirection: "row",
    alignItems: "center",
    gap: 8,
  },
  title: {
    fontFamily: FONTS.bold,
    fontSize: 16,
  },
  headerActions: {
    flexDirection: "row",
    alignItems: "center",
    gap: 6,
  },
  winChip: {
    paddingHorizontal: 8,
    paddingVertical: 5,
    borderRadius: 8,
  },
  iconBtn: {
    width: 30,
    height: 30,
    borderRadius: 8,
    alignItems: "center",
    justifyContent: "center",
  },
  body: {
    padding: 14,
  },
  center: {
    paddingVertical: 48,
    alignItems: "center",
  },
});
