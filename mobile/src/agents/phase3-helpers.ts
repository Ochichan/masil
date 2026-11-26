import type {
  ArenaRacerRecord,
  DvrTurnRecord,
  InteractivePrompt,
  PromptRiskTier,
} from "../protocol/schemas";

export type RiskBadgeTone = "success" | "warning" | "danger" | "muted";

export function riskTierTone(tier?: PromptRiskTier | string | null): RiskBadgeTone {
  switch (tier) {
    case "low":
      return "success";
    case "medium":
      return "warning";
    case "high":
    case "critical":
      return "danger";
    default:
      return "muted";
  }
}

export interface BatchPromptCandidate {
  agentName: string;
  remoteId: string;
  serverSocket: string;
  pane: string;
  prompt: InteractivePrompt;
}

/**
 * D-10 strict client-side batch approval filter (defense-in-depth matching the
 * host's server-side enforcement): ONLY non-truncated `low` risk prompts that
 * are not `deny` or `always_confirm` may enter a batch approval action.
 * Medium, High, and Critical prompts are ALWAYS excluded and isolated individually.
 */
export function filterBatchSafePrompts(
  candidates: BatchPromptCandidate[]
): BatchPromptCandidate[] {
  return candidates.filter(({ prompt }) => {
    const risk = prompt.risk;
    if (!risk) return false;
    if (risk.tier !== "low") return false;
    if (risk.truncated) return false;
    if (!risk.batch_eligible) return false;
    if (risk.matched_action === "deny" || risk.matched_action === "always_confirm") {
      return false;
    }
    if (prompt.id.startsWith("fallback")) return false;
    return true;
  });
}

export interface SandboxBadgeInfo {
  sandboxed: boolean;
  backend: string;
  profile: string;
  shortLabel: string;
}

/**
 * Parses `@masil-sandbox` pane metadata (`sandboxed:seatbelt:worktree` or
 * `unsandboxed:user_disabled`) for honest UI display (D-11).
 */
export function parseSandboxBadge(raw?: string | null): SandboxBadgeInfo | null {
  if (!raw || !raw.trim()) return null;
  const parts = raw.trim().split(":");
  const state = parts[0] ?? "";
  if (state === "sandboxed") {
    const backend = parts[1] ?? "os";
    const profile = parts[2] ?? "worktree";
    return {
      sandboxed: true,
      backend,
      profile,
      shortLabel: `Sandboxed (${profile})`,
    };
  }
  return {
    sandboxed: false,
    backend: "none",
    profile: parts[1] ?? "off",
    shortLabel: "Unsandboxed",
  };
}

/**
 * Deterministic racer ranking matching Rust `agent/src/arena/scorecard.rs`.
 */
export function rankArenaRacers(racers: ArenaRacerRecord[]): ArenaRacerRecord[] {
  return [...racers].sort((a, b) => {
    const vRank = (r: ArenaRacerRecord) =>
      r.verify_exit_code === 0 ? 0 : r.verify_exit_code == null ? 1 : 2;
    const aRank = (r: ArenaRacerRecord) =>
      r.audit_verdict === "verified"
        ? 0
        : r.audit_verdict === "warnings"
          ? 1
          : r.audit_verdict == null
            ? 2
            : 3;
    const diffV = vRank(a) - vRank(b);
    if (diffV !== 0) return diffV;
    const diffA = aRank(a) - aRank(b);
    if (diffA !== 0) return diffA;
    const churnA = (a.insertions ?? 0) + (a.deletions ?? 0);
    const churnB = (b.insertions ?? 0) + (b.deletions ?? 0);
    if (churnA !== churnB) return churnA - churnB;
    return a.racer_name.localeCompare(b.racer_name);
  });
}

export function clampTurnIndex(index: number, total: number): number {
  if (total <= 0) return 0;
  return Math.max(0, Math.min(Math.floor(index), total - 1));
}

export function countPinnedTurns(turns: DvrTurnRecord[]): number {
  return turns.filter((t) => t.pinned).length;
}
