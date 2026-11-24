/**
 * Demand-driven offline replica cache backed by `expo-sqlite`.
 * Lets the mobile UI paint `Last known` agents and conversation timelines
 * immediately before network reconnection settles.
 */
import * as SQLite from "expo-sqlite";
import type { AgentRow, TimelinePage } from "../protocol/schemas";

let dbPromise: Promise<SQLite.SQLiteDatabase> | null = null;

async function getDb(): Promise<SQLite.SQLiteDatabase> {
  if (!dbPromise) {
    dbPromise = (async () => {
      const db = await SQLite.openDatabaseAsync("masil-replica.db");
      await db.execAsync(`
        CREATE TABLE IF NOT EXISTS agent_cache (
          host_id TEXT PRIMARY KEY NOT NULL,
          updated_ms INTEGER NOT NULL,
          payload_json TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS timeline_cache (
          cache_key TEXT PRIMARY KEY NOT NULL,
          updated_ms INTEGER NOT NULL,
          payload_json TEXT NOT NULL
        );
      `);
      return db;
    })();
  }
  return dbPromise;
}

export async function saveCachedAgents(
  hostId: string,
  agents: AgentRow[]
): Promise<void> {
  try {
    const db = await getDb();
    await db.runAsync(
      "INSERT OR REPLACE INTO agent_cache (host_id, updated_ms, payload_json) VALUES (?, ?, ?)",
      [hostId, Date.now(), JSON.stringify(agents)]
    );
  } catch {
    // Non-fatal cache write failure.
  }
}

export async function loadCachedAgents(hostId: string): Promise<AgentRow[]> {
  try {
    const db = await getDb();
    const row = await db.getFirstAsync<{ payload_json: string }>(
      "SELECT payload_json FROM agent_cache WHERE host_id = ?",
      [hostId]
    );
    if (!row) return [];
    return JSON.parse(row.payload_json) as AgentRow[];
  } catch {
    return [];
  }
}

export async function saveCachedTimeline(
  hostId: string,
  remoteId: string,
  page: TimelinePage
): Promise<void> {
  try {
    const db = await getDb();
    await db.runAsync(
      "INSERT OR REPLACE INTO timeline_cache (cache_key, updated_ms, payload_json) VALUES (?, ?, ?)",
      [`${hostId}::${remoteId}`, Date.now(), JSON.stringify(page)]
    );
  } catch {
    // Non-fatal cache write failure.
  }
}

export async function loadCachedTimeline(
  hostId: string,
  remoteId: string
): Promise<TimelinePage | null> {
  try {
    const db = await getDb();
    const row = await db.getFirstAsync<{ payload_json: string }>(
      "SELECT payload_json FROM timeline_cache WHERE cache_key = ?",
      [`${hostId}::${remoteId}`]
    );
    if (!row) return null;
    return JSON.parse(row.payload_json) as TimelinePage;
  } catch {
    return null;
  }
}
