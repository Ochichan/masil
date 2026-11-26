import React, { useState } from "react";
import {
  ActivityIndicator,
  Pressable,
  StyleSheet,
  Text,
  TextInput,
  View,
} from "react-native";
import {
  ArrowRightLeft,
  FastForward,
  OctagonAlert,
  RotateCcw,
  Send,
} from "lucide-react-native";
import { useTranslation } from "react-i18next";
import { getHostClient } from "../hosts/HostRuntimeController";
import type { AgentRow, CircuitBreakerState } from "../protocol/schemas";
import { useMasilTheme } from "../theme/ThemeProvider";
import { FONTS } from "../theme/typography";

interface CircuitBreakerCardProps {
  hostId: string;
  agent: AgentRow;
  breaker: CircuitBreakerState;
  onResolved: (toast: string) => void;
  onError: (msg: string) => void;
}

export function CircuitBreakerCard({
  hostId,
  agent,
  breaker,
  onResolved,
  onError,
}: CircuitBreakerCardProps) {
  const { palette } = useMasilTheme();
  const { t } = useTranslation();
  const [hintMode, setHintMode] = useState(false);
  const [hintText, setHintText] = useState("");
  const [busyAction, setBusyAction] = useState<string | null>(null);

  const runAction = async (
    action: "resume_hint" | "handoff" | "rollback" | "extend",
    extra: Record<string, unknown> = {}
  ) => {
    const client = getHostClient(hostId);
    if (!client) {
      onError(t("hostNotConnected"));
      return;
    }
    setBusyAction(action);
    try {
      await client.request("guard.breaker.action.request", {
        server_socket: agent.server_socket,
        pane: agent.pane,
        target: agent.pane || agent.name,
        action,
        ...extra,
      });
      setHintMode(false);
      setHintText("");
      if (action === "resume_hint") onResolved(t("breakerHintQueued"));
      else if (action === "handoff") onResolved(t("breakerHandoffStarted"));
      else if (action === "rollback") onResolved(t("breakerRolledBack"));
      else onResolved(t("breakerExtended"));
    } catch (e) {
      onError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusyAction(null);
    }
  };

  const handoffProvider = agent.provider === "claude" ? "codex" : "claude";
  const reasonLabel =
    breaker.tripped_reason === "oscillation"
      ? t("breakerReasonOscillation")
      : breaker.tripped_reason === "same_error"
        ? t("breakerReasonSameError", { count: breaker.same_error_streak })
        : breaker.tripped_reason === "no_progress"
          ? t("breakerReasonNoProgress", { count: breaker.no_progress_streak })
          : breaker.tripped_reason || t("breakerTrippedTitle");

  return (
    <View
      style={[
        styles.card,
        {
          backgroundColor: palette.dangerSoft,
          borderColor: palette.danger,
        },
      ]}
    >
      <View style={styles.headerRow}>
        <OctagonAlert size={16} color={palette.danger} />
        <Text style={[styles.title, { color: palette.danger }]}>
          {t("breakerTrippedTitle")}
        </Text>
      </View>

      <Text style={[styles.reason, { color: palette.text }]}>{reasonLabel}</Text>

      {breaker.error_excerpt ? (
        <Text
          numberOfLines={2}
          style={[
            styles.excerpt,
            { backgroundColor: palette.panel, color: palette.textSecondary },
          ]}
        >
          {breaker.error_excerpt}
        </Text>
      ) : null}

      {hintMode ? (
        <View style={styles.hintRow}>
          <TextInput
            value={hintText}
            onChangeText={setHintText}
            placeholder={t("breakerHintPlaceholder")}
            placeholderTextColor={palette.muted}
            style={[
              styles.hintInput,
              {
                backgroundColor: palette.panel,
                borderColor: palette.hairline,
                color: palette.text,
              },
            ]}
          />
          <Pressable
            disabled={!hintText.trim() || busyAction !== null}
            onPress={() =>
              void runAction("resume_hint", { hint: hintText.trim() })
            }
            style={[
              styles.sendBtn,
              {
                backgroundColor: palette.accent,
                opacity: hintText.trim() ? 1 : 0.45,
              },
            ]}
          >
            {busyAction === "resume_hint" ? (
              <ActivityIndicator size="small" color={palette.accentText} />
            ) : (
              <Send size={13} color={palette.accentText} />
            )}
          </Pressable>
        </View>
      ) : null}

      <View style={styles.actions}>
        <Pressable
          disabled={busyAction !== null}
          onPress={() => setHintMode((v) => !v)}
          style={[
            styles.btn,
            { backgroundColor: palette.panel, borderColor: palette.accentBorder },
          ]}
        >
          <Send size={12} color={palette.accent} />
          <Text style={[styles.btnText, { color: palette.text }]}>
            {t("breakerActionHint")}
          </Text>
        </Pressable>

        <Pressable
          disabled={busyAction !== null}
          onPress={() =>
            void runAction("handoff", {
              provider: handoffProvider,
              cwd: agent.cwd,
            })
          }
          style={[
            styles.btn,
            { backgroundColor: palette.panel, borderColor: palette.hairline },
          ]}
        >
          <ArrowRightLeft size={12} color={palette.accent} />
          <Text style={[styles.btnText, { color: palette.text }]}>
            {t("breakerActionHandoff", { provider: handoffProvider })}
          </Text>
        </Pressable>

        {breaker.pre_oscillation_checkpoint_id ? (
          <Pressable
            disabled={busyAction !== null}
            onPress={() =>
              void runAction("rollback", {
                checkpoint_id: breaker.pre_oscillation_checkpoint_id,
              })
            }
            style={[
              styles.btn,
              { backgroundColor: palette.panel, borderColor: palette.warning },
            ]}
          >
            <RotateCcw size={12} color={palette.warning} />
            <Text style={[styles.btnText, { color: palette.warning }]}>
              {t("breakerActionRollback")}
            </Text>
          </Pressable>
        ) : null}

        <Pressable
          disabled={busyAction !== null}
          onPress={() => void runAction("extend", { turns: 10 })}
          style={[
            styles.btn,
            { backgroundColor: palette.panel, borderColor: palette.hairline },
          ]}
        >
          <FastForward size={12} color={palette.textSecondary} />
          <Text style={[styles.btnText, { color: palette.textSecondary }]}>
            {t("breakerActionExtend")}
          </Text>
        </Pressable>
      </View>
    </View>
  );
}

const styles = StyleSheet.create({
  card: {
    marginHorizontal: 12,
    marginVertical: 6,
    borderWidth: 1,
    borderRadius: 12,
    padding: 11,
    gap: 7,
  },
  headerRow: {
    flexDirection: "row",
    alignItems: "center",
    gap: 6,
  },
  title: {
    fontFamily: FONTS.bold,
    fontSize: 13,
  },
  reason: {
    fontFamily: FONTS.semi,
    fontSize: 12.5,
    lineHeight: 17,
  },
  excerpt: {
    fontFamily: FONTS.mono,
    fontSize: 11,
    padding: 7,
    borderRadius: 7,
  },
  hintRow: {
    flexDirection: "row",
    alignItems: "center",
    gap: 6,
  },
  hintInput: {
    flex: 1,
    borderWidth: 1,
    borderRadius: 8,
    paddingHorizontal: 10,
    paddingVertical: 7,
    fontFamily: FONTS.regular,
    fontSize: 12.5,
  },
  sendBtn: {
    width: 34,
    height: 34,
    borderRadius: 8,
    alignItems: "center",
    justifyContent: "center",
  },
  actions: {
    flexDirection: "row",
    flexWrap: "wrap",
    gap: 6,
  },
  btn: {
    flexDirection: "row",
    alignItems: "center",
    gap: 5,
    borderWidth: 1,
    borderRadius: 8,
    paddingHorizontal: 9,
    paddingVertical: 6,
  },
  btnText: {
    fontFamily: FONTS.semi,
    fontSize: 11.5,
  },
});
