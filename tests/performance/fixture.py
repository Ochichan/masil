#!/usr/bin/env python3
"""Deterministic PTY child used by the rmux performance harness."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import sys
import termios
import time
import tty


def write_all(fd: int, data: bytes) -> None:
    view = memoryview(data)
    while view:
        written = os.write(fd, view)
        view = view[written:]


def report(path: Path, event: str, **fields: object) -> None:
    record = {"event": event, "monotonic_ns": time.monotonic_ns(), **fields}
    encoded = (json.dumps(record, sort_keys=True, separators=(",", ":")) + "\n").encode()
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
    try:
        write_all(fd, encoded)
        os.fsync(fd)
    finally:
        os.close(fd)


def terminal_size() -> tuple[int, int]:
    try:
        size = os.get_terminal_size(0)
        return size.columns, size.lines
    except OSError:
        return 0, 0


def configure_terminal() -> None:
    if not os.isatty(0):
        raise SystemExit("fixture stdin is not a PTY")
    tty.setraw(0, termios.TCSANOW)
    attrs = termios.tcgetattr(0)
    attrs[3] &= ~termios.ECHO
    termios.tcsetattr(0, termios.TCSANOW, attrs)


def announce_ready(sidechannel: Path, nonce: str) -> None:
    columns, rows = terminal_size()
    marker = f"@@RMUX_BENCH_READY:{nonce}:{columns}x{rows}@@\r\n".encode("ascii")
    write_all(1, marker)
    report(sidechannel, "ready", nonce=nonce, columns=columns, rows=rows, pid=os.getpid())


def wait_until(deadline_ns: int) -> None:
    while True:
        remaining = deadline_ns - time.monotonic_ns()
        if remaining <= 0:
            return
        time.sleep(min(remaining / 1_000_000_000, 0.05))


def wait_for_quit(seconds: float) -> None:
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        try:
            data = os.read(0, 4096)
        except InterruptedError:
            continue
        if not data or b"QUIT" in data:
            return


def interactive(args: argparse.Namespace) -> None:
    announce_ready(args.sidechannel, args.nonce)
    pending = bytearray()
    while True:
        data = os.read(0, 4096)
        if not data:
            return
        pending.extend(data)
        while True:
            positions = [p for p in (pending.find(b"\r"), pending.find(b"\n")) if p >= 0]
            if not positions:
                break
            index = min(positions)
            token = bytes(pending[:index])
            del pending[: index + 1]
            if not token:
                continue
            if token == b"QUIT":
                return
            safe = token.hex().encode("ascii")
            marker = b"@@RMUX_BENCH_ECHO:" + args.nonce.encode("ascii") + b":" + safe + b"@@\r\n"
            write_all(1, marker)


def idle(args: argparse.Namespace) -> None:
    announce_ready(args.sidechannel, args.nonce)
    wait_for_quit(args.linger_seconds)


def output(args: argparse.Namespace) -> None:
    announce_ready(args.sidechannel, args.nonce)
    if args.start_gate:
        while not args.start_gate.exists():
            time.sleep(0.001)
    elif args.start_at_ns:
        wait_until(args.start_at_ns)
    marker = f"@@RMUX_BENCH_DONE:{args.nonce}@@\r\n".encode("ascii")
    if args.bytes < len(marker):
        raise SystemExit(f"--bytes must be at least {len(marker)}")

    remaining_payload = args.bytes - len(marker)
    pattern = (
        b"\x1b[H" + (b"0123456789abcdef" * 5) + b"\x1b[K"
        if args.output_style == "inplace"
        else (b"0123456789abcdef" * 15) + b"\r\n"
    )
    started_ns = time.monotonic_ns()
    sent = 0
    while remaining_payload:
        count = min(args.chunk_bytes, remaining_payload)
        offset = sent % len(pattern)
        block = (pattern * ((offset + count + len(pattern) - 1) // len(pattern)))[offset:offset + count]
        write_all(1, block)
        remaining_payload -= count
        sent += count
        if args.rate_bytes_per_second:
            target_ns = started_ns + int(sent * 1_000_000_000 / args.rate_bytes_per_second)
            wait_until(target_ns)
    write_all(1, marker)
    completed_ns = time.monotonic_ns()
    report(
        args.sidechannel,
        "complete",
        nonce=args.nonce,
        offered_bytes=args.bytes,
        write_started_ns=started_ns,
        write_completed_ns=completed_ns,
        completion_marker=marker.decode("ascii").strip(),
    )
    wait_for_quit(args.linger_seconds)


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("interactive", "idle", "output"))
    parser.add_argument("--sidechannel", required=True, type=Path)
    parser.add_argument("--nonce", required=True)
    parser.add_argument("--bytes", type=int, default=0)
    parser.add_argument("--chunk-bytes", type=int, default=4096)
    parser.add_argument("--output-style", choices=("scroll", "inplace"), default="scroll")
    parser.add_argument("--rate-bytes-per-second", type=int, default=0)
    parser.add_argument("--start-at-ns", type=int, default=0)
    parser.add_argument("--start-gate", type=Path)
    parser.add_argument("--linger-seconds", type=float, default=3600.0)
    args = parser.parse_args(argv)
    if args.bytes < 0 or args.chunk_bytes < 1 or args.rate_bytes_per_second < 0:
        parser.error("byte counts and rates must be non-negative; chunk size must be positive")
    if args.linger_seconds <= 0:
        parser.error("--linger-seconds must be positive")
    return args


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    configure_terminal()
    if args.mode == "interactive":
        interactive(args)
    elif args.mode == "idle":
        idle(args)
    else:
        output(args)
    return 0


if __name__ == "__main__":
    sys.exit(main())
