import React, { useEffect, useState } from "react";
import {
  FlatList,
  Pressable,
  StyleSheet,
  Switch,
  Text,
  View,
} from "react-native";
import { Trash2 } from "lucide-react-native";
import { getHostClient } from "../hosts/HostRuntimeController";
import { useMasilTheme } from "../theme/ThemeProvider";

interface ScheduleRow {
  name: string;
  enabled?: boolean;
  cron?: string;
  interval?: string;
  action?: string;
}

interface SchedulesViewProps {
  hostId: string;
  serverSocket: string;
}

export function SchedulesView({ hostId, serverSocket }: SchedulesViewProps) {
  const { palette } = useMasilTheme();
  const [rows, setRows] = useState<ScheduleRow[]>([]);

  const load = async () => {
    const client = getHostClient(hostId);
    if (!client) return;
    try {
      const res = await client.request<{ schedules?: ScheduleRow[] }>(
        "schedule.list.request",
        { server_socket: serverSocket }
      );
      setRows(res.schedules ?? []);
    } catch {
      setRows([]);
    }
  };

  useEffect(() => {
    void load();
  }, [hostId, serverSocket]);

  const toggle = async (name: string, enabled: boolean) => {
    const client = getHostClient(hostId);
    if (!client) return;
    await client.request("schedule.toggle.request", {
      server_socket: serverSocket,
      name,
      enabled,
    });
    await load();
  };

  const remove = async (name: string) => {
    const client = getHostClient(hostId);
    if (!client) return;
    await client.request("schedule.remove.request", {
      server_socket: serverSocket,
      name,
    });
    await load();
  };

  return (
    <FlatList
      data={rows}
      keyExtractor={(item) => item.name}
      contentContainerStyle={{ padding: 12, gap: 8 }}
      ListEmptyComponent={
        <Text style={{ color: palette.muted }}>
          No scheduled tasks configured on this server.
        </Text>
      }
      renderItem={({ item }) => (
        <View
          style={[
            styles.card,
            { backgroundColor: palette.panel, borderColor: palette.track },
          ]}
        >
          <View style={{ flex: 1 }}>
            <Text style={[styles.name, { color: palette.text }]}>
              {item.name}
            </Text>
            <Text style={{ color: palette.muted, fontSize: 12 }}>
              {item.cron || item.interval || "schedule"} ·{" "}
              {item.action || "draft/layout"}
            </Text>
          </View>
          <Switch
            value={Boolean(item.enabled)}
            onValueChange={(v) => void toggle(item.name, v)}
            trackColor={{ false: palette.track, true: palette.accent }}
          />
          <Pressable onPress={() => void remove(item.name)} hitSlop={8}>
            <Trash2 size={16} color={palette.danger} />
          </Pressable>
        </View>
      )}
    />
  );
}

const styles = StyleSheet.create({
  card: {
    flexDirection: "row",
    alignItems: "center",
    padding: 12,
    borderRadius: 8,
    borderWidth: 1,
    gap: 10,
  },
  name: { fontSize: 14, fontWeight: "700" },
});
