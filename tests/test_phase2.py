#!/usr/bin/env python3
"""Phase 2 end-to-end integration tests:
- 2B Attention Shield CLI (`masil-agent focus on <duration> | status | off`)
- 2B 90-Second Morning / Return Briefing (`masil-agent briefing [--since 8h] --json`)
"""

import json
import subprocess
import unittest

import test_managed_agents
from test_managed_agents import AGENT


class Phase2Integration(test_managed_agents.ManagedAgents):
    def setUp(self):
        super().setUp()
        self.env |= {
            "GIT_AUTHOR_NAME": "masil-test",
            "GIT_AUTHOR_EMAIL": "test@masil.dev",
            "GIT_COMMITTER_NAME": "masil-test",
            "GIT_COMMITTER_EMAIL": "test@masil.dev",
            "GIT_CONFIG_NOSYSTEM": "1",
        }
        self.repo = (self.server.path / "repo-p2").resolve()
        subprocess.run(
            ["git", "init", "-q", "-b", "main", str(self.repo)],
            cwd=self.server.path,
            env=self.env,
            check=True,
        )
        (self.repo / "app.ts").write_text("export const ok = true;\n", encoding="utf-8")
        subprocess.run(["git", "add", "-A"], cwd=self.repo, env=self.env, check=True)
        subprocess.run(
            ["git", "commit", "-q", "-m", "base"],
            cwd=self.repo,
            env=self.env,
            check=True,
        )

    def top_cli(self, *args):
        res = subprocess.run(
            [str(AGENT), *args],
            env=self.env,
            text=True,
            capture_output=True,
            timeout=60,
        )
        self.assertEqual(res.returncode, 0, res.stderr)
        return json.loads(res.stdout)

    def test_2b_focus_shield_cli_lifecycle_and_briefing_generation(self):
        self.cli("start", "frontend", "codex", "--cwd", str(self.repo))

        # Initial Focus Shield state is OFF.
        st0 = self.top_cli("focus", "status", "--socket", str(self.server.socket), "--json")
        self.assertFalse(st0["focus"]["active"])

        # Turn ON for 25m with reason.
        on_res = self.top_cli(
            "focus",
            "on",
            "25m",
            "--reason",
            "deep-architecture",
            "--socket",
            str(self.server.socket),
            "--json",
        )
        self.assertTrue(on_res["focus"]["active"])
        self.assertGreater(on_res["focus"]["remaining_ms"], 20 * 60_000)
        self.assertEqual(on_res["focus"]["reason"], "deep-architecture")

        # Status reflects active state.
        st1 = self.top_cli("focus", "status", "--socket", str(self.server.socket), "--json")
        self.assertTrue(st1["focus"]["active"])

        # Briefing returns deterministic buckets + validated Masil UI Spec v1 document.
        brief = self.top_cli(
            "briefing",
            "--since",
            "8h",
            "--socket",
            str(self.server.socket),
            "--json",
        )
        self.assertEqual(brief["window_label"], "8h")
        self.assertEqual(brief["ui"]["v"], 1)
        self.assertTrue(brief["focus"]["active"])
        self.assertGreaterEqual(brief["metrics"]["active_agents"], 1)

        # Turn OFF returns held_events list and deactivates shield.
        off_res = self.top_cli(
            "focus",
            "off",
            "--socket",
            str(self.server.socket),
            "--json",
        )
        self.assertFalse(off_res["focus"]["active"])
        self.assertIsInstance(off_res["held_events"], list)


for _name in dir(test_managed_agents.ManagedAgents):
    if _name.startswith("test_") and _name not in Phase2Integration.__dict__:
        setattr(Phase2Integration, _name, None)


if __name__ == "__main__":
    unittest.main()
