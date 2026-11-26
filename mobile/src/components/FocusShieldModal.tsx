import React, { useState } from "react";
import {
  ActivityIndicator,
  Modal,
  Pressable,
  StyleSheet,
  Text,
  View,
} from "react-native";
import { Moon, ShieldAlert, X } from "lucide-react-native";
import { useTranslation } from "react-i18next";
import { formatFocusRemaining } from "../agents/phase2-helpers";
import { useHostRuntimeStore } from "../hosts/HostRuntimeController";
import type { FocusShieldStatus } from "../protocol/schemas";
import { useMasilTheme } from "../theme/ThemeProvider";
import { FONTS } from "../theme/typography";

interface FocusShieldModalProps {
  visible: boolean;
  hostId: string | null;
  status: FocusShieldStatus | null;
  onClose: () => void;
  onEndedOpenBriefing: () => void;
}

export function FocusShieldModal({
  visible,
  hostId,
  status,
  onClose,
  onEndedOpenBriefing,
}: FocusShieldModalProps) {
  const { palette } = useMasilTheme();
  const { t, i18n } = useTranslation();
  const setFocusShield = useHostRuntimeStore((s) => s.setFocusShield);
  const [busy, setBusy] = useState(false);

  const isKo = i18n.language.startsWith("ko");

  const startFocus = async (duration: string) => {
    if (!hostId || busy) return;
    setBusy(true);
    try {
      await setFocusShield(hostId, true, duration);
      onClose();
    } finally {
      setBusy(false);
    }
  };

  const endFocus = async () => {
    if (!hostId || busy) return;
    setBusy(true);
    try {
      await setFocusShield(hostId, false);
      onClose();
      onEndedOpenBriefing();
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal
      visible={visible}
      transparent
      animationType="fade"
      onRequestClose={onClose}
    >
      <View style={styles.backdrop}>
        <View
          style={[
            styles.card,
            { backgroundColor: palette.panel, borderColor: palette.borderSubtle },
          ]}
        >
          <View style={styles.header}>
            <View style={styles.titleRow}>
              <Moon size={17} color={palette.accent} />
              <Text style={[styles.title, { color: palette.text }]}>
                {t("focusShieldTitle")}
              </Text>
            </View>
            <Pressable onPress={onClose} hitSlop={8}>
              <X size={18} color={palette.muted} />
            </Pressable>
          </View>

          <Text style={[styles.desc, { color: palette.textSecondary }]}>
            {t("focusShieldDesc")}
          </Text>

          {status?.active ? (
            <View
              style={[
                styles.activeBox,
                {
                  backgroundColor: palette.accentSoft,
                  borderColor: palette.accentBorder,
                },
              ]}
            >
              <ShieldAlert size={15} color={palette.accent} />
              <Text style={[styles.activeText, { color: palette.accent }]}>
                {t("focusActiveLabel", {
                  remaining: formatFocusRemaining(status.remaining_ms, isKo),
                })}
                {status.held_count > 0
                  ? ` · ${t("focusHeldBadge", { count: status.held_count })}`
                  : ""}
              </Text>
            </View>
          ) : null}

          <View style={styles.presets}>
            {(
              [
                { dur: "25m", label: t("focusDuration25m") },
                { dur: "60m", label: t("focusDuration60m") },
                { dur: "until-09:00", label: t("focusDurationUntilMorning") },
              ] as const
            ).map((opt) => (
              <Pressable
                key={opt.dur}
                onPress={() => void startFocus(opt.dur)}
                disabled={busy}
                style={[
                  styles.presetBtn,
                  {
                    backgroundColor: palette.raised,
                    borderColor: palette.hairline,
                  },
                ]}
              >
                {busy ? (
                  <ActivityIndicator size="small" color={palette.accent} />
                ) : (
                  <Text style={[styles.presetText, { color: palette.text }]}>
                    {opt.label}
                  </Text>
                )}
              </Pressable>
            ))}
          </View>

          {status?.active ? (
            <Pressable
              onPress={() => void endFocus()}
              disabled={busy}
              style={[styles.endBtn, { backgroundColor: palette.accent }]}
            >
              <Text style={[styles.endBtnText, { color: palette.accentText }]}>
                {t("focusTurnOff")}
              </Text>
            </Pressable>
          ) : null}
        </View>
      </View>
    </Modal>
  );
}

const styles = StyleSheet.create({
  backdrop: {
    flex: 1,
    backgroundColor: "rgba(0,0,0,0.58)",
    justifyContent: "center",
    padding: 20,
  },
  card: {
    borderRadius: 16,
    borderWidth: 1,
    padding: 16,
    gap: 12,
  },
  header: {
    flexDirection: "row",
    alignItems: "center",
    justifyContent: "space-between",
  },
  titleRow: {
    flexDirection: "row",
    alignItems: "center",
    gap: 8,
  },
  title: {
    fontFamily: FONTS.bold,
    fontSize: 15.5,
  },
  desc: {
    fontFamily: FONTS.regular,
    fontSize: 12.5,
    lineHeight: 18,
  },
  activeBox: {
    flexDirection: "row",
    alignItems: "center",
    gap: 8,
    paddingHorizontal: 10,
    paddingVertical: 8,
    borderRadius: 10,
    borderWidth: 1,
  },
  activeText: {
    fontFamily: FONTS.semi,
    fontSize: 12.5,
  },
  presets: {
    flexDirection: "row",
    gap: 8,
  },
  presetBtn: {
    flex: 1,
    borderRadius: 10,
    borderWidth: 1,
    paddingVertical: 11,
    alignItems: "center",
    justifyContent: "center",
  },
  presetText: {
    fontFamily: FONTS.semi,
    fontSize: 12.5,
  },
  endBtn: {
    borderRadius: 10,
    paddingVertical: 11,
    alignItems: "center",
  },
  endBtnText: {
    fontFamily: FONTS.bold,
    fontSize: 13,
  },
});
