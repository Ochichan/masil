#!/usr/bin/env python3
"""Narrow tests for the reproducible benchmark harness."""

from __future__ import annotations

import importlib.util
import contextlib
import io
import os
from pathlib import Path
import pty
import select
import shutil
import subprocess
import sys
import tempfile
import time
import unittest


ROOT = Path(__file__).resolve().parents[1]
BENCHMARK_PATH = ROOT / "tests" / "performance" / "benchmark.py"
FIXTURE_PATH = ROOT / "tests" / "performance" / "fixture.py"
SPEC = importlib.util.spec_from_file_location("masil_benchmark", BENCHMARK_PATH)
assert SPEC and SPEC.loader
benchmark = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = benchmark
SPEC.loader.exec_module(benchmark)


class BenchmarkHelpersTest(unittest.TestCase):
    def test_herdr_environment_is_fresh_allowlisted_and_rooted(self) -> None:
        root = Path(tempfile.mkdtemp(prefix="msl-bench-test-", dir="/tmp"))
        self.addCleanup(lambda: shutil.rmtree(root, ignore_errors=True))
        old = os.environ.get("HERDR_SESSION")
        os.environ["HERDR_SESSION"] = "must-not-leak"
        self.addCleanup(
            lambda: os.environ.pop("HERDR_SESSION", None)
            if old is None
            else os.environ.__setitem__("HERDR_SESSION", old)
        )

        env = benchmark.herdr_environment(root)

        self.assertEqual(set(env) - benchmark.HERDR_ALLOWED_ENV, set())
        self.assertTrue(benchmark.HERDR_FORBIDDEN_INHERITED.isdisjoint(env))
        for key in (
            "HOME",
            "XDG_CONFIG_HOME",
            "XDG_STATE_HOME",
            "XDG_DATA_HOME",
            "XDG_RUNTIME_DIR",
            "HERDR_CONFIG_PATH",
            "HERDR_SOCKET_PATH",
            "HERDR_CLIENT_SOCKET_PATH",
            "TMPDIR",
        ):
            self.assertTrue(Path(env[key]).resolve().is_relative_to(root.resolve()), key)

    def test_full_profile_rejects_too_few_samples(self) -> None:
        with self.assertRaises(SystemExit), contextlib.redirect_stderr(io.StringIO()):
            benchmark.parse_args(["--profile", "full", "--startup-samples", "99"])
        with self.assertRaises(SystemExit), contextlib.redirect_stderr(io.StringIO()):
            benchmark.parse_args(["--profile", "full", "--rtt-samples", "999"])

    def test_distribution_uses_nearest_rank_percentiles(self) -> None:
        result = benchmark.distribution([float(value) for value in range(1, 101)])
        self.assertEqual(result["p95"], 95.0)
        self.assertEqual(result["p99"], 99.0)
        self.assertEqual(result["samples"], 100)

    def test_process_cpu_time_parser_handles_macos_formats(self) -> None:
        self.assertEqual(benchmark.parse_cpu_time("0:00.25"), 0.25)
        self.assertEqual(benchmark.parse_cpu_time("1:02:03"), 3723.0)
        self.assertEqual(benchmark.parse_cpu_time("2-01:02:03"), 176523.0)

    def test_herdr_identity_is_dynamic_but_adapter_pairs_are_explicit(self) -> None:
        self.assertEqual(
            benchmark.parse_herdr_identity("herdr 0.9.5\n", "header\nprotocol: 22\n"),
            ("0.9.5", 22),
        )
        benchmark.require_supported_herdr("0.9.5", 22)
        with self.assertRaisesRegex(
            benchmark.BenchmarkError,
            r"unsupported Herdr adapter: observed 0\.9\.6/protocol 23",
        ):
            benchmark.require_supported_herdr("0.9.6", 23)

    def test_geometry_comparison_fails_closed_when_products_differ(self) -> None:
        def product(columns: int) -> dict[str, object]:
            geometry = [{"columns": columns, "rows": 39}]
            return {
                "measurements": {
                    "status": "measured",
                    "attached_input_rtt": {"inner_geometry": geometry},
                    "idle": [{"pane_count": 1, "inner_geometry": geometry}],
                    "output": [],
                }
            }

        result = benchmark.geometry_comparability(
            {"masil": product(93), "herdr": product(94)}
        )

        self.assertFalse(result["all_conditions_matched"])
        self.assertFalse(result["direct_matched_geometry_claim_supported"])

    def test_geometry_comparison_fails_closed_when_measurements_are_unavailable(self) -> None:
        unavailable = {"measurements": {"status": "error"}}
        measured = {
            "measurements": {
                "status": "measured",
                "attached_input_rtt": {"inner_geometry": [{"columns": 93, "rows": 39}]},
                "idle": [],
                "output": [],
            }
        }
        for products in (
            {"masil": unavailable, "herdr": unavailable},
            {"masil": measured, "herdr": unavailable},
        ):
            with self.subTest(products=products):
                result = benchmark.geometry_comparability(products)
                self.assertFalse(result["all_conditions_matched"])
                self.assertFalse(result["direct_matched_geometry_claim_supported"])

    def test_required_process_samples_fail_closed(self) -> None:
        class Instance:
            @staticmethod
            def process_roles() -> dict[str, list[int]]:
                return {"server": [42], "attached_client": []}

        with self.assertRaisesRegex(
            benchmark.BenchmarkError,
            r"server process sample missing at interval end",
        ):
            benchmark.require_process_samples(
                Instance(),
                {"server": [], "attached_client": []},
                ("server", "attached_client"),
                "interval end",
            )

        benchmark.require_process_samples(
            Instance(),
            {"server": [{"pid": 42}], "attached_client": []},
            ("server", "attached_client"),
            "interval end",
        )

    def test_existing_benchmark_artifact_is_never_overwritten(self) -> None:
        directory = Path(tempfile.mkdtemp(prefix="msl-bench-test-", dir="/tmp"))
        self.addCleanup(lambda: shutil.rmtree(directory, ignore_errors=True))
        output = directory / "result.json"
        output.write_text("historical\n")

        with self.assertRaisesRegex(
            benchmark.BenchmarkError,
            r"refusing to overwrite benchmark artifact",
        ):
            benchmark.require_new_output_path(output)


class FixtureTest(unittest.TestCase):
    def test_attached_marker_survives_differential_redraw_and_private_dsr(self) -> None:
        directory = Path(tempfile.mkdtemp(prefix="msl-bench-test-", dir="/tmp"))
        self.addCleanup(lambda: shutil.rmtree(directory, ignore_errors=True))
        program = (
            "import os,tty; tty.setraw(0); "
            "os.write(1,b'\\x1b[?6n\\x1b[1;1HFIRST_OLD_MARKER'); "
            "os.read(0,1); os.write(1,b'\\x1b[1;7HNEW'); os.read(0,1)"
        )
        client = benchmark.AttachedClient(
            [sys.executable, "-c", program],
            {"PATH": os.defpath, "TERM": "xterm-256color"},
            directory,
            directory / "pty.raw",
        )
        self.addCleanup(client.close)
        client.wait_marker(b"FIRST_OLD_MARKER", 3.0)
        client.send(b"x")
        client.wait_marker(b"FIRST_NEW_MARKER", 3.0)
        self.assertNotIn(b"FIRST_NEW_MARKER", client.buffer)

    def test_raw_fixture_reports_geometry_and_unique_echo(self) -> None:
        directory = Path(tempfile.mkdtemp(prefix="msl-bench-test-", dir="/tmp"))
        self.addCleanup(lambda: shutil.rmtree(directory, ignore_errors=True))
        sidechannel = directory / "events.jsonl"
        master, slave = pty.openpty()
        benchmark.set_pty_size(slave, 120, 40)
        process = subprocess.Popen(
            [
                sys.executable,
                str(FIXTURE_PATH),
                "interactive",
                "--sidechannel",
                str(sidechannel),
                "--nonce",
                "unit",
            ],
            stdin=slave,
            stdout=slave,
            stderr=slave,
            close_fds=True,
        )
        os.close(slave)
        self.addCleanup(lambda: process.kill() if process.poll() is None else None)
        self.addCleanup(lambda: os.close(master))

        ready = benchmark.wait_for_event([sidechannel], "ready", 3.0)[0]
        self.assertEqual((ready["columns"], ready["rows"]), (120, 40))
        os.write(master, b"token\n")
        expected = b"@@MASIL_BENCH_ECHO:unit:746f6b656e@@"
        output = bytearray()
        deadline = time.monotonic() + 3.0
        while expected not in output and time.monotonic() < deadline:
            readable, _, _ = select.select([master], [], [], 0.1)
            if readable:
                output.extend(os.read(master, 4096))
        self.assertIn(expected, output)
        os.write(master, b"QUIT\n")
        process.wait(timeout=3)


if __name__ == "__main__":
    unittest.main()
