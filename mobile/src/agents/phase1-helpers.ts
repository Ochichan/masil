/**
 * Pure Node-testable Phase 1 helpers for Mobile:
 * - Phase 1A: Hybrid Risk Card Stack ordering (D-03)
 * - Phase 1B: Collision severity presentation
 * - Phase 1C: Prompt Cache Warmth formatting, invalidation guard, and
 *   "Warmest Blocked First" sorting (D-06)
 */
import type {
  AgentRow,
  AuditFinding,
  CacheWarmthState,
  CollisionSeverity,
  RiskHunk,
  TurnAuditReport,
} from "../protocol/schemas";
import { classifyAgentStatus } from "./status-classification";

export type HybridTriageCard =
  | {
      kind: "finding";
      id: string;
      finding: AuditFinding;
    }
  | {
      kind: "hunk";
      id: string;
      hunk: RiskHunk;
    };

export interface HybridTriageStack {
  /** Primary stack: CRITICAL/HIGH findings first, then top-5 risk hunks. */
  primaryCards: HybridTriageCard[];
  /** Collapsed drawer: WARNING/INFO findings. */
  secondaryFindings: AuditFinding[];
}

const SEVERITY_RANK: Record<string, number> = {
  critical: 0,
  high: 1,
  warning: 2,
  info: 3,
};

/**
 * Builds the Hybrid Risk Card Stack specified in D-03:
 * 1. CRITICAL/HIGH findings first (sorted critical -> high)
 * 2. Top-5 Blast-Radius risk hunks (sorted descending by risk score)
 * 3. WARNING/INFO findings tucked into a collapsed secondary drawer
 */
export function buildHybridTriageStack(
  report: TurnAuditReport | null | undefined
): HybridTriageStack {
  if (!report) {
    return { primaryCards: [], secondaryFindings: [] };
  }

  const findings = [...(report.findings ?? [])].sort((a, b) => {
    const ra = SEVERITY_RANK[a.severity.toLowerCase()] ?? 9;
    const rb = SEVERITY_RANK[b.severity.toLowerCase()] ?? 9;
    if (ra !== rb) return ra - rb;
    return a.rule_id.localeCompare(b.rule_id);
  });

  const primaryCards: HybridTriageCard[] = [];
  const secondaryFindings: AuditFinding[] = [];

  findings.forEach((finding, idx) => {
    const sev = finding.severity.toLowerCase();
    if (sev === "critical" || sev === "high") {
      primaryCards.push({
        kind: "finding",
        id: `finding:${finding.rule_id}:${finding.file ?? "turn"}:${finding.line ?? idx}`,
        finding,
      });
    } else {
      secondaryFindings.push(finding);
    }
  });

  const topHunks = [...(report.hunks ?? [])]
    .sort((a, b) => b.risk_score - a.risk_score)
    .slice(0, 5);

  topHunks.forEach((hunk, idx) => {
    primaryCards.push({
      kind: "hunk",
      id: `hunk:${hunk.file}:${idx}`,
      hunk,
    });
  });

  return { primaryCards, secondaryFindings };
}

/**
 * Formats an honest passive cache warmth countdown badge (`~4m12s (est.)`).
 * Recomputes live remaining milliseconds against `nowMs` when available.
 */
export function computeLiveWarmRemainingMs(
  warmth: CacheWarmthState | null | undefined,
  nowMs: number = Date.now()
): number {
  if (!warmth) return 0;
  if (warmth.ttl_secs > 0 && warmth.last_turn_at_ms > 0) {
    const expires = warmth.last_turn_at_ms + warmth.ttl_secs * 1000;
    return Math.max(0, expires - nowMs);
  }
  return Math.max(0, warmth.warm_remaining_ms ?? 0);
}

export function resolveWarmthTier(
  warmth: CacheWarmthState | null | undefined,
  nowMs: number = Date.now()
): "hot" | "expiring" | "cold" {
  const remaining = computeLiveWarmRemainingMs(warmth, nowMs);
  if (remaining <= 0) return "cold";
  if (remaining <= 60_000) return "expiring";
  return "hot";
}

export function formatCacheWarmthCompact(
  warmth: CacheWarmthState | null | undefined,
  nowMs: number = Date.now()
): string | null {
  if (!warmth) return null;
  const remaining = computeLiveWarmRemainingMs(warmth, nowMs);
  const tier = resolveWarmthTier(warmth, nowMs);
  if (tier === "cold" || remaining <= 0) {
    return "❄️ cold";
  }
  const totalSecs = Math.floor(remaining / 1000);
  const mins = Math.floor(totalSecs / 60);
  const secs = totalSecs % 60;
  const icon = tier === "expiring" ? "⏳" : "🔥";
  if (mins >= 60) {
    const hrs = Math.floor(mins / 60);
    const remM = mins % 60;
    return `${icon} ~${hrs}h${String(remM).padStart(2, "0")}m`;
  }
  return `${icon} ~${mins}m${String(secs).padStart(2, "0")}s`;
}

/**
 * D-06 Cache Invalidation Guard: warn before switching model/effort/mode when
 * warm cache still has time left AND estimated context is >= 30k tokens.
 */
export function shouldWarnCacheInvalidation(
  warmth: CacheWarmthState | null | undefined,
  nowMs: number = Date.now()
): boolean {
  if (!warmth) return false;
  const remaining = computeLiveWarmRemainingMs(warmth, nowMs);
  return remaining > 0 && (warmth.est_context_tokens ?? 0) >= 30_000;
}

/**
 * D-06 "Warmest Blocked First" triage sort:
 * 1. Blocked / needs_input agents with hot/expiring cache (>= 30k tokens) ordered by
 *    SHORTEST remaining TTL first (most urgent to approve before cache expires!).
 * 2. Other blocked / needs_input agents with hot/expiring cache (< 30k tokens), shortest TTL first.
 * 3. Remaining blocked / needs_input agents (cold/unknown).
 * 4. Working / idle / unavailable agents ordered by hotness then name.
 */
export function sortAgentsWarmestBlockedFirst(
  agents: AgentRow[],
  nowMs: number = Date.now()
): AgentRow[] {
  const rankBucket = (agent: AgentRow): [number, number, string] => {
    const cat = classifyAgentStatus(agent);
    const rem = computeLiveWarmRemainingMs(agent.cache_warmth, nowMs);
    const tokens = agent.cache_warmth?.est_context_tokens ?? 0;
    const name = (agent.name || agent.pane || agent.remote_id).toLowerCase();

    if (cat === "needs_input") {
      if (rem > 0 && tokens >= 30_000) return [0, rem, name];
      if (rem > 0) return [1, rem, name];
      return [2, Number.MAX_SAFE_INTEGER, name];
    }
    if (cat === "working") {
      return [3, rem > 0 ? rem : Number.MAX_SAFE_INTEGER, name];
    }
    if (cat === "idle") {
      return [4, rem > 0 ? rem : Number.MAX_SAFE_INTEGER, name];
    }
    return [5, Number.MAX_SAFE_INTEGER, name];
  };

  return [...agents].sort((a, b) => {
    const [ba, ra, na] = rankBucket(a);
    const [bb, rb, nb] = rankBucket(b);
    if (ba !== bb) return ba - bb;
    if (ra !== rb) return ra - rb;
    return na.localeCompare(nb);
  });
}

export function collisionSeverityTone(
  severity: CollisionSeverity | null | undefined
): "danger" | "warning" | "accent" | "muted" {
  switch (severity) {
    case "hard_conflict":
    case "same_directory_race":
      return "danger";
    case "semantic_risk":
      return "warning";
    case "overlap":
      return "accent";
    default:
      return "muted";
  }
}
