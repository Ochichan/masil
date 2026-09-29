#!/usr/bin/env python3
"""Reproducible PTY benchmarks for masil, pinned tmux, and Herdr 0.8.2.

Smoke mode is a bounded harness check, not a performance-budget pass.  Use
``--profile full`` for the minimum sample counts and durations documented in
``docs/design/performance.md``.
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass
from datetime import datetime, timezone
import fcntl
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import re
import secrets
import select
import shlex
import shutil
import signal
import statistics
import struct
import subprocess
import sys
import tempfile
import time
from typing import Any, Iterable


ROOT = Path(__file__).resolve().parents[2]
FIXTURE = Path(__file__).with_name("fixture.py")
HOST_COLUMNS = 120
HOST_ROWS = 40
HERDR_ALLOWED_ENV = frozenset(
    {
        "HOME",
        "XDG_CONFIG_HOME",
        "XDG_STATE_HOME",
        "XDG_DATA_HOME",
        "XDG_RUNTIME_DIR",
        "HERDR_CONFIG_PATH",
        "HERDR_SOCKET_PATH",
        "HERDR_CLIENT_SOCKET_PATH",
        "PATH",
        "TERM",
        "LANG",
        "LC_ALL",
        "TMPDIR",
        "SHELL",
        "USER",
        "LOGNAME",
    }
)
HERDR_FORBIDDEN_INHERITED = frozenset({"HERDR_SESSION", "HERDR_REMOTE", "TMUX"})


class BenchmarkError(RuntimeError):
    pass


def monotonic_ns() -> int:
    return time.monotonic_ns()


def percentile(values: list[float], probability: float) -> float | None:
    if not values:
        return None
    ordered = sorted(values)
    rank = max(0, math.ceil(probability * len(ordered)) - 1)
    return ordered[rank]


def distribution(values: list[float]) -> dict[str, Any]:
    return {
        "samples": len(values),
        "min": min(values) if values else None,
        "median": statistics.median(values) if values else None,
        "p95": percentile(values, 0.95),
        "p99": percentile(values, 0.99),
        "max": max(values) if values else None,
        "mean": statistics.fmean(values) if values else None,
        "stdev": statistics.stdev(values) if len(values) > 1 else None,
    }


def parse_cpu_time(value: str) -> float:
    days = 0
    if "-" in value:
        day_text, value = value.split("-", 1)
        days = int(day_text)
    raw_parts = value.split(":")
    parts = [int(part) for part in raw_parts[:-1]]
    seconds = float(raw_parts[-1])
    if len(parts) == 2:
        hours, minutes = parts
    elif len(parts) == 1:
        hours, minutes = 0, parts[0]
    else:
        raise ValueError(f"unexpected process time: {value!r}")
    return days * 86400 + hours * 3600 + minutes * 60 + seconds


def process_sample(pid: int) -> dict[str, Any] | None:
    result = subprocess.run(
        ["ps", "-p", str(pid), "-o", "pid=,ppid=,rss=,%cpu=,time=,command="],
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    line = result.stdout.strip()
    if result.returncode or not line:
        return None
    parts = line.split(None, 5)
    if len(parts) != 6:
        return None
    return {
        "pid": int(parts[0]),
        "ppid": int(parts[1]),
        "rss_kib": int(parts[2]),
        "cpu_percent_ps": float(parts[3]),
        "cpu_time_seconds": parse_cpu_time(parts[4]),
        "command": parts[5],
    }


def sha256(path: Path) -> str | None:
    if not path.is_file():
        return None
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def read_json_lines(path: Path) -> list[dict[str, Any]]:
    if not path.exists():
        return []
    records: list[dict[str, Any]] = []
    for line in path.read_text(errors="replace").splitlines():
        try:
            record = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(record, dict):
            records.append(record)
    return records


def wait_for_event(paths: Iterable[Path], event: str, timeout: float) -> list[dict[str, Any]]:
    selected = list(paths)
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        found: list[dict[str, Any]] = []
        for path in selected:
            records = read_json_lines(path)
            match = next((item for item in records if item.get("event") == event), None)
            if match is not None:
                found.append(match)
        if len(found) == len(selected):
            return found
        time.sleep(0.01)
    missing = [str(path) for path in selected if not any(r.get("event") == event for r in read_json_lines(path))]
    raise BenchmarkError(f"timed out waiting for {event!r}: {missing}")


def set_pty_size(fd: int, columns: int = HOST_COLUMNS, rows: int = HOST_ROWS) -> None:
    fcntl.ioctl(fd, termios_tiocswinsz(), struct.pack("HHHH", rows, columns, 0, 0))


def termios_tiocswinsz() -> int:
    # Python exposes this through termios on supported systems, but the import is
    # intentionally local so unit tests of non-PTY helpers remain portable.
    import termios

    return termios.TIOCSWINSZ


class AttachedClient:
    def __init__(self, args: list[str], env: dict[str, str], cwd: Path, log: Path):
        try:
            import pyte
        except ImportError as error:
            raise BenchmarkError("benchmark requires pyte; use make benchmark-env and .build/bench-venv/bin/python") from error
        class MeasurementScreen(pyte.Screen):
            # Replies are handled by _respond; pyte 0.8.2 does not accept the
            # private flag on newer DSR queries such as DECDSR.
            def report_device_status(self, mode: int, **kwargs: Any) -> None:
                pass

        self.screen = MeasurementScreen(HOST_COLUMNS, HOST_ROWS)
        self.stream = pyte.ByteStream(self.screen)
        master, slave = os.openpty()
        set_pty_size(slave)
        self.master = master
        self.log = log
        self.buffer = bytearray()
        self.responses = 0
        self.process = subprocess.Popen(
            args,
            stdin=slave,
            stdout=slave,
            stderr=slave,
            cwd=cwd,
            env=env,
            close_fds=True,
            start_new_session=True,
        )
        os.close(slave)
        os.set_blocking(master, False)

    def _respond(self, data: bytes) -> None:
        replies: list[bytes] = []
        if b"\x1b[5n" in data:
            replies.append(b"\x1b[0n")
        if b"\x1b[6n" in data:
            replies.append(b"\x1b[1;1R")
        if b"\x1b[c" in data:
            replies.append(b"\x1b[?1;2c")
        if b"\x1b[>c" in data:
            replies.append(b"\x1b[>0;95;0c")
        for number, colour in ((10, b"ffff/ffff/ffff"), (11, b"0000/0000/0000"), (12, b"ffff/ffff/ffff")):
            query = f"\x1b]{number};?\x07".encode()
            if query in data:
                replies.append(f"\x1b]{number};rgb:".encode() + colour + b"\x07")
        for reply in replies:
            try:
                os.write(self.master, reply)
                self.responses += 1
            except OSError:
                return

    def drain(self, duration: float = 0.0) -> bytes:
        deadline = time.monotonic() + duration
        output = bytearray()
        while True:
            wait = max(0.0, min(0.05, deadline - time.monotonic())) if duration else 0.0
            readable, _, _ = select.select([self.master], [], [], wait)
            if not readable:
                if not duration or time.monotonic() >= deadline:
                    break
                continue
            try:
                block = os.read(self.master, 65536)
            except (BlockingIOError, OSError):
                break
            if not block:
                break
            output.extend(block)
            self.buffer.extend(block)
            self.stream.feed(block)
            self._respond(block)
        if output:
            with self.log.open("ab") as target:
                target.write(output)
        return bytes(output)

    def wait_marker(self, marker: bytes, timeout: float) -> int:
        deadline = time.monotonic() + timeout
        marker_text = marker.decode("ascii")
        if any(marker_text in row for row in self.screen.display):
            return monotonic_ns()
        while time.monotonic() < deadline:
            readable, _, _ = select.select([self.master], [], [], min(0.01, deadline - time.monotonic()))
            if readable:
                try:
                    block = os.read(self.master, 65536)
                except (BlockingIOError, OSError):
                    block = b""
                if block:
                    received_ns = monotonic_ns()
                    self.buffer.extend(block)
                    self.stream.feed(block)
                    self._respond(block)
                    with self.log.open("ab") as target:
                        target.write(block)
                    if any(marker_text in row for row in self.screen.display):
                        return received_ns
            if self.process.poll() is not None:
                raise BenchmarkError(f"attached client exited {self.process.returncode} before marker {marker!r}")
        raise BenchmarkError(
            f"attached client did not render marker {marker!r}; "
            f"PTY tail={bytes(self.buffer[-500:])!r}"
        )

    def send(self, data: bytes) -> None:
        view = memoryview(data)
        while view:
            _, writable, _ = select.select([], [self.master], [], 1.0)
            if not writable:
                raise BenchmarkError("attached client PTY was not writable")
            count = os.write(self.master, view)
            view = view[count:]

    def close(self) -> None:
        if self.process.poll() is None:
            self.process.send_signal(signal.SIGHUP)
            try:
                self.process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                self.process.terminate()
                try:
                    self.process.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    self.process.kill()
                    self.process.wait(timeout=2)
        try:
            os.close(self.master)
        except OSError:
            pass


@dataclass
class FixtureSpec:
    mode: str
    nonce: str
    sidechannel: Path
    offered_bytes: int = 0
    rate_bytes_per_second: int = 0
    start_at_ns: int = 0
    start_gate: Path | None = None
    output_style: str = "scroll"

    def command(self) -> list[str]:
        args = [
            sys.executable,
            str(FIXTURE),
            self.mode,
            "--sidechannel",
            str(self.sidechannel),
            "--nonce",
            self.nonce,
        ]
        if self.mode == "output":
            args += [
                "--bytes",
                str(self.offered_bytes),
                "--rate-bytes-per-second",
                str(self.rate_bytes_per_second),
                "--start-at-ns",
                str(self.start_at_ns),
                "--output-style",
                self.output_style,
            ]
            if self.start_gate is not None:
                args += ["--start-gate", str(self.start_gate)]
        return args


class ProductInstance:
    product: str

    def __init__(
        self,
        binary: Path,
        timeout: float,
        inner_columns: int,
        inner_rows: int,
        hidden_inner_columns: int,
    ):
        self.binary = binary.resolve()
        self.timeout = timeout
        self.inner_columns = inner_columns
        self.inner_rows = inner_rows
        self.hidden_inner_columns = hidden_inner_columns
        self.output_style = "scroll"
        self.root = Path(tempfile.mkdtemp(prefix="msl-bench-", dir="/tmp"))
        os.chmod(self.root, 0o700)
        self.client: AttachedClient | None = None
        self.server_pid: int | None = None
        self.child_pids: list[int] = []
        self.sidechannels: list[Path] = []
        self.fixtures: list[FixtureSpec] = []
        self.server_ready_ns: int | None = None
        self.cli_ready_ns: int | None = None
        self.started_ns: int | None = None
        self.output_gate = self.root / "output.start"

    def process_roles(self) -> dict[str, list[int]]:
        return {
            "server": [self.server_pid] if self.server_pid else [],
            "attached_client": [self.client.process.pid] if self.client else [],
            "fixture_children": list(self.child_pids),
        }

    def samples(self, roles: Iterable[str] | None = None) -> dict[str, list[dict[str, Any]]]:
        selected = set(roles) if roles is not None else None
        return {
            role: [sample for pid in pids if (sample := process_sample(pid)) is not None]
            for role, pids in self.process_roles().items()
            if selected is None or role in selected
        }

    def isolation_record(self) -> dict[str, Any]:
        return {"private_root": str(self.root), "mode": "product-specific"}

    def release_output(self) -> None:
        fd = os.open(self.output_gate, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        os.close(fd)

    def assert_inner_geometry(self) -> list[dict[str, Any]]:
        records = wait_for_event(self.sidechannels, "ready", self.timeout)
        mismatches = []
        for index, item in enumerate(records):
            expected_columns = self.inner_columns if index == 0 else self.hidden_inner_columns
            if int(item.get("columns", -1)) != expected_columns or int(item.get("rows", -1)) != self.inner_rows:
                mismatches.append(
                    {
                        "nonce": item.get("nonce"),
                        "columns": item.get("columns"),
                        "rows": item.get("rows"),
                        "expected_columns": expected_columns,
                        "expected_rows": self.inner_rows,
                    }
                )
        if mismatches:
            raise BenchmarkError(
                f"{self.product} inner geometry did not match "
                f"active={self.inner_columns}x{self.inner_rows}, "
                f"hidden={self.hidden_inner_columns}x{self.inner_rows}: {mismatches}"
            )
        return records

    def cleanup(self) -> list[str]:
        raise NotImplementedError


class TmuxInstance(ProductInstance):
    def __init__(self, product: str, binary: Path, timeout: float, inner_columns: int, inner_rows: int, hidden_inner_columns: int):
        super().__init__(binary, timeout, inner_columns, inner_rows, hidden_inner_columns)
        self.product = product
        self.socket = self.root / "tmux.sock"
        self.env = {
            "HOME": str(self.root / "home"),
            "PATH": os.environ.get("PATH", os.defpath),
            "TERM": "xterm-256color",
            "LANG": "C.UTF-8",
            "TMPDIR": str(self.root / "tmp"),
            "SHELL": "/bin/sh",
        }
        for path in (Path(self.env["HOME"]), Path(self.env["TMPDIR"])):
            path.mkdir(mode=0o700)

    def tmux(self, *args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
        result = subprocess.run(
            [str(self.binary), "-S", str(self.socket), *args],
            env=self.env,
            cwd=self.root,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=self.timeout,
            check=False,
        )
        if check and result.returncode:
            raise BenchmarkError(f"{self.product} {' '.join(args)} failed: {result.stderr.strip()}")
        return result

    def start(self, panes: int, mode: str, offered_bytes: int = 0, rate: int = 0, attach: bool = False) -> None:
        if panes < 1:
            raise ValueError("panes must be positive")
        self.started_ns = monotonic_ns()
        for index in range(panes):
            sidechannel = self.root / f"fixture-{index}.jsonl"
            fixture = FixtureSpec(
                mode=mode,
                nonce=f"{index}-{secrets.token_hex(6)}",
                sidechannel=sidechannel,
                offered_bytes=offered_bytes,
                rate_bytes_per_second=rate,
                start_gate=self.output_gate if mode == "output" else None,
                output_style=self.output_style,
            )
            self.fixtures.append(fixture)
            self.sidechannels.append(sidechannel)
            command = shlex.join(fixture.command())
            if index == 0:
                result = self.tmux(
                    "-f",
                    "/dev/null",
                    "new-session",
                    "-d",
                    "-s",
                    "bench",
                    "-x",
                    str(self.inner_columns),
                    "-y",
                    str(self.inner_rows),
                    command,
                )
                self.cli_ready_ns = monotonic_ns()
                self.server_ready_ns = self.cli_ready_ns
                self.tmux("set-option", "-t", "bench", "default-size", f"{self.inner_columns}x{self.inner_rows}")
                self.tmux("set-window-option", "-g", "-t", "bench", "window-size", "manual")
                self.tmux(
                    "resize-window",
                    "-t",
                    "bench:0",
                    "-x",
                    str(self.inner_columns),
                    "-y",
                    str(self.inner_rows),
                )
            else:
                placeholder = shlex.join([sys.executable, "-c", "import time; time.sleep(3600)"])
                result = self.tmux("new-window", "-d", "-t", "bench", "-n", f"p{index + 1}", placeholder)
                self.tmux("set-window-option", "-t", f"bench:p{index + 1}", "window-size", "manual")
                self.tmux(
                    "resize-window",
                    "-t",
                    f"bench:p{index + 1}",
                    "-x",
                    str(self.hidden_inner_columns),
                    "-y",
                    str(self.inner_rows),
                )
                self.tmux("respawn-pane", "-k", "-t", f"bench:p{index + 1}.0", command)
            if result.returncode:
                raise BenchmarkError(result.stderr.strip())
        self.server_pid = int(self.tmux("display-message", "-p", "#{pid}").stdout.strip())
        panes_text = self.tmux("list-panes", "-a", "-F", "#{pane_pid}").stdout
        self.child_pids = [int(line) for line in panes_text.splitlines() if line.strip().isdigit()]
        if attach:
            self.client = AttachedClient(
                [str(self.binary), "-S", str(self.socket), "attach-session", "-t", "bench"],
                self.env,
                self.root,
                self.root / "attached-client.raw",
            )
        self.assert_inner_geometry()

    def isolation_record(self) -> dict[str, Any]:
        return {
            "private_root": str(self.root),
            "socket": str(self.socket),
            "socket_within_private_root": self.socket.parent == self.root,
            "config": "/dev/null",
        }

    def cleanup(self) -> list[str]:
        errors: list[str] = []
        if self.client:
            self.client.close()
        if self.socket.exists():
            result = self.tmux("kill-server", check=False)
            if result.returncode and "no server running" not in result.stderr.lower():
                errors.append(result.stderr.strip())
        try:
            shutil.rmtree(self.root)
        except OSError as error:
            errors.append(str(error))
        return errors


def herdr_environment(root: Path) -> dict[str, str]:
    root = root.resolve()
    paths = {
        "HOME": root / "home",
        "XDG_CONFIG_HOME": root / "config",
        "XDG_STATE_HOME": root / "state",
        "XDG_DATA_HOME": root / "data",
        "XDG_RUNTIME_DIR": root / "runtime",
        "HERDR_CONFIG_PATH": root / "config" / "herdr" / "config.toml",
        "HERDR_SOCKET_PATH": root / "herdr.sock",
        "HERDR_CLIENT_SOCKET_PATH": root / "herdr-client.sock",
        "TMPDIR": root / "tmp",
    }
    for path in paths.values():
        directory = path if path.suffix == "" else path.parent
        directory.mkdir(parents=True, exist_ok=True, mode=0o700)
    env = {
        **{key: str(value) for key, value in paths.items()},
        "PATH": "/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin:/Users/ochi/.local/bin",
        "TERM": "xterm-256color",
        "LANG": "C.UTF-8",
        "LC_ALL": "C.UTF-8",
        "SHELL": "/bin/sh",
        "USER": "masil-bench",
        "LOGNAME": "masil-bench",
    }
    if set(env) - HERDR_ALLOWED_ENV:
        raise BenchmarkError(f"Herdr environment contains non-allowlisted keys: {set(env) - HERDR_ALLOWED_ENV}")
    if set(env) & HERDR_FORBIDDEN_INHERITED:
        raise BenchmarkError("Herdr environment contains forbidden inherited routing variables")
    for key in (
        "HOME",
        "XDG_CONFIG_HOME",
        "XDG_STATE_HOME",
        "XDG_DATA_HOME",
        "HERDR_CONFIG_PATH",
        "HERDR_SOCKET_PATH",
        "HERDR_CLIENT_SOCKET_PATH",
    ):
        if not Path(env[key]).resolve().is_relative_to(root):
            raise BenchmarkError(f"{key} escaped Herdr private root")
    Path(env["HERDR_CONFIG_PATH"]).write_text(
        'onboarding = false\n[ui.sound]\nenabled = false\n'
        '[terminal]\ndefault_shell = "/bin/sh"\nshell_mode = "non_login"\n'
        '[update]\nversion_check = false\nmanifest_check = false\n'
        '[experimental]\npane_history = false\n'
    )
    return env


def json_object(text: str, context: str) -> dict[str, Any]:
    try:
        value = json.loads(text)
    except json.JSONDecodeError as error:
        raise BenchmarkError(f"{context} did not return JSON: {text[-500:]!r}") from error
    if not isinstance(value, dict):
        raise BenchmarkError(f"{context} returned non-object JSON")
    return value


def nested_dicts(value: Any) -> Iterable[dict[str, Any]]:
    if isinstance(value, dict):
        yield value
        for child in value.values():
            yield from nested_dicts(child)
    elif isinstance(value, list):
        for child in value:
            yield from nested_dicts(child)


def find_ids(value: Any, key: str) -> list[str]:
    ids: list[str] = []
    for item in nested_dicts(value):
        candidate = item.get(key)
        if isinstance(candidate, str) and candidate not in ids:
            ids.append(candidate)
    return ids


class HerdrInstance(ProductInstance):
    product = "herdr-0.8.2"

    def __init__(self, binary: Path, timeout: float, inner_columns: int, inner_rows: int, hidden_inner_columns: int):
        super().__init__(binary, timeout, inner_columns, inner_rows, hidden_inner_columns)
        try:
            self.env = herdr_environment(self.root)
            self.server_process: subprocess.Popen[bytes] | None = None
            self.server_stdout = None
            self.server_stderr = None
            version = self.herdr("--version")
            if version.stdout.strip() != "herdr 0.8.2":
                raise BenchmarkError(f"expected installed Herdr 0.8.2, got {version.stdout.strip()!r}")
            schema = self.herdr("api", "schema")
            match = re.search(r"^protocol:\s*(\d+)\s*$", schema.stdout, re.MULTILINE)
            if not match or int(match.group(1)) != 20:
                raise BenchmarkError(f"expected Herdr 0.8.2 protocol 20, got {schema.stdout[:200]!r}")
            self.api_protocol = int(match.group(1))
        except Exception:
            shutil.rmtree(self.root, ignore_errors=True)
            raise

    def herdr(self, *args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
        result = subprocess.run(
            [str(self.binary), *args],
            env=self.env,
            cwd=self.root,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=self.timeout,
            check=False,
        )
        if check and result.returncode:
            raise BenchmarkError(f"herdr {' '.join(args)} failed: {result.stderr.strip()}")
        return result

    def wait_server(self) -> dict[str, Any]:
        deadline = time.monotonic() + self.timeout
        last = ""
        while time.monotonic() < deadline:
            if self.server_process and self.server_process.poll() is not None:
                raise BenchmarkError(f"Herdr server exited during startup: {self.server_process.returncode}")
            result = self.herdr("status", "server", "--json", check=False)
            last = result.stderr or result.stdout
            if result.returncode == 0:
                status = json_object(result.stdout, "herdr status server")
                if any(item.get("running") is True for item in nested_dicts(status)):
                    return status
            time.sleep(0.02)
        raise BenchmarkError(f"Herdr server did not become ready: {last[-500:]}")

    def _pane_inventory(self) -> tuple[str, list[str]]:
        workspaces = json_object(self.herdr("workspace", "list").stdout, "herdr workspace list")
        workspace_ids = find_ids(workspaces, "workspace_id")
        if not workspace_ids:
            raise BenchmarkError(f"Herdr workspace list contained no workspace_id: {workspaces!r}")
        workspace = workspace_ids[0]
        panes = json_object(
            self.herdr("pane", "list", "--workspace", workspace).stdout,
            "herdr pane list",
        )
        pane_ids = find_ids(panes, "pane_id")
        if not pane_ids:
            raise BenchmarkError("Herdr pane list contained no pane_id")
        return workspace, pane_ids

    def start(self, panes: int, mode: str, offered_bytes: int = 0, rate: int = 0, attach: bool = False) -> None:
        self.started_ns = monotonic_ns()
        self.server_stdout = (self.root / "server.stdout").open("wb")
        self.server_stderr = (self.root / "server.stderr").open("wb")
        self.server_process = subprocess.Popen(
            [str(self.binary), "server"],
            env=self.env,
            cwd=self.root,
            stdin=subprocess.DEVNULL,
            stdout=self.server_stdout,
            stderr=self.server_stderr,
            close_fds=True,
            start_new_session=True,
        )
        status = self.wait_server()
        self.server_ready_ns = monotonic_ns()
        status_pids = [item.get("pid") for item in nested_dicts(status) if isinstance(item.get("pid"), int)]
        self.server_pid = int(status_pids[0]) if status_pids else self.server_process.pid
        if not Path(self.env["HERDR_SOCKET_PATH"]).exists():
            raise BenchmarkError("Herdr reported ready without the private API socket")

        created_workspace = json_object(
            self.herdr(
                "workspace",
                "create",
                "--cwd",
                str(self.root),
                "--label",
                "masil-benchmark",
                "--no-focus",
            ).stdout,
            "herdr workspace create",
        )
        if not find_ids(created_workspace, "workspace_id") or not find_ids(created_workspace, "pane_id"):
            raise BenchmarkError(f"Herdr workspace create omitted IDs: {created_workspace!r}")
        self.cli_ready_ns = monotonic_ns()

        if attach:
            self.client = AttachedClient(
                [str(self.binary), "client"],
                self.env,
                self.root,
                self.root / "attached-client.raw",
            )
            self.client.drain(0.2)

        workspace, pane_ids = self._pane_inventory()
        while len(pane_ids) < panes:
            created = json_object(
                self.herdr(
                    "tab",
                    "create",
                    "--workspace",
                    workspace,
                    "--cwd",
                    str(self.root),
                    "--no-focus",
                ).stdout,
                "herdr tab create",
            )
            new_ids = find_ids(created, "pane_id")
            if not new_ids:
                raise BenchmarkError("Herdr tab create contained no pane_id")
            pane_ids.append(new_ids[0])

        for index, pane_id in enumerate(pane_ids[:panes]):
            sidechannel = self.root / f"fixture-{index}.jsonl"
            fixture = FixtureSpec(
                mode=mode,
                nonce=f"{index}-{secrets.token_hex(6)}",
                sidechannel=sidechannel,
                offered_bytes=offered_bytes,
                rate_bytes_per_second=rate,
                start_gate=self.output_gate if mode == "output" else None,
                output_style=self.output_style,
            )
            self.fixtures.append(fixture)
            self.sidechannels.append(sidechannel)
            self.herdr("pane", "run", pane_id, "exec " + shlex.join(fixture.command()))
        ready = self.assert_inner_geometry()
        self.child_pids = [int(item["pid"]) for item in ready]

    def isolation_record(self) -> dict[str, Any]:
        rooted = {
            key: Path(self.env[key]).resolve().is_relative_to(self.root.resolve())
            for key in (
                "HOME",
                "XDG_CONFIG_HOME",
                "XDG_STATE_HOME",
                "XDG_DATA_HOME",
                "HERDR_CONFIG_PATH",
                "HERDR_SOCKET_PATH",
                "HERDR_CLIENT_SOCKET_PATH",
            )
        }
        return {
            "private_root": str(self.root),
            "fresh_allowlisted_environment": True,
            "environment_keys": sorted(self.env),
            "forbidden_inherited_keys_absent": sorted(HERDR_FORBIDDEN_INHERITED),
            "paths_within_private_root": rooted,
            "api_socket": self.env["HERDR_SOCKET_PATH"],
            "client_socket": self.env["HERDR_CLIENT_SOCKET_PATH"],
            "observed_socket_files": sorted(str(path) for path in self.root.glob("*.sock")),
            "observed_server_pid": self.server_pid,
            "installed_version": "0.8.2",
            "installed_api_protocol": self.api_protocol,
        }

    def _exact_server_is_owned(self) -> bool:
        if not self.server_pid:
            return False
        sample = process_sample(self.server_pid)
        if not sample:
            return False
        return str(self.binary) in sample["command"] and Path(self.env["HERDR_SOCKET_PATH"]).parent == self.root

    def cleanup(self) -> list[str]:
        errors: list[str] = []
        if self.client:
            self.client.close()
        socket = Path(self.env["HERDR_SOCKET_PATH"])
        if socket.exists():
            result = self.herdr("server", "stop", check=False)
            if result.returncode:
                errors.append(f"qualified Herdr server stop failed: {result.stderr.strip()}")
        if self.server_process:
            try:
                self.server_process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                if self._exact_server_is_owned():
                    os.kill(self.server_process.pid, signal.SIGTERM)
                    try:
                        self.server_process.wait(timeout=2)
                    except subprocess.TimeoutExpired:
                        errors.append(f"owned Herdr server PID {self.server_process.pid} did not exit after SIGTERM")
                else:
                    errors.append("refused exact-PID cleanup because Herdr ownership could not be proven")
        for stream in (self.server_stdout, self.server_stderr):
            if stream:
                stream.close()
        if self.server_process and self.server_process.poll() is None:
            errors.append(f"preserved private root because server PID {self.server_process.pid} is still live")
        else:
            try:
                shutil.rmtree(self.root)
            except OSError as error:
                errors.append(str(error))
        return errors


def make_instance(product: str, args: argparse.Namespace) -> ProductInstance:
    if product == "masil":
        instance = TmuxInstance(product, args.masil, args.timeout, args.inner_columns, args.inner_rows, args.hidden_inner_columns)
    elif product == "tmux-baseline":
        instance = TmuxInstance(product, args.baseline, args.timeout, args.inner_columns, args.inner_rows, args.hidden_inner_columns)
    elif product == "herdr-0.8.2":
        instance = HerdrInstance(args.herdr, args.timeout, args.inner_columns, args.inner_rows, args.hidden_inner_columns)
    else:
        raise ValueError(product)
    instance.output_style = args.output_style
    return instance


def inner_geometry(instance: ProductInstance) -> list[dict[str, int]]:
    ready = wait_for_event(instance.sidechannels, "ready", instance.timeout)
    return [{"columns": int(item["columns"]), "rows": int(item["rows"])} for item in ready]


def startup_benchmark(product: str, args: argparse.Namespace) -> dict[str, Any]:
    server_ms: list[float] = []
    cli_ms: list[float] = []
    child_ms: list[float] = []
    samples: list[dict[str, Any]] = []
    for index in range(args.startup_samples):
        instance = make_instance(product, args)
        cleanup_errors: list[str] = []
        try:
            instance.start(1, "idle")
            ready = wait_for_event(instance.sidechannels, "ready", args.timeout)[0]
            if instance.started_ns is None or instance.server_ready_ns is None or instance.cli_ready_ns is None:
                raise BenchmarkError("startup timestamps missing")
            server = (instance.server_ready_ns - instance.started_ns) / 1_000_000
            cli = (instance.cli_ready_ns - instance.started_ns) / 1_000_000
            child = (int(ready["monotonic_ns"]) - instance.started_ns) / 1_000_000
            server_ms.append(server)
            cli_ms.append(cli)
            child_ms.append(child)
            samples.append(
                {
                    "index": index,
                    "server_readiness_ms": server,
                    "cli_readiness_ms": cli,
                    "child_readiness_ms": child,
                    "inner_geometry": inner_geometry(instance),
                    "processes": instance.samples(),
                    "isolation": instance.isolation_record(),
                }
            )
        finally:
            cleanup_errors = instance.cleanup()
        if cleanup_errors:
            raise BenchmarkError("; ".join(cleanup_errors))
    return {
        "status": "measured",
        "scope": {
            "server_readiness": "private server reachable; for tmux this is the detached new-session return",
            "cli_readiness": (
                "detached new-session command returned"
                if product != "herdr-0.8.2"
                else "server reachable and workspace-create CLI returned"
            ),
            "child_readiness": "fixture ready sidechannel after raw/noecho PTY setup",
        },
        "server_readiness_ms": distribution(server_ms),
        "cli_readiness_ms": distribution(cli_ms),
        "child_readiness_ms": distribution(child_ms),
        "samples": samples,
    }


def sample_interval(instance: ProductInstance, seconds: float, trigger: Any = None, require_completion: bool = False) -> dict[str, Any]:
    tracked_roles = ("server", "attached_client")
    before = instance.samples(tracked_roles)
    if trigger is not None:
        trigger()
    started = time.monotonic()
    rss_sums: dict[str, list[int]] = {role: [] for role in before}
    complete_at = None
    while True:
        elapsed_now = time.monotonic() - started
        if require_completion and complete_at is None:
            if all(any(row.get("event") == "complete" for row in read_json_lines(path)) for path in instance.sidechannels):
                complete_at = time.monotonic()
        if elapsed_now >= seconds and (not require_completion or (complete_at is not None and time.monotonic() - complete_at >= 0.25)):
            break
        if require_completion and elapsed_now > seconds + instance.timeout:
            raise BenchmarkError("output did not complete within measurement deadline")
        current = instance.samples(tracked_roles)
        for role, rows in current.items():
            rss_sums.setdefault(role, []).append(sum(int(row["rss_kib"]) for row in rows))
        time.sleep(0.1)
    elapsed = time.monotonic() - started
    after = instance.samples(tracked_roles)
    role_results: dict[str, Any] = {}
    for role in sorted(set(before) | set(after)):
        before_cpu = sum(float(row["cpu_time_seconds"]) for row in before.get(role, []))
        after_cpu = sum(float(row["cpu_time_seconds"]) for row in after.get(role, []))
        role_results[role] = {
            "pids": [row["pid"] for row in after.get(role, [])],
            "rss_kib_end_sum": sum(int(row["rss_kib"]) for row in after.get(role, [])),
            "rss_kib_sample_max_sum": max(rss_sums.get(role, []) or [0]),
            "cpu_seconds_delta": max(0.0, after_cpu - before_cpu),
            "average_percent_of_one_core": max(0.0, after_cpu - before_cpu) / elapsed * 100 if elapsed else None,
            "ps_end": after.get(role, []),
        }
    return {"wall_seconds": elapsed, "roles": role_results}


def idle_benchmark(product: str, panes: int, args: argparse.Namespace) -> dict[str, Any]:
    instance = make_instance(product, args)
    try:
        instance.start(panes, "idle")
        return {
            "status": "measured",
            "pane_count": panes,
            "attachment": "headless",
            "inner_geometry": inner_geometry(instance),
            "interval": sample_interval(instance, args.idle_seconds),
            "isolation": instance.isolation_record(),
        }
    finally:
        cleanup = instance.cleanup()
        if cleanup:
            # Cleanup failures must surface as benchmark failures rather than be
            # silently converted into successful measurements.
            raise BenchmarkError("; ".join(cleanup))


def output_benchmark(product: str, panes: int, pattern: str, args: argparse.Namespace) -> dict[str, Any]:
    repeats: list[dict[str, Any]] = []
    for repeat in range(args.throughput_repeats):
        instance = make_instance(product, args)
        try:
            if pattern == "paced":
                per_pane_rate = max(1, args.aggregate_rate_bytes_per_second // panes)
                per_pane_bytes = max(4096, int(per_pane_rate * args.throughput_seconds))
            else:
                per_pane_rate = 0
                per_pane_bytes = max(4096, args.burst_bytes // panes)
            instance.start(panes, "output", per_pane_bytes, per_pane_rate)
            interval_seconds = args.throughput_seconds + 0.75 if pattern == "paced" else 0.75
            process_interval = sample_interval(instance, interval_seconds, trigger=instance.release_output, require_completion=True)
            completed = wait_for_event(instance.sidechannels, "complete", args.timeout + args.throughput_seconds)
            first_start = min(int(item["write_started_ns"]) for item in completed)
            last_complete = max(int(item["write_completed_ns"]) for item in completed)
            offered = sum(int(item["offered_bytes"]) for item in completed)
            duration = (last_complete - first_start) / 1_000_000_000
            repeats.append(
                {
                    "repeat": repeat,
                    "pattern": pattern,
                    "output_style": args.output_style,
                    "attachment": "headless",
                    "completion_evidence": "fixture sidechannel after all PTY writes returned",
                    "offered_bytes": offered,
                    "completed_bytes": offered,
                    "fixture_write_seconds": duration,
                    "fixture_write_bytes_per_second": offered / duration if duration else None,
                    "inner_geometry": inner_geometry(instance),
                    "processes_after_completion": instance.samples(),
                    "server_client_interval": process_interval,
                    "isolation": instance.isolation_record(),
                }
            )
        finally:
            cleanup = instance.cleanup()
            if cleanup:
                raise BenchmarkError("; ".join(cleanup))
    return {
        "status": "measured",
        "pane_count": panes,
        "pattern": pattern,
        "scope": "PTY acceptance throughput in headless mode; no render-latency claim",
        "repeats": repeats,
    }


def input_rtt_benchmark(product: str, args: argparse.Namespace) -> dict[str, Any]:
    instance = make_instance(product, args)
    latencies: list[float] = []
    try:
        instance.start(1, "interactive", attach=True)
        assert instance.client is not None
        ready = instance.fixtures[0]
        instance.client.wait_marker(f"@@MASIL_BENCH_READY:{ready.nonce}:".encode(), args.timeout)
        instance.client.buffer.clear()
        for _ in range(args.rtt_samples):
            token = secrets.token_hex(8).encode("ascii")
            expected = b"@@MASIL_BENCH_ECHO:" + ready.nonce.encode() + b":" + token.hex().encode() + b"@@"
            sent = monotonic_ns()
            instance.client.send(token + b"\n")
            observed = instance.client.wait_marker(expected, args.timeout)
            latencies.append((observed - sent) / 1_000_000)
            instance.client.buffer.clear()
        return {
            "status": "measured",
            "attachment": "real client on Python PTY",
            "host_geometry": {"columns": HOST_COLUMNS, "rows": HOST_ROWS},
            "inner_geometry": inner_geometry(instance),
            "latency_scope": "host PTY write through child PTY and rendered marker back to host PTY",
            "input_to_rendered_marker_ms": distribution(latencies),
            "terminal_probe_responses": instance.client.responses,
            "processes": instance.samples(),
            "isolation": instance.isolation_record(),
        }
    finally:
        cleanup = instance.cleanup()
        if cleanup:
            raise BenchmarkError("; ".join(cleanup))


def product_metadata(product: str, path: Path) -> dict[str, Any]:
    result = subprocess.run([str(path), "--version" if product == "herdr-0.8.2" else "-V"], text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, check=False)
    return {
        "path": str(path.resolve()),
        "sha256": sha256(path),
        "version_output": result.stdout.strip(),
        "version_exit_code": result.returncode,
    }


def run_product(product: str, args: argparse.Namespace) -> dict[str, Any]:
    result: dict[str, Any] = {"status": "measured"}
    try:
        print(f"{product}: startup ({args.startup_samples})", file=sys.stderr, flush=True)
        result["startup"] = startup_benchmark(product, args)
        print(f"{product}: attached RTT ({args.rtt_samples})", file=sys.stderr, flush=True)
        result["attached_input_rtt"] = input_rtt_benchmark(product, args)
        result["idle"] = [idle_benchmark(product, panes, args) for panes in args.pane_counts]
        result["output"] = []
        for pattern in ("burst", "paced"):
            for panes in args.pane_counts:
                print(f"{product}: {pattern} output ({panes} panes)", file=sys.stderr, flush=True)
                result["output"].append(output_benchmark(product, panes, pattern, args))
    except Exception as error:
        result["status"] = "error"
        result["error"] = f"{type(error).__name__}: {error}"
        print(f"{product}: {result['error']}", file=sys.stderr, flush=True)
    return result


def geometry_comparability(products: dict[str, Any]) -> dict[str, Any]:
    """Compare measured inner grids without converting unlike runs into peers."""
    conditions: dict[str, dict[str, Any]] = {}
    for product, product_result in products.items():
        measurements = product_result.get("measurements", {})
        if measurements.get("status") != "measured":
            conditions.setdefault("all", {})[product] = {"status": "unavailable"}
            continue
        attached = measurements["attached_input_rtt"]
        conditions.setdefault("attached_input_rtt", {})[product] = attached["inner_geometry"]
        for item in measurements["idle"]:
            key = f"idle_headless_{item['pane_count']}_panes"
            conditions.setdefault(key, {})[product] = item["inner_geometry"]
        for item in measurements["output"]:
            key = f"output_{item['pattern']}_headless_{item['pane_count']}_panes"
            geometries = [repeat["inner_geometry"] for repeat in item["repeats"]]
            conditions.setdefault(key, {})[product] = geometries

    comparisons: dict[str, Any] = {}
    all_matched = True
    expected_products = set(products)
    for condition, by_product in sorted(conditions.items()):
        if set(by_product) != expected_products:
            matched = False
            reason = "one or more products did not produce this condition"
        else:
            canonical = {json.dumps(value, sort_keys=True) for value in by_product.values()}
            matched = len(canonical) == 1
            reason = "identical measured inner grids" if matched else "measured inner grids differ"
        comparisons[condition] = {
            "matched": matched,
            "reason": reason,
            "by_product": by_product,
        }
        all_matched = all_matched and matched
    return {
        "all_conditions_matched": all_matched,
        "direct_matched_geometry_claim_supported": all_matched,
        "conditions": comparisons,
        "policy": "A condition with different measured inner grids is reported but is not an exact matched-geometry comparison.",
    }


def parse_pane_counts(value: str) -> list[int]:
    try:
        values = [int(item) for item in value.split(",")]
    except ValueError as error:
        raise argparse.ArgumentTypeError("pane counts must be comma-separated integers") from error
    if not values or any(item < 1 for item in values):
        raise argparse.ArgumentTypeError("pane counts must be positive")
    return values


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--profile", choices=("smoke", "full"), default="smoke")
    parser.add_argument("--products", default="masil,tmux-baseline,herdr-0.8.2")
    parser.add_argument("--masil", type=Path, default=ROOT / "bin" / "masil")
    parser.add_argument("--baseline", type=Path, default=ROOT / "bin" / "tmux-baseline")
    parser.add_argument("--herdr", type=Path, default=Path("/Users/ochi/.local/bin/herdr"))
    parser.add_argument("--pane-counts", type=parse_pane_counts, default=parse_pane_counts("1,15,50"))
    parser.add_argument("--inner-columns", type=int, default=93)
    parser.add_argument("--inner-rows", type=int, default=39)
    parser.add_argument("--hidden-inner-columns", type=int, default=94)
    parser.add_argument("--startup-samples", type=int)
    parser.add_argument("--rtt-samples", type=int)
    parser.add_argument("--idle-seconds", type=float)
    parser.add_argument("--throughput-seconds", type=float)
    parser.add_argument("--throughput-repeats", type=int)
    parser.add_argument("--aggregate-rate-bytes-per-second", type=int, default=1024 * 1024)
    parser.add_argument("--burst-bytes", type=int, default=1024 * 1024)
    parser.add_argument("--output-style", choices=("scroll", "inplace"), default="scroll")
    parser.add_argument("--timeout", type=float, default=30.0)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args(argv)

    defaults = (
        {"startup_samples": 3, "rtt_samples": 20, "idle_seconds": 1.0, "throughput_seconds": 1.0, "throughput_repeats": 1}
        if args.profile == "smoke"
        else {"startup_samples": 100, "rtt_samples": 1000, "idle_seconds": 600.0, "throughput_seconds": 60.0, "throughput_repeats": 3}
    )
    for key, value in defaults.items():
        if getattr(args, key) is None:
            setattr(args, key, value)
    if args.profile == "full":
        minimums = {"startup_samples": 100, "rtt_samples": 1000, "idle_seconds": 600.0, "throughput_seconds": 60.0, "throughput_repeats": 3}
        for key, minimum in minimums.items():
            if getattr(args, key) < minimum:
                parser.error(f"full profile requires --{key.replace('_', '-')} >= {minimum}")
    if args.profile == "full" and set(args.pane_counts) != {1, 15, 50}:
        parser.error("full profile requires exactly pane counts 1,15,50")
    if any(getattr(args, key) <= 0 for key in ("inner_columns", "inner_rows", "hidden_inner_columns", "startup_samples", "rtt_samples", "idle_seconds", "throughput_seconds", "throughput_repeats", "aggregate_rate_bytes_per_second", "burst_bytes", "timeout")):
        parser.error("sample counts, durations, rate, and timeout must be positive")

    allowed_products = {"masil", "tmux-baseline", "herdr-0.8.2"}
    args.products = [item.strip() for item in args.products.split(",") if item.strip()]
    unknown = set(args.products) - allowed_products
    if unknown:
        parser.error(f"unknown products: {','.join(sorted(unknown))}")
    return args


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    binaries = {"masil": args.masil, "tmux-baseline": args.baseline, "herdr-0.8.2": args.herdr}
    missing = [f"{product}: {binaries[product]}" for product in args.products if not binaries[product].is_file()]
    if missing:
        raise SystemExit("missing benchmark binaries: " + ", ".join(missing))

    started = datetime.now(timezone.utc)
    report: dict[str, Any] = {
        "schema_version": 1,
        "started_at_utc": started.isoformat(),
        "profile": args.profile,
        "budget_eligible": False,
        "performance_budget_pass": None,
        "methodology": {
            "host_pty": {"columns": HOST_COLUMNS, "rows": HOST_ROWS},
            "required_inner_geometry": {
                "active": {"columns": args.inner_columns, "rows": args.inner_rows},
                "hidden": {"columns": args.hidden_inner_columns, "rows": args.inner_rows},
            },
            "pane_counts": args.pane_counts,
            "startup_samples": args.startup_samples,
            "idle_seconds": args.idle_seconds,
            "throughput_seconds": args.throughput_seconds,
            "throughput_repeats": args.throughput_repeats,
            "aggregate_offered_rate_bytes_per_second": args.aggregate_rate_bytes_per_second,
            "aggregate_burst_bytes": args.burst_bytes,
            "output_style": args.output_style,
            "scrollback": "product defaults; inplace workload overwrites the first row without scrolling",
            "memory": "per-process RSS from ps; shared pages may be double-counted when summed",
            "cpu": "process CPU time delta and ps point samples; percentages are of one logical core",
            "startup": "CLI/server readiness and fixture-child readiness are separate",
            "throughput": "headless PTY acceptance; offered and fixture-completed byte counts are equal by sidechannel evidence",
            "input_rtt": "attached client PTY round trip to a unique marker in a pyte 0.8.2 reconstructed screen; timestamp at read before parser/log work",
        },
        "host": {
            "platform": platform.platform(),
            "machine": platform.machine(),
            "python": sys.version,
            "logical_cpus": os.cpu_count(),
        },
        "products": {},
    }
    for product in args.products:
        report["products"][product] = {
            "binary": product_metadata(product, binaries[product]),
            "measurements": run_product(product, args),
        }
        if args.output:
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.with_suffix(".partial.json").write_text(json.dumps(report, indent=2) + "\n")
    report["geometry_comparability"] = geometry_comparability(report["products"])
    all_measured = all(item["measurements"]["status"] == "measured" for item in report["products"].values())
    report["budget_eligible"] = args.profile == "full" and all_measured
    report["completed_at_utc"] = datetime.now(timezone.utc).isoformat()
    report["caveats"] = [
        "Smoke results are harness validation only and cannot pass documented performance budgets.",
        "Headless throughput is not compared to attached render latency.",
        "Fixture children are reported separately from server and attached-client processes.",
        "No cold-cache claim is made; startup samples are sequential warm-host measurements.",
    ]
    encoded = json.dumps(report, indent=2, sort_keys=True) + "\n"
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(encoded)
    else:
        sys.stdout.write(encoded)
    return 0 if all_measured else 1


if __name__ == "__main__":
    sys.exit(main())
