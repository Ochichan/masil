import React, { useMemo } from "react";
import { Platform, StyleSheet, Text, View, type ViewStyle } from "react-native";
import { requireNativeViewManager } from "expo-modules-core";
import type { MasilTerminalFrame, MasilTerminalTheme } from "./contract";
import { encodeTerminalFrame, type NativeTerminalFrame } from "./frame-encoding";

export * from "./contract";
export * from "./frame-encoding";
export * from "./xterm-adapter";

interface NativeProps {
  style?: ViewStyle;
  frame: NativeTerminalFrame;
  theme: MasilTerminalTheme;
  fontSize: number;
  onGridMetrics?: (event: {
    nativeEvent: { cellWidth: number; cellHeight: number };
  }) => void;
}

let NativeTerminalView: React.ComponentType<NativeProps> | null = null;
if (Platform.OS === "ios" || Platform.OS === "android") {
  try {
    NativeTerminalView = requireNativeViewManager("MasilTerminal");
  } catch {
    NativeTerminalView = null;
  }
}

export interface MasilTerminalViewProps {
  style?: ViewStyle;
  frame: MasilTerminalFrame;
  theme: MasilTerminalTheme;
  fontSize?: number;
  onGridMetrics?: (metrics: { cellWidth: number; cellHeight: number }) => void;
}

export function MasilTerminalView({
  style,
  frame,
  theme,
  fontSize = 13,
  onGridMetrics,
}: MasilTerminalViewProps) {
  const encoded = useMemo(() => encodeTerminalFrame(frame), [frame]);

  if (NativeTerminalView) {
    return (
      <NativeTerminalView
        style={style}
        frame={encoded}
        theme={theme}
        fontSize={fontSize}
        onGridMetrics={(e) => onGridMetrics?.(e.nativeEvent)}
      />
    );
  }

  // Fallback monospace renderer (Expo Go / Web / preview without prebuild).
  const bgHex = `#${(theme.background & 0xffffff).toString(16).padStart(6, "0")}`;
  const fgHex = `#${(theme.foreground & 0xffffff).toString(16).padStart(6, "0")}`;
  return (
    <View style={[styles.fallbackRoot, { backgroundColor: bgHex }, style]}>
      {frame.rows.map((row, idx) => (
        <Text
          key={idx}
          selectable
          style={[
            styles.fallbackRow,
            { color: fgHex, fontSize, lineHeight: Math.round(fontSize * 1.32) },
          ]}
        >
          {row.text}
        </Text>
      ))}
    </View>
  );
}

const styles = StyleSheet.create({
  fallbackRoot: {
    padding: 6,
  },
  fallbackRow: {
    fontFamily: Platform.select({
      ios: "Menlo",
      android: "monospace",
      default: "monospace",
    }),
  },
});
