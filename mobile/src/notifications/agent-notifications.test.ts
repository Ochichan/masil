import { describe, expect, it } from "vitest";
import type { AgentRow, InboxItem, InteractivePrompt } from "../protocol/schemas";
import {
  DEFAULT_NOTIFICATION_PREFERENCES,
  NotificationDeduplicator,
  countAgentsNeedingAttention,
  detectAgentStateTransitions,
  detectNewInboxNotifications,
  detectNewPromptNotifications,
  extractDeepLinkTarget,
  formatNotificationCopy,
  shouldDeliverNotification,
} from "./agent-notifications";

function makeAgent(overrides: Partial<AgentRow> = {}): AgentRow {
  return {
    remote_id: "default::%1",
    server_name: "default",
    server_socket: "/tmp/masil/default",
    pane: "%1",
    name: "worker-1",
    provider: "claude",
    state: "idle",
    cwd: "/repo",
    run: "run-1",
    ...overrides,
  };
}

describe("agent notification transition detection", () => {
  it("detects task completion when a working or retrying agent transitions to idle", () => {
    const prev = [makeAgent({ state: "working" })];
    const next = [makeAgent({ state: "idle", returned_idle: true })];

    const events = detectAgentStateTransitions("host-1", "MacBook", prev, next, 1000);
    expect(events).toHaveLength(1);
    expect(events[0]?.kind).toBe("turn_completed");
    expect(events[0]?.agentName).toBe("worker-1");
    expect(events[0]?.hostName).toBe("MacBook");
  });

  it("does not emit completion when a new run starts idle", () => {
    const prev = [makeAgent({ state: "working", run: "run-1" })];
    const next = [makeAgent({ state: "idle", run: "run-2", returned_idle: false })];

    const events = detectAgentStateTransitions("host-1", "MacBook", prev, next, 1000);
    expect(events).toHaveLength(0);
  });

  it("detects approval, question, and generic blocked transitions", () => {
    const prev = [
      makeAgent({ remote_id: "default::%1", pane: "%1", name: "a1", state: "working" }),
      makeAgent({ remote_id: "default::%2", pane: "%2", name: "a2", state: "working" }),
      makeAgent({ remote_id: "default::%3", pane: "%3", name: "a3", state: "working" }),
    ];
    const next = [
      makeAgent({
        remote_id: "default::%1",
        pane: "%1",
        name: "a1",
        state: "blocked",
        permission_count: 2,
      }),
      makeAgent({
        remote_id: "default::%2",
        pane: "%2",
        name: "a2",
        state: "blocked",
        question_count: 1,
      }),
      makeAgent({
        remote_id: "default::%3",
        pane: "%3",
        name: "a3",
        state: "blocked",
      }),
    ];

    const events = detectAgentStateTransitions("host-1", "MacBook", prev, next, 2000);
    expect(events.map((e) => [e.agentName, e.kind])).toEqual([
      ["a1", "approval_requested"],
      ["a2", "question_asked"],
      ["a3", "blocked"],
    ]);
  });

  it("ignores steady idle/working/exited states and only alerts newly appearing agents if blocked", () => {
    const prev = [makeAgent({ remote_id: "default::%1", state: "idle" })];
    const next = [
      makeAgent({ remote_id: "default::%1", state: "idle" }),
      makeAgent({ remote_id: "default::%2", pane: "%2", name: "fresh-idle", state: "idle" }),
      makeAgent({ remote_id: "default::%3", pane: "%3", name: "fresh-blocked", state: "blocked" }),
      makeAgent({ remote_id: "default::%4", pane: "%4", name: "dead", state: "exited" }),
    ];

    const events = detectAgentStateTransitions("host-1", "MacBook", prev, next, 3000);
    expect(events).toHaveLength(1);
    expect(events[0]?.agentName).toBe("fresh-blocked");
    expect(events[0]?.kind).toBe("blocked");
  });

  it("detects new inbox items once and ignores already seen or acknowledged items", () => {
    const seen = new Set<string>();
    const agents = [makeAgent()];
    const initial: InboxItem[] = [
      { id: 1, kind: "returned_idle", agent: "worker-1", pane: "%1" },
      { id: 2, kind: "question_asked", agent: "worker-1", pane: "%1", ack: true },
    ];
    const first = detectNewInboxNotifications("h1", "Mac", seen, initial, agents, 1000);
    expect(first).toHaveLength(1);
    expect(first[0]?.kind).toBe("turn_completed");

    const second = detectNewInboxNotifications("h1", "Mac", seen, initial, agents, 2000);
    expect(second).toHaveLength(0);
  });

  it("detects newly appearing timeline interactive prompts", () => {
    const agent = makeAgent({ state: "working" });
    const prompt: InteractivePrompt = {
      id: "tui:q1",
      kind: "question",
      mode: "terminal_tui",
      title: "Choose deployment target",
      body: "Staging or production?",
      options: [],
      selected_index: 0,
      multi_select: false,
      allows_free_text: true,
      free_text_only: false,
      allows_cancel: true,
      can_navigate_tabs: false,
      always_patterns: [],
      session_peers_count: 0,
      is_subagent: false,
      questions: [],
    };
    const events = detectNewPromptNotifications("h1", "Mac", agent, [], [prompt], 4000);
    expect(events).toHaveLength(1);
    expect(events[0]?.kind).toBe("question_asked");
    expect(events[0]?.summary).toBe("Choose deployment target");
  });
});

describe("deduplication, preference filtering, formatting, and deep links", () => {
  it("coalesces duplicate notifications within the cooldown window and suppresses generic blocked after specific alerts", () => {
    const deduper = new NotificationDeduplicator(12_000);
    const base = {
      hostId: "h1",
      hostName: "Mac",
      remoteId: "default::%1",
      paneId: "%1",
      agentName: "worker-1",
      occurredMs: 10_000,
    };

    expect(deduper.shouldEmit({ ...base, kind: "question_asked" }, 10_000)).toBe(true);
    expect(deduper.shouldEmit({ ...base, kind: "question_asked" }, 11_000)).toBe(false);
    expect(deduper.shouldEmit({ ...base, kind: "blocked" }, 12_000)).toBe(false);
    expect(deduper.shouldEmit({ ...base, kind: "question_asked" }, 25_000)).toBe(true);
  });

  it("filters deliveries according to user notification preferences and active agent context", () => {
    const completionEvt = {
      hostId: "h1",
      hostName: "Mac",
      remoteId: "default::%1",
      paneId: "%1",
      agentName: "worker-1",
      kind: "turn_completed" as const,
      occurredMs: 1000,
    };
    const ctxSameAgent = {
      appInForeground: true,
      activePanel: "agent" as const,
      activeHostId: "h1",
      activeAgentRemoteId: "default::%1",
    };

    expect(
      shouldDeliverNotification(completionEvt, DEFAULT_NOTIFICATION_PREFERENCES, ctxSameAgent)
    ).toBe(true);
    expect(
      shouldDeliverNotification(
        completionEvt,
        { ...DEFAULT_NOTIFICATION_PREFERENCES, notifyActiveAgent: false },
        ctxSameAgent
      )
    ).toBe(false);
    expect(
      shouldDeliverNotification(
        completionEvt,
        { ...DEFAULT_NOTIFICATION_PREFERENCES, notifyActiveAgent: false },
        { ...ctxSameAgent, appInForeground: false }
      )
    ).toBe(true);
    expect(
      shouldDeliverNotification(
        completionEvt,
        { ...DEFAULT_NOTIFICATION_PREFERENCES, notifyTaskCompleted: false },
        { ...ctxSameAgent, appInForeground: false }
      )
    ).toBe(false);
  });

  it("formats Korean and English copy with proper Android channels", () => {
    const evt = {
      hostId: "h1",
      hostName: "MacBook",
      remoteId: "default::%1",
      paneId: "%1",
      agentName: "api-bot",
      kind: "approval_requested" as const,
      permissionCount: 2,
      occurredMs: 1000,
    };
    const ko = formatNotificationCopy(evt, "ko");
    expect(ko.title).toContain("승인 요청 (2)");
    expect(ko.title).toContain("api-bot");
    expect(ko.channelId).toBe("masil-attention");

    const en = formatNotificationCopy({ ...evt, kind: "turn_completed" }, "en");
    expect(en.title).toContain("Task completed");
    expect(en.channelId).toBe("masil-completed");
  });

  it("extracts deep-link targets from local, FCM, APNs nested, and URI payloads", () => {
    expect(
      extractDeepLinkTarget({ host_id: "h1", remote_id: "default::%3", agent_id: "%3" })
    ).toEqual({ hostId: "h1", remoteId: "default::%3", paneId: "%3" });

    expect(
      extractDeepLinkTarget({ masil: { host_id: "h2", agent_id: "%9" } })
    ).toEqual({ hostId: "h2", remoteId: undefined, paneId: "%9" });

    expect(
      extractDeepLinkTarget({ deep_link: "masil://host/mac-1/agent/%254" })
    ).toEqual({ hostId: "mac-1", remoteId: undefined, paneId: "%4" });
  });

  it("counts live agents needing attention for app icon badges", () => {
    const agents = [
      makeAgent({ remote_id: "a", state: "blocked" }),
      makeAgent({ remote_id: "b", state: "working", permission_count: 1 }),
      makeAgent({ remote_id: "c", state: "working" }),
      makeAgent({ remote_id: "d", state: "idle" }),
      makeAgent({ remote_id: "e", state: "blocked", stale: true }),
    ];
    expect(countAgentsNeedingAttention(agents)).toBe(2);
  });
});
