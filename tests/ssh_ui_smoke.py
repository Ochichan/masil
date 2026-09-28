#!/usr/bin/env python3
"""Real SSH PTY smoke for an already-built remote checkout and trusted SSH host.

Run locally with the benchmark venv. This copies nothing, installs nothing and
uses only private fixture servers on the explicitly selected remote checkout.
"""
import argparse
import json
import os
from pathlib import Path
import re
import shlex
import signal
import subprocess
import sys
import tempfile
import termios


def fixture(mode):
    from test_agentd import AGENT, AgentdHarness
    from test_compatibility import wait_for

    def interrupted(_signum, _frame):
        raise KeyboardInterrupt

    for signum in (signal.SIGTERM, signal.SIGHUP):
        signal.signal(signum, interrupted)
    harness = AgentdHarness()
    child = None
    result = None
    try:
        harness.setUp()
        harness.provider.permissions = [{"id": "ssh_permission", "sessionID": "ses_0"}]
        harness.start()
        harness.fresh()
        original_modes = termios.tcgetattr(0)
        if mode == "desk":
            child = subprocess.Popen([AGENT, "--socket", harness.manager, "ui"], env=harness.core.env)
            sidebar = None
        else:
            child = subprocess.Popen([harness.core.binary, "-S", harness.core.socket,
                                      "attach-session", "-t", "main"], env=harness.core.env)
            wait_for(lambda: bool(harness.core.text("list-clients", "-F", "#{client_name}").strip()))
            opened = subprocess.run([AGENT, "--socket", harness.manager, "sidebar",
                                     "--core-native", harness.core.socket],
                                    env=harness.core.env, capture_output=True, text=True, timeout=15)
            if opened.returncode:
                raise RuntimeError(opened.stderr)
            sidebar = harness.core.text("list-panes", "-t", "main:0", "-f",
                                        "#{@rmux-sidebar-owned}", "-F", "#{pane_id}").strip()
            harness.core.run("select-pane", "-t", sidebar)
        child.wait(timeout=90)
        size = os.get_terminal_size(1)
        result = {
            "mode": mode, "exit": child.returncode,
            "acknowledged": harness.row()["attention"]["acknowledged"],
            "provider_readonly": all(method == "GET" for method, _, _ in harness.provider.requests),
            "terminal_restored": termios.tcgetattr(0) == original_modes,
            "final_size": [size.columns, size.lines],
            "active_pane": harness.core.text("display-message", "-p", "-t", "main:0", "#{pane_id}").strip(),
            "sidebar_closed": sidebar is None or sidebar not in harness.core.text("list-panes", "-a", "-F", "#{pane_id}").splitlines(),
        }
    finally:
        if child is not None and child.poll() is None:
            child.terminate()
            try:
                child.wait(timeout=4)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait(timeout=4)
        if not harness.doCleanups():
            raise RuntimeError("SSH fixture cleanup failed")
    print("RMUX_SSH_RESULT " + json.dumps(result), flush=True)


def verify(host, remote_root, output):
    from test_compatibility import isolated_env, wait_for
    from test_ui import Terminal

    if not remote_root.startswith("/"):
        raise ValueError("remote root must be an absolute path")
    output.mkdir(parents=True, exist_ok=True)
    reports = []
    with tempfile.TemporaryDirectory(prefix="rmux-ssh-client-") as local_root:
        for mode in ("desk", "native"):
            command = "cd " + shlex.quote(remote_root) + " && " + shlex.join([
                "env", "LC_ALL=C.UTF-8", "TERM=xterm-256color", "python3",
                "tests/ssh_ui_smoke.py", "--fixture", mode,
            ])
            ui = Terminal(["ssh", "-tt", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10",
                           "-o", "StrictHostKeyChecking=yes", "--", host, command],
                          isolated_env(Path(local_root)), width=120, height=32)
            try:
                ui.until("Mark seen", timeout=15)
                ui.click("Mark seen")
                ui.until("Marked agent-0 as seen")
                if mode == "desk":
                    ui.send("/")
                    ui.send("\x1b[200~한글 中文\x1b[201~")
                    ui.until("한글 中文")
                    ui.send("\x15")
                    ui.send("\x1b")
                    ui.send("l")
                    ui.until("에이전트")
                    ui.resize(24, 8)
                    ui.until("28 x 9")
                    ui.resize(80, 24)
                    ui.until("에이전트")
                    (output / "ssh-korean.txt").write_text(ui.text)
                    ui.send("q")
                else:
                    (output / "ssh-sidebar.txt").write_text(ui.text)
                    ui.send("z")
                    ui.until("Expanded agent sidebar")
                    (output / "ssh-expanded.txt").write_text(ui.text)
                    ui.click("Go to pane")
                    ui.until("Selected the configured pane")
                    ui.send("\x02o")  # Native tmux default prefix + next pane.
                    ui.send("q")
                    wait_for(lambda: "Mark seen" not in ui.text, 5)
                    ui.send("\x02d")  # Native detach, through the real SSH PTY.
                ui.wait(timeout=10)
                ui.drain()
                match = re.search(rb"RMUX_SSH_RESULT (\{[^\r\n]+\})", ui.output)
                if not match:
                    raise AssertionError(f"No fixture result, SSH exit={ui.child.returncode}: {ui.text}")
                report = json.loads(match.group(1))
                assert ui.child.returncode == 0 and report["exit"] == 0, report
                for field in ("acknowledged", "provider_readonly", "terminal_restored", "sidebar_closed"):
                    assert report[field], (field, report)
                assert report["active_pane"] == "%0", report
                assert termios.tcgetattr(ui.master) == ui.original, "local SSH terminal modes not restored"
                if mode == "desk":
                    assert report["final_size"] == [80, 24], report
                reports.append(report)
                print(json.dumps(report), flush=True)
            finally:
                (output / f"ssh-{mode}.ansi").write_bytes(ui.output)
                ui.close()
    (output / "ssh-results.json").write_text(json.dumps(reports, indent=2) + "\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fixture", choices=("desk", "native"))
    parser.add_argument("--host")
    parser.add_argument("--remote-root")
    parser.add_argument("--output", type=Path, default=Path(".build/ssh-smoke"))
    args = parser.parse_args()
    if args.fixture:
        fixture(args.fixture)
    elif args.host and args.remote_root:
        verify(args.host, args.remote_root, args.output)
    else:
        parser.error("provide --host and --remote-root")


if __name__ == "__main__":
    main()
