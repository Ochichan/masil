import React, { useEffect, useState } from "react";
import {
  ActivityIndicator,
  FlatList,
  Modal,
  Pressable,
  StyleSheet,
  Text,
  View,
} from "react-native";
import { useTranslation } from "react-i18next";
import {
  getHostClient,
  useHostRuntimeStore,
} from "../hosts/HostRuntimeController";
import type { ImportableSession } from "../protocol/schemas";
import { useMasilTheme } from "../theme/ThemeProvider";

interface ImportSessionSheetProps {
  visible: boolean;
  hostId: string;
  onClose: () => void;
}

export function ImportSessionSheet({
  visible,
  hostId,
  onClose,
}: ImportSessionSheetProps) {
  const { palette } = useMasilTheme();
  const { t } = useTranslation();
  const refreshHost = useHostRuntimeStore((s) => s.refreshHost);
  const [sessions, setSessions] = useState<ImportableSession[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!visible) return;
    const client = getHostClient(hostId);
    if (!client) return;
    setLoading(true);
    setError(null);
    void client
      .request<{ sessions?: ImportableSession[] }>(
        "agent.importable.list.request",
        { limit: 40 }
      )
      .then((res) => setSessions(res.sessions ?? []))
      .catch((e) => setError(e instanceof Error ? e.message : String(e)))
      .finally(() => setLoading(false));
  }, [visible, hostId]);

  const importOne = async (item: ImportableSession) => {
    const client = getHostClient(hostId);
    if (!client) return;
    try {
      await client.request("agent.import.request", {
        provider: item.provider,
        session_id: item.session_id,
        cwd: item.cwd,
      });
      await refreshHost(hostId);
      onClose();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  return (
    <Modal visible={visible} transparent animationType="slide" onRequestClose={onClose}>
      <View style={styles.backdrop}>
        <View style={[styles.sheet, { backgroundColor: palette.panel, borderColor: palette.track }]}>
          <View style={styles.header}>
            <Text style={[styles.title, { color: palette.text }]}>
              {t("importSession")}
            </Text>
            <Pressable onPress={onClose}>
              <Text style={{ color: palette.muted, fontWeight: "700" }}>{t("cancel")}</Text>
            </Pressable>
          </View>
          {error ? <Text style={{ color: palette.danger, fontSize: 12 }}>{error}</Text> : null}
          {loading ? (
            <ActivityIndicator style={{ marginVertical: 24 }} color={palette.accent} />
          ) : (
            <FlatList
              data={sessions}
              keyExtractor={(s) => `${s.provider}-${s.session_id}`}
              renderItem={({ item }) => (
                <Pressable
                  onPress={() => void importOne(item)}
                  style={[
                    styles.card,
                    { backgroundColor: palette.canvas, borderColor: palette.track },
                  ]}
                >
                  <View style={styles.rowTop}>
                    <Text style={[styles.provider, { color: palette.accent }]}>
                      {item.provider}
                    </Text>
                    <Text numberOfLines={1} style={{ color: palette.muted, fontSize: 11, flex: 1 }}>
                      {item.cwd}
                    </Text>
                  </View>
                  <Text numberOfLines={2} style={[styles.itemTitle, { color: palette.text }]}>
                    {item.title}
                  </Text>
                </Pressable>
              )}
            />
          )}
        </View>
      </View>
    </Modal>
  );
}

const styles = StyleSheet.create({
  backdrop: {
    flex: 1,
    backgroundColor: "rgba(0,0,0,0.55)",
    justifyContent: "flex-end",
  },
  sheet: {
    borderTopLeftRadius: 16,
    borderTopRightRadius: 16,
    borderWidth: 1,
    padding: 16,
    maxHeight: "82%",
    gap: 8,
  },
  header: {
    flexDirection: "row",
    justifyContent: "space-between",
    alignItems: "center",
  },
  title: { fontSize: 16, fontWeight: "700" },
  card: {
    borderWidth: 1,
    borderRadius: 8,
    padding: 10,
    marginBottom: 8,
    gap: 4,
  },
  rowTop: { flexDirection: "row", alignItems: "center", gap: 8 },
  provider: { fontSize: 11, fontWeight: "700", textTransform: "uppercase" },
  itemTitle: { fontSize: 13, fontWeight: "600" },
});
