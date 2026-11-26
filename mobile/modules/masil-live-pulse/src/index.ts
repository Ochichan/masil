import { requireOptionalNativeModule } from "expo-modules-core";

export interface LivePulseUpdatePayload {
  enabled: boolean;
  hostId: string;
  hostName: string;
  workingCount: number;
  blockedCount: number;
  doneCount: number;
  alertCount: number;
  topAgentAlias?: string | null;
  topEventKind?: string | null;
  topSummaryLine?: string | null;
  deepLink: string;
  updatedAt: number;
}

export interface LivePulseTokenEvent {
  kind: "push_to_start" | "activity_update";
  hostId?: string;
  token: string;
}

interface NativeLivePulseModule {
  isSupported(): boolean;
  observePushToStartToken(enabled: boolean): Promise<{ observing: boolean }>;
  startOrUpdatePulse(
    payload: LivePulseUpdatePayload
  ): Promise<{ active: boolean; activityId?: string; reason?: string; error?: string }>;
  endPulse(hostId?: string): Promise<void>;
  addListener?(
    eventName: string,
    listener: (payload: Record<string, unknown>) => void
  ): { remove: () => void };
}

const nativeMod =
  requireOptionalNativeModule<NativeLivePulseModule>("MasilLivePulse");

export function isLivePulseNativeSupported(): boolean {
  try {
    return nativeMod ? Boolean(nativeMod.isSupported()) : false;
  } catch {
    return false;
  }
}

export async function setLivePulsePushToStartObserver(
  enabled: boolean
): Promise<boolean> {
  if (!nativeMod) return false;
  try {
    const res = await nativeMod.observePushToStartToken(enabled);
    return Boolean(res?.observing);
  } catch {
    return false;
  }
}

export async function updateNativeLivePulse(
  payload: LivePulseUpdatePayload
): Promise<boolean> {
  if (!nativeMod || !payload.enabled) return false;
  try {
    const res = await nativeMod.startOrUpdatePulse(payload);
    return Boolean(res?.active);
  } catch {
    return false;
  }
}

export async function endNativeLivePulse(hostId?: string): Promise<void> {
  if (!nativeMod) return;
  try {
    await nativeMod.endPulse(hostId);
  } catch {
    // Non-fatal on unsupported OS versions.
  }
}

export function subscribeLivePulseTokens(
  listener: (event: LivePulseTokenEvent) => void
): () => void {
  if (!nativeMod?.addListener) return () => {};
  const sub = nativeMod.addListener("onPushToken", (ev) => {
    const kind =
      ev.kind === "push_to_start" ? "push_to_start" : "activity_update";
    const token = typeof ev.token === "string" ? ev.token : "";
    if (!token) return;
    listener({
      kind,
      hostId: typeof ev.hostId === "string" ? ev.hostId : undefined,
      token,
    });
  });
  return () => sub.remove();
}
