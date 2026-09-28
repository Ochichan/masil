#!/usr/bin/env python3
"""Shared acknowledgements and bounded complete-state watch integration."""
import json
import select
import socket
import struct
import subprocess
import time
import unittest

from test_agentd import AGENT, AgentdHarness, manager_query
from test_compatibility import wait_for


class WatchPeer:
    def __init__(self, path, receive_buffer=None):
        self.socket = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.socket.settimeout(4)
        if receive_buffer:
            self.socket.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, receive_buffer)
        self.socket.connect(str(path))
        body = json.dumps({"v": 1, "kind": "watch-agents", "request_id": "watch-agents"}).encode()
        self.socket.sendall(struct.pack(">I", len(body)) + body)

    def close(self):
        self.socket.close()

    def receive(self):
        def exact(n):
            result = bytearray()
            while len(result) < n:
                data = self.socket.recv(n - len(result))
                if not data:
                    raise EOFError("projection watch closed")
                result.extend(data)
            return result
        size, = struct.unpack(">I", exact(4))
        if not 0 < size <= 65536:
            raise AssertionError(f"invalid frame size {size}")
        return json.loads(exact(size))

    def ready(self, timeout=.1):
        return bool(select.select([self.socket], [], [], timeout)[0])

    def until(self, predicate):
        end = time.monotonic() + 5
        while time.monotonic() < end:
            if self.ready():
                value = self.receive()
                if predicate(value):
                    return value
        raise AssertionError("expected projection did not arrive")


class AttentionIntegration(AgentdHarness):
    def pending(self, count=1):
        self.provider.permissions = [{"id": f"per_{i:03}", "sessionID": "ses_0"} for i in range(count)]

    def ack(self, row, epoch=None):
        return self.query("ack", id=row["id"], epoch=epoch or self.query("status")["epoch"],
                          revision=row["attention"]["revision"])

    def peer(self, receive_buffer=None):
        peer = WatchPeer(self.manager, receive_buffer)
        self.addCleanup(peer.close)
        return peer

    def test_shared_ack_is_precise_idempotent_and_not_provider_approval(self):
        self.pending(20)
        self.start()
        self.fresh()
        before = self.query("attention")
        self.assertEqual([r["id"] for r in before["items"]], ["agent-0"])
        row = before["items"][0]
        result = self.ack(row, before["epoch"])
        self.assertEqual(result["kind"], "ack", result)
        self.assertTrue(result["observation"]["attention"]["acknowledged"])
        # Every query opens a new client, so this asserts shared daemon state.
        self.assertEqual(self.query("attention")["items"], [])
        shown = self.query("attention", all=True)["items"][0]
        self.assertEqual(shown["native"]["attention"], "approval")
        self.assertTrue(shown["attention"]["acknowledged"])
        self.assertEqual(self.ack(row)["revision"], result["revision"], "idempotent ack changed projection")
        self.provider.permissions.reverse()
        self.provider.event()
        self.fresh()
        wait_for(lambda: self.row()["native"]["reconciliations"] >= 2)
        self.assertTrue(self.row()["attention"]["acknowledged"])
        # Replace the last ID while keeping the visible first 16 and count equal.
        self.provider.permissions.sort(key=lambda request: request["id"])
        self.provider.permissions[-1]["id"] = "per_replacement"
        self.provider.event("permission.asked")
        wait_for(lambda: self.row()["attention"]["revision"] != row["attention"]["revision"], 5)
        self.fresh()
        self.assertFalse(self.row()["attention"]["acknowledged"])
        self.assertEqual(self.ack(row)["code"], "stale_revision")
        self.assertEqual(self.query("attention")["items"][0]["native"]["permission_count"], 20)
        self.assertEqual(len(self.provider.permissions), 20)
        self.assertTrue(all(method == "GET" for method, _, _ in self.provider.requests))

    def test_unknown_and_restart_epochs_cannot_hide_or_ack_pending_work(self):
        self.pending()
        self.start()
        self.fresh()
        old_epoch = self.query("status")["epoch"]
        row = self.row()
        self.ack(row)
        self.provider.failure = "auth"
        self.provider.event()
        wait_for(lambda: self.row()["native"]["freshness"] != "fresh")
        uncertain = self.query("attention")
        self.assertEqual(uncertain["items"], [])
        self.assertEqual(len(uncertain["unavailable"]), 2)
        self.assertEqual(self.ack(row)["code"], "observation_unavailable")
        self.provider.failure = None
        self.fresh()
        self.assertFalse(self.row()["attention"]["acknowledged"])
        self.assertNotEqual(self.row()["attention"]["revision"], row["attention"]["revision"])
        self.query("stop")
        self.child.communicate(timeout=5)
        self.start()
        self.fresh()
        new_row = self.row()
        self.assertNotEqual(self.query("status")["epoch"], old_epoch)
        self.assertEqual(self.ack(new_row, old_epoch)["code"], "wrong_epoch")
        self.assertEqual(len(self.query("attention")["items"]), 1)

    def test_complete_stream_ack_updates_and_no_screen_dirty_or_idle_noise(self):
        self.pending()
        self.start()
        self.fresh()
        peer = self.peer()
        initial = peer.receive()
        self.assertEqual(initial["kind"], "agents_snapshot")
        self.assertTrue(initial["complete"])
        self.assertEqual(len(initial["observations"]), 2)
        self.assertFalse(peer.ready(.15))
        row = self.row()
        self.ack(row)
        changed = peer.until(lambda frame: frame["observations"][0]["attention"]["acknowledged"])
        self.assertEqual(changed["epoch"], initial["epoch"])
        self.assertGreater(int(changed["revision"]), int(initial["revision"]))
        self.core.run("send-keys", "-t", "%0", "screen-only", "Enter")
        self.assertFalse(peer.ready(.45), "screen bytes caused unchanged projection notification")
        self.assertTrue(self.row()["attention"]["acknowledged"])
        self.assertFalse(self.row()["frontend_verified"])

    def test_stream_capacity_reserves_queries_and_reclaims_disconnected_idle_peers(self):
        self.start()
        self.fresh()
        peers = [self.peer() for _ in range(16)]
        for peer in peers:
            self.assertEqual(peer.receive()["kind"], "agents_snapshot")
        overflow = self.peer()
        rejected = overflow.receive()
        self.assertEqual(rejected["kind"], "error")
        before = time.monotonic()
        self.assertEqual(self.query("status")["status"], "running")
        self.assertLess(time.monotonic() - before, 1)
        peers[0].close()
        time.sleep(.1)
        replacement = self.peer()
        self.assertEqual(replacement.receive()["kind"], "agents_snapshot")
        self.assertTrue(self.query("stop")["accepted"])
        self.child.wait(timeout=3)

    def test_cli_attention_ack_and_counted_stream(self):
        self.pending()
        self.start()
        self.fresh()
        def cli(*args):
            return subprocess.run([str(AGENT), "--socket", str(self.manager), *args],
                                  capture_output=True, text=True, timeout=5)
        listing = cli("attention")
        self.assertEqual(listing.returncode, 0, listing.stderr)
        payload = json.loads(listing.stdout)
        revision = payload["items"][0]["attention"]["revision"]
        ack = cli("ack", "agent-0", "--epoch", payload["epoch"], "--revision", revision)
        self.assertEqual(ack.returncode, 0, ack.stderr)
        self.assertEqual(json.loads(cli("attention").stdout)["items"], [])
        stream = cli("watch-agents", "--count", "1")
        self.assertEqual(stream.returncode, 0, stream.stderr)
        self.assertEqual(json.loads(stream.stdout)["kind"], "agents_snapshot")
        self.assertEqual(cli("watch-agents", "--count", "0").returncode, 2)
        self.assertEqual(cli("ack", "agent-0", "--epoch", payload["epoch"], "--revision", "999").returncode, 5)

    def test_maximum_scope_fits_complete_frames_and_slow_watchers_do_not_block_queries(self):
        cfg = json.loads(self.config.read_text())
        source = cfg["sources"][0]
        source["id"] = "s" * 64
        source["sessions"] = []
        self.provider.sessions = {}
        self.provider.statuses = {}
        self.provider.permissions = []
        for i in range(64):
            native = f"ses_{i:03}_" + "n" * 120
            label = f"a{i:02}" + "x" * 61
            source["sessions"].append({"id": label, "pane_id": "%0", "session_id": native})
            self.provider.sessions[native] = {"id": native, "directory": self.provider.directory}
            self.provider.permissions.append({"id": f"per_{i}", "sessionID": native})
        self.config.write_text(json.dumps(cfg))
        self.start()
        wait_for(lambda: len(self.query("attention")["items"]) == 64, 12)
        first = self.peer().receive()
        self.assertEqual(len(first["observations"]), 64)
        self.assertLessEqual(len(json.dumps(first, separators=(",", ":")).encode()), 65536)
        slow = [self.peer(1024) for _ in range(15)]
        epoch = first["epoch"]
        before = time.monotonic()
        for row in first["observations"]:
            self.assertEqual(self.ack(row, epoch)["kind"], "ack")
        self.assertLess(time.monotonic() - before, 4)
        self.assertEqual(self.query("status")["status"], "running")
        self.assertEqual(self.query("attention")["items"], [])
        # Keep all 16 original sockets open. Blocked writers must release their
        # permits by their own deadline, allowing a new subscriber to enter.
        until = time.monotonic() + 5
        while time.monotonic() < until:
            replacement = self.peer()
            result = replacement.receive()
            if result["kind"] == "agents_snapshot":
                break
            self.assertEqual(result["code"], "capacity")
            replacement.close()
            time.sleep(.05)
        else:
            self.fail("slow subscribers did not release capacity by their deadline")
        for peer in slow:
            peer.close()


if __name__ == "__main__":
    unittest.main()
