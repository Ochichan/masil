/**
 * Pure agent status classification, summary counts, section grouping, and
 * semantic badge descriptors shared across mobile agent list & detail views.
 */
import type { AgentRow, InteractivePromptKind } from "../protocol/schemas";

export type AgentStatusCategory =
  | "needs_input"
  | "working"
  | "idle"
  | "unavailable";

export type AgentStatusFilter = "all" | AgentStatusCategory;

export const AGENT_STATUS_CATEGORY_ORDER: readonly AgentStatusCategory[] = [
  "needs_input",
  "working",
  "idle",
  "unavailable",
] as const;

export interface AgentStatusCounts {
  all: number;
  needs_input: number;
  working: number;
  idle: number;
  unavailable: number;
}

export interface AgentStatusSection {
  category: AgentStatusCategory;
  count: number;
  data: AgentRow[];
}

export type BadgeTone = "warning" | "accent" | "success" | "muted" | "danger";

export type AgentBadgeI18nKey =
  | "approval"
  | "question"
  | "needsInput"
  | "working"
  | "retrying"
  | "returnedIdle"
  | "idle"
  | "exited"
  | "stateUnavailable";

export interface AgentBadgeDescriptor {
  key: AgentBadgeI18nKey;
  tone: BadgeTone;
  count?: number;
  category: AgentStatusCategory;
}

export function getAgentPermissionCount(agent: AgentRow): number {
  return agent.permission_count ?? agent.pending_approvals ?? 0;
}

export function getAgentQuestionCount(agent: AgentRow): number {
  return agent.question_count ?? agent.pending_questions ?? 0;
}

export function classifyAgentStatus(agent: AgentRow): AgentStatusCategory {
  if (agent.stale || agent.state === "exited" || agent.process === "exited") {
    return "unavailable";
  }
  if (
    getAgentPermissionCount(agent) > 0 ||
    getAgentQuestionCount(agent) > 0 ||
    agent.state === "blocked"
  ) {
    return "needs_input";
  }
  if (agent.state === "working" || agent.state === "retrying") {
    return "working";
  }
  if (agent.state === "idle") {
    return "idle";
  }
  return "unavailable";
}

export function getAgentBadgeDescriptor(
  agent: AgentRow,
  topPromptKind?: InteractivePromptKind | null
): AgentBadgeDescriptor {
  if (agent.state === "exited" || agent.process === "exited") {
    return { key: "exited", tone: "muted", category: "unavailable" };
  }
  if (agent.stale) {
    return { key: "stateUnavailable", tone: "danger", category: "unavailable" };
  }

  const permCount = getAgentPermissionCount(agent);
  const qCount = getAgentQuestionCount(agent);

  if (
    permCount > 0 ||
    topPromptKind === "permission" ||
    topPromptKind === "confirmation"
  ) {
    return {
      key: "approval",
      tone: "warning",
      count: permCount > 1 ? permCount : undefined,
      category: "needs_input",
    };
  }
  if (qCount > 0 || topPromptKind === "question") {
    return {
      key: "question",
      tone: "accent",
      count: qCount > 1 ? qCount : undefined,
      category: "needs_input",
    };
  }
  if (agent.state === "blocked" || topPromptKind === "input") {
    return { key: "needsInput", tone: "warning", category: "needs_input" };
  }
  if (agent.state === "working") {
    return { key: "working", tone: "success", category: "working" };
  }
  if (agent.state === "retrying") {
    return { key: "retrying", tone: "success", category: "working" };
  }
  if (agent.state === "idle") {
    if (agent.returned_idle && !agent.seen) {
      return { key: "returnedIdle", tone: "accent", category: "idle" };
    }
    return { key: "idle", tone: "muted", category: "idle" };
  }
  return { key: "stateUnavailable", tone: "danger", category: "unavailable" };
}

export function countAgentsByCategory(agents: AgentRow[]): AgentStatusCounts {
  const counts: AgentStatusCounts = {
    all: agents.length,
    needs_input: 0,
    working: 0,
    idle: 0,
    unavailable: 0,
  };
  for (const agent of agents) {
    counts[classifyAgentStatus(agent)] += 1;
  }
  return counts;
}

function labelForSort(agent: AgentRow): string {
  return (agent.name || agent.pane || agent.remote_id).toLowerCase();
}

export function sortAgentsWithinCategory(
  category: AgentStatusCategory,
  agents: AgentRow[]
): AgentRow[] {
  return [...agents].sort((a, b) => {
    if (category === "needs_input") {
      const aRank =
        getAgentPermissionCount(a) > 0
          ? 0
          : getAgentQuestionCount(a) > 0
            ? 1
            : 2;
      const bRank =
        getAgentPermissionCount(b) > 0
          ? 0
          : getAgentQuestionCount(b) > 0
            ? 1
            : 2;
      if (aRank !== bRank) return aRank - bRank;
    } else if (category === "idle") {
      const aDone = a.returned_idle && !a.seen ? 0 : 1;
      const bDone = b.returned_idle && !b.seen ? 0 : 1;
      if (aDone !== bDone) return aDone - bDone;
    }
    return labelForSort(a).localeCompare(labelForSort(b));
  });
}

export function buildAgentStatusSections(
  agents: AgentRow[],
  filter: AgentStatusFilter = "all"
): AgentStatusSection[] {
  const buckets: Record<AgentStatusCategory, AgentRow[]> = {
    needs_input: [],
    working: [],
    idle: [],
    unavailable: [],
  };

  for (const agent of agents) {
    buckets[classifyAgentStatus(agent)].push(agent);
  }

  const targetCategories: readonly AgentStatusCategory[] =
    filter === "all" ? AGENT_STATUS_CATEGORY_ORDER : [filter];

  const sections: AgentStatusSection[] = [];
  for (const category of targetCategories) {
    const items = sortAgentsWithinCategory(category, buckets[category]);
    if (items.length > 0) {
      sections.push({
        category,
        count: items.length,
        data: items,
      });
    }
  }
  return sections;
}

export function resolveInboxKindDisplay(rawKind: string): {
  key:
    | "inboxKindApproval"
    | "inboxKindQuestion"
    | "inboxKindBlocked"
    | "inboxKindCompleted"
    | "inboxKindError";
  tone: BadgeTone;
} {
  switch (rawKind) {
    case "approval_requested":
    case "approval":
      return { key: "inboxKindApproval", tone: "warning" };
    case "question_asked":
    case "question":
      return { key: "inboxKindQuestion", tone: "accent" };
    case "blocked":
    case "needs_input":
      return { key: "inboxKindBlocked", tone: "warning" };
    case "turn_completed":
    case "returned_idle":
      return { key: "inboxKindCompleted", tone: "success" };
    default:
      return { key: "inboxKindError", tone: "danger" };
  }
}
