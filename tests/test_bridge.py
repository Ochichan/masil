#!/usr/bin/env python3
"""End-to-end checks for the opt-in rmux observation bridge."""

from __future__ import annotations

import json
import os
from pathlib import Path
import shutil
import socket
import struct
import subprocess
import sys
import time
import uuid


ROOT = Path(__file__).resolve().parents[1]
RMUX = ROOT / "bin" / "rmux"
TEST_ROOT = ROOT / ".build" / f"bridge-test-{uuid.uuid4().hex}"
BRIDGE = TEST_ROOT / "bridge.sock"
NATIVE_LABEL = f"rmux-bridge-{uuid.uuid4().hex}"


def run_rmux(*args: str, env: dict[str, str] | None = None) -> str:
    command = [str(RMUX), "-L", NATIVE_LABEL, *args]
    completed = subprocess.run(
        command,
        cwd=ROOT,
        env=env,
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    return completed.stdout


def receive_exact(connection: socket.socket, length: int) -> bytes:
    chunks: list[bytes] = []
    remaining = length
    while remaining:
        chunk = connection.recv(remaining)
        if not chunk:
            raise AssertionError("bridge closed during a frame")
        chunks.append(chunk)
        remaining -= len(chunk)
    return b"".join(chunks)


def send_raw(connection: socket.socket, payload: bytes) -> dict[str, object]:
    connection.sendall(struct.pack(">I", len(payload)) + payload)
    length = struct.unpack(">I", receive_exact(connection, 4))[0]
    assert 0 < length <= 65536
    return json.loads(receive_exact(connection, length))


def request(connection: socket.socket, kind: str, request_id: str, **fields: object) -> dict[str, object]:
    payload: dict[str, object] = {"v": 1, "kind": kind, "request_id": request_id}
    payload.update(fields)
    return send_raw(connection, json.dumps(payload, separators=(",", ":")).encode())


def connect() -> socket.socket:
    connection = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    connection.settimeout(2)
    connection.connect(str(BRIDGE))
    return connection


def wait_for_socket() -> None:
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        if BRIDGE.exists():
            return
        time.sleep(0.02)
    raise AssertionError("bridge socket was not created")


def expect_error(response: dict[str, object], code: str) -> None:
    assert response["kind"] == "error", response
    assert response["code"] == code, response


def main() -> None:
    assert RMUX.is_file(), f"missing binary: {RMUX}"
    TEST_ROOT.mkdir(parents=True, mode=0o700)
    os.chmod(TEST_ROOT, 0o700)
    environment = os.environ.copy()
    environment["RMUX_BRIDGE_SOCKET"] = str(BRIDGE)

    try:
        run_rmux("-f", "/dev/null", "new-session", "-d", "-s", "bridge", "-x", "80", "-y", "24", env=environment)
        wait_for_socket()
        assert (BRIDGE.stat().st_mode & 0o777) == 0o600

        with connect() as connection:
            hello = request(connection, "hello", "hello-1")
            assert hello["kind"] == "hello"
            capabilities = hello["capabilities"]
            assert capabilities == {
                "inventory": True,
                "snapshot": True,
                "stats": True,
                "watch": False,
                "events": False,
                "actions": False,
                "submit": False,
            }
            boot_id = hello["core_boot_id"]

            inventory = request(connection, "inventory", "inventory-1")
            assert inventory["kind"] == "inventory"
            assert inventory["complete"] is True
            assert len(inventory["panes"]) == 1, inventory
            pane = inventory["panes"][0]
            pane_id = pane["pane_id"]

            first = request(connection, "snapshot", "snapshot-1", pane_id=pane_id)
            first_screen = int(first["screen_generation"])
            first_pty = int(first["pty_generation"])

            run_rmux("send-keys", "-t", pane_id, "-l", "printf 'bridge-utf8-Ω\\n'")
            run_rmux("send-keys", "-t", pane_id, "Enter")
            deadline = time.monotonic() + 3
            while True:
                output = request(connection, "snapshot", "snapshot-output", pane_id=pane_id)
                if "bridge-utf8-Ω" in output["text"]:
                    break
                if time.monotonic() >= deadline:
                    raise AssertionError(output)
                time.sleep(0.03)
            assert int(output["screen_generation"]) > first_screen
            assert int(output["pty_generation"]) == first_pty

            run_rmux("copy-mode", "-t", pane_id)
            copy_snapshot = request(connection, "snapshot", "snapshot-copy", pane_id=pane_id)
            assert "bridge-utf8-Ω" in copy_snapshot["text"]

            before_reset = int(copy_snapshot["screen_generation"])
            run_rmux("send-keys", "-R", "-t", pane_id)
            after_reset = request(connection, "snapshot", "snapshot-reset", pane_id=pane_id)
            assert int(after_reset["screen_generation"]) > before_reset
            expect_error(
                request(
                    connection,
                    "snapshot",
                    "stale-screen",
                    pane_id=pane_id,
                    expected_screen_generation=str(before_reset),
                ),
                "screen_generation_mismatch",
            )

            run_rmux("resize-window", "-t", "bridge:0", "-x", "300", "-y", "50")
            resized = request(connection, "snapshot", "snapshot-resize", pane_id=pane_id)
            assert resized["width"] == 300 and resized["height"] == 50
            assert resized["source"]["rows"] == 32
            assert resized["source"]["columns"] == 240
            assert resized["clipped"]["rows"] == 18
            assert resized["clipped"]["columns"] == 60
            assert resized["complete"] is False
            assert len(resized["text"].encode()) <= 16384

            old_pty = resized["pty_generation"]
            run_rmux("respawn-pane", "-k", "-t", pane_id)
            respawned = request(connection, "snapshot", "snapshot-respawn", pane_id=pane_id)
            assert int(respawned["pty_generation"]) > int(old_pty)
            expect_error(
                request(
                    connection,
                    "snapshot",
                    "stale-pty",
                    pane_id=pane_id,
                    expected_pty_generation=old_pty,
                ),
                "pty_generation_mismatch",
            )
            expect_error(
                request(
                    connection,
                    "snapshot",
                    "stale-boot",
                    pane_id=pane_id,
                    expected_core_boot_id="00000000-0000-4000-8000-000000000000",
                ),
                "boot_mismatch",
            )
            fenced = request(
                connection,
                "snapshot",
                "current-fences",
                pane_id=pane_id,
                expected_core_boot_id=boot_id,
                expected_pty_generation=respawned["pty_generation"],
                expected_screen_generation=respawned["screen_generation"],
            )
            assert fenced["kind"] == "snapshot"

            before_exit = request(connection, "inventory", "inventory-before-exit")
            before_exit_revision = int(before_exit["revision"])
            run_rmux("set-option", "-p", "-t", pane_id, "remain-on-exit", "on")
            run_rmux("send-keys", "-t", pane_id, "-l", "exit")
            run_rmux("send-keys", "-t", pane_id, "Enter")
            deadline = time.monotonic() + 3
            while True:
                exited = request(connection, "inventory", "inventory-exited")
                if exited["panes"][0]["dead"]:
                    break
                if time.monotonic() >= deadline:
                    raise AssertionError(exited)
                time.sleep(0.03)
            assert int(exited["revision"]) > before_exit_revision

            # A silent exit mutates the base grid only through the native
            # remain-on-exit banner, without any PTY output to mark it dirty.
            trigger = TEST_ROOT / "silent-exit"
            run_rmux("set-option", "-gw", "remain-on-exit", "on")
            silent_id = run_rmux(
                "new-window", "-d", "-P", "-F", "#{pane_id}",
                sys.executable, "-c",
                "import pathlib,time; p=pathlib.Path(" + repr(str(trigger)) + "); "
                "exec('while not p.exists(): time.sleep(0.01)')",
            ).strip()
            time.sleep(0.05)
            silent_before = request(connection, "snapshot", "silent-before", pane_id=silent_id)
            trigger.touch()
            deadline = time.monotonic() + 3
            while run_rmux("display-message", "-p", "-t", silent_id, "#{pane_dead}").strip() != "1":
                if time.monotonic() >= deadline:
                    raise AssertionError("silent child did not exit")
                time.sleep(0.03)
            expect_error(request(connection, "snapshot", "silent-fence", pane_id=silent_id,
                                 expected_screen_generation=silent_before["screen_generation"]),
                         "screen_generation_mismatch")

            limited = False
            for index in range(100):
                response = request(
                    connection, "snapshot", f"snapshot-rate-{index}", pane_id=pane_id
                )
                if response["kind"] == "error":
                    expect_error(response, "rate_limited")
                    limited = True
                    break
            assert limited, "snapshot burst did not hit the shared budget"

            stats = request(connection, "stats", "stats-1")
            assert stats["kind"] == "stats"
            assert int(stats["counters"]["snapshots"]) >= 6

        with connect() as connection:
            request(connection, "hello", "hello-duplicate")
            duplicate = send_raw(
                connection,
                b'{"v":1,"kind":"stats","kind":"stats","request_id":"duplicate"}',
            )
            expect_error(duplicate, "duplicate_field")

        with connect() as connection:
            deep = b'{"v":1,"kind":"hello","request_id":"deep","x":' + b"[" * 9 + b"0" + b"]" * 9 + b"}"
            expect_error(send_raw(connection, deep), "invalid_json")

        with connect() as connection:
            connection.sendall(struct.pack(">I", 8193))
            length = struct.unpack(">I", receive_exact(connection, 4))[0]
            response = json.loads(receive_exact(connection, length))
            expect_error(response, "invalid_frame_length")

        print("bridge integration: passed")
    finally:
        subprocess.run(
            [str(RMUX), "-L", NATIVE_LABEL, "kill-server"],
            cwd=ROOT,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        )
        resolved = TEST_ROOT.resolve()
        build_root = (ROOT / ".build").resolve()
        if resolved.parent == build_root and resolved.name.startswith("bridge-test-"):
            shutil.rmtree(resolved, ignore_errors=True)


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print(f"bridge integration: failed: {error}", file=sys.stderr)
        raise
