#!/usr/bin/env python3
"""Phase 1 end-to-end integration tests:
- 1A BS Detector: `agent audit` detection, history, and queue-only `--challenge`
- 1B Cross-Worktree Collision Radar: `worktree conflicts`, `worktree shadow-check`
     (with 0-byte `.git/objects` mutation check), and queue-only `worktree warn`
- 1C Circuit Breaker & Cache Warmth: `agent guard status | extend | dismiss`
"""

import json
import subprocess
import unittest
from pathlib import Path

import test_managed_agents
from test_managed_agents import AGENT


class Phase1Integration(test_managed_agents.ManagedAgents):
    def setUp(self):
        super().setUp()
        self.env |= {
            "GIT_AUTHOR_NAME": "masil-test",
            "GIT_AUTHOR_EMAIL": "test@masil.dev",
            "GIT_COMMITTER_NAME": "masil-test",
            "GIT_COMMITTER_EMAIL": "test@masil.dev",
            "GIT_CONFIG_NOSYSTEM": "1",
        }
        self.repo = (self.server.path / "repo").resolve()
        self.git("init", "-q", "-b", "main", str(self.repo), cwd=self.server.path)
        (self.repo / "src").mkdir(parents=True, exist_ok=True)
        (self.repo / "src/auth.ts").write_text(
            "export function verifyToken(t: string): boolean {\n  return t.length > 0;\n}\n",
            encoding="utf-8",
        )
        (self.repo / "src/auth.test.ts").write_text(
            "test('verify', () => {\n  expect(true).toBe(true);\n});\n",
            encoding="utf-8",
        )
        self.git("add", "-A")
        self.git("commit", "-q", "-m", "base")

    def git(self, *args, cwd=None):
        return subprocess.run(
            ["git", *args],
            cwd=cwd or self.repo,
            env=self.env,
            check=True,
            capture_output=True,
            text=True,
        ).stdout

    def worktree_cli(self, *args, check=True):
        res = subprocess.run(
            [str(AGENT), "worktree", *args],
            env=self.env,
            text=True,
            capture_output=True,
            timeout=60,
        )
        if check:
            self.assertEqual(res.returncode, 0, res.stderr)
            return json.loads(res.stdout)
        return res

    def test_1a_agent_audit_detects_bs_patterns_and_queues_challenge(self):
        self.cli("start", "auditor", "codex", "--cwd", str(self.repo))

        # Introduce T01 (.skip), G01 (eslint-disable), G02 (eval) in working tree.
        (self.repo / "src/auth.test.ts").write_text(
            "test.skip('verify', () => {\n  // skipped\n});\n",
            encoding="utf-8",
        )
        (self.repo / "src/auth.ts").write_text(
            "// eslint-disable-next-line\nexport function verifyToken(t: string): boolean {\n  return Boolean(eval(t));\n}\n",
            encoding="utf-8",
        )

        res = self.cli("audit", "auditor", "--json")
        audit = res["audit"]
        rules = {f["rule_id"] for f in audit["findings"]}
        self.assertIn("T01", rules, audit)
        self.assertIn("G01", rules, audit)
        self.assertIn("G02", rules, audit)
        self.assertEqual(audit["verdict"], "flagged")
        self.assertGreaterEqual(audit["risk_score"], 50)
        self.assertEqual(res["ui"]["v"], 1)

        # Challenge queues a prompt safely without injecting raw keys.
        t01 = next(f for f in audit["findings"] if f["rule_id"] == "T01")
        chal = self.cli("audit", "auditor", "--challenge", t01["id"])
        self.assertTrue(chal["challenged"])
        self.assertIn("T01", chal["prompt"])

        queue = self.cli("queue", "auditor")
        previews = [it.get("preview") or "" for it in queue["items"]]
        self.assertTrue(any("[masil Audit" in p for p in previews), previews)

    def test_1b_collision_radar_zero_object_pollution_shadow_check_and_warn(self):
        wt_a = (self.server.path / "wt-a").resolve()
        wt_b = (self.server.path / "wt-b").resolve()
        self.git("worktree", "add", "-q", "-b", "feat-a", str(wt_a))
        self.git("worktree", "add", "-q", "-b", "feat-b", str(wt_b))

        # Conflicting edits on the same lines in src/auth.ts.
        (wt_a / "src/auth.ts").write_text(
            "export function verifyToken(t: string): boolean {\n  return t.startsWith('A_');\n}\n",
            encoding="utf-8",
        )
        (wt_b / "src/auth.ts").write_text(
            "export function verifyToken(t: string): boolean {\n  return t.startsWith('B_');\n}\n",
            encoding="utf-8",
        )

        self.cli("start", "alpha", "codex", "--cwd", str(wt_a))
        self.cli("start", "beta", "codex", "--cwd", str(wt_b))

        obj_dir = self.repo / ".git/objects"
        before_objs = sorted(str(p.relative_to(obj_dir)) for p in obj_dir.rglob("*") if p.is_file())

        matrix = self.worktree_cli(
            "conflicts",
            str(self.repo),
            "--socket",
            str(self.server.socket),
            "--json",
        )
        self.assertGreaterEqual(matrix["agents_checked"], 2)
        pairs = matrix["pairs"]
        self.assertEqual(len(pairs), 1, pairs)
        self.assertEqual(pairs[0]["severity"], "hard_conflict")
        self.assertIn("src/auth.ts", pairs[0]["files"])

        # D-04 invariant: 0 bytes written into user's .git/objects!
        after_objs = sorted(str(p.relative_to(obj_dir)) for p in obj_dir.rglob("*") if p.is_file())
        self.assertEqual(before_objs, after_objs)

        # Shadow integration check on non-conflicting pair succeeds & cleans up.
        (wt_b / "src/auth.ts").write_text(
            "export function verifyToken(t: string): boolean {\n  return t.length > 0;\n}\n",
            encoding="utf-8",
        )
        (wt_b / "src/helper.ts").write_text("export const ok = 1;\n", encoding="utf-8")
        shadow = self.worktree_cli(
            "shadow-check",
            str(wt_a),
            str(wt_b),
            "--cmd",
            "test -f src/helper.ts && grep -q A_ src/auth.ts",
            "--repo",
            str(self.repo),
        )
        self.assertTrue(shadow["merge_clean"], shadow)
        self.assertEqual(shadow["exit_code"], 0, shadow)
        self.assertEqual(shadow["ui"]["v"], 1)

        after_shadow_objs = sorted(
            str(p.relative_to(obj_dir)) for p in obj_dir.rglob("*") if p.is_file()
        )
        self.assertEqual(before_objs, after_shadow_objs)

        # Queue-only sibling warning via `worktree warn`.
        warned = self.worktree_cli(
            "warn",
            "alpha",
            "--about",
            "beta",
            "--socket",
            str(self.server.socket),
        )
        self.assertEqual(warned["warned"], "alpha")
        queue = self.cli("queue", "alpha")
        self.assertTrue(
            any("Collision Radar" in (it.get("preview") or "") for it in queue["items"]),
            queue["items"],
        )

    def test_1c_agent_guard_status_extend_and_dismiss(self):
        self.cli("start", "guarded", "codex", "--cwd", str(self.repo))

        status = self.cli("guard", "guarded", "status")
        self.assertEqual(status["provider"], "codex")
        self.assertIn("breaker", status)
        self.assertIn("cache_warmth", status)

        extended = self.cli("guard", "guarded", "extend", "--turns", "10")
        self.assertGreaterEqual(extended["bypass_until_turn"], 10)
        self.assertFalse(extended["tripped"])

        dismissed = self.cli("guard", "guarded", "dismiss")
        self.assertFalse(dismissed["tripped"])


for _name in dir(test_managed_agents.ManagedAgents):
    if _name.startswith("test_") and _name not in Phase1Integration.__dict__:
        setattr(Phase1Integration, _name, None)


if __name__ == "__main__":
    unittest.main()
