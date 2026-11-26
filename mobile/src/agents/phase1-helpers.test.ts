import { describe, expect, it } from "vitest";
import type { AgentRow, TurnAuditReport } from "../protocol/schemas";
import { en } from "../i18n/en";
import { ko } from "../i18n/ko";
import {
  buildHybridTriageStack,
  formatCacheWarmthCompact,
  shouldWarnCacheInvalidation,
  sortAgentsWarmestBlockedFirst,
} from "./phase1-helpers";

function makeAgent(partial: Partial<AgentRow>): AgentRow {
  return {
    remote_id: partial.remote_id ?? "srv::%1",
    server_name: "srv",
    server_socket: "/tmp/s",
    pane: partial.pane ?? "%1",
    name: partial.name ?? "agent",
    provider: partial.provider ?? "claude",
    state: partial.state ?? "idle",
    cwd: "/tmp/repo",
    ...partial,
  };
}

describe("Phase 1A Hybrid Risk Card Stack (D-03)", () => {
  it("puts CRITICAL/HIGH findings first, then top-5 hunks by score, and tucks WARNING/INFO into secondary", () => {
    const report: TurnAuditReport = {
      id: 42,
      pane_id: "%1",
      run_id: "r1",
      checkpoint_id: "5",
      prev_checkpoint_id: "4",
      verdict: "flagged",
      risk_score: 88,
      claim_verified: false,
      created_ms: 1000,
      findings: [
        {
          rule_id: "G01",
          severity: "warning",
          title: "Lint ignore",
          detail: "Added eslint-disable",
          file: "src/a.ts",
        },
        {
          rule_id: "T02",
          severity: "high",
          title: "Assertions dropped",
          detail: "Removed 3 expects",
          file: "src/a.test.ts",
        },
        {
          rule_id: "T01",
          severity: "critical",
          title: "Test skipped",
          detail: "Added .skip",
          file: "src/a.test.ts",
          line: 10,
        },
        {
          rule_id: "S01",
          severity: "info",
          title: "Sensitive touch",
          detail: "Auth file touched",
          file: "src/auth.ts",
        },
      ],
      hunks: [10, 90, 40, 80, 20, 70, 60].map((risk_score, i) => ({
        id: `h${i}`,
        file: `src/f${i}.ts`,
        old_start: 1,
        new_start: 1,
        added: 1,
        removed: 0,
        risk_score,
        reason: "core",
        diff: "+x",
      })),
    };

    const stack = buildHybridTriageStack(report);
    expect(stack.primaryCards).toHaveLength(7); // 2 critical/high + 5 top hunks
    expect(stack.primaryCards[0]?.kind).toBe("finding");
    if (stack.primaryCards[0]?.kind === "finding") {
      expect(stack.primaryCards[0].finding.rule_id).toBe("T01");
    }
    expect(stack.primaryCards[1]?.kind).toBe("finding");
    if (stack.primaryCards[1]?.kind === "finding") {
      expect(stack.primaryCards[1].finding.rule_id).toBe("T02");
    }
    const hunkScores = stack.primaryCards
      .slice(2)
      .map((c) => (c.kind === "hunk" ? c.hunk.risk_score : -1));
    expect(hunkScores).toEqual([90, 80, 70, 60, 40]);

    expect(stack.secondaryFindings.map((f) => f.rule_id)).toEqual(["G01", "S01"]);
  });
});

describe("Phase 1C Cache Warmth & Warmest Blocked First (D-06)", () => {
  it("warns before model switch only when warm cache has time left AND tokens >= 30k", () => {
    const now = 100_000;
    expect(
      shouldWarnCacheInvalidation(
        {
          run_id: "r1",
          pane_id: "%1",
          provider: "claude",
          ttl_secs: 300,
          last_turn_at_ms: 50_000,
          est_context_tokens: 45_000,
          warm_remaining_ms: 250_000,
          tier: "hot",
          badge: "🔥 ~4m10s (est.)",
          invalidate_warning: true,
        },
        now
      )
    ).toBe(true);

    // Small context (< 30k) should NOT warn.
    expect(
      shouldWarnCacheInvalidation(
        {
          run_id: "r2",
          pane_id: "%2",
          provider: "claude",
          ttl_secs: 300,
          last_turn_at_ms: 50_000,
          est_context_tokens: 12_000,
          warm_remaining_ms: 250_000,
          tier: "hot",
          badge: "🔥 ~4m10s (est.)",
          invalidate_warning: false,
        },
        now
      )
    ).toBe(false);

    // Expired cache should NOT warn.
    expect(
      shouldWarnCacheInvalidation(
        {
          run_id: "r3",
          pane_id: "%3",
          provider: "claude",
          ttl_secs: 300,
          last_turn_at_ms: 0,
          est_context_tokens: 90_000,
          warm_remaining_ms: 0,
          tier: "cold",
          badge: "❄️ cold",
          invalidate_warning: false,
        },
        now + 500_000
      )
    ).toBe(false);
  });

  it("sorts blocked agents with hottest large context about to expire first", () => {
    const now = 1_000_000;
    const coldBlocked = makeAgent({
      remote_id: "s::%1",
      name: "cold-blocked",
      state: "blocked",
      permission_count: 1,
      cache_warmth: {
        run_id: "r1",
        pane_id: "%1",
        provider: "claude",
        ttl_secs: 300,
        last_turn_at_ms: now - 400_000,
        est_context_tokens: 50_000,
        warm_remaining_ms: 0,
        tier: "cold",
        badge: "❄️ cold",
        invalidate_warning: false,
      },
    });
    const expiringLargeBlocked = makeAgent({
      remote_id: "s::%2",
      name: "expiring-large",
      state: "blocked",
      permission_count: 1,
      cache_warmth: {
        run_id: "r2",
        pane_id: "%2",
        provider: "claude",
        ttl_secs: 300,
        last_turn_at_ms: now - 270_000, // 30s left!
        est_context_tokens: 80_000,
        warm_remaining_ms: 30_000,
        tier: "expiring",
        badge: "⏳ ~0m30s (est.)",
        invalidate_warning: true,
      },
    });
    const freshLargeBlocked = makeAgent({
      remote_id: "s::%3",
      name: "fresh-large",
      state: "blocked",
      permission_count: 1,
      cache_warmth: {
        run_id: "r3",
        pane_id: "%3",
        provider: "claude",
        ttl_secs: 300,
        last_turn_at_ms: now - 30_000, // 270s left
        est_context_tokens: 60_000,
        warm_remaining_ms: 270_000,
        tier: "hot",
        badge: "🔥 ~4m30s (est.)",
        invalidate_warning: true,
      },
    });
    const workingHot = makeAgent({
      remote_id: "s::%4",
      name: "working-hot",
      state: "working",
      cache_warmth: {
        run_id: "r4",
        pane_id: "%4",
        provider: "claude",
        ttl_secs: 300,
        last_turn_at_ms: now - 20_000,
        est_context_tokens: 50_000,
        warm_remaining_ms: 280_000,
        tier: "hot",
        badge: "🔥 ~4m40s (est.)",
        invalidate_warning: true,
      },
    });

    const sorted = sortAgentsWarmestBlockedFirst(
      [workingHot, coldBlocked, freshLargeBlocked, expiringLargeBlocked],
      now
    );
    expect(sorted.map((a) => a.name)).toEqual([
      "expiring-large",
      "fresh-large",
      "cold-blocked",
      "working-hot",
    ]);

    expect(formatCacheWarmthCompact(expiringLargeBlocked.cache_warmth, now)).toBe(
      "⏳ ~0m30s"
    );
    expect(formatCacheWarmthCompact(coldBlocked.cache_warmth, now)).toBe(
      "❄️ cold"
    );
  });

  it("keeps EN and KO i18n dictionaries strictly symmetric", () => {
    const enKeys = Object.keys(en).sort();
    const koKeys = Object.keys(ko).sort();
    expect(koKeys).toEqual(enKeys);
  });
});
