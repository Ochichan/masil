#!/usr/bin/env python3
"""Small bridge-off versus live-watch probe. Not a performance-budget gate."""
from __future__ import annotations

import argparse
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path
import socket
import sys
import threading

import benchmark

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from test_watch import Peer


class Consumer:
    def __init__(self, peer):
        self.peer = peer
        self.events = 0
        self.losses = []
        self.stopping = threading.Event()
        self.thread = threading.Thread(target=self.read, daemon=True)
        self.thread.start()

    def read(self):
        self.peer.socket.settimeout(None)
        try:
            while not self.stopping.is_set():
                message = self.peer.receive()
                if message.get("kind") != "event":
                    self.losses.append(message.get("kind"))
                    return
                self.events += 1
        except (EOFError, OSError) as error:
            if not self.stopping.is_set():
                self.losses.append(str(error))

    def close(self):
        self.stopping.set()
        try:
            self.peer.socket.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
        self.thread.join(timeout=3)
        self.peer.close()
        if self.thread.is_alive():
            raise RuntimeError("measurement reader did not stop")


def measure(panes, enabled, mode, seconds):
    instance = benchmark.TmuxInstance("masil", benchmark.ROOT / "bin/masil", 10, 93, 39, 94)
    instance.output_style = "inplace"
    bridge = instance.root / "observe.sock"
    if enabled:
        instance.env["MASIL_BRIDGE_SOCKET"] = str(bridge)
    consumer = None
    try:
        rate = max(1, 1048576 // panes) if mode == "output" else 0
        instance.start(panes, mode, max(4096, int(rate * seconds)) if rate else 0, rate)
        if enabled:
            peer = Peer(bridge)
            pane_ids = instance.tmux("list-panes", "-a", "-F", "#{pane_id}").stdout.splitlines()
            ack = peer.watch(pane_ids)
            if ack.get("kind") != "watch":
                peer.close()
                raise RuntimeError(ack)
            consumer = Consumer(peer)
        interval = benchmark.sample_interval(
            instance,
            seconds + .75 if mode == "output" else seconds,
            trigger=instance.release_output if mode == "output" else None,
            require_completion=mode == "output",
        )
        result = {"panes": panes, "watch": enabled, "mode": mode, "interval": interval,
                  "inner_geometry": benchmark.inner_geometry(instance), "events_received": consumer.events if consumer else 0,
                  "isolation": instance.isolation_record()}
        if mode == "output":
            completed = benchmark.wait_for_event(instance.sidechannels, "complete", 5)
            result["completed_bytes"] = sum(row["offered_bytes"] for row in completed)
        if enabled:
            stats_peer = Peer(bridge)
            try:
                result["stats"] = stats_peer.query("stats")
            finally:
                stats_peer.close()
            if consumer.losses:
                raise RuntimeError(f"observer lost stream: {consumer.losses}")
            if mode == "idle" and (consumer.events or result["stats"]["flush_timer_active"]):
                raise RuntimeError("idle watch emitted events or retained dirty timer")
        return result
    finally:
        if consumer:
            consumer.close()
        errors = instance.cleanup()
        if errors:
            raise RuntimeError(errors)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--seconds", type=float, default=5)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if args.seconds <= 0:
        parser.error("seconds must be positive")
    report = {"started_at_utc": datetime.now(timezone.utc).isoformat(), "budget_eligible": False,
              "binary": benchmark.product_metadata("masil", benchmark.ROOT / "bin/masil"),
              "script_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
              "seconds_per_condition": args.seconds, "conditions": [],
              "scope": "headless server RSS/CPU only; reader and Python workload children excluded",
              "output": "inplace ASCII at aggregate approximately 1 MiB/s; PTY write completion",
              "limits": "one sequential run, short idle, ps 0.01-second CPU resolution; no full budget claim"}
    for panes in (1, 50):
        for mode in ("idle", "output"):
            for enabled in (False, True):
                print(f"{panes} panes {mode} watch={enabled}", file=sys.stderr, flush=True)
                report["conditions"].append(measure(panes, enabled, mode, args.seconds))
    report["completed_at_utc"] = datetime.now(timezone.utc).isoformat()
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n")


if __name__ == "__main__":
    main()
