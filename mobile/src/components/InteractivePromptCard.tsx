import React, { useEffect, useMemo, useState } from "react";
import {
  ActivityIndicator,
  Pressable,
  ScrollView,
  StyleSheet,
  Text,
  TextInput,
  View,
} from "react-native";
import {
  Check,
  ChevronDown,
  ChevronLeft,
  ChevronRight,
  ChevronUp,
  Eye,
  EyeOff,
  HelpCircle,
  MessageSquareText,
  Send,
  ShieldAlert,
  Square,
  SquareCheck,
  Terminal,
} from "lucide-react-native";
import { useTranslation } from "react-i18next";
import { riskTierTone } from "../agents/phase3-helpers";
import type {
  InteractiveOption,
  InteractivePrompt,
} from "../protocol/schemas";
import { useMasilTheme } from "../theme/ThemeProvider";
import { FONTS, TABULAR_NUMS } from "../theme/typography";
import { ConfirmModal } from "./ConfirmModal";

export interface PromptRespondPayload {
  action:
    | "api_permission"
    | "api_question"
    | "tui_select_option"
    | "tui_toggle_options"
    | "tui_custom_text"
    | "tui_keys";
  request_id?: string;
  decision?: "once" | "always" | "reject";
  message?: string;
  answers?: string[];
  multi_answers?: string[][];
  reject?: boolean;
  option_index?: number;
  current_selected_index?: number;
  hotkey?: string;
  indices?: number[];
  text?: string;
  submit?: boolean;
  keys?: string[];
}

interface InteractivePromptCardProps {
  agentName: string;
  prompts: InteractivePrompt[];
  onRespond: (payload: PromptRespondPayload) => Promise<void>;
  onOpenTerminal: () => void;
}

export function InteractivePromptCard({
  agentName,
  prompts,
  onRespond,
  onOpenTerminal,
}: InteractivePromptCardProps) {
  const { palette } = useMasilTheme();
  const { t } = useTranslation();
  const [activeIdx, setActiveIdx] = useState(0);
  const [collapsed, setCollapsed] = useState(false);
  const [showScreen, setShowScreen] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // API Permission state
  const [rejectReason, setRejectReason] = useState("");
  const [showRejectReason, setShowRejectReason] = useState(false);
  const [confirmAlways, setConfirmAlways] = useState(false);

  // API Question state
  const [questionStep, setQuestionStep] = useState(0);
  const [apiSelections, setApiSelections] = useState<Record<number, string[]>>(
    {}
  );
  const [apiCustomText, setApiCustomText] = useState<Record<number, string>>(
    {}
  );

  // Live TUI state
  const [tuiChecked, setTuiChecked] = useState<number[]>([]);
  const [tuiCustomOption, setTuiCustomOption] =
    useState<InteractiveOption | null>(null);
  const [tuiCustomInput, setTuiCustomInput] = useState("");
  const [pendingTuiConfirm, setPendingTuiConfirm] =
    useState<InteractiveOption | null>(null);

  const safeIdx = Math.min(activeIdx, Math.max(0, prompts.length - 1));
  const prompt = prompts[safeIdx];
  const promptId = prompt?.id ?? "";

  // Reset per-prompt interactive state whenever the active prompt changes.
  useEffect(() => {
    if (!prompt) return;
    setCollapsed(false);
    setError(null);
    setRejectReason("");
    setShowRejectReason(false);
    setConfirmAlways(false);
    setQuestionStep(0);
    setApiSelections({});
    setApiCustomText({});
    setTuiCustomOption(null);
    setTuiCustomInput("");
    setPendingTuiConfirm(null);
    setTuiChecked(
      prompt.options.filter((o) => o.checked === true).map((o) => o.index)
    );
  }, [promptId]);

  useEffect(() => {
    if (activeIdx >= prompts.length && prompts.length > 0) {
      setActiveIdx(prompts.length - 1);
    }
  }, [activeIdx, prompts.length]);

  const kindMeta = useMemo(() => {
    if (!prompt) {
      return {
        label: t("needsInput"),
        color: palette.warning,
        bg: palette.warningSoft,
      };
    }
    switch (prompt.kind) {
      case "permission":
      case "confirmation":
        return {
          label: t("approval"),
          color: palette.warning,
          bg: palette.warningSoft,
        };
      case "question":
        return {
          label: t("question"),
          color: palette.accent,
          bg: palette.accentSoft,
        };
      default:
        return {
          label: t("needsInput"),
          color: palette.warning,
          bg: palette.warningSoft,
        };
    }
  }, [prompt?.kind, palette, t]);

  if (!prompt) return null;

  const runAction = async (payload: PromptRespondPayload) => {
    if (busy) return;
    setBusy(true);
    setError(null);
    try {
      await onRespond(payload);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  const modeLabel =
    prompt.mode === "structured_api" ? t("promptApiMode") : t("promptTuiMode");

  const renderKindIcon = () => {
    if (prompt.kind === "question") {
      return <HelpCircle size={15} color={kindMeta.color} strokeWidth={2.1} />;
    }
    if (prompt.kind === "input") {
      return (
        <MessageSquareText size={15} color={kindMeta.color} strokeWidth={2.1} />
      );
    }
    return <ShieldAlert size={15} color={kindMeta.color} strokeWidth={2.1} />;
  };

  if (collapsed) {
    return (
      <Pressable
        onPress={() => setCollapsed(false)}
        style={[
          styles.collapsedBar,
          {
            backgroundColor: palette.panel,
            borderTopColor: kindMeta.color,
            borderBottomColor: palette.hairline,
          },
        ]}
      >
        <View style={styles.collapsedLeft}>
          {renderKindIcon()}
          <View style={[styles.kindChip, { backgroundColor: kindMeta.bg }]}>
            <Text style={[styles.kindChipText, { color: kindMeta.color }]}>
              {kindMeta.label}
            </Text>
          </View>
          <Text
            numberOfLines={1}
            style={[styles.collapsedTitle, { color: palette.text }]}
          >
            {prompt.title}
          </Text>
        </View>
        <View style={styles.collapsedRight}>
          {prompts.length > 1 ? (
            <Text style={[styles.pagerNum, { color: palette.muted }]}>
              {safeIdx + 1}/{prompts.length}
            </Text>
          ) : null}
          <ChevronUp size={16} color={palette.muted} />
        </View>
      </Pressable>
    );
  }

  // --- Structured API Question helpers ---
  const apiQuestions = prompt.questions ?? [];
  const safeQStep = Math.min(questionStep, Math.max(0, apiQuestions.length - 1));
  const currentApiQ = apiQuestions[safeQStep];

  const toggleApiOption = (qIdx: number, label: string, multiple: boolean) => {
    setApiSelections((prev) => {
      const cur = prev[qIdx] ?? [];
      if (multiple) {
        const next = cur.includes(label)
          ? cur.filter((item) => item !== label)
          : [...cur, label];
        return { ...prev, [qIdx]: next };
      }
      return { ...prev, [qIdx]: cur[0] === label ? [] : [label] };
    });
  };

  const canSubmitApiQuestions =
    apiQuestions.length > 0 &&
    apiQuestions.every((_, idx) => {
      const selected = apiSelections[idx] ?? [];
      const custom = (apiCustomText[idx] ?? "").trim();
      return selected.length > 0 || custom.length > 0;
    });

  const submitApiQuestions = () => {
    if (!prompt.request_id || !canSubmitApiQuestions) return;
    const grouped: string[][] = apiQuestions.map((q, idx) => {
      const selected = [...(apiSelections[idx] ?? [])];
      const custom = (apiCustomText[idx] ?? "").trim();
      if (q.multiple) {
        if (custom && !selected.includes(custom)) selected.push(custom);
        return selected;
      }
      if (custom) return [custom];
      return selected.slice(0, 1);
    });
    const usesMulti =
      apiQuestions.some((q) => q.multiple) ||
      grouped.some((items) => items.length > 1);
    if (usesMulti) {
      void runAction({
        action: "api_question",
        request_id: prompt.request_id,
        multi_answers: grouped,
      });
    } else {
      void runAction({
        action: "api_question",
        request_id: prompt.request_id,
        answers: grouped.map((items) => items[0] ?? ""),
      });
    }
  };

  // --- Live TUI helpers ---
  const handleTuiOptionTap = (opt: InteractiveOption) => {
    if (busy) return;
    if (opt.is_custom_input) {
      setTuiCustomOption((prev) => (prev?.index === opt.index ? null : opt));
      return;
    }
    if (prompt.multi_select) {
      setTuiChecked((prev) =>
        prev.includes(opt.index)
          ? prev.filter((i) => i !== opt.index)
          : [...prev, opt.index]
      );
      return;
    }
    if (prompt.kind === "permission" || prompt.kind === "confirmation") {
      setPendingTuiConfirm(opt);
      return;
    }
    void runAction({
      action: "tui_select_option",
      option_index: opt.index,
      current_selected_index: prompt.selected_index,
      hotkey: opt.hotkey ?? undefined,
      submit: true,
    });
  };

  const submitTuiCustomText = () => {
    const trimmed = tuiCustomInput.trim();
    if (!trimmed || busy) return;
    void runAction({
      action: "tui_custom_text",
      option_index: tuiCustomOption?.index,
      text: trimmed,
      submit: true,
    });
  };

  const isApiPermission =
    prompt.mode === "structured_api" && prompt.kind === "permission";
  const isApiQuestion =
    prompt.mode === "structured_api" &&
    prompt.kind === "question" &&
    apiQuestions.length > 0;
  const isTui = prompt.mode === "terminal_tui";

  return (
    <View
      style={[
        styles.dock,
        {
          backgroundColor: palette.panel,
          borderTopColor: kindMeta.color,
          borderBottomColor: palette.hairline,
        },
      ]}
    >
      {/* Card Header */}
      <View style={styles.headerRow}>
        <View style={styles.headerBadges}>
          {renderKindIcon()}
          <View style={[styles.kindChip, { backgroundColor: kindMeta.bg }]}>
            <Text style={[styles.kindChipText, { color: kindMeta.color }]}>
              {kindMeta.label}
            </Text>
          </View>
          <View
            style={[
              styles.metaChip,
              {
                backgroundColor: palette.raised,
                borderColor: palette.hairline,
              },
            ]}
          >
            <Text style={[styles.metaChipText, { color: palette.textSecondary }]}>
              {modeLabel}
            </Text>
          </View>
          {prompt.step_label ? (
            <View
              style={[
                styles.metaChip,
                {
                  backgroundColor: palette.raised,
                  borderColor: palette.hairline,
                },
              ]}
            >
              <Text
                style={[styles.metaChipText, { color: palette.textSecondary }]}
              >
                {prompt.step_label}
              </Text>
            </View>
          ) : null}
          {prompt.is_subagent ? (
            <View
              style={[
                styles.metaChip,
                {
                  backgroundColor: palette.accentSoft,
                  borderColor: palette.accentBorder,
                },
              ]}
            >
              <Text style={[styles.metaChipText, { color: palette.accent }]}>
                {t("promptSubagent")}
              </Text>
            </View>
          ) : null}
          {prompt.risk ? (() => {
            const tone = riskTierTone(prompt.risk.tier);
            const rColor =
              tone === "success"
                ? palette.success
                : tone === "warning"
                  ? palette.warning
                  : tone === "danger"
                    ? palette.danger
                    : palette.muted;
            const rBg =
              tone === "success"
                ? palette.successSoft
                : tone === "warning"
                  ? palette.warningSoft
                  : tone === "danger"
                    ? palette.dangerSoft
                    : palette.raised;
            const tierKey =
              prompt.risk.tier === "low"
                ? "riskTierLow"
                : prompt.risk.tier === "medium"
                  ? "riskTierMedium"
                  : prompt.risk.tier === "high"
                    ? "riskTierHigh"
                    : "riskTierCritical";
            return (
              <View
                style={[
                  styles.metaChip,
                  { backgroundColor: rBg, borderColor: rColor },
                ]}
              >
                <Text style={[styles.metaChipText, { color: rColor }]}>
                  {t(tierKey)}
                </Text>
              </View>
            );
          })() : null}
        </View>

        <View style={styles.headerRight}>
          {prompts.length > 1 ? (
            <View style={styles.pager}>
              <Pressable
                onPress={() => setActiveIdx((i) => Math.max(0, i - 1))}
                disabled={safeIdx === 0}
                hitSlop={6}
                style={[styles.pagerBtn, safeIdx === 0 && { opacity: 0.35 }]}
              >
                <ChevronLeft size={14} color={palette.text} />
              </Pressable>
              <Text style={[styles.pagerNum, { color: palette.textSecondary }]}>
                {safeIdx + 1}/{prompts.length}
              </Text>
              <Pressable
                onPress={() =>
                  setActiveIdx((i) => Math.min(prompts.length - 1, i + 1))
                }
                disabled={safeIdx >= prompts.length - 1}
                hitSlop={6}
                style={[
                  styles.pagerBtn,
                  safeIdx >= prompts.length - 1 && { opacity: 0.35 },
                ]}
              >
                <ChevronRight size={14} color={palette.text} />
              </Pressable>
            </View>
          ) : null}
          {prompt.screen_excerpt ? (
            <Pressable
              onPress={() => setShowScreen((v) => !v)}
              hitSlop={6}
              style={[styles.iconBtn, { backgroundColor: palette.raised }]}
              accessibilityLabel={
                showScreen ? t("promptHideScreen") : t("promptShowScreen")
              }
            >
              {showScreen ? (
                <EyeOff size={14} color={palette.accent} />
              ) : (
                <Eye size={14} color={palette.muted} />
              )}
            </Pressable>
          ) : null}
          <Pressable
            onPress={() => setCollapsed(true)}
            hitSlop={6}
            style={[styles.iconBtn, { backgroundColor: palette.raised }]}
            accessibilityLabel={t("promptCollapse")}
          >
            <ChevronDown size={15} color={palette.muted} />
          </Pressable>
        </View>
      </View>

      {/* Scrollable Body */}
      <ScrollView
        style={styles.bodyScroll}
        contentContainerStyle={styles.bodyContent}
        keyboardShouldPersistTaps="handled"
        nestedScrollEnabled
      >
        {!isApiQuestion ? (
          <View style={styles.promptTextBlock}>
            <Text style={[styles.titleText, { color: palette.text }]}>
              {prompt.title}
            </Text>
            {prompt.risk?.truncated ? (
              <Text style={[styles.metaChipText, { color: palette.warning }]}>
                ⚠ {t("riskTruncatedWarn")}
              </Text>
            ) : null}
            {prompt.risk?.matched_rule_id ? (
              <Text style={[styles.metaChipText, { color: palette.accent }]}>
                {t("policyMatchedRule", {
                  id: prompt.risk.matched_rule_id,
                  action: prompt.risk.matched_action ?? "",
                })}
              </Text>
            ) : null}
            {prompt.body && prompt.body !== prompt.title ? (
              <Text
                selectable
                style={[styles.bodyText, { color: palette.textSecondary }]}
              >
                {prompt.body}
              </Text>
            ) : null}
          </View>
        ) : null}

        {showScreen && prompt.screen_excerpt ? (
          <View
            style={[
              styles.screenBox,
              {
                backgroundColor: palette.canvas,
                borderColor: palette.borderSubtle,
              },
            ]}
          >
            <Text
              selectable
              style={[styles.screenMono, { color: palette.textSecondary }]}
            >
              {prompt.screen_excerpt}
            </Text>
          </View>
        ) : null}

        {/* 1. Structured API Permission */}
        {isApiPermission && prompt.request_id ? (
          <View style={styles.section}>
            {(prompt.always_patterns?.length ?? 0) > 0 ? (
              <View style={styles.patternRow}>
                {prompt.always_patterns!.map((pat, idx) => (
                  <View
                    key={`${pat}-${idx}`}
                    style={[
                      styles.patternPill,
                      {
                        backgroundColor: palette.canvas,
                        borderColor: palette.hairline,
                      },
                    ]}
                  >
                    <Text
                      numberOfLines={1}
                      style={[styles.patternText, { color: palette.text }]}
                    >
                      {pat}
                    </Text>
                  </View>
                ))}
              </View>
            ) : null}

            {(prompt.session_peers_count ?? 0) > 0 ? (
              <Text style={[styles.peerWarn, { color: palette.warning }]}>
                {t("promptPeersWarning", { count: prompt.session_peers_count })}
              </Text>
            ) : null}

            {showRejectReason ? (
              <TextInput
                value={rejectReason}
                onChangeText={setRejectReason}
                placeholder={t("promptRejectReasonPlaceholder")}
                placeholderTextColor={palette.muted}
                style={[
                  styles.textInput,
                  {
                    backgroundColor: palette.canvas,
                    borderColor: palette.borderSubtle,
                    color: palette.text,
                  },
                ]}
              />
            ) : null}

            <View style={styles.actionRow}>
              <Pressable
                disabled={busy}
                onPress={() =>
                  void runAction({
                    action: "api_permission",
                    request_id: prompt.request_id!,
                    decision: "once",
                  })
                }
                style={[
                  styles.primaryBtn,
                  { backgroundColor: palette.accent, opacity: busy ? 0.6 : 1 },
                ]}
              >
                <Check size={14} color={palette.accentText} strokeWidth={2.4} />
                <Text
                  style={[styles.primaryBtnText, { color: palette.accentText }]}
                >
                  {t("promptAllowOnce")}
                </Text>
              </Pressable>

              <Pressable
                disabled={busy}
                onPress={() => setConfirmAlways(true)}
                style={[
                  styles.secondaryBtn,
                  {
                    backgroundColor: palette.raised,
                    borderColor: palette.borderSubtle,
                    opacity: busy ? 0.6 : 1,
                  },
                ]}
              >
                <Text style={[styles.secondaryBtnText, { color: palette.text }]}>
                  {t("promptAlwaysAllow")}
                </Text>
              </Pressable>

              <Pressable
                disabled={busy}
                onPress={() => {
                  if (!showRejectReason) {
                    setShowRejectReason(true);
                    return;
                  }
                  void runAction({
                    action: "api_permission",
                    request_id: prompt.request_id!,
                    decision: "reject",
                    message: rejectReason.trim() || undefined,
                  });
                }}
                style={[
                  styles.secondaryBtn,
                  {
                    backgroundColor: palette.dangerSoft,
                    borderColor: palette.danger,
                    opacity: busy ? 0.6 : 1,
                  },
                ]}
              >
                <Text
                  style={[styles.secondaryBtnText, { color: palette.danger }]}
                >
                  {t("promptReject")}
                </Text>
              </Pressable>
            </View>
          </View>
        ) : null}

        {/* 2. Structured API Question */}
        {isApiQuestion && currentApiQ && prompt.request_id ? (
          <View style={styles.section}>
            {apiQuestions.length > 1 ? (
              <ScrollView
                horizontal
                showsHorizontalScrollIndicator={false}
                contentContainerStyle={styles.stepTabs}
              >
                {apiQuestions.map((q, idx) => {
                  const active = idx === safeQStep;
                  const answered =
                    (apiSelections[idx]?.length ?? 0) > 0 ||
                    (apiCustomText[idx] ?? "").trim().length > 0;
                  return (
                    <Pressable
                      key={`qtab-${idx}`}
                      onPress={() => setQuestionStep(idx)}
                      style={[
                        styles.stepTab,
                        {
                          backgroundColor: active
                            ? palette.accentSoft
                            : palette.raised,
                          borderColor: active
                            ? palette.accent
                            : palette.hairline,
                        },
                      ]}
                    >
                      <Text
                        style={[
                          styles.stepTabText,
                          {
                            color: active
                              ? palette.accent
                              : answered
                                ? palette.text
                                : palette.muted,
                          },
                        ]}
                      >
                        {idx + 1}. {q.header || `Q${idx + 1}`}
                        {answered ? " ✓" : ""}
                      </Text>
                    </Pressable>
                  );
                })}
              </ScrollView>
            ) : null}

            <View style={styles.promptTextBlock}>
              {currentApiQ.header ? (
                <Text style={[styles.qHeader, { color: palette.accent }]}>
                  {currentApiQ.header}
                </Text>
              ) : null}
              <Text style={[styles.titleText, { color: palette.text }]}>
                {currentApiQ.question}
              </Text>
            </View>

            <View style={styles.optionList}>
              {currentApiQ.options.map((opt, idx) => {
                const selected = (apiSelections[safeQStep] ?? []).includes(
                  opt.label
                );
                return (
                  <Pressable
                    key={`${opt.label}-${idx}`}
                    disabled={busy}
                    onPress={() =>
                      toggleApiOption(safeQStep, opt.label, currentApiQ.multiple)
                    }
                    style={[
                      styles.optionCard,
                      {
                        backgroundColor: selected
                          ? palette.accentSoft
                          : palette.canvas,
                        borderColor: selected
                          ? palette.accent
                          : palette.hairline,
                      },
                    ]}
                  >
                    <View style={styles.optionIconWrap}>
                      {currentApiQ.multiple ? (
                        selected ? (
                          <SquareCheck size={16} color={palette.accent} />
                        ) : (
                          <Square size={16} color={palette.muted} />
                        )
                      ) : (
                        <View
                          style={[
                            styles.radioOuter,
                            {
                              borderColor: selected
                                ? palette.accent
                                : palette.muted,
                            },
                          ]}
                        >
                          {selected ? (
                            <View
                              style={[
                                styles.radioInner,
                                { backgroundColor: palette.accent },
                              ]}
                            />
                          ) : null}
                        </View>
                      )}
                    </View>
                    <View style={styles.optionTextCol}>
                      <Text style={[styles.optionLabel, { color: palette.text }]}>
                        {opt.label}
                      </Text>
                      {opt.description ? (
                        <Text
                          style={[styles.optionDesc, { color: palette.muted }]}
                        >
                          {opt.description}
                        </Text>
                      ) : null}
                    </View>
                  </Pressable>
                );
              })}
            </View>

            {currentApiQ.custom !== false ? (
              <TextInput
                value={apiCustomText[safeQStep] ?? ""}
                onChangeText={(txt) =>
                  setApiCustomText((prev) => ({ ...prev, [safeQStep]: txt }))
                }
                placeholder={t("promptCustomPlaceholder")}
                placeholderTextColor={palette.muted}
                style={[
                  styles.textInput,
                  {
                    backgroundColor: palette.canvas,
                    borderColor: palette.borderSubtle,
                    color: palette.text,
                  },
                ]}
              />
            ) : null}

            <View style={styles.actionRow}>
              {safeQStep < apiQuestions.length - 1 ? (
                <Pressable
                  onPress={() => setQuestionStep((s) => s + 1)}
                  style={[
                    styles.primaryBtn,
                    { backgroundColor: palette.accent },
                  ]}
                >
                  <Text
                    style={[
                      styles.primaryBtnText,
                      { color: palette.accentText },
                    ]}
                  >
                    {t("promptNext")}
                  </Text>
                </Pressable>
              ) : (
                <Pressable
                  disabled={busy || !canSubmitApiQuestions}
                  onPress={submitApiQuestions}
                  style={[
                    styles.primaryBtn,
                    {
                      backgroundColor: canSubmitApiQuestions
                        ? palette.accent
                        : palette.disabledBg,
                      opacity: busy ? 0.6 : 1,
                    },
                  ]}
                >
                  <Send
                    size={13}
                    color={
                      canSubmitApiQuestions
                        ? palette.accentText
                        : palette.disabledText
                    }
                  />
                  <Text
                    style={[
                      styles.primaryBtnText,
                      {
                        color: canSubmitApiQuestions
                          ? palette.accentText
                          : palette.disabledText,
                      },
                    ]}
                  >
                    {t("promptSubmit")}
                  </Text>
                </Pressable>
              )}

              <Pressable
                disabled={busy}
                onPress={() =>
                  void runAction({
                    action: "api_question",
                    request_id: prompt.request_id!,
                    reject: true,
                  })
                }
                style={[
                  styles.secondaryBtn,
                  {
                    backgroundColor: palette.raised,
                    borderColor: palette.borderSubtle,
                  },
                ]}
              >
                <Text style={[styles.secondaryBtnText, { color: palette.text }]}>
                  {t("promptDismiss")}
                </Text>
              </Pressable>
            </View>
          </View>
        ) : null}

        {/* 3. Live TUI Free-text Only */}
        {isTui && prompt.free_text_only ? (
          <View style={styles.section}>
            <View style={styles.customRow}>
              <TextInput
                value={tuiCustomInput}
                onChangeText={setTuiCustomInput}
                placeholder={t("promptCustomPlaceholder")}
                placeholderTextColor={palette.muted}
                onSubmitEditing={submitTuiCustomText}
                style={[
                  styles.textInput,
                  {
                    flex: 1,
                    backgroundColor: palette.canvas,
                    borderColor: palette.borderSubtle,
                    color: palette.text,
                  },
                ]}
              />
              <Pressable
                disabled={busy || !tuiCustomInput.trim()}
                onPress={submitTuiCustomText}
                style={[
                  styles.sendSquareBtn,
                  {
                    backgroundColor: tuiCustomInput.trim()
                      ? palette.accent
                      : palette.disabledBg,
                  },
                ]}
              >
                <Send
                  size={15}
                  color={
                    tuiCustomInput.trim()
                      ? palette.accentText
                      : palette.disabledText
                  }
                />
              </Pressable>
            </View>
          </View>
        ) : null}

        {/* 4. Live TUI Options (Single-select or Multi-select) */}
        {isTui && !prompt.free_text_only && prompt.options.length > 0 ? (
          <View style={styles.section}>
            <View style={styles.optionList}>
              {prompt.options.map((opt) => {
                const isChecked = tuiChecked.includes(opt.index);
                const isCustomOpen = tuiCustomOption?.index === opt.index;
                const highlight = prompt.multi_select
                  ? isChecked
                  : isCustomOpen || opt.selected;
                const badgeText =
                  opt.hotkey &&
                  !opt.hotkey.startsWith("\x1b") &&
                  opt.hotkey !== "\r"
                    ? opt.hotkey.replace("\r", "")
                    : String(opt.index + 1);
                return (
                  <Pressable
                    key={`tui-opt-${opt.index}-${opt.label}`}
                    disabled={busy}
                    onPress={() => handleTuiOptionTap(opt)}
                    style={[
                      styles.optionCard,
                      {
                        backgroundColor: highlight
                          ? palette.accentSoft
                          : palette.canvas,
                        borderColor: highlight
                          ? palette.accent
                          : palette.hairline,
                        opacity: busy ? 0.65 : 1,
                      },
                    ]}
                  >
                    {prompt.multi_select && !opt.is_custom_input ? (
                      <View style={styles.optionIconWrap}>
                        {isChecked ? (
                          <SquareCheck size={16} color={palette.accent} />
                        ) : (
                          <Square size={16} color={palette.muted} />
                        )}
                      </View>
                    ) : (
                      <View
                        style={[
                          styles.idxBadge,
                          {
                            backgroundColor: highlight
                              ? palette.accent
                              : palette.raised,
                          },
                        ]}
                      >
                        <Text
                          style={[
                            styles.idxBadgeText,
                            {
                              color: highlight
                                ? palette.accentText
                                : palette.textSecondary,
                            },
                          ]}
                        >
                          {badgeText}
                        </Text>
                      </View>
                    )}
                    <View style={styles.optionTextCol}>
                      <Text
                        style={[
                          styles.optionLabel,
                          {
                            color: opt.is_destructive
                              ? palette.danger
                              : palette.text,
                          },
                        ]}
                      >
                        {opt.label}
                      </Text>
                      {opt.description ? (
                        <Text
                          style={[styles.optionDesc, { color: palette.muted }]}
                        >
                          {opt.description}
                        </Text>
                      ) : null}
                    </View>
                  </Pressable>
                );
              })}
            </View>

            {tuiCustomOption ? (
              <View style={styles.customRow}>
                <TextInput
                  value={tuiCustomInput}
                  onChangeText={setTuiCustomInput}
                  placeholder={t("promptCustomPlaceholder")}
                  placeholderTextColor={palette.muted}
                  autoFocus
                  onSubmitEditing={submitTuiCustomText}
                  style={[
                    styles.textInput,
                    {
                      flex: 1,
                      backgroundColor: palette.canvas,
                      borderColor: palette.accent,
                      color: palette.text,
                    },
                  ]}
                />
                <Pressable
                  disabled={busy || !tuiCustomInput.trim()}
                  onPress={submitTuiCustomText}
                  style={[
                    styles.sendSquareBtn,
                    {
                      backgroundColor: tuiCustomInput.trim()
                        ? palette.accent
                        : palette.disabledBg,
                    },
                  ]}
                >
                  <Send
                    size={15}
                    color={
                      tuiCustomInput.trim()
                        ? palette.accentText
                        : palette.disabledText
                    }
                  />
                </Pressable>
              </View>
            ) : null}

            {prompt.multi_select ? (
              <View style={styles.actionRow}>
                <Pressable
                  disabled={busy}
                  onPress={() =>
                    void runAction({
                      action: "tui_toggle_options",
                      indices: tuiChecked,
                      submit: true,
                    })
                  }
                  style={[
                    styles.primaryBtn,
                    { backgroundColor: palette.accent, opacity: busy ? 0.6 : 1 },
                  ]}
                >
                  <Check
                    size={14}
                    color={palette.accentText}
                    strokeWidth={2.4}
                  />
                  <Text
                    style={[
                      styles.primaryBtnText,
                      { color: palette.accentText },
                    ]}
                  >
                    {t("promptSubmitSelection")} ({tuiChecked.length})
                  </Text>
                </Pressable>
              </View>
            ) : null}
          </View>
        ) : null}

        {prompt.hint_line ? (
          <Text
            numberOfLines={2}
            style={[styles.hintLine, { color: palette.muted }]}
          >
            {prompt.hint_line}
          </Text>
        ) : null}

        {error ? (
          <Text style={[styles.errorText, { color: palette.danger }]}>
            {error}
          </Text>
        ) : null}
      </ScrollView>

      {/* Compact Live TUI Key Strip */}
      {isTui ? (
        <View
          style={[
            styles.tuiFooter,
            { borderTopColor: palette.hairline, backgroundColor: palette.panel },
          ]}
        >
          <ScrollView
            horizontal
            showsHorizontalScrollIndicator={false}
            contentContainerStyle={styles.keyStrip}
          >
            {[
              { label: "Esc", key: "Escape" },
              { label: "Enter", key: "Enter" },
              ...(prompt.can_navigate_tabs
                ? [{ label: "Tab", key: "Tab" }]
                : []),
              ...(prompt.multi_select
                ? [{ label: "Space", key: "Space" }]
                : []),
              { label: "↑", key: "Up" },
              { label: "↓", key: "Down" },
            ].map((item) => (
              <Pressable
                key={item.key}
                disabled={busy}
                onPress={() =>
                  void runAction({ action: "tui_keys", keys: [item.key] })
                }
                style={[
                  styles.keyPill,
                  {
                    backgroundColor: palette.raised,
                    borderColor: palette.hairline,
                    opacity: busy ? 0.5 : 1,
                  },
                ]}
              >
                <Text style={[styles.keyPillText, { color: palette.text }]}>
                  {item.label}
                </Text>
              </Pressable>
            ))}
            <Pressable
              onPress={onOpenTerminal}
              style={[
                styles.keyPill,
                styles.termPill,
                {
                  backgroundColor: palette.raised,
                  borderColor: palette.borderSubtle,
                },
              ]}
            >
              <Terminal size={12} color={palette.textSecondary} />
              <Text
                style={[styles.keyPillText, { color: palette.textSecondary }]}
              >
                {t("terminal")}
              </Text>
            </Pressable>
          </ScrollView>
          {busy ? (
            <ActivityIndicator
              size="small"
              color={palette.accent}
              style={{ marginLeft: 8 }}
            />
          ) : null}
        </View>
      ) : busy ? (
        <View style={styles.apiBusyBar}>
          <ActivityIndicator size="small" color={palette.accent} />
        </View>
      ) : null}

      {/* Confirmation for "Always allow" patterns */}
      <ConfirmModal
        visible={confirmAlways}
        title={t("promptAlwaysAllow")}
        body={t("promptAlwaysConfirmBody", {
          name: agentName,
          patterns: (prompt.always_patterns ?? []).join(", ") || prompt.title,
        })}
        onCancel={() => setConfirmAlways(false)}
        onConfirm={() => {
          setConfirmAlways(false);
          if (prompt.request_id) {
            void runAction({
              action: "api_permission",
              request_id: prompt.request_id,
              decision: "always",
            });
          }
        }}
      />

      {/* Confirmation for raw TUI permission/confirmation key delivery */}
      <ConfirmModal
        visible={pendingTuiConfirm !== null}
        title={t("confirmActionTitle")}
        body={t("promptConfirmTuiOption", {
          name: agentName,
          option: pendingTuiConfirm?.label ?? "",
        })}
        destructive={Boolean(pendingTuiConfirm?.is_destructive)}
        onCancel={() => setPendingTuiConfirm(null)}
        onConfirm={() => {
          const chosen = pendingTuiConfirm;
          setPendingTuiConfirm(null);
          if (!chosen) return;
          void runAction({
            action: "tui_select_option",
            option_index: chosen.index,
            current_selected_index: prompt.selected_index,
            hotkey: chosen.hotkey ?? undefined,
            submit: true,
          });
        }}
      />
    </View>
  );
}

const styles = StyleSheet.create({
  collapsedBar: {
    flexDirection: "row",
    alignItems: "center",
    justifyContent: "space-between",
    paddingHorizontal: 12,
    paddingVertical: 9,
    borderTopWidth: 2,
    borderBottomWidth: 1,
    gap: 8,
  },
  collapsedLeft: {
    flexDirection: "row",
    alignItems: "center",
    gap: 7,
    flex: 1,
    minWidth: 0,
  },
  collapsedTitle: {
    fontFamily: FONTS.semi,
    fontSize: 13,
    flex: 1,
  },
  collapsedRight: {
    flexDirection: "row",
    alignItems: "center",
    gap: 6,
  },
  dock: {
    borderTopWidth: 2,
    borderBottomWidth: 1,
    maxHeight: 400,
  },
  headerRow: {
    flexDirection: "row",
    alignItems: "center",
    justifyContent: "space-between",
    paddingHorizontal: 12,
    paddingTop: 9,
    paddingBottom: 6,
    gap: 8,
  },
  headerBadges: {
    flexDirection: "row",
    alignItems: "center",
    flexWrap: "wrap",
    gap: 6,
    flex: 1,
  },
  headerRight: {
    flexDirection: "row",
    alignItems: "center",
    gap: 6,
  },
  kindChip: {
    paddingHorizontal: 7,
    paddingVertical: 2,
    borderRadius: 999,
  },
  kindChipText: {
    fontFamily: FONTS.bold,
    fontSize: 11,
  },
  metaChip: {
    paddingHorizontal: 6,
    paddingVertical: 2,
    borderRadius: 6,
    borderWidth: 1,
  },
  metaChipText: {
    fontFamily: FONTS.mono,
    fontSize: 10.5,
  },
  pager: {
    flexDirection: "row",
    alignItems: "center",
    gap: 2,
  },
  pagerBtn: {
    padding: 3,
  },
  pagerNum: {
    fontFamily: FONTS.mono,
    fontVariant: TABULAR_NUMS,
    fontSize: 11,
  },
  iconBtn: {
    width: 26,
    height: 26,
    borderRadius: 7,
    alignItems: "center",
    justifyContent: "center",
  },
  bodyScroll: {
    maxHeight: 280,
  },
  bodyContent: {
    paddingHorizontal: 12,
    paddingBottom: 10,
    gap: 9,
  },
  promptTextBlock: {
    gap: 3,
  },
  qHeader: {
    fontFamily: FONTS.semi,
    fontSize: 11.5,
  },
  titleText: {
    fontFamily: FONTS.bold,
    fontSize: 14,
    lineHeight: 20,
  },
  bodyText: {
    fontFamily: FONTS.mono,
    fontSize: 12,
    lineHeight: 17.5,
  },
  screenBox: {
    borderWidth: 1,
    borderRadius: 8,
    padding: 8,
  },
  screenMono: {
    fontFamily: FONTS.mono,
    fontSize: 10.5,
    lineHeight: 15,
  },
  section: {
    gap: 8,
  },
  patternRow: {
    flexDirection: "row",
    flexWrap: "wrap",
    gap: 6,
  },
  patternPill: {
    borderWidth: 1,
    borderRadius: 6,
    paddingHorizontal: 7,
    paddingVertical: 3,
    maxWidth: "100%",
  },
  patternText: {
    fontFamily: FONTS.mono,
    fontSize: 11,
  },
  peerWarn: {
    fontFamily: FONTS.regular,
    fontSize: 11.5,
    lineHeight: 16,
  },
  stepTabs: {
    gap: 6,
  },
  stepTab: {
    paddingHorizontal: 9,
    paddingVertical: 5,
    borderRadius: 8,
    borderWidth: 1,
  },
  stepTabText: {
    fontFamily: FONTS.semi,
    fontSize: 11.5,
  },
  optionList: {
    gap: 6,
  },
  optionCard: {
    flexDirection: "row",
    alignItems: "flex-start",
    gap: 9,
    borderWidth: 1,
    borderRadius: 10,
    paddingHorizontal: 10,
    paddingVertical: 9,
  },
  optionIconWrap: {
    paddingTop: 1,
  },
  radioOuter: {
    width: 16,
    height: 16,
    borderRadius: 8,
    borderWidth: 1.8,
    alignItems: "center",
    justifyContent: "center",
  },
  radioInner: {
    width: 8,
    height: 8,
    borderRadius: 4,
  },
  idxBadge: {
    minWidth: 21,
    height: 21,
    borderRadius: 6,
    paddingHorizontal: 5,
    alignItems: "center",
    justifyContent: "center",
  },
  idxBadgeText: {
    fontFamily: FONTS.monoSemi,
    fontSize: 11,
  },
  optionTextCol: {
    flex: 1,
    gap: 2,
  },
  optionLabel: {
    fontFamily: FONTS.semi,
    fontSize: 13.5,
    lineHeight: 18.5,
  },
  optionDesc: {
    fontFamily: FONTS.regular,
    fontSize: 12,
    lineHeight: 16.5,
  },
  customRow: {
    flexDirection: "row",
    alignItems: "center",
    gap: 6,
  },
  textInput: {
    borderWidth: 1,
    borderRadius: 9,
    paddingHorizontal: 10,
    paddingVertical: 8,
    fontFamily: FONTS.regular,
    fontSize: 13.5,
  },
  sendSquareBtn: {
    width: 38,
    height: 38,
    borderRadius: 9,
    alignItems: "center",
    justifyContent: "center",
  },
  actionRow: {
    flexDirection: "row",
    flexWrap: "wrap",
    gap: 7,
  },
  primaryBtn: {
    flexDirection: "row",
    alignItems: "center",
    justifyContent: "center",
    gap: 5,
    paddingHorizontal: 13,
    paddingVertical: 9,
    borderRadius: 9,
  },
  primaryBtnText: {
    fontFamily: FONTS.bold,
    fontSize: 13,
  },
  secondaryBtn: {
    flexDirection: "row",
    alignItems: "center",
    justifyContent: "center",
    paddingHorizontal: 11,
    paddingVertical: 9,
    borderRadius: 9,
    borderWidth: 1,
  },
  secondaryBtnText: {
    fontFamily: FONTS.semi,
    fontSize: 12.5,
  },
  hintLine: {
    fontFamily: FONTS.mono,
    fontSize: 10.5,
    lineHeight: 15,
  },
  errorText: {
    fontFamily: FONTS.semi,
    fontSize: 12,
    lineHeight: 17,
  },
  tuiFooter: {
    flexDirection: "row",
    alignItems: "center",
    borderTopWidth: 1,
    paddingHorizontal: 10,
    paddingVertical: 6,
  },
  keyStrip: {
    gap: 6,
    alignItems: "center",
  },
  keyPill: {
    paddingHorizontal: 9,
    paddingVertical: 5,
    borderRadius: 7,
    borderWidth: 1,
  },
  termPill: {
    flexDirection: "row",
    alignItems: "center",
    gap: 4,
  },
  keyPillText: {
    fontFamily: FONTS.monoSemi,
    fontSize: 11.5,
  },
  apiBusyBar: {
    paddingBottom: 8,
    alignItems: "center",
  },
});
