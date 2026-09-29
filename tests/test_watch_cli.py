#!/usr/bin/env python3
"""Deterministic watch CLI checks against a private mock Unix socket."""

from __future__ import annotations

import json
import os
from pathlib import Path
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
from typing import Callable


ROOT = Path(__file__).resolve().parents[1]
AGENT = Path(os.environ.get("MASIL_AGENT", ROOT / "bin" / "masil-agent"))
BOOT_ID = "12345678-1234-4123-8123-123456789abc"
Handler = Callable[[socket.socket, dict[str, object]], None]


def receive_exact(connection: socket.socket, length: int) -> bytes:
    chunks: list[bytes] = []
    while length:
        chunk = connection.recv(length)
        if not chunk:
            raise AssertionError("client closed during a request frame")
        chunks.append(chunk)
        length -= len(chunk)
    return b"".join(chunks)


def receive_frame(connection: socket.socket) -> dict[str, object]:
    length = struct.unpack(">I", receive_exact(connection, 4))[0]
    assert 0 < length <= 8192
    value = json.loads(receive_exact(connection, length))
    assert isinstance(value, dict)
    return value


def encoded_frame(value: dict[str, object]) -> bytes:
    payload = json.dumps(value, separators=(",", ":")).encode()
    return struct.pack(">I", len(payload)) + payload


def send_frame(connection: socket.socket, value: dict[str, object]) -> None:
    connection.sendall(encoded_frame(value))


def hello() -> dict[str, object]:
    return {
        "v": 1,
        "kind": "hello",
        "request_id": "hello",
        "core_boot_id": BOOT_ID,
        "negotiated_version": {"major": 1, "minor": 1},
        "capabilities": {
            "inventory": True,
            "snapshot": True,
            "stats": True,
            "watch": True,
            "events": True,
            "actions": False,
            "submit": False,
        },
        "limits": {"rx_frame": 8192, "tx_frame": 65536},
    }


def pane(pane_id: str) -> dict[str, object]:
    return {
        "pane_id": pane_id,
        "pty_generation": "2",
        "screen_generation": "7",
        "width": 80,
        "height": 24,
        "dead": False,
    }


def acknowledgement(pane_ids: list[str] | None = None) -> dict[str, object]:
    pane_ids = pane_ids or ["%0"]
    return {
        "v": 1,
        "kind": "watch",
        "request_id": "watch",
        "core_boot_id": BOOT_ID,
        "stream_epoch": "3",
        "fence_seq": "5",
        "scope_revision": "11",
        "complete": True,
        "panes": [pane(pane_id) for pane_id in pane_ids],
    }


def event(sequence: str, pane_id: str = "%0", reason: str = "screen_dirty") -> dict[str, object]:
    return {
        "v": 1,
        "kind": "event",
        "core_boot_id": BOOT_ID,
        "stream_epoch": "3",
        "event_seq": sequence,
        "pane_id": pane_id,
        "pty_generation": "2",
        "screen_generation": "8",
        "reason": reason,
    }


def run_case(
    test_root: Path,
    name: str,
    handler: Handler,
    *arguments: str,
    stdout: int | None = subprocess.PIPE,
    timeout: float = 10,
) -> tuple[subprocess.CompletedProcess[bytes], dict[str, object]]:
    socket_path = test_root / f"{name}.sock"
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    listener.bind(str(socket_path))
    listener.listen(1)
    captured: dict[str, object] = {}
    failures: list[BaseException] = []

    def serve() -> None:
        try:
            with listener.accept()[0] as connection:
                connection.settimeout(timeout)
                hello_request = receive_frame(connection)
                assert hello_request == {"v": 1, "kind": "hello", "request_id": "hello"}
                send_frame(connection, hello())
                watch_request = receive_frame(connection)
                captured.update(watch_request)
                handler(connection, watch_request)
        except BaseException as error:  # Propagate mock assertion failures to the test thread.
            failures.append(error)
        finally:
            listener.close()

    server = threading.Thread(target=serve, name=f"mock-core-{name}", daemon=True)
    server.start()
    command = [str(AGENT), "--socket", str(socket_path), "watch", *arguments]
    completed = subprocess.run(
        command,
        cwd=ROOT,
        stdout=stdout,
        stderr=subprocess.PIPE,
        timeout=timeout,
        check=False,
    )
    server.join(timeout)
    assert not server.is_alive(), f"mock core did not finish for {name}"
    if failures:
        raise failures[0]
    return completed, captured


def output_lines(completed: subprocess.CompletedProcess[bytes]) -> list[dict[str, object]]:
    return [json.loads(line) for line in completed.stdout.splitlines()]


def expect_protocol_error(
    test_root: Path, name: str, changed_event: dict[str, object]
) -> None:
    def handler(connection: socket.socket, _request: dict[str, object]) -> None:
        send_frame(connection, acknowledgement())
        send_frame(connection, changed_event)

    completed, _ = run_case(test_root, name, handler, "--count", "1", "%0")
    assert completed.returncode == 2, (name, completed.returncode, completed.stderr)
    assert len(output_lines(completed)) == 1


def main() -> None:
    assert AGENT.is_file(), f"missing agent binary: {AGENT}"
    build_root = ROOT / ".build"
    build_root.mkdir(exist_ok=True)
    test_root = Path(tempfile.mkdtemp(prefix="watch-cli-", dir=build_root))
    os.chmod(test_root, 0o700)
    try:
        def valid_handler(connection: socket.socket, request: dict[str, object]) -> None:
            assert request == {
                "v": 1,
                "kind": "watch",
                "request_id": "watch",
                "expected_core_boot_id": BOOT_ID,
                "pane_ids": ["%0", "%1"],
            }
            send_frame(connection, acknowledgement(["%0", "%1"]))
            time.sleep(3.2)
            send_frame(connection, event("8"))
            send_frame(connection, event("10", "%1", "resized"))

        started = time.monotonic()
        valid, request = run_case(
            test_root, "valid-idle", valid_handler, "--count", "2", "%0", "%1"
        )
        assert valid.returncode == 0, valid.stderr
        assert time.monotonic() - started >= 3.0
        assert request["pane_ids"] == ["%0", "%1"]
        assert [line["kind"] for line in output_lines(valid)] == ["watch", "event", "event"]

        invalid_boot = event("6")
        invalid_boot["core_boot_id"] = "87654321-4321-4321-8321-cba987654321"
        expect_protocol_error(test_root, "invalid-boot", invalid_boot)

        invalid_epoch = event("6")
        invalid_epoch["stream_epoch"] = "4"
        expect_protocol_error(test_root, "invalid-epoch", invalid_epoch)

        expect_protocol_error(test_root, "invalid-reason", event("6", reason="changed"))
        expect_protocol_error(test_root, "invalid-scope", event("6", pane_id="%9"))
        expect_protocol_error(test_root, "invalid-sequence", event("5"))

        def gap_handler(connection: socket.socket, _request: dict[str, object]) -> None:
            send_frame(connection, acknowledgement())
            send_frame(
                connection,
                {
                    "v": 1,
                    "kind": "gap",
                    "core_boot_id": BOOT_ID,
                    "stream_epoch": "3",
                    "after_seq": "7",
                    "first_available_seq": "10",
                    "last_seq": "12",
                    "code": "resync_required",
                },
            )

        gap, _ = run_case(test_root, "gap", gap_handler, "%0")
        assert gap.returncode == 3, gap.stderr
        assert [line["kind"] for line in output_lines(gap)] == ["watch", "gap"]
        assert b"observation lost" in gap.stderr

        def partial_eof_handler(connection: socket.socket, _request: dict[str, object]) -> None:
            send_frame(connection, acknowledgement())
            connection.sendall(b"\x00\x00")

        partial_eof, _ = run_case(test_root, "partial-eof", partial_eof_handler, "%0")
        assert partial_eof.returncode == 3, partial_eof.stderr
        assert b"observation lost" in partial_eof.stderr

        def partial_timeout_handler(connection: socket.socket, _request: dict[str, object]) -> None:
            send_frame(connection, acknowledgement())
            connection.sendall(b"\x00")
            time.sleep(3.2)

        partial_timeout, _ = run_case(
            test_root, "partial-timeout", partial_timeout_handler, "%0"
        )
        assert partial_timeout.returncode == 2, partial_timeout.stderr
        assert b"timed out" in partial_timeout.stderr

        def delayed_final_byte_handler(
            connection: socket.socket, _request: dict[str, object]
        ) -> None:
            send_frame(connection, acknowledgement())
            frame = encoded_frame(event("6"))
            connection.sendall(frame[:1])
            time.sleep(0.25)
            connection.sendall(frame[1:-1])
            time.sleep(2.9)
            connection.sendall(frame[-1:])

        delayed_final, _ = run_case(
            test_root,
            "delayed-final-byte",
            delayed_final_byte_handler,
            "--count",
            "1",
            "%0",
        )
        assert delayed_final.returncode == 2, delayed_final.stderr
        assert b"timed out" in delayed_final.stderr

        read_fd, write_fd = os.pipe()
        os.close(read_fd)
        try:
            def broken_pipe_handler(connection: socket.socket, _request: dict[str, object]) -> None:
                send_frame(connection, acknowledgement())

            broken_pipe, _ = run_case(
                test_root, "broken-pipe", broken_pipe_handler, "%0", stdout=write_fd
            )
        finally:
            os.close(write_fd)
        assert broken_pipe.returncode == 0, broken_pipe.stderr

        missing_socket = test_root / "does-not-exist.sock"
        for invalid_arguments in (
            ("%x",),
            ("%1", "%01"),
            ("--count", "no", "%0"),
            tuple(f"%{index}" for index in range(65)),
        ):
            invalid = subprocess.run(
                [str(AGENT), "--socket", str(missing_socket), "watch", *invalid_arguments],
                cwd=ROOT,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
            )
            assert invalid.returncode == 2, (invalid_arguments, invalid.stderr)
            assert b"bridge unavailable" not in invalid.stderr

        print("watch CLI integration: passed")
    finally:
        resolved = test_root.resolve()
        if resolved.parent == build_root.resolve() and resolved.name.startswith("watch-cli-"):
            shutil.rmtree(resolved, ignore_errors=True)


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print(f"watch CLI integration: failed: {error}", file=sys.stderr)
        raise
