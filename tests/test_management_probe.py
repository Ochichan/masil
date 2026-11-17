#!/usr/bin/env python3
"""Missing processes cannot be recorded as zero-cost resident features."""
from pathlib import Path
import sys
import subprocess
import unittest
from unittest.mock import MagicMock, patch

sys.path.insert(0, str(Path(__file__).resolve().parent / 'performance'))
import management


class ManagementProbeTests(unittest.TestCase):
    def test_missing_coordinator_fails_measurement(self):
        with patch.object(management, 'process_sample', side_effect=[{'pid': 1}, None]):
            with self.assertRaisesRegex(management.BenchmarkError, 'coordinator'):
                management.sample_roles({'core': 1, 'coordinator': 2})

    def test_desk_close_failure_still_cleans_the_private_server(self):
        fixture = MagicMock()
        fixture.server.path = Path('/private-fixture')
        fixture.cli.side_effect = [
            {}, {'state': 'running', 'coordinator': {'pid': 2}}, {'agents': [{}]},
        ]
        fixture.doCleanups.return_value = True
        ui = MagicMock()
        ui.buffer = b''
        ui.wait_marker.side_effect = management.BenchmarkError('render failed')
        ui.process.poll.return_value = 0
        ui.close.side_effect = subprocess.TimeoutExpired('desk', 2)
        with patch.object(management, 'ManagedAgents', return_value=fixture), \
                patch.object(management, 'AttachedClient', return_value=ui):
            with self.assertRaisesRegex(management.BenchmarkError, 'render failed'):
                management.measure(1, 1, 1)
        fixture.doCleanups.assert_called_once()

    def test_graceful_quit_failure_still_attempts_forced_close(self):
        fixture = MagicMock()
        fixture.server.path = Path('/private-fixture')
        fixture.cli.side_effect = [
            {}, {'state': 'running', 'coordinator': {'pid': 2}}, {'agents': [{}]},
        ]
        fixture.doCleanups.return_value = True
        ui = MagicMock()
        ui.buffer = b''
        ui.wait_marker.side_effect = management.BenchmarkError('render failed')
        ui.process.poll.return_value = None
        ui.send.side_effect = OSError('closed PTY')
        with patch.object(management, 'ManagedAgents', return_value=fixture), \
                patch.object(management, 'AttachedClient', return_value=ui):
            with self.assertRaisesRegex(management.BenchmarkError, 'render failed'):
                management.measure(1, 1, 1)
        ui.close.assert_called_once()
        fixture.doCleanups.assert_called_once()


if __name__ == '__main__':
    unittest.main()
