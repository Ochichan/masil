#!/usr/bin/env python3
"""Run the upstream regression scripts in private copied workspaces."""
import argparse
import concurrent.futures
import json
import os
from pathlib import Path
import shutil
import shlex
import signal
import stat
import subprocess
import tempfile
import time
import uuid

ROOT = Path(__file__).resolve().parents[1]


def run_one(binary, script, timeout, run_dir):
    logs = run_dir / 'logs' / binary.name
    logs.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='msl-reg-', dir='/tmp') as tmp:
        directory = Path(tmp)
        work = directory / 'regress'
        shutil.copytree(ROOT / 'core/regress', work,
                        ignore=shutil.ignore_patterns('logs'))
        normalizations = []
        copied = work/script.name
        source_text = copied.read_text()
        if script.name == 'socket-path.sh' and binary.name == 'masil':
            # The documented product namespace is the only expected difference.
            source_text = source_text.replace('/tmux-$(id -u)', '/masil-$(id -u)')
            normalizations.append('socket namespace tmux-UID -> masil-UID')
        if script.name == 'terminal-feature-utf8.sh':
            # The test deliberately clears locale/environment for a nested
            # client; retain only our private socket directory across env -i.
            source_text = source_text.replace('env -i PATH=',
                'env -i TMUX_TMPDIR=' + shlex.quote(tmp) + ' PATH=')
            normalizations.append('retain private socket directory across nested env -i')
        copied.write_text(source_text)
        # Some scripts create files next to their own source. Keep that private.
        env = {'PATH': '/usr/bin:/bin:/usr/sbin:/sbin', 'HOME': tmp,
               'SHELL': '/bin/sh', 'TERM': 'screen', 'LC_CTYPE': 'UTF-8',
               'TMPDIR': tmp, 'TMUX_TMPDIR': tmp, 'TEST_TMUX': str(binary),
               'MallocNanoZone': '0'}
        log = logs / (script.name + '.log')
        started = time.monotonic()
        status = 'failed'
        with log.open('w') as output:
            process = subprocess.Popen(['/bin/sh', '-x', script.name], cwd=work,
                                       env=env, stdout=output, stderr=subprocess.STDOUT,
                                       start_new_session=True)
            try:
                code = process.wait(timeout=timeout)
                status = 'passed' if code == 0 else 'failed'
            except subprocess.TimeoutExpired:
                code = None
                status = 'timeout'
                os.killpg(process.pid, signal.SIGTERM)
                try:
                    process.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait()
            finally:
                # Only sockets created inside this single test's private root.
                for path in directory.rglob('*'):
                    try:
                        is_socket = stat.S_ISSOCK(path.lstat().st_mode)
                    except FileNotFoundError:
                        continue
                    if is_socket:
                        subprocess.run([str(binary), '-S', str(path), 'kill-server'],
                                       env=env, stdout=subprocess.DEVNULL,
                                       stderr=subprocess.DEVNULL, timeout=3)
                # A script may exit before its FIFO writer or sleep helper.
                # Every script has its own session/process group from Popen.
                try:
                    os.killpg(process.pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
        result = {'binary': binary.name, 'script': script.name, 'status': status,
                  'returncode': code, 'process_group': process.pid,
                  'normalizations': normalizations,
                  'seconds': time.monotonic() - started,
                  'log': str(log.relative_to(ROOT))}
        print(f'{binary.name} {script.name}: {status}', flush=True)
        return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--jobs', type=int, default=4)
    parser.add_argument('--timeout', type=float, default=60)
    parser.add_argument('--match', default='*.sh')
    args = parser.parse_args()
    scripts = sorted((ROOT/'core/regress').glob(args.match))
    inputs = [ROOT/'bin/tmux-baseline', ROOT/'bin/masil']
    if not all(b.is_file() for b in inputs):
        parser.error('build both binaries first')
    run_dir = ROOT/'.build'/('regress-'+uuid.uuid4().hex[:12])
    frozen = run_dir/'bin'
    frozen.mkdir(parents=True, exist_ok=True)
    binaries = []
    for source in inputs:
        destination = frozen/source.name
        shutil.copy2(source, destination)
        binaries.append(destination)
    results = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.jobs) as pool:
        tasks = [pool.submit(run_one, b, s, args.timeout, run_dir) for s in scripts for b in binaries]
        for task in concurrent.futures.as_completed(tasks):
            results.append(task.result())
    results.sort(key=lambda r: (r['script'], r['binary']))
    path = run_dir/'results.json'
    path.write_text(json.dumps(results, indent=2)+'\n')
    if args.match == '*.sh':
        (ROOT/'.build/upstream-regress.json').write_text(json.dumps(results, indent=2)+'\n')
    print('results:', path)
    for binary in binaries:
        counts = {state: sum(r['binary']==binary.name and r['status']==state for r in results)
                  for state in ('passed','failed','timeout')}
        print(binary.name, counts)
    regressions = []
    for script in scripts:
        pair = {r['binary']:r['status'] for r in results if r['script']==script.name}
        if pair.get('tmux-baseline')=='passed' and pair.get('masil')!='passed':
            regressions.append(script.name)
    print('masil-only regressions:', regressions)
    # Any failure remains a failing gate, even when also present upstream.
    raise SystemExit(any(r['status']!='passed' for r in results))


if __name__ == '__main__':
    main()
