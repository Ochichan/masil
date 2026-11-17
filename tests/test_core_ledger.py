#!/usr/bin/env python3
"""C1a coordinator action ledger coverage against private bridge servers."""

from __future__ import annotations

import copy
import hashlib
import json
import os
import re
from pathlib import Path
import select
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import time
from typing import Any
import unittest


ROOT = Path(__file__).resolve().parents[1]
MASIL = ROOT / "bin" / "masil"


def wait_for(predicate, timeout=3, message="condition not satisfied before deadline"):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.02)
    raise AssertionError(message)


def isolated_env(path: Path) -> dict[str, str]:
    directories = {}
    for key, name in {
        "HOME": "h",
        "XDG_CONFIG_HOME": "c",
        "XDG_CACHE_HOME": "a",
        "XDG_DATA_HOME": "d",
        "XDG_STATE_HOME": "e",
        "XDG_RUNTIME_DIR": "r",
        "TMPDIR": "t",
        "TMUX_TMPDIR": "t",
    }.items():
        directory = path / name
        directory.mkdir(mode=0o700, exist_ok=True)
        directories[key] = str(directory)
    return {
        "PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
        "SHELL": "/bin/sh",
        "TERM": "xterm-256color",
        "LC_CTYPE": "en_US.UTF-8" if sys.platform == "darwin" else "C.UTF-8",
        **directories,
    }


class Server:
    def __init__(self, bridge_value: str | None = None):
        self.temp = tempfile.TemporaryDirectory(prefix="mcl-", dir="/tmp")
        self.path = Path(self.temp.name)
        os.chmod(self.path, 0o700)
        self.socket = self.path / "s"
        self.env = isolated_env(self.path)
        if bridge_value is not None:
            self.env["MASIL_BRIDGE_SOCKET"] = bridge_value
        self.started = False
        self.server_pid: int | None = None

    def argv(self, *args) -> list[str]:
        return [str(MASIL), "-S", str(self.socket), "-f", "/dev/null", *map(str, args)]

    def run(self, *args, check=True, timeout=5):
        return subprocess.run(
            self.argv(*args),
            env=self.env,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            check=check,
            timeout=timeout,
        )

    def text(self, *args, **kwargs) -> str:
        return self.run(*args, **kwargs).stdout

    def start(self):
        self.started = True
        try:
            self.run("new-session", "-d", "-s", "main", "-x", "100", "-y", "30", "/bin/cat")
            self.server_pid = int(self.text("display-message", "-p", "#{pid}").strip())
        except BaseException:
            self.close()
            raise
        return self

    @property
    def bridge(self) -> Path | None:
        value = self.text("display-message", "-p", "#{masil_bridge_socket}").strip()
        return Path(value) if value else None

    @property
    def active_bridge(self) -> Path:
        bridge = self.bridge
        if bridge is None:
            raise AssertionError("server did not publish a bridge socket")
        return bridge

    def alive(self) -> bool:
        if self.server_pid is None:
            return False
        try:
            os.kill(self.server_pid, 0)
        except OSError:
            return False
        return True

    def close(self):
        if self.started:
            try:
                self.run("kill-server", check=False, timeout=2)
            except (OSError, subprocess.TimeoutExpired):
                pass
            self.started = False
        if self.alive() and self.server_pid is not None:
            os.kill(self.server_pid, signal.SIGTERM)
            try:
                wait_for(lambda: not self.alive(), timeout=1)
            except AssertionError:
                os.kill(self.server_pid, signal.SIGKILL)
        self.temp.cleanup()

    def __enter__(self):
        return self.start()

    def __exit__(self, *_):
        self.close()


def receive_exact(connection: socket.socket, count: int) -> bytes:
    result = bytearray()
    while len(result) < count:
        part = connection.recv(count - len(result))
        if not part:
            raise EOFError("bridge connection ended during a frame")
        result.extend(part)
    return bytes(result)


def send_request(connection: socket.socket, payload: dict[str, object]) -> dict[str, Any]:
    body = json.dumps(payload, separators=(",", ":")).encode()
    connection.sendall(struct.pack(">I", len(body)) + body)
    length = struct.unpack(">I", receive_exact(connection, 4))[0]
    if not 0 < length <= 65536:
        raise AssertionError(f"invalid bridge response length {length}")
    return json.loads(receive_exact(connection, length))


class Peer:
    def __init__(self, path: Path, role: str | None = None):
        self.socket = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.socket.settimeout(2)
        self.socket.connect(str(path))
        self.closed = False
        hello: dict[str, object] = {"v": 1, "kind": "hello", "request_id": "hello"}
        if role is not None:
            hello["role"] = role
        try:
            self.hello = send_request(self.socket, hello)
            self.request_number = 0
        except BaseException:
            self.close()
            raise

    def request(self, kind: str, **fields: object) -> dict[str, Any]:
        self.request_number += 1
        return send_request(self.socket, {
            "v": 1,
            "kind": kind,
            "request_id": f"r{self.request_number}",
            **fields,
        })

    def close(self):
        if not self.closed:
            self.socket.close()
            self.closed = True


def sha256(text: str) -> str:
    return hashlib.sha256(text.encode()).hexdigest()


def action_key(index: int | str) -> dict[str, str]:
    return {
        "environment_id": "ledger-test",
        "principal": "local-user",
        "namespace_epoch": "1",
        "namespace_nonce": "c1a-test-nonce",
        "operation_id": str(index),
    }


def action_key_digest(key: dict[str, str]) -> str:
    fields = ("environment_id", "principal", "namespace_epoch", "namespace_nonce", "operation_id")
    canonical = b"".join(
        bytes((len(key[field].encode()),)) + key[field].encode()
        for field in fields
    )
    return hashlib.sha256(canonical).hexdigest()


def strip_sentinels(text: str) -> str:
    # Earlier sentinels stay on screen; only a stray effect may differ.
    return re.sub(r"mcl-sentinel-[A-Za-z0-9_-]+?-[0-9]+", "", text).rstrip("\n")


class CoreLedger(unittest.TestCase):
    def setUp(self):
        self.server = Server().start()
        self.server.run("set-option", "-p", "-t", "main", "@masil-managed-agent", "test-agent")
        self.server.run("set-option", "-p", "-t", "main", "@masil-managed-observation", "test-observation")
        self.bridge = self.server.active_bridge
        self.sentinel_number = 0

    def tearDown(self):
        self.server.close()

    def coordinator(self) -> Peer:
        peer = Peer(self.bridge, "coordinator")
        self.assertEqual(peer.hello["kind"], "hello")
        self.assertIs(peer.hello["capabilities"]["actions"], True)
        self.assertRegex(peer.hello["capabilities"]["dispatch_epoch"], r"^[1-9][0-9]*$")
        return peer

    def state(self, target="main", assert_live=True) -> dict[str, Any]:
        fields = self.server.text(
            "display-message", "-p", "-t", target,
            "#{masil_core_boot_id}\t#{pane_id}\t#{masil_pty_generation}\t"
            "#{masil_foreground_pgid}\t#{pane_current_command}\t#{pane_dead}\t"
            "#{pane_input_off}\t#{synchronize-panes}\t#{pane_in_mode}\t"
            "#{pane_output_generation}\t#{pane_title}\t#{masil_osc_progress}\t"
            "#{@masil-managed-agent}\t#{@masil-managed-observation}",
        ).rstrip("\n").split("\t")
        if len(fields) != 14:
            raise AssertionError(f"unexpected pane state: {fields!r}")
        (boot, pane, pty, foreground, command, dead, input_off, synchronize,
         mode, output, title, progress, meta, tracked) = fields
        if assert_live:
            self.assertEqual((dead, input_off, synchronize, mode), ("0", "0", "0", "0"))
        return {
            "target": {"core_boot_id": boot, "pane_id": pane, "pty_generation": pty},
            "preconditions": {
                "expected_foreground_pgid": foreground,
                "expected_current_command": command,
                "meta_digest": sha256(meta),
                "pane_dead": False,
                "input_off": False,
                "synchronize": False,
                "mode": "none",
                "expected_output_generation": output,
                "tracked_digest": sha256(tracked),
                "expected_title": title,
                "expected_progress": progress,
            },
        }

    def payload(self, peer: Peer, seq: int, operation: int | str, keys: list[str],
                retain=True, payload_digest: str | None = None,
                state: dict[str, Any] | None = None) -> dict[str, object]:
        state = state or self.state()
        epoch = peer.hello["capabilities"]["dispatch_epoch"]
        return {
            "operation_key": action_key(operation),
            "dispatch_ticket": {"epoch": epoch, "seq": str(seq)},
            "payload_digest": payload_digest or sha256(f"payload:{operation}:{keys}"),
            "retain": retain,
            "target": state["target"],
            "preconditions": state["preconditions"],
            "action": {"kind": "keys", "keys": keys},
        }

    def capture(self) -> str:
        return self.server.text("capture-pane", "-p", "-e", "-J", "-t", "main", "-S", "-")

    def assert_code(self, response: dict[str, Any], code: str):
        self.assertEqual(response.get("kind"), "error", response)
        self.assertEqual(response.get("code"), code, response)

    def assert_rejected(self, response: dict[str, Any], reason: str):
        self.assertEqual(response.get("result"), f"rejected_before_effect:{reason}", response)

    def _sentinel(self, label: str) -> str:
        self.sentinel_number += 1
        return f"mcl-sentinel-{label}-{self.sentinel_number}"

    def assert_no_effect_after_ledger_sentinel(self, peer: Peer, seq: int,
                                                before: str, label: str):
        sentinel = self._sentinel(label)
        request = self.payload(peer, seq, f"sentinel-{label}-{seq}", [sentinel],
                               retain=False)
        response = peer.request("guarded_action", **request)
        self.assertEqual(response["result"], "applied", response)
        wait_for(lambda: sentinel in self.capture(), message="sentinel was not typed")
        self.assertEqual(strip_sentinels(self.capture()), strip_sentinels(before))

    def assert_no_effect_after_server_sentinel(self, before: str, label: str):
        sentinel = self._sentinel(label)
        self.server.run("send-keys", "-t", "main", sentinel)
        wait_for(lambda: sentinel in self.capture(), message="sentinel was not typed")
        self.assertEqual(strip_sentinels(self.capture()), strip_sentinels(before))

    def reconnect_coordinator(self) -> Peer:
        peers: list[Peer] = []

        def connect():
            try:
                peer = Peer(self.bridge, "coordinator")
            except (ConnectionError, OSError):
                return False
            if peer.hello.get("kind") == "hello":
                peers.append(peer)
                return True
            peer.close()
            return False

        wait_for(connect, message="closed coordinator epoch was not released")
        return peers[0]

    def test_hello_capability_and_format_gate(self):
        ordinary = Peer(self.bridge)
        coordinator = None
        try:
            self.assertIs(ordinary.hello["capabilities"]["actions"], False)
            self.assertNotIn("dispatch_epoch", ordinary.hello["capabilities"])
            coordinator = self.coordinator()
            self.assertEqual(self.server.text("display-message", "-p", "#{masil_core_actions}").strip(), "1")
        finally:
            ordinary.close()
            if coordinator is not None:
                coordinator.close()

        with Server("off") as disabled:
            self.assertIsNone(disabled.bridge)
            self.assertEqual(disabled.text("display-message", "-p", "#{masil_core_actions}").strip(), "0")

    def test_ordinary_connection_requires_coordinator(self):
        coordinator = self.coordinator()
        ordinary = Peer(self.bridge)
        try:
            self.assert_code(ordinary.request("guarded_action"), "coordinator_required")
            self.assert_code(ordinary.request("retire_receipt"), "coordinator_required")
            self.assert_code(ordinary.request("ledger_query"), "coordinator_required")
            self.assert_code(ordinary.request("ledger_list"), "coordinator_required")
            self.assert_code(ordinary.request("ledger_epochs"), "coordinator_required")
        finally:
            ordinary.close()
            coordinator.close()

    def test_keys_and_precondition_rejections_do_not_type(self):
        coordinator = self.coordinator()
        try:
            first = self.payload(coordinator, 0, "keys-ok", ["core-ledger-text"])
            reply = coordinator.request("guarded_action", **first)
            self.assertEqual(reply["result"], "applied", reply)
            wait_for(lambda: "core-ledger-text" in self.capture(), message="cat did not receive keys")

            before = self.capture()
            expected = {
                "core_boot_changed": lambda request: request["target"].update(core_boot_id="other-boot"),
                "pty_generation_changed": lambda request: request["target"].update(pty_generation="999999"),
                "foreground_pgid_changed": lambda request: request["preconditions"].update(expected_foreground_pgid="0"),
                "current_command_changed": lambda request: request["preconditions"].update(expected_current_command="not-cat"),
                "meta_changed": lambda request: request["preconditions"].update(meta_digest="0" * 64),
                "pane_dead": lambda request: request["preconditions"].update(pane_dead=True),
                "input_off": lambda request: request["preconditions"].update(input_off=True),
                "synchronize_panes": lambda request: request["preconditions"].update(synchronize=True),
                "output_changed": lambda request: request["preconditions"].update(expected_output_generation="999999"),
                "tracked_changed": lambda request: request["preconditions"].update(tracked_digest="0" * 64),
                "title_changed": lambda request: request["preconditions"].update(expected_title="other-title"),
                "progress_changed": lambda request: request["preconditions"].update(expected_progress="4;1;99"),
            }
            for index, (reason, mutate) in enumerate(expected.items()):
                # Two tickets per case, so one failing case cannot shift the next.
                sequence = 1 + 2 * index
                with self.subTest(reason=reason):
                    request = self.payload(coordinator, sequence, f"reject-{reason}", ["must-not-type"], retain=False)
                    mutate(request)
                    self.assert_rejected(coordinator.request("guarded_action", **request), reason)
                    self.assert_no_effect_after_ledger_sentinel(
                        coordinator, sequence + 1, before, reason,
                    )
        finally:
            coordinator.close()

    def test_invalid_mouse_mode_and_synchronize_reject_before_effect(self):
        coordinator = self.coordinator()
        try:
            before = self.capture()
            mouse = self.payload(coordinator, 0, "mouse", ["MouseDown1Pane"], retain=False)
            self.assert_rejected(coordinator.request("guarded_action", **mouse), "invalid_key")
            self.assert_no_effect_after_ledger_sentinel(coordinator, 1, before, "mouse")

            copy_state = self.state()
            self.server.run("copy-mode", "-t", "main")
            copied = self.payload(coordinator, 2, "copy-mode", ["must-not-type"], retain=False, state=copy_state)
            self.assert_rejected(coordinator.request("guarded_action", **copied), "mode_active")
            self.server.run("send-keys", "-t", "main", "q")
            self.assert_no_effect_after_server_sentinel(before, "copy-mode")

            synchronize_state = self.state()
            self.server.run("set-option", "-w", "-t", "main", "synchronize-panes", "on")
            synchronized = self.payload(coordinator, 3, "synchronize", ["must-not-type"], retain=False, state=synchronize_state)
            self.assert_rejected(coordinator.request("guarded_action", **synchronized), "synchronize_panes")
            self.server.run("set-option", "-w", "-t", "main", "synchronize-panes", "off")
            self.assert_no_effect_after_server_sentinel(before, "synchronize")
        finally:
            coordinator.close()

    def test_replay_conflict_and_nonretained_receipts(self):
        coordinator = self.coordinator()
        try:
            request = self.payload(coordinator, 0, "replay", ["only-once"])
            first = coordinator.request("guarded_action", **request)
            self.assertEqual(first["result"], "applied", first)
            wait_for(lambda: "only-once" in self.capture())
            once = self.capture()
            replay = coordinator.request("guarded_action", **request)
            self.assertEqual(replay["result"], "applied", replay)
            self.assert_no_effect_after_ledger_sentinel(coordinator, 1, once, "replay")

            conflict = copy.deepcopy(request)
            conflict["payload_digest"] = sha256("different-payload")
            conflict["action"] = {"kind": "keys", "keys": ["must-not-type"]}
            self.assert_code(coordinator.request("guarded_action", **conflict), "idempotency_conflict")
            self.assert_no_effect_after_ledger_sentinel(coordinator, 2, once, "conflict")

            ephemeral = self.payload(coordinator, 3, "ephemeral", ["one-shot"], retain=False)
            self.assertEqual(coordinator.request("guarded_action", **ephemeral)["result"], "applied")
            wait_for(lambda: "one-shot" in self.capture())
            one_shot = self.capture()
            self.assert_code(coordinator.request("guarded_action", **ephemeral), "receipt_not_retained")
            self.assert_no_effect_after_ledger_sentinel(coordinator, 4, one_shot, "ephemeral")
        finally:
            coordinator.close()

    def test_later_actions_are_explicitly_unsupported(self):
        coordinator = self.coordinator()
        try:
            for sequence, kind in enumerate(("input_commit", "launch", "focus")):
                request = self.payload(coordinator, sequence, f"unsupported-{kind}", [], retain=False)
                request["action"] = {"kind": kind}
                reply = coordinator.request("guarded_action", **request)
                self.assertEqual(reply["result"], "unsupported_action", reply)
        finally:
            coordinator.close()

    def test_ledger_query_list_retire_and_full_capacity(self):
        coordinator = self.coordinator()
        try:
            gap = self.payload(coordinator, 1, "gap", [], retain=False)
            self.assert_code(coordinator.request("guarded_action", **gap), "ticket_gap")
            first = self.payload(coordinator, 0, "listed", [])
            response = coordinator.request("guarded_action", **first)
            self.assertEqual(response["result"], "applied", response)
            ticket = first["dispatch_ticket"]
            boot = first["target"]["core_boot_id"]
            query = coordinator.request(
                "ledger_query", core_boot_id=boot, dispatch_ticket=ticket,
            )
            self.assertEqual(query["result"], "applied", query)
            listed = coordinator.request(
                "ledger_list", core_boot_id=boot, epoch=ticket["epoch"],
            )
            entry = next(item for item in listed["entries"] if item["operation_id"] == "listed")
            self.assertEqual(entry["operation_key_digest"], action_key_digest(first["operation_key"]))
            self.assertEqual(entry["operation_key"], first["operation_key"])
            self.assertEqual(listed["next_seq"], ticket["seq"])
            self.assertIs(listed["truncated"], False)

            mismatch = coordinator.request(
                "retire_receipt", core_boot_id=boot,
                operation_key=first["operation_key"], dispatch_ticket=ticket,
                result_digest="0" * 64, store_revision="1",
            )
            self.assert_code(mismatch, "retire_mismatch")
            self.assert_code(
                coordinator.request("ledger_query", core_boot_id="other-boot", dispatch_ticket=ticket),
                "boot_mismatch",
            )
            self.assert_code(
                coordinator.request("ledger_list", core_boot_id="other-boot", epoch=ticket["epoch"]),
                "boot_mismatch",
            )
            self.assert_code(
                coordinator.request(
                    "retire_receipt", core_boot_id="other-boot",
                    operation_key=first["operation_key"], dispatch_ticket=ticket,
                    result_digest=response["result_digest"], store_revision="1",
                ),
                "boot_mismatch",
            )
            query = coordinator.request(
                "ledger_query", core_boot_id=boot, dispatch_ticket=ticket,
            )
            self.assertEqual(query["result"], "applied", query)
            retired = coordinator.request(
                "retire_receipt", core_boot_id=boot, operation_key=first["operation_key"],
                dispatch_ticket=ticket, result_digest=response["result_digest"], store_revision="1",
            )
            self.assertIs(retired["retired"], True, retired)
            self.assert_code(
                coordinator.request("ledger_query", core_boot_id=boot, dispatch_ticket=ticket),
                "receipt_not_retained",
            )
        finally:
            coordinator.close()


    def test_ledger_full_then_retire_frees_a_slot(self):
        coordinator = self.coordinator()
        try:
            state = self.state()
            first_request: dict[str, object] | None = None
            first_reply: dict[str, Any] | None = None
            for sequence in range(2048):
                request = self.payload(coordinator, sequence, f"fill-{sequence}", [], state=state)
                reply = coordinator.request("guarded_action", **request)
                self.assertEqual(reply["result"], "applied", (sequence, reply))
                if sequence == 0:
                    first_request, first_reply = request, reply
            full = self.payload(coordinator, 2048, "full", [], state=state)
            self.assert_code(coordinator.request("guarded_action", **full), "ledger_full")
            assert first_request is not None and first_reply is not None
            retired = coordinator.request(
                "retire_receipt", core_boot_id=state["target"]["core_boot_id"],
                operation_key=first_request["operation_key"],
                dispatch_ticket=first_request["dispatch_ticket"],
                result_digest=first_reply["result_digest"], store_revision="2",
            )
            self.assertIs(retired["retired"], True, retired)
            after_free = self.payload(coordinator, 2049, "after-free", [], state=state)
            self.assertEqual(coordinator.request("guarded_action", **after_free)["result"], "applied")
        finally:
            coordinator.close()

    def test_ledger_list_paginates_retained_entries(self):
        coordinator = self.coordinator()
        try:
            state = self.state()
            for sequence in range(320):
                request = self.payload(
                    coordinator, sequence, f"page-{sequence}", [], state=state,
                )
                reply = coordinator.request("guarded_action", **request)
                self.assertEqual(reply["result"], "applied", (sequence, reply))

            after = None
            entries: list[dict[str, Any]] = []
            saw_truncated = False
            while True:
                fields: dict[str, object] = {
                    "core_boot_id": state["target"]["core_boot_id"],
                    "epoch": coordinator.hello["capabilities"]["dispatch_epoch"],
                }
                if after is not None:
                    fields["after_seq"] = after
                page = coordinator.request("ledger_list", **fields)
                self.assertEqual(page["kind"], "ledger_list", page)
                self.assertTrue(page["entries"], page)
                entries.extend(page["entries"])
                if not page["truncated"]:
                    self.assertEqual(page["next_seq"], entries[-1]["dispatch_ticket"]["seq"])
                    break
                saw_truncated = True
                after = page["next_seq"]

            self.assertTrue(saw_truncated)
            self.assertEqual(
                [entry["dispatch_ticket"]["seq"] for entry in entries],
                [str(sequence) for sequence in range(320)],
            )
            self.assertEqual(entries[0]["operation_key_digest"], action_key_digest(action_key("page-0")))
        finally:
            coordinator.close()

    def test_closed_epoch_query_list_retire_and_kill_pane(self):
        first = self.coordinator()
        initial = self.payload(first, 0, "old-epoch", [])
        try:
            reply = first.request("guarded_action", **initial)
            self.assertEqual(reply["result"], "applied")
            old_epoch = initial["dispatch_ticket"]["epoch"]
            boot = initial["target"]["core_boot_id"]
        finally:
            first.close()

        replacement = self.reconnect_coordinator()
        try:
            self.assertNotEqual(replacement.hello["capabilities"]["dispatch_epoch"], old_epoch)
            self.assert_code(replacement.request("guarded_action", **initial), "epoch_closed")
            not_applied = replacement.request(
                "ledger_query", core_boot_id=boot,
                dispatch_ticket={"epoch": old_epoch, "seq": "1"},
            )
            self.assertEqual(not_applied["result"], "not_applied", not_applied)
            listed = replacement.request(
                "ledger_list", core_boot_id=boot, epoch=old_epoch,
            )
            self.assertEqual([entry["operation_id"] for entry in listed["entries"]], ["old-epoch"])
            retired = replacement.request(
                "retire_receipt", core_boot_id=boot,
                operation_key=initial["operation_key"], dispatch_ticket=initial["dispatch_ticket"],
                result_digest=reply["result_digest"], store_revision="3",
            )
            self.assertIs(retired["retired"], True, retired)
            self.assert_code(
                replacement.request(
                    "ledger_query", core_boot_id=boot, dispatch_ticket=initial["dispatch_ticket"],
                ),
                "receipt_not_retained",
            )

            self.server.run("new-window", "-d", "-t", "main", "-n", "keep", "/bin/cat")
            kill = self.payload(replacement, 0, "kill-pane", [])
            kill["action"] = {"kind": "kill_pane"}
            self.assertEqual(replacement.request("guarded_action", **kill)["result"], "applied")
            wait_for(lambda: kill["target"]["pane_id"] not in self.server.text("list-panes", "-a", "-F", "#{pane_id}"))
        finally:
            replacement.close()

    def test_retained_epoch_survives_reconnects(self):
        first = self.coordinator()
        initial = self.payload(first, 0, "retained-first", [])
        reply = first.request("guarded_action", **initial)
        self.assertEqual(reply["result"], "applied", reply)
        boot = initial["target"]["core_boot_id"]
        first.close()

        for _ in range(9):
            peer = self.reconnect_coordinator()
            peer.close()

        current = self.reconnect_coordinator()
        try:
            query = current.request(
                "ledger_query", core_boot_id=boot, dispatch_ticket=initial["dispatch_ticket"],
            )
            self.assertEqual(query["result"], "applied", query)
            listed = current.request(
                "ledger_list", core_boot_id=boot, epoch=initial["dispatch_ticket"]["epoch"],
            )
            self.assertEqual([entry["operation_id"] for entry in listed["entries"]], ["retained-first"])
            retired = current.request(
                "retire_receipt", core_boot_id=boot,
                operation_key=initial["operation_key"], dispatch_ticket=initial["dispatch_ticket"],
                result_digest=reply["result_digest"], store_revision="4",
            )
            self.assertIs(retired["retired"], True, retired)
        finally:
            current.close()

    def test_retained_epochs_do_not_exhaust_coordinator(self):
        retained: list[tuple[dict[str, object], dict[str, Any]]] = []
        boot = ""
        for index in range(10):
            peer = self.coordinator() if index == 0 else self.reconnect_coordinator()
            try:
                request = self.payload(peer, 0, f"closed-{index}", [])
                reply = peer.request("guarded_action", **request)
                self.assertEqual(reply["result"], "applied", reply)
                retained.append((request, reply))
                boot = request["target"]["core_boot_id"]
            finally:
                peer.close()

        current = self.reconnect_coordinator()
        try:
            epochs = current.request("ledger_epochs", core_boot_id=boot)
            self.assertEqual(epochs["kind"], "ledger_epochs", epochs)
            self.assertEqual(
                [(item["epoch"], item["watermark"], item["retained"])
                 for item in epochs["epochs"]],
                [(str(index + 1), "1", "1") for index in range(10)] +
                [("11", "0", "0")],
            )
            first, first_reply = retained[0]
            query = current.request(
                "ledger_query", core_boot_id=boot,
                dispatch_ticket=first["dispatch_ticket"],
            )
            self.assertEqual(query["result"], "applied", query)
            retired = current.request(
                "retire_receipt", core_boot_id=boot,
                operation_key=first["operation_key"],
                dispatch_ticket=first["dispatch_ticket"],
                result_digest=first_reply["result_digest"], store_revision="5",
            )
            self.assertIs(retired["retired"], True, retired)
        finally:
            current.close()

    def test_extended_key_mode_sends_encoded_keys_like_send_keys(self):
        self.server.run("set-option", "-g", "extended-keys", "always")
        # The option applies to panes created after it is set.
        self.server.run("respawn-pane", "-k", "-t", "main", "/bin/cat")
        coordinator = self.coordinator()
        try:
            stock_pane = self.server.text(
                "new-window", "-d", "-P", "-F", "#{pane_id}", "/bin/cat",
            ).strip()
            for pane in ("main", stock_pane):
                wait_for(
                    lambda pane=pane: self.server.text(
                        "display-message", "-p", "-t", pane, "#{pane_key_mode}",
                    ).strip().startswith("Ext"),
                    message="extended keys were not enabled",
                )

            def stock_capture():
                return self.server.text(
                    "capture-pane", "-p", "-e", "-J", "-t", stock_pane, "-S", "-",
                )

            keys = ["X", "C-#", "Y"]
            request = self.payload(coordinator, 0, "extended", keys, retain=False)
            self.assertEqual(coordinator.request("guarded_action", **request)["result"], "applied")
            self.server.run("send-keys", "-t", stock_pane, *keys)
            wait_for(
                lambda: "Y" in self.capture() and "Y" in stock_capture(),
                message="extended keys did not reach both panes",
            )
            self.assertNotIn("C-#", stock_capture())
            self.assertEqual(self.capture(), stock_capture())
        finally:
            coordinator.close()

    def test_literal_key_fallback_and_real_pane_preconditions(self):
        coordinator = self.coordinator()
        try:
            stock_pane = self.server.text(
                "new-window", "-d", "-P", "-F", "#{pane_id}", "/bin/cat",
            ).strip()

            def stock_capture():
                return self.server.text(
                    "capture-pane", "-p", "-e", "-J", "-t", stock_pane, "-S", "-",
                )

            literal = self.payload(coordinator, 0, "literal-name", ["not-a-key-name"], retain=False)
            reply = coordinator.request("guarded_action", **literal)
            self.assertEqual(reply["result"], "applied", reply)
            self.server.run("send-keys", "-t", stock_pane, "not-a-key-name")
            wait_for(
                lambda: "not-a-key-name" in self.capture() and
                "not-a-key-name" in stock_capture(),
                message="literal key name did not reach both panes",
            )
            self.assertEqual(self.capture(), stock_capture())
            key_names = [
                "C-Space", "C-/", "C-2", "C-Up", "C-F5", "C-#", "C-BSpace",
                "C-a",
            ]
            key_forms = self.payload(
                coordinator, 1, "stock-key-forms", key_names, retain=False,
            )
            self.assertEqual(coordinator.request("guarded_action", **key_forms)["result"], "applied")
            self.server.run("send-keys", "-t", stock_pane, *key_names)
            wait_for(
                lambda: all(key in self.capture() and key in stock_capture()
                            for key in ("C-#", "C-BSpace")),
                message="control-key fallback did not reach both panes",
            )
            wait_for(
                lambda: self.capture() == stock_capture(),
                message="ledger key delivery differs from stock send-keys",
            )
            self.assertEqual(self.capture(), stock_capture())

            self.server.run("set-option", "-g", "remain-on-exit", "on")
            dead_pane = self.server.text(
                "new-window", "-d", "-P", "-F", "#{pane_id}",
                "/bin/sh -c 'exit 0'",
            ).strip()
            wait_for(
                lambda: self.server.text(
                    "display-message", "-p", "-t", dead_pane, "#{pane_dead}",
                ).strip() == "1",
                message="remain-on-exit pane did not become dead",
            )
            before = self.capture()
            dead = self.payload(
                coordinator, 2, "real-pane-dead", ["must-not-type"], retain=False,
                state=self.state(dead_pane, assert_live=False),
            )
            self.assert_rejected(coordinator.request("guarded_action", **dead), "pane_dead")
            self.assert_no_effect_after_server_sentinel(before, "pane-dead")

            self.server.run("select-pane", "-t", "main", "-d")
            wait_for(
                lambda: self.server.text(
                    "display-message", "-p", "-t", "main", "#{pane_input_off}",
                ).strip() == "1",
                message="select-pane -d did not disable pane input",
            )
            before = self.capture()
            input_off = self.payload(
                coordinator, 3, "real-input-off", ["must-not-type"], retain=False,
                state=self.state(assert_live=False),
            )
            self.assert_rejected(coordinator.request("guarded_action", **input_off), "input_off")
            self.server.run("select-pane", "-t", "main", "-e")
            self.assert_no_effect_after_server_sentinel(before, "input-off")
        finally:
            coordinator.close()

    def test_sigusr1_rebinds_removed_default_directory_without_losing_peers(self):
        bridge = self.bridge
        peer = Peer(bridge)
        bridge.unlink()
        bridge.parent.rmdir()
        self.assertEqual(self.server.text("display-message", "-p", "#{masil_bridge_socket}").strip(), "")
        self.assertEqual(self.server.text("display-message", "-p", "#{masil_core_actions}").strip(), "1")
        assert self.server.server_pid is not None
        os.kill(self.server.server_pid, signal.SIGUSR1)
        wait_for(bridge.exists, message="SIGUSR1 did not restore bridge socket")
        try:
            self.assertEqual(self.server.text("display-message", "-p", "#{masil_bridge_socket}").strip(), str(bridge))
            self.assertEqual(self.server.text("display-message", "-p", "#{masil_core_actions}").strip(), "1")
            self.assertEqual(peer.request("stats")["kind"], "stats")
            restored = Peer(bridge)
            self.assertEqual(restored.hello["kind"], "hello")
        finally:
            peer.close()
            if "restored" in locals():
                restored.close()


if __name__ == "__main__":
    unittest.main()
