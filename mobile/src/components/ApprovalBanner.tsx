import React, { useState } from "react";
import { Pressable, StyleSheet, Text, View } from "react-native";
import { ShieldAlert } from "lucide-react-native";
import { useTranslation } from "react-i18next";
import { useMasilTheme } from "../theme/ThemeProvider";
import { ConfirmModal } from "./ConfirmModal";

interface ApprovalOption {
  id: string;
  label: string;
  key_sequence?: string | null;
}

interface ApprovalBannerProps {
  agentName: string;
  prompt: string;
  mode: "structured_api" | "terminal_confirm";
  requestId?: string | null;
  options: ApprovalOption[];
  onApprove: (option: ApprovalOption) => void;
  onOpenTerminal: () => void;
}

export function ApprovalBanner({
  agentName,
  prompt,
  mode,
  options,
  onApprove,
  onOpenTerminal,
}: ApprovalBannerProps) {
  const { palette } = useMasilTheme();
  const { t } = useTranslation();
  const [pendingOption, setPendingOption] = useState<ApprovalOption | null>(
    null
  );

  const handlePress = (opt: ApprovalOption) => {
    if (mode === "terminal_confirm") {
      setPendingOption(opt);
    } else {
      onApprove(opt);
    }
  };

  return (
    <View
      style={[
        styles.banner,
        { backgroundColor: palette.panel, borderColor: palette.warning },
      ]}
    >
      <View style={styles.titleRow}>
        <ShieldAlert size={16} color={palette.warning} />
        <Text style={[styles.title, { color: palette.warning }]}>
          {t("approval")} ·{" "}
          {mode === "structured_api" ? "Provider API" : "Live TUI"}
        </Text>
      </View>
      <Text style={[styles.prompt, { color: palette.text }]}>{prompt}</Text>
      <View style={styles.buttons}>
        {options.map((opt) => (
          <Pressable
            key={opt.id}
            onPress={() => handlePress(opt)}
            style={[
              styles.btn,
              {
                backgroundColor:
                  opt.id === "deny" ? palette.raised : palette.accent,
                borderColor: opt.id === "deny" ? palette.track : palette.accent,
              },
            ]}
          >
            <Text
              style={[
                styles.btnText,
                {
                  color:
                    opt.id === "deny" ? palette.text : palette.accentText,
                },
              ]}
            >
              {opt.label}
            </Text>
          </Pressable>
        ))}
        <Pressable
          onPress={onOpenTerminal}
          style={[
            styles.btn,
            { backgroundColor: palette.raised, borderColor: palette.track },
          ]}
        >
          <Text style={[styles.btnText, { color: palette.text }]}>
            {t("terminal")}
          </Text>
        </Pressable>
      </View>

      <ConfirmModal
        visible={pendingOption !== null}
        title={t("confirmActionTitle")}
        body={t("confirmApproveBody", { name: agentName })}
        onCancel={() => setPendingOption(null)}
        onConfirm={() => {
          const chosen = pendingOption;
          setPendingOption(null);
          if (chosen) onApprove(chosen);
        }}
      />
    </View>
  );
}

const styles = StyleSheet.create({
  banner: {
    borderWidth: 1,
    borderRadius: 10,
    padding: 12,
    marginVertical: 8,
    gap: 8,
  },
  titleRow: {
    flexDirection: "row",
    alignItems: "center",
    gap: 6,
  },
  title: {
    fontSize: 13,
    fontWeight: "700",
  },
  prompt: {
    fontSize: 13,
    lineHeight: 18,
  },
  buttons: {
    flexDirection: "row",
    flexWrap: "wrap",
    gap: 8,
    marginTop: 4,
  },
  btn: {
    paddingHorizontal: 12,
    paddingVertical: 8,
    borderRadius: 8,
    borderWidth: 1,
  },
  btnText: {
    fontSize: 13,
    fontWeight: "600",
  },
});
