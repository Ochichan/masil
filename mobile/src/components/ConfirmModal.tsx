import React from "react";
import { Modal, Pressable, StyleSheet, Text, View } from "react-native";
import { useTranslation } from "react-i18next";
import { useMasilTheme } from "../theme/ThemeProvider";

interface ConfirmModalProps {
  visible: boolean;
  title: string;
  body: string;
  destructive?: boolean;
  onCancel: () => void;
  onConfirm: () => void;
}

export function ConfirmModal({
  visible,
  title,
  body,
  destructive = false,
  onCancel,
  onConfirm,
}: ConfirmModalProps) {
  const { palette } = useMasilTheme();
  const { t } = useTranslation();

  return (
    <Modal
      visible={visible}
      transparent
      animationType="fade"
      onRequestClose={onCancel}
    >
      <View style={styles.backdrop}>
        <View
          style={[
            styles.card,
            { backgroundColor: palette.panel, borderColor: palette.track },
          ]}
        >
          <Text style={[styles.title, { color: palette.text }]}>{title}</Text>
          <Text style={[styles.body, { color: palette.muted }]}>{body}</Text>
          <View style={styles.actions}>
            <Pressable
              onPress={onCancel}
              style={[
                styles.button,
                { backgroundColor: palette.raised, borderColor: palette.track },
              ]}
            >
              <Text style={[styles.buttonText, { color: palette.text }]}>
                {t("cancel")}
              </Text>
            </Pressable>
            <Pressable
              onPress={onConfirm}
              style={[
                styles.button,
                {
                  backgroundColor: destructive ? palette.danger : palette.accent,
                  borderColor: destructive ? palette.danger : palette.accent,
                },
              ]}
            >
              <Text
                style={[
                  styles.buttonText,
                  { color: destructive ? "#ffffff" : palette.accentText },
                ]}
              >
                {t("confirm")}
              </Text>
            </Pressable>
          </View>
        </View>
      </View>
    </Modal>
  );
}

const styles = StyleSheet.create({
  backdrop: {
    flex: 1,
    backgroundColor: "rgba(0,0,0,0.6)",
    justifyContent: "center",
    padding: 24,
  },
  card: {
    borderWidth: 1,
    borderRadius: 12,
    padding: 18,
    gap: 12,
  },
  title: {
    fontSize: 16,
    fontWeight: "700",
  },
  body: {
    fontSize: 14,
    lineHeight: 20,
  },
  actions: {
    flexDirection: "row",
    justifyContent: "flex-end",
    gap: 10,
    marginTop: 6,
  },
  button: {
    paddingHorizontal: 16,
    paddingVertical: 10,
    borderRadius: 8,
    borderWidth: 1,
  },
  buttonText: {
    fontSize: 14,
    fontWeight: "600",
  },
});
