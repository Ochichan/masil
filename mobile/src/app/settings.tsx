import React, { useEffect, useState } from "react";
import {
  Modal,
  Pressable,
  ScrollView,
  StyleSheet,
  Switch,
  Text,
  View,
} from "react-native";
import { useRouter } from "expo-router";
import { Bell, RefreshCw, Sparkles, Terminal, X } from "lucide-react-native";
import { useTranslation } from "react-i18next";
import { SafeAreaView } from "react-native-safe-area-context";
import { PluginCreatorSheet } from "../components/PluginCreatorSheet";
import {
  getHostClient,
  useHostRuntimeStore,
} from "../hosts/HostRuntimeController";
import {
  type NotificationPermissionState,
  getNotificationPermissionStatus,
  registerPushTokenWithHost,
  requestNotificationPermissions,
  sendTestNotification,
} from "../notifications/push-manager";
import {
  classifyEndpointRoute,
  displayEndpointAddress,
  normalizeDirectAddrList,
} from "../protocol/endpoints";
import type { PluginCatalog } from "../protocol/schemas";
import { useMasilTheme } from "../theme/ThemeProvider";
import { FONTS } from "../theme/typography";

export default function SettingsScreen() {
  const {
    palette,
    preference,
    activeThemeId,
    availablePluginThemes,
    setPreference,
    setLocalPluginTheme,
  } = useMasilTheme();
  const { t, i18n } = useTranslation();
  const router = useRouter();

  const hostOrder = useHostRuntimeStore((s) => s.hostOrder);
  const hosts = useHostRuntimeStore((s) => s.hosts);
  const activeHostId = useHostRuntimeStore((s) => s.activeHostId);
  const removeHost = useHostRuntimeStore((s) => s.removeHost);
  const refreshHost = useHostRuntimeStore((s) => s.refreshHost);
  const setHostPlugins = useHostRuntimeStore((s) => s.setHostPlugins);
  const voiceMode = useHostRuntimeStore((s) => s.voiceMode);
  const setVoiceMode = useHostRuntimeStore((s) => s.setVoiceMode);
  const notificationPrefs = useHostRuntimeStore((s) => s.notificationPrefs);
  const setNotificationPrefs = useHostRuntimeStore(
    (s) => s.setNotificationPrefs
  );
  const livePulsePrefs = useHostRuntimeStore((s) => s.livePulsePrefs);
  const setLivePulsePrefs = useHostRuntimeStore((s) => s.setLivePulsePrefs);

  const [showCreator, setShowCreator] = useState(false);
  const [logModal, setLogModal] = useState<{
    pluginId: string;
    lines: string[];
  } | null>(null);
  const [busyPlugin, setBusyPlugin] = useState<string | null>(null);
  const [pluginNotice, setPluginNotice] = useState<string | null>(null);
  const [notifPerm, setNotifPerm] =
    useState<NotificationPermissionState>("undetermined");
  const [notifNotice, setNotifNotice] = useState<{
    text: string;
    isError?: boolean;
  } | null>(null);

  useEffect(() => {
    void getNotificationPermissionStatus().then(setNotifPerm);
  }, []);

  const handleRequestNotifPermission = async () => {
    const granted = await requestNotificationPermissions();
    const status = await getNotificationPermissionStatus();
    setNotifPerm(status);
    if (granted && activeHostId) {
      void registerPushTokenWithHost(activeHostId, {
        promptIfUndetermined: false,
      });
    }
  };

  const handleSendTestNotification = async () => {
    setNotifNotice(null);
    const sent = await sendTestNotification(i18n.language || "ko");
    const status = await getNotificationPermissionStatus();
    setNotifPerm(status);
    setNotifNotice({
      text: sent ? t("testNotificationSent") : t("testNotificationFailed"),
      isError: !sent,
    });
  };

  const handleRegisterPushToken = async () => {
    if (!activeHostId) return;
    setNotifNotice(null);
    const res = await registerPushTokenWithHost(activeHostId, {
      promptIfUndetermined: true,
    });
    const status = await getNotificationPermissionStatus();
    setNotifPerm(status);
    setNotifNotice({
      text: res.ok ? t("pushTokenRegistered") : t("pushTokenFallback"),
      isError: !res.ok && res.reason === "permission_denied",
    });
  };

  const activeHost = activeHostId ? hosts[activeHostId] : undefined;
  const plugins = activeHost?.plugins ?? null;

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

  const selectTheme = async (themeId: string) => {
    if (themeId === "dark" || themeId === "light" || themeId === "system") {
      setPreference(themeId);
    } else {
      setLocalPluginTheme(themeId);
    }
    if (!activeHostId) return;
    const client = getHostClient(activeHostId);
    if (!client) return;
    try {
      const next = await client.request<PluginCatalog>(
        "plugin.theme.set.request",
        { theme_id: themeId }
      );
      setHostPlugins(activeHostId, next);
    } catch {
      // Keep local theme choice even if host is momentarily offline.
    }
  };

  const togglePlugin = async (pluginId: string, enabled: boolean) => {
    if (!activeHostId) return;
    const client = getHostClient(activeHostId);
    if (!client) return;
    setBusyPlugin(pluginId);
    setPluginNotice(null);
    try {
      const next = await client.request<PluginCatalog>(
        "plugin.toggle.request",
        { plugin_id: pluginId, enabled }
      );
      setHostPlugins(activeHostId, next);
    } catch (e) {
      setPluginNotice(e instanceof Error ? e.message : String(e));
    } finally {
      setBusyPlugin(null);
    }
  };

  const reloadPlugins = async () => {
    if (!activeHostId) return;
    const client = getHostClient(activeHostId);
    if (!client) return;
    setBusyPlugin("__reload__");
    setPluginNotice(null);
    try {
      const next = await client.request<PluginCatalog>(
        "plugin.reload.request"
      );
      setHostPlugins(activeHostId, next);
    } catch (e) {
      setPluginNotice(e instanceof Error ? e.message : String(e));
    } finally {
      setBusyPlugin(null);
    }
  };

  const openLogs = async (pluginId: string) => {
    if (!activeHostId) return;
    const client = getHostClient(activeHostId);
    if (!client) return;
    try {
      const res = await client.request<{ lines?: string[] }>(
        "plugin.logs.request",
        { plugin_id: pluginId, tail: 80 }
      );
      setLogModal({ pluginId, lines: res.lines ?? [] });
    } catch (e) {
      setLogModal({
        pluginId,
        lines: [e instanceof Error ? e.message : String(e)],
      });
    }
  };

  const toggleSkill = async (target: string, enabled: boolean) => {
    if (!activeHostId) return;
    const client = getHostClient(activeHostId);
    if (!client) return;
    setPluginNotice(null);
    try {
      await client.request("plugin.skill.set.request", { target, enabled });
      await useHostRuntimeStore.getState().refreshPlugins(activeHostId);
    } catch (e) {
      setPluginNotice(e instanceof Error ? e.message : String(e));
    }
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
                const active =
                  (!activeThemeId ||
                    activeThemeId === "dark" ||
                    activeThemeId === "light" ||
                    activeThemeId === "system") &&
                  preference === th;
                return (
                  <Pressable
                    key={th}
                    onPress={() => void selectTheme(th)}
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

          {availablePluginThemes.length > 0 ? (
            <View style={{ gap: 8 }}>
              <Text style={{ color: palette.muted, fontSize: 11.5 }}>
                Plugin Themes (syncs live with TUI)
              </Text>
              <View style={styles.themeGrid}>
                {availablePluginThemes.map((th) => {
                  const active = activeThemeId === th.id;
                  return (
                    <Pressable
                      key={th.id}
                      onPress={() => void selectTheme(th.id)}
                      style={[
                        styles.themeChip,
                        {
                          backgroundColor: th.panel,
                          borderColor: active ? palette.accent : palette.track,
                          borderWidth: active ? 2 : 1,
                        },
                      ]}
                    >
                      <View
                        style={[
                          styles.swatchDot,
                          { backgroundColor: th.accent },
                        ]}
                      />
                      <Text
                        numberOfLines={1}
                        style={{
                          color: th.text,
                          fontFamily: FONTS.semi,
                          fontSize: 12,
                        }}
                      >
                        {th.name}
                      </Text>
                    </Pressable>
                  );
                })}
              </View>
            </View>
          ) : null}
        </View>

        {/* Plugins & Creator Mode */}
        {activeHost ? (
          <>
            <View style={styles.sectionHeaderRow}>
              <Text style={[styles.sectionTitle, { color: palette.muted }]}>
                Plugins ({plugins?.plugins.length ?? 0})
              </Text>
              <Pressable
                onPress={() => void reloadPlugins()}
                disabled={busyPlugin === "__reload__"}
                style={styles.inlineAction}
              >
                <RefreshCw size={12} color={palette.accent} />
                <Text
                  style={{
                    color: palette.accent,
                    fontFamily: FONTS.semi,
                    fontSize: 12,
                  }}
                >
                  Reload
                </Text>
              </Pressable>
            </View>
            <View
              style={[
                styles.card,
                { backgroundColor: palette.panel, borderColor: palette.track },
              ]}
            >
              <Pressable
                onPress={() => setShowCreator(true)}
                style={[
                  styles.creatorCta,
                  { backgroundColor: palette.accent },
                ]}
              >
                <Sparkles size={15} color={palette.accentText} />
                <Text
                  style={{
                    color: palette.accentText,
                    fontFamily: FONTS.bold,
                    fontSize: 13.5,
                  }}
                >
                  {t("createPluginWithAi")}
                </Text>
              </Pressable>

              {pluginNotice ? (
                <Text
                  style={{
                    color: palette.danger,
                    fontFamily: FONTS.mono,
                    fontSize: 11.5,
                  }}
                >
                  {pluginNotice}
                </Text>
              ) : null}
              {plugins && plugins.plugins.length > 0 ? (
                plugins.plugins.map((p) => {
                  const statusColor =
                    p.status === "running"
                      ? palette.success
                      : p.status === "error"
                        ? palette.danger
                        : palette.muted;
                  const badges: string[] = [];
                  const themeCount = p.manifest?.theme_count ?? 0;
                  const panelCount = p.manifest?.panel_count ?? 0;
                  const cmdCount = p.manifest?.command_count ?? 0;
                  const hookCount = p.manifest?.hook_count ?? 0;
                  const presetCount = p.manifest?.preset_count ?? 0;
                  if (themeCount > 0) badges.push(t("badgeThemes", { count: themeCount }));
                  if (panelCount > 0) badges.push(t("badgePanels", { count: panelCount }));
                  if (cmdCount > 0) badges.push(t("badgeCommands", { count: cmdCount }));
                  if (hookCount > 0) badges.push(t("badgeHooks", { count: hookCount }));
                  if (presetCount > 0) badges.push(t("badgePresets", { count: presetCount }));
                  return (
                    <View
                      key={p.id}
                      style={[
                        styles.pluginItem,
                        { borderTopColor: palette.hairline },
                      ]}
                    >
                      <View style={styles.row}>
                        <View style={{ flex: 1, gap: 2 }}>
                          <View style={styles.pluginNameRow}>
                            <Text
                              style={{
                                color: palette.text,
                                fontFamily: FONTS.semi,
                                fontSize: 14,
                              }}
                            >
                              {p.manifest?.name || p.id}
                            </Text>
                            <Text
                              style={{
                                color: statusColor,
                                fontFamily: FONTS.mono,
                                fontSize: 11,
                              }}
                            >
                              {p.status}
                            </Text>
                          </View>
                          <Text
                            numberOfLines={2}
                            style={{ color: palette.muted, fontSize: 11.5 }}
                          >
                            {p.manifest?.description || p.id}
                          </Text>
                          {badges.length > 0 ? (
                            <Text
                              numberOfLines={1}
                              style={{
                                color: palette.accent,
                                fontFamily: FONTS.mono,
                                fontSize: 10.5,
                              }}
                            >
                              {badges.join(" · ")}
                            </Text>
                          ) : null}
                        </View>
                        <Pressable
                          onPress={() => void openLogs(p.id)}
                          hitSlop={8}
                          style={[
                            styles.logBtn,
                            { backgroundColor: palette.raised },
                          ]}
                        >
                          <Terminal size={13} color={palette.textSecondary} />
                        </Pressable>
                        <Switch
                          value={p.enabled}
                          disabled={busyPlugin === p.id}
                          onValueChange={(v) => void togglePlugin(p.id, v)}
                          trackColor={{
                            false: palette.track,
                            true: palette.accent,
                          }}
                        />
                      </View>
                      {p.errors.length > 0 ? (
                        <Text
                          style={{
                            color: palette.danger,
                            fontFamily: FONTS.mono,
                            fontSize: 11,
                          }}
                        >
                          {p.errors.join(" · ")}
                        </Text>
                      ) : null}
                    </View>
                  );
                })
              ) : (
                <Text style={{ color: palette.muted, fontSize: 12.5 }}>
                  {t("noPluginsInstalledHint")}
                </Text>
              )}
            </View>

            {/* AI CLI Plugin Skills */}
            <Text style={[styles.sectionTitle, { color: palette.muted }]}>
              AI Plugin Skills (Claude Code · Codex · Antigravity)
            </Text>
            <View
              style={[
                styles.card,
                { backgroundColor: palette.panel, borderColor: palette.track },
              ]}
            >
              <Text style={{ color: palette.muted, fontSize: 12 }}>
                {t("pluginSkillsHint")}
              </Text>
              {(plugins?.skills ?? []).map((sk) => (
                <View key={sk.id} style={styles.row}>
                  <View style={{ flex: 1 }}>
                    <Text style={{ color: palette.text, fontWeight: "600" }}>
                      {sk.label}
                    </Text>
                    <Text style={{ color: palette.muted, fontSize: 11 }}>
                      {sk.cli_installed ? "CLI detected" : "CLI not in PATH"} ·{" "}
                      {sk.skill_installed ? "Skill installed" : "Not installed"}
                    </Text>
                  </View>
                  <Switch
                    value={sk.skill_installed}
                    onValueChange={(v) => void toggleSkill(sk.id, v)}
                    trackColor={{ false: palette.track, true: palette.accent }}
                  />
                </View>
              ))}
            </View>
          </>
        ) : null}

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

        {/* Phone Notifications */}
        <Text style={[styles.sectionTitle, { color: palette.muted }]}>
          {t("notifications")}
        </Text>
        <View
          style={[
            styles.card,
            { backgroundColor: palette.panel, borderColor: palette.track },
          ]}
        >
          <Text style={{ color: palette.muted, fontSize: 12, lineHeight: 17 }}>
            {t("notificationsDesc")}
          </Text>

          <View style={styles.row}>
            <View style={{ flex: 1, flexDirection: "row", alignItems: "center", gap: 7 }}>
              <Bell
                size={14}
                color={
                  notifPerm === "granted" ? palette.success : palette.warning
                }
              />
              <Text
                style={{
                  color:
                    notifPerm === "granted" ? palette.success : palette.warning,
                  fontFamily: FONTS.semi,
                  fontSize: 12,
                  flexShrink: 1,
                }}
              >
                {notifPerm === "granted"
                  ? t("notificationPermissionGranted")
                  : notifPerm === "denied"
                    ? t("notificationPermissionDenied")
                    : t("notificationPermissionUndetermined")}
              </Text>
            </View>
            {notifPerm !== "granted" ? (
              <Pressable
                onPress={() => void handleRequestNotifPermission()}
                style={[styles.pill, { backgroundColor: palette.accent }]}
              >
                <Text
                  style={{
                    color: palette.accentText,
                    fontWeight: "700",
                    fontSize: 12,
                  }}
                >
                  {t("allowNotifications")}
                </Text>
              </Pressable>
            ) : null}
          </View>

          <View style={styles.row}>
            <View style={{ flex: 1 }}>
              <Text style={{ color: palette.text, fontWeight: "600" }}>
                {t("notificationsEnabled")}
              </Text>
            </View>
            <Switch
              value={notificationPrefs.enabled}
              onValueChange={(v) => {
                setNotificationPrefs({ enabled: v });
                if (v && notifPerm !== "granted") {
                  void handleRequestNotifPermission();
                }
              }}
              trackColor={{ false: palette.track, true: palette.accent }}
            />
          </View>

          {notificationPrefs.enabled ? (
            <>
              <View style={styles.row}>
                <View style={{ flex: 1, gap: 2 }}>
                  <Text style={{ color: palette.text, fontWeight: "600" }}>
                    {t("notifyInputNeeded")}
                  </Text>
                  <Text style={{ color: palette.muted, fontSize: 11 }}>
                    {t("notifyInputNeededDesc")}
                  </Text>
                </View>
                <Switch
                  value={notificationPrefs.notifyInputNeeded}
                  onValueChange={(v) =>
                    setNotificationPrefs({ notifyInputNeeded: v })
                  }
                  trackColor={{ false: palette.track, true: palette.accent }}
                />
              </View>

              <View style={styles.row}>
                <View style={{ flex: 1, gap: 2 }}>
                  <Text style={{ color: palette.text, fontWeight: "600" }}>
                    {t("notifyTaskCompleted")}
                  </Text>
                  <Text style={{ color: palette.muted, fontSize: 11 }}>
                    {t("notifyTaskCompletedDesc")}
                  </Text>
                </View>
                <Switch
                  value={notificationPrefs.notifyTaskCompleted}
                  onValueChange={(v) =>
                    setNotificationPrefs({ notifyTaskCompleted: v })
                  }
                  trackColor={{ false: palette.track, true: palette.accent }}
                />
              </View>

              <View style={styles.row}>
                <View style={{ flex: 1, gap: 2 }}>
                  <Text style={{ color: palette.text, fontWeight: "600" }}>
                    {t("notifyActiveAgent")}
                  </Text>
                  <Text style={{ color: palette.muted, fontSize: 11 }}>
                    {t("notifyActiveAgentDesc")}
                  </Text>
                </View>
                <Switch
                  value={notificationPrefs.notifyActiveAgent}
                  onValueChange={(v) =>
                    setNotificationPrefs({ notifyActiveAgent: v })
                  }
                  trackColor={{ false: palette.track, true: palette.accent }}
                />
              </View>
            </>
          ) : null}

          <View style={styles.pills}>
            <Pressable
              onPress={() => void handleSendTestNotification()}
              style={[styles.pill, { backgroundColor: palette.raised }]}
            >
              <Text
                style={{
                  color: palette.accent,
                  fontWeight: "700",
                  fontSize: 12,
                }}
              >
                {t("sendTestNotification")}
              </Text>
            </Pressable>
          </View>

          {notifNotice ? (
            <Text
              style={{
                color: notifNotice.isError ? palette.danger : palette.textSecondary,
                fontSize: 11.5,
              }}
            >
              {notifNotice.text}
            </Text>
          ) : null}
        </View>

        {/* Lock Screen Fleet Pulse (Phase 2B / D-09 Strict Opt-In) */}
        <Text style={[styles.sectionTitle, { color: palette.muted }]}>
          {t("livePulseSection")}
        </Text>
        <View
          style={[
            styles.card,
            { backgroundColor: palette.panel, borderColor: palette.track },
          ]}
        >
          <Text style={{ color: palette.muted, fontSize: 12, lineHeight: 17 }}>
            {t("livePulseDesc")}
          </Text>

          <View style={styles.row}>
            <View style={{ flex: 1 }}>
              <Text style={{ color: palette.text, fontWeight: "600" }}>
                {t("livePulseEnabled")}
              </Text>
            </View>
            <Switch
              value={livePulsePrefs.enabled}
              onValueChange={(v) => void setLivePulsePrefs({ enabled: v })}
              trackColor={{ false: palette.track, true: palette.accent }}
            />
          </View>

          {livePulsePrefs.enabled ? (
            <View style={styles.row}>
              <View style={{ flex: 1, gap: 2 }}>
                <Text style={{ color: palette.text, fontWeight: "600" }}>
                  {t("livePulseSummaryEnabled")}
                </Text>
                <Text style={{ color: palette.muted, fontSize: 11 }}>
                  {t("livePulseSummaryDesc")}
                </Text>
              </View>
              <Switch
                value={livePulsePrefs.summaryEnabled}
                onValueChange={(v) =>
                  void setLivePulsePrefs({ summaryEnabled: v })
                }
                trackColor={{ false: palette.track, true: palette.accent }}
              />
            </View>
          ) : null}
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
                onPress={() => void handleRegisterPushToken()}
                style={[
                  styles.pill,
                  { backgroundColor: palette.raised, alignSelf: "flex-start" },
                ]}
              >
                <Text
                  style={{
                    color: palette.accent,
                    fontWeight: "700",
                    fontSize: 12,
                  }}
                >
                  {t("registerPushToken")}
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
            const directList = normalizeDirectAddrList([
              h.host.directAddr,
              ...(h.host.directAddrs ?? []),
            ]);
            const routeParts = [
              ...directList,
              ...(h.host.relayUrl ? [h.host.relayUrl] : []),
            ];
            const activeRoute = h.activeEndpoint
              ? classifyEndpointRoute(
                  h.activeEndpoint,
                  h.host.relayUrl,
                  h.host.hostId
                )
              : null;
            const activeRouteLabel =
              activeRoute === "lan"
                ? t("routeLan")
                : activeRoute === "tailscale"
                  ? t("routeTailscale")
                  : activeRoute === "relay"
                    ? t("routeRelay")
                    : activeRoute === "direct"
                      ? t("routeDirect")
                      : null;
            return (
              <View key={hid} style={styles.row}>
                <View style={{ flex: 1, gap: 2 }}>
                  <View style={styles.pluginNameRow}>
                    <Text style={{ color: palette.text, fontWeight: "700" }}>
                      {h.host.hostName}
                    </Text>
                    {activeRouteLabel && h.connection === "connected" ? (
                      <Text
                        style={{
                          color:
                            activeRoute === "tailscale"
                              ? palette.accent
                              : palette.success,
                          fontFamily: FONTS.monoSemi,
                          fontSize: 10.5,
                        }}
                      >
                        {activeRouteLabel}
                      </Text>
                    ) : null}
                  </View>
                  <Text style={{ color: palette.muted, fontSize: 11 }}>
                    {h.activeEndpoint && h.connection === "connected"
                      ? `${displayEndpointAddress(h.activeEndpoint)} · ${h.connection}`
                      : `${routeParts.join(" → ")} · ${h.connection}`}
                  </Text>
                  {routeParts.length > 1 ? (
                    <Text
                      numberOfLines={2}
                      style={{
                        color: palette.muted,
                        fontFamily: FONTS.mono,
                        fontSize: 10,
                      }}
                    >
                      {routeParts.join(" → ")}
                    </Text>
                  ) : null}
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

      {activeHostId ? (
        <PluginCreatorSheet
          visible={showCreator}
          hostId={activeHostId}
          onClose={() => setShowCreator(false)}
          onLaunched={() => router.back()}
        />
      ) : null}

      <Modal
        visible={Boolean(logModal)}
        transparent
        animationType="slide"
        onRequestClose={() => setLogModal(null)}
      >
        <View style={styles.logBackdrop}>
          <View
            style={[
              styles.logSheet,
              {
                backgroundColor: palette.panel,
                borderColor: palette.borderSubtle,
              },
            ]}
          >
            <View style={styles.row}>
              <Text
                style={{
                  color: palette.text,
                  fontFamily: FONTS.bold,
                  fontSize: 15,
                }}
              >
                Logs · {logModal?.pluginId}
              </Text>
              <Pressable onPress={() => setLogModal(null)} hitSlop={8}>
                <X size={18} color={palette.muted} />
              </Pressable>
            </View>
            <ScrollView style={{ maxHeight: 360 }}>
              <Text
                selectable
                style={{
                  color: palette.textSecondary,
                  fontFamily: FONTS.mono,
                  fontSize: 11.5,
                  lineHeight: 17,
                }}
              >
                {logModal && logModal.lines.length > 0
                  ? logModal.lines.join("\n")
                  : "(no logs recorded yet)"}
              </Text>
            </ScrollView>
          </View>
        </View>
      </Modal>
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
  sectionHeaderRow: {
    flexDirection: "row",
    alignItems: "center",
    justifyContent: "space-between",
    marginTop: 4,
  },
  inlineAction: {
    flexDirection: "row",
    alignItems: "center",
    gap: 5,
  },
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
  themeGrid: { flexDirection: "row", flexWrap: "wrap", gap: 8 },
  themeChip: {
    flexDirection: "row",
    alignItems: "center",
    gap: 7,
    paddingHorizontal: 10,
    paddingVertical: 7,
    borderRadius: 8,
  },
  swatchDot: { width: 10, height: 10, borderRadius: 5 },
  creatorCta: {
    height: 40,
    borderRadius: 9,
    flexDirection: "row",
    alignItems: "center",
    justifyContent: "center",
    gap: 7,
  },
  pluginItem: {
    borderTopWidth: 1,
    paddingTop: 10,
    gap: 6,
  },
  pluginNameRow: {
    flexDirection: "row",
    alignItems: "center",
    gap: 8,
  },
  logBtn: {
    width: 30,
    height: 30,
    borderRadius: 7,
    alignItems: "center",
    justifyContent: "center",
  },
  logBackdrop: {
    flex: 1,
    backgroundColor: "rgba(0,0,0,0.58)",
    justifyContent: "flex-end",
  },
  logSheet: {
    borderTopLeftRadius: 18,
    borderTopRightRadius: 18,
    borderTopWidth: 1,
    padding: 16,
    gap: 12,
  },
});
