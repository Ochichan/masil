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
import { appendFileSync, existsSync, readFileSync, writeFileSync } from "node:fs";
import http from "node:http";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { renderAnnotationsToPng } from "../agents/phase2-helpers";
import { parsePairingUri } from "../crypto/pairing";
import {
  type BinaryFrame,
  BinaryOpcode,
  decodeJsonPayload,
  decodeTerminalSnapshot,
  encodeJsonPayload,
  type TunnelOpenAckPayload,
} from "./binary-frames";
import { MasilRemoteClient } from "./client";
import type { InteractivePrompt } from "./schemas";

const pairUri = process.env.MASIL_E2E_PAIR_URI;
const masilBin = process.env.MASIL_E2E_MASIL_BIN ?? "";
const agentBin = process.env.MASIL_E2E_AGENT_BIN ?? "";
const serverSocket = process.env.MASIL_E2E_SOCKET ?? "";
const paneId = process.env.MASIL_E2E_PANE ?? "%0";
const repoDir = process.env.MASIL_E2E_REPO ?? "";
const claudeCwd = process.env.MASIL_E2E_CLAUDE_CWD ?? repoDir;
const codexCwd = process.env.MASIL_E2E_CODEX_CWD ?? repoDir;
const agyCwd = process.env.MASIL_E2E_AGY_CWD ?? repoDir;
const agyPane = process.env.MASIL_E2E_AGY_PANE ?? "";
const agyName = process.env.MASIL_E2E_AGY_NAME ?? "";
const agyLog = process.env.MASIL_E2E_AGY_LOG ?? "";
const ocPane = process.env.MASIL_E2E_OC_PANE ?? "";
const ocName = process.env.MASIL_E2E_OC_NAME ?? "";
const ocStateDir = process.env.MASIL_E2E_OC_STATE ?? "";

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
        capabilities: {
          desktop_size_preserved: boolean;
          voice_host_stt: boolean;
          checkpoint_preview?: boolean;
          worktree_management?: boolean;
          timeline_streaming?: boolean;
          turn_audit?: boolean;
          collision_radar?: boolean;
          circuit_breaker?: boolean;
          cache_warmth?: boolean;
          localhost_tunnel?: boolean;
          visual_markup?: boolean;
          live_pulse_push?: boolean;
          focus_briefing?: boolean;
        };
      }>("host.info.request");
      expect(hostInfo.host_id).toBe(offer.hostId);
      expect(hostInfo.capabilities.desktop_size_preserved).toBe(true);
      expect(hostInfo.capabilities.voice_host_stt).toBe(true);
      expect(hostInfo.capabilities.checkpoint_preview).toBe(true);
      expect(hostInfo.capabilities.worktree_management).toBe(true);
      expect(hostInfo.capabilities.timeline_streaming).toBe(true);
      expect(hostInfo.capabilities.turn_audit).toBe(true);
      expect(hostInfo.capabilities.collision_radar).toBe(true);
      expect(hostInfo.capabilities.circuit_breaker).toBe(true);
      expect(hostInfo.capabilities.cache_warmth).toBe(true);
      expect(hostInfo.capabilities.localhost_tunnel).toBe(true);
      expect(hostInfo.capabilities.visual_markup).toBe(true);
      expect(hostInfo.capabilities.live_pulse_push).toBe(true);
      expect(hostInfo.capabilities.focus_briefing).toBe(true);
      expect(hostInfo.servers.some((s) => s.socket === serverSocket)).toBe(true);
      directClient.disconnect();

      // 3. Reconnect through `masil-relay` for the main operational suite.
      const receivedSnapshots: ReturnType<typeof decodeTerminalSnapshot>[] = [];
      let receivedTerminalText = "";
      const voiceEvents: Array<Record<string, unknown>> = [];
      const timelineDeltaEvents: Array<Record<string, unknown>> = [];
      const assetUploadEvents: Array<Record<string, unknown>> = [];
      const tunnelFrames: BinaryFrame[] = [];
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
          } else if (
            frame.opcode === BinaryOpcode.TunnelOpenAck ||
            frame.opcode === BinaryOpcode.TunnelData ||
            frame.opcode === BinaryOpcode.TunnelClose ||
            frame.opcode === BinaryOpcode.TunnelWindow
          ) {
            tunnelFrames.push(frame);
          }
        },
        onEvent: (ev) => {
          if (ev.event === "voice.transcription") {
            voiceEvents.push(ev);
          } else if (ev.event === "timeline.delta") {
            timelineDeltaEvents.push(ev);
          } else if (ev.event === "asset.upload.result") {
            assetUploadEvents.push(ev);
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

      // 5b. Phase 0: WebSocket timeline.delta streaming subscribe/unsubscribe + worktree list/create/remove.
      const subAck = await relayClient.request<{
        sub_id: string;
        subscribed: boolean;
        page: WireTimelinePage;
      }>("agent.timeline.subscribe.request", {
        server_socket: serverSocket,
        pane: paneId,
        provider: "claude",
        cwd: claudeCwd,
        state: "idle",
        limit: 40,
      });
      expect(subAck.subscribed).toBe(true);
      expect(subAck.sub_id).toBeTruthy();
      expect(
        subAck.page.items.some((i) => i.markdown?.includes("Claude E2E reply"))
      ).toBe(true);

      // Append a new turn to the live Claude JSONL and assert a live encrypted `timeline.delta` push arrives.
      const claudeJsonl = join(
        process.env.HOME ?? "",
        ".claude",
        "projects",
        claudeCwd.replace(/[/_]/g, "-"),
        "claude-sess-001.jsonl"
      );
      appendFileSync(
        claudeJsonl,
        `${JSON.stringify({
          type: "assistant",
          cwd: claudeCwd,
          timestamp: "2026-11-24T01:00:09Z",
          message: {
            content: [{ type: "text", text: "Claude E2E live delta" }],
          },
        })}\n`,
        "utf8"
      );
      for (let i = 0; i < 60 && timelineDeltaEvents.length === 0; i++) {
        await delay(50);
      }
      expect(timelineDeltaEvents.length).toBeGreaterThan(0);
      const firstDelta = timelineDeltaEvents[0]!;
      expect(firstDelta.sub).toBe(subAck.sub_id);
      expect(firstDelta.mode).toBe("patch");
      const deltaItems = (firstDelta.items ?? []) as Array<{ markdown?: string }>;
      expect(
        deltaItems.some((i) => i.markdown?.includes("Claude E2E live delta"))
      ).toBe(true);

      const unsubAck = await relayClient.request<{ unsubscribed: boolean }>(
        "agent.timeline.unsubscribe.request",
        { sub_id: subAck.sub_id }
      );
      expect(unsubAck.unsubscribed).toBe(true);

      const wtListBefore = await relayClient.request<{
        worktrees?: Array<{ path?: string; branch?: string }>;
      }>("worktree.list.request", { repo: repoDir, all: true });
      expect(Array.isArray(wtListBefore.worktrees)).toBe(true);

      const wtCreated = await relayClient.request<{
        path?: string;
        name?: string;
        branch?: string;
      }>("worktree.create.request", {
        repo: repoDir,
        branch: "phase0-wt-e2e",
      });
      const wtTarget = wtCreated.name ?? wtCreated.path ?? "phase0-wt-e2e";
      expect(wtCreated.path).toBeTruthy();

      const wtListMid = await relayClient.request<{
        worktrees?: Array<{ path?: string; branch?: string }>;
      }>("worktree.list.request", { repo: repoDir, all: true });
      expect(
        wtListMid.worktrees?.some(
          (w) => w.path === wtCreated.path || w.branch === "phase0-wt-e2e"
        )
      ).toBe(true);

      const wtRemoveRes = await relayClient.request<{
        needs_confirmation?: boolean;
        confirm?: string;
        token?: string;
      }>("worktree.remove.request", {
        repo: repoDir,
        worktree: wtTarget,
        delete_branch: true,
      });
      if (wtRemoveRes.needs_confirmation && (wtRemoveRes.confirm || wtRemoveRes.token)) {
        await relayClient.request("worktree.remove.request", {
          repo: repoDir,
          worktree: wtTarget,
          confirm: wtRemoveRes.confirm ?? wtRemoveRes.token,
          delete_branch: true,
        });
      }
      const wtListAfter = await relayClient.request<{
        worktrees?: Array<{ path?: string; branch?: string; state?: string }>;
      }>("worktree.list.request", { repo: repoDir });
      expect(
        wtListAfter.worktrees?.some((w) => w.path === wtCreated.path)
      ).toBe(false);
      const wtListAll = await relayClient.request<{
        worktrees?: Array<{ path?: string; branch?: string; state?: string }>;
      }>("worktree.list.request", { repo: repoDir, all: true });
      expect(
        wtListAll.worktrees?.find((w) => w.path === wtCreated.path)?.state
      ).toBe("removed");

      // 5c. Phase 1: Turn Audit, Collision Radar, and Circuit Breaker / Cache Warmth RPCs over E2EE relay.
      const colMatrix = await relayClient.request<{
        pairs?: unknown[];
        agents_checked?: number;
      }>("collision.matrix.request", {
        server_socket: serverSocket,
        repo: repoDir,
      });
      expect(Array.isArray(colMatrix.pairs)).toBe(true);

      const guardSummary = await relayClient.request<{
        breakers?: unknown[];
        cache_warmth?: unknown[];
      }>("guard.status.request", {
        server_socket: serverSocket,
      });
      expect(Array.isArray(guardSummary.breakers)).toBe(true);
      expect(Array.isArray(guardSummary.cache_warmth)).toBe(true);

      if (agyPane) {
        const auditRes = await relayClient.request<{
          audit?: { verdict?: string; risk_score?: number };
        }>("audit.latest.request", {
          server_socket: serverSocket,
          pane: agyPane,
          target: agyPane,
        });
        expect(auditRes.audit).toBeTruthy();
        expect(typeof auditRes.audit?.risk_score).toBe("number");
      }

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

      // 8b. Interactive Live TUI question + custom Korean Other input + slash command
      //     (deliberately passing `target: agyName` without `pane` to prove target->%pane_id resolution!).
      if (agyPane && agyName && agyLog) {
        const agyPage = await relayClient.request<{
          active_prompts?: InteractivePrompt[];
        }>("agent.timeline.fetch.request", {
          server_socket: serverSocket,
          pane: agyPane,
          provider: "agy",
          cwd: repoDir,
          state: "blocked",
        });
        expect(agyPage.active_prompts?.length).toBe(1);
        const q1 = agyPage.active_prompts![0]!;
        expect(q1.mode).toBe("terminal_tui");
        expect(q1.options.length).toBe(3);
        expect(q1.options[1]!.label).toContain("Production");
        expect(q1.options[2]!.is_custom_input).toBe(true);

        // Select option 1 ("Production") using ONLY agent name (`agy-e2e` != tmux session `desk`).
        const resp1 = await relayClient.request<{
          ok: boolean;
          active_prompts: InteractivePrompt[];
        }>("agent.prompt.respond.request", {
          server_socket: serverSocket,
          target: agyName,
          action: "tui_select_option",
          option_index: 1,
          submit: true,
        });
        expect(resp1.ok).toBe(true);
        expect(resp1.active_prompts.length).toBe(1);
        expect(resp1.active_prompts[0]!.kind).toBe("permission");
        expect(resp1.active_prompts[0]!.options.length).toBe(3);
        expect(resp1.active_prompts[0]!.options[2]!.is_custom_input).toBe(true);

        // Choose option 2 ("Other") and submit Korean custom text.
        const resp2 = await relayClient.request<{
          ok: boolean;
          active_prompts: InteractivePrompt[];
        }>("agent.prompt.respond.request", {
          server_socket: serverSocket,
          target: agyName,
          action: "tui_custom_text",
          option_index: 2,
          text: "한국어 커스텀 답변",
          submit: true,
        });
        expect(resp2.ok).toBe(true);
        expect(resp2.active_prompts.length).toBe(0);

        // Deliver a direct slash command using only agent name (`target: agyName`).
        const slashRes = await relayClient.request<{
          delivered: boolean;
        }>("agent.prompt.request", {
          server_socket: serverSocket,
          target: agyName,
          text: "/status",
          force_keys: true,
        });
        expect(slashRes.delivered).toBe(true);

        let agyEvents: Array<Record<string, unknown>> = [];
        for (let i = 0; i < 30; i++) {
          const lines = readFileSync(agyLog, "utf8").trim().split("\n").filter(Boolean);
          agyEvents = lines.map((l) => JSON.parse(l) as Record<string, unknown>);
          if (agyEvents.length >= 3) break;
          await delay(80);
        }
        expect(agyEvents[0]?.phase).toBe(1);
        expect(agyEvents[0]?.selected).toBe(1);
        expect(agyEvents[1]?.phase).toBe(2);
        expect(agyEvents[1]?.custom_text).toBe("한국어 커스텀 답변");
        expect(agyEvents[2]?.phase).toBe(3);
        expect(agyEvents[2]?.line).toBe("/status");

        // Queue add -> list -> remove roundtrip.
        const added = await relayClient.request<{ id?: number; item?: { id: number } }>(
          "agent.queue.add.request",
          {
            server_socket: serverSocket,
            pane: agyPane,
            target: agyPane,
            text: "queued followup task",
          }
        );
        const queuedId = added.id ?? added.item?.id;
        expect(queuedId).toBeTruthy();

        const listed = await relayClient.request<{
          items?: Array<{ id: number }>;
        }>("agent.queue.list.request", {
          server_socket: serverSocket,
          pane: agyPane,
          target: agyPane,
        });
        expect(listed.items?.some((it) => it.id === queuedId)).toBe(true);

        await relayClient.request("agent.queue.remove.request", {
          server_socket: serverSocket,
          pane: agyPane,
          target: agyPane,
          item_id: queuedId,
        });
        const listedAfter = await relayClient.request<{
          items?: Array<{ id: number }>;
        }>("agent.queue.list.request", {
          server_socket: serverSocket,
          pane: agyPane,
          target: agyPane,
        });
        expect(listedAfter.items?.some((it) => it.id === queuedId)).toBe(false);

        // Checkpoint make -> mutate file -> checkpoint restore (2-step preview+confirm under the hood).
        const cpMake = await relayClient.request<{ id?: number | string }>(
          "checkpoint.make.request",
          {
            server_socket: serverSocket,
            pane: agyPane,
            target: agyPane,
          }
        );
        const cpId = cpMake.id;
        expect(cpId).toBeTruthy();
        const helloPath = join(repoDir, "hello.txt");
        writeFileSync(helloPath, "corrupted before restore\n", "utf8");
        const cpPreview = await relayClient.request<{
          token?: string;
          diff?: string;
          files?: Array<{ path: string; diff?: string }>;
        }>("checkpoint.preview.request", {
          server_socket: serverSocket,
          pane: agyPane,
          target: agyPane,
          checkpoint_id: cpId,
        });
        expect(cpPreview.token).toBeTruthy();
        expect(cpPreview.diff ?? cpPreview.files?.[0]?.diff ?? "").toContain(
          "modified line for diff"
        );
        await relayClient.request("checkpoint.restore.request", {
          server_socket: serverSocket,
          pane: agyPane,
          target: agyPane,
          checkpoint_id: cpId,
          confirm_token: cpPreview.token,
        });
        expect(readFileSync(helloPath, "utf8")).toContain("modified line for diff");
      }

      // 8c. Structured OpenCode `--answers` API permission + multi-select question.
      if (ocPane && ocName && ocStateDir) {
        const reqList = await relayClient.request<{
          prompts: InteractivePrompt[];
        }>("agent.requests.list.request", {
          server_socket: serverSocket,
          pane: ocPane,
          target: ocPane,
          provider: "opencode",
          state: "blocked",
        });
        expect(reqList.prompts.length).toBe(2);
        const perm = reqList.prompts.find((p) => p.request_id === "per_e2e_1");
        const ques = reqList.prompts.find((p) => p.request_id === "que_e2e_1");
        expect(perm?.mode).toBe("structured_api");
        expect(perm?.always_patterns).toEqual(["git *"]);
        expect(ques?.mode).toBe("structured_api");
        expect(ques?.questions?.[0]?.multiple).toBe(true);

        const permResp = await relayClient.request<{
          ok: boolean;
          active_prompts: InteractivePrompt[];
        }>("agent.prompt.respond.request", {
          server_socket: serverSocket,
          pane: ocPane,
          target: ocPane,
          provider: "opencode",
          action: "api_permission",
          request_id: "per_e2e_1",
          decision: "once",
        });
        expect(permResp.ok).toBe(true);
        expect(permResp.active_prompts.map((p) => p.request_id)).toEqual(["que_e2e_1"]);

        const qResp = await relayClient.request<{
          ok: boolean;
          active_prompts: InteractivePrompt[];
        }>("agent.prompt.respond.request", {
          server_socket: serverSocket,
          pane: ocPane,
          target: ocPane,
          provider: "opencode",
          action: "api_question",
          request_id: "que_e2e_1",
          multi_answers: [["iOS", "Web"]],
        });
        expect(qResp.ok).toBe(true);
        expect(qResp.active_prompts).toEqual([]);

        const posts = readFileSync(join(ocStateDir, "posts.log"), "utf8")
          .trim()
          .split("\n")
          .filter(Boolean)
          .map((l) => JSON.parse(l) as { path: string; body: unknown });
        expect(posts).toEqual([
          { path: "/permission/per_e2e_1/reply", body: { reply: "once" } },
          { path: "/question/que_e2e_1/reply", body: { answers: [["iOS", "Web"]] } },
        ]);
      }

      // 8d. Phase 2A: Live E2EE loopback HTTP tunnel through relay + denied port check.
      const mockDevServer = http.createServer((req, res) => {
        res.writeHead(200, { "Content-Type": "text/plain", Connection: "close" });
        res.end(`vite-e2e-ok:${req.url ?? "/"}`);
      });
      await new Promise<void>((resolve) =>
        mockDevServer.listen(0, "127.0.0.1", () => resolve())
      );
      const devAddr = mockDevServer.address();
      const devPort =
        typeof devAddr === "object" && devAddr ? devAddr.port : 0;
      expect(devPort).toBeGreaterThan(0);

      try {
        // Port discovery lists denied_ports and succeeds cleanly.
        const portDiscovery = await relayClient.request<{
          recommended: Array<{ port: number }>;
          host_ports: Array<{ port: number }>;
          denied_ports: number[];
        }>("tunnel.ports.list.request", {
          server_socket: serverSocket,
          pane: paneId,
        });
        expect(portDiscovery.denied_ports).toContain(22);
        expect(portDiscovery.denied_ports).toContain(7767);

        // Denied port (22) must be rejected with port_denied.
        tunnelFrames.length = 0;
        relayClient.sendBinaryFrame({
          opcode: BinaryOpcode.TunnelOpen,
          channelId: 901,
          payload: encodeJsonPayload({ target_port: 22 }),
        });
        for (let i = 0; i < 40; i++) {
          if (
            tunnelFrames.some(
              (f) =>
                f.channelId === 901 && f.opcode === BinaryOpcode.TunnelOpenAck
            )
          ) {
            break;
          }
          await delay(50);
        }
        const deniedAckFrame = tunnelFrames.find(
          (f) => f.channelId === 901 && f.opcode === BinaryOpcode.TunnelOpenAck
        );
        expect(deniedAckFrame).toBeTruthy();
        const deniedAck = decodeJsonPayload<TunnelOpenAckPayload>(
          deniedAckFrame!.payload
        );
        expect(deniedAck.ok).toBe(false);
        expect(deniedAck.error).toMatch(/port_denied/);

        // Open real loopback HTTP tunnel stream through Noise NK + masil-relay!
        tunnelFrames.length = 0;
        relayClient.sendBinaryFrame({
          opcode: BinaryOpcode.TunnelOpen,
          channelId: 902,
          payload: encodeJsonPayload({ target_port: devPort }),
        });
        for (let i = 0; i < 40; i++) {
          if (
            tunnelFrames.some(
              (f) =>
                f.channelId === 902 && f.opcode === BinaryOpcode.TunnelOpenAck
            )
          ) {
            break;
          }
          await delay(50);
        }
        const openAckFrame = tunnelFrames.find(
          (f) => f.channelId === 902 && f.opcode === BinaryOpcode.TunnelOpenAck
        );
        expect(openAckFrame).toBeTruthy();
        const openAck = decodeJsonPayload<TunnelOpenAckPayload>(
          openAckFrame!.payload
        );
        expect(openAck.ok).toBe(true);
        expect(openAck.initial_window).toBe(262_144);

        relayClient.sendBinaryFrame({
          opcode: BinaryOpcode.TunnelData,
          channelId: 902,
          payload: new TextEncoder().encode(
            `GET /dashboard HTTP/1.1\r\nHost: localhost:${devPort}\r\nConnection: close\r\n\r\n`
          ),
        });

        let httpResponseText = "";
        for (let i = 0; i < 60; i++) {
          httpResponseText = tunnelFrames
            .filter(
              (f) => f.channelId === 902 && f.opcode === BinaryOpcode.TunnelData
            )
            .map((f) => new TextDecoder().decode(f.payload))
            .join("");
          if (httpResponseText.includes("vite-e2e-ok:/dashboard")) {
            break;
          }
          await delay(50);
        }
        expect(httpResponseText).toContain("HTTP/1.1 200 OK");
        expect(httpResponseText).toContain("vite-e2e-ok:/dashboard");
      } finally {
        mockDevServer.close();
      }

      // 8e. Phase 2A: Finger-markup PNG upload (0x30/0x31) -> .masil-tmp + clean git status.
      const markupPng = renderAnnotationsToPng(
        [
          {
            id: "e2e-1",
            tool: "rect",
            color: "#ef4444",
            start: { x: 0.2, y: 0.2 },
            end: { x: 0.7, y: 0.6 },
          },
        ],
        { width: 64, height: 64, darkTheme: true }
      );
      assetUploadEvents.length = 0;
      relayClient.sendBinaryFrame({
        opcode: BinaryOpcode.AssetUploadChunk,
        channelId: 77,
        payload: markupPng,
      });
      relayClient.sendBinaryFrame({
        opcode: BinaryOpcode.AssetUploadEnd,
        channelId: 77,
        payload: encodeJsonPayload({
          agent_id: paneId,
          mime: "image/png",
          filename: "e2e-markup.png",
          delivery_mode: "prefill",
          prompt: "Increase contrast in red box",
          meta: {
            url: `http://localhost:${devPort}/dashboard`,
            viewport: "390x844",
            theme: "dark",
            server_socket: serverSocket,
            cwd: repoDir,
            console_errors: ["TypeError: undefined"],
            network_errors: ["GET /api/metrics -> 500"],
            annotations_count: 1,
          },
        }),
      });
      for (let i = 0; i < 60 && assetUploadEvents.length === 0; i++) {
        await delay(50);
      }
      expect(assetUploadEvents.length).toBeGreaterThanOrEqual(1);
      const uploadRes = assetUploadEvents[0]!;
      expect(uploadRes.ok).toBe(true);
      expect(uploadRes.in_git_worktree).toBe(true);
      expect(String(uploadRes.prompt_path)).toMatch(/^\.masil-tmp\/markup-.*\.png$/);
      expect(existsSync(String(uploadRes.saved_path))).toBe(true);
      expect(String(uploadRes.structured_prompt)).toContain(
        "[masil Mobile Visual Feedback]"
      );
      expect(String(uploadRes.structured_prompt)).toContain(
        "GET /api/metrics -> 500"
      );
      const excludeText = readFileSync(
        join(repoDir, ".git/info/exclude"),
        "utf8"
      );
      expect(excludeText).toContain(".masil-tmp/");
      const gitStatusAfterMarkup = execFileSync(
        "git",
        ["status", "--porcelain"],
        { cwd: repoDir, encoding: "utf8" }
      );
      expect(gitStatusAfterMarkup).not.toContain(".masil-tmp");

      // 8f. Phase 2B: Focus Shield, 90s Briefing, and D-09 Opt-In Live Pulse RPCs.
      const focusOn = await relayClient.request<{
        focus: { active: boolean; remaining_ms: number };
      }>("focus.set.request", {
        server_socket: serverSocket,
        active: true,
        duration: "25m",
        reason: "e2e-deep-work",
      });
      expect(focusOn.focus.active).toBe(true);
      expect(focusOn.focus.remaining_ms).toBeGreaterThan(20 * 60_000);

      const briefRes = await relayClient.request<{
        window_label: string;
        ui: { v: number; blocks: unknown[] };
        focus: { active: boolean };
      }>("briefing.generate.request", {
        server_socket: serverSocket,
        since: "8h",
      });
      expect(briefRes.window_label).toBe("8h");
      expect(briefRes.ui.v).toBe(1);
      expect(briefRes.focus.active).toBe(true);

      const focusOff = await relayClient.request<{
        focus: { active: boolean };
      }>("focus.set.request", {
        server_socket: serverSocket,
        active: false,
      });
      expect(focusOff.focus.active).toBe(false);

      const pulseOn = await relayClient.request<{
        registered: boolean;
        enabled: boolean;
        summary_enabled: boolean;
        has_push_to_start_token: boolean;
      }>("host.live_pulse.register.request", {
        platform: "ios",
        enabled: true,
        summary_enabled: false,
        push_to_start_token: "deadbeefcafebabe",
      });
      expect(pulseOn.registered).toBe(true);
      expect(pulseOn.enabled).toBe(true);
      expect(pulseOn.summary_enabled).toBe(false);
      expect(pulseOn.has_push_to_start_token).toBe(true);

      const pulseOff = await relayClient.request<{
        enabled: boolean;
        has_push_to_start_token: boolean;
      }>("host.live_pulse.register.request", {
        platform: "ios",
        enabled: false,
      });
      expect(pulseOff.enabled).toBe(false);
      expect(pulseOff.has_push_to_start_token).toBe(false);

      // 8g. Phase 3A: Host capabilities, OS Sandbox status, Zero-Trust Permission Firewall, and Batch Approve RPCs.
      const hostInfo3 = await relayClient.request<{
        capabilities: Record<string, unknown>;
      }>("host.info.request", {});
      expect(hostInfo3.capabilities.permission_firewall).toBe(true);
      expect(hostInfo3.capabilities.agent_arena).toBe(true);
      expect(hostInfo3.capabilities.time_travel_dvr).toBe(true);
      expect(hostInfo3.capabilities.os_sandbox).toBeTruthy();

      const sbxStatus = await relayClient.request<{
        backend: string;
        available: boolean;
        default_enabled: boolean;
      }>("sandbox.status.request", {});
      expect(sbxStatus.default_enabled).toBe(true);
      expect(typeof sbxStatus.backend).toBe("string");

      const policySaved = await relayClient.request<{
        saved: boolean;
        rule: { id: string; action: string };
      }>("policy.upsert.request", {
        server_socket: serverSocket,
        id: "e2e-allow-cargo-test",
        scope: "global",
        max_risk: "low",
        action: "auto_approve",
        match_command: ["cargo test*"],
        enabled: true,
      });
      expect(policySaved.saved).toBe(true);
      expect(policySaved.rule.id).toBe("e2e-allow-cargo-test");

      const policyCheckLow = await relayClient.request<{
        assessment: { tier: string; truncated: boolean };
        would_auto_approve: boolean;
      }>("policy.check.request", {
        server_socket: serverSocket,
        command: "cargo test --locked",
        cwd: repoDir,
      });
      expect(policyCheckLow.assessment.tier).toBe("low");
      expect(policyCheckLow.assessment.truncated).toBe(false);
      expect(policyCheckLow.would_auto_approve).toBe(true);

      const policyCheckTrunc = await relayClient.request<{
        assessment: { truncated: boolean };
        would_auto_approve: boolean;
      }>("policy.check.request", {
        server_socket: serverSocket,
        command: "cargo test --workspace…",
        cwd: repoDir,
      });
      expect(policyCheckTrunc.assessment.truncated).toBe(true);
      expect(policyCheckTrunc.would_auto_approve).toBe(false);

      const policyCheckCrit = await relayClient.request<{
        assessment: { tier: string };
        would_auto_approve: boolean;
      }>("policy.check.request", {
        server_socket: serverSocket,
        command: "cat ~/.ssh/id_rsa && rm -rf /",
        cwd: repoDir,
      });
      expect(policyCheckCrit.assessment.tier).toBe("critical");
      expect(policyCheckCrit.would_auto_approve).toBe(false);

      const policyListed = await relayClient.request<{
        rules: Array<{ id: string }>;
      }>("policy.list.request", { server_socket: serverSocket });
      expect(policyListed.rules.some((r) => r.id === "e2e-allow-cargo-test")).toBe(true);

      const policyRevoked = await relayClient.request<{ revoked: boolean }>(
        "policy.revoke.request",
        { server_socket: serverSocket, rule_id: "e2e-allow-cargo-test" }
      );
      expect(policyRevoked.revoked).toBe(true);

      const policyRemoved = await relayClient.request<{ removed: boolean }>(
        "policy.remove.request",
        { server_socket: serverSocket, rule_id: "e2e-allow-cargo-test" }
      );
      expect(policyRemoved.removed).toBe(true);

      const batchRes = await relayClient.request<{
        approved_count: number;
        rejected_count: number;
      }>("prompt.batch_approve.request", {
        server_socket: serverSocket,
        items: [],
      });
      expect(batchRes.approved_count).toBe(0);

      // 8h. Phase 3B: Agent Arena RPC lifecycle over E2EE relay.
      const arenaStart = await relayClient.request<{
        arena: { id: string; status: string };
        racers: Array<{ racer_name: string }>;
      }>("arena.start.request", {
        server_socket: serverSocket,
        prompt: "E2E Arena race test",
        repo_path: repoDir,
        racers: ["claude", "codex"],
        verify_cmd: "true",
        launch_agents: false,
      });
      expect(arenaStart.arena.id).toBeTruthy();
      expect(arenaStart.racers.length).toBe(2);

      const arenaVerify = await relayClient.request<{
        scorecard: { v: number };
        racers: Array<{ verify_exit_code: number | null }>;
      }>("arena.verify.request", {
        server_socket: serverSocket,
        arena_id: arenaStart.arena.id,
      });
      expect(arenaVerify.scorecard.v).toBe(1);
      expect(arenaVerify.racers.every((r) => r.verify_exit_code === 0)).toBe(true);

      const arenaList = await relayClient.request<{
        arenas: Array<{ arena: { id: string } }>;
      }>("arena.list.request", { server_socket: serverSocket });
      expect(arenaList.arenas.some((a) => a.arena.id === arenaStart.arena.id)).toBe(true);

      // 8i. Phase 3B: Time-Travel DVR snapshot, pin, inspect & Zero-CDN Redacted HTML Trace export over E2EE relay.
      const dvrRunId = `e2e-run-${Date.now()}`;
      const dvrSnap = await relayClient.request<{
        turn: { turn_seq: number; checkpoint_id: string };
      }>("dvr.snapshot.request", {
        server_socket: serverSocket,
        pane: paneId,
        run_id: dvrRunId,
        agent_name: "e2e-agent",
        provider: "codex",
        cwd: repoDir,
        note: "e2e turn with sk-proj-secret1234567890abcdefghijklmnop",
      });
      expect(dvrSnap.turn.turn_seq).toBe(1);

      const dvrPin = await relayClient.request<{ pinned: boolean }>(
        "dvr.pin.request",
        {
          server_socket: serverSocket,
          run_id: dvrRunId,
          cwd: repoDir,
          turn_seq: 1,
          pinned: true,
          note: "golden",
        }
      );
      expect(dvrPin.pinned).toBe(true);

      const dvrList = await relayClient.request<{
        turns: Array<{ turn_seq: number; pinned: boolean }>;
      }>("dvr.list.request", {
        server_socket: serverSocket,
        run_id: dvrRunId,
      });
      expect(dvrList.turns.length).toBe(1);
      expect(dvrList.turns[0]!.pinned).toBe(true);

      const traceRes = await relayClient.request<{
        turn_count: number;
        total_redactions: number;
        html: string;
      }>("dvr.export_trace.request", {
        server_socket: serverSocket,
        run_id: dvrRunId,
        cwd: repoDir,
        agent_name: "e2e-agent",
        provider: "codex",
        redact_strict: true,
      });
      expect(traceRes.turn_count).toBe(1);
      expect(traceRes.total_redactions).toBeGreaterThan(0);
      expect(traceRes.html).toContain("<!DOCTYPE html>");
      expect(traceRes.html).not.toContain("<script src=");
      expect(traceRes.html).not.toContain("sk-proj-secret1234567890abcdefghijklmnop");

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
