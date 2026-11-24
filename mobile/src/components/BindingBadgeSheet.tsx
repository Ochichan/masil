import React, { useEffect, useState } from "react";
import {
  FlatList,
  Modal,
  Pressable,
  StyleSheet,
  Text,
  View,
} from "react-native";
import { useTranslation } from "react-i18next";
import { getHostClient } from "../hosts/HostRuntimeController";
import type {
  AgentRow,
  BindingConfidence,
  ImportableSession,
} from "../protocol/schemas";
import { useMasilTheme } from "../theme/ThemeProvider";

interface BindingBadgeSheetProps {
  visible: boolean;
  hostId: string;
  agent: AgentRow;
  confidence: BindingConfidence;
  currentSessionId: string | null;
  onClose: () => void;
  onBound: () => void;
}

export function BindingBadgeSheet({
  visible,
  hostId,
  agent,
  confidence,
  currentSessionId,
  onClose,
  onBound,
}: BindingBadgeSheetProps) {
  const { palette } = useMasilTheme();
  const { t } = useTranslation();
  const [candidates, setCandidates] = useState<ImportableSession[]>([]);

  useEffect(() => {
    if (!visible) return;
    const client = getHostClient(hostId);
    if (!client) return;
    void client
      .request<{ candidates?: ImportableSession[] }>(
        "agent.session.candidates.request",
        {
          provider: agent.provider,
          cwd: agent.cwd,
          limit: 20,
        }
      )
      .then((res) => setCandidates(res.candidates ?? []))
      .catch(() => {});
  }, [visible, hostId, agent.provider, agent.cwd]);

  const bindSession = async (sessionId: string) => {
    const client = getHostClient(hostId);
    if (!client) return;
    await client.request("agent.session.bind.request", {
      server_socket: agent.server_socket,
      pane: agent.pane,
      session_id: sessionId,
    });
    onBound();
    onClose();
  };

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
            { backgroundColor: palette.panel, borderColor: palette.track },
          ]}
        >
          <View style={styles.header}>
            <Text style={[styles.title, { color: palette.text }]}>
              Transcript Binding ({confidence})
            </Text>
            <Pressable onPress={onClose}>
              <Text style={{ color: palette.accent, fontWeight: "700" }}>
                {t("cancel")}
              </Text>
            </Pressable>
          </View>
          <Text style={[styles.sub, { color: palette.muted }]}>
            {agent.provider} · {agent.cwd}
          </Text>
          <FlatList
            data={candidates}
            keyExtractor={(item) => item.session_id}
            style={styles.list}
            renderItem={({ item }) => {
              const active = item.session_id === currentSessionId;
              return (
                <Pressable
                  onPress={() => void bindSession(item.session_id)}
                  style={[
                    styles.row,
                    {
                      backgroundColor: active ? palette.raised : palette.canvas,
                      borderColor: active ? palette.accent : palette.track,
                    },
                  ]}
                >
                  <Text
                    numberOfLines={1}
                    style={[styles.rowTitle, { color: palette.text }]}
                  >
                    {item.title}
                  </Text>
                  <Text
                    numberOfLines={1}
                    style={[styles.rowMeta, { color: palette.muted }]}
                  >
                    {item.session_id.slice(0, 14)} · {item.cwd}
                  </Text>
                </Pressable>
              );
            }}
          />
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
    maxHeight: "75%",
    gap: 8,
  },
  header: {
    flexDirection: "row",
    justifyContent: "space-between",
    alignItems: "center",
  },
  title: {
    fontSize: 15,
    fontWeight: "700",
  },
  sub: {
    fontSize: 12,
  },
  list: {
    marginTop: 6,
  },
  row: {
    borderWidth: 1,
    borderRadius: 8,
    padding: 10,
    marginBottom: 8,
    gap: 4,
  },
  rowTitle: {
    fontSize: 14,
    fontWeight: "600",
  },
  rowMeta: {
    fontSize: 11,
  },
});
