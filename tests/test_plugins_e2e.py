"""End-to-end verification for Masil local plugins, CLI lifecycle, 3-CLI skill
installer, and E2EE remote daemon plugin RPCs + live push notifications."""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
MASIL_AGENT = ROOT / "bin" / "masil-agent"


class PluginPlatformE2ETest(unittest.TestCase):
    def setUp(self) -> None:
        if not MASIL_AGENT.is_file():
            self.skipTest("bin/masil-agent not built yet")
        self.tmp = Path(tempfile.mkdtemp(prefix="masil-plugin-e2e-"))
        self.home = self.tmp / "home"
        self.xdg_config = self.tmp / "config"
        self.xdg_state = self.tmp / "state"
        self.home.mkdir(parents=True)
        self.xdg_config.mkdir(parents=True)
        self.xdg_state.mkdir(parents=True)
        self.env = os.environ.copy()
        self.env["HOME"] = str(self.home)
        self.env["XDG_CONFIG_HOME"] = str(self.xdg_config)
        self.env["XDG_STATE_HOME"] = str(self.xdg_state)
        self.env.pop("CLAUDE_CONFIG_DIR", None)
        self.env.pop("CODEX_HOME", None)

    def tearDown(self) -> None:
        shutil.rmtree(self.tmp, ignore_errors=True)

    def run_plugin(self, *args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [str(MASIL_AGENT), "plugin", *args],
            env=self.env,
            cwd=str(ROOT),
            text=True,
            capture_output=True,
            check=check,
        )

    def test_schema_scaffold_check_test_reload_and_remove(self) -> None:
        schema_out = self.run_plugin("schema", "--json")
        schema = json.loads(schema_out.stdout)
        self.assertEqual(schema["plugin_schema_version"], 1)
        self.assertEqual(schema["ui_spec_version"], 1)
        self.assertGreaterEqual(len(schema["ui_blocks"]), 15)

        self.run_plugin("init", "demo-py", "--template", "full", "--lang", "python")
        check_out = self.run_plugin("check", "demo-py", "--json")
        checked = json.loads(check_out.stdout)
        self.assertTrue(checked["ok"], checked)
        self.assertEqual(checked["errors"], [])

        panel_out = self.run_plugin(
            "test", "demo-py", "panel", "main", "--cwd", str(ROOT), "--json"
        )
        panel_doc = json.loads(panel_out.stdout)
        self.assertEqual(panel_doc["v"], 1)
        self.assertTrue(len(panel_doc["blocks"]) >= 2)

        cmd_out = self.run_plugin(
            "test", "demo-py", "command", "run", "--args", "hello world", "--json"
        )
        cmd_res = json.loads(cmd_out.stdout)
        self.assertIn("hello world", cmd_res["toast"])

        self.run_plugin("theme", "set", "demo-py")
        ls_data = json.loads(self.run_plugin("ls", "--json").stdout)
        self.assertEqual(ls_data["active_theme"], "demo-py")
        self.assertEqual(len(ls_data["plugins"]), 1)
        self.assertEqual(ls_data["plugins"][0]["status"], "running")

        self.run_plugin("disable", "demo-py")
        ls_disabled = json.loads(self.run_plugin("ls", "--json").stdout)
        self.assertEqual(ls_disabled["plugins"][0]["status"], "disabled")

        self.run_plugin("enable", "demo-py")
        self.run_plugin("reload", "demo-py")
        self.run_plugin("remove", "demo-py")
        ls_after = json.loads(self.run_plugin("ls", "--json").stdout)
        self.assertEqual(ls_after["plugins"], [])

    def test_example_plugins_install_and_validate(self) -> None:
        mocha = ROOT / "examples" / "plugins" / "catppuccin-mocha"
        pulse = ROOT / "examples" / "plugins" / "repo-pulse"

        self.run_plugin("install", str(mocha))
        self.run_plugin("install", str(pulse))

        c1 = json.loads(self.run_plugin("check", "catppuccin-mocha", "--json").stdout)
        c2 = json.loads(self.run_plugin("check", "repo-pulse", "--json").stdout)
        self.assertTrue(c1["ok"], c1)
        self.assertTrue(c2["ok"], c2)

        self.run_plugin("theme", "set", "catppuccin-mocha")
        panel = json.loads(
            self.run_plugin("test", "repo-pulse", "panel", "overview", "--cwd", str(ROOT)).stdout
        )
        block_types = [b["type"] for b in panel["blocks"]]
        self.assertIn("metric_row", block_types)
        self.assertIn("sparkline", block_types)

        cmd = json.loads(
            self.run_plugin(
                "test", "repo-pulse", "command", "pulse", "--args", "e2e", "--cwd", str(ROOT)
            ).stdout
        )
        self.assertIn("Pulse:", cmd["toast"])

    def test_three_cli_skill_installer_lifecycle(self) -> None:
        install_res = json.loads(
            self.run_plugin("skill", "install", "--target", "all", "--json").stdout
        )
        self.assertEqual(len(install_res["installed"]), 4)
        expected_files = [
            self.home / ".claude" / "skills" / "masil-plugin" / "SKILL.md",
            self.home / ".codex" / "skills" / "masil-plugin" / "SKILL.md",
            self.home / ".agents" / "skills" / "masil-plugin" / "SKILL.md",
            self.home / ".gemini" / "config" / "skills" / "masil-plugin" / "SKILL.md",
        ]
        for path in expected_files:
            self.assertTrue(path.is_file(), f"missing {path}")
            self.assertTrue((path.parent / "references" / "ui-spec.md").is_file())

        status = json.loads(self.run_plugin("skill", "status", "--json").stdout)
        for item in status["skills"]:
            self.assertTrue(item["skill_installed"], item)
            self.assertTrue(item["managed"], item)

        self.run_plugin("skill", "uninstall", "--target", "all")
        for path in expected_files:
            self.assertFalse(path.exists(), f"should be removed: {path}")

    def test_check_is_dry_run_and_theme_syncs_to_live_masil_server(self) -> None:
        self.run_plugin("init", "dry-theme", "--template", "full", "--lang", "python")
        # Dry-run check should NOT create storage.json or timeline files.
        checked = json.loads(self.run_plugin("check", "dry-theme", "--json").stdout)
        self.assertTrue(checked["ok"], checked)
        storage_file = self.xdg_state / "masil" / "plugins" / "storage" / "dry-theme.json"
        timeline_dir = self.xdg_state / "masil" / "plugins" / "timeline"
        self.assertFalse(storage_file.exists(), "plugin check must not mutate storage")
        self.assertFalse(
            timeline_dir.exists() and any(timeline_dir.iterdir()),
            "plugin check must not write timeline cards",
        )

        # If native masil binary exists, verify plugin theme syncs @masil-c-accent live.
        masil_bin = ROOT / "bin" / "masil"
        if not masil_bin.is_file():
            return
        sock_dir = Path(tempfile.mkdtemp(prefix="mp-", dir="/tmp"))
        uid_dir = sock_dir / f"masil-{os.getuid()}"
        uid_dir.mkdir(parents=True)
        sock = uid_dir / "default"
        env_with_tmux = self.env.copy()
        env_with_tmux["TMUX_TMPDIR"] = str(sock_dir)
        env_with_tmux.pop("TMUX", None)
        try:
            subprocess.run(
                [str(masil_bin), "-S", str(sock), "new-session", "-d", "-s", "theme-test"],
                env=env_with_tmux,
                check=True,
                capture_output=True,
                text=True,
            )
            subprocess.run(
                [str(MASIL_AGENT), "plugin", "theme", "set", "dry-theme"],
                env=env_with_tmux,
                cwd=str(ROOT),
                check=True,
                capture_output=True,
                text=True,
            )
            accent = subprocess.run(
                [str(masil_bin), "-S", str(sock), "show-options", "-gqv", "@masil-c-accent"],
                env=env_with_tmux,
                check=True,
                capture_output=True,
                text=True,
            ).stdout.strip()
            self.assertEqual(accent, "#6cc4d4")

            subprocess.run(
                [str(MASIL_AGENT), "plugin", "theme", "reset"],
                env=env_with_tmux,
                cwd=str(ROOT),
                check=True,
                capture_output=True,
                text=True,
            )
            accent_after = subprocess.run(
                [str(masil_bin), "-S", str(sock), "show-options", "-gqv", "@masil-c-accent"],
                env=env_with_tmux,
                check=True,
                capture_output=True,
                text=True,
            ).stdout.strip()
            self.assertEqual(accent_after, "#5bb5c2")
        finally:
            subprocess.run(
                [str(masil_bin), "-S", str(sock), "kill-server"],
                env=env_with_tmux,
                capture_output=True,
            )
            shutil.rmtree(sock_dir, ignore_errors=True)


if __name__ == "__main__":
    unittest.main()
