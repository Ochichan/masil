/**
 * Real cross-language full-stack E2EE test executed by `tests/test_remote_e2e.py`.
 * Connects `MasilRemoteClient` (TypeScript) through live `masil-relay` (Rust)
 * and direct WebSocket to live `masil-agent remote daemon` (Rust) backed by a
 * real `masil` server (C).
 *
 * Skipped automatically when `MASIL_E2E_PAIR_URI` is not set so standalone
 * `npm test` stays fast and self-contained.
 */
import { execFileSync } from "node:child_process";
import { describe, expect, it } from "vitest";
import { parsePairingUri } from "../crypto/pairing";
import { BinaryOpcode, decodeTerminalSnapshot } from "./binary-frames";
import { MasilRemoteClient } from "./client";

const pairUri = process.env.MASIL_E2E_PAIR_URI;
const masilBin = process.env.MASIL_E2E_MASIL_BIN ?? "";
const agentBin = process.env.MASIL_E2E_AGENT_BIN ?? "";
const serverSocket = process.env.MASIL_E2E_SOCKET ?? "";
const paneId = process.env.MASIL_E2E_PANE ?? "%0";
const repoDir = process.env.MASIL_E2E_REPO ?? "";
const claudeCwd = process.env.MASIL_E2E_CLAUDE_CWD ?? repoDir;
const codexCwd = process.env.MASIL_E2E_CODEX_CWD ?? repoDir;
const agyCwd = process.env.MASIL_E2E_AGY_CWD ?? repoDir;

function delay(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}

describe.skipIf(!pairUri)("live cross-language E2EE remote stack", () => {
  it(
    "pairs over relay, preserves desktop pane size over -C, reads files/diff/transcripts/STT, and enforces device revocation",
    async () => {
      const offer = parsePairingUri(pairUri!);
      expect(offer.relayUrl).toBeTruthy();
      expect(offer.directAddr).toBeTruthy();

      // 1. Pair strictly through `masil-relay` (no directAddr).
      const pairClient = new MasilRemoteClient({
        hostId: offer.hostId,
        hostPub: offer.hostPub,
        relayUrl: offer.relayUrl,
        pairToken: offer.pairToken,
        deviceName: "Vitest E2E Phone",
        platform: "ios",
      });
      const pairResp = await pairClient.connect();
      const deviceId = String(pairResp.device_id ?? "");
      const deviceSecret = String(pairResp.device_secret ?? "");
      expect(deviceId.length).toBeGreaterThan(8);
      expect(deviceSecret.length).toBeGreaterThan(16);
      pairClient.disconnect();

      // 2. Verify direct LAN WebSocket also authenticates with the same Noise NK + device credentials.
      const directClient = new MasilRemoteClient({
        hostId: offer.hostId,
        hostPub: offer.hostPub,
        directAddr: offer.directAddr,
        deviceId,
        deviceSecret,
      });
      const directHello = await directClient.connect();
      expect(directHello.authenticated).toBe(true);
      const hostInfo = await directClient.request<{
        host_id: string;
        servers: Array<{ socket: string }>;
        capabilities: { desktop_size_preserved: boolean; voice_host_stt: boolean };
      }>("host.info.request");
      expect(hostInfo.host_id).toBe(offer.hostId);
      expect(hostInfo.capabilities.desktop_size_preserved).toBe(true);
      expect(hostInfo.capabilities.voice_host_stt).toBe(true);
      expect(hostInfo.servers.some((s) => s.socket === serverSocket)).toBe(true);
      directClient.disconnect();

      // 3. Reconnect through `masil-relay` for the main operational suite.
      const receivedSnapshots: ReturnType<typeof decodeTerminalSnapshot>[] = [];
      let receivedTerminalText = "";
      const voiceEvents: Array<Record<string, unknown>> = [];
      let disconnectedAfterRevoke = false;

      const relayClient = new MasilRemoteClient({
        hostId: offer.hostId,
        hostPub: offer.hostPub,
        relayUrl: offer.relayUrl,
        deviceId,
        deviceSecret,
        onBinaryFrame: (frame) => {
          if (frame.opcode === BinaryOpcode.TerminalSnapshot) {
            receivedSnapshots.push(decodeTerminalSnapshot(frame.payload));
          } else if (frame.opcode === BinaryOpcode.TerminalOutput) {
            receivedTerminalText += new TextDecoder().decode(frame.payload);
          }
        },
        onEvent: (ev) => {
          if (ev.event === "voice.transcription") {
            voiceEvents.push(ev);
          }
        },
        onStateChange: (state) => {
          if (state === "disconnected") {
            disconnectedAfterRevoke = true;
          }
        },
      });
      const relayHello = await relayClient.connect();
      expect(relayHello.authenticated).toBe(true);

      // 4. Workspace file listing, preview, and Git diff.
      const files = await relayClient.request<{
        entries: Array<{ name: string }>;
      }>("workspace.files.list.request", { cwd: repoDir, path: "" });
      expect(files.entries.some((e) => e.name === "hello.txt")).toBe(true);

      const preview = await relayClient.request<{ content: string }>(
        "workspace.files.read.request",
        { cwd: repoDir, path: "hello.txt" }
      );
      expect(preview.content).toContain("modified line for diff");

      const changes = await relayClient.request<{
        files: Array<{ path: string }>;
      }>("workspace.changes.list.request", { cwd: repoDir });
      expect(changes.files.some((f) => f.path === "hello.txt")).toBe(true);

      const diff = await relayClient.request<{ diff: string }>(
        "workspace.changes.diff.request",
        { cwd: repoDir, path: "hello.txt" }
      );
      expect(diff.diff).toContain("+modified line for diff");

      // 4b. Host GUI directory browser & mkdir RPCs.
      const browse = await relayClient.request<{
        current: string;
        current_is_git_repo: boolean;
        breadcrumbs: Array<{ label: string; path: string }>;
        quick: Array<{ path: string }>;
      }>("host.dirs.list.request", { path: repoDir, include_quick: true });
      expect(browse.current).toBe(repoDir);
      expect(browse.current_is_git_repo).toBe(true);
      expect(browse.breadcrumbs.length).toBeGreaterThan(0);

      const createdDir = await relayClient.request<{
        name: string;
        path: string;
      }>("host.dirs.mkdir.request", { parent: repoDir, name: "gui-sub" });
      expect(createdDir.name).toBe("gui-sub");
      const browseAfter = await relayClient.request<{
        entries: Array<{ name: string }>;
      }>("host.dirs.list.request", { path: repoDir, include_quick: false });
      expect(browseAfter.entries.some((e) => e.name === "gui-sub")).toBe(true);

      // 5. Synthetic 4-provider transcript streaming & importable session discovery.
      interface WireTimelinePage {
        binding_confidence: string;
        items: Array<{
          kind: string;
          text?: string;
          markdown?: string;
          name?: string;
        }>;
      }

      const claudeTimeline = await relayClient.request<WireTimelinePage>(
        "agent.timeline.fetch.request",
        {
          server_socket: serverSocket,
          pane: paneId,
          provider: "claude",
          cwd: claudeCwd,
          state: "idle",
        }
      );
      expect(claudeTimeline.binding_confidence).toBe("unverified_heuristic");
      expect(
        claudeTimeline.items.some((i) => i.text?.includes("한글 질문"))
      ).toBe(true);
      expect(
        claudeTimeline.items.some((i) =>
          i.markdown?.includes("Claude E2E reply")
        )
      ).toBe(true);
      expect(
        claudeTimeline.items.some(
          (i) => i.kind === "tool_call" && i.name === "Bash"
        )
      ).toBe(true);

      const codexTimeline = await relayClient.request<WireTimelinePage>(
        "agent.timeline.fetch.request",
        {
          server_socket: serverSocket,
          pane: paneId,
          provider: "codex",
          cwd: codexCwd,
          state: "idle",
        }
      );
      expect(
        codexTimeline.items.some((i) =>
          i.markdown?.includes("Codex E2E answer")
        )
      ).toBe(true);

      const agyTimeline = await relayClient.request<WireTimelinePage>(
        "agent.timeline.fetch.request",
        {
          server_socket: serverSocket,
          pane: paneId,
          provider: "agy",
          cwd: agyCwd,
          state: "idle",
        }
      );
      expect(
        agyTimeline.items.some((i) =>
          i.markdown?.includes("Antigravity E2E plan")
        )
      ).toBe(true);
      expect(agyTimeline.items.some((i) => i.kind === "thinking")).toBe(true);

      const opencodeTimeline = await relayClient.request<WireTimelinePage>(
        "agent.timeline.fetch.request",
        {
          server_socket: serverSocket,
          pane: paneId,
          provider: "opencode",
          cwd: repoDir,
          state: "idle",
        }
      );
      expect(
        opencodeTimeline.items.some((i) =>
          i.markdown?.includes("OpenCode E2E reply")
        )
      ).toBe(true);

      const importable = await relayClient.request<{
        sessions: Array<{ provider: string; session_id: string }>;
      }>("agent.importable.list.request", { limit: 20 });
      expect(importable.sessions.some((s) => s.provider === "claude")).toBe(true);
      expect(importable.sessions.some((s) => s.provider === "codex")).toBe(true);
      expect(importable.sessions.some((s) => s.provider === "agy")).toBe(true);
      expect(importable.sessions.some((s) => s.provider === "opencode")).toBe(true);

      // 6. Live `-C` terminal attach, I/O, and desktop pane size preservation.
      const geomBeforeAttach = execFileSync(
        masilBin,
        ["-S", serverSocket, "display-message", "-p", "-t", paneId, "#{pane_width}x#{pane_height}"],
        { encoding: "utf8" }
      ).trim();
      const [expectedColsStr, expectedRowsStr] = geomBeforeAttach.split("x");
      const expectedCols = Number(expectedColsStr);
      const expectedRows = Number(expectedRowsStr);
      expect(expectedCols).toBe(160);
      expect(expectedRows).toBeGreaterThanOrEqual(40);

      const attachMeta = await relayClient.request<{
        cols: number;
        rows: number;
        desktop_cols: number;
        desktop_rows: number;
        fit_active: boolean;
      }>("terminal.attach.request", {
        channel_id: 1,
        server_socket: serverSocket,
        pane: paneId,
      });
      expect(attachMeta.desktop_cols).toBe(expectedCols);
      expect(attachMeta.desktop_rows).toBe(expectedRows);
      expect(attachMeta.fit_active).toBe(false);

      // Wait for snapshot binary frame.
      for (let i = 0; i < 40 && receivedSnapshots.length === 0; i++) {
        await delay(50);
      }
      expect(receivedSnapshots.length).toBeGreaterThan(0);
      expect(receivedSnapshots[0]!.meta.cols).toBe(expectedCols);
      expect(receivedSnapshots[0]!.meta.rows).toBe(expectedRows);

      // Send live shell input over encrypted binary frame and verify %output stream.
      relayClient.sendTerminalInput(
        1,
        new TextEncoder().encode("echo MASIL_LIVE_TERM_OK\n")
      );
      for (
        let i = 0;
        i < 60 && !receivedTerminalText.includes("MASIL_LIVE_TERM_OK");
        i++
      ) {
        await delay(50);
      }
      expect(receivedTerminalText).toContain("MASIL_LIVE_TERM_OK");

      // Assert desktop pane was NEVER resized on mobile attach.
      const geomAfterAttach = execFileSync(
        masilBin,
        ["-S", serverSocket, "display-message", "-p", "-t", paneId, "#{pane_width}x#{pane_height}"],
        { encoding: "utf8" }
      ).trim();
      expect(geomAfterAttach).toBe(geomBeforeAttach);

      // Explicit opt-in Fit toggle followed by restoring Desktop mode.
      relayClient.sendTerminalResize(1, 52, 24, "fit");
      await delay(150);
      relayClient.sendTerminalResize(1, expectedCols, expectedRows, "desktop");
      await delay(150);
      await relayClient.request("terminal.detach.request", { channel_id: 1 });

      // 7. E2EE Voice audio upload -> host STT command -> encrypted transcription event.
      relayClient.sendBinaryFrame({
        opcode: BinaryOpcode.AudioChunk,
        channelId: 9,
        payload: new TextEncoder().encode("FAKE_WAV_AUDIO_BYTES"),
      });
      relayClient.sendBinaryFrame({
        opcode: BinaryOpcode.AudioEnd,
        channelId: 9,
        payload: new TextEncoder().encode(
          JSON.stringify({ mime: "audio/wav", mode: "composer" })
        ),
      });
      for (let i = 0; i < 60 && voiceEvents.length === 0; i++) {
        await delay(50);
      }
      expect(voiceEvents.length).toBeGreaterThan(0);
      expect(voiceEvents[0]!.ok).toBe(true);
      expect(voiceEvents[0]!.text).toBe("모바일 음성 테스트 완료");

      // 8. Push token registration.
      const pushReg = await relayClient.request<{ registered: boolean }>(
        "host.push.register.request",
        { platform: "ios", token: "apns-test-token-123" }
      );
      expect(pushReg.registered).toBe(true);

      // 9. Host revokes device -> active session closes AND reconnect fails.
      disconnectedAfterRevoke = false;
      const revokeOut = execFileSync(agentBin, ["remote", "revoke", deviceId], {
        encoding: "utf8",
      });
      expect(JSON.parse(revokeOut).revoked).toBe(true);

      for (let i = 0; i < 60 && !disconnectedAfterRevoke; i++) {
        await delay(100);
      }
      expect(disconnectedAfterRevoke).toBe(true);
      relayClient.disconnect();

      const revokedClient = new MasilRemoteClient({
        hostId: offer.hostId,
        hostPub: offer.hostPub,
        relayUrl: offer.relayUrl,
        deviceId,
        deviceSecret,
      });
      await expect(revokedClient.connect()).rejects.toThrow(/unauthorized/i);
      revokedClient.disconnect();
    },
    30_000
  );
});
