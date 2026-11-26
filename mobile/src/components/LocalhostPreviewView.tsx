import React, { useCallback, useEffect, useRef, useState } from "react";
import {
  ActivityIndicator,
  Pressable,
  ScrollView,
  StyleSheet,
  Text,
  TextInput,
  View,
} from "react-native";
import {
  AlertTriangle,
  Globe,
  Pencil,
  RefreshCw,
} from "lucide-react-native";
import { useTranslation } from "react-i18next";
import { WebView, type WebViewMessageEvent } from "react-native-webview";
import { isNativeTunnelProxyAvailable } from "../../modules/masil-tunnel-proxy/src";
import {
  type ActiveTunnelSession,
  startE2eeLocalhostTunnel,
} from "../../modules/masil-tunnel-proxy/src/tunnel-controller";
import {
  VIEWPORT_PRESETS,
  type ViewportPreset,
  type WebViewDiagnosticEntry,
  appendDiagnosticRingBuffer,
  buildWebViewDiagnosticsBootstrapScript,
  parseDiagnosticMessage,
  validateManualPortInput,
} from "../agents/phase2-helpers";
import { getHostClient } from "../hosts/HostRuntimeController";
import type { AgentRow, TunnelPortDiscovery } from "../protocol/schemas";
import { useMasilTheme } from "../theme/ThemeProvider";
import { FONTS } from "../theme/typography";
import { MarkupAnnotatorModal } from "./MarkupAnnotatorModal";

interface LocalhostPreviewViewProps {
  hostId: string;
  agent: AgentRow;
}

export function LocalhostPreviewView({
  hostId,
  agent,
}: LocalhostPreviewViewProps) {
  const { palette } = useMasilTheme();
  const { t } = useTranslation();

  const [discovery, setDiscovery] = useState<TunnelPortDiscovery | null>(null);
  const [loadingPorts, setLoadingPorts] = useState(false);
  const [manualPort, setManualPort] = useState("");
  const [activePort, setActivePort] = useState<number | null>(null);
  const [localOrigin, setLocalOrigin] = useState<string | null>(null);
  const [currentPath, setCurrentPath] = useState("/");
  const [viewport, setViewport] = useState<ViewportPreset>(VIEWPORT_PRESETS[0]!);
  const [diagnostics, setDiagnostics] = useState<WebViewDiagnosticEntry[]>([]);
  const [showErrors, setShowErrors] = useState(false);
  const [showMarkup, setShowMarkup] = useState(false);
  const [statusNotice, setStatusNotice] = useState<string | null>(null);
  const [errorMsg, setErrorMsg] = useState<string | null>(null);
  const [reloadKey, setReloadKey] = useState(0);

  const tunnelRef = useRef<ActiveTunnelSession | null>(null);
  const webViewRef = useRef<WebView | null>(null);

  const fetchPorts = useCallback(async () => {
    const client = getHostClient(hostId);
    if (!client || client.connectionState !== "connected") return;
    setLoadingPorts(true);
    try {
      const res = await client.request<TunnelPortDiscovery>(
        "tunnel.ports.list.request",
        {
          server_socket: agent.server_socket,
          pane: agent.pane,
        }
      );
      setDiscovery(res);
    } catch (e) {
      setErrorMsg(e instanceof Error ? e.message : String(e));
    } finally {
      setLoadingPorts(false);
    }
  }, [hostId, agent.server_socket, agent.pane]);

  useEffect(() => {
    void fetchPorts();
  }, [fetchPorts]);

  useEffect(() => {
    return () => {
      void tunnelRef.current?.stop();
      tunnelRef.current = null;
    };
  }, []);

  const openPort = useCallback(
    async (port: number, initialUrl?: string) => {
      setErrorMsg(null);
      setStatusNotice(null);
      setDiagnostics([]);
      setActivePort(port);

      try {
        const parsedPath = initialUrl
          ? new URL(initialUrl).pathname || "/"
          : "/";
        setCurrentPath(parsedPath);
      } catch {
        setCurrentPath("/");
      }

      if (!isNativeTunnelProxyAvailable()) {
        setLocalOrigin(null);
        setErrorMsg(t("previewNativeUnavailable"));
        return;
      }

      try {
        await tunnelRef.current?.stop();
        const session = await startE2eeLocalhostTunnel({
          hostId,
          targetPort: port,
          agentId: agent.remote_id,
          onRebound: (nextLocalPort) => {
            setLocalOrigin(`http://127.0.0.1:${nextLocalPort}`);
            setReloadKey((k) => k + 1);
          },
          onStreamError: (msg) => {
            setErrorMsg(msg);
          },
        });
        tunnelRef.current = session;
        setLocalOrigin(session.localOrigin);
        setReloadKey((k) => k + 1);
      } catch (e) {
        setLocalOrigin(null);
        setErrorMsg(e instanceof Error ? e.message : String(e));
      }
    },
    [hostId, agent.remote_id, t]
  );

  const handleOpenManualPort = () => {
    const denied = discovery?.denied_ports ?? [];
    const check = validateManualPortInput(manualPort, denied);
    if (!check.ok) {
      setErrorMsg(
        check.reason === "denied"
          ? t("previewPortDenied", { port: manualPort.trim() })
          : t("previewPortInvalid")
      );
      return;
    }
    void openPort(check.port);
  };

  const handleWebViewMessage = (ev: WebViewMessageEvent) => {
    const entry = parseDiagnosticMessage(ev.nativeEvent.data);
    if (entry) {
      setDiagnostics((prev) => appendDiagnosticRingBuffer(prev, entry, 20));
    }
  };

  const previewDisplayUrl = activePort
    ? `http://localhost:${activePort}${currentPath}`
    : "";

  const webViewUri = localOrigin ? `${localOrigin}${currentPath}` : null;

  return (
    <View style={[styles.root, { backgroundColor: palette.canvas }]}>
      {/* Port picker & manual input bar */}
      <View
        style={[
          styles.portBar,
          { backgroundColor: palette.panel, borderBottomColor: palette.hairline },
        ]}
      >
        <View style={styles.manualRow}>
          <TextInput
            value={manualPort}
            onChangeText={setManualPort}
            placeholder={t("previewCustomPortPlaceholder")}
            placeholderTextColor={palette.muted}
            keyboardType="number-pad"
            returnKeyType="go"
            onSubmitEditing={handleOpenManualPort}
            style={[
              styles.portInput,
              {
                backgroundColor: palette.canvas,
                borderColor: palette.borderSubtle,
                color: palette.text,
              },
            ]}
          />
          <Pressable
            onPress={handleOpenManualPort}
            style={[styles.openBtn, { backgroundColor: palette.accent }]}
          >
            <Text style={[styles.openBtnText, { color: palette.accentText }]}>
              {t("previewConnect")}
            </Text>
          </Pressable>
          <Pressable
            onPress={() => void fetchPorts()}
            style={[styles.iconBtn, { backgroundColor: palette.raised }]}
          >
            {loadingPorts ? (
              <ActivityIndicator size="small" color={palette.accent} />
            ) : (
              <RefreshCw size={14} color={palette.textSecondary} />
            )}
          </Pressable>
        </View>

        <ScrollView
          horizontal
          showsHorizontalScrollIndicator={false}
          contentContainerStyle={styles.chipRow}
        >
          {(discovery?.recommended ?? []).map((p) => {
            const selected = activePort === p.port;
            return (
              <Pressable
                key={`rec-${p.port}`}
                onPress={() => void openPort(p.port, p.url)}
                style={[
                  styles.portChip,
                  {
                    backgroundColor: selected
                      ? palette.accent
                      : palette.accentSoft,
                    borderColor: selected ? palette.accent : palette.accentBorder,
                  },
                ]}
              >
                <Globe
                  size={11}
                  color={selected ? palette.accentText : palette.accent}
                />
                <Text
                  style={[
                    styles.portChipText,
                    { color: selected ? palette.accentText : palette.accent },
                  ]}
                >
                  :{p.port}
                  {p.label ? ` ${p.label}` : ` ${p.process}`}
                </Text>
              </Pressable>
            );
          })}
          {(discovery?.host_ports ?? []).map((p) => {
            const selected = activePort === p.port;
            return (
              <Pressable
                key={`host-${p.port}`}
                onPress={() => void openPort(p.port, p.url)}
                style={[
                  styles.portChip,
                  {
                    backgroundColor: selected ? palette.accent : palette.raised,
                    borderColor: selected ? palette.accent : palette.hairline,
                  },
                ]}
              >
                <Text
                  style={[
                    styles.portChipText,
                    {
                      color: selected
                        ? palette.accentText
                        : palette.textSecondary,
                    },
                  ]}
                >
                  :{p.port} {p.process}
                </Text>
              </Pressable>
            );
          })}
        </ScrollView>

        {activePort ? (
          <View style={styles.controlsRow}>
            {VIEWPORT_PRESETS.map((vp) => {
              const active = viewport.id === vp.id;
              return (
                <Pressable
                  key={vp.id}
                  onPress={() => setViewport(vp)}
                  style={[
                    styles.vpChip,
                    {
                      backgroundColor: active ? palette.raised : "transparent",
                      borderColor: active ? palette.accent : palette.hairline,
                    },
                  ]}
                >
                  <Text
                    style={{
                      color: active ? palette.accent : palette.muted,
                      fontFamily: FONTS.mono,
                      fontSize: 10.5,
                    }}
                  >
                    {vp.label}
                  </Text>
                </Pressable>
              );
            })}
            <View style={{ flex: 1 }} />
            {diagnostics.length > 0 ? (
              <Pressable
                onPress={() => setShowErrors((v) => !v)}
                style={[
                  styles.errBadge,
                  { backgroundColor: palette.dangerSoft },
                ]}
              >
                <AlertTriangle size={11} color={palette.danger} />
                <Text
                  style={{
                    color: palette.danger,
                    fontFamily: FONTS.semi,
                    fontSize: 11,
                  }}
                >
                  {t("previewErrorsCount", { count: diagnostics.length })}
                </Text>
              </Pressable>
            ) : null}
            <Pressable
              onPress={() => webViewRef.current?.reload()}
              style={[styles.iconBtn, { backgroundColor: palette.raised }]}
            >
              <RefreshCw size={13} color={palette.textSecondary} />
            </Pressable>
            <Pressable
              onPress={() => setShowMarkup(true)}
              style={[
                styles.markupBtn,
                { backgroundColor: palette.dangerSoft, borderColor: palette.danger },
              ]}
            >
              <Pencil size={12} color={palette.danger} />
              <Text
                style={{
                  color: palette.danger,
                  fontFamily: FONTS.bold,
                  fontSize: 11.5,
                }}
              >
                {t("previewMarkupBtn")}
              </Text>
            </Pressable>
          </View>
        ) : null}
      </View>

      {statusNotice ? (
        <View
          style={[
            styles.noticeBanner,
            { backgroundColor: palette.successSoft },
          ]}
        >
          <Text style={{ color: palette.success, fontSize: 12 }}>
            {statusNotice}
          </Text>
        </View>
      ) : null}

      {errorMsg ? (
        <View
          style={[styles.noticeBanner, { backgroundColor: palette.dangerSoft }]}
        >
          <Text style={{ color: palette.danger, fontSize: 12 }}>{errorMsg}</Text>
        </View>
      ) : null}

      {showErrors && diagnostics.length > 0 ? (
        <ScrollView
          style={[
            styles.errorDrawer,
            { backgroundColor: palette.panel, borderBottomColor: palette.hairline },
          ]}
        >
          {diagnostics.map((d, idx) => (
            <Text
              key={`${d.ts}-${idx}`}
              style={{
                color: d.kind === "network" ? palette.warning : palette.danger,
                fontFamily: FONTS.mono,
                fontSize: 11,
                marginBottom: 4,
              }}
            >
              [{d.kind}] {d.message}
            </Text>
          ))}
        </ScrollView>
      ) : null}

      {webViewUri ? (
        <View style={styles.webWrap}>
          <WebView
            key={`${webViewUri}-${reloadKey}`}
            ref={webViewRef}
            source={{ uri: webViewUri }}
            injectedJavaScriptBeforeContentLoaded={buildWebViewDiagnosticsBootstrapScript()}
            onMessage={handleWebViewMessage}
            onNavigationStateChange={(nav) => {
              if (localOrigin && nav.url.startsWith(localOrigin)) {
                setCurrentPath(nav.url.slice(localOrigin.length) || "/");
              }
            }}
            javaScriptEnabled
            domStorageEnabled
            originWhitelist={["http://127.0.0.1:*", "http://localhost:*"]}
            style={{ flex: 1, backgroundColor: palette.canvas }}
          />
        </View>
      ) : (
        <View style={styles.emptyWrap}>
          <Globe size={28} color={palette.muted} />
          <Text style={[styles.emptyText, { color: palette.muted }]}>
            {t("previewNoPortSelected")}
          </Text>
          {activePort ? (
            <Pressable
              onPress={() => setShowMarkup(true)}
              style={[
                styles.markupFallbackBtn,
                { backgroundColor: palette.accentSoft, borderColor: palette.accent },
              ]}
            >
              <Pencil size={13} color={palette.accent} />
              <Text
                style={{
                  color: palette.accent,
                  fontFamily: FONTS.semi,
                  fontSize: 12.5,
                }}
              >
                {t("previewMarkupBtn")}
              </Text>
            </Pressable>
          ) : null}
        </View>
      )}

      <MarkupAnnotatorModal
        visible={showMarkup}
        hostId={hostId}
        agent={agent}
        previewUrl={previewDisplayUrl || "http://localhost"}
        viewportLabel={viewport.label}
        diagnostics={diagnostics}
        onClose={() => setShowMarkup(false)}
        onDelivered={(msg) => setStatusNotice(msg)}
      />
    </View>
  );
}

const styles = StyleSheet.create({
  root: { flex: 1 },
  portBar: {
    borderBottomWidth: 1,
    paddingHorizontal: 12,
    paddingVertical: 9,
    gap: 8,
  },
  manualRow: {
    flexDirection: "row",
    alignItems: "center",
    gap: 8,
  },
  portInput: {
    flex: 1,
    height: 34,
    borderWidth: 1,
    borderRadius: 9,
    paddingHorizontal: 10,
    fontFamily: FONTS.mono,
    fontSize: 12.5,
  },
  openBtn: {
    height: 34,
    paddingHorizontal: 12,
    borderRadius: 9,
    alignItems: "center",
    justifyContent: "center",
  },
  openBtnText: {
    fontFamily: FONTS.bold,
    fontSize: 12.5,
  },
  iconBtn: {
    width: 32,
    height: 32,
    borderRadius: 9,
    alignItems: "center",
    justifyContent: "center",
  },
  chipRow: {
    gap: 6,
    alignItems: "center",
  },
  portChip: {
    flexDirection: "row",
    alignItems: "center",
    gap: 5,
    paddingHorizontal: 9,
    paddingVertical: 5,
    borderRadius: 999,
    borderWidth: 1,
  },
  portChipText: {
    fontFamily: FONTS.mono,
    fontSize: 11.5,
  },
  controlsRow: {
    flexDirection: "row",
    alignItems: "center",
    gap: 6,
  },
  vpChip: {
    paddingHorizontal: 7,
    paddingVertical: 4,
    borderRadius: 6,
    borderWidth: 1,
  },
  errBadge: {
    flexDirection: "row",
    alignItems: "center",
    gap: 4,
    paddingHorizontal: 7,
    paddingVertical: 4,
    borderRadius: 999,
  },
  markupBtn: {
    flexDirection: "row",
    alignItems: "center",
    gap: 4,
    paddingHorizontal: 9,
    paddingVertical: 5,
    borderRadius: 8,
    borderWidth: 1,
  },
  noticeBanner: {
    paddingHorizontal: 12,
    paddingVertical: 7,
  },
  errorDrawer: {
    maxHeight: 120,
    borderBottomWidth: 1,
    paddingHorizontal: 12,
    paddingVertical: 8,
  },
  webWrap: {
    flex: 1,
  },
  emptyWrap: {
    flex: 1,
    alignItems: "center",
    justifyContent: "center",
    padding: 28,
    gap: 10,
  },
  emptyText: {
    fontFamily: FONTS.regular,
    fontSize: 13.5,
    textAlign: "center",
    lineHeight: 20,
  },
  markupFallbackBtn: {
    flexDirection: "row",
    alignItems: "center",
    gap: 6,
    paddingHorizontal: 14,
    paddingVertical: 8,
    borderRadius: 10,
    borderWidth: 1,
    marginTop: 6,
  },
});
