import * as SecureStore from "expo-secure-store";
import {
  DEFAULT_LIVE_PULSE_PREFERENCES,
} from "../agents/phase2-helpers";
import {
  DEFAULT_NOTIFICATION_PREFERENCES,
  type NotificationPreferences,
} from "../notifications/agent-notifications";
import type { LivePulsePreferences, SavedHost } from "../protocol/schemas";

const HOSTS_KEY = "masil.saved_hosts.v1";
const NOTIFICATION_PREFS_KEY = "masil.notification_prefs.v1";
const LIVE_PULSE_PREFS_KEY = "masil.live_pulse_prefs.v1";

export async function loadSavedHosts(): Promise<SavedHost[]> {
  try {
    const raw = await SecureStore.getItemAsync(HOSTS_KEY);
    if (!raw) return [];
    const parsed = JSON.parse(raw);
    return Array.isArray(parsed) ? (parsed as SavedHost[]) : [];
  } catch {
    return [];
  }
}

export async function saveHostRecord(host: SavedHost): Promise<SavedHost[]> {
  const existing = await loadSavedHosts();
  const filtered = existing.filter((h) => h.hostId !== host.hostId);
  const next = [host, ...filtered];
  await SecureStore.setItemAsync(HOSTS_KEY, JSON.stringify(next));
  return next;
}

export async function removeHostRecord(hostId: string): Promise<SavedHost[]> {
  const existing = await loadSavedHosts();
  const next = existing.filter((h) => h.hostId !== hostId);
  await SecureStore.setItemAsync(HOSTS_KEY, JSON.stringify(next));
  return next;
}

export async function loadNotificationPreferences(): Promise<NotificationPreferences> {
  try {
    const raw = await SecureStore.getItemAsync(NOTIFICATION_PREFS_KEY);
    if (!raw) return { ...DEFAULT_NOTIFICATION_PREFERENCES };
    const parsed = JSON.parse(raw) as Partial<NotificationPreferences>;
    return {
      enabled:
        typeof parsed.enabled === "boolean"
          ? parsed.enabled
          : DEFAULT_NOTIFICATION_PREFERENCES.enabled,
      notifyInputNeeded:
        typeof parsed.notifyInputNeeded === "boolean"
          ? parsed.notifyInputNeeded
          : DEFAULT_NOTIFICATION_PREFERENCES.notifyInputNeeded,
      notifyTaskCompleted:
        typeof parsed.notifyTaskCompleted === "boolean"
          ? parsed.notifyTaskCompleted
          : DEFAULT_NOTIFICATION_PREFERENCES.notifyTaskCompleted,
      notifyActiveAgent:
        typeof parsed.notifyActiveAgent === "boolean"
          ? parsed.notifyActiveAgent
          : DEFAULT_NOTIFICATION_PREFERENCES.notifyActiveAgent,
    };
  } catch {
    return { ...DEFAULT_NOTIFICATION_PREFERENCES };
  }
}

export async function saveNotificationPreferences(
  prefs: NotificationPreferences
): Promise<void> {
  try {
    await SecureStore.setItemAsync(
      NOTIFICATION_PREFS_KEY,
      JSON.stringify(prefs)
    );
  } catch {
    // Non-fatal preference persistence failure.
  }
}

export async function loadLivePulsePreferences(): Promise<LivePulsePreferences> {
  try {
    const raw = await SecureStore.getItemAsync(LIVE_PULSE_PREFS_KEY);
    if (!raw) return { ...DEFAULT_LIVE_PULSE_PREFERENCES };
    const parsed = JSON.parse(raw) as Partial<LivePulsePreferences>;
    return {
      enabled: parsed.enabled === true,
      summaryEnabled: parsed.enabled === true && parsed.summaryEnabled === true,
    };
  } catch {
    return { ...DEFAULT_LIVE_PULSE_PREFERENCES };
  }
}

export async function saveLivePulsePreferences(
  prefs: LivePulsePreferences
): Promise<void> {
  try {
    await SecureStore.setItemAsync(
      LIVE_PULSE_PREFS_KEY,
      JSON.stringify({
        enabled: prefs.enabled,
        summaryEnabled: prefs.enabled && prefs.summaryEnabled,
      })
    );
  } catch {
    // Non-fatal preference persistence failure.
  }
}
