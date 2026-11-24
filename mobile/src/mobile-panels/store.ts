/**
 * Single normalized-position 3-panel store (-1 left, 0 center, +1 right)
 * with revision-based gesture arbitration so a panel and its backdrop or
 * two drawers can never disagree.
 */
import { create } from "zustand";

export type MobilePanelTarget = "agent-list" | "agent" | "workspace-tools";

export function targetToPosition(target: MobilePanelTarget): -1 | 0 | 1 {
  switch (target) {
    case "agent-list":
      return -1;
    case "agent":
      return 0;
    case "workspace-tools":
      return 1;
  }
}

export function positionToTarget(pos: number): MobilePanelTarget {
  if (pos <= -0.45) return "agent-list";
  if (pos >= 0.45) return "workspace-tools";
  return "agent";
}

interface MobilePanelsState {
  target: MobilePanelTarget;
  settled: MobilePanelTarget;
  revision: number;
  openGestureBlockers: number;
  selectPanel: (target: MobilePanelTarget) => void;
  settleAt: (target: MobilePanelTarget, revision: number) => void;
  pushOpenBlocker: () => void;
  popOpenBlocker: () => void;
}

export const useMobilePanelsStore = create<MobilePanelsState>((set) => ({
  target: "agent",
  settled: "agent",
  revision: 0,
  openGestureBlockers: 0,
  selectPanel: (next) =>
    set((s) =>
      s.target === next
        ? s
        : {
            target: next,
            revision: s.revision + 1,
          }
    ),
  settleAt: (settledTarget, rev) =>
    set((s) =>
      rev < s.revision
        ? s
        : {
            target: settledTarget,
            settled: settledTarget,
          }
    ),
  pushOpenBlocker: () =>
    set((s) => ({ openGestureBlockers: s.openGestureBlockers + 1 })),
  popOpenBlocker: () =>
    set((s) => ({
      openGestureBlockers: Math.max(0, s.openGestureBlockers - 1),
    })),
}));
