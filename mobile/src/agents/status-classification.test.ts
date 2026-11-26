import { describe, expect, it } from "vitest";
import type { AgentRow } from "../protocol/schemas";
import {
  buildAgentStatusSections,
  classifyAgentStatus,
  countAgentsByCategory,
  getAgentBadgeDescriptor,
  resolveInboxKindDisplay,
} from "./status-classification";

function makeAgent(overrides: Partial<AgentRow> = {}): AgentRow {
  return {
    remote_id: "default::%1",
    server_name: "default",
    server_socket: "/tmp/masil/default",
    pane: "%1",
    name: "agent-1",
    provider: "claude",
    state: "idle",
    cwd: "/repo",
    ...overrides,
  };
}

describe("agent status classification and grouping", () => {
  it("classifies agents into needs_input, working, idle, and unavailable", () => {
    expect(classifyAgentStatus(makeAgent({ state: "blocked" }))).toBe("needs_input");
    expect(
      classifyAgentStatus(makeAgent({ state: "working", permission_count: 1 }))
    ).toBe("needs_input");
    expect(
      classifyAgentStatus(makeAgent({ state: "idle", question_count: 1 }))
    ).toBe("needs_input");
    expect(classifyAgentStatus(makeAgent({ state: "working" }))).toBe("working");
    expect(classifyAgentStatus(makeAgent({ state: "retrying" }))).toBe("working");
    expect(classifyAgentStatus(makeAgent({ state: "idle" }))).toBe("idle");
    expect(classifyAgentStatus(makeAgent({ state: "unknown" }))).toBe("unavailable");
    expect(classifyAgentStatus(makeAgent({ state: "exited" }))).toBe("unavailable");
    expect(
      classifyAgentStatus(makeAgent({ state: "blocked", stale: true }))
    ).toBe("unavailable");
  });

  it("counts agents accurately across all categories", () => {
    const agents = [
      makeAgent({ remote_id: "1", state: "blocked", permission_count: 1 }),
      makeAgent({ remote_id: "2", state: "blocked", question_count: 1 }),
      makeAgent({ remote_id: "3", state: "working" }),
      makeAgent({ remote_id: "4", state: "retrying" }),
      makeAgent({ remote_id: "5", state: "idle", returned_idle: true }),
      makeAgent({ remote_id: "6", state: "idle" }),
      makeAgent({ remote_id: "7", state: "unknown" }),
    ];

    expect(countAgentsByCategory(agents)).toEqual({
      all: 7,
      needs_input: 2,
      working: 2,
      idle: 2,
      unavailable: 1,
    });
  });

  it("builds grouped sections in priority order and sorts approvals & recently finished first", () => {
    const agents = [
      makeAgent({ remote_id: "idle-plain", name: "alpha-idle", state: "idle" }),
      makeAgent({
        remote_id: "idle-done",
        name: "zeta-done",
        state: "idle",
        returned_idle: true,
        seen: false,
      }),
      makeAgent({ remote_id: "work-b", name: "beta-work", state: "working" }),
      makeAgent({ remote_id: "work-a", name: "alpha-work", state: "working" }),
      makeAgent({ remote_id: "blk-plain", name: "a-blocked", state: "blocked" }),
      makeAgent({
        remote_id: "blk-q",
        name: "b-question",
        state: "blocked",
        question_count: 1,
      }),
      makeAgent({
        remote_id: "blk-perm",
        name: "c-approval",
        state: "blocked",
        permission_count: 2,
      }),
    ];

    const allSections = buildAgentStatusSections(agents, "all");
    expect(allSections.map((s) => s.category)).toEqual([
      "needs_input",
      "working",
      "idle",
    ]);
    expect(allSections[0]?.data.map((a) => a.remote_id)).toEqual([
      "blk-perm",
      "blk-q",
      "blk-plain",
    ]);
    expect(allSections[1]?.data.map((a) => a.remote_id)).toEqual([
      "work-a",
      "work-b",
    ]);
    expect(allSections[2]?.data.map((a) => a.remote_id)).toEqual([
      "idle-done",
      "idle-plain",
    ]);

    const onlyWorking = buildAgentStatusSections(agents, "working");
    expect(onlyWorking).toHaveLength(1);
    expect(onlyWorking[0]?.category).toBe("working");
    expect(onlyWorking[0]?.count).toBe(2);
  });

  it("produces consistent badge descriptors for list and detail headers", () => {
    expect(
      getAgentBadgeDescriptor(makeAgent({ state: "blocked", permission_count: 3 }))
    ).toEqual({
      key: "approval",
      tone: "warning",
      count: 3,
      category: "needs_input",
    });

    expect(
      getAgentBadgeDescriptor(makeAgent({ state: "idle" }), "question")
    ).toEqual({
      key: "question",
      tone: "accent",
      count: undefined,
      category: "needs_input",
    });

    expect(
      getAgentBadgeDescriptor(
        makeAgent({ state: "idle", returned_idle: true, seen: false })
      )
    ).toEqual({
      key: "returnedIdle",
      tone: "accent",
      category: "idle",
    });

    expect(
      getAgentBadgeDescriptor(makeAgent({ state: "exited" }))
    ).toEqual({
      key: "exited",
      tone: "muted",
      category: "unavailable",
    });
  });

  it("maps inbox event kinds to localized badge keys and tones", () => {
    expect(resolveInboxKindDisplay("approval_requested")).toEqual({
      key: "inboxKindApproval",
      tone: "warning",
    });
    expect(resolveInboxKindDisplay("question_asked")).toEqual({
      key: "inboxKindQuestion",
      tone: "accent",
    });
    expect(resolveInboxKindDisplay("returned_idle")).toEqual({
      key: "inboxKindCompleted",
      tone: "success",
    });
    expect(resolveInboxKindDisplay("turn_completed")).toEqual({
      key: "inboxKindCompleted",
      tone: "success",
    });
  });
});
