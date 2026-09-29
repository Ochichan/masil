#!/usr/bin/env python3
"""Read-only agentd integration with a real private core and deterministic HTTP/SSE."""
from __future__ import annotations

import json
from pathlib import Path
import queue
import socket
import stat
import struct
import subprocess
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlsplit

from test_compatibility import MASIL, Server, wait_for

AGENT = MASIL.with_name("masil-agent")


class Provider:
    def __init__(self, directory: Path, count=2):
        self.directory = str(directory.resolve())
        self.sessions = {
            f"ses_{i}": {"id": f"ses_{i}", "directory": self.directory,
                        "version": "1.18.32", **({"parentID": "ses_0"} if i else {})}
            for i in range(count)
        }
        self.statuses = {"ses_0": {"type": "busy"}}
        self.permissions = []
        self.questions = []
        self.requests = []
        self.streams = []
        self.stream_count = 0
        self.failure = None
        self.delay = 0
        self.status_gate = None
        self.status_captured = threading.Event()
        self.stopped = threading.Event()
        owner = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_):
                pass

            def handle(self):
                try:
                    super().handle()
                except ConnectionResetError:
                    # The observer deliberately closes malformed/oversized streams.
                    self.close_connection = True

            def do_GET(self):
                url = urlsplit(self.path)
                owner.requests.append((self.command, url.path, parse_qs(url.query)))
                try:
                    if url.path == "/event":
                        self.send_response(200)
                        self.send_header("Content-Type", "text/event-stream")
                        self.send_header("Connection", "close")
                        self.end_headers()
                        events = queue.Queue()
                        owner.streams.append(events)
                        owner.stream_count += 1
                        self.wfile.write(b'data: {"id":"evt_connected","type":"server.connected","properties":{}}\n\n')
                        self.wfile.flush()
                        while not owner.stopped.is_set():
                            try:
                                event = events.get(timeout=.1)
                            except queue.Empty:
                                continue
                            if event is None:
                                self.close_connection = True
                                return
                            self.wfile.write(event)
                            self.wfile.flush()
                        self.close_connection = True
                        return
                    if owner.delay:
                        time.sleep(owner.delay)
                    status = 200
                    if owner.failure == "auth":
                        status, value = 401, {"error": "unauthorized"}
                    elif owner.failure == "oversize":
                        value = "x" * 262145
                    elif url.path == "/global/health":
                        value = {"healthy": True, "version": "1.18.32"}
                    elif url.path == "/session/status":
                        value = dict(owner.statuses)
                        if owner.status_gate is not None:
                            gate = owner.status_gate
                            owner.status_captured.set()
                            gate.wait(4)
                    elif url.path == "/permission":
                        value = owner.permissions
                    elif url.path == "/question":
                        value = owner.questions
                    elif url.path.startswith("/session/"):
                        value = owner.sessions.get(url.path.removeprefix("/session/"))
                        if value is None:
                            status, value = 404, {"name": "NotFoundError"}
                    else:
                        status, value = 404, {"error": "unknown fixture route"}
                    body = json.dumps(value).encode()
                    self.send_response(status)
                    self.send_header("Content-Type", "application/json")
                    self.send_header("Content-Length", str(len(body)))
                    self.end_headers()
                    self.wfile.write(body)
                except (BrokenPipeError, ConnectionResetError):
                    self.close_connection = True

            def do_POST(self):
                owner.requests.append((self.command, self.path, {}))
                self.send_error(405)

        self.http = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.http.serve_forever, daemon=True)
        self.thread.start()
        self.endpoint = f"http://127.0.0.1:{self.http.server_port}"

    def event(self, event_type="session.status", session_id="ses_0", **properties):
        event = {"id": "evt_fixture", "type": event_type, "properties": {"sessionID": session_id, **properties}}
        self.raw(b"data: " + json.dumps(event).encode() + b"\n\n")

    def raw(self, payload):
        for events in list(self.streams):
            events.put(payload)

    def close(self):
        self.stopped.set()
        self.http.shutdown()
        self.http.server_close()
        self.thread.join(timeout=2)


def manager_query(path: Path, kind, **fields):
    body = json.dumps({"v": 1, "kind": kind, "request_id": kind, **fields}).encode()
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
        client.settimeout(4)
        client.connect(str(path))
        client.sendall(struct.pack(">I", len(body)) + body)

        def exact(length):
            data = bytearray()
            while len(data) < length:
                part = client.recv(length - len(data))
                if not part:
                    raise AssertionError("agentd closed before a complete response")
                data.extend(part)
            return data

        length, = struct.unpack(">I", exact(4))
        if not 0 < length <= 65536:
            raise AssertionError(f"invalid frame length: {length}")
        return json.loads(exact(length))


class AgentdHarness(unittest.TestCase):
    def setUp(self):
        self.core = Server(MASIL, command=["/bin/cat"])
        self.bridge = self.core.path / "observe.sock"
        self.core.env["MASIL_BRIDGE_SOCKET"] = str(self.bridge)
        self.core.__enter__()
        self.addCleanup(self.core.__exit__)
        self.panes = ["%0", self.core.text("new-window", "-d", "-P", "-F", "#{pane_id}", "/bin/cat").strip()]
        self.provider = Provider(self.core.path)
        self.addCleanup(self.provider.close)
        self.manager = self.core.path / "manager.sock"
        self.config = self.core.path / "sources.json"
        self.config.write_text(json.dumps({"sources": [{
            "id": "local", "endpoint": self.provider.endpoint,
            "directory": self.provider.directory,
            "sessions": [{"id": f"agent-{i}", "pane_id": pane, "session_id": f"ses_{i}"}
                         for i, pane in enumerate(self.panes)]
        }]}))
        self.child = None

    def start(self):
        self.child = subprocess.Popen(self.command(), env=self.core.env,
                                      stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        self.addCleanup(self.cleanup_child)

        def ready():
            if self.child.poll() is not None:
                self.fail(f"agentd exited: {self.child.communicate()}")
            return self.manager.exists()

        wait_for(ready, 5)
        wait_for(lambda: self.query("status")["core"]["freshness"] == "fresh", 5)

    def command(self, manager=None):
        return [str(AGENT), "--socket", str(manager or self.manager), "serve",
                "--core", str(self.bridge), "--config", str(self.config)]

    def cleanup_child(self):
        if self.child and self.child.poll() is None:
            self.child.terminate()
        if self.child:
            self.child.communicate(timeout=5)

    def query(self, kind, **fields):
        return manager_query(self.manager, kind, **fields)

    def row(self, index=0):
        return self.query("inspect", id=f"agent-{index}")["observation"]

    def fresh(self):
        wait_for(lambda: self.row()["native"]["freshness"] == "fresh", 8)


class AgentdIntegration(AgentdHarness):
    def test_lifecycle_readonly_state_and_child_isolation(self):
        result = subprocess.run([str(AGENT), "--socket", str(self.manager), "status"],
                                capture_output=True, text=True, check=True)
        self.assertEqual(json.loads(result.stdout)["status"], "not_started")
        self.assertFalse(self.manager.exists())
        self.provider.permissions = [{"id": "per_parent", "sessionID": "ses_0"}]
        self.provider.questions = [{"id": "que_child", "sessionID": "ses_1"}]
        self.start()
        self.fresh()
        self.assertEqual(stat.S_IMODE(self.manager.stat().st_mode), 0o600)
        parent, child = self.row(), self.row(1)
        self.assertEqual(parent["native"]["activity"], "working")
        self.assertEqual(parent["native"]["attention"], "approval")
        self.assertEqual(child["native"]["attention"], "question")
        self.assertEqual(child["native"]["parent_session_id"], "ses_0")
        self.assertEqual(parent["binding"], "explicit_unverified")
        self.assertFalse(parent["frontend_verified"])
        self.assertFalse(parent["capabilities"]["completion"])
        self.provider.permissions = []
        self.provider.statuses["ses_1"] = {"type": "idle"}
        self.provider.event(session_id="ses_1", status={"type": "idle"})
        wait_for(lambda: self.row()["native"]["attention"] == "none", 5)
        self.assertEqual(self.row()["native"]["activity"], "working")
        self.provider.statuses["ses_0"] = {"type": "idle"}
        self.provider.event(status={"type": "idle"})
        wait_for(lambda: self.row()["native"]["activity"] == "idle", 5)
        self.assertNotIn("done", json.dumps(self.query("agents")))
        count = len(self.provider.requests)
        time.sleep(.4)
        self.assertEqual(len(self.provider.requests), count, "idle observation polled HTTP")
        self.assertEqual(self.provider.stream_count, 1)
        self.assertTrue(all(method == "GET" for method, _, _ in self.provider.requests))
        for _, path, query in self.provider.requests:
            if path != "/global/health":
                self.assertEqual(query.get("directory"), [self.provider.directory])
        self.assertEqual(self.query("inspect", id="missing")["code"], "not_found")
        duplicate = subprocess.run(self.command(self.core.path / "duplicate.sock"),
                                   env=self.core.env, capture_output=True, text=True, timeout=5)
        self.assertNotEqual(duplicate.returncode, 0)
        self.assertFalse((self.core.path / "duplicate.sock").exists())
        self.assertTrue(self.query("stop")["accepted"])
        self.child.wait(timeout=5)
        self.assertFalse(self.manager.exists())
        self.assertEqual(self.core.text("list-sessions", "-F", "#{session_name}").strip(), "main")

    def test_stale_recovery_and_request_bounds(self):
        self.start()
        self.fresh()
        self.provider.failure = "auth"
        self.provider.event()
        wait_for(lambda: self.row()["native"]["freshness"] != "fresh", 4)
        wait_for(lambda: self.row()["native"]["reason"] is not None, 7)
        stale = self.row()["native"]
        self.assertIsNone(stale["exists"])
        self.assertEqual(stale["activity"], "unknown")
        self.assertEqual(stale["last_activity"], "working")
        self.provider.failure = None
        self.fresh()
        self.provider.permissions = [{"id": f"per_{i}", "sessionID": "ses_0"} for i in range(20)]
        self.provider.event("permission.asked")
        wait_for(lambda: self.row()["native"]["permission_count"] == 20, 5)
        native = self.row()["native"]
        self.assertEqual(len(native["permission_ids"]), 16)
        self.assertTrue(native["request_ids_truncated"])
        self.provider.failure = "oversize"
        self.provider.event()
        wait_for(lambda: self.row()["native"]["freshness"] != "fresh", 4)
        wait_for(lambda: self.row()["native"]["reason"] is not None, 7)
        self.assertEqual(self.row()["native"]["attention"], "unknown")

    def test_slow_provider_and_client_do_not_block_management(self):
        self.start()
        self.fresh()
        self.provider.delay = 2
        self.provider.event()
        wait_for(lambda: self.row()["native"]["freshness"] != "fresh", 3)
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as slow:
            slow.connect(str(self.manager))
            slow.sendall(b"\x00")
            before = time.monotonic()
            self.assertEqual(self.query("status")["status"], "running")
            self.assertLess(time.monotonic() - before, 1)
            self.assertTrue(self.query("stop")["accepted"])
        self.child.wait(timeout=3)

    def test_core_replacement_and_disconnect_invalidate_association(self):
        self.start()
        self.fresh()
        self.core.run("respawn-pane", "-k", "-t", self.panes[0], "/bin/cat")
        wait_for(lambda: self.row()["binding"] == "invalidated", 3)
        self.assertEqual(self.row(1)["binding"], "explicit_unverified")
        self.core.run("kill-pane", "-t", self.panes[0])
        wait_for(lambda: self.row()["core"]["process"] == "removed", 3)
        self.assertEqual(self.row()["native"]["freshness"], "fresh")
        self.core.run("kill-server")
        wait_for(lambda: self.query("status")["core"]["freshness"] == "stale", 3)
        self.assertEqual(self.row(1)["binding"], "invalidated")
        self.assertEqual(self.row(1)["core"]["freshness"], "stale")

    def test_sse_disconnect_and_malformed_event_lose_freshness(self):
        self.start()
        self.fresh()
        self.provider.failure = "auth"
        self.provider.raw(None)
        wait_for(lambda: self.row()["native"]["freshness"] != "fresh", 3)
        self.provider.failure = None
        self.fresh()
        self.provider.raw(b"data: {invalid}\n\n")
        wait_for(lambda: self.row()["native"]["freshness"] != "fresh", 3)
        self.assertEqual(self.row()["native"]["activity"], "unknown")

    def test_snapshot_race_is_reconciled_before_fresh_publication(self):
        self.start()
        self.fresh()
        self.provider.status_gate = threading.Event()
        self.provider.statuses = {"ses_0": {"type": "retry", "attempt": 1, "message": "retry", "next": 100}}
        self.provider.event(status=self.provider.statuses["ses_0"])
        self.assertTrue(self.provider.status_captured.wait(3))
        self.provider.statuses = {"ses_0": {"type": "idle"}}
        self.provider.event(status={"type": "idle"})
        wait_for(lambda: self.row()["native"]["native_events"] >= 2, 3)
        self.provider.status_gate.set()
        until = time.monotonic() + 5
        while time.monotonic() < until:
            row = self.row()["native"]
            if row["freshness"] == "fresh":
                self.assertEqual(row["activity"], "idle", "published snapshot predating an observed event")
                break
            time.sleep(.01)
        else:
            self.fail("snapshot did not reconcile after concurrent invalidation")

    def test_continuous_irrelevant_sse_allows_snapshot_and_management_progress(self):
        self.start()
        stopped = threading.Event()
        payload = b'data: {"id":"evt_noise","type":"message.part.delta","properties":{"sessionID":"ses_other"}}\n\n' * 32

        def flood():
            while not stopped.is_set():
                self.provider.raw(payload)
                stopped.wait(.002)

        thread = threading.Thread(target=flood, daemon=True)
        thread.start()
        try:
            self.fresh()
            before = time.monotonic()
            self.assertEqual(self.query("status")["status"], "running")
            self.assertLess(time.monotonic() - before, 1)
        finally:
            stopped.set()
            thread.join(timeout=2)

    def test_partial_native_event_blocks_fresh_snapshot_publication(self):
        self.start()
        self.fresh()
        previous_questions = sum(path == "/question" for _, path, _ in self.provider.requests)
        self.provider.status_gate = threading.Event()
        self.provider.statuses = {"ses_0": {"type": "retry", "attempt": 1, "message": "retry", "next": 100}}
        self.provider.event(status=self.provider.statuses["ses_0"])
        self.assertTrue(self.provider.status_captured.wait(3))
        self.provider.statuses = {"ses_0": {"type": "idle"}}
        event = b'data: {"id":"evt_partial","type":"session.status","properties":{"sessionID":"ses_0","status":{"type":"idle"}}}\n\n'
        self.provider.raw(event[:50])
        time.sleep(.05)
        self.provider.status_gate.set()
        wait_for(lambda: sum(path == "/question" for _, path, _ in self.provider.requests) > previous_questions, 3)
        time.sleep(.1)
        self.assertNotEqual(self.row()["native"]["freshness"], "fresh")
        self.provider.raw(event[50:])
        self.fresh()
        self.assertEqual(self.row()["native"]["activity"], "idle")

    def test_client_deadline_and_existing_path_refusal(self):
        self.manager.write_text("do not overwrite")
        result = subprocess.run(self.command(), env=self.core.env, capture_output=True, text=True, timeout=5)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.manager.read_text(), "do not overwrite")
        self.manager.unlink()
        self.start()
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as slow:
            slow.settimeout(5)
            slow.connect(str(self.manager))
            slow.sendall(b"\x00")
            self.assertEqual(slow.recv(1), b"")
        self.assertEqual(self.query("status")["status"], "running")


if __name__ == "__main__":
    unittest.main()
