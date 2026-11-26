import React, { useEffect, useState } from "react";
import {
  FlatList,
  Pressable,
  StyleSheet,
  Text,
  View,
} from "react-native";
import { useTranslation } from "react-i18next";
import { getHostClient } from "../hosts/HostRuntimeController";
import type { AgentRow } from "../protocol/schemas";
import { useMasilTheme } from "../theme/ThemeProvider";
import { ConfirmModal } from "./ConfirmModal";

interface CheckpointRow {
  id: string | number;
  created_ms?: number;
  files?: number;
  label?: string;
}

interface CheckpointsViewProps {
  hostId: string;
  agent: AgentRow;
}

export function CheckpointsView({ hostId, agent }: CheckpointsViewProps) {
  const { palette } = useMasilTheme();
  const { t } = useTranslation();
  const [rows, setRows] = useState<CheckpointRow[]>([]);
  const [restoreTarget, setRestoreTarget] = useState<string | null>(null);
  const [message, setMessage] = useState<string | null>(null);

  const load = async () => {
    const client = getHostClient(hostId);
    if (!client) return;
    try {
      const res = await client.request<{ checkpoints?: CheckpointRow[] }>(
        "checkpoint.list.request",
        {
          server_socket: agent.server_socket,
          pane: agent.pane,
          target: agent.pane || agent.name,
        }
      );
      setRows(res.checkpoints ?? []);
    } catch {
      setRows([]);
    }
  };

  useEffect(() => {
    void load();
  }, [hostId, agent.remote_id]);

  const makeCheckpoint = async () => {
    const client = getHostClient(hostId);
    if (!client) return;
    try {
      await client.request("checkpoint.make.request", {
        server_socket: agent.server_socket,
        pane: agent.pane,
        target: agent.pane || agent.name,
      });
      setMessage(t("checkpointCreated"));
      await load();
    } catch (e) {
      setMessage(e instanceof Error ? e.message : String(e));
    }
  };

  const confirmRestore = async () => {
    const cpId = restoreTarget;
    setRestoreTarget(null);
    if (!cpId) return;
    const client = getHostClient(hostId);
    if (!client) return;
    try {
      await client.request("checkpoint.restore.request", {
        server_socket: agent.server_socket,
        pane: agent.pane,
        target: agent.pane || agent.name,
        checkpoint_id: cpId,
      });
      setMessage(t("checkpointRestored", { id: cpId }));
    } catch (e) {
      setMessage(e instanceof Error ? e.message : String(e));
    }
  };

  return (
    <View style={styles.root}>
      <View style={styles.topRow}>
        <Pressable
          onPress={() => void makeCheckpoint()}
          style={[styles.createBtn, { backgroundColor: palette.accent }]}
        >
          <Text style={{ color: palette.accentText, fontWeight: "700" }}>
            {t("createCheckpoint")}
          </Text>
        </Pressable>
      </View>
      {message ? (
        <Text style={{ color: palette.muted, paddingHorizontal: 12, fontSize: 12 }}>
          {message}
        </Text>
      ) : null}
      <FlatList
        data={rows}
        keyExtractor={(item) => String(item.id)}
        ListEmptyComponent={
          <Text style={{ color: palette.muted, padding: 16 }}>
            {t("noCheckpointsSaved", { name: agent.name })}
          </Text>
        }
        renderItem={({ item }) => (
          <View
            style={[
              styles.row,
              { backgroundColor: palette.panel, borderColor: palette.track },
            ]}
          >
            <View style={{ flex: 1 }}>
              <Text style={[styles.id, { color: palette.text }]}>
                #{String(item.id)}
              </Text>
              <Text style={{ color: palette.muted, fontSize: 12 }}>
                {t("filesCount", { count: item.files ?? 0 })}
              </Text>
            </View>
            <Pressable
              onPress={() => setRestoreTarget(String(item.id))}
              style={[
                styles.restoreBtn,
                { borderColor: palette.danger, backgroundColor: palette.raised },
              ]}
            >
              <Text style={{ color: palette.danger, fontWeight: "600", fontSize: 12 }}>
                {t("restore")}
              </Text>
            </Pressable>
          </View>
        )}
      />

      <ConfirmModal
        visible={restoreTarget !== null}
        title={t("confirmActionTitle")}
        body={t("confirmRestoreBody", { id: restoreTarget ?? "" })}
        destructive
        onCancel={() => setRestoreTarget(null)}
        onConfirm={() => void confirmRestore()}
      />
    </View>
  );
}

const styles = StyleSheet.create({
  root: { flex: 1, gap: 8 },
  topRow: { padding: 12, paddingBottom: 4 },
  createBtn: {
    paddingVertical: 10,
    borderRadius: 8,
    alignItems: "center",
  },
  row: {
    flexDirection: "row",
    alignItems: "center",
    marginHorizontal: 12,
    marginBottom: 8,
    padding: 12,
    borderRadius: 8,
    borderWidth: 1,
  },
  id: { fontFamily: "monospace", fontWeight: "700", fontSize: 13 },
  restoreBtn: {
    paddingHorizontal: 10,
    paddingVertical: 6,
    borderRadius: 6,
    borderWidth: 1,
  },
});
