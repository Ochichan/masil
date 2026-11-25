import React, { useCallback, useEffect, useState } from "react";
import {
  ActivityIndicator,
  RefreshControl,
  ScrollView,
  StyleSheet,
  Text,
  View,
} from "react-native";
import { getHostClient } from "../hosts/HostRuntimeController";
import type {
  AgentRow,
  PluginPanelDef,
  UiDocument,
} from "../protocol/schemas";
import { useMasilTheme } from "../theme/ThemeProvider";
import { FONTS } from "../theme/typography";
import { PluginActionEvent, PluginUiRenderer } from "./PluginUiRenderer";

interface PluginPanelViewProps {
  hostId: string;
  agent: AgentRow | null;
  panel: PluginPanelDef;
}

export function PluginPanelView({
  hostId,
  agent,
  panel,
}: PluginPanelViewProps) {
  const { palette } = useMasilTheme();
  const [doc, setDoc] = useState<UiDocument | null>(null);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const fetchPanel = useCallback(
    async (actionEvent?: PluginActionEvent, silent = false) => {
      const client = getHostClient(hostId);
      if (!client) {
        setError("호스트에 연결되어 있지 않습니다.");
        setLoading(false);
        return;
      }
      if (actionEvent) {
        setBusy(true);
      } else if (!silent) {
        setLoading(true);
      }
      try {
        const res = await client.request<UiDocument>(
          "plugin.panel.render.request",
          {
            plugin_id: panel.plugin_id,
            panel_id: panel.id,
            server_socket: agent?.server_socket,
            agent: agent
              ? {
                  remote_id: agent.remote_id,
                  name: agent.name,
                  provider: agent.provider,
                  state: agent.state,
                  cwd: agent.cwd,
                  pane: agent.pane,
                  server_socket: agent.server_socket,
                  session_id: agent.session_id ?? null,
                }
              : null,
            action: actionEvent?.action ?? null,
            payload: actionEvent?.payload ?? null,
            form_values: actionEvent?.formValues ?? {},
          }
        );
        setDoc(res);
        setError(null);
      } catch (e) {
        setError(e instanceof Error ? e.message : String(e));
      } finally {
        setLoading(false);
        setBusy(false);
      }
    },
    [hostId, panel.plugin_id, panel.id, agent?.remote_id, agent?.cwd, agent?.state]
  );

  useEffect(() => {
    void fetchPanel(undefined, false);
  }, [fetchPanel]);

  useEffect(() => {
    if (!panel.refresh_secs || panel.refresh_secs < 3) return;
    const ms = Math.max(3, panel.refresh_secs) * 1000;
    const timer = setInterval(() => {
      void fetchPanel(undefined, true);
    }, ms);
    return () => clearInterval(timer);
  }, [fetchPanel, panel.refresh_secs]);

  if (loading && !doc) {
    return (
      <View style={styles.center}>
        <ActivityIndicator size="small" color={palette.accent} />
        <Text style={[styles.hint, { color: palette.muted }]}>
          {panel.title} 불러오는 중...
        </Text>
      </View>
    );
  }

  return (
    <ScrollView
      contentContainerStyle={styles.content}
      refreshControl={
        <RefreshControl
          refreshing={loading && Boolean(doc)}
          onRefresh={() => void fetchPanel(undefined, false)}
          tintColor={palette.accent}
        />
      }
    >
      {error ? (
        <View
          style={[
            styles.errBox,
            {
              backgroundColor: palette.dangerSoft,
              borderColor: palette.danger,
            },
          ]}
        >
          <Text style={[styles.errText, { color: palette.danger }]}>
            {error}
          </Text>
        </View>
      ) : null}

      {doc ? (
        <PluginUiRenderer
          blocks={doc.blocks ?? []}
          busy={busy}
          onAction={(evt) => void fetchPanel(evt, false)}
        />
      ) : null}
    </ScrollView>
  );
}

const styles = StyleSheet.create({
  center: {
    flex: 1,
    alignItems: "center",
    justifyContent: "center",
    padding: 24,
    gap: 10,
  },
  hint: { fontFamily: FONTS.regular, fontSize: 13 },
  content: { padding: 14, gap: 12, paddingBottom: 36 },
  errBox: {
    borderWidth: 1,
    borderRadius: 10,
    padding: 11,
  },
  errText: { fontFamily: FONTS.mono, fontSize: 12, lineHeight: 17 },
});
