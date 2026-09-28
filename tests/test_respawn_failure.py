#!/usr/bin/env python3
"""Inject forkpty failure into a private macOS test server only."""
import json
import os
from pathlib import Path
import socket
import struct
import subprocess
import sys
import tempfile

from test_compatibility import ROOT, RMUX, Server


def exact(connection, count):
    data = b''
    while len(data) < count:
        chunk = connection.recv(count - len(data))
        if not chunk:
            raise AssertionError('unexpected EOF')
        data += chunk
    return data


def request(connection, kind, **fields):
    body = json.dumps({'v': 1, 'kind': kind, 'request_id': kind, **fields}).encode()
    connection.sendall(struct.pack('>I', len(body)) + body)
    length = struct.unpack('>I', exact(connection, 4))[0]
    assert 0 < length <= 65536
    return json.loads(exact(connection, length))


def main():
    if sys.platform != 'darwin':
        print('forkpty failure injection: unsupported on this test platform')
        return 77
    with tempfile.TemporaryDirectory(prefix='rmx-fault-', dir='/tmp') as directory:
        directory = Path(directory)
        library = directory / 'forkpty.dylib'
        marker = directory / 'fail'
        bridge = directory / 'bridge.sock'
        subprocess.run(['clang', '-dynamiclib', '-Wall', '-Wextra', '-Werror',
                        '-o', str(library), str(ROOT/'tests/faults/forkpty.c')], check=True)
        env = {'DYLD_INSERT_LIBRARIES': str(library),
               'RMUX_TEST_FAIL_FORKPTY_FILE': str(marker),
               'RMUX_BRIDGE_SOCKET': str(bridge)}
        with Server(RMUX, ['/bin/sleep', '60'], env) as server:
            with socket.socket(socket.AF_UNIX) as connection:
                connection.settimeout(3)
                connection.connect(str(bridge))
                assert request(connection, 'hello')['kind'] == 'hello'
                before = request(connection, 'snapshot', pane_id='%0')
                assert before['kind'] == 'snapshot'
                marker.touch()
                failure = server.run('respawn-pane', '-k', '-t', '%0', '/bin/sleep', '60', check=False)
                assert failure.returncode != 0 and 'fork failed' in failure.stderr, failure
                after = request(connection, 'snapshot', pane_id='%0')
                assert int(after['pty_generation']) > int(before['pty_generation'])
                assert int(after['screen_generation']) > int(before['screen_generation'])
                for field in ('pty_generation', 'screen_generation'):
                    rejected = request(connection, 'snapshot', pane_id='%0',
                                       **{'expected_'+field: before[field]})
                    assert rejected['code'] == field+'_mismatch', rejected
                marker.unlink()
                server.run('respawn-pane', '-k', '-t', '%0', '/bin/sleep', '60')
                restored = request(connection, 'snapshot', pane_id='%0')
                assert int(restored['pty_generation']) == int(after['pty_generation']) + 1
    print('forkpty failure injection: passed')
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
