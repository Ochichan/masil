#!/usr/bin/env python3
"""P8b: the coordinator holds a stream to an endpoint. Reads go through it, the
endpoint's inbox merges into this one, and it comes back after a drop. It never
starts anything on the endpoint."""

import json
import os
from pathlib import Path
import signal
import subprocess
import time
import unittest

import test_agent_endpoints
from test_compatibility import wait_for

ROOT = Path(__file__).resolve().parents[1]


class RemoteLinks(test_agent_endpoints.AgentEndpoints):
    def setUp(self):
        super().setUp()
        # One state directory for both servers, as for two servers of one
        # user on one machine: each has its own store, keyed by socket.
        state = self.local.path / ".local/state"
        for server in (self.local, self.remote):
            server.run("set-environment", "-g", "XDG_STATE_HOME", str(state))
            server.run("set-environment", "-g", "XDG_CONFIG_HOME", str(self.config))
            server.run("set-environment", "-g", "PATH", self.env["PATH"])
        self.env["XDG_STATE_HOME"] = str(state)
        self.env["MASIL_AGENTD_WATCH_MS"] = "200"
        self.env["MASIL_AGENTD_IDLE_SECONDS"] = "2"
        self.log = self.local.path / "rpc.log"
        self.wrapper = self.local.path / "logging-agent"
        self.wrapper.write_text(
            f'#!/bin/sh\necho "$*" >> {self.log}\nexec {test_agent_endpoints.AGENT.resolve()} "$@"\n')
        self.wrapper.chmod(0o755)
        for server in (self.local, self.remote):
            self.addCleanup(lambda server=server: self.cli("coordinator", "stop", socket=server.socket,
                                                             check=False))

    def link(self, endpoint="remote"):
        links = self.cli("endpoints", "status", socket=self.local.socket)["links"]
        return next((link for link in links if link["endpoint"] == endpoint), {})

    def calls(self):
        """One-shot RPC calls the endpoint received (not the stream)."""
        if not self.log.exists():
            return 0
        return sum(1 for line in self.log.read_text().splitlines() if line.endswith(" rpc"))

    def streams(self):
        found = subprocess.run(["pgrep", "-f", f"{self.remote.socket} rpc --stream"],
                               capture_output=True, text=True)
        return [int(pid) for pid in found.stdout.split()]

    def start_remote(self, name="builder"):
        self.cli("start", name, "codex", "--cwd", str(self.remote.path), socket=self.remote.socket)
        wait_for(lambda: self.cli("get", name, socket=self.remote.socket)["state"] == "idle", 4)
        return self.cli("get", name, socket=self.remote.socket)

    def local_events(self, all=False):
        args = ["inbox", "--all"] if all else ["inbox"]
        return [event for event in self.cli(*args, socket=self.local.socket)["events"]
                if event["source"] == "endpoint:remote"]

    def test_reads_go_over_the_link_and_the_inbox_merges(self):
        self.add_endpoint(binary=self.wrapper)
        self.cli("inbox", "enable", socket=self.remote.socket)
        self.cli("inbox", "enable", socket=self.local.socket)
        connected = self.cli("endpoints", "connect", "remote", socket=self.local.socket)
        self.assertEqual((connected["desired"], connected["coordinator"]), ("connected", "running"))
        wait_for(lambda: self.link().get("state") == "connected", 10)
        agent = self.start_remote()
        before = self.calls()
        listed = self.cli("list", "--all", socket=self.local.socket)
        self.assertIn("remote::builder", [item["id"] for item in listed["agents"]])
        self.assertEqual(self.calls(), before)
        # The endpoint's watch records the block; the link brings it here.
        self.remote.run("send-keys", "-t", agent["pane_id"], "blocked", "Enter")
        wait_for(lambda: [event["kind"] for event in self.local_events()] == ["blocked"], 20)
        [event] = self.local_events()
        self.assertEqual((event["pane"], event["run"]), (f"remote::{agent['pane_id']}", agent["run"]))
        # Notifications name it as the endpoint's agent.
        self.assertEqual(event["summary"]["endpoint_agent"], "remote::builder")
        self.remote.run("send-keys", "-t", agent["pane_id"], "idle", "Enter")
        wait_for(lambda: self.local_events() == [], 20)
        [settled] = [event for event in self.local_events(all=True) if event["kind"] == "blocked"]
        self.assertEqual(settled["resolution"], "remote_resolved")

    def test_disconnect_ends_the_stream_and_starts_nothing_there(self):
        self.add_endpoint()
        self.cli("endpoints", "connect", "remote", socket=self.local.socket)
        wait_for(lambda: self.link().get("state") == "connected", 10)
        self.assertEqual(len(self.streams()), 1)
        # Linking reads the endpoint; it starts no coordinator there.
        self.assertEqual(self.cli("coordinator", "status", socket=self.remote.socket)["state"],
                         "not_running")
        self.assertEqual(self.link()["remote"]["coordinator"], "not_running")
        stopped = self.cli("endpoints", "disconnect", "remote", socket=self.local.socket)
        self.assertEqual(stopped["desired"], "stopped")
        wait_for(lambda: self.streams() == [], 10)
        self.assertEqual(self.link()["state"], "stopped")

    def test_a_dropped_stream_comes_back(self):
        self.add_endpoint()
        self.cli("endpoints", "connect", "remote", socket=self.local.socket)
        wait_for(lambda: self.link().get("state") == "connected", 10)
        [pid] = self.streams()
        os.kill(pid, signal.SIGKILL)
        wait_for(lambda: self.link().get("state") in ("backoff", "connecting"), 5)
        wait_for(lambda: self.link().get("state") == "connected" and self.streams() not in ([], [pid]), 10)

    def test_two_servers_linked_both_ways_do_not_echo(self):
        self.add_endpoint()
        self.add_endpoint("back", socket=self.local.socket)
        for server in (self.local, self.remote):
            self.cli("inbox", "enable", socket=server.socket)
        self.cli("endpoints", "connect", "remote", socket=self.local.socket)
        self.cli("endpoints", "connect", "back", socket=self.remote.socket)
        wait_for(lambda: self.link().get("state") == "connected", 10)
        agent = self.start_remote()
        self.remote.run("send-keys", "-t", agent["pane_id"], "blocked", "Enter")
        wait_for(lambda: len(self.local_events()) == 1, 20)
        # Three more polls each way: what came from there does not go back.
        time.sleep(16)
        self.assertEqual(len(self.local_events(all=True)), 1)
        mirrored = [event for event in self.cli("inbox", "--all", socket=self.remote.socket)["events"]
                    if event["source"].startswith("endpoint:")]
        self.assertEqual(mirrored, [])

    def test_reads_through_links_do_not_keep_an_idle_coordinator(self):
        self.add_endpoint()
        self.cli("endpoints", "connect", "remote", socket=self.local.socket)
        wait_for(lambda: self.link().get("state") == "connected", 10)
        self.cli("endpoints", "disconnect", "remote", socket=self.local.socket)
        # A desk's polling: reads that ask the coordinator first. (A status
        # request would itself be activity; the process is looked at.)
        def running():
            found = subprocess.run(["pgrep", "-f", f"agentd --socket {self.local.socket.resolve()}"],
                                   capture_output=True, text=True)
            return bool(found.stdout.split())
        self.assertTrue(running())
        deadline = time.monotonic() + 15
        while running():
            self.assertLess(time.monotonic(), deadline)
            self.cli("list", "--all", socket=self.local.socket)
            time.sleep(1)

    def test_an_older_endpoint_and_a_link_to_itself_are_not_retried(self):
        old = ROOT / ".build/perf-baseline-p6/masil-agent"
        if not old.exists():
            self.skipTest("no earlier masil-agent at .build/perf-baseline-p6")
        # Its engine predates interrupt keys, and it shares this
        # configuration: the manifest without them.
        manifest = self.config / "masil/agent-detection/codex.toml"
        manifest.write_text(manifest.read_text().replace("min_engine_version = 4\ninterrupt_keys = [\"Escape\"]\n",
                                                         "min_engine_version = 3\n")
                            .replace("interruptible = true\n", ""))
        self.add_endpoint("old", binary=old)
        self.add_endpoint("itself", socket=self.local.socket)
        for endpoint in ("old", "itself"):
            self.cli("endpoints", "connect", endpoint, socket=self.local.socket)
        wait_for(lambda: self.link("old").get("state") == "version_mismatch", 10)
        wait_for(lambda: self.link("itself").get("state") == "self_link", 10)
        # Calls of their own still work for the older one.
        self.assertEqual(self.cli("--endpoint", "old", "list")["agents"], [])


for name in dir(test_agent_endpoints.AgentEndpoints):
    if name.startswith("test_"):
        setattr(RemoteLinks, name, None)
del name


if __name__ == "__main__":
    unittest.main()
