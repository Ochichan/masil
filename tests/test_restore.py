#!/usr/bin/env python3
"""`agent checkpoint TARGET restore`: files only, previewed, saved first."""

import json
import os
from pathlib import Path
import subprocess
import time
import unittest

from test_compatibility import wait_for
import test_managed_agents
from test_managed_agents import AGENT


class Restore(test_managed_agents.ManagedAgents):
    def setUp(self):
        super().setUp()
        self.home = Path(self.env['HOME']).resolve()
        self.env |= {'GIT_AUTHOR_NAME': 't', 'GIT_AUTHOR_EMAIL': 't@t', 'GIT_COMMITTER_NAME': 't',
                     'GIT_COMMITTER_EMAIL': 't@t', 'GIT_CONFIG_NOSYSTEM': '1'}
        self.repo = (self.server.path / 'repo').resolve()
        self.git('init', '-q', '-b', 'main', str(self.repo), cwd=self.server.path)
        (self.repo / '.gitignore').write_text('*.log\n')
        (self.repo / 'a.txt').write_text('original\n')
        (self.repo / 'keep.txt').write_text('keep\n')
        self.git('add', '-A')
        self.git('commit', '-q', '-m', 'base')
        self.cli('start', 'cx', 'codex', '--cwd', str(self.repo))
        wait_for(lambda: self.cli('get', 'cx')['state'] == 'idle', 4)

    def git(self, *args, cwd=None):
        return subprocess.run(['git', *args], cwd=cwd or self.repo, env=self.env, check=True,
                              capture_output=True, text=True).stdout

    def cp(self, *args, code=0):
        result = subprocess.run([str(AGENT), 'agent', '--socket', str(self.server.socket), 'checkpoint', 'cx',
                                 *args], env=self.env, text=True, capture_output=True, timeout=60)
        self.assertEqual(result.returncode, code, result.stderr + result.stdout)
        return json.loads(result.stdout) if code == 0 else result

    def restore(self, id, *paths):
        preview = self.cp('restore', str(id), *paths)
        self.assertEqual(preview['stage'], 'preview')
        return preview, self.cp('restore', str(id), *paths, '--confirm', preview['token'])

    def stored(self, commit, path):
        (store,) = (self.home / '.local/state/masil/checkpoints').glob('*.git')
        return subprocess.run(['git', '--git-dir', str(store), 'cat-file', '-p', f'{commit}:w/{path}'],
                              capture_output=True, check=True).stdout

    def commit_of(self, id):
        return next(c['commit'] for c in self.cp('list')['checkpoints'] if c['id'] == id)

    def test_a_previewed_restore_saves_the_files_first_and_writes_only_them(self):
        (self.repo / 'a.txt').write_text('saved\n')
        (self.repo / 'gone.txt').write_text('will be deleted, then back\n')
        (self.repo / 'run.sh').write_text('#!/bin/sh\n')
        (self.repo / 'run.sh').chmod(0o755)
        (self.repo / 'secret.txt').write_text('private\n')
        (self.repo / 'secret.txt').chmod(0o600)
        (self.repo / 'link').symlink_to('a.txt')
        self.cp('make', '--reason', 'good state')
        # The agent then edits things.
        (self.repo / 'a.txt').write_text('broken\n')
        (self.repo / 'gone.txt').unlink()
        (self.repo / 'run.sh').chmod(0o644)
        (self.repo / 'secret.txt').write_text('changed\n')
        (self.repo / 'link').unlink()
        (self.repo / 'link').symlink_to('keep.txt')
        (self.repo / 'new.txt').write_text('agent made this\n')
        targets = ('a.txt', 'gone.txt', 'run.sh', 'secret.txt', 'link', 'new.txt')
        preview, done = self.restore(1, *targets)
        actions = {f['path']: f['action'] for f in preview['files']}
        self.assertEqual(actions, {'a.txt': 'restore', 'gone.txt': 'restore', 'run.sh': 'restore',
                                   'secret.txt': 'restore', 'link': 'restore', 'new.txt': 'delete'})
        diff = next(f['diff'] for f in preview['files'] if f['path'] == 'a.txt')
        self.assertIn('-broken\n+saved\n', diff)
        self.assertIn('conversation', preview['note'])
        self.assertEqual(done['stage'], 'restored', done)
        self.assertEqual({f['path']: f['result'] for f in done['files']},
                         {'a.txt': 'restored', 'gone.txt': 'restored', 'run.sh': 'restored',
                          'secret.txt': 'restored', 'link': 'restored', 'new.txt': 'deleted'})
        self.assertEqual((self.repo / 'a.txt').read_text(), 'saved\n')
        self.assertEqual((self.repo / 'gone.txt').read_text(), 'will be deleted, then back\n')
        self.assertEqual(oct((self.repo / 'run.sh').stat().st_mode & 0o777), '0o755')
        self.assertEqual(oct((self.repo / 'secret.txt').stat().st_mode & 0o777), '0o600')
        self.assertEqual(os.readlink(self.repo / 'link'), 'a.txt')
        self.assertFalse((self.repo / 'new.txt').exists())
        self.assertEqual(sorted(p.name for p in self.repo.iterdir() if p.name.startswith('.masil')), [])
        # What was overwritten is in the checkpoint made just before.
        before = self.commit_of(done['before'])
        self.assertEqual(self.stored(before, 'a.txt'), b'broken\n')
        self.assertEqual(self.stored(before, 'new.txt'), b'agent made this\n')
        # Files only: git's own state is as it was.
        self.assertEqual(self.git('log', '--oneline').count('\n'), 1)

    def test_nothing_is_written_when_files_change_after_the_preview(self):
        self.cp('make')
        (self.repo / 'a.txt').write_text('first edit\n')
        preview = self.cp('restore', '1', 'a.txt')
        (self.repo / 'a.txt').write_text('second edit\n')
        stale = self.cp('restore', '1', 'a.txt', '--confirm', preview['token'], code=3)
        self.assertIn('checkpoint_stale', stale.stderr)
        self.assertEqual((self.repo / 'a.txt').read_text(), 'second edit\n')
        wrong = self.cp('restore', '1', 'a.txt', '--confirm', '0' * 16, code=3)
        self.assertIn('checkpoint_stale', wrong.stderr)

    def test_a_working_agent_blocks_a_restore(self):
        self.cp('make')
        (self.repo / 'a.txt').write_text('edit\n')
        self.cli('prompt', 'cx', 'working')
        wait_for(lambda: self.cli('get', 'cx')['state'] == 'working', 4)
        refused = self.cp('restore', '1', 'a.txt', code=3)
        self.assertIn('checkpoint_agent_working', refused.stderr)
        self.assertEqual((self.repo / 'a.txt').read_text(), 'edit\n')

    def test_what_no_checkpoint_holds_is_never_written_over_or_deleted(self):
        (self.repo / 'big.bin').write_bytes(b'1' * (11 * 1024 * 1024))
        self.cp('make')
        (self.repo / 'big.bin').write_bytes(b'2')
        incomplete = self.cp('restore', '1', 'big.bin', code=3)
        self.assertIn('checkpoint_incomplete', incomplete.stderr)
        (self.repo / 'notes.log').write_text('ignored, never saved\n')
        ignored = self.cp('restore', '1', 'notes.log', code=3)
        self.assertIn('checkpoint_incomplete', ignored.stderr)
        self.assertTrue((self.repo / 'notes.log').exists())

    def test_paths_must_be_plain_and_named_exactly(self):
        self.cp('make')
        outside = self.server.path / 'outside'
        outside.mkdir()
        (outside / 'x.txt').write_text('outside\n')
        (self.repo / 'linked').symlink_to(outside)
        for path in ('../x', '/etc/hosts', '.git/config', 'linked/x.txt', 'A.txt', 'missing.txt'):
            result = self.cp('restore', '1', path, code=2)
            self.assertIn('invalid_argument', result.stderr, path)
        self.assertEqual((outside / 'x.txt').read_text(), 'outside\n')

    def test_an_interrupted_restore_is_shown_and_its_leftovers_cleared(self):
        self.cp('make')
        listed = self.cp('list')
        self.assertNotIn('interrupted_restore', listed)
        (store,) = (self.home / '.local/state/masil/checkpoints').glob('*.git')
        key = next((store / 'masil-index').iterdir()).name
        journal = store / 'masil-restore' / f'{key}.json'
        journal.parent.mkdir(exist_ok=True)
        leftover = self.repo / '.masil-restore-0123456789abcdef'
        leftover.write_text('half written\n')
        journal.write_text(json.dumps({'source': 1, 'before': 2, 'files': [
            {'path': 'a.txt', 'action': 'restore', 'temporary': leftover.name}]}))
        self.assertEqual(self.cp('list')['interrupted_restore']['source'], 1)
        self.assertTrue(leftover.exists())
        self.cp('make')
        self.assertFalse(leftover.exists())
        # The record stays to be read, marked cleaned.
        self.assertIn('cleaned_ms', self.cp('list')['interrupted_restore'])


class RestoreRaces(Restore):
    """Cases from the restore review."""

    def config(self, text):
        path = Path(self.env['XDG_CONFIG_HOME']) / 'masil/checkpoints.toml'
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
        path.chmod(0o600)

    def test_a_write_during_the_restore_is_never_lost(self):
        self.config('max_file_mb = 1000\nmax_total_mb = 2000\n')
        big = self.repo / 'big.bin'
        big.write_bytes(b'a' * (150 * 1024 * 1024))
        self.cp('make')
        big.write_bytes(b'b' * (150 * 1024 * 1024))
        preview = self.cp('restore', '1', 'big.bin')
        running = subprocess.Popen([str(AGENT), 'agent', '--socket', str(self.server.socket), 'checkpoint', 'cx',
                                    'restore', '1', 'big.bin', '--confirm', preview['token']],
                                   env=self.env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        wait_for(lambda: any(p.name.startswith('.masil-restore-') for p in self.repo.iterdir()), 30)
        big.write_text('USER EDIT DURING RESTORE\n')
        out, err = running.communicate(timeout=60)
        self.assertEqual(big.read_text(), 'USER EDIT DURING RESTORE\n')
        self.assertEqual(running.returncode, 5, err)
        self.assertIn('checkpoint_partial', err)
        self.assertEqual(json.loads(out)['files'][0]['result'], 'stale')
        self.assertEqual([p.name for p in self.repo.iterdir() if p.name.startswith('.masil')], [])

    def test_restore_stays_out_of_nested_repositories_and_left_out_directories(self):
        (self.repo / 'vendor').mkdir()
        (self.repo / 'vendor/x.c').write_text('plain\n')
        nested = self.repo / 'lib'
        self.git('init', '-q', str(nested), cwd=self.server.path)
        (nested / 'a.c').write_text('nested\n')
        self.cp('make')
        (self.repo / 'vendor/x.c').write_text('changed\n')
        self.git('init', '-q', str(self.repo / 'vendor'), cwd=self.server.path)
        refused = self.cp('restore', '1', 'vendor/x.c', code=2)
        self.assertIn('repository of its own', refused.stderr)
        subprocess.run(['rm', '-rf', str(nested / '.git')], check=True)
        left_out = self.cp('restore', '1', 'lib/a.c', code=3)
        self.assertIn('checkpoint_incomplete', left_out.stderr)
        self.assertEqual((nested / 'a.c').read_text(), 'nested\n')

    def test_a_name_in_other_letter_case_is_not_deleted(self):
        (self.repo / 'foo.txt').write_text('saved\n')
        self.cp('make')
        os.rename(self.repo / 'foo.txt', self.repo / 'FOO.txt')
        refused = self.cp('restore', '1', 'FOO.txt', code=3)
        self.assertIn('only in letter case', refused.stderr)
        self.assertEqual((self.repo / 'FOO.txt').read_text(), 'saved\n')

    def test_setuid_bits_do_not_carry_over(self):
        (self.repo / 'tool').write_text('one\n')
        self.cp('make')
        (self.repo / 'tool').write_text('two\n')
        (self.repo / 'tool').chmod(0o4755)
        self.restore(1, 'tool')
        self.assertEqual(oct((self.repo / 'tool').stat().st_mode & 0o7777), '0o644')


class CheckpointFixes(Restore):
    """Cases from the checkpoint review."""

    def test_a_directory_replaced_by_a_symlink_does_not_stop_checkpoints(self):
        (self.repo / 'vendor').mkdir()
        (self.repo / 'vendor/x.js').write_text('x\n')
        self.git('add', 'vendor/x.js')
        self.git('commit', '-q', '-m', 'vendor')
        copy = self.server.path / 'vendor-copy'
        os.rename(self.repo / 'vendor', copy)
        (self.repo / 'vendor').symlink_to(copy)
        made = self.cp('make')
        self.assertIn({'path': 'vendor/x.js', 'why': 'beyond_symlink'}, made['excluded'])

    def test_show_tells_a_file_now_too_large_from_a_deleted_one(self):
        (self.repo / 'data.bin').write_bytes(b'small')
        self.cp('make')
        (self.repo / 'data.bin').write_bytes(b'0' * (11 * 1024 * 1024))
        (self.repo / 'keep.txt').unlink()
        kinds = {f['path']: f['kind'] for f in self.cp('show', '1')['since']['files']}
        self.assertEqual(kinds, {'data.bin': 'excluded_now', 'keep.txt': 'deleted'})
        bad = self.cp('show', '1', '--file', '../x', code=2)
        self.assertIn('invalid_argument', bad.stderr)

    def test_a_blank_reason_keeps_the_metadata(self):
        self.cp('make', '--reason', '  \n  ')
        (entry,) = self.cp('list')['checkpoints']
        self.assertEqual((entry['reason'].strip(), entry['auto']), ('', False))
        self.assertIn('head', entry)

    def test_many_first_uses_at_once_all_make_their_checkpoint(self):
        runs = [subprocess.Popen([str(AGENT), 'checkpoint-auto', str(self.repo), 'cx', f'r{i}', '0'],
                                 env=self.env) for i in range(6)]
        for run in runs:
            run.wait(timeout=60)
        listed = self.cp('list')
        self.assertEqual(len(listed['checkpoints']) + len(listed['skipped']), 6, listed)

    def test_retention_survives_a_stale_packed_refs_lock(self):
        for i in range(3):
            self.cp('make')
        (store,) = (self.home / '.local/state/masil/checkpoints').glob('*.git')
        (store / 'packed-refs.lock').write_text('')
        self.cp('make')
        self.assertFalse((store / 'packed-refs.lock').exists())


for cls, parent in ((Restore, test_managed_agents.ManagedAgents), (RestoreRaces, Restore),
                    (CheckpointFixes, Restore)):
    for name in dir(parent):
        if name.startswith('test_') and name not in cls.__dict__:
            setattr(cls, name, None)
del cls, parent


if __name__ == '__main__':
    unittest.main(verbosity=2)
