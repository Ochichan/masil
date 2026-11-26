import React, { useEffect, useMemo, useState } from "react";
import {
  ActivityIndicator,
  Modal,
  Pressable,
  ScrollView,
  StyleSheet,
  Text,
  View,
} from "react-native";
import { AlertTriangle, Check, SlidersHorizontal, X } from "lucide-react-native";
import { useTranslation } from "react-i18next";
import { useSafeAreaInsets } from "react-native-safe-area-context";
import {
  formatCacheWarmthCompact,
  shouldWarnCacheInvalidation,
} from "../agents/phase1-helpers";
import { getHostClient } from "../hosts/HostRuntimeController";
import type { AgentRow, ProviderModelsResponse } from "../protocol/schemas";
import { useMasilTheme } from "../theme/ThemeProvider";
import { FONTS } from "../theme/typography";
import { ModelPickerDropdown } from "./ModelPickerDropdown";
import {
  SelectableChip,
  formatEffortLabel,
  formatModeLabel,
} from "./SelectableChip";

interface AgentOptionsSheetProps {
  visible: boolean;
  hostId: string;
  agent: AgentRow;
  detectedModel?: string | null;
  onClose: () => void;
  onApplied: () => void;
}

function inferCurrentEffort(
  detectedModel: string | null | undefined,
  efforts: string[]
): string {
  if (efforts.length === 0) return "";
  const lower = (detectedModel ?? "").toLowerCase();
  for (const ef of [...efforts].sort((a, b) => b.length - a.length)) {
    if (lower.endsWith(`-${ef.toLowerCase()}`)) {
      return ef;
    }
  }
  return efforts.includes("high") ? "high" : efforts[0] ?? "";
}

export function AgentOptionsSheet({
  visible,
  hostId,
  agent,
  detectedModel,
  onClose,
  onApplied,
}: AgentOptionsSheetProps) {
  const { palette } = useMasilTheme();
  const { t } = useTranslation();
  const insets = useSafeAreaInsets();
  const [meta, setMeta] = useState<ProviderModelsResponse | null>(null);
  const [loading, setLoading] = useState(false);
  const [selectedModel, setSelectedModel] = useState("");
  const [selectedEffort, setSelectedEffort] = useState("");
  const [selectedMode, setSelectedMode] = useState("");
  const [busy, setBusy] = useState(false);
  const [feedback, setFeedback] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!visible) return;
    setSelectedModel("");
    setSelectedEffort("");
    setSelectedMode("");
    setFeedback(null);
    setError(null);

    const client = getHostClient(hostId);
    if (!client) return;
    let cancelled = false;
    setLoading(true);
    client
      .request<ProviderModelsResponse>("host.models.list.request", {
        provider: agent.provider,
      })
      .then((res) => {
        if (!cancelled) setMeta(res);
      })
      .catch(() => {})
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [visible, hostId, agent.provider]);

  const inferredEffort = useMemo(
    () => inferCurrentEffort(detectedModel, meta?.efforts ?? []),
    [detectedModel, meta?.efforts]
  );

  const hasLiveEffort = Boolean(
    meta && meta.supports_live_effort && meta.efforts.length > 0
  );
  const hasLiveMode = Boolean(
    meta && meta.supports_live_mode && meta.modes.length > 0
  );

  const sheetTitle = hasLiveMode
    ? t("optionsTitleModelEffortMode")
    : hasLiveEffort
      ? t("optionsTitleModelEffort")
      : t("optionsTitleModel");

  const effectiveEffort = selectedEffort || inferredEffort;
  const effortChanged =
    Boolean(selectedEffort) && selectedEffort !== inferredEffort;
  const hasChanges = Boolean(selectedModel || effortChanged || selectedMode);
  const showCacheInvalidateWarning =
    hasChanges && shouldWarnCacheInvalidation(agent.cache_warmth);
  const warmthBadge = formatCacheWarmthCompact(agent.cache_warmth);

  const apply = async () => {
    const client = getHostClient(hostId);
    if (!client) {
      setError(t("hostNotConnected"));
      return;
    }
    if (!hasChanges) return;
    setBusy(true);
    setError(null);
    setFeedback(null);
    try {
      const res = await client.request<{ applied?: string[] }>(
        "agent.configure.request",
        {
          server_socket: agent.server_socket,
          pane: agent.pane,
          target: agent.pane || agent.name,
          provider: agent.provider,
          model: selectedModel || undefined,
          effort: effortChanged ? selectedEffort : undefined,
          mode: selectedMode || undefined,
        }
      );
      const cmds = res.applied ?? [];
      setFeedback(t("optionsApplied", { commands: cmds.join(" · ") }));
      onApplied();
      setTimeout(onClose, 500);
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
              paddingBottom: Math.max(insets.bottom, 16) + 8,
            },
          ]}
        >
          <View style={[styles.grabBar, { backgroundColor: palette.track }]} />

          <View style={styles.header}>
            <View style={{ flex: 1, gap: 4 }}>
              <View style={styles.titleRow}>
                <SlidersHorizontal size={16} color={palette.accent} />
                <Text style={[styles.title, { color: palette.text }]}>
                  {sheetTitle}
                </Text>
              </View>
              <View style={styles.subtitleRow}>
                <Text
                  numberOfLines={1}
                  style={[styles.subtitleName, { color: palette.textSecondary }]}
                >
                  {agent.name}
                </Text>
                <Text style={[styles.subtitleSep, { color: palette.track }]}>·</Text>
                <Text
                  numberOfLines={1}
                  style={[styles.subtitleModel, { color: palette.muted }]}
                >
                  {detectedModel || agent.provider}
                </Text>
              </View>
            </View>
            <Pressable onPress={onClose} hitSlop={8} style={styles.closeBtn}>
              <X size={19} color={palette.muted} />
            </Pressable>
          </View>

          <View style={[styles.divider, { backgroundColor: palette.hairline }]} />

          <ScrollView contentContainerStyle={styles.body}>
            <Text style={[styles.label, { color: palette.muted }]}>
              {t("optionsSelectModel")}
            </Text>
            <ModelPickerDropdown
              models={meta?.models ?? []}
              selectedModel={selectedModel}
              currentModel={detectedModel}
              loading={loading}
              placeholder={t("optionsSelectModelPlaceholder")}
              onSelect={setSelectedModel}
            />

            {hasLiveEffort && meta ? (
              <View style={styles.section}>
                <View style={styles.sectionRow}>
                  <Text style={[styles.label, { color: palette.muted }]}>
                    {t("optionsReasoningEffort")}
                  </Text>
                  {inferredEffort ? (
                    <Text style={[styles.hintTag, { color: palette.muted }]}>
                      {t("optionsCurrentValue", {
                        value: formatEffortLabel(inferredEffort),
                      })}
                    </Text>
                  ) : null}
                </View>
                <View style={styles.chips}>
                  {meta.efforts.map((ef) => (
                    <SelectableChip
                      key={ef}
                      label={formatEffortLabel(ef)}
                      selected={effectiveEffort === ef}
                      onPress={() =>
                        setSelectedEffort(ef === inferredEffort ? "" : ef)
                      }
                    />
                  ))}
                </View>
              </View>
            ) : null}

            {hasLiveMode && meta ? (
              <View style={styles.section}>
                <Text style={[styles.label, { color: palette.muted }]}>
                  {t("optionsExecutionMode")}
                </Text>
                <View style={styles.chips}>
                  {meta.modes.map((md) => (
                    <SelectableChip
                      key={md}
                      label={formatModeLabel(md)}
                      selected={selectedMode === md}
                      onPress={() =>
                        setSelectedMode(selectedMode === md ? "" : md)
                      }
                    />
                  ))}
                </View>
              </View>
            ) : null}

            {showCacheInvalidateWarning ? (
              <View
                style={[
                  styles.cacheWarnBox,
                  {
                    backgroundColor: palette.warningSoft,
                    borderColor: palette.warning,
                  },
                ]}
              >
                <AlertTriangle size={15} color={palette.warning} />
                <Text
                  style={[styles.cacheWarnText, { color: palette.warning }]}
                >
                  {t("cacheInvalidateWarning", {
                    badge: warmthBadge ?? "",
                    tokens: Math.round(
                      (agent.cache_warmth?.est_context_tokens ?? 0) / 1000
                    ),
                  })}
                </Text>
              </View>
            ) : null}

            {feedback ? (
              <Text style={[styles.feedbackText, { color: palette.success }]}>
                {feedback}
              </Text>
            ) : null}
            {error ? (
              <Text style={[styles.feedbackText, { color: palette.danger }]}>
                {error}
              </Text>
            ) : null}

            <Pressable
              onPress={() => void apply()}
              disabled={!hasChanges || busy}
              style={[
                styles.submit,
                {
                  backgroundColor: palette.accent,
                  opacity: hasChanges ? (busy ? 0.75 : 1) : 0.36,
                },
              ]}
            >
              {busy ? (
                <ActivityIndicator size="small" color={palette.accentText} />
              ) : (
                <>
                  <Check size={16} color={palette.accentText} strokeWidth={2.5} />
                  <Text
                    style={[
                      styles.submitText,
                      { color: palette.accentText },
                    ]}
                  >
                    {t("optionsApplyLive")}
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
    backgroundColor: "rgba(4,7,11,0.72)",
    justifyContent: "flex-end",
  },
  sheet: {
    borderTopLeftRadius: 20,
    borderTopRightRadius: 20,
    borderTopWidth: 1,
    borderLeftWidth: 1,
    borderRightWidth: 1,
    paddingHorizontal: 16,
    paddingTop: 10,
    maxHeight: "82%",
  },
  grabBar: {
    width: 36,
    height: 4,
    borderRadius: 2,
    alignSelf: "center",
    marginBottom: 12,
  },
  header: {
    flexDirection: "row",
    alignItems: "flex-start",
    gap: 10,
    marginBottom: 12,
  },
  titleRow: {
    flexDirection: "row",
    alignItems: "center",
    gap: 7,
  },
  title: {
    fontFamily: FONTS.bold,
    fontSize: 16,
    letterSpacing: -0.2,
  },
  subtitleRow: {
    flexDirection: "row",
    alignItems: "center",
    gap: 5,
  },
  subtitleName: {
    fontFamily: FONTS.regular,
    fontSize: 12,
    flexShrink: 0,
  },
  subtitleSep: {
    fontFamily: FONTS.regular,
    fontSize: 11,
  },
  subtitleModel: {
    fontFamily: FONTS.mono,
    fontSize: 11.5,
    flexShrink: 1,
  },
  closeBtn: {
    width: 32,
    height: 32,
    borderRadius: 8,
    alignItems: "center",
    justifyContent: "center",
  },
  divider: {
    height: 1,
    marginHorizontal: -16,
    marginBottom: 14,
  },
  body: { gap: 10, paddingBottom: 10 },
  section: { gap: 8, marginTop: 6 },
  sectionRow: {
    flexDirection: "row",
    justifyContent: "space-between",
    alignItems: "center",
  },
  label: {
    fontFamily: FONTS.regular,
    fontSize: 11.5,
    letterSpacing: 0.2,
  },
  hintTag: {
    fontFamily: FONTS.regular,
    fontSize: 11.5,
  },
  chips: { flexDirection: "row", flexWrap: "wrap", gap: 8 },
  cacheWarnBox: {
    flexDirection: "row",
    alignItems: "flex-start",
    gap: 8,
    borderWidth: 1,
    borderRadius: 10,
    padding: 10,
    marginTop: 4,
  },
  cacheWarnText: {
    flex: 1,
    fontFamily: FONTS.semi,
    fontSize: 12,
    lineHeight: 17,
  },
  feedbackText: {
    fontFamily: FONTS.semi,
    fontSize: 12.5,
  },
  submit: {
    marginTop: 10,
    height: 46,
    borderRadius: 12,
    flexDirection: "row",
    alignItems: "center",
    justifyContent: "center",
    gap: 6,
  },
  submitText: {
    fontFamily: FONTS.bold,
    fontSize: 14,
  },
});
