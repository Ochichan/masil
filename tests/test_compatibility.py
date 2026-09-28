#!/usr/bin/env python3
"""Differential public-interface tests against the exact upstream build."""
import contextlib
import fcntl
import json
import os
from pathlib import Path
import pty
import select
import struct
import subprocess
import sys
import tempfile
import termios
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]
RMUX = ROOT / 'bin/rmux'
BASELINE = ROOT / 'bin/tmux-baseline'


def isolated_env(path):
    return {'PATH': '/usr/bin:/bin:/usr/sbin:/sbin', 'HOME': str(path),
            'SHELL': '/bin/sh', 'TERM': 'xterm-256color',
            'LC_CTYPE': 'en_US.UTF-8' if sys.platform == 'darwin' else 'C.UTF-8',
            'TMPDIR': str(path), 'TMUX_TMPDIR': str(path)}


class Server:
    def __init__(self, binary, command=None, extra_env=None):
        self.temp = tempfile.TemporaryDirectory(prefix='rmx-test-', dir='/tmp')
        self.path = Path(self.temp.name)
        self.socket = self.path / 'core.sock'
        self.binary = str(binary)
        self.env = isolated_env(self.path)
        self.env.update(extra_env or {})
        self.command = command or ['/bin/sh']

    def __enter__(self):
        try:
            self.run('new-session', '-d', '-s', 'main', '-x', '120', '-y', '40', *self.command)
        except subprocess.CalledProcessError as error:
            self.__exit__()
            raise AssertionError(error.stderr) from error
        return self

    def run(self, *args, check=True, env=None):
        return subprocess.run([self.binary, '-S', str(self.socket), '-f', '/dev/null', *args],
                              env=env or self.env, stdin=subprocess.DEVNULL,
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                              text=True, check=check, timeout=5)

    def text(self, *args):
        return self.run(*args).stdout

    def __exit__(self, *_):
        try:
            self.run('kill-server', check=False)
        finally:
            self.temp.cleanup()


def wait_for(predicate, timeout=3):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        if predicate():
            return
        time.sleep(.01)
    raise AssertionError('condition not satisfied before deadline')


@contextlib.contextmanager
def attached(server):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 40, 120, 0, 0))
    child = subprocess.Popen([server.binary, '-S', str(server.socket), 'attach-session', '-t', 'main'],
                             stdin=slave, stdout=slave, stderr=slave, env=server.env,
                             start_new_session=True)
    os.close(slave)
    try:
        wait_for(lambda: bool(server.text('list-clients', '-F', '#{client_pid}').strip()))
        yield master, child
    finally:
        if child.poll() is None:
            child.terminate()
        try:
            child.wait(timeout=3)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait()
        os.close(master)


class Compatibility(unittest.TestCase):
    def test_default_public_inventory(self):
        with Server(BASELINE) as base, Server(RMUX) as rmux:
            queries = [('list-commands',), ('list-keys', '-a'),
                       ('show-options', '-g'), ('show-options', '-gw'), ('show-options', '-gs')]
            for query in queries:
                with self.subTest(query=query):
                    self.assertEqual(base.text(*query), rmux.text(*query))
            self.assertEqual(len(rmux.text('list-commands').splitlines()), 92)
            self.assertEqual(rmux.text('show-options', '-gv', 'prefix').strip(), 'C-b')

    def test_editor_defaults(self):
        for editor in ('/usr/bin/vi', '/usr/bin/emacs'):
            with self.subTest(editor=editor), Server(BASELINE, extra_env={'EDITOR': editor}) as b, Server(RMUX, extra_env={'EDITOR': editor}) as r:
                self.assertEqual(b.text('show-options', '-gwv', 'mode-keys'),
                                 r.text('show-options', '-gwv', 'mode-keys'))

    def test_layout_session_links_and_zoom(self):
        with Server(BASELINE) as b, Server(RMUX) as r:
            actions = [('split-window', '-h', '-t', 'main:0', '/bin/sh'),
                       ('split-window', '-v', '-t', '%0', '/bin/sh'),
                       ('select-layout', '-t', 'main:0', 'tiled'),
                       ('resize-pane', '-t', '%0', '-Z'),
                       ('resize-pane', '-t', '%0', '-Z'),
                       ('new-session', '-d', '-s', 'other', '/bin/sh'),
                       ('link-window', '-s', 'main:0', '-t', 'other:1'),
                       ('unlink-window', '-t', 'other:1')]
            for action in actions:
                for server in (b, r):
                    server.run(*action)
                self.assertEqual(b.text('list-panes', '-a', '-F', '#{session_name}:#{window_index}:#{pane_id}:#{pane_width}x#{pane_height}:#{window_zoomed_flag}'),
                                 r.text('list-panes', '-a', '-F', '#{session_name}:#{window_index}:#{pane_id}:#{pane_width}x#{pane_height}:#{window_zoomed_flag}'))

    def test_buffers_and_formats(self):
        with Server(BASELINE) as b, Server(RMUX) as r:
            content = '한글 테스트\n中文 日本語\ncombining e\u0301\tend\n'
            for server in (b, r):
                path = server.path / 'buffer'
                path.write_text(content)
                server.run('load-buffer', '-b', 'unicode', str(path))
                self.assertEqual(server.text('save-buffer', '-b', 'unicode', '-'), content)
                server.run('set-option', '-g', '@example', 'value')
            for fmt in ('#{@example}', '#{session_name}:#{window_index}:#{pane_index}',
                        '#{?pane_in_mode,mode,normal}', '#{e|+|:7,3}'):
                self.assertEqual(b.text('display-message', '-p', fmt), r.text('display-message', '-p', fmt))

    def test_unicode_capture_and_copy_mode(self):
        code = "import sys,time;print('RMUX_READY 한글 中文 e\\u0301',flush=True);time.sleep(30)"
        with Server(BASELINE, [sys.executable, '-u', '-c', code]) as b, Server(RMUX, [sys.executable, '-u', '-c', code]) as r:
            for server in (b, r):
                wait_for(lambda: 'RMUX_READY' in server.text('capture-pane', '-p'))
                server.run('copy-mode')
                self.assertEqual(server.text('display-message', '-p', '#{pane_in_mode}').strip(), '1')
            self.assertEqual(b.text('capture-pane', '-p'), r.text('capture-pane', '-p'))

    def test_native_prefix_and_detach(self):
        for binary in (BASELINE, RMUX):
            with self.subTest(binary=binary.name), Server(binary) as server:
                with attached(server) as (master, child):
                    os.write(master, b'\x02c')
                    wait_for(lambda: len(server.text('list-windows').splitlines()) == 2)
                    os.write(master, b'\x02d')
                    child.wait(timeout=3)
                self.assertEqual(len(server.text('list-windows').splitlines()), 2)

    def test_control_mode(self):
        for binary in (BASELINE, RMUX):
            with self.subTest(binary=binary.name), Server(binary) as server:
                child = subprocess.Popen([server.binary, '-S', str(server.socket), '-C', 'attach-session', '-t', 'main'],
                                         env=server.env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
                try:
                    child.stdin.write(b'display-message -p CONTROL_ROUNDTRIP\n')
                    child.stdin.flush()
                    data = b''
                    end = time.monotonic() + 3
                    while time.monotonic() < end and b'CONTROL_ROUNDTRIP\n%end' not in data:
                        if select.select([child.stdout], [], [], .1)[0]:
                            block = os.read(child.stdout.fileno(), 65536)
                            if not block:
                                break
                            data += block
                    self.assertIn(b'%begin ', data)
                    self.assertIn(b'CONTROL_ROUNDTRIP\n%end', data)
                finally:
                    child.terminate()
                    child.communicate(timeout=3)

    def test_protocol_product_isolation_and_inheritance(self):
        with Server(BASELINE) as b, Server(RMUX) as r:
            for own, foreign in ((r, b), (b, r)):
                env = own.env | {'TMUX': str(foreign.socket) + ',1,0'}
                result = subprocess.run([own.binary, 'kill-server'], env=env,
                                        capture_output=True, text=True, timeout=3)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn('protocol version mismatch', result.stderr)
                self.assertEqual(foreign.text('display-message', '-p', '#{session_name}').strip(), 'main')
            env = r.env | {'TMUX': str(r.socket) + ',1,0'}
            result = subprocess.run([r.binary, 'list-sessions', '-F', '#{session_name}'], env=env,
                                    capture_output=True, text=True, timeout=3)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout.strip(), 'main')

    def test_native_hook_and_async_queue(self):
        with Server(BASELINE) as b, Server(RMUX) as r:
            for server in (b, r):
                server.run('set-hook', '-g', 'after-new-window', 'set-option -g @hook seen')
                server.run('new-window', '-d', '/bin/sh')
                wait_for(lambda: server.text('show-options', '-gv', '@hook').strip() == 'seen')
            self.assertEqual(b.text('show-hooks', '-g'), r.text('show-hooks', '-g'))

    def test_native_error_semantics_and_alias(self):
        with Server(BASELINE) as b, Server(RMUX) as r:
            for action in [('select-pane', '-t', '%999'), ('definitely-not-a-command',),
                           ('set-option', '-g', 'not-an-option', 'x'), ('lsp', '-F', '#{pane_id}')]:
                left, right = b.run(*action, check=False), r.run(*action, check=False)
                self.assertEqual((left.returncode, left.stdout, left.stderr),
                                 (right.returncode, right.stdout, right.stderr))


if __name__ == '__main__':
    for binary in (RMUX, BASELINE):
        if not binary.is_file():
            raise SystemExit(f'Build first: missing {binary}')
    unittest.main(verbosity=2)
