/**
 * Pure notification detection, deduplication, preference filtering, copy
 * formatting, and deep-link parsing for Masil mobile agent alerts.
 *
 * Kept completely free of React Native / Expo runtime imports so it runs
 * deterministically under Vitest in Node.
 */
import type {
  AgentRow,
  InboxItem,
  InteractivePrompt,
} from "../protocol/schemas";

export type AgentNotificationKind =
  | "approval_requested"
  | "question_asked"
  | "blocked"
  | "turn_completed"
  | "error";

export interface NotificationPreferences {
  enabled: boolean;
  notifyInputNeeded: boolean;
  notifyTaskCompleted: boolean;
  notifyActiveAgent: boolean;
}

export const DEFAULT_NOTIFICATION_PREFERENCES: NotificationPreferences = {
  enabled: true,
  notifyInputNeeded: true,
  notifyTaskCompleted: true,
  notifyActiveAgent: true,
};

export interface AgentNotificationEvent {
  hostId: string;
  hostName: string;
  remoteId: string;
  paneId: string;
  agentName: string;
  provider?: string;
  kind: AgentNotificationKind;
  permissionCount?: number;
  questionCount?: number;
  summary?: string | null;
  occurredMs: number;
}

export interface NotificationDeliveryContext {
  appInForeground: boolean;
  activePanel: "agent-list" | "agent" | "workspace-tools";
  activeHostId: string | null;
  activeAgentRemoteId: string | null;
}

export interface NotificationDeepLinkTarget {
  hostId: string;
  remoteId?: string;
  paneId?: string;
}

export function normalizeNotificationKind(
  rawKind: string | null | undefined
): AgentNotificationKind | null {
  switch (rawKind) {
    case "approval_requested":
    case "approval":
    case "permission":
    case "confirmation":
      return "approval_requested";
    case "question_asked":
    case "question":
      return "question_asked";
    case "blocked":
    case "needs_input":
    case "input":
      return "blocked";
    case "turn_completed":
    case "returned_idle":
    case "completed":
      return "turn_completed";
    case "error":
    case "operation_unknown":
    case "missed_schedule":
      return "error";
    default:
      return null;
  }
}

export function isInputNeededNotificationKind(
  kind: AgentNotificationKind
): boolean {
  return (
    kind === "approval_requested" ||
    kind === "question_asked" ||
    kind === "blocked" ||
    kind === "error"
  );
}

function getPermissionCount(agent: AgentRow): number {
  return agent.permission_count ?? agent.pending_approvals ?? 0;
}

function getQuestionCount(agent: AgentRow): number {
  return agent.question_count ?? agent.pending_questions ?? 0;
}

/**
 * Compares previous and next live snapshots of agents on a host and emits
 * notifications for newly entered input-needed or task-completed states.
 */
export function detectAgentStateTransitions(
  hostId: string,
  hostName: string,
  prevAgents: AgentRow[],
  nextAgents: AgentRow[],
  nowMs: number = Date.now()
): AgentNotificationEvent[] {
  const prevMap = new Map<string, AgentRow>();
  for (const prev of prevAgents) {
    prevMap.set(prev.remote_id, prev);
  }

  const events: AgentNotificationEvent[] = [];

  for (const next of nextAgents) {
    if (next.stale || next.state === "exited" || next.process === "exited") {
      continue;
    }

    const prev = prevMap.get(next.remote_id);
    const nextPerm = getPermissionCount(next);
    const nextQ = getQuestionCount(next);
    const agentName = next.name || next.pane || next.remote_id;

    if (!prev) {
      // Newly observed agent after baseline: only alert if it already needs input.
      let newKind: AgentNotificationKind | null = null;
      if (nextPerm > 0) {
        newKind = "approval_requested";
      } else if (nextQ > 0) {
        newKind = "question_asked";
      } else if (next.state === "blocked") {
        newKind = "blocked";
      }
      if (newKind) {
        events.push({
          hostId,
          hostName,
          remoteId: next.remote_id,
          paneId: next.pane || next.pane_id || "",
          agentName,
          provider: next.provider,
          kind: newKind,
          permissionCount: nextPerm,
          questionCount: nextQ,
          summary: next.summary ?? null,
          occurredMs: nowMs,
        });
      }
      continue;
    }

    const prevPerm = getPermissionCount(prev);
    const prevQ = getQuestionCount(prev);

    const newlyApproval =
      nextPerm > prevPerm ||
      (prev.state !== "blocked" && next.state === "blocked" && nextPerm > 0);
    const newlyQuestion =
      nextQ > prevQ ||
      (prev.state !== "blocked" && next.state === "blocked" && nextQ > 0);

    if (newlyApproval) {
      events.push({
        hostId,
        hostName,
        remoteId: next.remote_id,
        paneId: next.pane || next.pane_id || "",
        agentName,
        provider: next.provider,
        kind: "approval_requested",
        permissionCount: nextPerm,
        questionCount: nextQ,
        summary: next.summary ?? null,
        occurredMs: nowMs,
      });
      continue;
    }

    if (newlyQuestion) {
      events.push({
        hostId,
        hostName,
        remoteId: next.remote_id,
        paneId: next.pane || next.pane_id || "",
        agentName,
        provider: next.provider,
        kind: "question_asked",
        permissionCount: nextPerm,
        questionCount: nextQ,
        summary: next.summary ?? null,
        occurredMs: nowMs,
      });
      continue;
    }

    if (prev.state !== "blocked" && next.state === "blocked") {
      events.push({
        hostId,
        hostName,
        remoteId: next.remote_id,
        paneId: next.pane || next.pane_id || "",
        agentName,
        provider: next.provider,
        kind: "blocked",
        permissionCount: nextPerm,
        questionCount: nextQ,
        summary: next.summary ?? null,
        occurredMs: nowMs,
      });
      continue;
    }

    const sameRun = !prev.run || !next.run || prev.run === next.run;
    const wasWorking = prev.state === "working" || prev.state === "retrying";
    const newlyReturnedIdle =
      !prev.returned_idle && next.returned_idle === true && next.state === "idle";
    if (sameRun && next.state === "idle" && (wasWorking || newlyReturnedIdle)) {
      events.push({
        hostId,
        hostName,
        remoteId: next.remote_id,
        paneId: next.pane || next.pane_id || "",
        agentName,
        provider: next.provider,
        kind: "turn_completed",
        summary: next.summary ?? null,
        occurredMs: nowMs,
      });
    }
  }

  return events;
}

export function inboxItemKey(hostId: string, item: InboxItem): string {
  const idPart = item.id ?? item.seq ?? `${item.pane}:${item.kind}:${item.observed_ms ?? 0}`;
  return `${hostId}::${item.server_name ?? "default"}::${idPart}`;
}

export function detectNewInboxNotifications(
  hostId: string,
  hostName: string,
  seenKeys: Set<string>,
  inbox: InboxItem[],
  agents: AgentRow[],
  nowMs: number = Date.now()
): AgentNotificationEvent[] {
  const events: AgentNotificationEvent[] = [];
  for (const item of inbox) {
    const key = inboxItemKey(hostId, item);
    if (seenKeys.has(key)) continue;
    seenKeys.add(key);
    if (item.ack) continue;

    const kind = normalizeNotificationKind(item.kind);
    if (!kind) continue;

    const match = agents.find(
      (a) =>
        (item.pane && (a.pane === item.pane || a.pane_id === item.pane)) ||
        (item.agent && a.name === item.agent)
    );
    const serverName = item.server_name || match?.server_name || "default";
    const paneId = item.pane || match?.pane || "";
    const remoteId = match?.remote_id || `${serverName}::${paneId || item.agent}`;

    events.push({
      hostId,
      hostName,
      remoteId,
      paneId,
      agentName: item.agent || match?.name || paneId || "agent",
      provider: item.provider || match?.provider,
      kind,
      summary: typeof item.summary === "string" ? item.summary : null,
      occurredMs: item.observed_ms ?? nowMs,
    });
  }
  return events;
}

/**
 * Detects newly appeared interactive TUI / structured prompts on an active
 * timeline even before the provider's screen state flips.
 */
export function detectNewPromptNotifications(
  hostId: string,
  hostName: string,
  agent: AgentRow,
  prevPrompts: InteractivePrompt[] | undefined,
  nextPrompts: InteractivePrompt[] | undefined,
  nowMs: number = Date.now()
): AgentNotificationEvent[] {
  if (!nextPrompts || nextPrompts.length === 0) return [];
  const prevIds = new Set((prevPrompts ?? []).map((p) => p.id));
  const newlyAppeared = nextPrompts.filter((p) => !prevIds.has(p.id));
  if (newlyAppeared.length === 0) return [];

  const top = newlyAppeared[0]!;
  const kind = normalizeNotificationKind(top.kind) ?? "blocked";
  return [
    {
      hostId,
      hostName,
      remoteId: agent.remote_id,
      paneId: agent.pane || agent.pane_id || "",
      agentName: agent.name || agent.pane || agent.remote_id,
      provider: agent.provider,
      kind,
      summary: top.title || top.body || null,
      occurredMs: nowMs,
    },
  ];
}

export const DEFAULT_DEDUPE_WINDOW_MS = 12_000;

export class NotificationDeduplicator {
  private readonly recent = new Map<string, number>();
  private readonly windowMs: number;

  constructor(windowMs: number = DEFAULT_DEDUPE_WINDOW_MS) {
    this.windowMs = windowMs;
  }

  shouldEmit(event: AgentNotificationEvent, nowMs: number = Date.now()): boolean {
    const pruneBefore = nowMs - this.windowMs * 4;
    for (const [k, ts] of this.recent.entries()) {
      if (ts < pruneBefore) this.recent.delete(k);
    }

    const agentPrefix = `${event.hostId}::${event.remoteId}`;

    // If a specific approval or question notification already fired recently
    // for this agent, suppress a redundant generic "blocked" notification.
    if (event.kind === "blocked") {
      for (const specific of ["approval_requested", "question_asked"] as const) {
        const specificTs = this.recent.get(`${agentPrefix}::${specific}`);
        if (specificTs !== undefined && nowMs - specificTs < this.windowMs) {
          return false;
        }
      }
    }

    const key = `${agentPrefix}::${event.kind}`;
    const lastTs = this.recent.get(key);
    if (lastTs !== undefined && nowMs - lastTs < this.windowMs) {
      return false;
    }

    this.recent.set(key, nowMs);
    return true;
  }

  clearHost(hostId: string): void {
    const prefix = `${hostId}::`;
    for (const k of this.recent.keys()) {
      if (k.startsWith(prefix)) this.recent.delete(k);
    }
  }
}

export function shouldDeliverNotification(
  event: AgentNotificationEvent,
  prefs: NotificationPreferences,
  context: NotificationDeliveryContext
): boolean {
  if (!prefs.enabled) return false;

  if (event.kind === "turn_completed") {
    if (!prefs.notifyTaskCompleted) return false;
  } else if (isInputNeededNotificationKind(event.kind)) {
    if (!prefs.notifyInputNeeded) return false;
  }

  const viewingSameAgent =
    context.appInForeground &&
    context.activePanel === "agent" &&
    context.activeHostId === event.hostId &&
    context.activeAgentRemoteId === event.remoteId;

  if (viewingSameAgent && !prefs.notifyActiveAgent) {
    return false;
  }

  return true;
}

export function formatNotificationCopy(
  event: AgentNotificationEvent,
  language: string
): { title: string; body: string; channelId: "masil-attention" | "masil-completed" } {
  const isKo = language.toLowerCase().startsWith("ko");
  const summary = event.summary?.trim();
  const shortSummary =
    summary && summary.length > 90 ? `${summary.slice(0, 89)}…` : summary;
  const hostPrefix = event.hostName ? `${event.hostName} · ` : "";

  switch (event.kind) {
    case "approval_requested": {
      const count = event.permissionCount ?? 0;
      const countSuffix = count > 1 ? ` (${count})` : "";
      return {
        title: isKo
          ? `승인 요청${countSuffix} · ${event.agentName}`
          : `Approval requested${countSuffix} · ${event.agentName}`,
        body: shortSummary
          ? `${hostPrefix}${shortSummary}`
          : isKo
            ? `${hostPrefix}에이전트가 권한 승인을 기다리고 있습니다.`
            : `${hostPrefix}Agent is waiting for permission approval.`,
        channelId: "masil-attention",
      };
    }
    case "question_asked": {
      const count = event.questionCount ?? 0;
      const countSuffix = count > 1 ? ` (${count})` : "";
      return {
        title: isKo
          ? `질문 대기${countSuffix} · ${event.agentName}`
          : `Question waiting${countSuffix} · ${event.agentName}`,
        body: shortSummary
          ? `${hostPrefix}${shortSummary}`
          : isKo
            ? `${hostPrefix}에이전트가 질문에 대한 답변을 기다리고 있습니다.`
            : `${hostPrefix}Agent has a question waiting for your answer.`,
        channelId: "masil-attention",
      };
    }
    case "blocked":
      return {
        title: isKo
          ? `입력 필요 · ${event.agentName}`
          : `Input needed · ${event.agentName}`,
        body: shortSummary
          ? `${hostPrefix}${shortSummary}`
          : isKo
            ? `${hostPrefix}에이전트가 사용자 입력을 기다리고 있습니다.`
            : `${hostPrefix}Agent needs your input to continue.`,
        channelId: "masil-attention",
      };
    case "turn_completed":
      return {
        title: isKo
          ? `작업 완료 · ${event.agentName}`
          : `Task completed · ${event.agentName}`,
        body: shortSummary
          ? `${hostPrefix}${shortSummary}`
          : isKo
            ? `${hostPrefix}에이전트 작업이 끝나 대기 상태로 전환되었습니다.`
            : `${hostPrefix}Agent finished its turn and returned to idle.`,
        channelId: "masil-completed",
      };
    case "error":
      return {
        title: isKo
          ? `확인 필요 · ${event.agentName}`
          : `Attention needed · ${event.agentName}`,
        body: shortSummary
          ? `${hostPrefix}${shortSummary}`
          : isKo
            ? `${hostPrefix}에이전트에서 확인이 필요한 이벤트가 발생했습니다.`
            : `${hostPrefix}An event requires your attention.`,
        channelId: "masil-attention",
      };
  }
}

export function extractDeepLinkTarget(
  rawData: unknown
): NotificationDeepLinkTarget | null {
  if (!rawData || typeof rawData !== "object") return null;
  const obj = rawData as Record<string, unknown>;
  const nested =
    obj.masil && typeof obj.masil === "object"
      ? (obj.masil as Record<string, unknown>)
      : null;

  const pickString = (...keys: string[]): string | undefined => {
    for (const src of [obj, nested]) {
      if (!src) continue;
      for (const k of keys) {
        const val = src[k];
        if (typeof val === "string" && val.trim().length > 0) {
          return val.trim();
        }
      }
    }
    return undefined;
  };

  let hostId = pickString("host_id", "hostId");
  const remoteId = pickString("remote_id", "remoteId");
  let paneId = pickString("agent_id", "agentId", "pane_id", "pane");

  const deepLink = pickString("deep_link", "deepLink");
  if ((!hostId || !paneId) && deepLink) {
    const match = deepLink.match(/^masil:\/\/host\/([^/]+)\/agent\/(.+)$/);
    if (match) {
      hostId = hostId || decodeURIComponent(match[1]!);
      paneId = paneId || decodeURIComponent(match[2]!);
    }
  }

  if (!hostId) return null;
  return { hostId, remoteId, paneId };
}

export function countAgentsNeedingAttention(agents: AgentRow[]): number {
  let count = 0;
  for (const agent of agents) {
    if (agent.stale || agent.state === "exited" || agent.process === "exited") {
      continue;
    }
    if (
      agent.state === "blocked" ||
      getPermissionCount(agent) > 0 ||
      getQuestionCount(agent) > 0
    ) {
      count += 1;
    }
  }
  return count;
}
