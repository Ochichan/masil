#!/usr/bin/env python3
"""Exercise installed sibling binaries without user configuration or providers."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile


def smoke(binary_dir):
    core = str(binary_dir / 'masil')
    agent = str(binary_dir / 'masil-agent')
    # macOS's default temp directory can exceed the Unix socket path limit.
    with tempfile.TemporaryDirectory(prefix='masil-ci-', dir='/tmp') as temporary:
        home = Path(temporary)
        socket = str(home / 'core.sock')
        bridge = str(home / 'bridge.sock')
        env = {
            'PATH': str(binary_dir) + os.pathsep + '/usr/bin:/bin:/usr/sbin:/sbin',
            'HOME': temporary, 'SHELL': '/bin/sh', 'TERM': 'xterm-256color',
            'LC_ALL': 'en_US.UTF-8' if os.uname().sysname == 'Darwin' else 'C.UTF-8',
            'XDG_CONFIG_HOME': str(home / 'config'),
            'XDG_STATE_HOME': str(home / 'state'),
            'TMPDIR': temporary, 'TMUX_TMPDIR': temporary,
            'MASIL_BRIDGE_SOCKET': bridge,
        }

        def run(*args, check=True):
            result = subprocess.run(args, env=env, cwd=home, text=True,
                                    stdin=subprocess.DEVNULL, capture_output=True,
                                    timeout=15, check=False)
            if check and result.returncode:
                raise RuntimeError(f'{args!r}: {result.stdout}\n{result.stderr}')
            return result.stdout.strip()

        def tmux(*args, check=True):
            return run(core, '-S', socket, '-f', '/dev/null', *args, check=check)

        assert run(core, '-V').startswith('tmux ')
        assert run(agent, '--version').startswith('masil-agent ')
        assert json.loads(run(agent, 'agent', 'providers'))['providers']
        try:
            tmux('new-session', '-d', '-s', 'ci', '-x', '80', '-y', '24', '/bin/sh')
            assert tmux('list-sessions', '-F', '#{session_name}') == 'ci'
            pane = tmux('list-panes', '-t', 'ci', '-F', '#{pane_id}')
            assert pane.startswith('%')
            assert Path(bridge).is_socket()
            for command in [('hello',), ('inventory',), ('snapshot', pane), ('stats',)]:
                assert json.loads(run(agent, '--socket', bridge, *command))
            assert json.loads(run(agent, 'agent', '--socket', socket, 'list'))['agents'] == []
            report = json.loads(run(agent, 'doctor', '--socket', socket))
            assert report['summary']['fail'] == 0, report
        finally:
            tmux('kill-server', check=False)
    print(f'Smoke passed: {binary_dir}')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('binary_dir', type=Path)
    smoke(parser.parse_args().binary_dir.resolve())
