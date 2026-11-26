#!/usr/bin/env python3
"""Phase 3 end-to-end integration tests:
- 3A Cross-Provider Zero-Trust Permission Firewall (`masil-agent policy ...`)
- 3A Default-ON OS Micro-Sandbox + SHA-256 `.masil/sandbox.toml` trust + `doctor` (`masil-agent sandbox ...`)
- 3B Native Best-of-N Agent Arena (`masil-agent arena start|list|status|verify|promote|synthesize`)
- 3B Cross-Provider Time-Travel DVR & Zero-CDN Redacted HTML PR Verification Trace (`masil-agent agent dvr|trace`)
"""

import json
import os
import platform
import subprocess
import unittest
from pathlib import Path

import test_managed_agents
from test_managed_agents import AGENT


class Phase3Integration(test_managed_agents.ManagedAgents):
    def setUp(self):
        super().setUp()
        self.env |= {
            "GIT_AUTHOR_NAME": "masil-test",
            "GIT_AUTHOR_EMAIL": "test@masil.dev",
            "GIT_COMMITTER_NAME": "masil-test",
            "GIT_COMMITTER_EMAIL": "test@masil.dev",
            "GIT_CONFIG_NOSYSTEM": "1",
        }
        self.repo = (self.server.path / "repo-p3").resolve()
        subprocess.run(
            ["git", "init", "-q", "-b", "main", str(self.repo)],
            cwd=self.server.path,
            env=self.env,
            check=True,
        )
        (self.repo / "calc.py").write_text(
            "def add(a, b):\n    return a + b\n",
            encoding="utf-8",
        )
        subprocess.run(["git", "add", "-A"], cwd=self.repo, env=self.env, check=True)
        subprocess.run(
            ["git", "commit", "-q", "-m", "initial commit"],
            cwd=self.repo,
            env=self.env,
            check=True,
        )

    def top_cli(self, *args, check=True):
        res = subprocess.run(
            [str(AGENT), *args],
            env=self.env,
            text=True,
            capture_output=True,
            timeout=60,
        )
        if check:
            self.assertEqual(res.returncode, 0, f"cmd={args}\nstdout={res.stdout}\nstderr={res.stderr}")
            return json.loads(res.stdout)
        return res

    def test_3a_permission_firewall_policy_lifecycle_and_blast_radius(self):
        sock = str(self.server.socket)

        # Add a low-risk auto_approve rule for cargo test / pytest
        added = self.top_cli(
            "policy",
            "add",
            "--id",
            "allow-tests",
            "--scope",
            "global",
            "--max-risk",
            "low",
            "--action",
            "auto_approve",
            "--match-command",
            "cargo test*",
            "--socket",
            sock,
        )
        self.assertTrue(added["saved"])
        self.assertEqual(added["rule"]["id"], "allow-tests")

        listed = self.top_cli("policy", "list", "--socket", sock)
        self.assertTrue(any(r["id"] == "allow-tests" and r["enabled"] for r in listed["rules"]))

        # Low-risk non-truncated command matches and would auto-approve.
        low_chk = self.top_cli(
            "policy",
            "check",
            "cargo",
            "test",
            "--locked",
            "--cwd",
            str(self.repo),
            "--socket",
            sock,
        )
        self.assertEqual(low_chk["assessment"]["tier"], "low")
        self.assertFalse(low_chk["assessment"]["truncated"])
        self.assertTrue(low_chk["would_auto_approve"])

        # D-10 Guard #2: Visually truncated command NEVER auto-approves even if Low.
        trunc_chk = self.top_cli(
            "policy",
            "check",
            "cargo test --workspace…",
            "--cwd",
            str(self.repo),
            "--socket",
            sock,
        )
        self.assertTrue(trunc_chk["assessment"]["truncated"])
        self.assertFalse(trunc_chk["would_auto_approve"])

        # D-10 Guard #1: Critical/High risk commands are NEVER auto-approved.
        crit_chk = self.top_cli(
            "policy",
            "check",
            "cargo test && rm -rf /",
            "--cwd",
            str(self.repo),
            "--socket",
            sock,
        )
        self.assertEqual(crit_chk["assessment"]["tier"], "critical")
        self.assertFalse(crit_chk["would_auto_approve"])

        ssh_chk = self.top_cli(
            "policy",
            "check",
            "cat ~/.ssh/id_rsa",
            "--cwd",
            str(self.repo),
            "--socket",
            sock,
        )
        self.assertEqual(ssh_chk["assessment"]["tier"], "critical")
        self.assertFalse(ssh_chk["would_auto_approve"])

        # 1-tap revoke disables the rule immediately so subsequent checks do not auto-approve.
        revoked = self.top_cli("policy", "revoke", "allow-tests", "--socket", sock)
        self.assertTrue(revoked["revoked"])

        after_revoke = self.top_cli(
            "policy",
            "check",
            "cargo",
            "test",
            "--cwd",
            str(self.repo),
            "--socket",
            sock,
        )
        self.assertFalse(after_revoke["would_auto_approve"])

        # Audit log & remove work cleanly.
        log_out = self.top_cli("policy", "log", "--socket", sock)
        self.assertIn("entries", log_out)
        removed = self.top_cli("policy", "remove", "allow-tests", "--socket", sock)
        self.assertTrue(removed["removed"])

    def test_3a_os_sandbox_default_on_badge_trust_flow_and_doctor(self):
        # 1. `sandbox status` reports platform backend honestly.
        st = self.top_cli("sandbox", "status", "--repo", str(self.repo))
        self.assertIn("sandbox", st)
        if platform.system() == "Darwin":
            self.assertTrue(st["sandbox"]["available"])
            self.assertEqual(st["sandbox"]["backend"], "seatbelt")

        # 2. Default-ON sandbox stamps `@masil-sandbox` pane option on agent start,
        # while `--no-sandbox` explicitly stamps `unsandboxed:user_disabled`.
        started_default = self.cli("start", "sbx-on", "codex", "--cwd", str(self.repo))
        pane_on = started_default["pane_id"]
        badge_on = self.server.text(
            "display-message", "-p", "-t", pane_on, "#{@masil-sandbox}"
        ).strip()
        if platform.system() == "Darwin":
            self.assertTrue(
                badge_on.startswith("sandboxed:seatbelt:worktree"),
                f"expected sandboxed badge, got {badge_on!r}",
            )
        else:
            self.assertTrue(
                badge_on.startswith("sandboxed:") or badge_on == "unsandboxed:backend_missing",
                f"unexpected badge {badge_on!r}",
            )

        started_off = self.cli(
            "start", "sbx-off", "codex", "--cwd", str(self.repo), "--no-sandbox"
        )
        pane_off = started_off["pane_id"]
        badge_off = self.server.text(
            "display-message", "-p", "-t", pane_off, "#{@masil-sandbox}"
        ).strip()
        self.assertEqual(badge_off, "unsandboxed:user_disabled")

        # 3. D-11: `.masil/sandbox.toml` 2-step SHA-256 trust flow.
        masil_dir = self.repo / ".masil"
        masil_dir.mkdir(exist_ok=True)
        (masil_dir / "sandbox.toml").write_text(
            'allow_write = ["./build-cache"]\n', encoding="utf-8"
        )

        # Untrusted before user confirms digest.
        st_untrusted = self.top_cli("sandbox", "status", "--repo", str(self.repo))
        self.assertTrue(st_untrusted["repo"]["has_repo_sandbox_file"])
        self.assertFalse(st_untrusted["repo"]["trusted"])

        # Step 1: review_required returns SHA-256 digest.
        review = self.top_cli("sandbox", "trust", str(self.repo))
        self.assertEqual(review["stage"], "review_required")
        digest = review["sha256"]
        self.assertEqual(len(digest), 64)

        # Wrong digest is rejected.
        bad = self.top_cli(
            "sandbox", "trust", str(self.repo), "--confirm", "0" * 64, check=False
        )
        self.assertNotEqual(bad.returncode, 0)

        # Step 2: confirming exact SHA-256 trusts the repo file.
        trusted = self.top_cli("sandbox", "trust", str(self.repo), "--confirm", digest)
        self.assertEqual(trusted["stage"], "trusted")

        st_trusted = self.top_cli("sandbox", "status", "--repo", str(self.repo))
        self.assertTrue(st_trusted["repo"]["trusted"])

        # Modifying `.masil/sandbox.toml` invalidates trust automatically!
        (masil_dir / "sandbox.toml").write_text(
            'allow_write = ["./build-cache", "./other"]\n', encoding="utf-8"
        )
        st_tampered = self.top_cli("sandbox", "status", "--repo", str(self.repo))
        self.assertFalse(st_tampered["repo"]["trusted"])

        untrusted = self.top_cli("sandbox", "untrust", str(self.repo))
        self.assertEqual(untrusted["stage"], "untrusted")
        self.assertTrue(untrusted["removed"])

        # 4. `masil-agent doctor` includes `sandbox.backend` check.
        doc = self.top_cli("doctor", "--socket", str(self.server.socket))
        checks = doc.get("checks", [])
        self.assertTrue(any(c.get("id") == "sandbox.backend" for c in checks), checks)

    def test_3b_arena_race_verify_safe_squash_branch_promote_and_synthesize(self):
        sock = str(self.server.socket)

        # Start Race 1 (squash + archive losers + dirty base checkpoint protection).
        race1 = self.top_cli(
            "arena",
            "start",
            "Implement multiply helper",
            "--repo",
            str(self.repo),
            "--racers",
            "claude,codex",
            "--verify-cmd",
            "python3 -c 'import calc; assert calc.mul(3, 4) == 12'",
            "--socket",
            sock,
            "--no-launch",
        )
        arena1_id = race1["arena"]["id"]
        self.assertEqual(len(race1["racers"]), 2)
        wt_by_name = {r["racer_name"]: Path(r["worktree_id"]) for r in race1["racers"]}

        # Claude produces passing implementation; Codex produces buggy implementation.
        (wt_by_name["claude"] / "calc.py").write_text(
            "def add(a, b):\n    return a + b\n\ndef mul(a, b):\n    return a * b\n",
            encoding="utf-8",
        )
        (wt_by_name["codex"] / "calc.py").write_text(
            "def add(a, b):\n    return a + b\n\ndef mul(a, b):\n    return a + b\n",
            encoding="utf-8",
        )

        # Verify race 1 and inspect Masil UI Spec v1 scorecard.
        ver = self.top_cli("arena", "verify", arena1_id, "--socket", sock)
        self.assertEqual(ver["scorecard"]["v"], 1)
        by_name = {r["racer_name"]: r for r in ver["racers"]}
        self.assertEqual(by_name["claude"]["verify_exit_code"], 0)
        self.assertNotEqual(by_name["codex"]["verify_exit_code"], 0)

        # Synthesize prompt combines winner + alternative diff.
        synth = self.top_cli(
            "arena",
            "synthesize",
            arena1_id,
            "--judge",
            "claude",
            "--winner",
            "claude",
            "--socket",
            sock,
            "--no-launch",
        )
        self.assertIn("WINNER DIFF", synth["prompt"])
        self.assertIn("ALTERNATIVE RACER: codex", synth["prompt"])

        # Dirty the base repository before squash promote -> D-12 safety checkpoint MUST be taken.
        (self.repo / "dirty.txt").write_text("unsaved local work\n", encoding="utf-8")
        promoted1 = self.top_cli(
            "arena",
            "promote",
            arena1_id,
            "--winner",
            "claude",
            "--mode",
            "squash",
            "--losers",
            "archive",
            "--socket",
            sock,
        )
        self.assertEqual(promoted1["stage"], "promoted")
        self.assertEqual(promoted1["mode"], "squash")
        self.assertIsNotNone(promoted1["safety_checkpoint_id"])
        self.assertIn("def mul(a, b):", (self.repo / "calc.py").read_text(encoding="utf-8"))
        # Archived loser worktree stays on disk.
        self.assertTrue(wt_by_name["codex"].exists())

        # Start Race 2 to verify `--mode branch --losers clean`.
        (self.repo / "dirty.txt").unlink()
        subprocess.run(["git", "checkout", "--", "calc.py"], cwd=self.repo, env=self.env, check=True)

        race2 = self.top_cli(
            "arena",
            "start",
            "Add subtract helper",
            "--repo",
            str(self.repo),
            "--racers",
            "claude,gemini",
            "--socket",
            sock,
            "--no-launch",
        )
        arena2_id = race2["arena"]["id"]
        wt2 = {r["racer_name"]: Path(r["worktree_id"]) for r in race2["racers"]}
        (wt2["claude"] / "calc.py").write_text(
            "def add(a, b):\n    return a + b\n\ndef sub(a, b):\n    return a - b\n",
            encoding="utf-8",
        )
        promoted2 = self.top_cli(
            "arena",
            "promote",
            arena2_id,
            "--winner",
            "claude",
            "--mode",
            "branch",
            "--losers",
            "clean",
            "--socket",
            sock,
        )
        self.assertEqual(promoted2["mode"], "branch")
        self.assertEqual(promoted2["promoted_branch"], f"arena/{arena2_id}-winner")
        # Cleaned loser worktree is removed.
        self.assertFalse(wt2["gemini"].exists())
        # Arena list includes both arenas.
        listed = self.top_cli("arena", "list", "--socket", sock)
        ids = {item["arena"]["id"] for item in listed["arenas"]}
        self.assertIn(arena1_id, ids)
        self.assertIn(arena2_id, ids)

    def test_3b_time_travel_dvr_restore_fork_and_redacted_html_trace_export(self):
        # Launch an agent in self.repo (no-sandbox not needed, worktree is writable).
        self.cli("start", "dvr-worker", "codex", "--cwd", str(self.repo))

        # Turn 1: clean baseline with a secret string in note to test redaction on export.
        snap1 = self.cli(
            "dvr",
            "snapshot",
            "dvr-worker",
            "--note",
            "baseline clean state",
        )
        self.assertEqual(snap1["stage"], "recorded")
        self.assertEqual(snap1["turn"]["turn_seq"], 1)

        # Pin Turn 1 permanently.
        pinned = self.cli(
            "dvr",
            "pin",
            "dvr-worker",
            "--turn",
            "1",
            "--note",
            "golden-baseline",
        )
        self.assertTrue(pinned["pinned"])

        # Turn 2: mutate calc.py AND simulate a Bash side-effect stray file with secrets.
        (self.repo / "calc.py").write_text(
            "def add(a, b):\n    return a - b  # BUG\n",
            encoding="utf-8",
        )
        stray_file = self.repo / "bash_generated_artifact.txt"
        stray_file.write_text(
            f"home={os.environ.get('HOME', '/Users/test')}\n"
            "OPENAI_API_KEY=sk-proj-secret1234567890abcdefghijklmnop\n"
            "GH_TOKEN=ghp_1234567890abcdefghijklmnopqrstuvwxyz\n",
            encoding="utf-8",
        )
        snap2 = self.cli(
            "dvr",
            "snapshot",
            "dvr-worker",
            "--note",
            "bad turn with sk-proj-secret1234567890abcdefghijklmnop",
        )
        self.assertEqual(snap2["turn"]["turn_seq"], 2)

        # List and inspect Turn 2 diff.
        listed = self.cli("dvr", "list", "dvr-worker")
        self.assertEqual(len(listed["turns"]), 2)
        insp = self.cli("dvr", "inspect", "dvr-worker", "--turn", "2")
        self.assertIn("BUG", insp["diff"])

        # Export zero-CDN secret-redacted HTML PR Verification Trace BEFORE restore.
        trace_path = self.server.path / "pr-trace.html"
        exported = self.cli(
            "trace",
            "export",
            "dvr-worker",
            "--out",
            str(trace_path),
            "--redact-strict",
        )
        self.assertEqual(exported["stage"], "exported")
        self.assertGreaterEqual(exported["turn_count"], 2)
        self.assertGreater(exported["total_redactions"], 0)
        html = trace_path.read_text(encoding="utf-8")
        self.assertIn("<!DOCTYPE html>", html)
        self.assertNotIn("<script src=", html)
        self.assertNotIn("https://cdn", html)
        self.assertNotIn("sk-proj-secret1234567890abcdefghijklmnop", html)
        self.assertNotIn("ghp_1234567890abcdefghijklmnopqrstuvwxyz", html)

        # Restore Turn 1 -> reverts calc.py AND removes Bash side-effect stray file!
        restored = self.cli("dvr", "restore", "dvr-worker", "--turn", "1")
        self.assertEqual(restored["stage"], "restored")
        self.assertIn("return a + b", (self.repo / "calc.py").read_text(encoding="utf-8"))
        self.assertFalse(stray_file.exists(), "DVR restore must undo Bash-created stray files")

        # Fork Turn 2 into an isolated git worktree -> preserves Turn 2 contents independently.
        fork_dest = self.server.path / "dvr-fork-turn2"
        forked = self.cli(
            "dvr",
            "fork",
            "dvr-worker",
            "--turn",
            "2",
            "--dest",
            str(fork_dest),
        )
        self.assertEqual(forked["stage"], "forked")
        self.assertTrue((fork_dest / "bash_generated_artifact.txt").exists())
        self.assertIn("BUG", (fork_dest / "calc.py").read_text(encoding="utf-8"))


for _name in dir(test_managed_agents.ManagedAgents):
    if _name.startswith("test_") and _name not in Phase3Integration.__dict__:
        setattr(Phase3Integration, _name, None)


if __name__ == "__main__":
    unittest.main()
