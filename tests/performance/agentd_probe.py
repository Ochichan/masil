#!/usr/bin/env python3
"""Short observer-only idle probe; not a performance acceptance gate."""
import argparse
import hashlib
import json
from pathlib import Path
import platform
import subprocess
import sys
import time

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from test_agentd import AGENT, Provider, manager_query
from test_attention import WatchPeer
from test_compatibility import RMUX, Server, wait_for
from benchmark import process_sample


def measure(count, seconds, subscribers):
    with Server(RMUX, command=["/bin/cat"]) as core:
        # Start a distinct private core with observation enabled from creation.
        core.run("kill-server")
        bridge = core.path / "observe.sock"
        core.env["RMUX_BRIDGE_SOCKET"] = str(bridge)
        core.run("new-session", "-d", "-s", "main", "/bin/cat")
        panes = ["%0"] + [core.text("new-window", "-d", "-E", "-P", "-F", "#{pane_id}").strip()
                           for _ in range(count - 1)]
        provider = Provider(core.path, count)
        manager = core.path / "manager.sock"
        config = core.path / "sources.json"
        config.write_text(json.dumps({"sources": [{
            "id": "local", "endpoint": provider.endpoint, "directory": provider.directory,
            "sessions": [{"id": f"agent-{i}", "pane_id": pane, "session_id": f"ses_{i}"}
                         for i, pane in enumerate(panes)]
        }]}))
        child = subprocess.Popen([str(AGENT), "--socket", str(manager), "serve", "--core", str(bridge),
                                  "--config", str(config)], env=core.env,
                                 stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
        peers = []
        try:
            wait_for(lambda: manager.exists() or child.poll() is not None, 5)
            if child.poll() is not None:
                raise RuntimeError(child.stderr.read())
            wait_for(lambda: all(row["native"]["freshness"] == "fresh" for row in
                                 manager_query(manager, "agents")["observations"]), 15)
            for _ in range(subscribers):
                peer = WatchPeer(manager)
                peers.append(peer)
                if peer.receive()["kind"] != "agents_snapshot":
                    raise RuntimeError("projection subscription failed")
            # Let initial dirty-only notification coalescing settle.
            time.sleep(.1)
            before = process_sample(child.pid)
            requests = len(provider.requests)
            started = time.monotonic()
            time.sleep(seconds)
            elapsed = time.monotonic() - started
            after = process_sample(child.pid)
            if before is None or after is None:
                raise RuntimeError("agentd exited during sampling")
            threads = subprocess.check_output(
                ["ps", "-M" if sys.platform == "darwin" else "-L", "-p", str(child.pid), "-o", "pid="], text=True)
            result = {
                "observations": count, "sources": 1, "elapsed_seconds": elapsed,
                "projection_subscribers": subscribers,
                "rss_bytes": after["rss_kib"] * 1024,
                "threads": len(threads.strip().splitlines()) - int(sys.platform == "darwin"),
                "cpu_seconds": after["cpu_time_seconds"] - before["cpu_time_seconds"],
                "http_requests_while_idle": len(provider.requests) - requests,
                "sse_connections": provider.stream_count,
                "initial_http_requests": requests,
                "readable_projection_subscribers": sum(peer.ready(0) for peer in peers),
                "core_pid": int(core.text("display-message", "-p", "#{pid}").strip()),
            }
            result["cpu_percent_one_core"] = result["cpu_seconds"] / elapsed * 100
            result["core_rss_bytes"] = process_sample(result["core_pid"])["rss_kib"] * 1024
            result.pop("core_pid")
            manager_query(manager, "stop")
            child.wait(timeout=5)
            return result
        finally:
            for peer in peers:
                peer.close()
            if child.poll() is None:
                child.terminate()
            child.communicate(timeout=5)
            provider.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--seconds", type=float, default=5)
    parser.add_argument("--subscribers", type=int, default=0)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if not 1 <= args.seconds <= 30:
        parser.error("seconds must be 1..30")
    if not 0 <= args.subscribers <= 16:
        parser.error("subscribers must be 0..16")
    result = {
        "kind": "short_idle_probe", "platform": platform.platform(),
        "agent_sha256": hashlib.sha256(AGENT.read_bytes()).hexdigest(),
        "core_sha256": hashlib.sha256(RMUX.read_bytes()).hexdigest(),
        "provider": "deterministic local HTTP/SSE fixture; no model or TUI",
        "cpu_method": "ps cumulative CPU delta; short interval and coarse timer resolution",
        "samples": [measure(count, args.seconds, args.subscribers) for count in (1, 50)],
    }
    args.output.write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
