import React, { useMemo, useRef, useState } from "react";
import {
  ActivityIndicator,
  Modal,
  PanResponder,
  Pressable,
  StyleSheet,
  Text,
  TextInput,
  View,
  type LayoutChangeEvent,
} from "react-native";
import {
  ArrowUpRight,
  MapPin,
  Mic,
  MicOff,
  Pencil,
  RotateCcw,
  Square,
  Trash2,
  X,
} from "lucide-react-native";
import { useTranslation } from "react-i18next";
import { SafeAreaView } from "react-native-safe-area-context";
import Svg, { Circle, G, Line, Polyline, Rect, Text as SvgText } from "react-native-svg";
import {
  type MarkupAnnotation,
  type MarkupPoint,
  type MarkupTool,
  type WebViewDiagnosticEntry,
  chunkBinaryAsset,
  renderAnnotationsToPng,
} from "../agents/phase2-helpers";
import {
  getHostClient,
  subscribeAssetUploadResults,
  useHostRuntimeStore,
} from "../hosts/HostRuntimeController";
import { useMobilePanelsStore } from "../mobile-panels/store";
import {
  type AssetDeliveryMode,
  BinaryOpcode,
  encodeJsonPayload,
} from "../protocol/binary-frames";
import type { AgentRow } from "../protocol/schemas";
import { useMasilTheme } from "../theme/ThemeProvider";
import { FONTS } from "../theme/typography";
import { useVoiceDictation } from "../voice/useVoiceDictation";

interface MarkupAnnotatorModalProps {
  visible: boolean;
  hostId: string;
  agent: AgentRow;
  previewUrl: string;
  viewportLabel: string;
  diagnostics: WebViewDiagnosticEntry[];
  onClose: () => void;
  onDelivered?: (message: string) => void;
}

const MARKUP_RED = "#ef4444";

let nextUploadSeq = 1;

export function MarkupAnnotatorModal({
  visible,
  hostId,
  agent,
  previewUrl,
  viewportLabel,
  diagnostics,
  onClose,
  onDelivered,
}: MarkupAnnotatorModalProps) {
  const { palette, preference } = useMasilTheme();
  const appearance = preference === "light" ? "light" : "dark";
  const { t, i18n } = useTranslation();
  const setComposerDraftPrefill = useHostRuntimeStore(
    (s) => s.setComposerDraftPrefill
  );
  const selectPanel = useMobilePanelsStore((s) => s.selectPanel);

  const [tool, setTool] = useState<MarkupTool>("pen");
  const [annotations, setAnnotations] = useState<MarkupAnnotation[]>([]);
  const [activeDraft, setActiveDraft] = useState<MarkupAnnotation | null>(null);
  const [instruction, setInstruction] = useState("");
  const [uploading, setUploading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const canvasSizeRef = useRef({ width: 340, height: 380 });
  const toolRef = useRef<MarkupTool>(tool);
  toolRef.current = tool;
  const pinCountRef = useRef(0);
  pinCountRef.current = annotations.filter((a) => a.tool === "pin").length;
  const activeDraftRef = useRef<MarkupAnnotation | null>(null);
  activeDraftRef.current = activeDraft;

  const lang = i18n.language.startsWith("ko") ? "ko" : "en";
  const voice = useVoiceDictation(
    hostId,
    agent.name || agent.pane,
    lang,
    (spoken) => {
      setInstruction((prev) => (prev ? `${prev} ${spoken}` : spoken));
    }
  );

  const normalizePoint = (x: number, y: number): MarkupPoint => {
    const { width, height } = canvasSizeRef.current;
    return {
      x: Math.max(0, Math.min(1, x / Math.max(1, width))),
      y: Math.max(0, Math.min(1, y / Math.max(1, height))),
    };
  };

  const panResponder = useMemo(
    () =>
      PanResponder.create({
        onStartShouldSetPanResponder: () => true,
        onMoveShouldSetPanResponder: () => true,
        onPanResponderGrant: (ev) => {
          const pt = normalizePoint(
            ev.nativeEvent.locationX,
            ev.nativeEvent.locationY
          );
          const id = `ann-${Date.now()}-${Math.random().toString(36).slice(2, 6)}`;
          const currentTool = toolRef.current;
          if (currentTool === "pin") {
            const num = pinCountRef.current + 1;
            setAnnotations((prev) => [
              ...prev,
              { id, tool: "pin", point: pt, number: num, color: MARKUP_RED },
            ]);
            setActiveDraft(null);
            return;
          }
          if (currentTool === "pen") {
            setActiveDraft({
              id,
              tool: "pen",
              points: [pt],
              color: MARKUP_RED,
            });
          } else {
            setActiveDraft({
              id,
              tool: currentTool,
              start: pt,
              end: pt,
              color: MARKUP_RED,
            });
          }
        },
        onPanResponderMove: (ev) => {
          const cur = activeDraftRef.current;
          if (!cur) return;
          const pt = normalizePoint(
            ev.nativeEvent.locationX,
            ev.nativeEvent.locationY
          );
          if (cur.tool === "pen") {
            setActiveDraft({ ...cur, points: [...cur.points, pt] });
          } else if (cur.tool === "rect" || cur.tool === "arrow") {
            setActiveDraft({ ...cur, end: pt });
          }
        },
        onPanResponderRelease: () => {
          const cur = activeDraftRef.current;
          if (cur) {
            setAnnotations((prev) => [...prev, cur]);
            setActiveDraft(null);
          }
        },
      }),
    []
  );

  const onCanvasLayout = (e: LayoutChangeEvent) => {
    canvasSizeRef.current = {
      width: e.nativeEvent.layout.width,
      height: e.nativeEvent.layout.height,
    };
  };

  const allItems = activeDraft ? [...annotations, activeDraft] : annotations;

  const submitMarkup = async (deliveryMode: AssetDeliveryMode) => {
    const client = getHostClient(hostId);
    if (!client || client.connectionState !== "connected") {
      setError(t("disconnected"));
      return;
    }
    setUploading(true);
    setError(null);
    const uploadId = (nextUploadSeq++ % 65000) + 1;

    try {
      const pngBytes = renderAnnotationsToPng(annotations, {
        width: 390,
        height: 520,
        darkTheme: appearance === "dark",
      });
      const chunks = chunkBinaryAsset(pngBytes);

      const resultPromise = new Promise<{
        ok: boolean;
        structured_prompt?: string;
        error?: string;
      }>((resolve, reject) => {
        const timer = setTimeout(() => {
          unsub();
          reject(new Error("Upload timed out"));
        }, 20_000);
        const unsub = subscribeAssetUploadResults((res) => {
          if (res.upload_id !== uploadId) return;
          clearTimeout(timer);
          unsub();
          resolve(res);
        });
      });

      for (const chunk of chunks) {
        client.sendBinaryFrame({
          opcode: BinaryOpcode.AssetUploadChunk,
          channelId: uploadId,
          payload: chunk,
        });
      }

      const consoleErrors = diagnostics
        .filter((d) => d.kind === "console")
        .map((d) => d.message);
      const networkErrors = diagnostics
        .filter((d) => d.kind === "network")
        .map((d) => d.message);

      client.sendBinaryFrame({
        opcode: BinaryOpcode.AssetUploadEnd,
        channelId: uploadId,
        payload: encodeJsonPayload({
          agent_id: agent.pane || agent.remote_id,
          mime: "image/png",
          filename: `markup-${Date.now()}.png`,
          delivery_mode: deliveryMode,
          prompt: instruction.trim() || undefined,
          meta: {
            url: previewUrl,
            viewport: viewportLabel,
            theme: appearance,
            server_socket: agent.server_socket,
            cwd: agent.cwd,
            console_errors: consoleErrors,
            network_errors: networkErrors,
            annotations_count: annotations.length,
          },
        }),
      });

      const outcome = await resultPromise;
      if (!outcome.ok) {
        setError(outcome.error || "Upload failed");
        return;
      }

      if (deliveryMode === "prefill" && outcome.structured_prompt) {
        setComposerDraftPrefill(outcome.structured_prompt);
        selectPanel("agent");
        onDelivered?.(t("markupPrefilledSuccess"));
      } else if (deliveryMode === "send") {
        onDelivered?.(
          t("markupSentSuccess", { name: agent.name || agent.pane })
        );
      } else {
        onDelivered?.(
          t("markupQueuedSuccess", { name: agent.name || agent.pane })
        );
      }
      setAnnotations([]);
      setInstruction("");
      onClose();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setUploading(false);
    }
  };

  const w = canvasSizeRef.current.width;
  const h = canvasSizeRef.current.height;

  return (
    <Modal visible={visible} animationType="slide" onRequestClose={onClose}>
      <SafeAreaView
        style={[styles.root, { backgroundColor: palette.canvas }]}
        edges={["top", "bottom"]}
      >
        <View
          style={[
            styles.header,
            { backgroundColor: palette.panel, borderBottomColor: palette.hairline },
          ]}
        >
          <View style={{ flex: 1 }}>
            <Text style={[styles.title, { color: palette.text }]}>
              {t("markupTitle")}
            </Text>
            <Text
              numberOfLines={1}
              style={[styles.subtitle, { color: palette.muted }]}
            >
              {previewUrl} · {viewportLabel}
            </Text>
          </View>
          <Pressable onPress={onClose} hitSlop={8} style={styles.closeBtn}>
            <X size={20} color={palette.textSecondary} />
          </Pressable>
        </View>

        {/* Drawing tool strip */}
        <View
          style={[
            styles.toolbar,
            { backgroundColor: palette.panel, borderBottomColor: palette.hairline },
          ]}
        >
          {(
            [
              { id: "pen", label: t("markupToolPen"), icon: Pencil },
              { id: "rect", label: t("markupToolRect"), icon: Square },
              { id: "arrow", label: t("markupToolArrow"), icon: ArrowUpRight },
              { id: "pin", label: t("markupToolPin"), icon: MapPin },
            ] as const
          ).map((item) => {
            const Icon = item.icon;
            const active = tool === item.id;
            return (
              <Pressable
                key={item.id}
                onPress={() => setTool(item.id)}
                style={[
                  styles.toolChip,
                  {
                    backgroundColor: active ? palette.dangerSoft : palette.raised,
                    borderColor: active ? palette.danger : palette.hairline,
                  },
                ]}
              >
                <Icon
                  size={13}
                  color={active ? palette.danger : palette.textSecondary}
                />
                <Text
                  style={[
                    styles.toolLabel,
                    { color: active ? palette.danger : palette.textSecondary },
                  ]}
                >
                  {item.label}
                </Text>
              </Pressable>
            );
          })}
          <View style={{ flex: 1 }} />
          <Pressable
            onPress={() => setAnnotations((prev) => prev.slice(0, -1))}
            disabled={annotations.length === 0}
            style={[styles.iconBtn, { backgroundColor: palette.raised }]}
            accessibilityLabel={t("markupUndo")}
          >
            <RotateCcw size={14} color={palette.textSecondary} />
          </Pressable>
          <Pressable
            onPress={() => setAnnotations([])}
            disabled={annotations.length === 0}
            style={[styles.iconBtn, { backgroundColor: palette.raised }]}
            accessibilityLabel={t("markupClear")}
          >
            <Trash2 size={14} color={palette.textSecondary} />
          </Pressable>
        </View>

        {/* Interactive SVG markup surface */}
        <View
          style={[
            styles.canvasWrap,
            { backgroundColor: palette.panel, borderColor: palette.hairline },
          ]}
          onLayout={onCanvasLayout}
          {...panResponder.panHandlers}
        >
          <Svg width="100%" height="100%" style={StyleSheet.absoluteFill}>
            {allItems.map((ann) => {
              if (ann.tool === "pen") {
                const pts = ann.points
                  .map((p) => `${(p.x * w).toFixed(1)},${(p.y * h).toFixed(1)}`)
                  .join(" ");
                return (
                  <Polyline
                    key={ann.id}
                    points={pts}
                    fill="none"
                    stroke={ann.color}
                    strokeWidth={3}
                    strokeLinecap="round"
                    strokeLinejoin="round"
                  />
                );
              }
              if (ann.tool === "rect") {
                const rx = Math.min(ann.start.x, ann.end.x) * w;
                const ry = Math.min(ann.start.y, ann.end.y) * h;
                const rw = Math.abs(ann.end.x - ann.start.x) * w;
                const rh = Math.abs(ann.end.y - ann.start.y) * h;
                return (
                  <Rect
                    key={ann.id}
                    x={rx}
                    y={ry}
                    width={rw}
                    height={rh}
                    stroke={ann.color}
                    strokeWidth={3}
                    fill="rgba(239,68,68,0.08)"
                  />
                );
              }
              if (ann.tool === "arrow") {
                const x1 = ann.start.x * w;
                const y1 = ann.start.y * h;
                const x2 = ann.end.x * w;
                const y2 = ann.end.y * h;
                const angle = Math.atan2(y2 - y1, x2 - x1);
                const head = 14;
                const hx1 = x2 - head * Math.cos(angle - Math.PI / 6);
                const hy1 = y2 - head * Math.sin(angle - Math.PI / 6);
                const hx2 = x2 - head * Math.cos(angle + Math.PI / 6);
                const hy2 = y2 - head * Math.sin(angle + Math.PI / 6);
                return (
                  <G key={ann.id}>
                    <Line
                      x1={x1}
                      y1={y1}
                      x2={x2}
                      y2={y2}
                      stroke={ann.color}
                      strokeWidth={3}
                      strokeLinecap="round"
                    />
                    <Line
                      x1={x2}
                      y1={y2}
                      x2={hx1}
                      y2={hy1}
                      stroke={ann.color}
                      strokeWidth={3}
                      strokeLinecap="round"
                    />
                    <Line
                      x1={x2}
                      y1={y2}
                      x2={hx2}
                      y2={hy2}
                      stroke={ann.color}
                      strokeWidth={3}
                      strokeLinecap="round"
                    />
                  </G>
                );
              }
              const cx = ann.point.x * w;
              const cy = ann.point.y * h;
              return (
                <G key={ann.id}>
                  <Circle cx={cx} cy={cy} r={13} fill={ann.color} />
                  <SvgText
                    x={cx}
                    y={cy + 4}
                    fill="#ffffff"
                    fontSize={12}
                    fontWeight="bold"
                    textAnchor="middle"
                  >
                    {String(ann.number)}
                  </SvgText>
                </G>
              );
            })}
          </Svg>
          {allItems.length === 0 ? (
            <View pointerEvents="none" style={styles.canvasHint}>
              <Text style={{ color: palette.muted, fontSize: 12.5 }}>
                {previewUrl}
              </Text>
            </View>
          ) : null}
        </View>

        {/* Voice + text instruction + 3-way delivery */}
        <View
          style={[
            styles.footer,
            { backgroundColor: palette.panel, borderTopColor: palette.hairline },
          ]}
        >
          {error ? (
            <Text style={{ color: palette.danger, fontSize: 12 }}>{error}</Text>
          ) : null}
          {diagnostics.length > 0 ? (
            <Text style={{ color: palette.warning, fontSize: 11.5 }}>
              {t("previewErrorsCount", { count: diagnostics.length })}
            </Text>
          ) : null}

          <View style={styles.inputRow}>
            <Pressable
              onPress={() => void (voice.recording ? voice.stop() : voice.start())}
              style={[
                styles.micBtn,
                {
                  backgroundColor: voice.recording
                    ? palette.danger
                    : palette.raised,
                },
              ]}
            >
              {voice.transcribing ? (
                <ActivityIndicator size="small" color={palette.accent} />
              ) : voice.recording ? (
                <MicOff size={16} color="#ffffff" />
              ) : (
                <Mic size={16} color={palette.textSecondary} />
              )}
            </Pressable>
            <TextInput
              value={instruction}
              onChangeText={setInstruction}
              placeholder={t("markupPromptPlaceholder")}
              placeholderTextColor={palette.muted}
              multiline
              style={[
                styles.promptInput,
                {
                  backgroundColor: palette.canvas,
                  borderColor: palette.borderSubtle,
                  color: palette.text,
                },
              ]}
            />
          </View>

          <View style={styles.deliveryRow}>
            <Pressable
              onPress={() => void submitMarkup("send")}
              disabled={uploading}
              style={[
                styles.deliverBtn,
                { backgroundColor: palette.accent, opacity: uploading ? 0.6 : 1 },
              ]}
            >
              {uploading ? (
                <ActivityIndicator size="small" color={palette.accentText} />
              ) : (
                <Text
                  style={[styles.deliverText, { color: palette.accentText }]}
                >
                  {t("markupDeliverSend")}
                </Text>
              )}
            </Pressable>
            <Pressable
              onPress={() => void submitMarkup("queue")}
              disabled={uploading}
              style={[
                styles.deliverBtn,
                {
                  backgroundColor: palette.raised,
                  borderColor: palette.borderSubtle,
                  borderWidth: 1,
                },
              ]}
            >
              <Text style={[styles.deliverText, { color: palette.text }]}>
                {t("markupDeliverQueue")}
              </Text>
            </Pressable>
            <Pressable
              onPress={() => void submitMarkup("prefill")}
              disabled={uploading}
              style={[
                styles.deliverBtn,
                {
                  backgroundColor: palette.raised,
                  borderColor: palette.borderSubtle,
                  borderWidth: 1,
                },
              ]}
            >
              <Text style={[styles.deliverText, { color: palette.text }]}>
                {t("markupDeliverPrefill")}
              </Text>
            </Pressable>
          </View>
        </View>
      </SafeAreaView>
    </Modal>
  );
}

const styles = StyleSheet.create({
  root: { flex: 1 },
  header: {
    flexDirection: "row",
    alignItems: "center",
    paddingHorizontal: 14,
    paddingVertical: 11,
    borderBottomWidth: 1,
    gap: 10,
  },
  title: {
    fontFamily: FONTS.bold,
    fontSize: 15,
  },
  subtitle: {
    fontFamily: FONTS.mono,
    fontSize: 11,
    marginTop: 2,
  },
  closeBtn: {
    padding: 4,
  },
  toolbar: {
    flexDirection: "row",
    alignItems: "center",
    gap: 6,
    paddingHorizontal: 12,
    paddingVertical: 8,
    borderBottomWidth: 1,
  },
  toolChip: {
    flexDirection: "row",
    alignItems: "center",
    gap: 4,
    paddingHorizontal: 9,
    paddingVertical: 6,
    borderRadius: 8,
    borderWidth: 1,
  },
  toolLabel: {
    fontFamily: FONTS.semi,
    fontSize: 11.5,
  },
  iconBtn: {
    width: 30,
    height: 30,
    borderRadius: 8,
    alignItems: "center",
    justifyContent: "center",
  },
  canvasWrap: {
    flex: 1,
    margin: 12,
    borderRadius: 12,
    borderWidth: 1,
    overflow: "hidden",
  },
  canvasHint: {
    flex: 1,
    alignItems: "center",
    justifyContent: "center",
    padding: 16,
  },
  footer: {
    borderTopWidth: 1,
    padding: 12,
    gap: 8,
  },
  inputRow: {
    flexDirection: "row",
    alignItems: "flex-end",
    gap: 8,
  },
  micBtn: {
    width: 36,
    height: 36,
    borderRadius: 10,
    alignItems: "center",
    justifyContent: "center",
  },
  promptInput: {
    flex: 1,
    minHeight: 38,
    maxHeight: 96,
    borderWidth: 1,
    borderRadius: 10,
    paddingHorizontal: 10,
    paddingVertical: 8,
    fontFamily: FONTS.regular,
    fontSize: 13.5,
  },
  deliveryRow: {
    flexDirection: "row",
    gap: 8,
  },
  deliverBtn: {
    flex: 1,
    borderRadius: 10,
    paddingVertical: 10,
    alignItems: "center",
    justifyContent: "center",
  },
  deliverText: {
    fontFamily: FONTS.bold,
    fontSize: 12.5,
  },
});
