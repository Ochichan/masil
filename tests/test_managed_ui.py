#!/usr/bin/env python3
"""Native management UI behavior in an isolated PTY and masil server."""
import termios
import subprocess
from pathlib import Path

from test_compatibility import MASIL, Server, attached, wait_for
from test_managed_agents import AGENT, ManagedAgents
from test_ui import Terminal


class ManagedUi(ManagedAgents):
    def desk(self, *args, width=100, height=30):
        terminal = Terminal(
            [AGENT, 'agent', '--socket', self.server.socket, 'ui', *args],
            self.env,
            width,
            height,
        )
        self.addCleanup(terminal.close)
        terminal.until('에이전트 관리' if 'ko' in args else 'Agent management')
        return terminal

    def remote_cli(self, remote, *args, check=True):
        result = subprocess.run(
            [str(AGENT), 'agent', '--socket', remote.socket, *args],
            env=self.env,
            text=True,
            capture_output=True,
            timeout=10,
        )
        if check:
            self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
            import json
            return json.loads(result.stdout)
        return result

    def test_native_rows_generic_attention_context_menu_and_idle_render(self):
        self.start()
        ui = self.desk(width=80)
        ui.until('builder')
        self.cli('send-keys', 'builder', 'blocked', 'Enter')
        wait_for(lambda: self.cli('get', 'builder')['state'] == 'blocked', 4)
        ui.until('Needs input')
        self.assertNotIn('Question 1', ui.text)
        self.cli('send-keys', 'builder', 'working', 'Enter')
        wait_for(lambda: self.cli('get', 'builder')['state'] == 'working', 4)
        self.cli('send-keys', 'builder', 'idle', 'Enter')
        wait_for(lambda: self.cli('get', 'builder')['returned_idle'], 4)
        ui.until('Returned idle')

        x, y = ui.locate('builder')
        ui.mouse(x, y, code=2)
        ui.until('Resume session')
        self.assertIn('Prepare draft', ui.text)
        self.assertIn('Close pane', ui.text)
        ui.click('Prepare draft')
        ui.until('does not paste or submit')
        ui.click('Cancel')

        ui.drain(.3)
        self.assertEqual(ui.drain(1.2), 0, 'unchanged native inventory redrew the terminal')
        ui.send('q')
        ui.wait()
        ui.drain()
        self.assertEqual(ui.child.returncode, 0)
        self.assertEqual(termios.tcgetattr(ui.master), ui.original)

    def test_new_form_and_draft_are_native_and_buffer_only(self):
        self.start()
        ui = self.desk()
        ui.until('builder')
        ui.send('n')
        ui.until('New native agent')
        ui.send('reviewer')
        ui.send('\t\x15codex')
        ui.send('\t\x15' + str(self.server.path))
        ui.send('\r')
        ui.until('provider acceptance is unverified')
        wait_for(lambda: self.cli('get', 'reviewer')['process'] == 'running', 4)

        # Search keeps selection deterministic while the one-second inventory changes.
        ui.send('/builder\r')
        ui.send('d')
        ui.until('Prepare draft')
        ui.send('\x1b[200~hello 한국어\x1b[201~\r')
        ui.until('it was not pasted or submitted')
        self.assertEqual(self.server.text('show-buffer', '-b', 'masil-agent-draft'), 'hello 한국어')
        ui.send('q')
        ui.wait()
        self.assertEqual(ui.child.returncode, 0)

    def test_korean_dialog_copy(self):
        self.start()
        ui = self.desk()
        ui.until('builder')
        ui.send('l')
        ui.until('에이전트 관리')
        ui.send('n')
        ui.until('새 네이티브 에이전트')
        self.assertIn('작업 디렉터리', ui.text)
        self.assertIn('취소', ui.text)
        ui.send('\x1b')
        ui.send('q')
        ui.wait()
        self.assertEqual(ui.child.returncode, 0)

    def test_saved_name_order_is_preserved_in_ui(self):
        self.start('zulu-blocked')
        self.cli('send-keys', 'zulu-blocked', 'blocked', 'Enter')
        wait_for(lambda: self.cli('get', 'zulu-blocked')['state'] == 'blocked', 4)
        self.start('alpha-idle')
        self.cli('view', 'set', '--sort', 'name')
        self.assertEqual(
            [agent['name'] for agent in self.cli('list')['agents']],
            ['alpha-idle', 'zulu-blocked'],
        )
        ui = self.desk(width=80)
        ui.until('alpha-idle')
        ui.until('zulu-blocked')
        self.assertLess(ui.locate('alpha-idle')[1], ui.locate('zulu-blocked')[1])
        ui.send('q')
        ui.wait()

    def test_restart_and_respawn_cancel_agent_dialog_identity(self):
        first = self.start()
        ui = self.desk()
        ui.until('builder')
        ui.send('d')
        ui.until('Draft text')

        self.server.run('kill-server', check=False)
        self.server.run('new-session', '-d', '-s', 'main', '-x', '120', '-y', '40', '/bin/sh')
        second = self.start()
        self.assertNotEqual(first['run'], second['run'])
        wait_for(lambda: 'Draft text' not in ui.text and 'builder' in ui.text, 6)

        ui.send('d')
        ui.until('Draft text')
        self.server.run('respawn-pane', '-k', '-t', second['pane_id'], '/bin/sh')
        wait_for(lambda: self.cli('list')['agents'] == [], 4)
        wait_for(lambda: 'Draft text' not in ui.text, 4)
        ui.send('q')
        ui.wait()

    def test_send_prompt_is_idle_only_and_never_claims_acceptance(self):
        self.start()
        ui = self.desk()
        ui.until('builder')
        ui.send('p')
        ui.until('Prompt text')
        ui.send('working\r')
        ui.until('provider acceptance is unverified')
        wait_for(lambda: self.cli('get', 'builder')['state'] == 'working', 4)
        self.cli('send-keys', 'builder', 'blocked', 'Enter')
        wait_for(lambda: self.cli('get', 'builder')['state'] == 'blocked', 4)
        ui.until('Needs input')

        x, y = ui.locate('builder')
        ui.mouse(x, y, code=2)
        ui.until('Send prompt')
        ui.click('Send prompt')
        self.assertNotIn('Prompt text', ui.text)
        current = self.cli('get', 'builder')
        self.assertEqual(current['state'], 'blocked')
        self.assertFalse(current['seen'])
        ui.send('q')
        ui.wait()

    def test_fleet_routes_by_endpoint_and_retains_offline_rows(self):
        remote = Server(MASIL)
        remote.__enter__()
        self.addCleanup(remote.__exit__)
        local = self.start('builder')
        remote_started = self.remote_cli(
            remote,
            'start', 'builder', 'codex', '--cwd', str(remote.path),
        )
        wait_for(lambda: self.remote_cli(remote, 'get', 'builder')['process'] == 'running', 4)
        self.assertEqual(local['pane_id'], remote_started['pane_id'])
        self.cli(
            'endpoints', 'add', 'remote', '--socket', str(remote.socket),
            '--binary', str(AGENT), '--label', 'Remote',
        )

        ui = self.desk(width=120)
        ui.until('local::builder')
        ui.until('remote::builder', 8)
        ui.until('Remote [remote] online')
        ui.send('/remote::builder\r')
        ui.send('n')
        ui.until('Server')
        self.assertIn('remote', ui.text)
        self.assertIn('Working directory', ui.text)
        ui.send('\x1b')

        ui.send('p')
        ui.until('Prompt text')
        ui.send('working\r')
        wait_for(lambda: self.remote_cli(remote, 'get', 'builder')['state'] == 'working', 4)
        self.assertEqual(self.cli('get', 'builder')['state'], 'idle')

        old_run = self.remote_cli(remote, 'get', 'builder')['run']
        remote.run('kill-server', check=False)
        ui.until('Remote [remote] offline', 8)
        self.assertIn('remote::builder', ui.text)
        ui.send('p')
        self.assertNotIn('Prompt text', ui.text)

        ui.send('/\x15local::builder\r')
        ui.send('p')
        ui.until('Prompt text')
        ui.send('working\r')
        wait_for(lambda: self.cli('get', 'builder')['state'] == 'working', 4)

        remote.run('new-session', '-d', '-s', 'main', '-x', '120', '-y', '40', '/bin/sh')
        restored = self.remote_cli(
            remote,
            'start', 'builder', 'codex', '--cwd', str(remote.path),
        )
        self.assertNotEqual(old_run, restored['run'])
        ui.until('Remote [remote] online', 8)
        ui.send('/\x15remote::builder\r')
        ui.send('p')
        ui.until('Prompt text')
        ui.send('blocked\r')
        wait_for(lambda: self.remote_cli(remote, 'get', 'builder')['state'] == 'blocked', 4)
        ui.send('q')
        ui.wait()

    def test_new_dialog_rejects_replaced_endpoint_configuration(self):
        original = Server(MASIL)
        replacement = Server(MASIL)
        original.__enter__()
        replacement.__enter__()
        self.addCleanup(original.__exit__)
        self.addCleanup(replacement.__exit__)
        self.remote_cli(
            original,
            'start', 'builder', 'codex', '--cwd', str(original.path),
        )

        ui = self.desk(width=120)
        self.cli(
            'endpoints', 'add', 'remote', '--socket', str(original.socket),
            '--binary', str(AGENT), '--label', 'Original',
        )
        ui.until('remote::builder', 8)
        ui.send('/remote::builder\r')
        ui.send('n')
        ui.until('Server')
        self.assertIn('remote', ui.text)
        self.assertNotIn(str(Path.cwd()), ui.text)

        self.cli('endpoints', 'remove', 'remote')
        self.cli(
            'endpoints', 'add', 'remote', '--socket', str(replacement.socket),
            '--binary', str(AGENT), '--label', 'Replacement',
        )
        ui.send('\tbound-agent\t\t' + str(replacement.path) + '\r')
        ui.until('endpoint connection changed since the start dialog opened', 5)
        self.assertNotEqual(
            self.remote_cli(replacement, 'get', 'bound-agent', check=False).returncode,
            0,
        )
        self.assertNotEqual(
            self.remote_cli(original, 'get', 'bound-agent', check=False).returncode,
            0,
        )
        ui.send('q')
        ui.wait()

    def test_sidebar_reuses_owned_pane_zoom_and_focus_restores_layout(self):
        started = self.start()
        with attached(self.server):
            client = self.server.text('list-clients', '-F', '#{client_name}').strip()
            command = [AGENT, 'agent', '--socket', self.server.socket,
                       '--client', client, 'sidebar']
            first = subprocess.run(command, env=self.env, text=True, capture_output=True, timeout=10)
            self.assertEqual(first.returncode, 0, first.stderr)
            self.assertIn('Created agent management sidebar', first.stdout)
            panes = self.server.text('list-panes', '-t', 'main:0', '-F',
                                     '#{pane_id}\t#{@masil-managed-sidebar}').splitlines()
            owned = [line.split('\t')[0] for line in panes if line.split('\t')[1]]
            self.assertEqual(len(owned), 1)

            second = subprocess.run(command, env=self.env, text=True, capture_output=True, timeout=10)
            self.assertEqual(second.returncode, 0, second.stderr)
            self.assertIn('already open', second.stdout)
            self.assertEqual(len(self.server.text('list-panes', '-t', 'main:0').splitlines()), 2)

            time_limit = 4
            wait_for(lambda: 'builder' in self.server.text('capture-pane', '-p', '-t', owned[0]), time_limit)
            self.server.run('send-keys', '-t', owned[0], 'z')
            wait_for(lambda: self.server.text('display-message', '-p', '-t', owned[0],
                                              '#{window_zoomed_flag}').strip() == '1', time_limit)
            self.server.run('send-keys', '-t', owned[0], 'g')
            wait_for(lambda: self.server.text('display-message', '-p', '-c', client,
                                              '#{pane_id}').strip() == started['pane_id'], time_limit)
            self.assertEqual(self.server.text('display-message', '-p', '-t', owned[0],
                                              '#{window_zoomed_flag}').strip(), '0')


if __name__ == '__main__':
    import unittest
    unittest.main(verbosity=2)
