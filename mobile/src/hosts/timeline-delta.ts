import type {
  BindingConfidence,
  InteractivePrompt,
  TimelineItem,
  TimelinePage,
} from "../protocol/schemas";

export interface TimelineDeltaEvent {
  mode: "patch" | "replace";
  from_index?: number;
  items?: TimelineItem[];
  page?: TimelinePage;
  binding_confidence?: BindingConfidence;
  session_id?: string | null;
  source_label?: string;
  detected_model?: string | null;
  has_more?: boolean;
  active_prompts?: InteractivePrompt[];
}

export function applyTimelineDeltaToPage(
  prev: TimelinePage | undefined,
  delta: TimelineDeltaEvent
): TimelinePage | null {
  if (delta.mode === "replace" && delta.page) {
    return delta.page;
  }
  if (delta.mode === "patch" && prev && Array.isArray(delta.items)) {
    const idx = Math.max(
      0,
      Math.min(delta.from_index ?? prev.items.length, prev.items.length)
    );
    const mergedItems = [...prev.items.slice(0, idx), ...delta.items];
    return {
      binding_confidence: delta.binding_confidence ?? prev.binding_confidence,
      session_id:
        delta.session_id !== undefined ? delta.session_id : prev.session_id,
      source_label: delta.source_label ?? prev.source_label,
      detected_model:
        delta.detected_model !== undefined
          ? delta.detected_model
          : prev.detected_model,
      items: mergedItems,
      active_prompts: delta.active_prompts ?? prev.active_prompts,
      has_more: delta.has_more ?? prev.has_more,
    };
  }
  return null;
}
