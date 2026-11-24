import React, { useState } from "react";
import { Pressable, StyleSheet, Text, View } from "react-native";
import { ChevronDown, ChevronRight, Terminal } from "lucide-react-native";
import { useMasilTheme } from "../theme/ThemeProvider";

interface ToolCallCardProps {
  name: string;
  summary: string;
  status: "running" | "completed" | "failed";
  argsJson?: unknown;
  output?: string | null;
}

export function ToolCallCard({
  name,
  summary,
  status,
  argsJson,
  output,
}: ToolCallCardProps) {
  const { palette } = useMasilTheme();
  const [open, setOpen] = useState(false);

  const statusColor =
    status === "failed"
      ? palette.danger
      : status === "running"
        ? palette.warning
        : palette.success;

  return (
    <View
      style={[
        styles.container,
        { backgroundColor: palette.panel, borderColor: palette.track },
      ]}
    >
      <Pressable
        onPress={() => setOpen((v) => !v)}
        style={[styles.header, { backgroundColor: palette.raised }]}
      >
        {open ? (
          <ChevronDown size={15} color={palette.muted} />
        ) : (
          <ChevronRight size={15} color={palette.muted} />
        )}
        <Terminal size={14} color={palette.accent} />
        <Text
          numberOfLines={1}
          style={[styles.summary, { color: palette.text }]}
        >
          {summary || name}
        </Text>
        <Text style={[styles.status, { color: statusColor }]}>{status}</Text>
      </Pressable>
      {open ? (
        <View style={styles.body}>
          {argsJson !== undefined && argsJson !== null ? (
            <Text selectable style={[styles.mono, { color: palette.muted }]}>
              {typeof argsJson === "string"
                ? argsJson
                : JSON.stringify(argsJson, null, 2)}
            </Text>
          ) : null}
          {output ? (
            <Text selectable style={[styles.mono, { color: palette.text }]}>
              {output}
            </Text>
          ) : null}
        </View>
      ) : null}
    </View>
  );
}

const styles = StyleSheet.create({
  container: {
    borderWidth: 1,
    borderRadius: 8,
    overflow: "hidden",
    marginVertical: 4,
  },
  header: {
    flexDirection: "row",
    alignItems: "center",
    paddingHorizontal: 10,
    paddingVertical: 8,
    gap: 6,
  },
  summary: {
    flex: 1,
    fontSize: 13,
    fontWeight: "600",
  },
  status: {
    fontSize: 11,
    fontWeight: "700",
    textTransform: "uppercase",
  },
  body: {
    padding: 10,
    gap: 8,
  },
  mono: {
    fontFamily: "monospace",
    fontSize: 12,
    lineHeight: 17,
  },
});
