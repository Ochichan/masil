import React from "react";
import { Pressable, StyleSheet, Text, View } from "react-native";
import { useMasilTheme } from "../theme/ThemeProvider";
import { FONTS } from "../theme/typography";

interface SelectableChipProps {
  label: string;
  selected: boolean;
  onPress: () => void;
  icon?: (color: string) => React.ReactNode;
  maxLabelWidth?: number;
}

/**
 * Single unified selectable chip across every sheet and form:
 * unselected chips use calm borderless surface elevation (no heavy outline);
 * selected chips use ONE consistent Desk Teal tinted fill + 1px teal border.
 */
export function SelectableChip({
  label,
  selected,
  onPress,
  icon,
  maxLabelWidth,
}: SelectableChipProps) {
  const { palette } = useMasilTheme();
  const iconColor = selected ? palette.accent : palette.muted;

  return (
    <Pressable
      onPress={onPress}
      accessibilityRole="button"
      accessibilityState={{ selected }}
      style={[
        styles.chip,
        {
          backgroundColor: selected ? palette.accentSoft : palette.raised,
          borderColor: selected ? palette.accent : "transparent",
        },
      ]}
    >
      {icon ? icon(iconColor) : null}
      <Text
        numberOfLines={1}
        style={[
          styles.label,
          maxLabelWidth ? { maxWidth: maxLabelWidth } : null,
          {
            color: selected ? palette.accent : palette.textSecondary,
            fontFamily: selected ? FONTS.semi : FONTS.regular,
          },
        ]}
      >
        {label}
      </Text>
    </Pressable>
  );
}

interface CapsuleSwitchProps {
  value: boolean;
  onValueChange: (next: boolean) => void;
  accessibilityLabel?: string;
}

/**
 * Precision 48x28 capsule toggle switch with a 24x24 circular thumb fully
 * enclosed inside 2px track padding in both off and on states.
 */
export function CapsuleSwitch({
  value,
  onValueChange,
  accessibilityLabel,
}: CapsuleSwitchProps) {
  const { palette } = useMasilTheme();

  return (
    <Pressable
      onPress={() => onValueChange(!value)}
      accessibilityRole="switch"
      accessibilityState={{ checked: value }}
      accessibilityLabel={accessibilityLabel}
      hitSlop={6}
      style={[
        styles.switchTrack,
        {
          backgroundColor: value ? palette.accent : palette.track,
          borderColor: value ? palette.accent : palette.borderSubtle,
          justifyContent: "center",
          alignItems: value ? "flex-end" : "flex-start",
        },
      ]}
    >
      <View style={styles.switchThumb} />
    </Pressable>
  );
}

const EFFORT_LABELS: Record<string, string> = {
  low: "Low",
  medium: "Medium",
  high: "High",
  xhigh: "Extra high",
  max: "Max",
  minimal: "Minimal",
  none: "None",
};

const MODE_LABELS: Record<string, string> = {
  auto: "Auto",
  "accept-edits": "Accept edits",
  accept_edits: "Accept edits",
  plan: "Plan",
  default: "Default",
  ask: "Ask",
  bypass: "Bypass",
  "bypass-permissions": "Bypass",
  "read-only": "Read only",
  "workspace-write": "Workspace",
  "danger-full-access": "Full access",
};

export function formatEffortLabel(raw: string): string {
  const key = raw.trim().toLowerCase();
  return (
    EFFORT_LABELS[key] ??
    raw
      .replace(/[-_]+/g, " ")
      .replace(/\b\w/g, (c) => c.toUpperCase())
  );
}

export function formatModeLabel(raw: string): string {
  const key = raw.trim().toLowerCase();
  return (
    MODE_LABELS[key] ??
    raw
      .replace(/[-_]+/g, " ")
      .replace(/\b\w/g, (c) => c.toUpperCase())
  );
}

const styles = StyleSheet.create({
  chip: {
    flexDirection: "row",
    alignItems: "center",
    justifyContent: "center",
    gap: 6,
    minHeight: 38,
    paddingHorizontal: 13,
    paddingVertical: 7,
    borderRadius: 10,
    borderWidth: 1,
  },
  label: {
    fontSize: 13.5,
    letterSpacing: -0.1,
  },
  switchTrack: {
    width: 48,
    height: 28,
    borderRadius: 14,
    borderWidth: 1,
    padding: 2,
  },
  switchThumb: {
    width: 22,
    height: 22,
    borderRadius: 11,
    backgroundColor: "#ffffff",
  },
});
