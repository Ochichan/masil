#!/usr/bin/env python3
"""End-to-end integration tests for masil remote gateway, E2EE relay,
external terminal wrap, opt-in hook installer, desktop pane preservation,
4-provider transcripts, files/git diff, voice STT, and device revocation."""

import json
import os
import pty
import select
import shutil
import socket
import sqlite3
import subprocess
import tempfile
import time
import unittest
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
MASIL = ROOT / "bin/masil"
AGENT = ROOT / "bin/masil-agent"
RELAY = ROOT / "bin/masil-relay"
MOBILE_DIR = ROOT / "mobile"


def free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def wait_until(predicate, timeout: float = 10.0, step: float = 0.1):
    deadline = time.monotonic() + timeout
    last_err = None
    while time.monotonic() < deadline:
        try:
            res = predicate()
            if res:
                return res
        except Exception as err:  # noqa: BLE001
            last_err = err
        time.sleep(step)
    raise AssertionError(f"condition not met within {timeout}s (last_err={last_err})")


class RemoteGatewayE2E(unittest.TestCase):
    def setUp(self):
        for bin_path in (MASIL, AGENT, RELAY):
            if not bin_path.exists():
                self.fail(f"missing built binary {bin_path}; run `make all && make relay`")

        self.tmp = Path(tempfile.mkdtemp(prefix="masil-remote-e2e-", dir="/tmp")).resolve()
        self.addCleanup(lambda: shutil.rmtree(self.tmp, ignore_errors=True))

        self.home = self.tmp / "home"
        self.xdg_config = self.tmp / "config"
        self.xdg_state = self.tmp / "state"
        self.tmux_tmp = self.tmp / "tmux"
        self.repo = self.tmp / "repo"
        for d in (self.home, self.xdg_config, self.xdg_state, self.tmux_tmp, self.repo):
            d.mkdir(parents=True, exist_ok=True)

        self.env = os.environ.copy()
        self.env.pop("TMUX", None)
        self.env.pop("TMUX_PANE", None)
        self.env.update(
            {
                "HOME": str(self.home),
                "XDG_CONFIG_HOME": str(self.xdg_config),
                "XDG_STATE_HOME": str(self.xdg_state),
                "TMUX_TMPDIR": str(self.tmux_tmp),
                "PATH": f"{ROOT / 'bin'}:{self.env.get('PATH', '')}",
            }
        )

        # Initialize a real git repository with an uncommitted modification.
        self._git("init", "-b", "main")
        self._git("config", "user.name", "Masil Test")
        self._git("config", "user.email", "test@masil.dev")
        (self.repo / "hello.txt").write_text("initial line\n", encoding="utf-8")
        self._git("add", "hello.txt")
        self._git("commit", "-m", "init")
        (self.repo / "hello.txt").write_text(
            "initial line\nmodified line for diff\n", encoding="utf-8"
        )

        # Start an isolated masil server with an explicit 160x45 desktop pane.
        sock_dir = self.tmux_tmp / f"masil-{os.getuid()}"
        sock_dir.mkdir(parents=True, exist_ok=True)
        os.chmod(sock_dir, 0o700)
        self.socket = sock_dir / "e2e"
        subprocess.run(
            [
                str(MASIL),
                "-S",
                str(self.socket),
                "new-session",
                "-d",
                "-s",
                "desk",
                "-x",
                "160",
                "-y",
                "45",
                "-c",
                str(self.repo),
                "/bin/sh",
            ],
            env=self.env,
            check=True,
        )
        self.addCleanup(
            lambda: subprocess.run(
                [str(MASIL), "-S", str(self.socket), "kill-server"],
                env=self.env,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                check=False,
            )
        )
        self.pane_id = subprocess.check_output(
            [
                str(MASIL),
                "-S",
                str(self.socket),
                "display-message",
                "-p",
                "-t",
                "desk:0.0",
                "#{pane_id}",
            ],
            env=self.env,
            text=True,
        ).strip()

        # Seed synthetic transcripts for Claude, Codex, and Antigravity (agy).
        self._seed_transcripts()

        # Prepare mock interactive TUI (`agy`) and mock OpenCode (`--answers`) binaries.
        self.mock_bin = self.tmp / "mock-bin"
        self.mock_bin.mkdir(parents=True, exist_ok=True)
        self.agy_log = self.tmp / "agy-e2e.jsonl"
        self.oc_state = self.tmp / "oc-mock-state"
        self.oc_state.mkdir(parents=True, exist_ok=True)
        self.oc_cwd = self.tmp / "oc-cwd"
        self.oc_cwd.mkdir(parents=True, exist_ok=True)

        shutil.copy2(ROOT / "tests/faults/opencode_mock.py", self.mock_bin / "opencode")
        (self.mock_bin / "opencode").chmod(0o755)
        subprocess.run(
            ["cc", str(ROOT / "tests/faults/node_launcher.c"), "-o", str(self.mock_bin / "node")],
            check=True,
            capture_output=True,
        )
        self._write_mock_agy_script(self.mock_bin / "agy")

        # Prepend mock_bin to PATH in agent/masil env (keep clean PATH for Node/Vitest).
        self.clean_path = self.env["PATH"]
        self.env["PATH"] = f"{self.mock_bin}:{self.clean_path}"
        self.env["MOCK_OPENCODE_STATE"] = str(self.oc_state)
        self.env["MOCK_AGY_LOG"] = str(self.agy_log)
        for k, v in (
            ("PATH", self.env["PATH"]),
            ("MOCK_OPENCODE_STATE", str(self.oc_state)),
            ("MOCK_AGY_LOG", str(self.agy_log)),
            ("XDG_CONFIG_HOME", str(self.xdg_config)),
            ("XDG_STATE_HOME", str(self.xdg_state)),
        ):
            subprocess.run(
                [str(MASIL), "-S", str(self.socket), "set-environment", "-g", k, v],
                env=self.env,
                check=True,
            )

        # Create a mock host STT script so E2EE voice upload works offline.
        self.stt_script = self.tmp / "mock-stt.sh"
        self.stt_script.write_text(
            "#!/bin/sh\n[ -s \"$1\" ] && printf '모바일 음성 테스트 완료\\n'\n",
            encoding="utf-8",
        )
        self.stt_script.chmod(0o755)

        self.procs: list[subprocess.Popen] = []
        self.addCleanup(self._terminate_procs)

    def _terminate_procs(self):
        for p in self.procs:
            if p.poll() is None:
                p.terminate()
        for p in self.procs:
            try:
                p.wait(timeout=3)
            except subprocess.TimeoutExpired:
                p.kill()
                p.wait(timeout=2)
            for stream in (p.stdin, p.stdout, p.stderr):
                if stream is not None:
                    stream.close()

    def _write_mock_agy_script(self, path: Path):
        path.write_text(
            r'''#!/usr/bin/env python3
import json
import os
import sys
import termios
import tty

LOG_PATH = os.environ.get("MOCK_AGY_LOG", "")

def log(obj):
    if not LOG_PATH:
        return
    with open(LOG_PATH, "a", encoding="utf-8") as f:
        f.write(json.dumps(obj, ensure_ascii=False) + "\n")
        f.flush()

def draw_menu(title, body, options, selected):
    sys.stdout.write("\x1b[2J\x1b[H")
    sys.stdout.write(f"{title}\r\n")
    if body:
        sys.stdout.write(f"{body}\r\n")
    sys.stdout.write("\r\n")
    for idx, label in enumerate(options):
        prefix = "❯ " if idx == selected else "  "
        sys.stdout.write(f"{prefix}{idx + 1}. {label}\r\n")
    sys.stdout.write("\r\n↑/↓ to navigate · Enter to confirm · Esc to cancel\r\n")
    sys.stdout.flush()

def read_key(fd):
    b = os.read(fd, 1)
    if not b:
        return ""
    if b == b"\x1b":
        # Check for CSI arrow sequence
        import select
        if select.select([fd], [], [], 0.05)[0]:
            b2 = os.read(fd, 1)
            if b2 == b"[" and select.select([fd], [], [], 0.05)[0]:
                b3 = os.read(fd, 1)
                return "\x1b[" + b3.decode("latin1")
        return "\x1b"
    return b.decode("utf-8", "replace")

def read_line_raw(fd):
    buf = bytearray()
    while True:
        b = os.read(fd, 1)
        if not b or b in (b"\r", b"\n"):
            break
        if b in (b"\x7f", b"\x08"):
            if buf:
                buf.pop()
            continue
        buf.extend(b)
    return buf.decode("utf-8", "replace")

def main():
    fd = sys.stdin.fileno()
    old = termios.tcgetattr(fd)
    try:
        tty.setraw(fd)
        # Phase 1: Question menu
        opts1 = ["Staging (Recommended)", "Production", "Other"]
        sel = 0
        draw_menu("Deployment Mode", "Pick deployment mode", opts1, sel)
        while True:
            k = read_key(fd)
            if k == "\x1b[B":
                sel = min(len(opts1) - 1, sel + 1)
                draw_menu("Deployment Mode", "Pick deployment mode", opts1, sel)
            elif k == "\x1b[A":
                sel = max(0, sel - 1)
                draw_menu("Deployment Mode", "Pick deployment mode", opts1, sel)
            elif k in ("\r", "\n"):
                log({"phase": 1, "selected": sel, "label": opts1[sel]})
                break

        # Phase 2: Permission / Custom-other dialog
        opts2 = ["Allow once", "Always allow", "Other"]
        sel = 0
        draw_menu("Run command?", "Command: rm -rf dist", opts2, sel)
        while True:
            k = read_key(fd)
            if k == "\x1b[B":
                sel = min(len(opts2) - 1, sel + 1)
                draw_menu("Run command?", "Command: rm -rf dist", opts2, sel)
            elif k == "\x1b[A":
                sel = max(0, sel - 1)
                draw_menu("Run command?", "Command: rm -rf dist", opts2, sel)
            elif k in ("\r", "\n"):
                if sel == 2:
                    sys.stdout.write("\x1b[2J\x1b[HTell agent what to do instead:\r\n> ")
                    sys.stdout.flush()
                    text = read_line_raw(fd)
                    log({"phase": 2, "selected": sel, "custom_text": text})
                else:
                    log({"phase": 2, "selected": sel, "label": opts2[sel]})
                break

        # Phase 3: Idle composer matching antigravity.toml idle rule
        sys.stdout.write("\x1b[2J\x1b[H")
        sys.stdout.write("────────────────────────────────────────\r\n")
        sys.stdout.write("> \r\n")
        sys.stdout.write("────────────────────────────────────────\r\n")
        sys.stdout.write("? for shortcuts    Gemini 4 Neon · xhigh\r\n")
        sys.stdout.flush()
        while True:
            line = read_line_raw(fd)
            if line:
                log({"phase": 3, "line": line})
    finally:
        termios.tcsetattr(fd, termios.TCSADRAIN, old)

if __name__ == "__main__":
    main()
''',
            encoding="utf-8",
        )
        path.chmod(0o755)

    def _git(self, *args: str):
        subprocess.run(
            ["git", *args],
            cwd=self.repo,
            env=self.env,
            check=True,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )

    def _seed_transcripts(self):
        # 1. Claude JSONL
        encoded_cwd = str(self.repo).replace("/", "-").replace("_", "-")
        claude_dir = self.home / ".claude/projects" / encoded_cwd
        claude_dir.mkdir(parents=True, exist_ok=True)
        (claude_dir / "claude-sess-001.jsonl").write_text(
            "\n".join(
                [
                    json.dumps(
                        {
                            "type": "user",
                            "cwd": str(self.repo),
                            "timestamp": "2026-11-24T01:00:00Z",
                            "message": {"content": "한글 질문입니다"},
                        }
                    ),
                    json.dumps(
                        {
                            "type": "assistant",
                            "cwd": str(self.repo),
                            "timestamp": "2026-11-24T01:00:02Z",
                            "message": {
                                "content": [
                                    {"type": "text", "text": "Claude E2E reply"},
                                    {
                                        "type": "tool_use",
                                        "id": "call_1",
                                        "name": "Bash",
                                        "input": {"cmd": "git status"},
                                    },
                                ]
                            },
                        }
                    ),
                ]
            )
            + "\n",
            encoding="utf-8",
        )

        # 2. Codex rollout JSONL
        codex_dir = self.home / ".codex/sessions/2026/11/24"
        codex_dir.mkdir(parents=True, exist_ok=True)
        (codex_dir / "rollout-2026-11-24T01-00-00-codex-e2e.jsonl").write_text(
            "\n".join(
                [
                    json.dumps(
                        {
                            "type": "session_meta",
                            "timestamp": "2026-11-24T01:00:00Z",
                            "payload": {"id": "codex-e2e-1", "cwd": str(self.repo)},
                        }
                    ),
                    json.dumps(
                        {
                            "type": "response_item",
                            "timestamp": "2026-11-24T01:00:03Z",
                            "payload": {
                                "type": "message",
                                "role": "assistant",
                                "content": [
                                    {"type": "output_text", "text": "Codex E2E answer"}
                                ],
                            },
                        }
                    ),
                ]
            )
            + "\n",
            encoding="utf-8",
        )

        # 3. Antigravity (agy) brain transcript + SQLite conversation_summaries.db
        agy_root = self.home / ".gemini/antigravity-cli"
        conv_id = "agy-conv-e2e-001"
        logs_dir = agy_root / "brain" / conv_id / ".system_generated/logs"
        logs_dir.mkdir(parents=True, exist_ok=True)
        (logs_dir / "transcript.jsonl").write_text(
            "\n".join(
                [
                    json.dumps(
                        {
                            "type": "USER_INPUT",
                            "created_at": "2026-11-24T01:00:00Z",
                            "content": "Review changes",
                        }
                    ),
                    json.dumps(
                        {
                            "type": "PLANNER_RESPONSE",
                            "created_at": "2026-11-24T01:00:05Z",
                            "thinking": "Checking repository files",
                            "response": "Antigravity E2E plan",
                            "tool_calls": [
                                {"name": "read_file", "args": {"Path": "hello.txt"}}
                            ],
                        }
                    ),
                ]
            )
            + "\n",
            encoding="utf-8",
        )
        conn = sqlite3.connect(agy_root / "conversation_summaries.db")
        try:
            conn.execute(
                """CREATE TABLE conversation_summaries (
                    conversation_id TEXT PRIMARY KEY,
                    title TEXT,
                    preview TEXT,
                    workspace_uris TEXT,
                    parent_conversation_id TEXT,
                    last_modified_time TEXT
                )"""
            )
            conn.execute(
                "INSERT INTO conversation_summaries VALUES (?, ?, ?, ?, ?, ?)",
                (
                    conv_id,
                    "E2E Agy Session",
                    "Review changes",
                    json.dumps([f"file://{self.repo}"]),
                    "",
                    "2026-11-24T01:00:05Z",
                ),
            )
            conn.commit()
        finally:
            conn.close()

        # 4. OpenCode SQLite (strictly read-only indexed query test)
        oc_dir = self.home / ".local/share/opencode"
        oc_dir.mkdir(parents=True, exist_ok=True)
        oc_conn = sqlite3.connect(oc_dir / "opencode.db")
        try:
            oc_conn.execute(
                """CREATE TABLE session (
                    id TEXT PRIMARY KEY,
                    title TEXT,
                    directory TEXT,
                    parent_id TEXT,
                    time_archived INTEGER,
                    time_updated INTEGER
                )"""
            )
            oc_conn.execute(
                """CREATE TABLE part (
                    id TEXT PRIMARY KEY,
                    session_id TEXT,
                    time_created INTEGER,
                    data TEXT
                )"""
            )
            oc_conn.execute(
                "INSERT INTO session VALUES (?, ?, ?, NULL, NULL, ?)",
                ("oc-e2e-1", "OpenCode E2E", str(self.repo), 1795500000000),
            )
            oc_conn.execute(
                "INSERT INTO part VALUES (?, ?, ?, ?)",
                (
                    "part-1",
                    "oc-e2e-1",
                    1795500000001,
                    json.dumps({"type": "text", "text": "OpenCode E2E reply"}),
                ),
            )
            oc_conn.commit()
        finally:
            oc_conn.close()

    def test_hooks_installer_is_idempotent_and_preserves_user_hooks(self):
        claude_settings = self.home / ".claude/settings.json"
        claude_settings.parent.mkdir(parents=True, exist_ok=True)
        claude_settings.write_text(
            json.dumps(
                {
                    "hooks": {
                        "Stop": [
                            {
                                "matcher": "*",
                                "hooks": [{"type": "command", "command": "echo keep-user"}],
                            }
                        ]
                    }
                }
            ),
            encoding="utf-8",
        )

        for _ in range(2):
            out = subprocess.check_output(
                [str(AGENT), "remote", "setup-hooks", "--install", "all"],
                env=self.env,
                text=True,
            )
            data = json.loads(out)
            self.assertTrue(all(h["hook_installed"] for h in data["hooks"]))

        settings_after = json.loads(claude_settings.read_text(encoding="utf-8"))
        stop_hooks = settings_after["hooks"]["Stop"]
        self.assertEqual(len(stop_hooks), 2)
        self.assertIn("keep-user", json.dumps(stop_hooks))

        subprocess.check_call(
            [str(AGENT), "remote", "setup-hooks", "--remove", "all"],
            env=self.env,
            stdout=subprocess.DEVNULL,
        )
        settings_removed = json.loads(claude_settings.read_text(encoding="utf-8"))
        self.assertEqual(len(settings_removed["hooks"]["Stop"]), 1)
        self.assertIn("keep-user", json.dumps(settings_removed["hooks"]["Stop"]))

    def test_wrap_shell_init_and_pty_execution(self):
        for shell in ("zsh", "bash", "fish"):
            script = subprocess.check_output(
                [str(AGENT), "wrap", "init", shell], env=self.env, text=True
            )
            self.assertIn("masil", script)
            self.assertNotIn("wrap -- command", script)

        # Execute `masil-agent wrap` inside a real PTY targeting our isolated socket.
        master_fd, slave_fd = pty.openpty()
        wrap_env = self.env.copy()
        wrap_env["TERM"] = "xterm-256color"
        try:
            proc = subprocess.Popen(
                [
                    str(AGENT),
                    "wrap",
                    "--socket",
                    str(self.socket),
                    "--session",
                    "wrap-e2e-test",
                    "--",
                    "/bin/sh",
                    "-c",
                    "echo WRAP_E2E_OK",
                ],
                stdin=slave_fd,
                stdout=slave_fd,
                stderr=slave_fd,
                cwd=self.repo,
                env=wrap_env,
                start_new_session=True,
                close_fds=True,
            )
            os.close(slave_fd)
            slave_fd = -1
            output = bytearray()
            deadline = time.monotonic() + 8.0
            while time.monotonic() < deadline:
                rlist, _, _ = select.select([master_fd], [], [], 0.2)
                if rlist:
                    try:
                        chunk = os.read(master_fd, 4096)
                        if not chunk:
                            break
                        output.extend(chunk)
                    except OSError:
                        break
                if proc.poll() is not None:
                    break
            rc = proc.wait(timeout=5)
            self.assertEqual(rc, 0, f"wrap failed: {output.decode('utf-8', 'replace')}")
        finally:
            if slave_fd >= 0:
                os.close(slave_fd)
            os.close(master_fd)

    def test_full_stack_relay_daemon_and_typescript_e2ee_client(self):
        relay_port = free_port()
        direct_port = free_port()
        relay_env = self.env.copy()
        relay_env["MASIL_RELAY_LISTEN"] = f"127.0.0.1:{relay_port}"

        relay_proc = subprocess.Popen(
            [str(RELAY)],
            env=relay_env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        self.procs.append(relay_proc)

        wait_until(
            lambda: urllib.request.urlopen(
                f"http://127.0.0.1:{relay_port}/healthz", timeout=1
            ).status
            == 200,
            timeout=10,
        )

        # Generate pairing offer and configure relay + direct listener.
        pair_json = json.loads(
            subprocess.check_output(
                [
                    str(AGENT),
                    "remote",
                    "pair",
                    "--relay",
                    f"ws://127.0.0.1:{relay_port}",
                    "--direct",
                    f"127.0.0.1:{direct_port}",
                    "--listen",
                    f"127.0.0.1:{direct_port}",
                    "--json",
                ],
                env=self.env,
                text=True,
            )
        )
        pair_uri = pair_json["uri"]
        self.assertTrue(pair_uri.startswith("masil://pair?v=1&"))

        # Add mock STT command to remote.toml for E2EE voice verification.
        remote_toml = self.xdg_config / "masil/remote.toml"
        remote_toml.write_text(
            remote_toml.read_text(encoding="utf-8")
            + f'\nstt_command = ["{self.stt_script}", "{{file}}"]\n',
            encoding="utf-8",
        )

        daemon_proc = subprocess.Popen(
            [str(AGENT), "remote", "daemon"],
            env=self.env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        self.procs.append(daemon_proc)

        # Wait until host registers with masil-relay.
        def host_online():
            with urllib.request.urlopen(
                f"http://127.0.0.1:{relay_port}/healthz", timeout=1
            ) as resp:
                data = json.loads(resp.read().decode("utf-8"))
                return data.get("hosts_online", 0) >= 1

        wait_until(host_online, timeout=10)

        status = json.loads(
            subprocess.check_output(
                [str(AGENT), "remote", "status"], env=self.env, text=True
            )
        )
        self.assertTrue(status["daemon_running"])
        self.assertTrue(
            any(s["socket"] == str(self.socket) for s in status["masil_servers"])
        )

        # Start managed interactive mock agents inside the isolated masil server:
        # 1) agy-e2e (agent name != tmux session "desk") running interactive TUI dialogs
        # 2) oc-e2e running OpenCode --answers HTTP API mock server
        agy_start = json.loads(
            subprocess.check_output(
                [
                    str(AGENT),
                    "agent",
                    "--socket",
                    str(self.socket),
                    "start",
                    "agy-e2e",
                    "agy",
                    "--cwd",
                    str(self.repo),
                    "--no-sandbox",
                ],
                env=self.env,
                text=True,
            )
        )
        agy_pane = agy_start["pane_id"]
        wait_until(
            lambda: "Pick deployment mode"
            in subprocess.check_output(
                [str(MASIL), "-S", str(self.socket), "capture-pane", "-p", "-t", agy_pane],
                env=self.env,
                text=True,
            ),
            timeout=10,
        )

        oc_start = json.loads(
            subprocess.check_output(
                [
                    str(AGENT),
                    "agent",
                    "--socket",
                    str(self.socket),
                    "start",
                    "oc-e2e",
                    "opencode",
                    "--cwd",
                    str(self.oc_cwd),
                    "--answers",
                    "--no-sandbox",
                ],
                env=self.env,
                text=True,
            )
        )
        oc_pane = oc_start["pane_id"]
        self.assertEqual(oc_start.get("answer_channel"), "ready")
        (self.oc_state / "permissions.json").write_text(
            json.dumps(
                [
                    {
                        "id": "per_e2e_1",
                        "sessionID": "ses_e2e",
                        "permission": "bash",
                        "patterns": ["git status"],
                        "always": ["git *"],
                        "metadata": {"command": "git status"},
                    }
                ]
            ),
            encoding="utf-8",
        )
        (self.oc_state / "questions.json").write_text(
            json.dumps(
                [
                    {
                        "id": "que_e2e_1",
                        "sessionID": "ses_e2e",
                        "questions": [
                            {
                                "header": "Targets",
                                "question": "Which platforms should we ship?",
                                "multiple": True,
                                "custom": True,
                                "options": [
                                    {"label": "iOS", "description": "Apple mobile"},
                                    {"label": "Android", "description": "Android mobile"},
                                    {"label": "Web", "description": "Browser"},
                                ],
                            }
                        ],
                    }
                ]
            ),
            encoding="utf-8",
        )

        # Run real TypeScript MasilRemoteClient E2E test over live sockets.
        if not (MOBILE_DIR / "node_modules/.bin/vitest").exists():
            self.skipTest("mobile/node_modules not installed; skipped TS client step")

        ts_env = self.env.copy()
        ts_env["PATH"] = self.clean_path
        ts_env.update(
            {
                "MASIL_E2E_PAIR_URI": pair_uri,
                "MASIL_E2E_MASIL_BIN": str(MASIL),
                "MASIL_E2E_AGENT_BIN": str(AGENT),
                "MASIL_E2E_SOCKET": str(self.socket),
                "MASIL_E2E_PANE": self.pane_id,
                "MASIL_E2E_REPO": str(self.repo),
                "MASIL_E2E_AGY_PANE": agy_pane,
                "MASIL_E2E_AGY_NAME": "agy-e2e",
                "MASIL_E2E_AGY_LOG": str(self.agy_log),
                "MASIL_E2E_OC_PANE": oc_pane,
                "MASIL_E2E_OC_NAME": "oc-e2e",
                "MASIL_E2E_OC_STATE": str(self.oc_state),
            }
        )
        res = subprocess.run(
            ["npx", "vitest", "run", "src/protocol/live-e2e.test.ts"],
            cwd=MOBILE_DIR,
            env=ts_env,
            text=True,
            capture_output=True,
        )
        if res.returncode != 0:
            self.fail(
                f"TypeScript live E2E client failed (rc={res.returncode}):\n"
                f"STDOUT:\n{res.stdout}\nSTDERR:\n{res.stderr}"
            )


if __name__ == "__main__":
    unittest.main()
