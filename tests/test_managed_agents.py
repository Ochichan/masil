#!/usr/bin/env python3
"""Native agent workflows using an isolated server and a deterministic provider."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time
import unittest

from test_compatibility import MASIL, ROOT, Server, wait_for
from test_core_ledger import Peer
from test_core_prompt import (
    commit_staged_core_input,
    core_bridge,
    core_input_state,
    core_operation_key,
    core_ticket,
)

AGENT = ROOT / 'bin/masil-agent'


class ManagedAgents(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.fixture = Path(tempfile.mkdtemp(prefix='masil-provider-'))
        subprocess.run(['cc', '-Wall', '-Wextra', '-Werror', str(ROOT / 'tests/faults/agent.c'),
                        '-o', str(cls.fixture / 'codex')], check=True, capture_output=True)

    @classmethod
    def tearDownClass(cls):
        import shutil
        shutil.rmtree(cls.fixture)

    def setUp(self):
        self.server = Server(MASIL)
        # Core actions start a resident coordinator in the server's
        # environment. Give it the same detection manifests as the CLI so a
        # background pass cannot overwrite TRACKED with another judgment.
        self.server.env['XDG_CONFIG_HOME'] = str(self.server.path / 'config')
        self.server.__enter__()
        self.addCleanup(self.server.__exit__)
        self.env = self.server.env | {'PATH': f'{self.fixture}:/usr/bin:/bin',
                                     'XDG_CONFIG_HOME': str(self.server.path / 'config')}
        directory = Path(self.env['XDG_CONFIG_HOME']) / 'masil/agent-detection'
        directory.mkdir(parents=True)
        (directory / 'codex.toml').write_text('''id = "codex"
version = "1.0.0"
min_engine_version = 4
interrupt_keys = ["Escape"]
[[rules]]
id = "synthetic_progress"
state = "working"
priority = 200
region = "osc_progress"
contains = ["4;3"]
[[rules]]
id = "synthetic_blocker"
state = "blocked"
priority = 100
region = "whole_recent"
visible_blocker = true
contains = ["STATE:blocked"]
[[rules]]
id = "synthetic_work"
state = "working"
interruptible = true
priority = 90
region = "whole_recent"
contains = ["STATE:working"]
[[rules]]
id = "synthetic_idle"
state = "idle"
priority = 80
region = "whole_recent"
visible_idle = true
contains = ["STATE:idle"]
''')

    def cli(self, *args, check=True, timeout=10):
        result = subprocess.run([str(AGENT), 'agent', '--socket', str(self.server.socket), *args],
                                env=self.env, text=True, capture_output=True, timeout=timeout)
        if check:
            self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
            self.assertEqual(result.stderr, '')
            return json.loads(result.stdout)
        return result

    def start(self, name='builder', *args):
        result = self.cli('start', name, 'codex', '--cwd', str(self.server.path), *args)
        wait_for(lambda: self.cli('get', name)['process'] == 'running', 4)
        wait_for(lambda: self.cli('get', name)['state'] == 'idle', 4)
        return result

    def uses_core_ledger(self):
        return self.server.text('display-message', '-p', '#{masil_core_actions}').strip() == '1'

    def test_discovery_launch_rename_explain_read_and_draft(self):
        self.assertEqual(len(self.cli('providers')['providers']), 24)
        self.assertEqual(self.cli('list')['agents'], [])
        receipt = self.start()
        self.assertEqual(receipt['stage'], 'process_started')
        self.assertFalse(receipt['provider_accepted'])
        observed = self.cli('get', 'builder')
        self.assertEqual(observed['state'], 'idle')
        self.assertEqual(observed['provider'], 'codex')
        self.assertEqual(self.cli('explain', 'builder')['evidence']['matched_rule']['id'], 'synthetic_idle')
        self.assertIn('STATE:idle', self.cli('read', 'builder')['text'])
        self.cli('rename', 'builder', 'renamed')
        self.assertEqual(self.cli('get', 'renamed')['pane_id'], receipt['pane_id'])
        result = self.cli('draft', 'renamed', 'hello\n한국어')
        self.assertFalse(result['submitted'])
        self.assertEqual(self.server.text('show-buffer', '-b', 'masil-agent-draft'), 'hello\n한국어')
        self.assertEqual(self.cli('get', 'renamed')['state'], 'idle')

    def test_delivery_wait_and_shared_seen_do_not_approve(self):
        self.start()
        result = self.cli('send-keys', 'builder', 'blocked', 'Enter')
        self.assertEqual(result['stage'], 'keys_delivered')
        result = self.cli('wait', 'builder', '--state', 'blocked', '--timeout', '3')
        self.assertEqual(result['outcome'], 'state_observed')
        self.assertIsNone(result['task_success'])
        agent = self.cli('get', 'builder')
        self.assertFalse(agent['seen'])
        self.cli('ack', 'builder', '--run', agent['run'], '--revision', agent['revision'])
        self.assertTrue(self.cli('get', 'builder')['seen'])
        self.assertEqual(self.cli('get', 'builder')['state'], 'blocked')
        self.cli('send-keys', 'builder', 'idle', 'Enter')
        self.assertEqual(self.cli('get', 'builder')['state'], 'idle')
        self.cli('send-keys', 'builder', 'blocked', 'Enter')
        current = self.cli('get', 'builder')
        self.assertFalse(current['seen'])
        self.assertNotEqual(current['revision'], agent['revision'])
        stale = self.cli('ack', 'builder', '--run', agent['run'], '--revision', agent['revision'], check=False)
        self.assertNotEqual(stale.returncode, 0)
        timeout = self.cli('wait', 'builder', '--state', 'working', '--timeout', '0.1', check=False)
        self.assertEqual(timeout.returncode, 6, timeout.stderr)

    def test_stale_run_reports_and_respawn_are_rejected(self):
        receipt = self.start()
        pane = receipt['pane_id']
        self.cli('report', '--pane', pane, '--run', receipt['run'], '--sequence', '1',
                 '--state', 'working', '--session', 'session-example')
        self.assertEqual(self.cli('get', pane)['session_id'], 'session-example')
        stale = self.cli('report', '--pane', pane, '--run', receipt['run'], '--sequence', '1',
                         '--state', 'idle', check=False)
        self.assertNotEqual(stale.returncode, 0)
        self.server.run('respawn-pane', '-k', '-t', pane, '/bin/sh')
        self.assertEqual(self.cli('list')['agents'], [])
        self.assertNotEqual(self.cli('report', '--pane', pane, '--run', receipt['run'],
                                    '--sequence', '2', '--state', 'idle', check=False).returncode, 0)

    def test_wait_stops_on_removal_and_arguments_are_literal(self):
        self.start('builder', '--', 'arg with spaces', '$(touch should-not-exist)')
        child = subprocess.Popen([str(AGENT), 'agent', '--socket', str(self.server.socket),
                                  'wait', 'builder', '--state', 'working', '--timeout', '10'],
                                 env=self.env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        self.addCleanup(lambda: child.poll() is None and child.kill())
        time.sleep(.4)
        self.cli('close', 'builder')
        out, err = child.communicate(timeout=4)
        self.assertEqual(child.returncode, 5, err)
        self.assertEqual(json.loads(out)['outcome'], 'target_removed')
        self.assertEqual(json.loads(out)['end_reason'], 'pane_closed')
        self.assertFalse((self.server.path / 'should-not-exist').exists())

    def waiting(self, *args):
        child = subprocess.Popen([str(AGENT), 'agent', '--socket', str(self.server.socket), 'wait', *args],
                                 env=self.env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        self.addCleanup(lambda: child.poll() is None and child.kill())
        time.sleep(.4)
        return child

    def ended(self, child):
        out, err = child.communicate(timeout=6)
        return child.returncode, json.loads(out) if out else err

    def test_a_wait_for_a_state_tells_a_left_agent_from_a_closed_pane(self):
        self.server.run('send-keys', '-t', '%0', str(self.fixture / 'codex'), 'Enter')
        wait_for(lambda: len(self.cli('list')['agents']) == 1)
        child = self.waiting('%0', '--state', 'working', '--timeout', '10')
        self.server.run('send-keys', '-t', '%0', 'exit', 'Enter')
        code, result = self.ended(child)
        self.assertEqual((code, result['outcome'], result['end_reason']),
                         (5, 'target_removed', 'agent_left_foreground'))

    def test_a_wait_ends_for_a_run_an_operation_a_window_or_a_session(self):
        started = self.start()
        run = started['run']
        timed_out = self.cli('wait', '--run', run, '--ended', '--timeout', '0.3', check=False)
        self.assertEqual((timed_out.returncode, json.loads(timed_out.stdout)['end_reason']), (6, 'timeout'))
        child = self.waiting('--run', run, '--ended', '--timeout', '10')
        closed = self.cli('close', 'builder', '--run', run, '--operation', 'c1')
        code, result = self.ended(child)
        self.assertEqual((code, result['end_reason'], result['seen']), (0, 'run_ended', True))
        key = closed['operation_key']
        reached = self.cli('wait', '--operation', key, '--stage', 'pane_closed')
        self.assertEqual((reached['end_reason'], reached['stage']), ('stage_reached', 'pane_closed'))
        wrong = self.cli('wait', '--operation', key, '--stage', 'native_accepted', check=False)
        self.assertEqual(wrong.returncode, 2, wrong.stderr)
        self.assertIn('pane_closed', wrong.stderr)
        # Gone before the wait began: an ID was closed; a name is unknown.
        self.assertFalse(self.cli('wait', '--run', run, '--ended')['seen'])
        self.assertEqual(self.cli('wait', '--pane', '%99', '--closed')['end_reason'], 'pane_closed')
        self.assertEqual(self.cli('wait', '--session', 'nowhere', '--closed', check=False).returncode, 5)

        window = self.server.text('new-window', '-d', '-n', 'scratch', '-P', '-F', '#{window_id}').strip()
        session = self.server.text('display-message', '-p', '-t', window, '#{session_name}').strip()
        # Exact names only: a prefix or a missing window is not this one.
        for wrong in [f'{session}:scr', f'{session}:nosuch']:
            self.assertEqual(self.cli('wait', '--window', wrong, '--closed', check=False).returncode, 5)
        self.assertEqual(self.cli('wait', '--window', f'{session}:scratch', '--closed', '--timeout', '0.3',
                                  check=False).returncode, 6)
        self.assertEqual(self.cli('wait', '--window', '@' + '0' + window[1:], '--closed', '--timeout', '0.3',
                                  check=False).returncode, 6)
        child = self.waiting('--window', window, '--closed', '--timeout', '10')
        self.server.run('kill-window', '-t', window)
        code, result = self.ended(child)
        self.assertEqual((code, result['end_reason'], result['window'], result['seen']),
                         (0, 'window_closed', window, True))
        self.server.run('new-session', '-d', '-s', 'later')
        child = self.waiting('--session', 'later', '--closed', '--timeout', '10')
        # A rename is not a close: the name was turned into its ID.
        self.server.run('rename-session', '-t', 'later', 'renamed')
        time.sleep(.4)
        self.assertIsNone(child.poll())
        self.server.run('kill-session', '-t', 'renamed')
        code, result = self.ended(child)
        self.assertEqual((code, result['end_reason']), (0, 'session_closed'))

    def test_a_run_masil_did_not_start_ends_when_its_agent_does(self):
        self.server.run('send-keys', '-t', '%0', str(self.fixture / 'codex'), 'Enter')
        wait_for(lambda: len(self.cli('list')['agents']) == 1)
        run = self.cli('get', '%0')['run']
        # Stopped with Ctrl-Z it leaves the list but is still the run.
        self.server.run('send-keys', '-t', '%0', 'C-z')
        wait_for(lambda: self.cli('list')['agents'] == [])
        stopped = self.cli('wait', '--run', run, '--ended', '--timeout', '0.5', check=False)
        self.assertEqual((stopped.returncode, json.loads(stopped.stdout)['end_reason']), (6, 'timeout'))
        self.server.run('send-keys', '-t', '%0', 'fg', 'Enter')
        wait_for(lambda: [agent['run'] for agent in self.cli('list')['agents']] == [run])
        child = self.waiting('--run', run, '--ended', '--timeout', '10')
        self.server.run('send-keys', '-t', '%0', 'exit', 'Enter')
        code, result = self.ended(child)
        self.assertEqual((code, result['end_reason'], result['seen']), (0, 'run_ended', True))

    def test_a_wait_tells_a_gone_server(self):
        child = self.waiting('--pane', '%0', '--closed', '--timeout', '10')
        self.server.run('kill-server')
        code, result = self.ended(child)
        self.assertEqual((code, result['end_reason']), (6, 'server_gone'))

    def test_failed_launch_and_duplicate_name_do_not_add_panes(self):
        self.start()
        before = self.server.text('list-panes', '-a', '-F', '#{pane_id}')
        for args in [('start', 'builder', 'codex'), ('start', 'unsafe;name', 'codex'),
                     ('start', 'missing', 'codex', '--cwd', '/not/a/real/directory')]:
            self.assertNotEqual(self.cli(*args, check=False).returncode, 0)
        self.assertEqual(self.server.text('list-panes', '-a', '-F', '#{pane_id}'), before)

    def test_new_foreground_job_in_same_pane_is_a_new_agent_run(self):
        executable = str(self.fixture / 'codex')
        self.server.run('send-keys', '-t', '%0', executable, 'Enter')
        wait_for(lambda: len(self.cli('list')['agents']) == 1)
        self.cli('attach', '%0', 'manual')
        first = self.cli('get', 'manual')
        self.cli('send-keys', 'manual', 'exit', 'Enter')
        wait_for(lambda: self.cli('list')['agents'] == [])
        self.server.run('send-keys', '-t', '%0', executable, 'Enter')
        wait_for(lambda: len(self.cli('list')['agents']) == 1)
        second = self.cli('get', '%0')
        self.assertNotEqual(second['run'], first['run'])
        self.assertNotEqual(second['name'], 'manual')
        self.assertIsNone(second['session_id'])

    def test_an_agent_started_outside_masil_keeps_its_run_until_its_group_id_is_reused(self):
        executable = str(self.fixture / 'codex')
        self.server.run('send-keys', '-t', '%0', executable, 'Enter')
        wait_for(lambda: len(self.cli('list')['agents']) == 1)
        agent = self.cli('get', '%0')
        group, started = agent['process_epoch'].split(':')
        self.assertNotEqual(started, '0')
        # The name earlier binaries give it, so the two agree.
        self.assertTrue(agent['run'].endswith(f'-{group}'), agent['run'])

        def record():
            return self.server.text('show-options', '-pqv', '-t', '%0', '@masil-agent-epoch').strip()
        self.assertEqual(record(), f"{agent['run']} {agent['process_epoch']}")
        item = self.cli('queue', '%0', 'add', 'for this process')
        # As if the recorded process had ended and another took its group ID.
        self.server.run('set-option', '-p', '-t', '%0', '@masil-agent-epoch', f"{agent['run']} {group}:1")
        reused = self.cli('get', '%0')
        self.assertEqual(reused['run'], f"{agent['run']}-{started}")
        self.assertEqual(record(), f"{reused['run']} {agent['process_epoch']}")
        self.assertEqual(self.cli('get', '%0')['run'], reused['run'])
        queue = self.cli('queue', '%0')
        self.assertEqual((queue['items'], [entry['id'] for entry in queue['held']]), ([], [item['id']]))

    def test_persistent_target_wait_with_64_panes_observes_output_change(self):
        # A one-word command runs as `$SHELL -c WORD`. Debian's dash does
        # not exec it, so the pane's command would be `sh` and no agent
        # would be found; `exec` makes the fixture the pane's process.
        executable = 'exec ' + str(self.fixture / 'codex')
        self.server.run('respawn-pane', '-k', '-t', '%0', executable)
        for _ in range(63):
            self.server.run('new-window', '-d', executable)
        wait_for(lambda: len(self.cli('list')['agents']) == 64, 8)

        child = subprocess.Popen(
            [str(AGENT), 'agent', '--socket', str(self.server.socket),
             'wait', '%0', '--state', 'working', '--timeout', '5'],
            env=self.env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        self.addCleanup(lambda: child.poll() is None and child.kill())
        time.sleep(.5)
        self.server.run('send-keys', '-t', '%0', 'working', 'Enter')
        out, err = child.communicate(timeout=8)
        self.assertEqual(child.returncode, 0, err)
        self.assertEqual(json.loads(out)['outcome'], 'state_observed')

    def test_return_to_idle_is_attention_without_a_success_claim(self):
        self.start()
        self.assertFalse(self.cli('get', 'builder')['returned_idle'])
        self.cli('send-keys', 'builder', 'working', 'Enter')
        self.assertEqual(self.cli('get', 'builder')['state'], 'working')
        self.cli('send-keys', 'builder', 'idle', 'Enter')
        idle = self.cli('get', 'builder')
        self.assertTrue(idle['returned_idle'])
        self.assertFalse(idle['seen'])
        self.cli('ack', 'builder', '--run', idle['run'], '--revision', idle['revision'])
        self.assertTrue(self.cli('get', 'builder')['seen'])
        self.cli('send-keys', 'builder', 'working', 'Enter')
        self.assertFalse(self.cli('get', 'builder')['returned_idle'])

    def test_python_versioned_foreground_wrapper_is_discovered(self):
        import shutil
        runtime = self.server.path / 'python3.11'
        shutil.copy2(self.fixture / 'codex', runtime)
        self.server.run('new-window', '-d', str(runtime), '/venv/bin/hermes')
        wait_for(lambda: len(self.cli('list')['agents']) == 1)
        agent = self.cli('list')['agents'][0]
        self.assertEqual(agent['provider'], 'hermes')
        self.assertEqual(agent['process'], 'running')

    def test_relative_socket_launch_uses_original_server_after_cwd_change(self):
        work = self.server.path / 'other-directory'
        work.mkdir()
        result = subprocess.run([str(AGENT), 'agent', '--socket', self.server.socket.name,
                                 'start', 'relative', 'codex', '--cwd', str(work)],
                                cwd=self.server.path, env=self.env, text=True,
                                capture_output=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        wait_for(lambda: self.cli('get', 'relative')['process'] == 'running')
        self.assertEqual(self.cli('get', 'relative')['cwd'], str(work.resolve()))

    def test_management_works_without_a_utf8_locale(self):
        launch = self.start()
        self.server.run('select-pane', '-t', launch['pane_id'], '-T', '작업\t중')
        env = {key: value for key, value in self.env.items()
               if key not in ('LANG', 'LC_ALL', 'LC_CTYPE')}
        for args in (('list',), ('get', 'builder'), ('find', 'builder')):
            result = subprocess.run([str(AGENT), 'agent', '--socket', str(self.server.socket), *args],
                                    env=env, text=True, capture_output=True, timeout=10)
            self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)['results'][0]['label'], 'builder')
        doctor = subprocess.run([str(AGENT), 'doctor', '--socket', str(self.server.socket)],
                                env=env, text=True, capture_output=True, timeout=10)
        checks = {check['id']: check['status'] for check in json.loads(doctor.stdout)['checks']}
        self.assertEqual(checks['server.reachable'], 'ok')

    def test_start_keeps_a_directory_with_tmux_format_characters_literal(self):
        directory = self.server.path / '#{session_name}' / '#(touch pwned)'
        directory.mkdir(parents=True)
        launch = self.cli('start', 'literal', 'codex', '--cwd', str(directory))
        wait_for(lambda: self.cli('get', 'literal')['process'] == 'running', 4)
        path = self.server.text('display-message', '-p', '-t', launch['pane_id'],
                                '#{pane_current_path}').strip()
        self.assertEqual(path, os.path.realpath(directory))
        self.assertFalse((self.server.path / 'pwned').exists())

    def test_osc_progress_overrides_idle_screen_and_resets(self):
        self.start()
        self.cli('send-keys', 'builder', 'progress', 'Enter')
        wait_for(lambda: self.cli('get', 'builder')['state'] == 'working')
        self.assertEqual(self.cli('explain', 'builder')['evidence']['matched_rule']['id'], 'synthetic_progress')
        self.cli('send-keys', 'builder', 'progress-reset', 'Enter')
        wait_for(lambda: self.cli('get', 'builder')['state'] == 'idle')

    def test_prompt_delivers_once_and_retries_use_receipts(self):
        launch = self.start()
        sent = self.cli('prompt', 'builder', 'working', '--operation', '1', '--run', launch['run'])
        self.assertEqual(sent['stage'], 'delivered')
        self.assertFalse(sent['provider_accepted'])
        wait_for(lambda: self.cli('get', 'builder')['state'] == 'working')
        self.assertEqual(self.cli('prompt', 'builder', 'working', '--operation', '1', '--run', launch['run']), sent)
        self.assertIn('INPUTS:1', self.cli('read', 'builder')['text'])
        self.assertNotEqual(self.cli('prompt', 'builder', 'different', '--operation', '1', '--run', launch['run'], check=False).returncode, 0)
        self.assertNotEqual(self.cli('prompt', 'builder', 'new work', check=False).returncode, 0)
        self.assertEqual(self.cli('prompt-receipt', 'builder')['stage'], 'delivered')
        self.cli('send-keys', 'builder', 'blocked', 'Enter')
        self.assertNotEqual(self.cli('prompt', 'builder', 'yes', check=False).returncode, 0)
        self.assertEqual(self.cli('get', 'builder')['state'], 'blocked')

    def test_prompt_rejects_control_keys_multiline_without_bracketing_and_input_off(self):
        launch = self.start()
        for text in ('hello\nworld', 'hello\tworld', 'hello\x1b[201~', '\x03'):
            self.assertNotEqual(self.cli('prompt', 'builder', text, check=False).returncode, 0)
        self.server.run('select-pane', '-t', launch['pane_id'], '-d')
        self.assertNotEqual(self.cli('prompt', 'builder', 'working', check=False).returncode, 0)
        self.assertEqual(self.cli('get', 'builder')['state'], 'idle')

    def test_pending_prompt_never_resubmits(self):
        import hashlib
        launch = self.start()
        if self.uses_core_ledger():
            # A core-path pending prompt lives only in SQLite. Make the same
            # precondition from a completed receipt, then leave it unresolved.
            self.cli('prompt', 'builder', 'working', '--operation', '1', '--run', launch['run'])
            self.cli('send-keys', 'builder', 'idle', 'Enter')
            wait_for(lambda: self.cli('get', 'builder')['state'] == 'idle')
            import sqlite3
            database, = (self.server.path / '.local/state/masil/operations').glob('*.sqlite3')
            with sqlite3.connect(database) as store:
                store.execute("UPDATE operations SET state = 'dispatching', lease_until_ms = ? WHERE key = ?",
                              (2 ** 62, f"run:{launch['run']}/prompt/1"))
        else:
            ledger = {'run': launch['run'], 'sequence': 1, 'entries': [
                {'operation': 1, 'digest': hashlib.sha256(b'working').hexdigest(), 'stage': 'pending'}]}
            self.server.run('set-option', '-p', '-t', launch['pane_id'], '@masil-agent-prompt-receipts',
                            json.dumps(ledger).encode().hex())
        pending = self.cli('prompt', 'builder', 'working', '--operation', '1', '--run', launch['run'], check=False)
        self.assertEqual(pending.returncode, 6, pending.stderr)
        self.assertEqual(json.loads(pending.stdout)['stage'], 'pending')
        self.assertNotEqual(self.cli('prompt', 'builder', 'working', check=False).returncode, 0)
        self.assertEqual(self.cli('get', 'builder')['state'], 'idle')

    def test_prompt_retry_is_bound_to_original_run(self):
        launch = self.start()
        self.cli('prompt', 'builder', 'working', '--run', launch['run'], '--operation', '1')
        self.cli('close', 'builder')
        replacement = self.start()
        self.assertNotEqual(launch['run'], replacement['run'])
        retry = self.cli('prompt', 'builder', 'working', '--run', launch['run'], '--operation', '1', check=False)
        self.assertNotEqual(retry.returncode, 0)
        self.assertIn('run changed', retry.stderr)
        self.assertNotEqual(self.cli('prompt', 'builder', 'working', '--operation', '1', check=False).returncode, 0)
        self.assertNotEqual(self.cli('prompt-receipt', 'builder', '--run', launch['run'], '--operation', '1', check=False).returncode, 0)
        self.assertEqual(self.cli('get', 'builder')['state'], 'idle')

    def test_prompt_rejects_synchronized_panes_and_copy_mode(self):
        launch = self.start()
        peer = self.start('peer', '--split', launch['pane_id'])
        self.server.run('send-keys', '-t', peer['pane_id'], 'blocked')
        self.server.run('set-window-option', '-t', launch['pane_id'], 'synchronize-panes', 'on')
        self.assertNotEqual(self.cli('prompt', 'builder', 'working', check=False).returncode, 0)
        self.assertEqual(self.cli('get', 'peer')['state'], 'idle')
        self.assertNotIn('INPUTS:1', self.cli('read', 'peer')['text'])
        self.server.run('set-window-option', '-t', launch['pane_id'], 'synchronize-panes', 'off')
        self.server.run('copy-mode', '-t', launch['pane_id'])
        self.assertNotEqual(self.cli('prompt', 'builder', 'working', check=False).returncode, 0)
        self.server.run('send-keys', '-t', launch['pane_id'], '-X', 'cancel')
        self.assertEqual(self.cli('get', 'builder')['state'], 'idle')
        self.assertNotIn('working', self.cli('read', 'builder')['text'])

    def test_prompt_rejects_screen_or_title_changed_during_preparation(self):
        if self.uses_core_ledger():
            launch = self.start()
            pane = launch['pane_id']
            self.cli('coordinator', 'stop')
            peer = Peer(core_bridge(self.server), 'coordinator')
            try:
                state = core_input_state(self.server, pane)
                self.server.run('send-keys', '-t', pane, 'blocked', 'Enter')
                commit_staged_core_input(
                    self.server,
                    pane,
                    peer,
                    core_operation_key(state, 'screen-changed'),
                    core_ticket(peer),
                    'working',
                    state,
                    'rejected_before_effect:output_changed',
                )
                wait_for(lambda: self.cli('get', 'builder')['state'] == 'blocked', 4)
                # The test holds the coordinator role, so type through the
                # server rather than an agent command that needs it.
                self.server.run('send-keys', '-t', pane, 'idle', 'Enter')
                wait_for(lambda: self.cli('get', 'builder')['state'] == 'idle', 4)
                state = core_input_state(self.server, pane)
                self.server.run('select-pane', '-t', pane, '-T', 'changed-title')
                commit_staged_core_input(
                    self.server,
                    pane,
                    peer,
                    core_operation_key(state, 'title-changed'),
                    core_ticket(peer, 1),
                    'working',
                    state,
                    'rejected_before_effect:title_changed',
                )
            finally:
                peer.close()
            self.assertEqual(self.cli('get', 'builder')['state'], 'idle')
            text = self.cli('read', 'builder')['text']
            self.assertIn('INPUTS:2', text)
            self.assertNotIn('INPUTS:3', text)
            return
        launch = self.start()
        pane = launch['pane_id']
        self.server.run('set-hook', '-g', 'after-load-buffer',
                        f"send-keys -t {pane} blocked Enter ; run-shell 'sleep 0.15'")
        result = self.cli('prompt', 'builder', 'yes', check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.cli('get', 'builder')['state'], 'blocked')
        self.assertIn('INPUTS:1', self.cli('read', 'builder')['text'])
        self.server.run('set-hook', '-gu', 'after-load-buffer')
        self.cli('send-keys', 'builder', 'idle', 'Enter')
        wait_for(lambda: self.cli('get', 'builder')['state'] == 'idle')
        self.server.run('set-hook', '-g', 'after-load-buffer', f'select-pane -t {pane} -T changed-title')
        self.assertNotEqual(self.cli('prompt', 'builder', 'working', check=False).returncode, 0)
        self.assertEqual(self.cli('get', 'builder')['state'], 'idle')

    def test_prompt_paste_failure_never_sends_enter_or_marks_delivered(self):
        if self.uses_core_ledger():
            self.skipTest("the native buffer-removal fault does not exist for staged core input")
        import shlex
        self.start()
        native = shlex.join([str(MASIL), '-S', str(self.server.socket)])
        remove_buffer = f'{native} delete-buffer -b "$({native} list-buffers | head -1 | cut -d: -f1)"'
        self.server.run('set-hook', '-g', 'after-load-buffer', 'run-shell ' + shlex.quote(remove_buffer))
        result = self.cli('prompt', 'builder', 'working', check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.cli('prompt-receipt', 'builder')['stage'], 'pending')
        self.assertEqual(self.cli('get', 'builder')['state'], 'idle')
        self.assertNotIn('INPUTS:', self.cli('read', 'builder')['text'])

    def test_prompt_title_format_characters_are_literal(self):
        launch = self.start()
        self.server.run('select-pane', '-t', launch['pane_id'], '-T', "a#{?pane_id,yes,no},b}'$(false)")
        self.assertEqual(self.cli('prompt', 'builder', 'working')['stage'], 'delivered')
        wait_for(lambda: self.cli('get', 'builder')['state'] == 'working')


if __name__ == '__main__':
    unittest.main(verbosity=2)
