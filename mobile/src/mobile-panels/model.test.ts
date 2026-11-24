import { describe, expect, it } from "vitest";
import {
  positionToTarget,
  targetToPosition,
  useMobilePanelsStore,
} from "./store";

describe("3-panel mobile swipe model", () => {
  it("maps normalized positions and arbitrates stale settlement revisions", () => {
    expect(targetToPosition("agent-list")).toBe(-1);
    expect(targetToPosition("agent")).toBe(0);
    expect(targetToPosition("workspace-tools")).toBe(1);
    expect(positionToTarget(-0.7)).toBe("agent-list");
    expect(positionToTarget(0.1)).toBe("agent");
    expect(positionToTarget(0.7)).toBe("workspace-tools");

    const store = useMobilePanelsStore;
    store.getState().selectPanel("agent-list");
    const rev1 = store.getState().revision;
    store.getState().selectPanel("workspace-tools");
    const rev2 = store.getState().revision;
    expect(rev2).toBeGreaterThan(rev1);

    // Stale settle from rev1 must be ignored.
    store.getState().settleAt("agent-list", rev1);
    expect(store.getState().target).toBe("workspace-tools");

    store.getState().settleAt("workspace-tools", rev2);
    expect(store.getState().settled).toBe("workspace-tools");
  });
});
