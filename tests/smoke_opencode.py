#!/usr/bin/env python3
"""Optional installed-OpenCode GET smoke with isolated config, storage and credentials."""
import argparse
import base64
import json
from pathlib import Path
import secrets
import shutil
import socket
import subprocess
import time
from urllib.request import Request, build_opener, ProxyHandler

from test_agentd import AGENT, manager_query
from test_compatibility import RMUX, Server, wait_for


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--opencode", default=shutil.which("opencode"))
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if not args.opencode:
        parser.error("installed opencode is required")
    core = Server(RMUX, command=["/bin/cat"])
    bridge = core.path / "observe.sock"
    core.env["RMUX_BRIDGE_SOCKET"] = str(bridge)
    with core:
        with socket.socket() as reservation:
            reservation.bind(("127.0.0.1", 0))
            port = reservation.getsockname()[1]
        endpoint = f"http://127.0.0.1:{port}"
        password = secrets.token_urlsafe(24)
        env = core.env | {
            "PATH": "/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin",
            "OPENCODE_TEST_HOME": str(core.path),
            "XDG_CONFIG_HOME": str(core.path / "config"),
            "XDG_DATA_HOME": str(core.path / "data"),
            "XDG_STATE_HOME": str(core.path / "state"),
            "XDG_CACHE_HOME": str(core.path / "cache"),
            "OPENCODE_CONFIG_CONTENT": '{"plugin":[],"autoupdate":false}',
            "OPENCODE_AUTH_CONTENT": "{}", "OPENCODE_PURE": "1",
            "OPENCODE_DISABLE_PROJECT_CONFIG": "1",
            "OPENCODE_DISABLE_DEFAULT_PLUGINS": "1",
            "OPENCODE_DISABLE_AUTOUPDATE": "1",
            "OPENCODE_DISABLE_AUTOCOMPACT": "1",
            "OPENCODE_DISABLE_MODELS_FETCH": "1",
            "OPENCODE_SERVER_PASSWORD": password,
        }
        version = subprocess.check_output([args.opencode, "--version"], env=env,
                                          cwd=core.path, text=True, timeout=15).strip()
        log = (core.path / "opencode.log").open("w+")
        provider = subprocess.Popen([args.opencode, "serve", "--hostname", "127.0.0.1", "--port", str(port)],
                                    env=env, cwd=core.path, stdout=log, stderr=log)
        daemon = None
        try:
            opener = build_opener(ProxyHandler({}))
            auth = base64.b64encode(f"opencode:{password}".encode()).decode()
            health = None
            until = time.monotonic() + 30
            while time.monotonic() < until:
                if provider.poll() is not None:
                    raise RuntimeError("isolated OpenCode exited before health")
                try:
                    request = Request(endpoint + "/global/health", headers={"Authorization": f"Basic {auth}"})
                    with opener.open(request, timeout=1) as response:
                        health = json.load(response)
                    break
                except OSError:
                    time.sleep(.1)
            if health is None:
                raise RuntimeError("isolated OpenCode health timed out")
            config = core.path / "sources.json"
            config.write_text(json.dumps({"sources": [{
                "id": "installed", "endpoint": endpoint, "directory": str(core.path.resolve()),
                "password_env": "RMUX_TEST_OPENCODE_PASSWORD",
                "sessions": [{"id": "missing", "pane_id": "%0", "session_id": "ses_missing"}]
            }]}))
            manager = core.path / "manager.sock"
            daemon = subprocess.Popen([str(AGENT), "--socket", str(manager), "serve",
                                       "--core", str(bridge), "--config", str(config)],
                                      env=core.env | {"RMUX_TEST_OPENCODE_PASSWORD": password},
                                      stdout=log, stderr=log)
            wait_for(manager.exists, 5)
            row = None
            until = time.monotonic() + 20
            while time.monotonic() < until:
                row = manager_query(manager, "inspect", id="missing")["observation"]
                if row["native"]["freshness"] == "fresh":
                    break
                time.sleep(.1)
            if row["native"]["freshness"] != "fresh" or row["native"]["exists"] is not False:
                raise AssertionError(row)
            result = {"installed_version": version, "health": health, "observation": row,
                      "scope": "GET-only empty server and verified missing native session; no model or TUI",
                      "isolated": ["HOME", "XDG config/data/state/cache", "project", "credentials", "plugins"]}
            args.output.write_text(json.dumps(result, indent=2) + "\n")
            print(json.dumps(result, indent=2))
            manager_query(manager, "stop")
            daemon.wait(timeout=5)
        finally:
            for child in (daemon, provider):
                if child is None:
                    continue
                if child.poll() is None:
                    child.terminate()
                try:
                    child.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait(timeout=5)
            log.close()


if __name__ == "__main__":
    main()
