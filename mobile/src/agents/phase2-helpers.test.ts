import { describe, expect, it } from "vitest";
import { en } from "../i18n/en";
import { ko } from "../i18n/ko";
import type { AgentRow, LivePulsePayload } from "../protocol/schemas";
import {
  DEFAULT_LIVE_PULSE_PREFERENCES,
  appendDiagnosticRingBuffer,
  buildLocalLivePulse,
  buildWebViewDiagnosticsBootstrapScript,
  chunkBinaryAsset,
  formatFocusRemaining,
  parseDiagnosticMessage,
  projectLivePulseForDevice,
  renderAnnotationsToPng,
  validateManualPortInput,
} from "./phase2-helpers";

describe("Phase 2 pure helpers", () => {
  it("validates manual localhost port inputs against denied ports", () => {
    const denied = [22, 5432, 6379, 7767, 27017];
    expect(validateManualPortInput("5173", denied)).toEqual({
      ok: true,
      port: 5173,
    });
    expect(validateManualPortInput(":3000", denied)).toEqual({
      ok: true,
      port: 3000,
    });
    expect(validateManualPortInput("22", denied)).toEqual({
      ok: false,
      reason: "denied",
    });
    expect(validateManualPortInput("7767", denied)).toEqual({
      ok: false,
      reason: "denied",
    });
    expect(validateManualPortInput("0", denied)).toEqual({
      ok: false,
      reason: "invalid",
    });
    expect(validateManualPortInput("70000", denied)).toEqual({
      ok: false,
      reason: "invalid",
    });
    expect(validateManualPortInput("example.com:80", denied)).toEqual({
      ok: false,
      reason: "invalid",
    });
  });

  it("captures and bounds WebView console/network diagnostics to 20 entries", () => {
    const bootstrap = buildWebViewDiagnosticsBootstrapScript();
    expect(bootstrap).toContain("console.error");
    expect(bootstrap).toContain("unhandledrejection");
    expect(bootstrap).toContain("XMLHttpRequest");

    const parsed = parseDiagnosticMessage(
      JSON.stringify({
        __masil_diag: true,
        kind: "network",
        message: "GET /api/metrics -> 500",
        ts: 123,
      })
    );
    expect(parsed).toEqual({
      kind: "network",
      message: "GET /api/metrics -> 500",
      ts: 123,
    });
    expect(parseDiagnosticMessage("not json")).toBeNull();
    expect(parseDiagnosticMessage(JSON.stringify({ hello: 1 }))).toBeNull();

    let buf: Array<{ kind: "console" | "network"; message: string; ts: number }> = [];
    for (let i = 0; i < 25; i += 1) {
      buf = appendDiagnosticRingBuffer(
        buf,
        { kind: "console", message: `err-${i}`, ts: i },
        20
      );
    }
    expect(buf).toHaveLength(20);
    expect(buf[0]?.message).toBe("err-5");
    expect(buf[19]?.message).toBe("err-24");
  });

  it("renders finger markup annotations into a valid PNG and chunks cleanly", () => {
    const png = renderAnnotationsToPng(
      [
        {
          id: "1",
          tool: "pen",
          color: "#ef4444",
          points: [
            { x: 0.1, y: 0.1 },
            { x: 0.4, y: 0.3 },
          ],
        },
        {
          id: "2",
          tool: "rect",
          color: "#ef4444",
          start: { x: 0.2, y: 0.2 },
          end: { x: 0.8, y: 0.6 },
        },
        {
          id: "3",
          tool: "arrow",
          color: "#ef4444",
          start: { x: 0.1, y: 0.8 },
          end: { x: 0.5, y: 0.5 },
        },
        {
          id: "4",
          tool: "pin",
          color: "#ef4444",
          point: { x: 0.5, y: 0.5 },
          number: 1,
        },
      ],
      { width: 64, height: 64, darkTheme: true }
    );

    // Valid PNG magic header + IHDR + IEND trailer
    expect(Array.from(png.subarray(0, 8))).toEqual([
      0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a,
    ]);
    expect(new TextDecoder().decode(png.subarray(12, 16))).toBe("IHDR");
    expect(
      new TextDecoder().decode(png.subarray(png.length - 8, png.length - 4))
    ).toBe("IEND");

    const chunks = chunkBinaryAsset(png, 4096);
    const total = chunks.reduce((acc, c) => acc + c.length, 0);
    expect(total).toBe(png.length);
  });

  it("enforces D-09 strict opt-in defaults for Live Activity and summary line", () => {
    expect(DEFAULT_LIVE_PULSE_PREFERENCES).toEqual({
      enabled: false,
      summaryEnabled: false,
    });

    const basePulse: LivePulsePayload = {
      host_id: "mac-1",
      host_name: "Mac Studio",
      working_count: 2,
      blocked_count: 1,
      done_count: 3,
      alert_count: 1,
      top_agent_alias: "builder-agent-with-very-long-name-beyond-32-chars",
      top_event_kind: "approval_requested",
      top_summary_line: "Run cargo test on workspace",
      deep_link: "masil://host/mac-1/agent/%251",
      updated_at: 1000,
    };

    // Opt-in disabled -> nothing emitted
    expect(
      projectLivePulseForDevice(basePulse, {
        enabled: false,
        summaryEnabled: true,
      })
    ).toBeNull();

    // Live Pulse enabled, summary disabled (default) -> summary stripped!
    const minimal = projectLivePulseForDevice(basePulse, {
      enabled: true,
      summaryEnabled: false,
    });
    expect(minimal?.top_summary_line).toBeNull();
    expect(minimal?.top_agent_alias?.length).toBeLessThanOrEqual(32);

    // Both opt-ins enabled -> summary retained (bounded to <= 60 chars)
    const full = projectLivePulseForDevice(basePulse, {
      enabled: true,
      summaryEnabled: true,
    });
    expect(full?.top_summary_line).toBe("Run cargo test on workspace");

    const sampleAgents: AgentRow[] = [
      {
        remote_id: "d::%1",
        server_name: "d",
        server_socket: "/tmp/s",
        pane: "%1",
        name: "builder",
        provider: "claude",
        state: "blocked",
        cwd: "/tmp",
        permission_count: 1,
      },
      {
        remote_id: "d::%2",
        server_name: "d",
        server_socket: "/tmp/s",
        pane: "%2",
        name: "worker",
        provider: "codex",
        state: "working",
        cwd: "/tmp",
      },
    ];
    expect(
      buildLocalLivePulse("mac-1", "Mac", sampleAgents, {
        enabled: false,
        summaryEnabled: false,
      })
    ).toBeNull();
    const localPulse = buildLocalLivePulse("mac-1", "Mac", sampleAgents, {
      enabled: true,
      summaryEnabled: false,
    });
    expect(localPulse?.working_count).toBe(1);
    expect(localPulse?.blocked_count).toBe(1);
    expect(localPulse?.top_agent_alias).toBe("builder");
    expect(localPulse?.top_event_kind).toBe("approval_requested");
  });

  it("formats Focus Shield remaining durations in EN and KO and keeps i18n keys symmetric", () => {
    expect(formatFocusRemaining(25 * 60_000, false)).toBe("25m");
    expect(formatFocusRemaining(25 * 60_000, true)).toBe("25분");
    expect(formatFocusRemaining(95 * 60_000, false)).toBe("1h 35m");
    expect(formatFocusRemaining(95 * 60_000, true)).toBe("1시간 35분");

    expect(Object.keys(en).sort()).toEqual(Object.keys(ko).sort());
  });
});
