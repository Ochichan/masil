/**
 * Pure Phase 2 helpers (localhost preview diagnostics, port validation,
 * markup raster PNG encoder, upload chunking, Focus Shield formatting, and
 * D-09 Live Pulse opt-in payload projection).
 *
 * Kept completely free of React Native / Expo runtime imports so every helper
 * runs deterministically under Vitest in Node.
 */
import type {
  AgentRow,
  LivePulsePayload,
  LivePulsePreferences,
} from "../protocol/schemas";

export const DEFAULT_LIVE_PULSE_PREFERENCES: LivePulsePreferences = {
  enabled: false,
  summaryEnabled: false,
};

export interface ViewportPreset {
  id: "mobile" | "tablet" | "desktop";
  label: string;
  width: number;
  height: number;
}

export const VIEWPORT_PRESETS: ViewportPreset[] = [
  { id: "mobile", label: "390×844", width: 390, height: 844 },
  { id: "tablet", label: "768×1024", width: 768, height: 1024 },
  { id: "desktop", label: "1280×800", width: 1280, height: 800 },
];

export interface WebViewDiagnosticEntry {
  kind: "console" | "network";
  message: string;
  ts: number;
}

export type MarkupTool = "pen" | "rect" | "arrow" | "pin";

export interface MarkupPoint {
  x: number; // normalized 0..1
  y: number; // normalized 0..1
}

export type MarkupAnnotation =
  | { id: string; tool: "pen"; points: MarkupPoint[]; color: string }
  | {
      id: string;
      tool: "rect";
      start: MarkupPoint;
      end: MarkupPoint;
      color: string;
    }
  | {
      id: string;
      tool: "arrow";
      start: MarkupPoint;
      end: MarkupPoint;
      color: string;
    }
  | {
      id: string;
      tool: "pin";
      point: MarkupPoint;
      number: number;
      color: string;
    };

/**
 * Script injected via `injectedJavaScriptBeforeContentLoaded` in `LocalhostPreviewView`
 * to capture `console.error`, `window.onerror`, `unhandledrejection`, and 4xx/5xx
 * `fetch`/`XHR` responses (D-08: recent 20 ring buffer).
 */
export function buildWebViewDiagnosticsBootstrapScript(): string {
  return `(function(){
  if (window.__masilDiagInstalled) return;
  window.__masilDiagInstalled = true;
  function post(kind, msg) {
    try {
      if (window.ReactNativeWebView && window.ReactNativeWebView.postMessage) {
        window.ReactNativeWebView.postMessage(JSON.stringify({
          __masil_diag: true,
          kind: kind,
          message: String(msg || '').slice(0, 400),
          ts: Date.now()
        }));
      }
    } catch (_) {}
  }
  var origErr = console.error;
  console.error = function() {
    try {
      var text = Array.prototype.slice.call(arguments).map(function(a) {
        if (a instanceof Error) return a.message || String(a);
        if (typeof a === 'object') { try { return JSON.stringify(a); } catch(_) {} }
        return String(a);
      }).join(' ');
      post('console', text);
    } catch (_) {}
    if (origErr) origErr.apply(console, arguments);
  };
  window.addEventListener('error', function(ev) {
    post('console', (ev && ev.message) || 'Window error');
  });
  window.addEventListener('unhandledrejection', function(ev) {
    var r = ev && ev.reason;
    post('console', 'UnhandledRejection: ' + (r && r.message ? r.message : String(r)));
  });
  if (typeof window.fetch === 'function') {
    var origFetch = window.fetch;
    window.fetch = function(input, init) {
      var method = (init && init.method) || 'GET';
      var url = typeof input === 'string' ? input : (input && input.url) || '';
      return origFetch.apply(this, arguments).then(function(resp) {
        if (resp && resp.status >= 400) {
          post('network', method.toUpperCase() + ' ' + url + ' -> ' + resp.status);
        }
        return resp;
      }, function(err) {
        post('network', method.toUpperCase() + ' ' + url + ' -> FAILED (' + (err && err.message ? err.message : err) + ')');
        throw err;
      });
    };
  }
  if (typeof XMLHttpRequest === 'function') {
    var origOpen = XMLHttpRequest.prototype.open;
    var origSend = XMLHttpRequest.prototype.send;
    XMLHttpRequest.prototype.open = function(method, url) {
      this.__masilMethod = String(method || 'GET').toUpperCase();
      this.__masilUrl = String(url || '');
      return origOpen.apply(this, arguments);
    };
    XMLHttpRequest.prototype.send = function() {
      this.addEventListener('loadend', function() {
        if (this.status >= 400 || this.status === 0) {
          post('network', (this.__masilMethod || 'GET') + ' ' + (this.__masilUrl || '') + ' -> ' + this.status);
        }
      });
      return origSend.apply(this, arguments);
    };
  }
})(); true;`;
}

export function parseDiagnosticMessage(
  raw: string
): WebViewDiagnosticEntry | null {
  try {
    const parsed = JSON.parse(raw) as Record<string, unknown>;
    if (parsed.__masil_diag !== true) return null;
    const kind = parsed.kind === "network" ? "network" : "console";
    const message =
      typeof parsed.message === "string" ? parsed.message.trim() : "";
    if (!message) return null;
    const ts = typeof parsed.ts === "number" ? parsed.ts : Date.now();
    return { kind, message, ts };
  } catch {
    return null;
  }
}

export function appendDiagnosticRingBuffer(
  existing: WebViewDiagnosticEntry[],
  entry: WebViewDiagnosticEntry,
  maxItems = 20
): WebViewDiagnosticEntry[] {
  const next = [...existing, entry];
  return next.length > maxItems ? next.slice(next.length - maxItems) : next;
}

export function validateManualPortInput(
  raw: string,
  deniedPorts: number[]
): { ok: true; port: number } | { ok: false; reason: "invalid" | "denied" } {
  const cleaned = raw.trim().replace(/^:/, "");
  if (!/^\d{1,5}$/.test(cleaned)) {
    return { ok: false, reason: "invalid" };
  }
  const port = Number.parseInt(cleaned, 10);
  if (!Number.isInteger(port) || port < 1 || port > 65535) {
    return { ok: false, reason: "invalid" };
  }
  if (deniedPorts.includes(port)) {
    return { ok: false, reason: "denied" };
  }
  return { ok: true, port };
}

export function chunkBinaryAsset(
  bytes: Uint8Array,
  chunkSize = 32 * 1024
): Uint8Array[] {
  const chunks: Uint8Array[] = [];
  for (let i = 0; i < bytes.length; i += chunkSize) {
    chunks.push(bytes.subarray(i, Math.min(i + chunkSize, bytes.length)));
  }
  return chunks;
}

export function formatFocusRemaining(remainingMs: number, isKo: boolean): string {
  const totalSec = Math.max(0, Math.ceil(remainingMs / 1000));
  const hours = Math.floor(totalSec / 3600);
  const mins = Math.floor((totalSec % 3600) / 60);
  const secs = totalSec % 60;
  if (hours > 0) {
    return isKo ? `${hours}시간 ${mins}분` : `${hours}h ${mins}m`;
  }
  if (mins > 0) {
    return isKo ? `${mins}분` : `${mins}m`;
  }
  return isKo ? `${secs}초` : `${secs}s`;
}

/**
 * Projects a D-09 Live Pulse payload honoring user opt-in settings.
 * When `prefs.summaryEnabled` is false, `top_summary_line` is ALWAYS stripped.
 */
export function projectLivePulseForDevice(
  pulse: LivePulsePayload,
  prefs: LivePulsePreferences
): LivePulsePayload | null {
  if (!prefs.enabled) return null;
  return {
    ...pulse,
    top_agent_alias: pulse.top_agent_alias
      ? pulse.top_agent_alias.slice(0, 32)
      : null,
    top_summary_line: prefs.summaryEnabled
      ? pulse.top_summary_line?.slice(0, 60) ?? null
      : null,
  };
}

/**
 * Pure TypeScript fallback Live Pulse aggregator from client-side `AgentRow[]`
 * (used when the host sends `agents.changed` directly).
 */
export function buildLocalLivePulse(
  hostId: string,
  hostName: string,
  agents: AgentRow[],
  prefs: LivePulsePreferences
): LivePulsePayload | null {
  if (!prefs.enabled) return null;
  let working = 0;
  let blocked = 0;
  let done = 0;
  let alerts = 0;
  let topAlias: string | null = null;
  let topKind: string | null = null;
  let topPane: string | null = null;
  let topPrio = 0;

  for (const a of agents) {
    if (a.state === "exited") continue;
    const perm = a.permission_count ?? a.pending_approvals ?? 0;
    const q = a.question_count ?? a.pending_questions ?? 0;
    const tripped = Boolean(a.breaker?.tripped);
    const hardColl =
      a.collision_severity === "hard_conflict" ||
      a.collision_severity === "same_directory_race";
    const critAudit = (a.audit_summary?.critical_count ?? 0) > 0;

    if (a.state === "working" || a.state === "retrying") working += 1;
    else if (a.state === "blocked" || perm > 0 || q > 0) blocked += 1;
    else if (a.state === "idle") done += 1;
    if (tripped || hardColl || critAudit) alerts += 1;

    let prio = 0;
    let kind: string | null = null;
    if (tripped) {
      prio = 6;
      kind = "loop_paused";
    } else if (hardColl) {
      prio = 5;
      kind = "hard_conflict";
    } else if (perm > 0) {
      prio = 4;
      kind = "approval_requested";
    } else if (q > 0 || a.state === "blocked") {
      prio = 3;
      kind = "question_asked";
    } else if (critAudit) {
      prio = 2;
      kind = "bs_flagged";
    } else if (a.state === "idle") {
      prio = 1;
      kind = "turn_finished";
    }
    if (prio > topPrio) {
      topPrio = prio;
      topAlias = (a.name || a.pane).slice(0, 32);
      topKind = kind;
      topPane = a.pane || null;
    }
  }

  return {
    host_id: hostId,
    host_name: hostName,
    working_count: working,
    blocked_count: blocked,
    done_count: done,
    alert_count: alerts,
    top_agent_alias: topAlias,
    top_event_kind: topKind,
    top_summary_line: null,
    deep_link: topPane
      ? `masil://host/${encodeURIComponent(hostId)}/agent/${encodeURIComponent(topPane)}`
      : `masil://host/${encodeURIComponent(hostId)}`,
    updated_at: Math.floor(Date.now() / 1000),
  };
}

// ---------------------------------------------------------------------------
// Pure TypeScript RGBA -> PNG encoder for finger markup annotations (D-08)
// Produces a valid PNG (with IHDR, uncompressed zlib IDAT, IEND + CRC32/Adler32)
// with zero native view-shot dependency, guaranteeing reliable upload everywhere.
// ---------------------------------------------------------------------------

const CRC_TABLE = (() => {
  const table = new Uint32Array(256);
  for (let n = 0; n < 256; n += 1) {
    let c = n;
    for (let k = 0; k < 8; k += 1) {
      c = (c & 1) !== 0 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    }
    table[n] = c >>> 0;
  }
  return table;
})();

function crc32(bytes: Uint8Array): number {
  let c = 0xffffffff;
  for (let i = 0; i < bytes.length; i += 1) {
    c = CRC_TABLE[(c ^ bytes[i]!) & 0xff]! ^ (c >>> 8);
  }
  return (c ^ 0xffffffff) >>> 0;
}

function adler32(bytes: Uint8Array): number {
  let s1 = 1;
  let s2 = 0;
  for (let i = 0; i < bytes.length; i += 1) {
    s1 = (s1 + bytes[i]!) % 65521;
    s2 = (s2 + s1) % 65521;
  }
  return ((s2 << 16) | s1) >>> 0;
}

function pngChunk(type: string, data: Uint8Array): Uint8Array {
  const out = new Uint8Array(12 + data.length);
  const view = new DataView(out.buffer);
  view.setUint32(0, data.length, false);
  for (let i = 0; i < 4; i += 1) {
    out[4 + i] = type.charCodeAt(i);
  }
  out.set(data, 8);
  const crc = crc32(out.subarray(4, 8 + data.length));
  view.setUint32(8 + data.length, crc, false);
  return out;
}

function zlibStore(raw: Uint8Array): Uint8Array {
  const maxBlock = 65535;
  const blocks = Math.max(1, Math.ceil(raw.length / maxBlock));
  const out = new Uint8Array(2 + raw.length + blocks * 5 + 4);
  out[0] = 0x78;
  out[1] = 0x01;
  let dst = 2;
  let src = 0;
  for (let b = 0; b < blocks; b += 1) {
    const len = Math.min(maxBlock, raw.length - src);
    const isFinal = b === blocks - 1 ? 1 : 0;
    out[dst++] = isFinal;
    out[dst++] = len & 0xff;
    out[dst++] = (len >> 8) & 0xff;
    const nlen = ~len & 0xffff;
    out[dst++] = nlen & 0xff;
    out[dst++] = (nlen >> 8) & 0xff;
    out.set(raw.subarray(src, src + len), dst);
    dst += len;
    src += len;
  }
  const adler = adler32(raw);
  const view = new DataView(out.buffer);
  view.setUint32(dst, adler, false);
  return out;
}

function parseHexRgba(hex: string): [number, number, number, number] {
  const clean = hex.replace(/^#/, "");
  if (clean.length === 6) {
    return [
      Number.parseInt(clean.slice(0, 2), 16) || 0,
      Number.parseInt(clean.slice(2, 4), 16) || 0,
      Number.parseInt(clean.slice(4, 6), 16) || 0,
      255,
    ];
  }
  return [239, 68, 68, 255];
}

function drawDisk(
  rgba: Uint8Array,
  w: number,
  h: number,
  cx: number,
  cy: number,
  radius: number,
  color: [number, number, number, number]
): void {
  const r2 = radius * radius;
  const x0 = Math.max(0, Math.floor(cx - radius));
  const x1 = Math.min(w - 1, Math.ceil(cx + radius));
  const y0 = Math.max(0, Math.floor(cy - radius));
  const y1 = Math.min(h - 1, Math.ceil(cy + radius));
  for (let y = y0; y <= y1; y += 1) {
    for (let x = x0; x <= x1; x += 1) {
      const dx = x - cx;
      const dy = y - cy;
      if (dx * dx + dy * dy <= r2) {
        const idx = (y * w + x) * 4;
        rgba[idx] = color[0];
        rgba[idx + 1] = color[1];
        rgba[idx + 2] = color[2];
        rgba[idx + 3] = color[3];
      }
    }
  }
}

function drawSegment(
  rgba: Uint8Array,
  w: number,
  h: number,
  x0: number,
  y0: number,
  x1: number,
  y1: number,
  radius: number,
  color: [number, number, number, number]
): void {
  const steps = Math.max(1, Math.ceil(Math.hypot(x1 - x0, y1 - y0)));
  for (let i = 0; i <= steps; i += 1) {
    const t = i / steps;
    drawDisk(rgba, w, h, x0 + (x1 - x0) * t, y0 + (y1 - y0) * t, radius, color);
  }
}

/**
 * Renders finger markup annotations onto a clean rasterized PNG buffer
 * (`image/png`, verified magic header `89 50 4E 47 0D 0A 1A 0A`).
 */
export function renderAnnotationsToPng(
  annotations: MarkupAnnotation[],
  options?: { width?: number; height?: number; darkTheme?: boolean }
): Uint8Array {
  const width = options?.width ?? 390;
  const height = options?.height ?? 520;
  const bg: [number, number, number, number] = options?.darkTheme === false
    ? [248, 250, 252, 255]
    : [15, 21, 30, 255];
  const grid: [number, number, number, number] = options?.darkTheme === false
    ? [226, 232, 240, 255]
    : [30, 41, 56, 255];

  const rgba = new Uint8Array(width * height * 4);
  for (let y = 0; y < height; y += 1) {
    const isGridRow = y % 40 === 0;
    for (let x = 0; x < width; x += 1) {
      const idx = (y * width + x) * 4;
      const c = isGridRow || x % 40 === 0 ? grid : bg;
      rgba[idx] = c[0];
      rgba[idx + 1] = c[1];
      rgba[idx + 2] = c[2];
      rgba[idx + 3] = c[3];
    }
  }

  for (const ann of annotations) {
    const color = parseHexRgba(ann.color);
    if (ann.tool === "pen" && ann.points.length > 0) {
      for (let i = 1; i < ann.points.length; i += 1) {
        const a = ann.points[i - 1]!;
        const b = ann.points[i]!;
        drawSegment(
          rgba,
          width,
          height,
          a.x * width,
          a.y * height,
          b.x * width,
          b.y * height,
          2.5,
          color
        );
      }
      if (ann.points.length === 1) {
        const p = ann.points[0]!;
        drawDisk(rgba, width, height, p.x * width, p.y * height, 3, color);
      }
    } else if (ann.tool === "rect") {
      const x0 = ann.start.x * width;
      const y0 = ann.start.y * height;
      const x1 = ann.end.x * width;
      const y1 = ann.end.y * height;
      drawSegment(rgba, width, height, x0, y0, x1, y0, 2.5, color);
      drawSegment(rgba, width, height, x1, y0, x1, y1, 2.5, color);
      drawSegment(rgba, width, height, x1, y1, x0, y1, 2.5, color);
      drawSegment(rgba, width, height, x0, y1, x0, y0, 2.5, color);
    } else if (ann.tool === "arrow") {
      const x0 = ann.start.x * width;
      const y0 = ann.start.y * height;
      const x1 = ann.end.x * width;
      const y1 = ann.end.y * height;
      drawSegment(rgba, width, height, x0, y0, x1, y1, 2.5, color);
      const angle = Math.atan2(y1 - y0, x1 - x0);
      const head = 14;
      drawSegment(
        rgba,
        width,
        height,
        x1,
        y1,
        x1 - head * Math.cos(angle - Math.PI / 6),
        y1 - head * Math.sin(angle - Math.PI / 6),
        2.5,
        color
      );
      drawSegment(
        rgba,
        width,
        height,
        x1,
        y1,
        x1 - head * Math.cos(angle + Math.PI / 6),
        y1 - head * Math.sin(angle + Math.PI / 6),
        2.5,
        color
      );
    } else if (ann.tool === "pin") {
      const cx = ann.point.x * width;
      const cy = ann.point.y * height;
      drawDisk(rgba, width, height, cx, cy, 12, color);
      drawDisk(rgba, width, height, cx, cy, 5, [255, 255, 255, 255]);
    }
  }

  // Build scanlines with filter byte 0 per row.
  const stride = width * 4 + 1;
  const raw = new Uint8Array(stride * height);
  for (let y = 0; y < height; y += 1) {
    const rowOff = y * stride;
    raw[rowOff] = 0;
    raw.set(rgba.subarray(y * width * 4, (y + 1) * width * 4), rowOff + 1);
  }

  const ihdr = new Uint8Array(13);
  const ihdrView = new DataView(ihdr.buffer);
  ihdrView.setUint32(0, width, false);
  ihdrView.setUint32(4, height, false);
  ihdr[8] = 8; // bit depth
  ihdr[9] = 6; // RGBA
  ihdr[10] = 0;
  ihdr[11] = 0;
  ihdr[12] = 0;

  const sig = new Uint8Array([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);
  const c1 = pngChunk("IHDR", ihdr);
  const c2 = pngChunk("IDAT", zlibStore(raw));
  const c3 = pngChunk("IEND", new Uint8Array(0));

  const png = new Uint8Array(sig.length + c1.length + c2.length + c3.length);
  let pos = 0;
  png.set(sig, pos);
  pos += sig.length;
  png.set(c1, pos);
  pos += c1.length;
  png.set(c2, pos);
  pos += c2.length;
  png.set(c3, pos);
  return png;
}
