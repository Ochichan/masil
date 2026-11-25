import React, { useMemo, useState } from "react";
import {
  ActivityIndicator,
  Alert,
  Pressable,
  ScrollView,
  StyleSheet,
  Text,
  TextInput,
  View,
} from "react-native";
import * as Clipboard from "expo-clipboard";
import { Check, ChevronRight, Copy } from "lucide-react-native";
import Svg, { Path, Polyline } from "react-native-svg";
import type {
  UiBlock,
  UiButtonDef,
  UiTone,
} from "../protocol/schemas";
import { useMasilTheme } from "../theme/ThemeProvider";
import type { MasilPalette } from "../theme/tokens";
import { FONTS, TABULAR_NUMS } from "../theme/typography";
import { CapsuleSwitch, SelectableChip } from "./SelectableChip";

export interface PluginActionEvent {
  action: string;
  payload?: unknown;
  formValues?: Record<string, string>;
}

interface PluginUiRendererProps {
  blocks: UiBlock[];
  busy?: boolean;
  onAction?: (event: PluginActionEvent) => void;
}

function toneColor(palette: MasilPalette, tone?: UiTone): string {
  switch (tone) {
    case "muted":
      return palette.muted;
    case "accent":
      return palette.accent;
    case "success":
      return palette.success;
    case "warning":
      return palette.warning;
    case "danger":
      return palette.danger;
    default:
      return palette.text;
  }
}

function toneSoftBg(palette: MasilPalette, tone?: UiTone): string {
  switch (tone) {
    case "accent":
      return palette.accentSoft;
    case "success":
      return palette.successSoft;
    case "warning":
      return palette.warningSoft;
    case "danger":
      return palette.dangerSoft;
    default:
      return palette.raised;
  }
}

function SparklineBlock({
  label,
  values,
  unit,
  tone,
  palette,
}: {
  label?: string | null;
  values: number[];
  unit?: string | null;
  tone?: UiTone;
  palette: MasilPalette;
}) {
  const color = toneColor(palette, tone ?? "accent");
  const width = 260;
  const height = 46;
  const pad = 4;

  const { points, areaPath, lastValue } = useMemo(() => {
    if (!values.length) {
      return { points: "", areaPath: "", lastValue: "" };
    }
    const min = Math.min(...values);
    const max = Math.max(...values);
    const span = max - min || 1;
    const step =
      values.length > 1 ? (width - pad * 2) / (values.length - 1) : 0;
    const coords = values.map((v, idx) => {
      const x = pad + idx * step;
      const y = height - pad - ((v - min) / span) * (height - pad * 2);
      return [x, y] as const;
    });
    const pts = coords.map(([x, y]) => `${x.toFixed(1)},${y.toFixed(1)}`).join(" ");
    const first = coords[0]!;
    const last = coords[coords.length - 1]!;
    const area = `M ${first[0].toFixed(1)} ${height} L ${coords
      .map(([x, y]) => `${x.toFixed(1)} ${y.toFixed(1)}`)
      .join(" L ")} L ${last[0].toFixed(1)} ${height} Z`;
    const latest = values[values.length - 1]!;
    const formatted = Number.isInteger(latest) ? String(latest) : latest.toFixed(1);
    return {
      points: pts,
      areaPath: area,
      lastValue: `${formatted}${unit ?? ""}`,
    };
  }, [values, unit]);

  return (
    <View
      style={[
        styles.card,
        { backgroundColor: palette.panel, borderColor: palette.hairline },
      ]}
    >
      <View style={styles.sparkHeader}>
        {label ? (
          <Text style={[styles.sectionLabel, { color: palette.muted }]}>
            {label}
          </Text>
        ) : (
          <View />
        )}
        <Text style={[styles.sparkVal, { color }]}>{lastValue}</Text>
      </View>
      <Svg width="100%" height={height} viewBox={`0 0 ${width} ${height}`}>
        {areaPath ? <Path d={areaPath} fill={color} fillOpacity={0.14} /> : null}
        {points ? (
          <Polyline
            points={points}
            fill="none"
            stroke={color}
            strokeWidth={2}
            strokeLinecap="round"
            strokeLinejoin="round"
          />
        ) : null}
      </Svg>
    </View>
  );
}

function FormBlock({
  block,
  busy,
  onAction,
  palette,
}: {
  block: Extract<UiBlock, { type: "form" }>;
  busy?: boolean;
  onAction?: (event: PluginActionEvent) => void;
  palette: MasilPalette;
}) {
  const [values, setValues] = useState<Record<string, string>>(() => {
    const init: Record<string, string> = {};
    for (const f of block.fields) {
      init[f.name] = f.value ?? "";
    }
    return init;
  });

  const submit = () => {
    if (!onAction || busy) return;
    onAction({ action: block.action, formValues: values });
  };

  return (
    <View
      style={[
        styles.card,
        { backgroundColor: palette.panel, borderColor: palette.hairline, gap: 10 },
      ]}
    >
      {block.fields.map((field) => (
        <View key={field.name} style={{ gap: 5 }}>
          <Text style={[styles.fieldLabel, { color: palette.muted }]}>
            {field.label}
          </Text>
          <TextInput
            value={values[field.name] ?? ""}
            onChangeText={(txt) =>
              setValues((prev) => ({ ...prev, [field.name]: txt }))
            }
            placeholder={field.placeholder ?? ""}
            placeholderTextColor={palette.muted}
            multiline={Boolean(field.multiline)}
            style={[
              styles.input,
              field.multiline && { minHeight: 74, textAlignVertical: "top" },
              {
                backgroundColor: palette.canvas,
                borderColor: palette.borderSubtle,
                color: palette.text,
              },
            ]}
          />
        </View>
      ))}
      <Pressable
        onPress={submit}
        disabled={busy}
        style={[
          styles.btn,
          { backgroundColor: palette.accent, opacity: busy ? 0.6 : 1 },
        ]}
      >
        {busy ? (
          <ActivityIndicator size="small" color={palette.accentText} />
        ) : (
          <Text style={[styles.btnText, { color: palette.accentText }]}>
            {block.submit_label}
          </Text>
        )}
      </Pressable>
    </View>
  );
}

export function PluginUiRenderer({
  blocks,
  busy,
  onAction,
}: PluginUiRendererProps) {
  const { palette } = useMasilTheme();
  const [copiedKey, setCopiedKey] = useState<string | null>(null);

  const copyText = (key: string, text: string) => {
    void Clipboard.setStringAsync(text);
    setCopiedKey(key);
    setTimeout(() => setCopiedKey(null), 1200);
  };

  const fireButton = (btn: UiButtonDef) => {
    if (!onAction || busy) return;
    if (btn.confirm) {
      Alert.alert(btn.label, btn.confirm, [
        { text: "취소", style: "cancel" },
        {
          text: "실행",
          style: btn.style === "danger" ? "destructive" : "default",
          onPress: () => onAction({ action: btn.action, payload: btn.payload }),
        },
      ]);
      return;
    }
    onAction({ action: btn.action, payload: btn.payload });
  };

  const renderButton = (btn: UiButtonDef, flex?: boolean) => {
    const isPrimary = btn.style === "primary";
    const isDanger = btn.style === "danger";
    const bg = isPrimary
      ? palette.accent
      : isDanger
        ? palette.dangerSoft
        : palette.raised;
    const fg = isPrimary
      ? palette.accentText
      : isDanger
        ? palette.danger
        : palette.text;
    const border = isPrimary
      ? "transparent"
      : isDanger
        ? palette.danger
        : palette.borderSubtle;

    return (
      <Pressable
        key={btn.id}
        onPress={() => fireButton(btn)}
        disabled={busy}
        style={[
          styles.btn,
          flex && { flex: 1 },
          {
            backgroundColor: bg,
            borderColor: border,
            borderWidth: isPrimary ? 0 : 1,
            opacity: busy ? 0.65 : 1,
          },
        ]}
      >
        <Text numberOfLines={1} style={[styles.btnText, { color: fg }]}>
          {btn.label}
        </Text>
      </Pressable>
    );
  };

  return (
    <View style={styles.root}>
      {blocks.map((block, index) => {
        const key = `${block.type}-${index}`;
        switch (block.type) {
          case "header":
            return (
              <View key={key} style={styles.headerBlock}>
                <View style={styles.headerRow}>
                  <Text
                    style={[styles.headerTitle, { color: palette.text }]}
                    numberOfLines={2}
                  >
                    {block.title}
                  </Text>
                  {block.badge ? (
                    <View
                      style={[
                        styles.badge,
                        { backgroundColor: toneSoftBg(palette, block.tone ?? "accent") },
                      ]}
                    >
                      <Text
                        style={[
                          styles.badgeText,
                          { color: toneColor(palette, block.tone ?? "accent") },
                        ]}
                      >
                        {block.badge}
                      </Text>
                    </View>
                  ) : null}
                </View>
                {block.subtitle ? (
                  <Text
                    style={[styles.headerSub, { color: palette.muted }]}
                    numberOfLines={2}
                  >
                    {block.subtitle}
                  </Text>
                ) : null}
              </View>
            );

          case "text":
            return (
              <Text
                key={key}
                selectable
                style={[
                  styles.bodyText,
                  block.mono && { fontFamily: FONTS.mono, fontSize: 12.5 },
                  { color: toneColor(palette, block.tone) },
                ]}
              >
                {block.text}
              </Text>
            );

          case "markdown":
            return (
              <View key={key} style={{ gap: 4 }}>
                {block.markdown.split("\n").map((line, li) => {
                  if (line.startsWith("# ")) {
                    return (
                      <Text
                        key={li}
                        style={[styles.mdH1, { color: palette.text }]}
                      >
                        {line.slice(2)}
                      </Text>
                    );
                  }
                  if (line.startsWith("## ")) {
                    return (
                      <Text
                        key={li}
                        style={[styles.mdH2, { color: palette.text }]}
                      >
                        {line.slice(3)}
                      </Text>
                    );
                  }
                  return (
                    <Text
                      key={li}
                      selectable
                      style={[styles.bodyText, { color: palette.text }]}
                    >
                      {line}
                    </Text>
                  );
                })}
              </View>
            );

          case "metric_row":
            return (
              <View key={key} style={styles.metricRow}>
                {block.items.map((item, mi) => (
                  <View
                    key={`${item.label}-${mi}`}
                    style={[
                      styles.metricCard,
                      {
                        backgroundColor: palette.panel,
                        borderColor: palette.hairline,
                      },
                    ]}
                  >
                    <Text
                      numberOfLines={1}
                      style={[styles.metricLabel, { color: palette.muted }]}
                    >
                      {item.label}
                    </Text>
                    <Text
                      numberOfLines={1}
                      style={[
                        styles.metricValue,
                        { color: toneColor(palette, item.tone) },
                      ]}
                    >
                      {item.value}
                    </Text>
                    {item.delta ? (
                      <Text
                        numberOfLines={1}
                        style={[styles.metricDelta, { color: palette.muted }]}
                      >
                        {item.delta}
                      </Text>
                    ) : null}
                  </View>
                ))}
              </View>
            );

          case "badge":
            return (
              <View key={key} style={{ flexDirection: "row" }}>
                <View
                  style={[
                    styles.badge,
                    { backgroundColor: toneSoftBg(palette, block.tone ?? "accent") },
                  ]}
                >
                  <Text
                    style={[
                      styles.badgeText,
                      { color: toneColor(palette, block.tone ?? "accent") },
                    ]}
                  >
                    {block.label}
                  </Text>
                </View>
              </View>
            );

          case "key_value":
            return (
              <View
                key={key}
                style={[
                  styles.card,
                  {
                    backgroundColor: palette.panel,
                    borderColor: palette.hairline,
                    gap: 8,
                  },
                ]}
              >
                {block.rows.map((row, ri) => {
                  const rowKey = `${key}-kv-${ri}`;
                  return (
                    <View key={rowKey} style={styles.kvRow}>
                      <Text
                        style={[styles.kvKey, { color: palette.muted }]}
                        numberOfLines={1}
                      >
                        {row.key}
                      </Text>
                      <View style={styles.kvValWrap}>
                        <Text
                          selectable
                          style={[
                            styles.kvVal,
                            row.mono && { fontFamily: FONTS.mono, fontSize: 12 },
                            { color: toneColor(palette, row.tone) },
                          ]}
                        >
                          {row.value}
                        </Text>
                        {row.copyable ? (
                          <Pressable
                            onPress={() => copyText(rowKey, row.value)}
                            hitSlop={8}
                          >
                            {copiedKey === rowKey ? (
                              <Check size={13} color={palette.success} />
                            ) : (
                              <Copy size={13} color={palette.muted} />
                            )}
                          </Pressable>
                        ) : null}
                      </View>
                    </View>
                  );
                })}
              </View>
            );

          case "list":
            return (
              <View key={key} style={{ gap: 6 }}>
                {block.title ? (
                  <Text style={[styles.sectionLabel, { color: palette.muted }]}>
                    {block.title}
                  </Text>
                ) : null}
                <View
                  style={[
                    styles.listBox,
                    {
                      backgroundColor: palette.panel,
                      borderColor: palette.hairline,
                    },
                  ]}
                >
                  {block.items.map((item, ii) => {
                    const interactive = Boolean(item.action && onAction);
                    return (
                      <Pressable
                        key={item.id ?? `${ii}`}
                        disabled={!interactive || busy}
                        onPress={() => {
                          if (item.action && onAction) {
                            onAction({
                              action: item.action,
                              payload: item.payload,
                            });
                          }
                        }}
                        style={[
                          styles.listRow,
                          ii > 0 && {
                            borderTopWidth: 1,
                            borderTopColor: palette.hairline,
                          },
                        ]}
                      >
                        <View style={{ flex: 1, minWidth: 0, gap: 2 }}>
                          <Text
                            style={[
                              styles.listTitle,
                              { color: toneColor(palette, item.tone) },
                            ]}
                          >
                            {item.title}
                          </Text>
                          {item.subtitle ? (
                            <Text
                              style={[
                                styles.listSub,
                                { color: palette.muted },
                              ]}
                            >
                              {item.subtitle}
                            </Text>
                          ) : null}
                        </View>
                        {item.badge ? (
                          <View
                            style={[
                              styles.badge,
                              {
                                backgroundColor: toneSoftBg(palette, item.tone),
                              },
                            ]}
                          >
                            <Text
                              style={[
                                styles.badgeText,
                                { color: toneColor(palette, item.tone) },
                              ]}
                            >
                              {item.badge}
                            </Text>
                          </View>
                        ) : null}
                        {interactive ? (
                          <ChevronRight size={15} color={palette.muted} />
                        ) : null}
                      </Pressable>
                    );
                  })}
                </View>
              </View>
            );

          case "table":
            return (
              <ScrollView
                key={key}
                horizontal
                showsHorizontalScrollIndicator={false}
                style={[
                  styles.tableWrap,
                  {
                    backgroundColor: palette.panel,
                    borderColor: palette.hairline,
                  },
                ]}
              >
                <View>
                  <View
                    style={[
                      styles.tableRow,
                      {
                        backgroundColor: palette.raised,
                        borderBottomWidth: 1,
                        borderBottomColor: palette.hairline,
                      },
                    ]}
                  >
                    {block.columns.map((col, ci) => (
                      <Text
                        key={ci}
                        style={[styles.tableHead, { color: palette.muted }]}
                      >
                        {col}
                      </Text>
                    ))}
                  </View>
                  {block.rows.map((row, ri) => (
                    <View
                      key={ri}
                      style={[
                        styles.tableRow,
                        ri > 0 && {
                          borderTopWidth: 1,
                          borderTopColor: palette.hairline,
                        },
                      ]}
                    >
                      {row.map((cell, ci) => (
                        <Text
                          key={ci}
                          style={[styles.tableCell, { color: palette.text }]}
                        >
                          {cell}
                        </Text>
                      ))}
                    </View>
                  ))}
                </View>
              </ScrollView>
            );

          case "code":
            return (
              <View
                key={key}
                style={[
                  styles.codeBox,
                  {
                    backgroundColor: palette.canvas,
                    borderColor: palette.borderSubtle,
                  },
                ]}
              >
                <View style={styles.codeHeader}>
                  <Text style={[styles.codeLang, { color: palette.muted }]}>
                    {block.language || "code"}
                  </Text>
                  {block.copyable !== false ? (
                    <Pressable
                      onPress={() => copyText(key, block.code)}
                      hitSlop={8}
                    >
                      {copiedKey === key ? (
                        <Check size={13} color={palette.success} />
                      ) : (
                        <Copy size={13} color={palette.muted} />
                      )}
                    </Pressable>
                  ) : null}
                </View>
                <ScrollView horizontal showsHorizontalScrollIndicator={false}>
                  <Text
                    selectable
                    style={[styles.codeText, { color: palette.text }]}
                  >
                    {block.code}
                  </Text>
                </ScrollView>
              </View>
            );

          case "diff":
            return (
              <View
                key={key}
                style={[
                  styles.codeBox,
                  {
                    backgroundColor: palette.canvas,
                    borderColor: palette.borderSubtle,
                  },
                ]}
              >
                <ScrollView horizontal showsHorizontalScrollIndicator={false}>
                  <View>
                    {block.text.split("\n").map((line, di) => {
                      const color =
                        line.startsWith("+") && !line.startsWith("+++")
                          ? palette.success
                          : line.startsWith("-") && !line.startsWith("---")
                            ? palette.danger
                            : line.startsWith("@@")
                              ? palette.accent
                              : palette.muted;
                      return (
                        <Text key={di} style={[styles.codeText, { color }]}>
                          {line}
                        </Text>
                      );
                    })}
                  </View>
                </ScrollView>
              </View>
            );

          case "progress": {
            const pct = Math.max(0, Math.min(1, block.value / (block.max || 1)));
            const color = toneColor(palette, block.tone ?? "accent");
            return (
              <View key={key} style={{ gap: 6 }}>
                <View style={styles.sparkHeader}>
                  <Text style={[styles.bodyText, { color: palette.text }]}>
                    {block.label}
                  </Text>
                  <Text style={[styles.sparkVal, { color }]}>
                    {Math.round(pct * 100)}%
                  </Text>
                </View>
                <View
                  style={[
                    styles.progressTrack,
                    { backgroundColor: palette.track },
                  ]}
                >
                  <View
                    style={[
                      styles.progressFill,
                      { backgroundColor: color, width: `${pct * 100}%` },
                    ]}
                  />
                </View>
              </View>
            );
          }

          case "sparkline":
            return (
              <SparklineBlock
                key={key}
                label={block.label}
                values={block.values}
                unit={block.unit}
                tone={block.tone}
                palette={palette}
              />
            );

          case "divider":
            return (
              <View
                key={key}
                style={[styles.divider, { backgroundColor: palette.hairline }]}
              />
            );

          case "callout": {
            const c = toneColor(palette, block.tone ?? "accent");
            return (
              <View
                key={key}
                style={[
                  styles.callout,
                  {
                    backgroundColor: toneSoftBg(palette, block.tone ?? "accent"),
                    borderLeftColor: c,
                  },
                ]}
              >
                {block.title ? (
                  <Text style={[styles.calloutTitle, { color: c }]}>
                    {block.title}
                  </Text>
                ) : null}
                <Text
                  selectable
                  style={[styles.bodyText, { color: palette.text }]}
                >
                  {block.text}
                </Text>
              </View>
            );
          }

          case "button":
            return renderButton(block);

          case "button_row":
            return (
              <View key={key} style={styles.btnRow}>
                {block.buttons.map((b) => renderButton(b, true))}
              </View>
            );

          case "toggle":
            return (
              <View
                key={key}
                style={[
                  styles.card,
                  styles.toggleRow,
                  {
                    backgroundColor: palette.panel,
                    borderColor: palette.hairline,
                  },
                ]}
              >
                <Text style={[styles.bodyText, { color: palette.text, flex: 1 }]}>
                  {block.label}
                </Text>
                <CapsuleSwitch
                  value={block.value}
                  onValueChange={(next) => {
                    if (busy) return;
                    onAction?.({
                      action: block.action,
                      payload: {
                        ...(typeof block.payload === "object" && block.payload
                          ? (block.payload as Record<string, unknown>)
                          : {}),
                        value: next,
                      },
                    });
                  }}
                />
              </View>
            );

          case "select":
            return (
              <View key={key} style={{ gap: 6 }}>
                <Text style={[styles.sectionLabel, { color: palette.muted }]}>
                  {block.label}
                </Text>
                <View style={styles.chips}>
                  {block.options.map((opt) => (
                    <SelectableChip
                      key={opt.value}
                      label={opt.label}
                      selected={opt.value === block.value}
                      onPress={() =>
                        onAction?.({
                          action: block.action,
                          payload: { value: opt.value },
                        })
                      }
                    />
                  ))}
                </View>
              </View>
            );

          case "form":
            return (
              <FormBlock
                key={key}
                block={block}
                busy={busy}
                onAction={onAction}
                palette={palette}
              />
            );

          case "empty":
            return (
              <View key={key} style={styles.emptyBox}>
                <Text style={[styles.emptyTitle, { color: palette.text }]}>
                  {block.title}
                </Text>
                {block.body ? (
                  <Text style={[styles.emptyBody, { color: palette.muted }]}>
                    {block.body}
                  </Text>
                ) : null}
              </View>
            );

          default:
            return null;
        }
      })}
    </View>
  );
}

const styles = StyleSheet.create({
  root: { gap: 11 },
  headerBlock: { gap: 3 },
  headerRow: {
    flexDirection: "row",
    alignItems: "center",
    justifyContent: "space-between",
    gap: 8,
  },
  headerTitle: {
    fontFamily: FONTS.bold,
    fontSize: 16,
    letterSpacing: -0.2,
    flex: 1,
  },
  headerSub: { fontFamily: FONTS.mono, fontSize: 11.5 },
  bodyText: { fontFamily: FONTS.regular, fontSize: 13.5, lineHeight: 20 },
  mdH1: { fontFamily: FONTS.bold, fontSize: 16, marginTop: 4 },
  mdH2: { fontFamily: FONTS.semi, fontSize: 14.5, marginTop: 2 },
  sectionLabel: {
    fontFamily: FONTS.semi,
    fontSize: 11.5,
    letterSpacing: 0.2,
  },
  card: {
    borderWidth: 1,
    borderRadius: 11,
    padding: 12,
  },
  metricRow: {
    flexDirection: "row",
    flexWrap: "wrap",
    gap: 8,
  },
  metricCard: {
    flexGrow: 1,
    minWidth: 92,
    borderWidth: 1,
    borderRadius: 10,
    paddingHorizontal: 11,
    paddingVertical: 9,
    gap: 3,
  },
  metricLabel: { fontFamily: FONTS.regular, fontSize: 11 },
  metricValue: {
    fontFamily: FONTS.bold,
    fontVariant: TABULAR_NUMS,
    fontSize: 16,
  },
  metricDelta: { fontFamily: FONTS.mono, fontSize: 10.5 },
  badge: {
    paddingHorizontal: 8,
    paddingVertical: 3,
    borderRadius: 999,
  },
  badgeText: { fontFamily: FONTS.semi, fontSize: 11 },
  kvRow: {
    flexDirection: "row",
    alignItems: "center",
    justifyContent: "space-between",
    gap: 10,
  },
  kvKey: { fontFamily: FONTS.regular, fontSize: 12.5, flexShrink: 0 },
  kvValWrap: {
    flexDirection: "row",
    alignItems: "center",
    gap: 6,
    flex: 1,
    justifyContent: "flex-end",
  },
  kvVal: {
    fontFamily: FONTS.semi,
    fontSize: 12.5,
    textAlign: "right",
    flexShrink: 1,
  },
  listBox: {
    borderWidth: 1,
    borderRadius: 11,
    overflow: "hidden",
  },
  listRow: {
    flexDirection: "row",
    alignItems: "center",
    paddingHorizontal: 12,
    paddingVertical: 10,
    gap: 8,
  },
  listTitle: { fontFamily: FONTS.semi, fontSize: 13.5 },
  listSub: { fontFamily: FONTS.regular, fontSize: 12 },
  tableWrap: {
    borderWidth: 1,
    borderRadius: 10,
  },
  tableRow: { flexDirection: "row" },
  tableHead: {
    fontFamily: FONTS.semi,
    fontSize: 11.5,
    paddingHorizontal: 10,
    paddingVertical: 8,
    minWidth: 96,
  },
  tableCell: {
    fontFamily: FONTS.mono,
    fontSize: 12,
    paddingHorizontal: 10,
    paddingVertical: 8,
    minWidth: 96,
  },
  codeBox: {
    borderWidth: 1,
    borderRadius: 10,
    padding: 10,
    gap: 6,
  },
  codeHeader: {
    flexDirection: "row",
    alignItems: "center",
    justifyContent: "space-between",
  },
  codeLang: { fontFamily: FONTS.mono, fontSize: 11 },
  codeText: { fontFamily: FONTS.mono, fontSize: 12, lineHeight: 18 },
  sparkHeader: {
    flexDirection: "row",
    alignItems: "center",
    justifyContent: "space-between",
  },
  sparkVal: {
    fontFamily: FONTS.bold,
    fontVariant: TABULAR_NUMS,
    fontSize: 13,
  },
  progressTrack: {
    height: 7,
    borderRadius: 999,
    overflow: "hidden",
  },
  progressFill: { height: 7, borderRadius: 999 },
  divider: { height: 1, marginVertical: 2 },
  callout: {
    borderLeftWidth: 3,
    borderRadius: 8,
    paddingHorizontal: 12,
    paddingVertical: 10,
    gap: 4,
  },
  calloutTitle: { fontFamily: FONTS.bold, fontSize: 12.5 },
  btnRow: { flexDirection: "row", gap: 8 },
  btn: {
    height: 38,
    borderRadius: 9,
    paddingHorizontal: 14,
    alignItems: "center",
    justifyContent: "center",
  },
  btnText: { fontFamily: FONTS.semi, fontSize: 13 },
  toggleRow: {
    flexDirection: "row",
    alignItems: "center",
    justifyContent: "space-between",
    gap: 10,
  },
  chips: { flexDirection: "row", flexWrap: "wrap", gap: 7 },
  fieldLabel: { fontFamily: FONTS.semi, fontSize: 12 },
  input: {
    borderWidth: 1,
    borderRadius: 9,
    paddingHorizontal: 11,
    paddingVertical: 9,
    fontFamily: FONTS.regular,
    fontSize: 13.5,
  },
  emptyBox: {
    alignItems: "center",
    justifyContent: "center",
    paddingVertical: 26,
    gap: 6,
  },
  emptyTitle: { fontFamily: FONTS.semi, fontSize: 14 },
  emptyBody: {
    fontFamily: FONTS.regular,
    fontSize: 12.5,
    textAlign: "center",
  },
});
