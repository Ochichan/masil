#!/usr/bin/env python3
"""C2b public prompt delivery through the core input ledger."""

from __future__ import annotations

import base64
import hashlib
import json
import os
from pathlib import Path
import shutil
import sqlite3
import subprocess
import tempfile
from typing import Any
import unittest

from test_compatibility import MASIL, ROOT, Server, wait_for
from test_core_ledger import Peer


AGENT = Path(os.environ.get("MASIL_AGENT", ROOT / "bin" / "masil-agent"))
MANIFEST = """id = "codex"
version = "1.0.0"
min_engine_version = 4
[[rules]]
id = "working"
state = "working"
priority = 20
region = "whole_recent"
contains = ["STATE:working"]
[[rules]]
id = "idle"
state = "idle"
visible_idle = true
priority = 10
region = "whole_recent"
contains = ["STATE:idle"]
"""


def sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def core_bridge(server: Server) -> Path:
    value = server.text("display-message", "-p", "#{masil_bridge_socket}").strip()
    if not value:
        raise AssertionError("server did not publish a core bridge socket")
    return Path(value)


def core_ticket(peer: Peer, sequence=0) -> dict[str, str]:
    return {
        "epoch": peer.hello["capabilities"]["dispatch_epoch"],
        "seq": str(sequence),
    }


def core_input_state(server: Server, pane: str) -> dict[str, Any]:
    fields = server.text(
        "display-message", "-p", "-t", pane,
        "#{masil_core_boot_id}\t#{pane_id}\t#{masil_pty_generation}\t"
        "#{masil_foreground_pgid}\t#{pane_current_command}\t#{pane_dead}\t"
        "#{pane_input_off}\t#{synchronize-panes}\t#{pane_in_mode}\t"
        "#{pane_output_generation}\t#{pane_title}\t#{masil_osc_progress}\t"
        "#{@masil-managed-agent}\t#{@masil-managed-observation}",
    ).rstrip("\n").split("\t")
    if len(fields) != 14:
        raise AssertionError(f"unexpected pane state: {fields!r}")
    (boot, pane_id, pty, foreground, command, dead, input_off, synchronize,
     mode, output, title, progress, meta, tracked) = fields
    if (dead, input_off, synchronize, mode) != ("0", "0", "0", "0"):
        raise AssertionError(f"core input target is not live: {fields!r}")
    return {
        "target": {"core_boot_id": boot, "pane_id": pane_id, "pty_generation": pty},
        "preconditions": {
            "expected_foreground_pgid": foreground,
            "expected_current_command": command,
            "meta_digest": sha256_bytes(meta.encode()),
            "pane_dead": False,
            "input_off": False,
            "synchronize": False,
            "mode": "none",
            "expected_output_generation": output,
            "tracked_digest": sha256_bytes(tracked.encode()),
            "expected_title": title,
            "expected_progress": progress,
        },
    }


def core_operation_key(state: dict[str, Any], label: str) -> dict[str, str]:
    return {
        "environment_id": "c2b-recovery-test",
        "principal": "local",
        "namespace_epoch": state["target"]["core_boot_id"],
        "namespace_nonce": label,
        "operation_id": sha256_bytes(label.encode()),
    }


def commit_staged_core_input(
    server: Server,
    pane: str,
    peer: Peer,
    operation_key: dict[str, str],
    ticket: dict[str, str],
    text: str,
    state: dict[str, Any] | None = None,
    expected_result="applied",
) -> dict[str, Any]:
    state = state or core_input_state(server, pane)
    data = text.encode()
    staging_id = f"reconcile-{ticket['epoch']}-{ticket['seq']}"
    begin = peer.request(
        "input_begin",
        staging_id=staging_id,
        operation_key=operation_key,
        total_len=len(data),
        sha256=sha256_bytes(data),
        core_boot_id=state["target"]["core_boot_id"],
    )
    if begin.get("kind") != "input_begin":
        raise AssertionError(begin)
    for offset in range(0, len(data), 4_608):
        chunk = data[offset:offset + 4_608]
        reply = peer.request(
            "input_chunk",
            staging_id=staging_id,
            offset=offset,
            data_b64=base64.b64encode(chunk).decode(),
        )
        if reply.get("kind") != "input_chunk":
            raise AssertionError(reply)
    reply = peer.request(
        "guarded_action",
        operation_key=operation_key,
        dispatch_ticket=ticket,
        payload_digest=sha256_bytes(f"{staging_id}:{text}".encode()),
        retain=True,
        target=state["target"],
        preconditions=state["preconditions"],
        action={"kind": "input_commit", "staging_id": staging_id,
                "bracketed": False, "submit": "enter"},
    )
    if reply.get("result") != expected_result:
        raise AssertionError(reply)
    return reply


def rewrite_core_prompt_dispatch(
    database: Path,
    operation_key: str,
    core_key: dict[str, str],
    ticket: dict[str, str],
) -> None:
    """Make a durable prompt look admitted, before its final SQLite receipt."""
    with sqlite3.connect(database) as db:
        row = db.execute(
            "SELECT o.id, r.ordinal, r.evidence FROM operations o JOIN receipts r "
            "ON r.operation_id = o.id WHERE o.key = ? AND r.stage = 'dispatch_intent' "
            "ORDER BY r.ordinal DESC LIMIT 1",
            (operation_key,),
        ).fetchone()
        if row is None:
            raise AssertionError("prompt dispatch intent is missing")
        operation_id, ordinal, evidence = row
        intent = json.loads(evidence)
        if not isinstance(intent.get("ticket"), str):
            raise AssertionError(intent)
        # What recovery reads must be what production wrote before it is
        # replaced with the test's own ticket.
        produced = intent.get("core")
        if not (isinstance(produced, dict)
                and isinstance(produced.get("operation_key"), dict)
                and isinstance(produced.get("dispatch_ticket"), dict)
                and {"epoch", "seq"} <= set(produced["dispatch_ticket"])):
            raise AssertionError(f"production prompt intent has no core ticket: {intent}")
        intent["core"] = {"operation_key": core_key, "dispatch_ticket": ticket}
        db.execute(
            "DELETE FROM receipts WHERE operation_id = ? AND ordinal > ?",
            (operation_id, ordinal),
        )
        db.execute(
            "UPDATE receipts SET evidence = ? WHERE operation_id = ? AND ordinal = ?",
            (json.dumps(intent, separators=(",", ":")), operation_id, ordinal),
        )
        db.execute(
            "UPDATE operations SET state = 'dispatching', lease_until_ms = 0 WHERE id = ?",
            (operation_id,),
        )


class CorePrompt(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.fixture = Path(tempfile.mkdtemp(prefix="masil-core-prompt-"))
        subprocess.run(
            [
                "cc",
                "-Wall",
                "-Wextra",
                "-Werror",
                str(ROOT / "tests" / "faults" / "agent.c"),
                "-o",
                str(cls.fixture / "codex"),
            ],
            check=True,
            capture_output=True,
        )
        cls.raw = cls.fixture / "raw"
        cls.raw.mkdir()
        subprocess.run(
            [
                "cc",
                "-Wall",
                "-Wextra",
                "-Werror",
                str(ROOT / "tests" / "faults" / "raw_reader.c"),
                "-o",
                str(cls.raw / "codex"),
            ],
            check=True,
            capture_output=True,
        )

    @classmethod
    def tearDownClass(cls):
        shutil.rmtree(cls.fixture)

    def open_server(self, raw=False):
        server = Server(MASIL)
        config = server.path / "config"
        provider_path = self.raw if raw else self.fixture
        server.env["XDG_CONFIG_HOME"] = str(config)
        server.__enter__()
        self.addCleanup(server.__exit__)
        env = server.env | {
            "PATH": f"{provider_path}:/usr/bin:/bin",
            "XDG_CONFIG_HOME": str(config),
        }
        detection = config / "masil" / "agent-detection"
        detection.mkdir(parents=True)
        (detection / "codex.toml").write_text(MANIFEST)
        server.run("set-environment", "-g", "PATH", env["PATH"])
        server.run("set-environment", "-g", "XDG_CONFIG_HOME", env["XDG_CONFIG_HOME"])
        self.assertEqual(server.text("display-message", "-p", "#{masil_core_actions}").strip(), "1")
        return server, env

    def cli(self, server, env, *args, check=True, timeout=15):
        result = subprocess.run(
            [str(AGENT), "agent", "--socket", str(server.socket), *args],
            env=env,
            text=True,
            capture_output=True,
            timeout=timeout,
        )
        if check:
            self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
            return json.loads(result.stdout)
        return result

    def database(self, server):
        paths = list((server.path / ".local/state/masil/operations").glob("*.sqlite3"))
        self.assertEqual(len(paths), 1, paths)
        return paths[0]

    def start(self, server, env, name="builder"):
        receipt = self.cli(server, env, "start", name, "codex", "--cwd", str(server.path))
        wait_for(lambda: self.cli(server, env, "get", name)["process"] == "running", 5)
        return receipt

    def screen(self, server, pane):
        return server.text("capture-pane", "-p", "-e", "-J", "-t", pane, "-S", "-")

    def test_delivery_replay_uses_the_core_receipt_and_fences_old_options(self):
        server, env = self.open_server()
        started = self.start(server, env)
        pane = started["pane_id"]
        wait_for(lambda: self.cli(server, env, "get", "builder")["state"] == "idle", 5)

        sent = self.cli(
            server,
            env,
            "prompt",
            "builder",
            "working",
            "--run",
            started["run"],
            "--operation",
            "1",
        )
        self.assertEqual((sent["stage"], sent["path"]), ("delivered", "core_ledger"))
        wait_for(lambda: "INPUTS:1" in self.screen(server, pane), 5)
        replay = self.cli(
            server,
            env,
            "prompt",
            "builder",
            "working",
            "--run",
            started["run"],
            "--operation",
            "1",
        )
        self.assertEqual(replay, sent)
        self.assertNotIn("INPUTS:2", self.screen(server, pane))
        for option in ("@masil-agent-prompt-receipts", "@masil-agent-prompt-tickets"):
            self.assertEqual(
                server.text("show-options", "-p", "-qv", "-t", pane, option).strip(),
                "core-ledger-c2",
            )

    def test_a_32k_multiline_prompt_arrives_once_with_bracket_markers(self):
        server, env = self.open_server(raw=True)
        started = self.start(server, env)
        pane = started["pane_id"]
        wait_for(
            lambda: server.text(
                "display-message", "-p", "-t", pane, "#{masil_bracketed_paste}"
            ).strip()
            == "1",
            5,
        )
        self.cli(server, env, "send-keys", "builder", "STATE:idle", "Enter")
        wait_for(lambda: self.cli(server, env, "get", "builder")["state"] == "idle", 5)

        text = ("x" * 63 + "\n") * 512
        self.assertEqual(len(text.encode()), 32 * 1024)
        sent = self.cli(server, env, "prompt", "builder", text)
        self.assertEqual((sent["stage"], sent["path"]), ("delivered", "core_ledger"))
        expected = "^[[200~" + text + "^[[201~^M"
        wait_for(lambda: expected in self.screen(server, pane), 8)

    def test_an_applied_core_ticket_without_a_sqlite_receipt_recovers_once(self):
        server, env = self.open_server()
        started = self.start(server, env)
        pane = started["pane_id"]
        wait_for(lambda: self.cli(server, env, "get", "builder")["state"] == "idle", 5)
        body = "working"
        first = self.cli(
            server, env, "prompt", "builder", body, "--run", started["run"], "--operation", "1",
        )
        self.assertEqual(first["stage"], "delivered")
        wait_for(lambda: "INPUTS:1" in self.screen(server, pane), 5)
        key = f"run:{started['run']}/prompt/1"
        self.cli(server, env, "coordinator", "stop")
        peer = Peer(core_bridge(server), "coordinator")
        try:
            state = core_input_state(server, pane)
            ticket = core_ticket(peer)
            operation_key = core_operation_key(state, "applied-without-sqlite-receipt")
            rewrite_core_prompt_dispatch(self.database(server), key, operation_key, ticket)
            commit_staged_core_input(server, pane, peer, operation_key, ticket, body)
        finally:
            peer.close()
        wait_for(lambda: "INPUTS:2" in self.screen(server, pane), 5)
        self.cli(server, env, "coordinator", "start")
        record = self.cli(server, env, "operation", key)
        self.assertEqual(record["state"], "delivered")
        self.assertIn("delivered", [receipt["stage"] for receipt in record["receipts"]])
        retry = self.cli(
            server,
            env,
            "prompt",
            "builder",
            body,
            "--run",
            started["run"],
            "--operation",
            "1",
        )
        self.assertEqual((retry["stage"], retry["path"]), ("delivered", "core_ledger"))
        pane_text = self.screen(server, pane)
        self.assertIn("INPUTS:2", pane_text)
        self.assertNotIn("INPUTS:3", pane_text)

    def test_reconcile_marks_an_expired_core_dispatch_without_a_startup_receipt_unknown(self):
        server, env = self.open_server()
        started = self.start(server, env)
        first = self.cli(
            server, env, "prompt", "builder", "idle", "--run", started["run"], "--operation", "1",
        )
        pane = started["pane_id"]
        wait_for(lambda: "INPUTS:1" in self.screen(server, pane), 5)
        state = core_input_state(server, pane)
        rewrite_core_prompt_dispatch(
            self.database(server),
            first["operation_key"],
            core_operation_key(state, "expired-without-startup-receipt"),
            {"epoch": "999999", "seq": "0"},
        )
        # The coordinator is already live. `ensure` cannot perform a startup
        # reconciliation, so operations reconcile must settle the expired
        # dispatch itself instead of leaving it forever dispatching.
        self.cli(server, env, "operations", "reconcile")
        record = self.cli(server, env, "operation", first["operation_key"])
        self.assertEqual(record["state"], "outcome_unknown")

    def test_an_unknown_core_prompt_is_released_only_by_the_user_and_the_next_is_delivered(self):
        server, env = self.open_server()
        started = self.start(server, env)
        pane = started["pane_id"]
        wait_for(lambda: self.cli(server, env, "get", "builder")["state"] == "idle", 5)
        first = self.cli(
            server, env, "prompt", "builder", "idle", "--run", started["run"], "--operation", "1",
        )
        self.assertEqual((first["stage"], first["path"]), ("delivered", "core_ledger"))
        wait_for(lambda: "INPUTS:1" in self.screen(server, pane), 5)
        # The coordinator was lost between the core's ticket and the receipt,
        # and the dispatch expired: what happened to the prompt is not known.
        rewrite_core_prompt_dispatch(
            self.database(server),
            first["operation_key"],
            core_operation_key(core_input_state(server, pane), "unknown-then-released"),
            {"epoch": "999999", "seq": "0"},
        )
        self.cli(server, env, "operations", "reconcile")
        record = self.cli(server, env, "operation", first["operation_key"])
        self.assertEqual(record["state"], "outcome_unknown")
        # No prompt goes to the pane until the person decides.
        refused = self.cli(server, env, "prompt", "builder", "working", check=False)
        self.assertNotEqual(refused.returncode, 0)
        self.assertNotIn("INPUTS:2", self.screen(server, pane))
        wrong = self.cli(
            server, env, "operation", "resolve", first["operation_key"], "--as", "maybe",
            check=False,
        )
        self.assertNotEqual(wrong.returncode, 0)
        resolved = self.cli(
            server, env, "operation", "resolve", first["operation_key"], "--as", "not-delivered",
            check=False,
        )
        self.assertEqual(resolved.returncode, 5, resolved.stderr)
        self.assertEqual(json.loads(resolved.stdout)["state"], "user_confirmed_not_delivered")
        again = self.cli(
            server, env, "operation", "resolve", first["operation_key"], "--as", "delivered",
            check=False,
        )
        self.assertNotEqual(again.returncode, 0)
        sent = self.cli(server, env, "prompt", "builder", "working")
        self.assertEqual((sent["stage"], sent["path"]), ("delivered", "core_ledger"))
        wait_for(lambda: "INPUTS:2" in self.screen(server, pane), 5)
        self.assertNotIn("INPUTS:3", self.screen(server, pane))


if __name__ == "__main__":
    unittest.main()
