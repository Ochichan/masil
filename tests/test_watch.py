#!/usr/bin/env python3
"""Exercise bounded watch streams through real private core sockets."""
from __future__ import annotations

import json
from pathlib import Path
import select
import socket
import struct
import subprocess
import threading
import time
import unittest

from test_compatibility import RMUX, Server, wait_for


class Peer:
    def __init__(self, path: Path, receive_buffer: int | None = None):
        self.socket = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.socket.settimeout(3)
        if receive_buffer:
            self.socket.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, receive_buffer)
        self.socket.connect(str(path))
        self.hello = self.query("hello")

    def close(self):
        self.socket.close()

    def exact(self, count):
        result = bytearray()
        while len(result) < count:
            part = self.socket.recv(count - len(result))
            if not part:
                raise EOFError("watch connection ended")
            result.extend(part)
        return bytes(result)

    def receive(self):
        length, = struct.unpack(">I", self.exact(4))
        if not 0 < length <= 65536:
            raise AssertionError(f"bad response length: {length}")
        return json.loads(self.exact(length))

    def query(self, kind, **fields):
        body = json.dumps({"v": 1, "kind": kind, "request_id": kind, **fields}).encode()
        self.socket.sendall(struct.pack(">I", len(body)) + body)
        return self.receive()

    def watch(self, panes, **overrides):
        fields = {"pane_ids": panes, "expected_core_boot_id": self.hello["core_boot_id"]}
        fields.update(overrides)
        return self.query("watch", **fields)

    def ready(self, timeout=0):
        return bool(select.select([self.socket], [], [], timeout)[0])

    def until(self, predicate, timeout=3):
        end = time.monotonic() + timeout
        received = []
        while time.monotonic() < end:
            if not self.ready(min(.1, max(0, end - time.monotonic()))):
                continue
            message = self.receive()
            received.append(message)
            if predicate(message):
                return message, received
        raise AssertionError(f"event not observed: {received}")


class WatchIntegration(unittest.TestCase):
    def setUp(self):
        self.server = Server(RMUX, command=["/bin/cat"])
        self.bridge = self.server.path / "observe.sock"
        self.server.env["RMUX_BRIDGE_SOCKET"] = str(self.bridge)
        self.server.__enter__()
        self.addCleanup(self.server.__exit__)
        wait_for(self.bridge.exists)
        self.pane = self.empty_pane()

    def empty_pane(self):
        return self.server.text("new-window", "-d", "-E", "-P", "-F", "#{pane_id}").strip()

    def peer(self, receive_buffer=None):
        peer = Peer(self.bridge, receive_buffer)
        self.addCleanup(peer.close)
        return peer

    def stats(self):
        peer = Peer(self.bridge)
        try:
            return peer.query("stats")
        finally:
            peer.close()

    def inject(self, pane, data):
        result = subprocess.run(
            [self.server.binary, "-S", str(self.server.socket), "display-message", "-I", "-t", pane],
            env=self.server.env, input=data, text=True, capture_output=True, timeout=5,
        )
        self.assertEqual(result.returncode, 0, result.stderr)

    def script(self, commands):
        path = self.server.path / "commands.conf"
        path.write_text("\n".join(commands) + "\n")
        return self.server.run("source-file", str(path)).stdout

    def test_atomic_admission_and_idle_cleanup(self):
        peer = self.peer()
        self.assertTrue(peer.hello["capabilities"]["watch"])
        for ids, extra, code in [
            ([], {}, "invalid_pane_ids"),
            ([self.pane, self.pane], {}, "duplicate_pane_id"),
            ([self.pane, "%4294967295"], {}, "target_gone"),
            ([self.pane], {"expected_core_boot_id": "wrong"}, "boot_mismatch"),
            ([f"%{n}" for n in range(65)], {}, "invalid_pane_ids"),
        ]:
            with self.subTest(code=code):
                response = peer.watch(ids, **extra)
                self.assertEqual(response.get("code"), code, response)
                self.assertEqual(self.stats()["watched_panes"], 0)
        ack = peer.watch([self.pane])
        self.assertEqual(ack["kind"], "watch")
        self.assertTrue(ack["complete"])
        self.assertEqual([p["pane_id"] for p in ack["panes"]], [self.pane])
        self.assertFalse(peer.ready(.4), "unchanged pane emitted an event")
        stats = self.stats()
        self.assertEqual(stats["watched_panes"], 1)
        self.assertEqual(stats["watch_subscriptions"], 1)
        self.assertEqual(stats["pending_dirty"], 0)
        self.assertFalse(stats["flush_timer_active"])
        peer.close()
        wait_for(lambda: self.stats()["watched_panes"] == 0)
        self.assertEqual(self.stats()["watch_subscriptions"], 0)

    def test_scope_fence_and_coalesced_output(self):
        peer = self.peer()
        ack = peer.watch([self.pane])
        other = self.empty_pane()
        self.inject(other, "unwatched\n")
        self.assertFalse(peer.ready(.35), "unwatched pane leaked into stream")
        for n in range(12):
            self.inject(self.pane, f"output-{n}\n")
        latest = self.server.text("capture-pane", "-p", "-t", self.pane)
        self.assertIn("output-11", latest)
        _, events = peer.until(lambda m: m.get("reason") == "screen_dirty")
        time.sleep(.35)
        while peer.ready(.02):
            events.append(peer.receive())
        self.assertLess(len(events), 12, "output was not coalesced")
        previous = int(ack["fence_seq"])
        for event in events:
            self.assertEqual(event["kind"], "event")
            self.assertEqual(event["pane_id"], self.pane)
            self.assertEqual(event["core_boot_id"], ack["core_boot_id"])
            self.assertEqual(event["stream_epoch"], ack["stream_epoch"])
            self.assertGreater(int(event["event_seq"]), previous)
            previous = int(event["event_seq"])
        later = self.peer()
        baseline = later.watch([self.pane])
        self.assertGreaterEqual(int(baseline["fence_seq"]), previous)
        self.assertNotEqual(baseline["stream_epoch"], ack["stream_epoch"])
        self.assertFalse(later.ready(.3), "old events replayed across a new watch")
        self.assertGreater(int(self.stats()["counters"]["coalesced"]), 0)
        self.assertFalse(self.stats()["flush_timer_active"])

    def test_filtered_journal_scan_continues_without_more_native_output(self):
        noisy = self.peer()
        self.assertEqual(noisy.watch([self.pane])["kind"], "watch")
        quiet_pane = self.empty_pane()
        quiet = self.peer()
        ack = quiet.watch([quiet_pane])
        commands = [f"resize-window -t {self.pane} -x {90 + i % 2}" for i in range(1000)]
        commands.append(f"resize-window -t {quiet_pane} -x 95")
        self.script(commands)
        event, _ = quiet.until(lambda m: m.get("reason") == "resized")
        self.assertEqual(event["pane_id"], quiet_pane)
        self.assertGreater(int(event["event_seq"]), int(ack["fence_seq"]) + 256)
        self.assertFalse(quiet.ready(.2))

    def test_rust_cli_receives_real_core_event(self):
        agent = RMUX.with_name("rmux-agent")
        child = subprocess.Popen(
            [str(agent), "--socket", str(self.bridge), "watch", "--count", "1", self.pane],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        )
        try:
            self.assertTrue(select.select([child.stdout], [], [], 3)[0], "CLI did not flush ACK")
            ack = json.loads(child.stdout.readline())
            self.assertEqual(ack["kind"], "watch")
            self.inject(self.pane, "live-rust-watch\n")
            output, errors = child.communicate(timeout=4)
            self.assertEqual(child.returncode, 0, errors)
            event = json.loads(output)
            self.assertEqual(event["kind"], "event")
            self.assertEqual(event["reason"], "screen_dirty")
            self.assertEqual(event["pane_id"], self.pane)
            self.assertEqual(event["stream_epoch"], ack["stream_epoch"])
        finally:
            if child.poll() is None:
                child.terminate()
                child.communicate(timeout=3)
        wait_for(lambda: self.stats()["watched_panes"] == 0)

    def test_lifecycle_and_deleted_dirty_slot(self):
        pane = self.server.text("new-window", "-d", "-P", "-F", "#{pane_id}", "/bin/cat").strip()
        self.server.run("set-window-option", "-t", pane, "remain-on-exit", "on")
        peer = self.peer()
        ack = peer.watch([pane, self.pane])
        self.server.run("resize-window", "-t", pane, "-x", "90", "-y", "30")
        resized, _ = peer.until(lambda m: m.get("reason") == "resized" and m.get("pane_id") == pane)
        self.server.run("respawn-pane", "-k", "-t", pane, "/bin/cat")
        changed, _ = peer.until(lambda m: m.get("reason") == "pty_changed")
        old = next(p for p in ack["panes"] if p["pane_id"] == pane)
        self.assertGreater(int(changed["pty_generation"]), int(old["pty_generation"]))
        self.assertGreater(int(changed["event_seq"]), int(resized["event_seq"]))
        self.server.run("respawn-pane", "-k", "-t", pane, "exit 0")
        exited, _ = peer.until(lambda m: m.get("reason") == "exited")
        self.assertGreaterEqual(int(exited["pty_generation"]), int(changed["pty_generation"]))
        self.inject(self.pane, "pending-before-delete\n")
        self.server.run("kill-window", "-t", self.pane)
        removed, _ = peer.until(lambda m: m.get("reason") == "removed" and m.get("pane_id") == self.pane)
        time.sleep(.35)
        while peer.ready(.02):
            event = peer.receive()
            self.assertFalse(event.get("pane_id") == self.pane and event.get("reason") == "screen_dirty", event)
        self.assertEqual(removed["pane_id"], self.pane)
        self.assertEqual(self.stats()["pending_dirty"], 0)

    def test_global_watch_capacity_and_shared_references(self):
        ids = self.script(["new-window -d -E -P -F '#{pane_id}'"] * 513).splitlines()
        self.assertEqual(len(ids), 513)
        peers = []
        for index in range(8):
            peer = self.peer()
            response = peer.watch(ids[index * 64:(index + 1) * 64])
            self.assertEqual(response["kind"], "watch", response)
            peers.append(peer)
        self.assertEqual(self.stats()["watched_panes"], 512)
        extra = self.peer()
        rejected = extra.watch([ids[-1]])
        self.assertEqual(rejected.get("code"), "observation_capacity_exceeded", rejected)
        self.assertEqual(extra.watch([ids[0]])["kind"], "watch")
        self.assertEqual(self.stats()["watched_panes"], 512)
        self.assertEqual(self.stats()["watch_subscriptions"], 9)
        peers[0].close()
        wait_for(lambda: self.stats()["watched_panes"] == 449)
        replacement = self.peer()
        self.assertEqual(replacement.watch([ids[-1]])["kind"], "watch")
        self.assertEqual(self.stats()["watched_panes"], 450)

    def test_slow_reader_reports_loss_without_blocking_native_commands(self):
        # Stay above the macOS UDS write low-water mark so hello can complete.
        peer = self.peer(receive_buffer=8192)
        peer.watch([self.pane])
        # One native command queue deliberately overwrites the bounded journal.
        # The non-reading socket must not hold this queue or other clients.
        self.script([f"resize-window -t {self.pane} -x {90 + (i % 2)}" for i in range(10000)])
        started = time.monotonic()
        self.server.run("send-keys", "-t", "%0", "-l", "native-still-responsive")
        self.server.run("send-keys", "-t", "%0", "Enter")
        wait_for(lambda: "native-still-responsive" in self.server.text("capture-pane", "-p", "-t", "%0"))
        self.assertLess(time.monotonic() - started, 3)
        stats = self.stats()
        self.assertLessEqual(stats["journal_events"], 4096)
        self.assertLessEqual(stats["tx_queued_bytes"], 512 * 1024)
        lost = False
        end = time.monotonic() + 7
        while time.monotonic() < end:
            if not peer.ready(.1):
                continue
            try:
                message = peer.receive()
            except (EOFError, ConnectionResetError):
                lost = True
                break
            if message.get("kind") == "gap":
                self.assertEqual(message["code"], "resync_required")
                self.assertLess(int(message["after_seq"]), int(message["first_available_seq"]))
                lost = True
                break
        self.assertTrue(lost, "slow observer silently retained a fresh stream")
        peer.close()
        wait_for(lambda: self.stats()["watched_panes"] == 0)
        recovered = self.peer().watch([self.pane])
        self.assertEqual(recovered["kind"], "watch")
        self.assertTrue(recovered["complete"])

    def test_blocked_writer_expires_while_native_input_progresses(self):
        peer = self.peer(receive_buffer=8192)
        peer.watch([self.pane])
        stop = threading.Event()
        failures = []

        def produce():
            try:
                # Below the journal capacity: cleanup must come from the
                # blocked TX deadline, not from journal overflow.
                for index in range(3000):
                    if stop.is_set():
                        break
                    self.server.run("resize-window", "-t", self.pane, "-x", str(90 + index % 2))
                    stop.wait(.002)
            except Exception as error:
                failures.append(error)

        thread = threading.Thread(target=produce)
        thread.start()
        try:
            wait_for(lambda: self.stats()["tx_queued_bytes"] > 0, timeout=5)
            time.sleep(.25)
            self.assertGreater(self.stats()["tx_queued_bytes"], 0)
            self.assertTrue(thread.is_alive())
            self.server.run("send-keys", "-t", "%0", "-l", "concurrent-input-alive")
            self.server.run("send-keys", "-t", "%0", "Enter")
            wait_for(lambda: "concurrent-input-alive" in self.server.text("capture-pane", "-p", "-t", "%0"))
            wait_for(lambda: self.stats()["watch_subscriptions"] == 0, timeout=8)
            stats = self.stats()
            self.assertEqual(stats["watched_panes"], 0)
            self.assertEqual(int(stats["counters"]["gaps"]), 0)
            self.assertLess(int(stats["counters"]["events"]), 4096)
            self.assertFalse(stats["flush_timer_active"])
        finally:
            stop.set()
            thread.join(timeout=6)
        self.assertFalse(thread.is_alive())
        self.assertEqual(failures, [])


if __name__ == "__main__":
    unittest.main(verbosity=2)
