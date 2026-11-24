import React, { useEffect, useMemo, useRef, useState } from "react";
import {
  ActivityIndicator,
  FlatList,
  Pressable,
  RefreshControl,
  StyleSheet,
  Text,
  View,
} from "react-native";
import * as Clipboard from "expo-clipboard";
import { Check, ChevronDown, ChevronRight, Copy, Wrench } from "lucide-react-native";
import type { AgentRow, TimelineItem, TimelinePage } from "../protocol/schemas";
import { useMasilTheme } from "../theme/ThemeProvider";
import { FONTS, TABULAR_NUMS } from "../theme/typography";
import { ApprovalBanner } from "./ApprovalBanner";
import { ToolCallCard } from "./ToolCallCard";

interface ConversationViewProps {
  agent: AgentRow;
  page: TimelinePage | null;
  refreshing: boolean;
  onRefresh: () => void;
  onApprove: (
    requestId: string | null | undefined,
    decision: string,
    keySequence?: string | null
  ) => void;
  onOpenTerminal: () => void;
}

type ActivitySubItem = Extract<
  TimelineItem,
  { kind: "tool_call" | "thinking" }
>;

type DisplayEntry =
  | { kind: "item"; key: string; item: TimelineItem; showModelLabel?: boolean }
  | { kind: "activity_group"; key: string; items: ActivitySubItem[] };

/**
 * Collapses consecutive `tool_call` and `thinking` items between user prompts
 * and assistant responses into a single lightweight 1-line summary pill so the
 * mobile conversation stays focused on dialogue.
 */
function buildDisplayEntries(items: TimelineItem[]): DisplayEntry[] {
  const out: DisplayEntry[] = [];
  let pending: ActivitySubItem[] = [];
  let lastAssistantModel: string | null = null;

  const flushPending = () => {
    if (pending.length === 0) return;
    const first = pending[0]!;
    const last = pending[pending.length - 1]!;
    out.push({
      kind: "activity_group",
      key: `act-${first.id}-${last.seq}-${pending.length}`,
      items: pending,
    });
    pending = [];
  };

  for (const item of items) {
    if (item.kind === "tool_call" || item.kind === "thinking") {
      pending.push(item);
    } else {
      flushPending();
      let showModelLabel = false;
      if (item.kind === "assistant_message") {
        const modelKey = item.model || "";
        showModelLabel = lastAssistantModel === null || modelKey !== lastAssistantModel;
        lastAssistantModel = modelKey;
      } else if (item.kind === "user_message") {
        lastAssistantModel = null;
      }
      out.push({
        kind: "item",
        key: `${item.kind}-${item.id}-${item.seq}`,
        item,
        showModelLabel,
      });
    }
  }
  flushPending();
  return out;
}

interface ActivitySummaryParts {
  headline: string;
  detail: string;
  failedCount: number;
}

function summarizeActivityGroup(items: ActivitySubItem[]): ActivitySummaryParts {
  const toolCounts = new Map<string, number>();
  let totalTools = 0;
  let failedTools = 0;
  let thinkingCount = 0;

  for (const item of items) {
    if (item.kind === "thinking") {
      thinkingCount += 1;
    } else {
      totalTools += 1;
      if (item.status === "failed") failedTools += 1;
      toolCounts.set(item.name, (toolCounts.get(item.name) ?? 0) + 1);
    }
  }

  if (totalTools > 0) {
    const topNames = [...toolCounts.entries()]
      .sort((a, b) => b[1] - a[1])
      .slice(0, 3)
      .map(([name, count]) => (count > 1 ? `${name}×${count}` : name))
      .join(", ");
    return {
      headline: `도구 ${totalTools}회 실행`,
      detail: failedTools > 0 ? `${topNames} · 실패 ${failedTools}` : topNames,
      failedCount: failedTools,
    };
  }
  return {
    headline: thinkingCount > 1 ? `생각 ${thinkingCount}단계` : "생각 과정",
    detail: "",
    failedCount: 0,
  };
}

function ActivitySummaryRow({ items }: { items: ActivitySubItem[] }) {
  const { palette } = useMasilTheme();
  const [expanded, setExpanded] = useState(false);
  const summary = useMemo(() => summarizeActivityGroup(items), [items]);
  const hasFailed = summary.failedCount > 0;

  return (
    <View style={styles.activityWrap}>
      <Pressable
        onPress={() => setExpanded((v) => !v)}
        hitSlop={6}
        style={[styles.activityRow, !hasFailed && styles.activityRowQuiet]}
      >
        <Wrench
          size={12}
          color={hasFailed ? palette.danger : palette.muted}
          strokeWidth={1.8}
        />
        <Text
          numberOfLines={1}
          style={[
            styles.activityHeadline,
            { color: hasFailed ? palette.danger : palette.textSecondary },
          ]}
        >
          {summary.headline}
        </Text>
        {summary.detail ? (
          <>
            <Text style={[styles.activityDot, { color: palette.track }]}>·</Text>
            <Text
              numberOfLines={1}
              style={[styles.activityDetail, { color: palette.muted }]}
            >
              {summary.detail}
            </Text>
          </>
        ) : null}
        {expanded ? (
          <ChevronDown size={13} color={palette.muted} />
        ) : (
          <ChevronRight size={13} color={palette.muted} />
        )}
      </Pressable>

      {expanded ? (
        <View style={styles.activityExpanded}>
          {items.map((sub) =>
            sub.kind === "tool_call" ? (
              <ToolCallCard
                key={`${sub.id}-${sub.seq}`}
                name={sub.name}
                summary={sub.summary}
                status={sub.status}
                argsJson={sub.args_json}
                output={sub.output}
              />
            ) : (
              <View
                key={`${sub.id}-${sub.seq}`}
                style={[
                  styles.thinkingBox,
                  {
                    backgroundColor: palette.panel,
                    borderColor: palette.hairline,
                  },
                ]}
              >
                <Text
                  selectable
                  style={[styles.thinkingText, { color: palette.muted }]}
                >
                  {sub.text}
                </Text>
              </View>
            )
          )}
        </View>
      ) : null}
    </View>
  );
}

function renderInlineRuns(
  text: string,
  palette: ReturnType<typeof useMasilTheme>["palette"]
): React.ReactNode[] {
  const nodes: React.ReactNode[] = [];
  const tokenRe = /(`[^`\n]+`|\*\*[^*\n]+\*\*)/g;
  let last = 0;
  let idx = 0;
  let match: RegExpExecArray | null;
  while ((match = tokenRe.exec(text)) !== null) {
    if (match.index > last) {
      nodes.push(text.slice(last, match.index));
    }
    const token = match[0];
    if (token.startsWith("`") && token.endsWith("`")) {
      const code = token.slice(1, -1);
      nodes.push(
        <Text
          key={`c-${idx++}`}
          style={{
            fontFamily: FONTS.mono,
            fontSize: 13,
            backgroundColor: palette.raised,
            color: palette.isDark ? "#9fdce4" : palette.accent,
          }}
        >
          {`\u2009${code}\u2009`}
        </Text>
      );
    } else if (token.startsWith("**") && token.endsWith("**")) {
      nodes.push(
        <Text
          key={`b-${idx++}`}
          style={{ fontFamily: FONTS.semi, color: palette.text }}
        >
          {token.slice(2, -2)}
        </Text>
      );
    }
    last = match.index + token.length;
  }
  if (last < text.length) {
    nodes.push(text.slice(last));
  }
  return nodes;
}

function FormattedMarkdown({
  markdown,
  palette,
}: {
  markdown: string;
  palette: ReturnType<typeof useMasilTheme>["palette"];
}) {
  const segments = useMemo(() => {
    const out: Array<{ kind: "prose" | "code"; text: string }> = [];
    const fenceRe = /```[^\n]*\n([\s\S]*?)```/g;
    let last = 0;
    let m: RegExpExecArray | null;
    while ((m = fenceRe.exec(markdown)) !== null) {
      const before = markdown.slice(last, m.index).trim();
      if (before) out.push({ kind: "prose", text: before });
      out.push({ kind: "code", text: (m[1] ?? "").replace(/\n$/, "") });
      last = m.index + m[0].length;
    }
    const tail = markdown.slice(last).trim();
    if (tail) out.push({ kind: "prose", text: tail });
    if (out.length === 0) out.push({ kind: "prose", text: markdown });
    return out;
  }, [markdown]);

  return (
    <View style={{ gap: 8 }}>
      {segments.map((seg, i) =>
        seg.kind === "code" ? (
          <View
            key={`f-${i}`}
            style={[
              styles.codeBlock,
              { backgroundColor: palette.panel, borderColor: palette.hairline },
            ]}
          >
            <Text selectable style={[styles.codeBlockText, { color: palette.text }]}>
              {seg.text}
            </Text>
          </View>
        ) : (
          <Text
            key={`p-${i}`}
            selectable
            style={[styles.assistantText, { color: palette.text }]}
          >
            {renderInlineRuns(seg.text, palette)}
          </Text>
        )
      )}
    </View>
  );
}

function AssistantMessageBlock({
  markdown,
  modelLabel,
  showModelLabel,
}: {
  markdown: string;
  modelLabel: string;
  showModelLabel: boolean;
}) {
  const { palette } = useMasilTheme();
  const [copied, setCopied] = useState(false);

  const handleCopy = () => {
    void Clipboard.setStringAsync(markdown);
    setCopied(true);
    setTimeout(() => setCopied(false), 1200);
  };

  return (
    <Pressable onLongPress={handleCopy} style={styles.assistantBlock}>
      {showModelLabel ? (
        <View style={styles.assistantTopRow}>
          <Text
            numberOfLines={1}
            style={[styles.modelTag, { color: palette.muted }]}
          >
            {modelLabel}
          </Text>
          <Pressable onPress={handleCopy} hitSlop={8} style={styles.copyBtn}>
            {copied ? (
              <Check size={13} color={palette.success} />
            ) : (
              <Copy size={13} color={palette.muted} />
            )}
          </Pressable>
        </View>
      ) : null}
      <FormattedMarkdown markdown={markdown} palette={palette} />
    </Pressable>
  );
}

export function ConversationView({
  agent,
  page,
  refreshing,
  onRefresh,
  onApprove,
  onOpenTerminal,
}: ConversationViewProps) {
  const { palette } = useMasilTheme();
  const items = page?.items ?? [];
  const entries = useMemo(() => buildDisplayEntries(items), [items]);
  const listRef = useRef<FlatList<DisplayEntry>>(null);
  const prevCount = useRef(0);

  useEffect(() => {
    prevCount.current = 0;
  }, [agent.remote_id]);

  useEffect(() => {
    if (entries.length > prevCount.current && entries.length > 0) {
      setTimeout(() => {
        listRef.current?.scrollToEnd({ animated: prevCount.current > 0 });
      }, 60);
    }
    prevCount.current = entries.length;
  }, [entries.length]);

  const renderEntry = ({ item: entry }: { item: DisplayEntry }) => {
    if (entry.kind === "activity_group") {
      return <ActivitySummaryRow items={entry.items} />;
    }

    const { item } = entry;
    switch (item.kind) {
      case "user_message":
        return (
          <View
            style={[
              styles.userBubble,
              {
                backgroundColor: palette.raised,
              },
            ]}
          >
            <Text selectable style={[styles.msgText, { color: palette.text }]}>
              {item.text}
            </Text>
          </View>
        );
      case "assistant_message":
        return (
          <AssistantMessageBlock
            markdown={item.markdown}
            modelLabel={item.model || agent.provider}
            showModelLabel={Boolean(entry.showModelLabel)}
          />
        );
      case "approval_banner":
        return (
          <ApprovalBanner
            agentName={agent.name}
            prompt={item.prompt}
            mode={item.mode}
            requestId={item.request_id}
            options={item.options}
            onApprove={(opt) =>
              onApprove(item.request_id, opt.id, opt.key_sequence)
            }
            onOpenTerminal={onOpenTerminal}
          />
        );
      case "system_notice":
        return (
          <View
            style={[
              styles.noticeCard,
              { backgroundColor: palette.panel, borderColor: palette.hairline },
            ]}
          >
            <Text selectable style={[styles.mono, { color: palette.muted }]}>
              {item.text}
            </Text>
          </View>
        );
      default:
        return null;
    }
  };

  return (
    <FlatList
      ref={listRef}
      data={entries}
      keyExtractor={(e) => e.key}
      renderItem={renderEntry}
      contentContainerStyle={styles.listContent}
      refreshControl={
        <RefreshControl
          refreshing={refreshing}
          onRefresh={onRefresh}
          tintColor={palette.accent}
        />
      }
      ListFooterComponent={null}
      ListEmptyComponent={
        <View style={styles.emptyBox}>
          <ActivityIndicator size="small" color={palette.accent} />
          <Text style={[styles.emptyText, { color: palette.muted }]}>
            {agent.name}의 대화를 동기화하는 중입니다...
          </Text>
        </View>
      }
    />
  );
}

const styles = StyleSheet.create({
  listContent: {
    paddingHorizontal: 16,
    paddingTop: 14,
    paddingBottom: 24,
    gap: 10,
  },
  activityWrap: {
    gap: 6,
  },
  activityRow: {
    flexDirection: "row",
    alignItems: "center",
    gap: 6,
    paddingVertical: 2,
    paddingHorizontal: 2,
    alignSelf: "flex-start",
    maxWidth: "100%",
  },
  activityRowQuiet: {
    opacity: 0.82,
  },
  activityHeadline: {
    fontFamily: FONTS.regular,
    fontVariant: TABULAR_NUMS,
    fontSize: 13,
  },
  activityDot: {
    fontFamily: FONTS.regular,
    fontSize: 12,
  },
  activityDetail: {
    fontFamily: FONTS.mono,
    fontVariant: TABULAR_NUMS,
    fontSize: 12,
    flexShrink: 1,
  },
  activityExpanded: {
    gap: 6,
    paddingLeft: 8,
  },
  userBubble: {
    alignSelf: "flex-end",
    maxWidth: "86%",
    borderRadius: 16,
    borderBottomRightRadius: 5,
    paddingHorizontal: 14,
    paddingVertical: 10,
  },
  assistantBlock: {
    paddingHorizontal: 2,
    paddingVertical: 2,
    gap: 5,
  },
  assistantTopRow: {
    flexDirection: "row",
    justifyContent: "space-between",
    alignItems: "center",
    minHeight: 18,
  },
  modelTag: {
    fontFamily: FONTS.mono,
    fontSize: 11.5,
    letterSpacing: -0.1,
  },
  copyBtn: {
    padding: 4,
    opacity: 0.72,
  },
  assistantText: {
    fontFamily: FONTS.regular,
    fontSize: 14.5,
    lineHeight: 22.5,
    letterSpacing: -0.1,
  },
  codeBlock: {
    borderWidth: 1,
    borderRadius: 8,
    paddingHorizontal: 11,
    paddingVertical: 9,
  },
  codeBlockText: {
    fontFamily: FONTS.mono,
    fontSize: 12,
    lineHeight: 18,
  },
  msgText: {
    fontFamily: FONTS.regular,
    fontSize: 14.5,
    lineHeight: 21.5,
    letterSpacing: -0.1,
  },
  thinkingBox: {
    borderWidth: 1,
    borderRadius: 10,
    padding: 10,
  },
  thinkingText: {
    fontFamily: FONTS.regular,
    fontSize: 12.5,
    lineHeight: 19,
  },
  noticeCard: {
    borderWidth: 1,
    borderRadius: 10,
    padding: 11,
  },
  mono: {
    fontFamily: FONTS.mono,
    fontSize: 12,
    lineHeight: 18,
  },
  emptyBox: {
    paddingVertical: 48,
    paddingHorizontal: 24,
    alignItems: "center",
    gap: 10,
  },
  emptyText: {
    fontFamily: FONTS.regular,
    fontSize: 13.5,
    textAlign: "center",
  },
  workingFooter: {
    flexDirection: "row",
    alignItems: "center",
    alignSelf: "flex-start",
    gap: 8,
    paddingHorizontal: 2,
    paddingVertical: 6,
    marginTop: 2,
    maxWidth: "100%",
  },
  spinnerRing: {
    width: 24,
    height: 24,
    borderRadius: 12,
    borderWidth: 1.5,
    alignItems: "center",
    justifyContent: "center",
  },
  workingLabel: {
    fontFamily: FONTS.semi,
    fontSize: 13,
  },
  workingSep: {
    fontFamily: FONTS.regular,
    fontSize: 12,
  },
  workingModel: {
    fontFamily: FONTS.mono,
    fontSize: 11.5,
    flexShrink: 1,
  },
});
