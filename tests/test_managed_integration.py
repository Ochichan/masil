#!/usr/bin/env python3
"""Optional managed-agent hook bridge with synthetic provider callbacks."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

from test_compatibility import RMUX, ROOT, Server, wait_for


AGENT = Path(os.environ.get("RMUX_AGENT", ROOT / "bin/rmux-agent"))
MAX_CALLBACK_BYTES = 256 * 1024


class ManagedIntegration(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.fixture = Path(tempfile.mkdtemp(prefix="rmux-integration-provider-"))
        fixture = cls.fixture / "provider"
        subprocess.run(
            [
                "cc",
                "-Wall",
                "-Wextra",
                "-Werror",
                str(ROOT / "tests/faults/agent.c"),
                "-o",
                str(fixture),
            ],
            check=True,
            capture_output=True,
        )
        for provider in ("claude", "codex", "opencode"):
            shutil.copy2(fixture, cls.fixture / provider)

    @classmethod
    def tearDownClass(cls):
        shutil.rmtree(cls.fixture)

    def setUp(self):
        self.server = Server(RMUX)
        self.server.__enter__()
        self.addCleanup(self.server.__exit__)
        self.env = self.server.env | {
            "PATH": f"{self.fixture}:/usr/bin:/bin",
            "XDG_CONFIG_HOME": str(self.server.path / "config"),
        }

    def cli(self, *args, check=True, env=None, input_text=None, timeout=10):
        result = subprocess.run(
            [str(AGENT), "agent", "--socket", str(self.server.socket), *args],
            env=env or self.env,
            input=input_text,
            text=True,
            capture_output=True,
            timeout=timeout,
        )
        if check:
            self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
            self.assertEqual(result.stderr, "")
            return json.loads(result.stdout)
        return result

    def start(self, name, provider):
        receipt = self.cli("start", name, provider, "--cwd", str(self.server.path))
        wait_for(lambda: self.cli("get", name)["process"] == "running", 4)
        return receipt

    def hook(self, receipt, provider, payload, sequence, *, action=None, run=None, check=True):
        env = self.env | {
            "TMUX_PANE": receipt["pane_id"],
            "RMUX_AGENT_RUN": run or receipt["run"],
            "RMUX_AGENT_SEQUENCE": str(sequence),
        }
        input_text = payload if isinstance(payload, str) else json.dumps(payload)
        args = ["integration", "hook", provider]
        if action is not None:
            args.append(action)
        return self.cli(
            *args,
            check=check,
            env=env,
            input_text=input_text,
        )

    def test_status_and_export_are_explicit_and_non_installing(self):
        status = self.cli("integration", "status")
        self.assertFalse(status["installed"])
        self.assertEqual(len(status["targets"]), 17)
        capabilities = {item["provider"]: item["capability"] for item in status["targets"]}
        self.assertEqual(capabilities["opencode"], "full_lifecycle")
        self.assertEqual(capabilities["codex"], "native_session_only")

        exported = self.cli("integration", "export", "codex")
        self.assertFalse(exported["installed"])
        self.assertFalse(exported["live_provider_tested"])
        script = exported["files"][0]["content"]
        self.assertIn(str(AGENT.resolve()), script)
        self.assertNotIn("eval", script)
        path = self.server.path / "hook.sh"
        path.write_text(script)
        syntax = subprocess.run(
            ["/bin/sh", "-n", str(path)], capture_output=True, text=True, timeout=5
        )
        self.assertEqual(syntax.returncode, 0, syntax.stderr)

        opencode = self.cli(
            "integration", "export", "opencode", "--directory", str(self.server.path)
        )
        self.assertEqual(opencode["export"], "javascript_plugin")
        plugin = self.server.path / "opencode.mjs"
        plugin_text = opencode["files"][0]["content"]
        plugin.write_text(plugin_text)
        node = shutil.which("node")
        if node:
            checked = subprocess.run(
                [node, "--check", str(plugin)], capture_output=True, text=True, timeout=5
            )
            self.assertEqual(checked.returncode, 0, checked.stderr)

            driver = self.server.path / "exercise-plugin.mjs"
            driver.write_text(
                """import { pathToFileURL } from 'node:url';
const plugin = await import(pathToFileURL(process.argv[2]).href);
const hooks = await plugin.RmuxAgentStatePlugin();
const mode = process.argv[3];
if (mode === 'unconfigured' || mode === 'oversize') {
  delete process.env.TMUX_PANE;
  delete process.env.RMUX_AGENT_RUN;
  delete process.env.RMUX_AGENT_SOCKET;
}
await hooks['chat.message']({sessionID:'s1'});
if (mode === 'oversize') {
  process.env.TMUX_PANE = '%999';
  process.env.RMUX_AGENT_RUN = 'test-run';
  process.env.RMUX_AGENT_SOCKET = process.argv[4];
}
const padding = mode === 'oversize' ? 'x'.repeat(4 * 1024 * 1024) : '';
await hooks.event({event:{type:'session.updated', properties:{sessionID:'s1', padding}}});
await new Promise(resolve => setTimeout(resolve, 300));
console.log('provider-survived');
"""
            )

            adapter_env = self.env | {
                "TMUX_PANE": "%999",
                "RMUX_AGENT_RUN": "test-run",
                "RMUX_AGENT_SOCKET": str(self.server.socket),
            }

            vanished = self.server.path / "vanished-rmux-agent"
            vanished.write_text("#!/bin/sh\nexit 0\n")
            vanished.chmod(0o700)
            missing_plugin = self.server.path / "opencode-missing.mjs"
            missing_plugin.write_text(plugin_text.replace(str(AGENT.resolve()), str(vanished)))
            vanished.unlink()
            missing = subprocess.run(
                [node, str(driver), str(missing_plugin), "missing"],
                capture_output=True,
                text=True,
                timeout=5,
                env=adapter_env,
            )
            self.assertEqual(missing.returncode, 0, missing.stderr)
            self.assertIn("provider-survived", missing.stdout)

            epipe_plugin = self.server.path / "opencode-epipe.mjs"
            epipe_plugin.write_text(plugin_text.replace(str(AGENT.resolve()), "/bin/true"))
            epipe = subprocess.run(
                [node, str(driver), str(epipe_plugin), "epipe"],
                capture_output=True,
                text=True,
                timeout=5,
                env=adapter_env,
            )
            self.assertEqual(epipe.returncode, 0, epipe.stderr)
            self.assertIn("provider-survived", epipe.stdout)

            marker = self.server.path / "spawned"
            recorder = self.server.path / "record-rmux-agent"
            recorder.write_text('#!/bin/sh\nprintf "spawned\\n" >> "$RMUX_SPAWN_MARKER"\n')
            recorder.chmod(0o700)
            guarded_plugin = self.server.path / "opencode-guarded.mjs"
            guarded_plugin.write_text(
                plugin_text.replace(str(AGENT.resolve()), str(recorder))
            )
            guard_env = self.env | {"RMUX_SPAWN_MARKER": str(marker)}
            for mode in ("unconfigured", "oversize"):
                with self.subTest(adapter_guard=mode):
                    guarded = subprocess.run(
                        [
                            node,
                            str(driver),
                            str(guarded_plugin),
                            mode,
                            str(self.server.socket),
                        ],
                        capture_output=True,
                        text=True,
                        timeout=5,
                        env=guard_env,
                    )
                    self.assertEqual(guarded.returncode, 0, guarded.stderr)
                    self.assertIn("provider-survived", guarded.stdout)
                    self.assertFalse(marker.exists(), f"{mode} callback spawned adapter")

        pi = self.cli("integration", "export", "pi", "--directory", str(self.server.path))
        extension = pi["files"][0]["content"]
        self.assertIn("export default function (pi: any)", extension)
        self.assertIn('pi.on("session_start"', extension)
        self.assertIn("shell: false", extension)
        self.assertIn("scopeConfigured()", extension)
        self.assertIn('typeof body !== "string"', extension)
        self.assertIn('Buffer.byteLength(body, "utf8")', extension)

        hermes = self.cli(
            "integration", "export", "hermes", "--directory", str(self.server.path)
        )
        init = next(item for item in hermes["files"] if item["path"].endswith("__init__.py"))
        plugin = self.server.path / "hermes.py"
        plugin.write_text(init["content"])
        compiled = subprocess.run(
            ["python3", "-m", "py_compile", str(plugin)],
            capture_output=True,
            text=True,
            timeout=5,
        )
        self.assertEqual(compiled.returncode, 0, compiled.stderr)

    def test_claude_codex_identity_and_opencode_lifecycle_callbacks(self):
        codex = self.start("codex-agent", "codex")
        self.cli(
            "report",
            "--pane",
            codex["pane_id"],
            "--run",
            codex["run"],
            "--sequence",
            "1",
            "--state",
            "working",
        )
        result = self.hook(
            codex,
            "codex",
            {"hook_event_name": "SessionStart", "session_id": "codex-session"},
            2,
        )
        self.assertEqual(result["state"], "unknown")
        observed = self.cli("get", codex["pane_id"])
        self.assertEqual(observed["state"], "working")
        self.assertEqual(observed["session_id"], "codex-session")

        claude = self.start("claude-agent", "claude")
        self.hook(
            claude,
            "claude",
            {"hook_event_name": "SessionStart", "session_id": "claude-session"},
            3,
        )
        self.assertEqual(self.cli("get", claude["pane_id"])["session_id"], "claude-session")

        opencode = self.start("opencode-agent", "opencode")
        initial_global = self.hook(
            opencode,
            "opencode",
            {
                "event": {
                    "type": "session.updated",
                    "properties": {"sessionID": "open-session"},
                }
            },
            1,
            check=False,
        )
        self.assertNotEqual(initial_global.returncode, 0)

        self.hook(
            opencode,
            "opencode",
            {
                "hook_event_name": "chat.message",
                "rmux_scope": "frontend",
                "sessionID": "open-session",
            },
            1,
            action="working",
        )
        bound = self.cli("get", opencode["pane_id"])
        self.assertEqual(bound["state"], "working")
        self.assertEqual(bound["session_id"], "open-session")

        child_update = self.hook(
            opencode,
            "opencode",
            {
                "event": {
                    "type": "session.updated",
                    "properties": {
                        "sessionID": "child-session",
                        "info": {"id": "child-session", "parentID": "open-session"},
                    },
                }
            },
            2,
            check=False,
        )
        self.assertNotEqual(child_update.returncode, 0)
        self.assertIn("child-session", child_update.stderr)

        child_idle = self.hook(
            opencode,
            "opencode",
            {
                "event": {
                    "type": "session.idle",
                    "properties": {
                        "sessionID": "child-session",
                        "parentSessionID": "open-session",
                    },
                }
            },
            3,
            check=False,
        )
        self.assertNotEqual(child_idle.returncode, 0)
        after_child = self.cli("get", opencode["pane_id"])
        self.assertEqual(after_child["state"], "working")
        self.assertEqual(after_child["session_id"], "open-session")

        self.hook(
            opencode,
            "opencode",
            {
                "event": {
                    "type": "permission.asked",
                    "properties": {"sessionID": "open-session"},
                }
            },
            4,
        )
        observed = self.cli("get", opencode["pane_id"])
        self.assertEqual(observed["state"], "blocked")
        self.assertEqual(observed["session_id"], "open-session")

    def test_bad_json_bounds_and_stale_scope_are_rejected(self):
        codex = self.start("codex-agent", "codex")
        bad_json = self.hook(codex, "codex", "not-json", 1, check=False)
        self.assertNotEqual(bad_json.returncode, 0)
        self.assertIn("invalid hook callback JSON", bad_json.stderr)

        oversized = self.hook(
            codex,
            "codex",
            " " * (MAX_CALLBACK_BYTES + 1),
            2,
            check=False,
        )
        self.assertNotEqual(oversized.returncode, 0)
        self.assertIn("exceeds", oversized.stderr)

        stale = self.hook(
            codex,
            "codex",
            {"hook_event_name": "SessionStart", "session_id": "wrong-run"},
            3,
            run="stale-run",
            check=False,
        )
        self.assertNotEqual(stale.returncode, 0)
        self.assertIn("stale or mismatched", stale.stderr)
        self.assertIsNone(self.cli("get", codex["pane_id"])["session_id"])

        wrong_provider = self.hook(
            codex,
            "claude",
            {"hook_event_name": "SessionStart", "session_id": "wrong-provider"},
            4,
            check=False,
        )
        self.assertNotEqual(wrong_provider.returncode, 0)
        self.assertIn("stale or mismatched", wrong_provider.stderr)


if __name__ == "__main__":
    unittest.main(verbosity=2)
