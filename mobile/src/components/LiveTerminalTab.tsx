/**
 * Live interactive terminal tab backed by `@xterm/headless` + `MasilTerminalView`
 * (CoreText on iOS, Canvas on Android) over `masil -C` binary frames.
 *
 * Preserves desktop PTY dimensions by default (pan/zoom viewport); only sends
 * `TerminalResize(mode: "fit")` when the user explicitly enables Fit mode.
 */
import React, { useEffect, useMemo, useRef, useState } from "react";
import {
  Pressable,
  ScrollView,
  StyleSheet,
  Text,
  TextInput,
  View,
  useWindowDimensions,
} from "react-native";
import * as Clipboard from "expo-clipboard";
import {
  Maximize2,
  Minimize2,
  RefreshCw,
  ZoomIn,
  ZoomOut,
} from "lucide-react-native";
import { useTranslation } from "react-i18next";
import {
  HeadlessTerminalEngine,
  MasilTerminalFrame,
  MasilTerminalView,
} from "masil-terminal";
import {
  getHostClient,
  subscribeHostBinary,
  useHostRuntimeStore,
} from "../hosts/HostRuntimeController";
import { useBlockMobilePanelOpenGestures } from "../mobile-panels/gestures";
import {
  BinaryOpcode,
  decodeTerminalSnapshot,
} from "../protocol/binary-frames";
import type { AgentRow } from "../protocol/schemas";
import { useMasilTheme } from "../theme/ThemeProvider";

interface LiveTerminalTabProps {
  hostId: string;
  agent: AgentRow;
  active: boolean;
}

const EXTRA_KEYS: Array<{ label: string; bytes: string }> = [
  { label: "Esc", bytes: "\x1b" },
  { label: "Ctrl-C", bytes: "\x03" },
  { label: "Tab", bytes: "\t" },
  { label: "S-Tab", bytes: "\x1b[Z" },
  { label: "Space", bytes: " " },
  { label: "↑", bytes: "\x1b[A" },
  { label: "↓", bytes: "\x1b[B" },
  { label: "←", bytes: "\x1b[D" },
  { label: "→", bytes: "\x1b[C" },
  { label: "Enter", bytes: "\r" },
  { label: "Bksp", bytes: "\x7f" },
  { label: "y", bytes: "y" },
  { label: "n", bytes: "n" },
  { label: "1", bytes: "1" },
  { label: "2", bytes: "2" },
  { label: "3", bytes: "3" },
  { label: "4", bytes: "4" },
];

function hexToArgb(hex: string): number {
  const clean = hex.replace("#", "");
  return (0xff000000 | parseInt(clean, 16)) >>> 0;
}

export function LiveTerminalTab({ hostId, agent, active }: LiveTerminalTabProps) {
  const { palette } = useMasilTheme();
  const { t } = useTranslation();
  const { width } = useWindowDimensions();
  const [fitMode, setFitMode] = useState(false);
  const [fontSize, setFontSize] = useState(12);
  const [cellMetrics, setCellMetrics] = useState({
    cellWidth: 7.5,
    cellHeight: 16.5,
  });
  const [desktopSize, setDesktopSize] = useState({ cols: 120, rows: 36 });
  const [rawInput, setRawInput] = useState("");
  const [attachSeq, setAttachSeq] = useState(0);
  const hostConnection = useHostRuntimeStore(
    (s) => s.hosts[hostId]?.connection ?? "disconnected"
  );
  const channelId = 1;

  // Block side-panel swipe-open gestures while panning a desktop-width terminal.
  useBlockMobilePanelOpenGestures(active && !fitMode);

  const engineRef = useRef<HeadlessTerminalEngine | null>(null);
  const [frame, setFrame] = useState<MasilTerminalFrame>({
    columns: 80,
    rows: [],
    cursor: null,
  });

  const theme = useMemo(
    () => ({
      background: hexToArgb(palette.canvas),
      foreground: hexToArgb(palette.text),
      cursor: hexToArgb(palette.accent),
    }),
    [palette]
  );

  useEffect(() => {
    if (!active) return;
    const engine = new HeadlessTerminalEngine(80, 28);
    engineRef.current = engine;

    const flushFrame = () => {
      setFrame(engine.snapshotFrame(theme.foreground, theme.background));
    };

    const unsub = subscribeHostBinary(hostId, (bin) => {
      if (bin.channelId !== channelId) return;
      if (bin.opcode === BinaryOpcode.TerminalSnapshot) {
        try {
          const { meta, ansi } = decodeTerminalSnapshot(bin.payload);
          setDesktopSize({ cols: meta.desktop_cols, rows: meta.desktop_rows });
          engine.resize(meta.cols, meta.rows);
          engine.reset();
          engine.write(ansi, flushFrame);
        } catch {
          // ignore malformed snapshot
        }
      } else if (bin.opcode === BinaryOpcode.TerminalOutput) {
        engine.write(bin.payload, flushFrame);
      }
    });

    const client = getHostClient(hostId);
    if (client && hostConnection === "connected") {
      void client
        .request("terminal.attach.request", {
          channel_id: channelId,
          server_socket: agent.server_socket,
          pane: agent.pane,
        })
        .catch(() => {});
    }

    return () => {
      unsub();
      if (client && client.connectionState === "connected") {
        void client
          .request("terminal.detach.request", { channel_id: channelId })
          .catch(() => {});
      }
      engine.dispose();
      engineRef.current = null;
    };
  }, [
    active,
    hostId,
    hostConnection,
    attachSeq,
    agent.server_socket,
    agent.pane,
    theme,
  ]);

  const sendBytes = (str: string) => {
    const client = getHostClient(hostId);
    if (!client) return;
    client.sendTerminalInput(channelId, new TextEncoder().encode(str));
  };

  const sendLinePaced = (line: string) => {
    if (!line) return;
    sendBytes(line);
    setTimeout(() => sendBytes("\r"), 75);
  };

  const toggleFit = () => {
    const client = getHostClient(hostId);
    const next = !fitMode;
    setFitMode(next);
    if (!client) return;
    if (next) {
      const cols = Math.max(34, Math.floor((width - 12) / cellMetrics.cellWidth));
      const rows = 28;
      engineRef.current?.resize(cols, rows);
      client.sendTerminalResize(channelId, cols, rows, "fit");
    } else {
      engineRef.current?.resize(desktopSize.cols, desktopSize.rows);
      client.sendTerminalResize(
        channelId,
        desktopSize.cols,
        desktopSize.rows,
        "desktop"
      );
    }
  };

  const canvasWidth = Math.max(
    width,
    Math.ceil(frame.columns * cellMetrics.cellWidth + 12)
  );
  const canvasHeight = Math.max(
    360,
    Math.ceil(Math.max(frame.rows.length, 24) * cellMetrics.cellHeight + 16)
  );

  return (
    <View style={[styles.root, { backgroundColor: palette.canvas }]}>
      <View
        style={[
          styles.toolbar,
          { backgroundColor: palette.panel, borderBottomColor: palette.track },
        ]}
      >
        <Text style={[styles.geo, { color: palette.muted }]}>
          {frame.columns}×{frame.rows.length} ·{" "}
          {fitMode ? t("fitToPhone") : t("desktopSizePreserved")}
        </Text>
        <View style={styles.toolbarActions}>
          <Pressable
            onPress={() => setAttachSeq((n) => n + 1)}
            style={[styles.toolBtn, { backgroundColor: palette.raised }]}
            accessibilityLabel={t("terminalReattaching")}
          >
            <RefreshCw size={14} color={palette.text} />
          </Pressable>
          <Pressable
            onPress={() => setFontSize((s) => Math.max(9, s - 1))}
            style={[styles.toolBtn, { backgroundColor: palette.raised }]}
          >
            <ZoomOut size={15} color={palette.text} />
          </Pressable>
          <Pressable
            onPress={() => setFontSize((s) => Math.min(20, s + 1))}
            style={[styles.toolBtn, { backgroundColor: palette.raised }]}
          >
            <ZoomIn size={15} color={palette.text} />
          </Pressable>
          <Pressable
            onPress={toggleFit}
            style={[
              styles.fitBtn,
              {
                backgroundColor: fitMode ? palette.accent : palette.raised,
                borderColor: palette.track,
              },
            ]}
          >
            {fitMode ? (
              <Minimize2 size={13} color={palette.accentText} />
            ) : (
              <Maximize2 size={13} color={palette.text} />
            )}
            <Text
              style={{
                fontSize: 12,
                fontWeight: "600",
                color: fitMode ? palette.accentText : palette.text,
              }}
            >
              {fitMode ? t("desktopSizePreserved") : t("fitToPhone")}
            </Text>
          </Pressable>
        </View>
      </View>

      <ScrollView style={styles.scrollOuter}>
        <ScrollView horizontal={!fitMode} showsHorizontalScrollIndicator>
          <MasilTerminalView
            style={{ width: canvasWidth, height: canvasHeight }}
            frame={frame}
            theme={theme}
            fontSize={fontSize}
            onGridMetrics={setCellMetrics}
          />
        </ScrollView>
      </ScrollView>

      <View
        style={[
          styles.inputBar,
          { backgroundColor: palette.panel, borderTopColor: palette.track },
        ]}
      >
        <ScrollView
          horizontal
          showsHorizontalScrollIndicator={false}
          contentContainerStyle={styles.keysRow}
        >
          {EXTRA_KEYS.map((k) => (
            <Pressable
              key={k.label}
              onPress={() => sendBytes(k.bytes)}
              style={[
                styles.keyCap,
                { backgroundColor: palette.raised, borderColor: palette.track },
              ]}
            >
              <Text style={[styles.keyText, { color: palette.text }]}>
                {k.label}
              </Text>
            </Pressable>
          ))}
          <Pressable
            onPress={async () => {
              const clip = await Clipboard.getStringAsync();
              if (clip) sendBytes(clip);
            }}
            style={[
              styles.keyCap,
              { backgroundColor: palette.raised, borderColor: palette.accent },
            ]}
          >
            <Text style={[styles.keyText, { color: palette.accent }]}>
              Paste
            </Text>
          </Pressable>
        </ScrollView>

        <View style={styles.directTypeRow}>
          <TextInput
            value={rawInput}
            onChangeText={setRawInput}
            placeholder="Direct PTY line input..."
            placeholderTextColor={palette.muted}
            onSubmitEditing={() => {
              if (!rawInput) return;
              sendLinePaced(rawInput);
              setRawInput("");
            }}
            style={[
              styles.directInput,
              {
                backgroundColor: palette.canvas,
                borderColor: palette.track,
                color: palette.text,
              },
            ]}
          />
          <Pressable
            onPress={() => {
              if (!rawInput) return;
              sendLinePaced(rawInput);
              setRawInput("");
            }}
            style={[styles.sendKey, { backgroundColor: palette.accent }]}
          >
            <Text style={{ color: palette.accentText, fontWeight: "700" }}>
              ↵
            </Text>
          </Pressable>
        </View>
      </View>
    </View>
  );
}

const styles = StyleSheet.create({
  root: { flex: 1 },
  toolbar: {
    flexDirection: "row",
    alignItems: "center",
    justifyContent: "space-between",
    paddingHorizontal: 10,
    paddingVertical: 6,
    borderBottomWidth: 1,
  },
  geo: { fontSize: 12, fontFamily: "monospace" },
  toolbarActions: { flexDirection: "row", alignItems: "center", gap: 6 },
  toolBtn: {
    padding: 6,
    borderRadius: 6,
  },
  fitBtn: {
    flexDirection: "row",
    alignItems: "center",
    gap: 5,
    paddingHorizontal: 9,
    paddingVertical: 6,
    borderRadius: 6,
    borderWidth: 1,
  },
  scrollOuter: { flex: 1 },
  inputBar: {
    borderTopWidth: 1,
    paddingVertical: 6,
    gap: 6,
  },
  keysRow: {
    paddingHorizontal: 8,
    gap: 6,
  },
  keyCap: {
    paddingHorizontal: 10,
    paddingVertical: 6,
    borderRadius: 6,
    borderWidth: 1,
  },
  keyText: {
    fontSize: 12,
    fontWeight: "600",
    fontFamily: "monospace",
  },
  directTypeRow: {
    flexDirection: "row",
    paddingHorizontal: 8,
    gap: 6,
  },
  directInput: {
    flex: 1,
    height: 36,
    borderWidth: 1,
    borderRadius: 8,
    paddingHorizontal: 10,
    fontSize: 13,
  },
  sendKey: {
    width: 40,
    height: 36,
    borderRadius: 8,
    alignItems: "center",
    justifyContent: "center",
  },
});
