import { describe, expect, it } from "vitest";
import { en } from "../i18n/en";
import { ko } from "../i18n/ko";
import type { ArenaRacerRecord, InteractivePrompt } from "../protocol/schemas";
import {
  type BatchPromptCandidate,
  clampTurnIndex,
  countPinnedTurns,
  filterBatchSafePrompts,
  parseSandboxBadge,
  rankArenaRacers,
  riskTierTone,
} from "./phase3-helpers";

function makeCandidate(
  id: string,
  tier: "low" | "medium" | "high" | "critical",
  overrides: Partial<NonNullable<InteractivePrompt["risk"]>> = {}
): BatchPromptCandidate {
  return {
    agentName: `agent-${id}`,
    remoteId: `local::%${id}`,
    serverSocket: "/tmp/sock",
    pane: `%${id}`,
    prompt: {
      id,
      kind: "permission",
      mode: "terminal_tui",
      title: "Allow command",
      body: "command: cargo test",
      options: [],
      selected_index: 0,
      multi_select: false,
      allows_free_text: false,
      free_text_only: false,
      allows_cancel: true,
      can_navigate_tabs: false,
      always_patterns: [],
      session_peers_count: 0,
      is_subagent: false,
      questions: [],
      risk: {
        tier,
        label: tier.toUpperCase(),
        score: tier === "low" ? 15 : 85,
        reasons: [],
        command: "cargo test",
        truncated: false,
        batch_eligible: tier === "low",
        prompt_digest: "abc",
        ...overrides,
      },
    },
  };
}

describe("Phase 3 helpers", () => {
  it("excludes medium/high/critical, truncated, and denied prompts from batch approval (D-10)", () => {
    const candidates: BatchPromptCandidate[] = [
      makeCandidate("safe-1", "low"),
      makeCandidate("safe-2", "low", { matched_action: "suggest_batch" }),
      makeCandidate("trunc-low", "low", { truncated: true }),
      makeCandidate("denied-low", "low", { matched_action: "deny" }),
      makeCandidate("confirm-low", "low", { matched_action: "always_confirm" }),
      makeCandidate("med-1", "medium", { batch_eligible: true }),
      makeCandidate("high-1", "high", { batch_eligible: true }),
      makeCandidate("crit-1", "critical", { batch_eligible: true }),
      makeCandidate("fallback-1", "low"),
    ];
    const safe = filterBatchSafePrompts(candidates);
    expect(safe.map((c) => c.prompt.id)).toEqual(["safe-1", "safe-2"]);
    expect(riskTierTone("low")).toBe("success");
    expect(riskTierTone("medium")).toBe("warning");
    expect(riskTierTone("critical")).toBe("danger");
  });

  it("parses honest OS sandbox badges (D-11)", () => {
    expect(parseSandboxBadge("sandboxed:seatbelt:worktree")).toEqual({
      sandboxed: true,
      backend: "seatbelt",
      profile: "worktree",
      shortLabel: "Sandboxed (worktree)",
    });
    expect(parseSandboxBadge("unsandboxed:user_disabled")).toEqual({
      sandboxed: false,
      backend: "none",
      profile: "user_disabled",
      shortLabel: "Unsandboxed",
    });
    expect(parseSandboxBadge(null)).toBeNull();
  });

  it("ranks Arena racers by verify pass then BS audit cleanliness then churn (D-12)", () => {
    const racers: ArenaRacerRecord[] = [
      {
        arena_id: "a1",
        racer_name: "codex",
        provider: "codex",
        worktree_id: "/tmp/wt1",
        pane_id: "%1",
        run_id: "r1",
        status: "completed",
        verify_exit_code: 1,
        audit_verdict: "verified",
        files_changed: 1,
        insertions: 5,
        deletions: 0,
      },
      {
        arena_id: "a1",
        racer_name: "agy",
        provider: "agy",
        worktree_id: "/tmp/wt2",
        pane_id: "%2",
        run_id: "r2",
        status: "completed",
        verify_exit_code: 0,
        audit_verdict: "flagged",
        files_changed: 2,
        insertions: 10,
        deletions: 2,
      },
      {
        arena_id: "a1",
        racer_name: "claude",
        provider: "claude",
        worktree_id: "/tmp/wt3",
        pane_id: "%3",
        run_id: "r3",
        status: "completed",
        verify_exit_code: 0,
        audit_verdict: "verified",
        files_changed: 2,
        insertions: 20,
        deletions: 5,
      },
    ];
    expect(rankArenaRacers(racers).map((r) => r.racer_name)).toEqual([
      "claude",
      "agy",
      "codex",
    ]);
  });

  it("clamps DVR turn indices and counts pinned turns", () => {
    expect(clampTurnIndex(-2, 5)).toBe(0);
    expect(clampTurnIndex(99, 5)).toBe(4);
    expect(clampTurnIndex(0, 0)).toBe(0);
    expect(
      countPinnedTurns([
        {
          id: 1,
          run_id: "r",
          pane_id: "%1",
          turn_seq: 1,
          provider: "claude",
          checkpoint_id: "1",
          tree_oid: "t1",
          files_changed: 1,
          pinned: true,
          created_ms: 1,
        },
        {
          id: 2,
          run_id: "r",
          pane_id: "%1",
          turn_seq: 2,
          provider: "claude",
          checkpoint_id: "2",
          tree_oid: "t2",
          files_changed: 1,
          pinned: false,
          created_ms: 2,
        },
      ])
    ).toBe(1);
  });

  it("maintains symmetric EN and KO i18n keys for Phase 3 surfaces", () => {
    const enKeys = Object.keys(en).sort();
    const koKeys = Object.keys(ko).sort();
    expect(enKeys).toEqual(koKeys);
  });
});
