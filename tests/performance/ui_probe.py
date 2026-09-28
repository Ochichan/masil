#!/usr/bin/env python3
"""Short real-PTY UI idle probe. Not a latency or performance acceptance gate."""
import argparse
import hashlib
import json
from pathlib import Path
import platform
import subprocess
import sys
import time

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from test_agentd import AGENT, AgentdHarness
from test_compatibility import RMUX, wait_for
from test_ui import Terminal
from benchmark import process_sample


def measure(count, seconds):
    harness = AgentdHarness()
    harness.setUp()
    ui = None
    try:
        config = json.loads(harness.config.read_text())
        sessions = []
        for index in range(count):
            sid = f"ses_{index}"
            harness.provider.sessions[sid] = {"id":sid,"directory":harness.provider.directory,"version":"1.18.32"}
            sessions.append({"id":f"agent-{index}","pane_id":harness.panes[index%2],"session_id":sid})
        config["sources"][0]["sessions"] = sessions
        harness.config.write_text(json.dumps(config))
        harness.start()
        wait_for(lambda: all(row["native"]["freshness"] == "fresh" for row in harness.query("agents")["observations"]), 10)
        ui = Terminal([AGENT,"--socket",harness.manager,"ui"], harness.core.env, 120,32)
        ui.until("agent-0")
        ui.drain(.3)
        requests = len(harness.provider.requests)
        before_ui = process_sample(ui.child.pid)
        before_daemon = process_sample(harness.child.pid)
        started = time.monotonic()
        output_bytes = ui.drain(seconds)
        elapsed = time.monotonic() - started
        after_ui = process_sample(ui.child.pid)
        after_daemon = process_sample(harness.child.pid)
        threads = subprocess.check_output(["ps","-M" if sys.platform == "darwin" else "-L","-p",str(ui.child.pid),"-o","pid="], text=True)
        return {"observations":count,"native_panes":2,"sources":1,"viewport":[120,32],
                "elapsed_seconds":elapsed,"ui_rss_bytes":after_ui["rss_kib"]*1024,
                "ui_threads":len(threads.strip().splitlines())-int(sys.platform=="darwin"),
                "ui_cpu_seconds":after_ui["cpu_time_seconds"]-before_ui["cpu_time_seconds"],
                "daemon_rss_bytes":after_daemon["rss_kib"]*1024,
                "daemon_cpu_seconds":after_daemon["cpu_time_seconds"]-before_daemon["cpu_time_seconds"],
                "idle_terminal_bytes":output_bytes,"idle_http_requests":len(harness.provider.requests)-requests,
                "sse_connections":harness.provider.stream_count}
    finally:
        if ui:
            ui.close()
        harness.doCleanups()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--seconds",type=float,default=5)
    parser.add_argument("--output",type=Path,required=True)
    args=parser.parse_args()
    if not 1 <= args.seconds <= 30:
        parser.error("seconds must be 1..30")
    result={"kind":"short_ui_idle_probe","platform":platform.platform(),
            "agent_sha256":hashlib.sha256(AGENT.read_bytes()).hexdigest(),
            "core_sha256":hashlib.sha256(RMUX.read_bytes()).hexdigest(),
            "provider":"deterministic GET/SSE fixture, no model or provider TUI",
            "cpu_method":"ps cumulative CPU delta with coarse resolution",
            "samples":[measure(count,args.seconds) for count in (1,50)]}
    args.output.write_text(json.dumps(result,indent=2)+"\n")
    print(json.dumps(result,indent=2))


if __name__ == "__main__":
    main()
