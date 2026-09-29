#!/usr/bin/env python3
"""Endpoint-qualified aggregation and original native terminal navigation."""
import json
import os
import shlex
import signal
import subprocess
import time
import unittest
import test_managed_agents as fixtures
from test_managed_agents import AGENT
from test_compatibility import Server, RMUX, wait_for, attached
from test_ui import Terminal


class Fleet(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        fixtures.ManagedAgents.setUpClass()

    @classmethod
    def tearDownClass(cls):
        fixtures.ManagedAgents.tearDownClass()

    def setUp(self):
        self.base = fixtures.ManagedAgents()
        self.base.setUp()
        self.addCleanup(self.base.doCleanups)
        self.base.start()
        self.remote = Server(RMUX)
        self.remote.__enter__()
        self.addCleanup(self.remote.__exit__)
        self.command('endpoints', 'add', 'lab', '--socket', str(self.remote.socket),
                     '--binary', str(AGENT), '--label', 'Lab server')
        self.command('--endpoint', 'lab', 'start', 'builder', 'codex', '--cwd', str(self.remote.path))
        wait_for(lambda: self.command('--endpoint', 'lab', 'get', 'builder')['state'] == 'idle', 5)

    def command(self, *args, check=True):
        result = subprocess.run([str(AGENT), 'agent', *args], env=self.base.env,
                                text=True, capture_output=True, timeout=12)
        if check:
            self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
            return json.loads(result.stdout)
        return result

    def test_duplicate_native_ids_route_prompt_and_receipts_to_exact_endpoint(self):
        snapshot = self.base.cli('list', '--all')
        self.assertFalse(snapshot['partial'])
        by_id = {a['id']: a for a in snapshot['agents']}
        self.assertEqual(set(by_id), {'local::builder', 'lab::builder'})
        self.assertEqual(by_id['local::builder']['pane_id'], by_id['lab::builder']['pane_id'])
        run = by_id['lab::builder']['run']
        sent = self.command('--endpoint', 'lab', 'prompt', 'builder', 'working', '--run', run, '--operation', '1')
        self.assertEqual(sent['stage'], 'delivered')
        self.assertEqual(self.command('--endpoint', 'lab', 'prompt', 'builder', 'working', '--run', run, '--operation', '1'), sent)
        self.assertEqual(self.base.cli('get', 'builder')['state'], 'idle')
        wait_for(lambda: self.command('--endpoint', 'lab', 'get', 'builder')['state'] == 'working', 4)
        self.command('endpoints', 'disable', 'lab')
        self.assertNotEqual(self.command('--endpoint', 'lab', 'send-keys', 'builder', 'idle', 'Enter', check=False).returncode, 0)
        self.assertTrue(self.base.cli('list', '--all')['partial'])

    def test_disconnected_endpoint_is_reported_without_starting_server(self):
        self.remote.run('kill-server')
        result = self.base.cli('list', '--all')
        self.assertTrue(result['partial'])
        self.assertTrue(next(s for s in result['endpoints'] if s['id'] == 'local')['connected'])
        self.assertFalse(next(s for s in result['endpoints'] if s['id'] == 'lab')['connected'])
        self.assertFalse(self.remote.run('list-sessions', check=False).returncode == 0)
        self.assertEqual(self.base.cli('get', 'builder')['state'], 'idle')

    def test_focus_attaches_original_remote_tui_and_detach_returns_cleanly(self):
        ui = Terminal([AGENT, 'agent', '--endpoint', 'lab', 'focus', 'builder'], self.base.env, 100, 30)
        self.addCleanup(ui.close)
        ui.until('STATE:idle')
        ui.send('blocked\r')
        ui.until('STATE:blocked')
        self.assertEqual(self.base.cli('get', 'builder')['state'], 'idle')
        ui.send('\x02d')
        ui.wait()
        self.assertEqual(ui.child.returncode, 0)
        self.assertEqual(self.command('--endpoint', 'lab', 'get', 'builder')['state'], 'blocked')

    def test_focus_refuses_to_change_an_existing_remote_clients_window(self):
        with attached(self.remote):
            before = self.remote.text('list-clients', '-F', '#{window_id}:#{pane_id}')
            ui = Terminal([AGENT, 'agent', '--endpoint', 'lab', 'focus', 'builder'], self.base.env, 100, 30)
            self.addCleanup(ui.close)
            ui.wait()
            self.assertNotEqual(ui.child.returncode, 0)
            self.assertEqual(self.remote.text('list-clients', '-F', '#{window_id}:#{pane_id}'), before)

    def test_connection_window_reuse_checks_remote_pane_and_local_generation(self):
        with attached(self.base.server):
            ui = Terminal([AGENT, 'agent', '--socket', self.base.server.socket, 'ui'], self.base.env, 100, 30)
            self.addCleanup(ui.close)
            ui.until('lab::builder')
            ui.send('/lab::builder\r')
            ui.send('g')

            def connections():
                lines = self.base.server.text('list-panes', '-a', '-F', '#{pane_id}\t#{@rmux-agent-connection}').splitlines()
                return [line.split('\t')[0] for line in lines if len(line.split('\t')) == 2 and line.split('\t')[1]]

            wait_for(lambda: len(connections()) == 1, 5)
            old = connections()[0]
            wait_for(lambda: 'STATE:idle' in self.base.server.text('capture-pane', '-p', '-t', old), 5)
            wait_for(lambda: 'rmux-agent-lease-' in self.remote.text('show-options', '-g'), 5)
            ui.send('g')
            ui.until('Selected pane')
            self.assertEqual(connections(), [old])

            self.remote.run('select-window', '-t', 'main:0')
            ui.send('g')
            ui.until('connection no longer shows')
            self.assertEqual(connections(), [old])
            self.assertIn('%0', self.remote.text('list-clients', '-F', '#{pane_id}'))

            self.base.server.run('respawn-pane', '-k', '-t', old, '/bin/sh')
            wait_for(lambda: not self.remote.text('list-clients').strip(), 5)
            ui.send('g')
            wait_for(lambda: len(connections()) == 2, 5)
            new = next(pane for pane in connections() if pane != old)
            wait_for(lambda: 'STATE:idle' in self.base.server.text('capture-pane', '-p', '-t', new), 5)
            wait_for(lambda: new in self.base.server.text('list-clients', '-F', '#{pane_id}').splitlines(), 5)
            self.assertNotIn(old, self.base.server.text('list-clients', '-F', '#{pane_id}').splitlines())
            ui.send('q')
            ui.wait()

    def test_sigterm_cancels_endpoint_process_group(self):
        wrapper = self.base.server.path / 'slow-endpoint'
        pids = self.base.server.path / 'slow-pids'
        wrapper.write_text('#!/bin/sh\nsleep 30 &\necho "$$\n$!" > ' + shlex.quote(str(pids)) + '\nwait\n')
        wrapper.chmod(0o700)
        self.command('endpoints', 'add', 'slow', '--socket', str(self.remote.socket), '--binary', str(wrapper))
        child = subprocess.Popen([str(AGENT), 'agent', '--endpoint', 'slow', 'list'], env=self.base.env,
                                 text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.addCleanup(lambda: child.kill() if child.poll() is None else None)
        wait_for(lambda: pids.exists() and len(pids.read_text().splitlines()) == 2, 4)
        child.send_signal(signal.SIGTERM)
        _, error = child.communicate(timeout=3)
        self.assertNotEqual(child.returncode, 0)
        self.assertIn('terminated', error)

        def gone():
            for pid in map(int, pids.read_text().splitlines()):
                try:
                    os.kill(pid, 0)
                except ProcessLookupError:
                    continue
                return False
            return True

        wait_for(gone, 2)

    def test_hung_local_server_does_not_block_remote_refresh(self):
        ui = Terminal([AGENT, 'agent', '--socket', self.base.server.socket, 'ui'], self.base.env, 100, 30)
        self.addCleanup(ui.close)
        ui.until('lab::builder')
        pid = int(self.base.server.text('display-message', '-p', '#{pid}'))
        os.kill(pid, signal.SIGSTOP)
        try:
            self.command('--endpoint', 'lab', 'send-keys', 'builder', 'blocked', 'Enter')
            start = time.monotonic()
            ui.until('Needs input', timeout=3)
            self.assertLess(time.monotonic() - start, 2.9)
        finally:
            os.kill(pid, signal.SIGCONT)
        ui.send('q')
        ui.wait()


if __name__ == '__main__':
    unittest.main(verbosity=2)
