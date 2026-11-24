import React from "react";
import {
  Pressable,
  ScrollView,
  StyleSheet,
  Switch,
  Text,
  View,
} from "react-native";
import { useRouter } from "expo-router";
import { useTranslation } from "react-i18next";
import { SafeAreaView } from "react-native-safe-area-context";
import {
  getHostClient,
  useHostRuntimeStore,
} from "../hosts/HostRuntimeController";
import { registerPushTokenWithHost } from "../notifications/push-manager";
import { useMasilTheme } from "../theme/ThemeProvider";

export default function SettingsScreen() {
  const { palette, preference, setPreference } = useMasilTheme();
  const { t, i18n } = useTranslation();
  const router = useRouter();

  const hostOrder = useHostRuntimeStore((s) => s.hostOrder);
  const hosts = useHostRuntimeStore((s) => s.hosts);
  const activeHostId = useHostRuntimeStore((s) => s.activeHostId);
  const removeHost = useHostRuntimeStore((s) => s.removeHost);
  const refreshHost = useHostRuntimeStore((s) => s.refreshHost);
  const voiceMode = useHostRuntimeStore((s) => s.voiceMode);
  const setVoiceMode = useHostRuntimeStore((s) => s.setVoiceMode);

  const activeHost = activeHostId ? hosts[activeHostId] : undefined;

  const toggleHook = async (providerId: string, enabled: boolean) => {
    if (!activeHostId) return;
    const client = getHostClient(activeHostId);
    if (!client) return;
    await client.request("host.hooks.set.request", {
      provider: providerId,
      enabled,
    });
    await refreshHost(activeHostId);
  };

  return (
    <SafeAreaView style={[styles.root, { backgroundColor: palette.canvas }]}>
      <View
        style={[
          styles.header,
          { backgroundColor: palette.panel, borderBottomColor: palette.track },
        ]}
      >
        <Text style={[styles.title, { color: palette.text }]}>
          {t("settings")}
        </Text>
        <Pressable onPress={() => router.back()}>
          <Text style={{ color: palette.accent, fontWeight: "700" }}>Done</Text>
        </Pressable>
      </View>

      <ScrollView contentContainerStyle={styles.content}>
        {/* Language & Theme */}
        <Text style={[styles.sectionTitle, { color: palette.muted }]}>
          Appearance & Language
        </Text>
        <View
          style={[
            styles.card,
            { backgroundColor: palette.panel, borderColor: palette.track },
          ]}
        >
          <View style={styles.row}>
            <Text style={{ color: palette.text, fontWeight: "600" }}>
              Language
            </Text>
            <View style={styles.pills}>
              {(["ko", "en"] as const).map((lng) => {
                const active = i18n.language.startsWith(lng);
                return (
                  <Pressable
                    key={lng}
                    onPress={() => void i18n.changeLanguage(lng)}
                    style={[
                      styles.pill,
                      {
                        backgroundColor: active
                          ? palette.accent
                          : palette.raised,
                      },
                    ]}
                  >
                    <Text
                      style={{
                        color: active ? palette.accentText : palette.text,
                        fontWeight: "700",
                        fontSize: 12,
                      }}
                    >
                      {lng === "ko" ? "한국어" : "English"}
                    </Text>
                  </Pressable>
                );
              })}
            </View>
          </View>

          <View style={styles.row}>
            <Text style={{ color: palette.text, fontWeight: "600" }}>
              Theme
            </Text>
            <View style={styles.pills}>
              {(["dark", "light", "system"] as const).map((th) => {
                const active = preference === th;
                return (
                  <Pressable
                    key={th}
                    onPress={() => setPreference(th)}
                    style={[
                      styles.pill,
                      {
                        backgroundColor: active
                          ? palette.accent
                          : palette.raised,
                      },
                    ]}
                  >
                    <Text
                      style={{
                        color: active ? palette.accentText : palette.text,
                        fontWeight: "700",
                        fontSize: 12,
                      }}
                    >
                      {th}
                    </Text>
                  </Pressable>
                );
              })}
            </View>
          </View>
        </View>

        {/* Voice Dictation STT Mode */}
        <Text style={[styles.sectionTitle, { color: palette.muted }]}>
          Voice Dictation
        </Text>
        <View
          style={[
            styles.card,
            { backgroundColor: palette.panel, borderColor: palette.track },
          ]}
        >
          {(["os", "host"] as const).map((mode) => (
            <Pressable
              key={mode}
              onPress={() => setVoiceMode(mode)}
              style={styles.row}
            >
              <Text style={{ color: palette.text }}>
                {mode === "os" ? t("voiceModeOs") : t("voiceModeHost")}
              </Text>
              <Text
                style={{
                  color: voiceMode === mode ? palette.accent : palette.muted,
                  fontWeight: "700",
                }}
              >
                {voiceMode === mode ? "Selected" : ""}
              </Text>
            </Pressable>
          ))}
        </View>

        {/* Active Host Provider Hooks & Push */}
        {activeHost ? (
          <>
            <Text style={[styles.sectionTitle, { color: palette.muted }]}>
              Active Host ({activeHost.host.hostName}) · Hooks & Push
            </Text>
            <View
              style={[
                styles.card,
                { backgroundColor: palette.panel, borderColor: palette.track },
              ]}
            >
              <Text style={{ color: palette.muted, fontSize: 12 }}>
                {activeHost.capabilities.push_enabled
                  ? t("pushStatusConfigured")
                  : t("pushStatusHookFallback")}
              </Text>
              <Pressable
                onPress={() => void registerPushTokenWithHost(activeHost.host.hostId)}
                style={[styles.pill, { backgroundColor: palette.raised, alignSelf: "flex-start" }]}
              >
                <Text style={{ color: palette.accent, fontWeight: "700", fontSize: 12 }}>
                  Register push token
                </Text>
              </Pressable>
              {activeHost.hooks.map((h) => (
                <View key={h.id} style={styles.row}>
                  <View style={{ flex: 1 }}>
                    <Text style={{ color: palette.text, fontWeight: "600" }}>
                      {h.label}
                    </Text>
                    <Text style={{ color: palette.muted, fontSize: 11 }}>
                      {h.cli_installed ? "CLI detected" : "CLI not found"}
                    </Text>
                  </View>
                  <Switch
                    value={h.hook_installed}
                    onValueChange={(v) => void toggleHook(h.id, v)}
                    trackColor={{ false: palette.track, true: palette.accent }}
                  />
                </View>
              ))}
            </View>
          </>
        ) : null}

        {/* Paired Hosts */}
        <Text style={[styles.sectionTitle, { color: palette.muted }]}>
          Paired Hosts
        </Text>
        <View
          style={[
            styles.card,
            { backgroundColor: palette.panel, borderColor: palette.track },
          ]}
        >
          {hostOrder.map((hid) => {
            const h = hosts[hid];
            if (!h) return null;
            return (
              <View key={hid} style={styles.row}>
                <View style={{ flex: 1 }}>
                  <Text style={{ color: palette.text, fontWeight: "700" }}>
                    {h.host.hostName}
                  </Text>
                  <Text style={{ color: palette.muted, fontSize: 11 }}>
                    {h.host.relayUrl || h.host.directAddr} · {h.connection}
                  </Text>
                </View>
                <Pressable onPress={() => void removeHost(hid)}>
                  <Text style={{ color: palette.danger, fontWeight: "700" }}>
                    Remove
                  </Text>
                </Pressable>
              </View>
            );
          })}
        </View>
      </ScrollView>
    </SafeAreaView>
  );
}

const styles = StyleSheet.create({
  root: { flex: 1 },
  header: {
    flexDirection: "row",
    justifyContent: "space-between",
    alignItems: "center",
    paddingHorizontal: 16,
    paddingVertical: 12,
    borderBottomWidth: 1,
  },
  title: { fontSize: 16, fontWeight: "700" },
  content: { padding: 14, gap: 12, paddingBottom: 40 },
  sectionTitle: {
    fontSize: 12,
    fontWeight: "700",
    textTransform: "uppercase",
    marginTop: 4,
  },
  card: {
    borderWidth: 1,
    borderRadius: 10,
    padding: 12,
    gap: 12,
  },
  row: {
    flexDirection: "row",
    alignItems: "center",
    justifyContent: "space-between",
    gap: 10,
  },
  pills: { flexDirection: "row", gap: 6 },
  pill: { paddingHorizontal: 10, paddingVertical: 6, borderRadius: 6 },
});
