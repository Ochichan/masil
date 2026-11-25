import React, { useEffect, useMemo, useState } from "react";
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
  ArrowUp,
  ListPlus,
  Mic,
  MicOff,
  Paperclip,
  Square,
  Terminal,
  X,
} from "lucide-react-native";
import { useTranslation } from "react-i18next";
import { useHostRuntimeStore } from "../hosts/HostRuntimeController";
import type { PluginCommandDef } from "../protocol/schemas";
import { useMasilTheme } from "../theme/ThemeProvider";
import { FONTS } from "../theme/typography";
import { useVoiceDictation } from "../voice/useVoiceDictation";

interface MobileComposerProps {
  hostId: string;
  agentName: string;
  isWorking: boolean;
  pluginCommands?: PluginCommandDef[];
  draftOverride?: string | null;
  onDraftOverrideConsumed?: () => void;
  onSend: (text: string) => Promise<void>;
  onPluginCommand?: (cmd: PluginCommandDef, args: string) => Promise<void>;
  onQueueAdd: (text: string) => Promise<void>;
  onInterrupt: () => Promise<void>;
}

export function MobileComposer({
  hostId,
  agentName,
  isWorking,
  pluginCommands = [],
  draftOverride,
  onDraftOverrideConsumed,
  onSend,
  onPluginCommand,
  onQueueAdd,
  onInterrupt,
}: MobileComposerProps) {
  const { palette } = useMasilTheme();
  const { t, i18n } = useTranslation();
  const [draft, setDraft] = useState("");
  const [busy, setBusy] = useState(false);
  const attachment = useHostRuntimeStore((s) => s.composerAttachment);
  const setAttachment = useHostRuntimeStore((s) => s.setComposerAttachment);

  useEffect(() => {
    if (draftOverride) {
      setDraft(draftOverride);
      onDraftOverrideConsumed?.();
    }
  }, [draftOverride, onDraftOverrideConsumed]);

  const matchingSlashCommands = useMemo(() => {
    if (!pluginCommands.length) return [];
    const trimmed = draft.trimStart();
    if (!trimmed.startsWith("/") || trimmed.includes("\n")) return [];
    const token = trimmed.slice(1).split(/\s+/)[0]?.toLowerCase() ?? "";
    if (trimmed.includes(" ")) return [];
    return pluginCommands
      .filter(
        (c) =>
          !token ||
          c.slash.toLowerCase().startsWith(token) ||
          c.title.toLowerCase().includes(token)
      )
      .slice(0, 6);
  }, [draft, pluginCommands]);

  const lang = i18n.language.startsWith("ko") ? "ko" : "en";
  const voice = useVoiceDictation(hostId, agentName, lang, (spoken) => {
    setDraft((prev) => (prev ? `${prev} ${spoken}` : spoken));
  });

  const hasText = draft.trim().length > 0;

  const composeFinalText = (): string => {
    const body = draft.trim();
    if (!body) return "";
    return attachment ? `${body}\n\nFile: ${attachment}` : body;
  };

  const handleSend = async () => {
    const text = composeFinalText();
    if (!text || busy) return;
    if (!attachment && onPluginCommand && text.startsWith("/")) {
      const firstSpace = text.indexOf(" ");
      const cmdToken = (
        firstSpace === -1 ? text.slice(1) : text.slice(1, firstSpace)
      ).toLowerCase();
      const args = firstSpace === -1 ? "" : text.slice(firstSpace + 1).trim();
      const matched = pluginCommands.find(
        (c) => c.slash.toLowerCase() === cmdToken
      );
      if (matched) {
        setBusy(true);
        try {
          await onPluginCommand(matched, args);
          setDraft("");
        } finally {
          setBusy(false);
        }
        return;
      }
    }
    setBusy(true);
    try {
      await onSend(text);
      setDraft("");
      setAttachment(null);
    } finally {
      setBusy(false);
    }
  };

  const handleSelectSlash = async (cmd: PluginCommandDef) => {
    if (cmd.argument_hint) {
      setDraft(`/${cmd.slash} `);
      return;
    }
    if (onPluginCommand && !busy) {
      setBusy(true);
      try {
        await onPluginCommand(cmd, "");
        setDraft("");
      } finally {
        setBusy(false);
      }
    }
  };

  const handleQueue = async () => {
    const text = composeFinalText();
    if (!text || busy) return;
    setBusy(true);
    try {
      await onQueueAdd(text);
      setDraft("");
      setAttachment(null);
    } finally {
      setBusy(false);
    }
  };

  return (
    <View
      style={[
        styles.wrapper,
        { backgroundColor: palette.canvas, borderTopColor: palette.hairline },
      ]}
    >
      {isWorking ? (
        <View style={styles.workingBar}>
          <View
            style={[
              styles.spinnerRing,
              { borderColor: palette.borderSubtle },
            ]}
          >
            <ActivityIndicator size="small" color={palette.success} />
          </View>
          <Text style={[styles.workingLabel, { color: palette.textSecondary }]}>
            작업 중
          </Text>
        </View>
      ) : null}

      {attachment ? (
        <View
          style={[
            styles.chip,
            { backgroundColor: palette.raised, borderColor: palette.borderSubtle },
          ]}
        >
          <Paperclip size={12} color={palette.accent} />
          <Text
            numberOfLines={1}
            style={[styles.chipText, { color: palette.text }]}
          >
            {attachment}
          </Text>
          <Pressable onPress={() => setAttachment(null)} hitSlop={8}>
            <X size={13} color={palette.muted} />
          </Pressable>
        </View>
      ) : null}

      {matchingSlashCommands.length > 0 ? (
        <View
          style={[
            styles.slashBox,
            {
              backgroundColor: palette.panel,
              borderColor: palette.borderSubtle,
            },
          ]}
        >
          <ScrollView keyboardShouldPersistTaps="handled" style={{ maxHeight: 190 }}>
            {matchingSlashCommands.map((cmd, idx) => (
              <Pressable
                key={`${cmd.plugin_id}:${cmd.id}`}
                onPress={() => void handleSelectSlash(cmd)}
                style={[
                  styles.slashRow,
                  idx > 0 && {
                    borderTopWidth: 1,
                    borderTopColor: palette.hairline,
                  },
                ]}
              >
                <Terminal size={13} color={palette.accent} />
                <View style={{ flex: 1, minWidth: 0 }}>
                  <View style={styles.slashTop}>
                    <Text style={[styles.slashName, { color: palette.accent }]}>
                      /{cmd.slash}
                      {cmd.argument_hint ? ` ${cmd.argument_hint}` : ""}
                    </Text>
                    <Text
                      numberOfLines={1}
                      style={[styles.slashPlugin, { color: palette.muted }]}
                    >
                      {cmd.plugin_id}
                    </Text>
                  </View>
                  <Text
                    numberOfLines={1}
                    style={[styles.slashDesc, { color: palette.textSecondary }]}
                  >
                    {cmd.description || cmd.title}
                  </Text>
                </View>
              </Pressable>
            ))}
          </ScrollView>
        </View>
      ) : null}

      {voice.error ? (
        <Text style={[styles.voiceError, { color: palette.danger }]}>
          {voice.error}
        </Text>
      ) : null}

      <View
        style={[
          styles.composerBox,
          {
            backgroundColor: palette.panel,
            borderColor: palette.hairline,
          },
        ]}
      >
        <Pressable
          onPress={() => void (voice.recording ? voice.stop() : voice.start())}
          style={[
            styles.ghostBtn,
            voice.recording && { backgroundColor: palette.danger },
          ]}
          accessibilityLabel={
            voice.recording ? t("voiceStop") : t("voiceStart")
          }
        >
          {voice.transcribing ? (
            <ActivityIndicator size="small" color={palette.accent} />
          ) : voice.recording ? (
            <MicOff size={17} color="#ffffff" />
          ) : (
            <Mic size={17} color={palette.muted} />
          )}
        </Pressable>

        <View style={styles.inputWrap}>
          {draft.length === 0 ? (
            <Text
              pointerEvents="none"
              numberOfLines={1}
              style={[styles.placeholder, { color: palette.muted }]}
            >
              {t("composerPlaceholder")}
            </Text>
          ) : null}
          <TextInput
            value={draft}
            onChangeText={setDraft}
            multiline
            maxLength={16000}
            style={[styles.input, { color: palette.text }]}
          />
        </View>

        {hasText ? (
          <Pressable
            onPress={() => void handleQueue()}
            disabled={busy}
            style={[styles.ghostBtn, { backgroundColor: palette.raised }]}
            accessibilityLabel={t("queue")}
          >
            <ListPlus size={16} color={palette.text} />
          </Pressable>
        ) : null}

        {isWorking && !hasText ? (
          <Pressable
            onPress={() => void onInterrupt()}
            style={[
              styles.primaryBtn,
              {
                backgroundColor: palette.raised,
                borderWidth: 1,
                borderColor: palette.borderSubtle,
              },
            ]}
            accessibilityLabel={t("interrupt")}
          >
            <Square size={12} color="#ffffff" fill="#ffffff" />
          </Pressable>
        ) : (
          <Pressable
            onPress={() => void handleSend()}
            disabled={!hasText || busy}
            style={[
              styles.primaryBtn,
              {
                backgroundColor: hasText ? palette.accent : palette.disabledBg,
                borderWidth: hasText ? 0 : 1,
                borderColor: palette.borderSubtle,
              },
            ]}
            accessibilityLabel={t("send")}
          >
            {busy ? (
              <ActivityIndicator size="small" color={palette.accentText} />
            ) : (
              <ArrowUp
                size={17}
                color={hasText ? palette.accentText : palette.disabledText}
                strokeWidth={2.4}
              />
            )}
          </Pressable>
        )}
      </View>
    </View>
  );
}

const styles = StyleSheet.create({
  wrapper: {
    borderTopWidth: 1,
    paddingHorizontal: 12,
    paddingTop: 8,
    paddingBottom: 10,
    gap: 8,
  },
  workingBar: {
    flexDirection: "row",
    alignItems: "center",
    gap: 8,
    paddingHorizontal: 4,
  },
  spinnerRing: {
    width: 22,
    height: 22,
    borderRadius: 11,
    borderWidth: 1.5,
    alignItems: "center",
    justifyContent: "center",
  },
  workingLabel: {
    fontFamily: FONTS.semi,
    fontSize: 12.5,
  },
  chip: {
    flexDirection: "row",
    alignItems: "center",
    alignSelf: "flex-start",
    gap: 6,
    borderWidth: 1,
    borderRadius: 8,
    paddingHorizontal: 9,
    paddingVertical: 5,
    maxWidth: "100%",
  },
  chipText: {
    fontFamily: FONTS.mono,
    fontSize: 11.5,
    flexShrink: 1,
  },
  voiceError: {
    fontFamily: FONTS.regular,
    fontSize: 11.5,
    paddingHorizontal: 4,
  },
  slashBox: {
    borderWidth: 1,
    borderRadius: 12,
    overflow: "hidden",
  },
  slashRow: {
    flexDirection: "row",
    alignItems: "center",
    gap: 9,
    paddingHorizontal: 11,
    paddingVertical: 9,
  },
  slashTop: {
    flexDirection: "row",
    alignItems: "center",
    justifyContent: "space-between",
    gap: 8,
  },
  slashName: {
    fontFamily: FONTS.mono,
    fontSize: 12.5,
  },
  slashPlugin: {
    fontFamily: FONTS.mono,
    fontSize: 10.5,
  },
  slashDesc: {
    fontFamily: FONTS.regular,
    fontSize: 11.5,
    marginTop: 1,
  },
  composerBox: {
    flexDirection: "row",
    alignItems: "flex-end",
    borderWidth: 1,
    borderRadius: 16,
    paddingHorizontal: 6,
    paddingVertical: 6,
    gap: 6,
  },
  inputWrap: {
    flex: 1,
    justifyContent: "center",
    position: "relative",
  },
  placeholder: {
    position: "absolute",
    left: 6,
    right: 6,
    fontFamily: FONTS.regular,
    fontSize: 14,
    lineHeight: 20,
  },
  input: {
    minHeight: 36,
    maxHeight: 128,
    paddingHorizontal: 6,
    paddingVertical: 7,
    fontFamily: FONTS.regular,
    fontSize: 14,
    lineHeight: 20,
  },
  ghostBtn: {
    width: 34,
    height: 34,
    borderRadius: 10,
    alignItems: "center",
    justifyContent: "center",
  },
  primaryBtn: {
    width: 34,
    height: 34,
    borderRadius: 10,
    alignItems: "center",
    justifyContent: "center",
  },
});
