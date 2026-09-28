#!/usr/bin/env python3
"""Projection CLI rejects broken identity, schema, ordering and truncated streams."""
import copy
import json
from pathlib import Path
import socket
import struct
import subprocess
import tempfile
import threading
import unittest

from test_agentd import AGENT


def snapshot(revision="1"):
    return {"v": 1, "kind": "agents_snapshot", "request_id": "watch-agents",
            "epoch": "a" * 32, "revision": revision, "complete": True,
            "observations": [{"id": "task", "source_id": "local", "session_id": "ses_one", "pane_id": "%0",
                "native": {"exists": True, "activity": "idle", "last_activity": None,
                           "attention": "none", "permission_count": 0, "question_count": 0,
                           "freshness": "fresh", "observed_at_ms": 1},
                "core": {"process": "running", "pty_generation": "1", "freshness": "fresh"},
                "binding": "explicit_unverified", "frontend_verified": False,
                "capabilities": {"read": True, "input": False, "approval": False,
                                 "completion": False, "child_aggregation": False},
                "attention": {"revision": "1", "acknowledged": False, "pending": False, "available": True}}]}


class ProjectionCLI(unittest.TestCase):
    def execute(self, frames, count=None):
        failures = []
        with tempfile.TemporaryDirectory(prefix="rmx-stream-", dir="/tmp") as temp:
            path = Path(temp) / "manager.sock"
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as listener:
                listener.bind(str(path))
                listener.listen(1)
                listener.settimeout(4)

                def serve():
                    try:
                        with listener.accept()[0] as peer:
                            peer.settimeout(3)
                            header = peer.recv(4, socket.MSG_WAITALL)
                            length, = struct.unpack(">I", header)
                            request = json.loads(peer.recv(length, socket.MSG_WAITALL))
                            self.assertEqual(request["kind"], "watch-agents")
                            for frame in frames:
                                if isinstance(frame, bytes):
                                    peer.sendall(frame)
                                else:
                                    body = json.dumps(frame).encode()
                                    peer.sendall(struct.pack(">I", len(body)) + body)
                    except (BrokenPipeError, ConnectionResetError):
                        pass
                    except BaseException as error:
                        failures.append(error)

                thread = threading.Thread(target=serve)
                thread.start()
                result = subprocess.run([str(AGENT), "--socket", str(path), "watch-agents",
                                         *(["--count", str(count)] if count else [])],
                                        text=True, capture_output=True, timeout=6)
                thread.join(timeout=4)
                self.assertFalse(thread.is_alive())
                if failures:
                    raise failures[0]
                return result

    def test_full_snapshots_may_skip_revisions_but_must_advance(self):
        accepted = self.execute([snapshot(), snapshot("9")], count=2)
        self.assertEqual(accepted.returncode, 0, accepted.stderr)
        self.assertEqual(len(accepted.stdout.splitlines()), 2)
        for revision in ("1", "0"):
            with self.subTest(revision=revision):
                self.assertEqual(self.execute([snapshot(), snapshot(revision)]).returncode, 2)

    def test_epoch_scope_capability_and_schema_are_validated(self):
        changes = [
            lambda frame: frame.update(epoch="b" * 32),
            lambda frame: frame.update(complete=False),
            lambda frame: frame.update(request_id="different"),
            lambda frame: frame["observations"][0].update(pane_id="%1"),
            lambda frame: frame["observations"][0].update(source_id="other"),
            lambda frame: frame["observations"][0]["capabilities"].update(approval=True),
            lambda frame: frame["observations"][0]["attention"].update(acknowledged="yes"),
            lambda frame: frame["observations"].append(copy.deepcopy(frame["observations"][0])),
        ]
        for index, change in enumerate(changes):
            with self.subTest(case=index):
                invalid = snapshot("2")
                change(invalid)
                self.assertEqual(self.execute([snapshot(), invalid]).returncode, 2)

    def test_eof_and_partial_frame_never_report_counted_success(self):
        self.assertEqual(self.execute([snapshot()], count=2).returncode, 3)
        self.assertEqual(self.execute([snapshot(), b"\x00\x00\x00\x20{"], count=2).returncode, 3)
        self.assertEqual(self.execute([snapshot(), struct.pack(">I", 65537)]).returncode, 2)


if __name__ == "__main__":
    unittest.main()
