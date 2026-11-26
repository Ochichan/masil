import { describe, expect, it } from "vitest";
import { en } from "../i18n/en";
import { ko } from "../i18n/ko";
import type { TimelineItem, TimelinePage } from "../protocol/schemas";
import {
  applyTimelineDeltaToPage,
  type TimelineDeltaEvent,
} from "./timeline-delta";

function makePage(items: TimelineItem[], sessionId = "sess-1"): TimelinePage {
  return {
    binding_confidence: "reported_by_hook",
    session_id: sessionId,
    source_label: "claude",
    detected_model: "claude-sonnet-4-6",
    items,
    active_prompts: [],
    has_more: false,
  };
}

describe("applyTimelineDeltaToPage", () => {
  it("replaces the entire page on mode=replace (session switch)", () => {
    const prev = makePage(
      [{ kind: "user_message", id: "u1", seq: 1, ts_ms: 100, text: "hi" }],
      "sess-1"
    );
    const next = makePage(
      [{ kind: "user_message", id: "u2", seq: 1, ts_ms: 200, text: "new" }],
      "sess-2"
    );
    const delta: TimelineDeltaEvent = {
      mode: "replace",
      page: next,
    };
    const merged = applyTimelineDeltaToPage(prev, delta);
    expect(merged).toEqual(next);
  });

  it("appends tail items on mode=patch without disturbing unchanged prefix", () => {
    const first: TimelineItem = {
      kind: "user_message",
      id: "u1",
      seq: 1,
      ts_ms: 100,
      text: "hello",
    };
    const second: TimelineItem = {
      kind: "assistant_message",
      id: "a1",
      seq: 2,
      ts_ms: 200,
      markdown: "world",
    };
    const prev = makePage([first]);
    const merged = applyTimelineDeltaToPage(prev, {
      mode: "patch",
      from_index: 1,
      items: [second],
      detected_model: "claude-opus-4-7",
      has_more: true,
    });
    expect(merged).not.toBeNull();
    expect(merged!.items).toEqual([first, second]);
    expect(merged!.detected_model).toBe("claude-opus-4-7");
    expect(merged!.has_more).toBe(true);
  });

  it("updates streaming tail item in-place when from_index points inside prev.items", () => {
    const prev = makePage([
      { kind: "user_message", id: "u1", seq: 1, ts_ms: 100, text: "hello" },
      { kind: "assistant_message", id: "a1", seq: 2, ts_ms: 200, markdown: "par" },
    ]);
    const updatedTail: TimelineItem = {
      kind: "assistant_message",
      id: "a1",
      seq: 2,
      ts_ms: 250,
      markdown: "partial -> complete",
    };
    const merged = applyTimelineDeltaToPage(prev, {
      mode: "patch",
      from_index: 1,
      items: [updatedTail],
    });
    expect(merged!.items).toHaveLength(2);
    expect(merged!.items[1]).toEqual(updatedTail);
  });

  it("returns null on mode=patch when no previous page exists", () => {
    const merged = applyTimelineDeltaToPage(undefined, {
      mode: "patch",
      from_index: 0,
      items: [],
    });
    expect(merged).toBeNull();
  });
});

describe("mobile i18n key parity", () => {
  it("keeps en and ko dictionaries strictly symmetric with non-empty values", () => {
    const enKeys = Object.keys(en).sort();
    const koKeys = Object.keys(ko).sort();
    expect(koKeys).toEqual(enKeys);
    for (const key of enKeys) {
      expect(String(en[key as keyof typeof en]).trim().length).toBeGreaterThan(0);
      expect(String(ko[key as keyof typeof ko]).trim().length).toBeGreaterThan(0);
    }
  });
});
