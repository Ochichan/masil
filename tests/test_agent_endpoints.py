#!/usr/bin/env python3
"""Bounded endpoint configuration, stdin RPC, and two-server aggregation."""

import json
import os
from pathlib import Path
import shutil
import shlex
import stat
import subprocess
import sys
import tempfile
import time
import unittest

from test_compatibility import MASIL, ROOT, Server, wait_for


AGENT = Path(os.environ.get("MASIL_AGENT", ROOT / "bin/masil-agent"))


class AgentEndpoints(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.fixture = Path(tempfile.mkdtemp(prefix="masil-endpoint-provider-"))
        subprocess.run(
            [
                "cc",
                "-Wall",
                "-Wextra",
                "-Werror",
                str(ROOT / "tests/faults/agent.c"),
                "-o",
                str(cls.fixture / "codex"),
            ],
            check=True,
            capture_output=True,
        )

    @classmethod
    def tearDownClass(cls):
        shutil.rmtree(cls.fixture)

    def setUp(self):
        self.local = Server(MASIL)
        self.remote = Server(MASIL)
        self.local.__enter__()
        self.addCleanup(self.local.__exit__)
        self.remote.__enter__()
        self.addCleanup(self.remote.__exit__)
        self.config = self.local.path / "config"
        masil_config = self.config / "masil"
        masil_config.mkdir(parents=True, mode=0o700)
        masil_config.chmod(0o700)
        detection = masil_config / "agent-detection"
        detection.mkdir(mode=0o700)
        (detection / "codex.toml").write_text(
            """id = "codex"
version = "1.0.0"
min_engine_version = 3
[[rules]]
id = "synthetic_blocker"
state = "blocked"
priority = 100
region = "whole_recent"
visible_blocker = true
contains = ["STATE:blocked"]
[[rules]]
id = "synthetic_work"
state = "working"
priority = 90
region = "whole_recent"
contains = ["STATE:working"]
[[rules]]
id = "synthetic_idle"
state = "idle"
priority = 80
region = "whole_recent"
visible_idle = true
contains = ["STATE:idle"]
"""
        )
        self.env = self.local.env | {
            "PATH": f"{self.fixture}:/usr/bin:/bin",
            "XDG_CONFIG_HOME": str(self.config),
        }

    def cli(self, *args, socket=None, check=True, timeout=12, input_text=None):
        command = [str(AGENT), "agent"]
        if socket is not None:
            command.extend(["--socket", str(socket)])
        command.extend(args)
        result = subprocess.run(
            command,
            env=self.env,
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

    def add_endpoint(self, endpoint="remote", *, socket=None, binary=None):
        return self.cli(
            "endpoints",
            "add",
            endpoint,
            "--socket",
            str(socket or self.remote.socket),
            "--binary",
            str(binary or AGENT.resolve()),
        )

    def rpc(self, request, *, socket=None, check=True):
        result = self.cli(
            "rpc",
            socket=socket or self.remote.socket,
            check=False,
            input_text=json.dumps(request),
        )
        if check:
            self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
            self.assertEqual(result.stderr, "")
            return json.loads(result.stdout)
        return result

    def test_private_atomic_configuration_and_disabled_refusal(self):
        self.assertEqual(self.cli("endpoints", "list")["endpoints"], [])
        added = self.add_endpoint()
        self.assertEqual(added["endpoint"]["id"], "remote")
        self.assertTrue(added["endpoint"]["enabled"])
        path = self.config / "masil/endpoints.json"
        self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)
        self.assertEqual(stat.S_IMODE(path.parent.stat().st_mode), 0o700)

        disabled = self.cli("endpoints", "disable", "remote")
        self.assertFalse(disabled["endpoint"]["enabled"])
        refused = self.cli("--endpoint", "remote", "list", check=False)
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn("disabled", refused.stderr)
        self.cli("endpoints", "enable", "remote")
        removed = self.cli("endpoints", "remove", "remote")
        self.assertEqual(removed["endpoint"]["id"], "remote")
        self.assertEqual(self.cli("endpoints", "list")["endpoints"], [])

    def test_bad_configuration_identity_and_protocol_are_rejected(self):
        path = self.config / "masil/endpoints.json"
        path.write_text('{"version":1,"endpoints":[],"unknown":true}')
        path.chmod(0o600)
        invalid = self.cli("endpoints", "list", check=False)
        self.assertNotEqual(invalid.returncode, 0)
        self.assertIn("invalid endpoint configuration", invalid.stderr)

        path.write_text('{"version":1,"endpoints":[]}')
        path.chmod(0o644)
        public = self.cli("endpoints", "list", check=False)
        self.assertNotEqual(public.returncode, 0)
        self.assertIn("private owner file", public.stderr)

        path.unlink()
        victim = self.local.path / "victim.json"
        victim.write_text('{"version":1,"endpoints":[]}')
        victim.chmod(0o600)
        path.symlink_to(victim)
        linked = self.cli("endpoints", "list", check=False)
        self.assertNotEqual(linked.returncode, 0)
        self.assertEqual(victim.read_text(), '{"version":1,"endpoints":[]}')

        path.unlink()
        malformed = self.rpc({"operation": "list", "unknown": True})
        self.assertFalse(malformed["ok"])
        self.assertIn("invalid RPC request", malformed["error"])

        started = self.cli(
            "start",
            "builder",
            "codex",
            "--cwd",
            str(self.remote.path),
            socket=self.remote.socket,
        )
        wait_for(
            lambda: self.cli("get", "builder", socket=self.remote.socket)["state"]
            == "idle",
            4,
        )
        agent = self.cli("get", "builder", socket=self.remote.socket)
        expected = {
            key: agent[key]
            for key in (
                "pane_id",
                "boot",
                "generation",
                "run",
                "revision",
                "session_id",
            )
        }
        expected["run"] = started["run"] + "-stale"
        stale = self.rpc(
            {
                "operation": "action",
                "expected": expected,
                "action": {"read": {"history": False}},
            }
        )
        self.assertFalse(stale["ok"])
        self.assertIn("changed", stale["error"])

    def test_local_transport_and_two_server_aggregation(self):
        self.add_endpoint()
        empty = self.cli("--endpoint", "remote", "list")
        self.assertEqual(empty["agents"], [])
        self.assertEqual(len(empty["boot"]), 36)

        self.cli(
            "start",
            "builder",
            "codex",
            "--cwd",
            str(self.remote.path),
            socket=self.remote.socket,
        )
        wait_for(
            lambda: self.cli("get", "builder", socket=self.remote.socket)["state"]
            == "idle",
            4,
        )
        remote = self.cli("--endpoint", "remote", "get", "builder")
        self.assertEqual(remote["name"], "builder")
        self.assertEqual(remote["boot"], empty["boot"])
        screen = self.cli("--endpoint", "remote", "read", "builder")
        self.assertIn("STATE:idle", screen["text"])

        aggregate = self.cli("list", "--all", socket=self.local.socket)
        by_id = {agent["id"]: agent for agent in aggregate["agents"]}
        self.assertIn("remote::builder", by_id)
        self.assertEqual(by_id["remote::builder"]["endpoint_id"], "remote")
        statuses = {item["id"]: item for item in aggregate["endpoints"]}
        self.assertTrue(statuses["local"]["connected"])
        self.assertTrue(statuses["remote"]["connected"])

        envelope = self.rpc({"operation": "get", "target": "builder"})
        self.assertTrue(envelope["ok"])
        self.assertEqual(envelope["value"]["run"], remote["run"])

    def test_faulting_local_transports_are_bounded(self):
        slow = self.local.path / "slow-agent"
        pids = self.local.path / "slow-agent.pids"
        slow.write_text(
            "#!/bin/sh\n"
            + "printf '%s\\n' \"$$\" > "
            + shlex.quote(str(pids))
            + "\nsleep 30 &\nprintf '%s\\n' \"$!\" >> "
            + shlex.quote(str(pids))
            + "\nwait\n"
        )
        slow.chmod(0o700)
        self.add_endpoint("slow", socket=self.remote.socket, binary=slow)
        before = time.monotonic()
        timed_out = self.cli("--endpoint", "slow", "list", check=False, timeout=12)
        elapsed = time.monotonic() - before
        self.assertNotEqual(timed_out.returncode, 0)
        self.assertIn("timed out after 8 seconds", timed_out.stderr)
        self.assertLess(elapsed, 10)
        recorded = [int(pid) for pid in pids.read_text().splitlines()]

        def processes_gone():
            for pid in recorded:
                try:
                    os.kill(pid, 0)
                except ProcessLookupError:
                    continue
                return False
            return True

        wait_for(processes_gone, 2)

        oversized = self.local.path / "oversized-agent"
        oversized.write_text(
            "#!/bin/sh\nexec "
            + shlex.quote(sys.executable)
            + " -c 'import sys; sys.stdout.write(\"x\" * (1024 * 1024 + 1))'\n"
        )
        oversized.chmod(0o700)
        self.add_endpoint("oversized", socket=self.remote.socket, binary=oversized)
        too_large = self.cli("--endpoint", "oversized", "list", check=False)
        self.assertNotEqual(too_large.returncode, 0)
        self.assertIn("output exceeds 1048576 bytes", too_large.stderr)


if __name__ == "__main__":
    unittest.main(verbosity=2)
