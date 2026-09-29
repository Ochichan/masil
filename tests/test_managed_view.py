#!/usr/bin/env python3
"""Saved native agent list filters and ordering."""

import json
import unittest

from test_compatibility import wait_for
from test_managed_agents import ManagedAgents as _ManagedAgents


class ManagedView(_ManagedAgents):
    test_discovery_launch_rename_explain_read_and_draft = None
    test_delivery_wait_and_shared_seen_do_not_approve = None
    test_stale_run_reports_and_respawn_are_rejected = None
    test_wait_stops_on_removal_and_arguments_are_literal = None
    test_failed_launch_and_duplicate_name_do_not_add_panes = None
    test_new_foreground_job_in_same_pane_is_a_new_agent_run = None
    test_return_to_idle_is_attention_without_a_success_claim = None

    def set_state(self, name, state):
        self.cli('send-keys', name, state, 'Enter')
        wait_for(lambda: self.cli('get', name)['state'] == state, 4)

    def test_view_persists_filters_without_hiding_control_targets(self):
        self.start('alpha')
        self.start('beta')
        self.set_state('beta', 'blocked')
        workspace = self.cli('get', 'alpha')['workspace']

        saved = self.cli(
            'view', 'set',
            '--provider', 'codex', '--provider', 'codex',
            '--state', 'blocked', '--state', 'blocked',
            '--workspace', workspace, '--sort', 'name',
        )
        self.assertEqual(saved['stage'], 'view_saved')
        self.assertEqual(saved['view']['providers'], ['codex'])
        self.assertEqual(saved['view']['states'], ['blocked'])
        self.assertEqual(saved['view']['workspaces'], [workspace])
        self.assertEqual(saved['view']['sort'], 'name')

        encoded = self.server.text('show-options', '-gqv', '@masil-agent-view').strip()
        self.assertTrue(encoded)
        self.assertEqual(json.loads(bytes.fromhex(encoded)), saved['view'])
        self.assertEqual(self.cli('view', 'get')['view'], saved['view'])
        self.assertEqual([agent['name'] for agent in self.cli('list')['agents']], ['beta'])

        # View filters affect presentation only. Direct targeting, uniqueness,
        # and snapshots still use the complete unfiltered inventory.
        self.assertEqual(self.cli('get', 'alpha')['name'], 'alpha')
        duplicate = self.cli('start', 'alpha', 'codex', '--cwd', str(self.server.path), check=False)
        self.assertNotEqual(duplicate.returncode, 0)
        snapshot = self.server.path / 'all-agents.json'
        self.cli('save', str(snapshot))
        self.assertEqual(len(json.loads(snapshot.read_text())['agents']), 2)

        cleared = self.cli('view', 'clear')
        self.assertEqual(cleared['stage'], 'view_cleared')
        self.assertEqual(self.server.text('show-options', '-gqv', '@masil-agent-view'), '')
        self.assertEqual(
            {agent['name'] for agent in self.cli('list')['agents']},
            {'alpha', 'beta'},
        )

    def test_priority_sort_is_deterministic(self):
        self.start('alpha')
        self.start('beta')
        self.start('gamma')
        self.set_state('beta', 'working')
        self.set_state('gamma', 'blocked')
        self.cli('view', 'set', '--sort', 'priority')
        self.assertEqual(
            [agent['name'] for agent in self.cli('list')['agents']],
            ['gamma', 'beta', 'alpha'],
        )

        self.cli('view', 'set', '--sort', 'name')
        self.assertEqual(
            [agent['name'] for agent in self.cli('list')['agents']],
            ['alpha', 'beta', 'gamma'],
        )

    def test_invalid_views_fail_closed_and_workspace_is_literal(self):
        self.start('alpha')
        invalid = [
            ('view', 'set', '--provider', 'missing-provider'),
            ('view', 'set', '--state', 'running'),
            ('view', 'set', '--workspace', 'x' * 129),
            ('view', 'set', '--workspace', 'line\nbreak'),
            ('view', 'set', '--sort', 'name', '--sort', 'name'),
        ]
        for args in invalid:
            with self.subTest(args=args):
                self.assertNotEqual(self.cli(*args, check=False).returncode, 0)

        marker = self.server.path / 'must-not-exist'
        literal = f'$(touch {marker})'
        self.cli('view', 'set', '--workspace', literal)
        self.assertEqual(self.cli('list')['agents'], [])
        self.assertFalse(marker.exists())

        malformed = {
            'version': 1,
            'providers': [],
            'states': [],
            'workspaces': [],
            'sort': 'priority',
            'unexpected': True,
        }
        self.server.run('set-option', '-g', '@masil-agent-view',
                        json.dumps(malformed, separators=(',', ':')).encode().hex())
        rejected = self.cli('list', check=False)
        self.assertNotEqual(rejected.returncode, 0)
        self.assertIn('invalid saved agent view', rejected.stderr)
        self.cli('view', 'clear')
        self.assertEqual(self.cli('list')['agents'][0]['name'], 'alpha')


if __name__ == '__main__':
    suite = unittest.defaultTestLoader.loadTestsFromTestCase(ManagedView)
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    raise SystemExit(not result.wasSuccessful())
