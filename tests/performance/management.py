#!/usr/bin/env python3
"""Measure masil's resident management features with a private synthetic provider."""
import argparse
from datetime import datetime, timezone
import json
from pathlib import Path
import statistics
import subprocess
import sys
import time

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from test_managed_agents import AGENT, MASIL, ManagedAgents
from benchmark import AttachedClient, BenchmarkError, process_sample, sha256


def sample_roles(roles):
    result = {}
    for role, pid in roles.items():
        sample = process_sample(pid)
        if sample is None or sample['pid'] != pid:
            raise BenchmarkError(f'{role} process {pid} disappeared during measurement')
        result[role] = sample
    return result


def measure(count, seconds, samples):
    fixture = ManagedAgents()
    ui = None
    try:
        fixture.setUp()
        fixture.cli('inbox', 'enable')
        for index in range(count):
            fixture.start(f'agent{index}')
        coordinator = fixture.cli('coordinator', 'status')
        if coordinator['state'] != 'running':
            raise BenchmarkError('coordinator did not start')
        list_ms = []
        for _ in range(samples):
            started = time.monotonic()
            agents = fixture.cli('list')['agents']
            list_ms.append((time.monotonic() - started) * 1000)
            if len(agents) != count:
                raise BenchmarkError(f'expected {count} synthetic agents, got {len(agents)}')
        # AttachedClient creates a new session. Claim its PTY before exec so
        # the desk can verify stdin/stdout belong to that controlling terminal.
        claim_tty = ('import fcntl, os, sys, termios; '
                     'fcntl.ioctl(0, termios.TIOCSCTTY, 0); '
                     'os.execv(sys.argv[1], sys.argv[1:])')
        ui = AttachedClient(
            [sys.executable, '-c', claim_tty, str(AGENT), 'agent',
             '--socket', str(fixture.server.socket), 'ui'],
            fixture.env, fixture.server.path, fixture.server.path / 'desk.raw',
        )
        try:
            ui.wait_marker(b'agent0', 15)
        except BenchmarkError as error:
            raise BenchmarkError(f'{error}; desk output: {bytes(ui.buffer[-2000:])!r}') from error
        ui.drain(1)
        roles = {
            'core': int(fixture.server.text('display-message', '-p', '#{pid}')),
            'coordinator': int(coordinator['coordinator']['pid']),
            'desk': ui.process.pid,
        }
        before = sample_roles(roles)
        peaks = {role: sample['rss_kib'] for role, sample in before.items()}
        started = time.monotonic()
        emitted = 0
        while time.monotonic() - started < seconds:
            emitted += len(ui.drain(min(.25, max(0, seconds - (time.monotonic() - started)))))
            current = sample_roles(roles)
            for role, sample in current.items():
                peaks[role] = max(peaks[role], sample['rss_kib'])
        after = sample_roles(roles)
        elapsed = time.monotonic() - started
        resources = {}
        for role, sample in after.items():
            cpu = sample['cpu_time_seconds'] - before[role]['cpu_time_seconds']
            if cpu < 0:
                raise BenchmarkError(f'{role} CPU accounting went backwards')
            resources[role] = {
                'rss_kib_end': sample['rss_kib'], 'rss_kib_sample_peak': peaks[role],
                'cpu_seconds': cpu, 'percent_of_one_core': cpu / elapsed * 100,
            }
        return {
            'agents': count, 'status': 'measured', 'list_ms': list_ms,
            'list_median_ms': statistics.median(list_ms), 'wall_seconds': elapsed,
            'roles': resources, 'desk_output_bytes': emitted,
        }
    finally:
        pending_error = sys.exc_info()[0] is not None
        close_error = None
        if ui is not None:
            try:
                if ui.process.poll() is None:
                    ui.send(b'q')
                    ui.drain(.25)
            except (OSError, subprocess.TimeoutExpired, BenchmarkError) as error:
                close_error = error
            finally:
                try:
                    ui.close()
                except (OSError, subprocess.TimeoutExpired, BenchmarkError) as error:
                    close_error = error
        if not fixture.doCleanups():
            raise BenchmarkError('private management fixture cleanup failed')
        if close_error is not None and not pending_error:
            raise BenchmarkError(f'desk cleanup failed: {close_error}')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--counts', default='1,15,50')
    parser.add_argument('--seconds', type=float, default=5)
    parser.add_argument('--samples', type=int, default=10)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    try:
        counts = [int(value) for value in args.counts.split(',')]
    except ValueError:
        parser.error('--counts must be comma-separated integers')
    if not counts or any(value < 1 or value > 63 for value in counts):
        parser.error('--counts must be between 1 and 63 (one shell pane is retained)')
    if not 0 < args.seconds <= 600 or args.samples < 1:
        parser.error('--seconds must be in (0, 600] and --samples must be positive')
    if args.output.exists():
        parser.error('refusing to overwrite an existing report')
    report = {
        'schema_version': 1, 'status': 'running',
        'scope': 'masil only: synthetic provider, custom detection rules, inbox, coordinator, full-screen desk and list; no native provider, model calls, SSH or Herdr comparison',
        'started_at_utc': datetime.now(timezone.utc).isoformat(),
        'binary_sha256': {'masil': sha256(MASIL), 'masil-agent': sha256(AGENT)},
        'harness_sha256': sha256(Path(__file__)),
        'fixture_sha256': sha256(Path(__file__).resolve().parents[1] / 'faults/agent.c'),
        'fixture_setup_sha256': sha256(Path(__file__).resolve().parents[1] / 'test_managed_agents.py'),
        'parameters': {'counts': counts, 'seconds': args.seconds, 'samples': args.samples},
        'measurements': [], 'performance_budget_pass': None,
    }
    try:
        ManagedAgents.setUpClass()
        try:
            for count in counts:
                print(f'management: {count} synthetic agents', file=sys.stderr, flush=True)
                report['measurements'].append(measure(count, args.seconds, args.samples))
        finally:
            ManagedAgents.tearDownClass()
        if report['binary_sha256'] != {'masil': sha256(MASIL), 'masil-agent': sha256(AGENT)}:
            raise BenchmarkError('a binary changed during management measurement')
        if report['harness_sha256'] != sha256(Path(__file__)):
            raise BenchmarkError('management measurement code changed during the run')
        report['status'] = 'measured'
    except Exception as error:
        report['status'] = 'error'
        report['error'] = f'{type(error).__name__}: {error}'
    report['completed_at_utc'] = datetime.now(timezone.utc).isoformat()
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + '\n')
    return 0 if report['status'] == 'measured' else 1


if __name__ == '__main__':
    raise SystemExit(main())
