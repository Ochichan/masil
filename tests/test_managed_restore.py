#!/usr/bin/env python3
"""Crash-safe native agent snapshot and restore behavior."""

import fcntl
import json
import os
from pathlib import Path
import stat
import subprocess
import unittest

from test_compatibility import MASIL, Server, wait_for
from test_managed_agents import AGENT, ManagedAgents as _ManagedAgents


class ManagedRestore(_ManagedAgents):
    # This file uses the shared fixture without rerunning its own test cases.
    test_discovery_launch_rename_explain_read_and_draft = None
    test_delivery_wait_and_shared_seen_do_not_approve = None
    test_stale_run_reports_and_respawn_are_rejected = None
    test_wait_stops_on_removal_and_arguments_are_literal = None
    test_failed_launch_and_duplicate_name_do_not_add_panes = None
    test_new_foreground_job_in_same_pane_is_a_new_agent_run = None
    test_return_to_idle_is_attention_without_a_success_claim = None
    test_python_versioned_foreground_wrapper_is_discovered = None
    test_relative_socket_launch_uses_original_server_after_cwd_change = None
    test_osc_progress_overrides_idle_screen_and_resets = None
    test_prompt_delivers_once_and_retries_use_receipts = None
    test_prompt_rejects_control_keys_multiline_without_bracketing_and_input_off = None
    test_pending_prompt_never_resubmits = None

    def report_session(self, name, session):
        agent = self.cli('get', name)
        self.cli('report', '--pane', agent['pane_id'], '--run', agent['run'],
                 '--sequence', '1', '--state', 'idle', '--session', session)
        return self.cli('get', name)

    def close_and_wait(self, name):
        self.cli('close', name)
        wait_for(lambda: all(agent['name'] != name for agent in self.cli('list')['agents']), 4)

    def snapshot(self, name='agents.json'):
        return self.server.path / name

    def cli_on(self, server, *args, check=True):
        result = subprocess.run(
            [str(AGENT), 'agent', '--socket', str(server.socket), *args],
            env=self.env, text=True, capture_output=True, timeout=10,
        )
        if check:
            self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
            self.assertEqual(result.stderr, '')
            return json.loads(result.stdout)
        return result

    @staticmethod
    def receipt(snapshot):
        return snapshot.with_name(f'.{snapshot.name}.restore.json')

    def test_snapshot_is_private_versioned_and_preserves_native_references(self):
        self.start('builder', '--', '--literal', 'arg with spaces')
        observed = self.report_session('builder', 'native-session-1')
        snapshot = self.snapshot()
        result = self.cli('save', str(snapshot))
        self.assertEqual(result['stage'], 'snapshot_saved')
        self.assertEqual(stat.S_IMODE(snapshot.stat().st_mode), 0o600)

        document = json.loads(snapshot.read_text())
        self.assertEqual(document['version'], 1)
        self.assertEqual(len(document['agents']), 1)
        entry = document['agents'][0]
        self.assertEqual(entry['native_session_ref'], 'native-session-1')
        self.assertEqual(entry['original_args'], ['--literal', 'arg with spaces'])
        self.assertEqual(entry['native'], {
            'core_boot_id': observed['boot'],
            'pty_generation': observed['generation'],
            'pane_id': observed['pane_id'],
            'run': observed['run'],
        })
        serialized = snapshot.read_text()
        self.assertNotIn('STATE:idle', serialized)
        self.assertNotIn('evidence', serialized)
        self.assertNotIn('MASIL_AGENT_', serialized)

        invalid = self.snapshot('invalid.json')
        document['version'] = 99
        invalid.write_text(json.dumps(document))
        invalid.chmod(0o600)
        rejected = self.cli('restore', str(invalid), check=False)
        self.assertNotEqual(rejected.returncode, 0)
        self.assertIn('unsupported agent snapshot version', rejected.stderr)

        victim = self.snapshot('victim')
        victim.write_text('do not replace')
        victim.chmod(0o600)
        link = self.snapshot('link.json')
        link.symlink_to(victim)
        rejected = self.cli('save', str(link), check=False)
        self.assertNotEqual(rejected.returncode, 0)
        self.assertEqual(victim.read_text(), 'do not replace')

    def test_restore_and_repeat_do_not_duplicate_an_agent(self):
        self.start('builder')
        self.report_session('builder', 'native-session-2')
        snapshot = self.snapshot()
        self.cli('save', str(snapshot))
        live_panes = self.server.text('list-panes', '-a', '-F', '#{pane_id}')
        live = self.cli('restore', str(snapshot))
        self.assertEqual(live['entries'][0]['stage'], 'already_running')
        self.assertEqual(self.server.text('list-panes', '-a', '-F', '#{pane_id}'), live_panes)
        self.close_and_wait('builder')
        before = self.server.text('list-panes', '-a', '-F', '#{pane_id}').splitlines()

        first = self.cli('restore', str(snapshot))
        self.assertEqual(first['stage'], 'restore_finished')
        self.assertTrue(first['all_started'])
        self.assertFalse(first['partial'])
        self.assertEqual(first['entries'][0]['stage'], 'process_started')
        self.assertFalse(first['entries'][0]['native_session_verified'])
        after_first = self.server.text('list-panes', '-a', '-F', '#{pane_id}').splitlines()
        self.assertEqual(len(after_first), len(before) + 1)

        second = self.cli('restore', str(snapshot))
        self.assertEqual(second['entries'][0]['stage'], 'already_restored')
        self.assertEqual(
            self.server.text('list-panes', '-a', '-F', '#{pane_id}').splitlines(),
            after_first,
        )

    def test_partial_failure_retries_only_the_proven_unlaunched_entry(self):
        missing = self.server.path / 'disappearing-cwd'
        missing.mkdir()
        self.start('good')
        bad = self.cli('start', 'bad', 'codex', '--cwd', str(missing))
        wait_for(lambda: self.cli('get', 'bad')['process'] == 'running', 4)
        self.report_session('good', 'native-session-good')
        self.report_session('bad', 'native-session-bad')
        snapshot = self.snapshot()
        self.cli('save', str(snapshot))
        self.close_and_wait('good')
        self.close_and_wait('bad')
        missing.rmdir()

        first = self.cli('restore', str(snapshot))
        self.assertFalse(first['all_started'])
        self.assertTrue(first['partial'])
        self.assertEqual(first['counts']['failed_not_started'], 1)
        self.assertEqual(first['counts']['launched'], 1)
        stages = {entry['name']: entry['stage'] for entry in first['entries']}
        self.assertEqual(stages, {'good': 'process_started', 'bad': 'failed_not_started'})
        self.assertEqual(len([agent for agent in self.cli('list')['agents']
                              if agent['name'] in ('good', 'bad')]), 1)

        missing.mkdir()
        second = self.cli('restore', str(snapshot))
        stages = {entry['name']: entry['stage'] for entry in second['entries']}
        self.assertEqual(stages, {'good': 'already_restored', 'bad': 'process_started'})
        self.assertEqual(len([agent for agent in self.cli('list')['agents']
                              if agent['name'] in ('good', 'bad')]), 2)
        self.assertEqual(bad['provider'], 'codex')

    def test_unresolved_pending_is_reported_without_resubmission(self):
        self.start('builder')
        self.report_session('builder', 'native-session-pending')
        snapshot = self.snapshot()
        saved = self.cli('save', str(snapshot))
        self.close_and_wait('builder')
        receipt = self.receipt(snapshot)
        receipt.write_text(json.dumps({
            'version': 1,
            'snapshot_id': saved['snapshot_id'],
            'entries': [{
                'index': 0,
                'state': 'pending',
                'pane_id': None,
                'run': None,
                'error': None,
            }],
        }))
        receipt.chmod(0o600)
        before = self.server.text('list-panes', '-a', '-F', '#{pane_id}')

        result = self.cli('restore', str(snapshot))
        self.assertEqual(result['counts']['unknown'], 1)
        self.assertTrue(result['partial'])
        self.assertEqual(result['entries'][0]['stage'], 'unknown')
        self.assertFalse(result['entries'][0]['can_retry'])
        self.assertEqual(self.server.text('list-panes', '-a', '-F', '#{pane_id}'), before)

    def test_fresh_start_requires_explicit_permission(self):
        self.start('builder')
        snapshot = self.snapshot()
        self.cli('save', str(snapshot))
        self.close_and_wait('builder')

        denied = self.cli('restore', str(snapshot))
        self.assertEqual(denied['entries'][0]['stage'], 'fresh_start_not_allowed')
        self.assertEqual(self.cli('list')['agents'], [])

        allowed = self.cli('restore', str(snapshot), '--allow-fresh')
        self.assertEqual(allowed['entries'][0]['stage'], 'process_started')
        self.assertIsNone(allowed['entries'][0]['native_session_requested'])

    def test_parallel_restore_lock_fails_without_launching(self):
        self.start('builder')
        self.report_session('builder', 'native-session-lock')
        snapshot = self.snapshot()
        self.cli('save', str(snapshot))
        self.close_and_wait('builder')
        lock_path = self.server.socket.parent / f'.{self.server.socket.name}.restore.lock'
        lock = open(lock_path, 'a+')
        self.addCleanup(lock.close)
        os.chmod(lock_path, 0o600)
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)

        rejected = self.cli('restore', str(snapshot), check=False)
        self.assertNotEqual(rejected.returncode, 0)
        self.assertIn('another restore operation is in progress', rejected.stderr)
        self.assertEqual(self.cli('list')['agents'], [])

    def test_snapshot_lock_excludes_save_and_restore_across_servers(self):
        self.start('builder')
        self.report_session('builder', 'native-session-shared-lock')
        snapshot = self.snapshot()
        self.cli('save', str(snapshot))
        snapshot_before = snapshot.read_bytes()
        self.close_and_wait('builder')

        second = Server(MASIL)
        second.__enter__()
        self.addCleanup(second.__exit__)
        alias_directory = self.server.path / 'alias'
        alias_directory.mkdir(mode=0o700)
        alias = alias_directory / '..' / snapshot.name

        lock_path = snapshot.with_name(f'.{snapshot.name}.snapshot.lock')
        lock = open(lock_path, 'a+')
        self.addCleanup(lock.close)
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        for server, path in ((self.server, snapshot), (second, alias)):
            rejected = self.cli_on(server, 'restore', str(path), check=False)
            self.assertNotEqual(rejected.returncode, 0)
            self.assertIn('another snapshot operation is in progress', rejected.stderr)
        rejected = self.cli('save', str(snapshot), check=False)
        self.assertNotEqual(rejected.returncode, 0)
        self.assertIn('another snapshot operation is in progress', rejected.stderr)
        self.assertEqual(snapshot.read_bytes(), snapshot_before)
        self.assertFalse(self.receipt(snapshot).exists())
        self.assertEqual(self.cli_on(second, 'list')['agents'], [])

        fcntl.flock(lock, fcntl.LOCK_UN)
        first = self.cli('restore', str(snapshot))
        self.assertEqual(first['entries'][0]['stage'], 'process_started')
        repeated = self.cli_on(second, 'restore', str(alias))
        self.assertEqual(repeated['entries'][0]['stage'], 'already_restored')
        self.assertEqual(self.cli_on(second, 'list')['agents'], [])


if __name__ == '__main__':
    suite = unittest.defaultTestLoader.loadTestsFromTestCase(ManagedRestore)
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    raise SystemExit(not result.wasSuccessful())
