#!/usr/bin/env python3
"""Native server and pane identities without the observation bridge."""

import os
from pathlib import Path
import shutil
import stat
import subprocess
import tempfile
import unittest
import uuid


ROOT = Path(__file__).resolve().parents[1]
MASIL = ROOT / "bin" / "masil"


class NativeIdentity(unittest.TestCase):
    def setUp(self):
        build = ROOT / ".build"
        build.mkdir(exist_ok=True)
        self.path = Path(tempfile.mkdtemp(prefix="native-identity-", dir=build))
        self.socket = self.path / "server.sock"
        self.env = os.environ.copy()
        self.env.pop("MASIL_BRIDGE_SOCKET", None)
        self.addCleanup(shutil.rmtree, self.path, ignore_errors=True)
        self.addCleanup(lambda: self.masil("kill-server", check=False))

    def masil(self, *args, check=True):
        return subprocess.run(
            [str(MASIL), "-S", str(self.socket), "-f", "/dev/null", *args],
            cwd=ROOT,
            env=self.env,
            check=check,
            capture_output=True,
            text=True,
            timeout=10,
        )

    def start(self):
        self.masil("new-session", "-d", "-s", "identity")

    def identity(self):
        output = self.masil(
            "display-message", "-p",
            "#{masil_core_boot_id} #{masil_pty_generation}",
        ).stdout.strip()
        boot_id, generation = output.split()
        parsed = uuid.UUID(boot_id)
        self.assertEqual(parsed.version, 4)
        self.assertEqual(str(parsed), boot_id)
        return boot_id, int(generation)

    def test_identity_without_observation_bridge(self):
        self.start()
        boot_id, generation = self.identity()
        self.assertGreater(generation, 0)
        self.assertEqual(self.identity(), (boot_id, generation))

        sockets = {
            path for path in self.path.iterdir()
            if stat.S_ISSOCK(path.stat().st_mode)
        }
        self.assertEqual(sockets, {self.socket})

        self.masil("respawn-pane", "-k", "-t", "identity:0")
        respawn_boot_id, respawn_generation = self.identity()
        self.assertEqual(respawn_boot_id, boot_id)
        self.assertGreater(respawn_generation, generation)

    def test_server_restart_gets_a_new_boot_id(self):
        self.start()
        first_boot_id, _ = self.identity()
        self.masil("kill-server")

        self.start()
        second_boot_id, _ = self.identity()
        self.assertNotEqual(second_boot_id, first_boot_id)


if __name__ == "__main__":
    unittest.main(verbosity=2)
