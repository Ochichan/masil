import { describe, expect, it } from "vitest";
import {
  InteractivePromptSchema,
  TimelineItemSchema,
  type AgentRow,
  type TimelinePage,
} from "./schemas";

describe("interactive prompt wire schemas", () => {
  it("validates structured API permission prompts with patterns and subagent flag", () => {
    const parsed = InteractivePromptSchema.parse({
      id: "api:per_1",
      kind: "permission",
      mode: "structured_api",
      request_id: "per_1",
      title: "bash",
      body: "git status",
      options: [
        { id: "once", index: 0, label: "Allow once", selected: true },
        { id: "always", index: 1, label: "Always allow" },
        { id: "reject", index: 2, label: "Reject", is_destructive: true },
      ],
      selected_index: 0,
      always_patterns: ["git *"],
      session_peers_count: 2,
      is_subagent: true,
    });
    expect(parsed.mode).toBe("structured_api");
    expect(parsed.always_patterns).toEqual(["git *"]);
    expect(parsed.options[2]?.is_destructive).toBe(true);
    expect(parsed.is_subagent).toBe(true);
  });

  it("validates structured API multi-select question prompts", () => {
    const parsed = InteractivePromptSchema.parse({
      id: "api:que_1",
      kind: "question",
      mode: "structured_api",
      request_id: "que_1",
      title: "Choose targets",
      body: "Which targets should we build?",
      questions: [
        {
          header: "Targets",
          question: "Which targets should we build?",
          options: [
            { label: "iOS", description: "Apple simulator" },
            { label: "Android", description: "Pixel emulator" },
          ],
          custom: true,
          multiple: true,
        },
      ],
    });
    expect(parsed.questions).toHaveLength(1);
    expect(parsed.questions[0]?.multiple).toBe(true);
    expect(parsed.questions[0]?.custom).toBe(true);
  });

  it("validates live TUI agy/claude question prompts with custom input and screen excerpt", () => {
    const parsed = InteractivePromptSchema.parse({
      id: "tui:abc123",
      kind: "question",
      mode: "terminal_tui",
      title: "Architecture",
      body: "Pick an approach",
      step_label: "1/2",
      options: [
        { id: "opt-0", index: 0, label: "Option A", selected: true, hotkey: "1" },
        { id: "opt-1", index: 1, label: "Option B", selected: false, hotkey: "2" },
        {
          id: "opt-2",
          index: 2,
          label: "Other",
          selected: false,
          hotkey: "3",
          is_custom_input: true,
        },
      ],
      selected_index: 0,
      multi_select: false,
      allows_free_text: true,
      free_text_only: false,
      allows_cancel: true,
      can_navigate_tabs: true,
      screen_excerpt: "Pick an approach\n❯ 1. Option A\n  2. Option B\n  3. Other",
      hint_line: "↑/↓ to navigate · Enter to confirm · Esc to cancel",
    });
    expect(parsed.options[2]?.is_custom_input).toBe(true);
    expect(parsed.step_label).toBe("1/2");
    expect(parsed.screen_excerpt).toContain("Option A");

    const page: TimelinePage = {
      binding_confidence: "reported_by_hook",
      session_id: "sess-1",
      source_label: "agy_transcript",
      items: [
        TimelineItemSchema.parse({
          kind: "user_message",
          id: "u1",
          seq: 1,
          ts_ms: 1000,
          text: "hello",
        }),
      ],
      active_prompts: [parsed],
      has_more: false,
    };
    expect(page.active_prompts).toHaveLength(1);

    const row: AgentRow = {
      remote_id: "default::%0",
      server_name: "default",
      server_socket: "/tmp/masil-501/default",
      pane: "%0",
      name: "agy-0",
      provider: "agy",
      state: "blocked",
      cwd: "/repo",
      permission_count: 1,
      question_count: 2,
      subagent_count: 0,
    };
    expect(row.permission_count).toBe(1);
    expect(row.question_count).toBe(2);
  });
});
