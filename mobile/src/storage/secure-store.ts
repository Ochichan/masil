import * as SecureStore from "expo-secure-store";
import type { SavedHost } from "../protocol/schemas";

const HOSTS_KEY = "masil.saved_hosts.v1";

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
