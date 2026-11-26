import React, { useState } from "react";
import {
  ActivityIndicator,
  Modal,
  Pressable,
  ScrollView,
  StyleSheet,
  Text,
  TextInput,
  View,
} from "react-native";
import { Sparkles, Wand2, X } from "lucide-react-native";
import { useTranslation } from "react-i18next";
import { useSafeAreaInsets } from "react-native-safe-area-context";
import {
  getHostClient,
  useHostRuntimeStore,
} from "../hosts/HostRuntimeController";
import { useMobilePanelsStore } from "../mobile-panels/store";
import { useMasilTheme } from "../theme/ThemeProvider";
import { FONTS } from "../theme/typography";
import { SelectableChip } from "./SelectableChip";

interface PluginCreatorSheetProps {
  visible: boolean;
  hostId: string;
  onClose: () => void;
  onLaunched?: () => void;
}

const CREATOR_PROVIDERS = [
  { id: "agy", label: "Antigravity (agy)" },
  { id: "claude", label: "Claude Code" },
  { id: "codex", label: "Codex" },
] as const;

const STARTER_IDEA_KEYS = [
  {
    labelKey: "pluginIdeaTokyoLabel",
    goalKey: "pluginIdeaTokyoGoal",
  },
  {
    labelKey: "pluginIdeaPrLabel",
    goalKey: "pluginIdeaPrGoal",
  },
  {
    labelKey: "pluginIdeaStandupLabel",
    goalKey: "pluginIdeaStandupGoal",
  },
  {
    labelKey: "pluginIdeaTodoLabel",
    goalKey: "pluginIdeaTodoGoal",
  },
] as const;

export function PluginCreatorSheet({
  visible,
  hostId,
  onClose,
  onLaunched,
}: PluginCreatorSheetProps) {
  const { palette } = useMasilTheme();
  const { t } = useTranslation();
  const insets = useSafeAreaInsets();
  const refreshHost = useHostRuntimeStore((s) => s.refreshHost);
  const selectAgent = useHostRuntimeStore((s) => s.selectAgent);
  const selectPanel = useMobilePanelsStore((s) => s.selectPanel);

  const [provider, setProvider] = useState<"agy" | "claude" | "codex">("agy");
  const [goal, setGoal] = useState("");
  const [pluginId, setPluginId] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const launchCreator = async () => {
    const trimmed = goal.trim();
    if (!trimmed) {
      setError(t("pluginCreatorGoalRequired"));
      return;
    }
    const client = getHostClient(hostId);
    if (!client) {
      setError(t("hostNotConnected"));
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const res = await client.request<{
        agent_name?: string;
        started?: { pane?: string; pane_id?: string; name?: string };
      }>("plugin.creator.start.request", {
        goal: trimmed,
        provider,
        plugin_id: pluginId.trim() || undefined,
      });
      await refreshHost(hostId, true);
      setTimeout(() => {
        void useHostRuntimeStore.getState().refreshHost(hostId, false);
      }, 1500);

      const latestHost = useHostRuntimeStore.getState().hosts[hostId];
      const paneHint = res.started?.pane_id ?? res.started?.pane;
      const found = latestHost?.agents.find(
        (a) =>
          (paneHint && a.pane === paneHint) ||
          a.name === (res.started?.name ?? res.agent_name)
      );
      if (found) {
        selectAgent(hostId, found.remote_id);
        selectPanel("agent");
      }
      setGoal("");
      setPluginId("");
      onClose();
      onLaunched?.();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
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
            {
              backgroundColor: palette.panel,
              borderColor: palette.borderSubtle,
              paddingBottom: Math.max(insets.bottom, 16),
            },
          ]}
        >
          <View style={[styles.grabBar, { backgroundColor: palette.track }]} />
          <View style={styles.header}>
            <View style={styles.titleRow}>
              <Sparkles size={17} color={palette.accent} />
              <Text style={[styles.title, { color: palette.text }]}>
                {t("createPluginWithAi")}
              </Text>
            </View>
            <Pressable onPress={onClose} hitSlop={8}>
              <X size={19} color={palette.muted} />
            </Pressable>
          </View>
          <View
            style={[styles.divider, { backgroundColor: palette.hairline }]}
          />

          <ScrollView contentContainerStyle={styles.body}>
            <Text style={[styles.sub, { color: palette.textSecondary }]}>
              {t("pluginCreatorSubtitle")}
            </Text>

            <View style={styles.section}>
              <Text style={[styles.label, { color: palette.muted }]}>
                {t("pluginCreatorToolLabel")}
              </Text>
              <View style={styles.chips}>
                {CREATOR_PROVIDERS.map((p) => (
                  <SelectableChip
                    key={p.id}
                    label={p.label}
                    selected={provider === p.id}
                    onPress={() => setProvider(p.id)}
                  />
                ))}
              </View>
            </View>

            <View style={styles.section}>
              <Text style={[styles.label, { color: palette.muted }]}>
                {t("pluginCreatorQuickIdeas")}
              </Text>
              <View style={styles.chips}>
                {STARTER_IDEA_KEYS.map((idea) => {
                  const ideaLabel = t(idea.labelKey);
                  const ideaGoal = t(idea.goalKey);
                  return (
                    <SelectableChip
                      key={idea.labelKey}
                      label={ideaLabel}
                      selected={goal === ideaGoal}
                      onPress={() => setGoal(ideaGoal)}
                    />
                  );
                })}
              </View>
            </View>

            <View style={styles.section}>
              <Text style={[styles.label, { color: palette.muted }]}>
                {t("pluginCreatorGoalLabel")}
              </Text>
              <TextInput
                value={goal}
                onChangeText={setGoal}
                multiline
                placeholder={t("pluginCreatorGoalPlaceholder")}
                placeholderTextColor={palette.muted}
                style={[
                  styles.textarea,
                  {
                    backgroundColor: palette.canvas,
                    borderColor: palette.borderSubtle,
                    color: palette.text,
                  },
                ]}
              />
            </View>

            <View style={styles.section}>
              <Text style={[styles.label, { color: palette.muted }]}>
                {t("pluginCreatorIdLabel")}
              </Text>
              <TextInput
                value={pluginId}
                onChangeText={setPluginId}
                autoCapitalize="none"
                autoCorrect={false}
                placeholder={t("pluginCreatorIdPlaceholder")}
                placeholderTextColor={palette.muted}
                style={[
                  styles.input,
                  {
                    backgroundColor: palette.canvas,
                    borderColor: palette.borderSubtle,
                    color: palette.text,
                  },
                ]}
              />
            </View>

            {error ? (
              <Text style={[styles.err, { color: palette.danger }]}>
                {error}
              </Text>
            ) : null}

            <Pressable
              onPress={() => void launchCreator()}
              disabled={busy}
              style={[
                styles.launchBtn,
                {
                  backgroundColor: palette.accent,
                  opacity: busy ? 0.65 : 1,
                },
              ]}
            >
              {busy ? (
                <ActivityIndicator size="small" color={palette.accentText} />
              ) : (
                <>
                  <Wand2 size={16} color={palette.accentText} />
                  <Text style={[styles.launchText, { color: palette.accentText }]}>
                    {t("pluginCreatorLaunch")}
                  </Text>
                </>
              )}
            </Pressable>
          </ScrollView>
        </View>
      </View>
    </Modal>
  );
}

const styles = StyleSheet.create({
  backdrop: {
    flex: 1,
    backgroundColor: "rgba(0,0,0,0.58)",
    justifyContent: "flex-end",
  },
  sheet: {
    maxHeight: "88%",
    borderTopLeftRadius: 20,
    borderTopRightRadius: 20,
    borderTopWidth: 1,
  },
  grabBar: {
    width: 36,
    height: 4,
    borderRadius: 2,
    alignSelf: "center",
    marginTop: 9,
    marginBottom: 4,
  },
  header: {
    flexDirection: "row",
    alignItems: "center",
    justifyContent: "space-between",
    paddingHorizontal: 16,
    paddingVertical: 12,
  },
  titleRow: { flexDirection: "row", alignItems: "center", gap: 8 },
  title: { fontFamily: FONTS.bold, fontSize: 16 },
  divider: { height: 1 },
  body: { padding: 16, gap: 14 },
  sub: { fontFamily: FONTS.regular, fontSize: 13, lineHeight: 19 },
  section: { gap: 7 },
  label: { fontFamily: FONTS.semi, fontSize: 12 },
  chips: { flexDirection: "row", flexWrap: "wrap", gap: 7 },
  textarea: {
    minHeight: 88,
    borderWidth: 1,
    borderRadius: 11,
    paddingHorizontal: 12,
    paddingVertical: 10,
    fontFamily: FONTS.regular,
    fontSize: 14,
    lineHeight: 20,
    textAlignVertical: "top",
  },
  input: {
    borderWidth: 1,
    borderRadius: 10,
    paddingHorizontal: 12,
    paddingVertical: 9,
    fontFamily: FONTS.mono,
    fontSize: 13,
  },
  err: { fontFamily: FONTS.regular, fontSize: 12.5 },
  launchBtn: {
    height: 44,
    borderRadius: 11,
    flexDirection: "row",
    alignItems: "center",
    justifyContent: "center",
    gap: 8,
    marginTop: 4,
  },
  launchText: { fontFamily: FONTS.bold, fontSize: 14.5 },
});
