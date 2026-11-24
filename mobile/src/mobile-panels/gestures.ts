import { useEffect } from "react";
import { useMobilePanelsStore } from "./store";

/**
 * Registers an active horizontal-scroll/pan surface (e.g., Desktop-size
 * LiveTerminalTab) so horizontal panning does not accidentally open a side panel.
 */
export function useBlockMobilePanelOpenGestures(active: boolean): void {
  const push = useMobilePanelsStore((s) => s.pushOpenBlocker);
  const pop = useMobilePanelsStore((s) => s.popOpenBlocker);

  useEffect(() => {
    if (!active) return;
    push();
    return () => {
      pop();
    };
  }, [active, push, pop]);
}
