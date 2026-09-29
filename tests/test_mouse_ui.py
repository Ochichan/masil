#!/usr/bin/env python3
"""rmux UI layer, status column and settings screen through real PTYs."""
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time
import unittest

from test_compatibility import RMUX
from test_ui import Terminal

AGENT = Path(__file__).resolve().parents[1] / 'bin/rmux-agent'


def wait_until(predicate, timeout=4, step=.05):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        if predicate():
            return True
        time.sleep(step)
    return predicate()


class Desk(unittest.TestCase):
    """One isolated rmux server per test; HOME has no tmux configuration."""

    def setUp(self):
        self.path = Path(tempfile.mkdtemp(prefix='rmux-mouse-ui-'))
        self.home = self.path / 'home'
        self.home.mkdir()
        self.socket = self.path / 'socket'
        self.env = {'HOME': str(self.home), 'PATH': '/usr/bin:/bin',
                    'TERM': 'xterm-256color', 'SHELL': '/bin/sh', 'PS1': '$ ',
                    'LANG': 'en_US.UTF-8'}
        self.addCleanup(shutil.rmtree, self.path, ignore_errors=True)
        self.addCleanup(lambda: self.rmux('kill-server', check=False))

    def rmux(self, *args, check=True):
        result = subprocess.run([str(RMUX), '-S', str(self.socket), *args], env=self.env,
                                capture_output=True, text=True, timeout=10)
        if check and result.returncode:
            self.fail(f'rmux {args}: {result.stderr}')
        return result.stdout.strip()

    def settings(self, *args):
        return subprocess.run([str(AGENT), 'settings', '--socket', str(self.socket), *args],
                              env=self.env, capture_output=True, text=True, timeout=20)

    def attach(self, *args, width=100, height=24):
        ui = Terminal([RMUX, '-S', self.socket, *args, 'new-session', '-s', 'work'],
                      self.env, width, height)
        self.addCleanup(ui.close)
        self.assertTrue(wait_until(lambda: self.rmux('list-sessions', check=False) != ''))
        ui.drain(.4)
        return ui

    def settle(self, ui):
        # The server defers redraws until earlier output reaches the terminal.
        ui.drain(.3)
        ui.drain(.2)

    def click(self, ui, x, y, button=0):
        ui.mouse(x, y, code=button)
        ui.drain(.1)
        ui.mouse(x, y, code=button, release=True)
        self.settle(ui)

    def windows(self):
        return self.rmux('list-windows', '-F', '#{window_index}').split()


class Layer(Desk):
    def test_layer_loads_without_f_and_user_config_wins(self):
        self.attach()
        self.assertEqual(self.rmux('show', '-gv', 'pane-border-status'), 'top')
        self.assertEqual(self.rmux('show', '-gv', '@rmux-menu-stay-open'), 'on')
        self.assertTrue(self.rmux('show', '-gv', '@rmux-agent').endswith('rmux-agent'))
        notes = self.rmux('list-keys', '-N', '-T', 'root')
        self.assertEqual(notes.count('rmux-ui:'), 9)
        self.assertEqual(self.rmux('list-keys', '-N', '-T', 'prefix').count('rmux-ui:'), 2)
        self.rmux('kill-server')

        (self.home / '.tmux.conf').write_text('set -g pane-border-status off\n')
        self.attach()
        self.assertEqual(self.rmux('show', '-gv', 'pane-border-status'), 'off')

    def test_f_skips_the_layer(self):
        self.attach('-f', '/dev/null')
        self.assertEqual(self.rmux('show', '-gv', 'pane-border-status'), 'off')
        self.assertEqual(self.rmux('show', '-gqv', '@rmux-agent'), '')
        self.assertNotIn('rmux-ui:', self.rmux('list-keys', '-N', '-T', 'root'))

    def test_plus_button_and_menu_stay_open(self):
        ui = self.attach()
        self.settle(ui)
        bar = ui.screen.display[-1]
        self.assertIn(' + ', bar)
        # + offers a new window, a floating pane or a split.
        self.click(ui, bar.index(' + ') + 1, ui.screen.lines - 1)
        x, y = ui.locate('Window')
        ui.mouse(x, y, code=35)
        ui.drain(.1)
        self.click(ui, x, y)
        self.assertTrue(wait_until(lambda: len(self.windows()) == 2), self.windows())

        # The rmux button opens a menu that stays open after the click.
        self.click(ui, 2, ui.screen.lines - 1)
        self.assertIn('Settings', ui.text)
        self.assertIn('Sessions and windows', ui.text)
        ui.send('\x1b')

    def test_saved_choices_load_before_the_layer(self):
        config = self.home / '.config/rmux'
        config.mkdir(parents=True)
        (config / 'settings.conf').write_text('set -g @rmux-status-position left\n'
                                              'set -g @rmux-lang ko\n')
        ui = self.attach()
        self.assertEqual(self.rmux('show', '-gv', 'status-position'), 'left')
        self.settle(ui)
        self.assertIn('새 창', ui.text)


class Column(Desk):
    def test_left_and_right_columns_size_draw_and_click(self):
        ui = self.attach('-f', '/dev/null', width=100, height=24)
        self.rmux('new-window')
        self.rmux('set', '-g', 'status-position', 'left')
        self.settle(ui)
        self.assertEqual(self.rmux('display', '-p', '#{window_width}x#{window_height}'), '76x24')
        rows = ui.screen.display
        self.assertTrue(rows[2].startswith('0:'), rows[2])
        self.assertTrue(rows[3].startswith('1:'), rows[3])
        self.click(ui, 3, 2)
        self.assertEqual(self.rmux('display', '-p', '#{window_index}'), '0')

        # A right-click on a window's row opens the window menu beside it.
        # Wait out the double-click interval so it is a fresh click.
        self.rmux('set', '-g', '@rmux-menu-stay-open', 'on')
        time.sleep(.6)
        self.click(ui, 3, 2, button=2)
        self.assertIn('New After', ui.text)
        ui.send('\x1b')
        self.settle(ui)

        self.rmux('set', '-g', 'status-position', 'right')
        self.settle(ui)
        self.assertTrue(ui.screen.display[3][76:].startswith('1:'), ui.screen.display[3])
        self.click(ui, 80, 3)
        self.assertEqual(self.rmux('display', '-p', '#{window_index}'), '1')

        # Too narrow for a usable window: the column is not drawn.
        ui.resize(30, 20)
        self.settle(ui)
        self.assertEqual(self.rmux('display', '-p', '#{window_width}'), '30')

        self.rmux('set', '-g', 'status-position', 'bottom')
        ui.resize(100, 24)
        self.settle(ui)
        self.assertEqual(self.rmux('display', '-p', '#{window_width}x#{window_height}'), '100x23')

    def test_column_keeps_pane_mouse_coordinates(self):
        ui = self.attach('-f', '/dev/null', width=100, height=24)
        self.rmux('set', '-g', 'status-position', 'left')
        self.rmux('split-window', '-h')
        self.settle(ui)
        left = self.rmux('display', '-p', '-t', '{left}', '#{pane_width}')
        border = 24 + int(left)
        # Drag the border between the panes five cells to the right.
        ui.mouse(border, 10)
        ui.mouse(border + 5, 10, code=32)
        ui.mouse(border + 5, 10, release=True)
        self.settle(ui)
        self.assertEqual(self.rmux('display', '-p', '-t', '{left}', '#{pane_width}'),
                         str(int(left) + 5))

        # A click in the right pane selects it, not the pane a column away.
        self.click(ui, border + 10, 5)
        self.assertEqual(self.rmux('display', '-p', '#{pane_at_right}'), '1')
        self.click(ui, 30, 5)
        self.assertEqual(self.rmux('display', '-p', '#{pane_at_left}'), '1')

    def test_message_and_prompt_overlay_the_last_row(self):
        ui = self.attach('-f', '/dev/null', width=100, height=24)
        self.rmux('set', '-g', 'status-position', 'left')
        self.rmux('display-message', '-d', '2000', 'hello-overlay')
        self.settle(ui)
        self.assertTrue(ui.screen.display[-1].startswith('hello-overlay'), ui.screen.display[-1])
        self.assertTrue(ui.screen.display[0].startswith('[work]'), ui.screen.display[0])


class ColumnReview(Desk):
    """Regressions found in review of the status column."""

    def test_height_changes_and_overlays_keep_the_server_alive(self):
        ui = self.attach('-f', '/dev/null', width=100, height=24)
        for _ in range(6):
            self.rmux('new-window')
        for position in ('left', 'right'):
            self.rmux('set', '-g', 'status-position', position)
            for height in (30, 18, 40, 9, 24):
                ui.resize(100, height)
                self.settle(ui)
            self.rmux('display-message', '-d', '0', 'overlay')
            for height in (40, 20, 30):
                ui.resize(100, height)
                self.settle(ui)
            self.rmux('display-message', '-d', '1', 'clear')
        self.assertEqual(len(self.windows()), 7)

    def test_drag_in_the_column_does_not_select_pane_text(self):
        ui = self.attach('-f', '/dev/null', width=100, height=24)
        self.rmux('set', '-g', 'status-position', 'left')
        self.rmux('send-keys', 'printf "LINE-A\\nLINE-B\\n"', 'Enter')
        self.settle(ui)
        ui.mouse(3, 2)
        for y in (3, 4, 5, 6):
            ui.mouse(3, y, code=32)
        ui.mouse(3, 6, release=True)
        self.settle(ui)
        self.assertEqual(self.rmux('list-buffers'), '')

    def test_column_rows_follow_window_style_changes(self):
        ui = self.attach('-f', '/dev/null', width=100, height=24)
        self.rmux('set', '-g', 'status-position', 'left')
        self.settle(ui)
        before = ui.screen.buffer[2][1].bg
        self.rmux('set', '-g', 'window-status-current-style', 'bg=blue')
        self.rmux('refresh-client', '-S')
        self.settle(ui)
        self.assertNotEqual(ui.screen.buffer[2][1].bg, before)
        self.assertEqual(ui.screen.buffer[2][1].bg, 'blue')


class MenuOverlay(Desk):
    def test_removed_menu_target_does_not_fall_back_to_current_window(self):
        ui = self.attach(width=120, height=32)
        self.rmux('rename-window', 'first')
        self.rmux('new-window', '-n', 'second')
        self.settle(ui)
        x, y = ui.locate('0 first')
        self.click(ui, x + 2, y, button=2)
        self.assertIn('New After', ui.text)
        self.rmux('kill-window', '-t', 'work:0')
        ui.send('X')
        self.settle(ui)
        self.assertEqual(self.rmux('list-windows', '-F', '#W'), 'second')

    def test_inactive_window_menu_is_visible_and_keeps_its_target(self):
        ui = self.attach(width=120, height=32)
        self.rmux('rename-window', 'first')
        self.rmux('new-window', '-n', 'second')
        for position in ('bottom', 'top', 'left', 'right'):
            with self.subTest(position=position):
                self.rmux('set', '-g', 'status-position', position)
                self.rmux('select-window', '-t', 'work:1')
                self.settle(ui)
                x, y = ui.locate('0 first')
                self.click(ui, x + 2, y, button=2)
                self.assertIn('New After', ui.text)
                self.assertEqual(self.rmux('display', '-p', '#{window_index}'), '1')
                ui.send('n')
                self.settle(ui)
                ui.send('\x15renamed\r')
                self.settle(ui)
                self.assertEqual(self.rmux('display', '-p', '-t', 'work:0', '#W'), 'renamed')
                self.assertEqual(self.rmux('display', '-p', '-t', 'work:1', '#W'), 'second')
                self.rmux('rename-window', '-t', 'work:0', 'first')

    def test_pane_output_cannot_paint_over_a_context_menu(self):
        ui = self.attach(width=100, height=32)
        self.settle(ui)
        self.click(ui, 40, 12, button=2)
        self.assertIn('Split', ui.text)
        menu_row = ui.locate('Split')[1]
        self.rmux('send-keys', 'while :; do printf "BACKGROUND\\n"; sleep 0.02; done', 'Enter')
        ui.drain(2.2)
        self.assertIn('Split', ui.text)
        self.assertEqual(ui.locate('Split')[1], menu_row)
        ui.send('\x1b')
        self.settle(ui)
        self.assertNotIn('Split', ui.text)

    def test_floating_pane_output_cannot_paint_over_a_context_menu(self):
        ui = self.attach(width=100, height=32)
        pane = self.rmux('new-pane', '-dPF', '#{pane_id}',
                         '-x', '60', '-y', '20', '-X', '15', '-Y', '5')
        self.settle(ui)
        self.click(ui, 40, 12, button=2)
        self.assertIn('Move & Resize', ui.text)
        menu_row = ui.locate('Move & Resize')[1]
        self.rmux('send-keys', '-t', pane,
                  'while :; do printf "FLOATING\\n"; sleep 0.02; done', 'Enter')
        ui.drain(2.1)
        self.assertEqual(ui.locate('Move & Resize')[1], menu_row)
        ui.send('\x1b')
        self.settle(ui)
        self.assertNotIn('Move & Resize', ui.text)


class FloatingGroups(Desk):
    """The + menus make floating panes and splits; a split float is a group."""

    def pick(self, ui, text):
        x, y = ui.locate(text)
        # An open menu picks the item under the last pointer move.
        ui.mouse(x + 1, y, code=35)
        ui.drain(.1)
        self.click(ui, x + 1, y)

    def drag(self, ui, x0, y0, x1, y1):
        ui.mouse(x0, y0, code=0)
        ui.drain(.05)
        steps = max(abs(x1 - x0), abs(y1 - y0))
        for i in range(1, steps + 1):
            ui.mouse(x0 + (x1 - x0) * i // steps, y0 + (y1 - y0) * i // steps, code=32)
            ui.drain(.02)
        ui.mouse(x1, y1, code=0, release=True)
        self.settle(ui)

    def group(self):
        import json
        layout = json.loads(self.rmux('display', '-p', '#{window_layout}'))['L']
        return next(c for c in layout.get('c', []) if 'z' in c and 'c' in c)

    def test_plus_menus_make_a_floating_group_that_moves_and_resizes_as_one(self):
        ui = self.attach(width=100, height=30)
        bar = ui.screen.display[-1]
        self.click(ui, bar.index(' + ') + 1, ui.screen.lines - 1)
        self.pick(ui, 'Floating pane')
        self.assertTrue(wait_until(lambda: self.rmux('display', '-p', '#{pane_floating_flag}') == '1'))
        self.settle(ui)

        # The floating pane's own + opens the split menu.
        row, line = next((y, l) for y, l in enumerate(ui.screen.display) if '┌' in l and ' + ' in l)
        self.click(ui, line.index(' + ') + 1, row)
        self.pick(ui, 'Split right')
        self.assertTrue(wait_until(lambda: len(self.rmux('list-panes').splitlines()) == 3))
        self.settle(ui)
        group = self.group()
        self.assertEqual(len(group['c']), 2)
        top = group['y'] - 1
        self.assertIn('┬', ui.screen.display[top], 'separator joins the top border')
        self.assertIn('┴', ui.screen.display[group['y'] + group['h']])

        # The top border moves the whole group.
        line = ui.screen.display[top]
        x = line.index('───', line.index('2 bash') + 6) + 1
        self.drag(ui, x, top, x + 6, top + 3)
        moved = self.group()
        self.assertEqual((moved['x'], moved['y']), (group['x'] + 6, group['y'] + 3))
        self.assertEqual([c['x'] - moved['x'] for c in moved['c']],
                         [c['x'] - group['x'] for c in group['c']])

        # The separator between the panes resizes them inside the group.
        separator = moved['c'][0]['x'] + moved['c'][0]['w']
        self.drag(ui, separator, moved['y'] + 3, separator - 5, moved['y'] + 3)
        resized = self.group()
        self.assertEqual(resized['w'], moved['w'])
        self.assertEqual(resized['c'][0]['w'], moved['c'][0]['w'] - 5)

        # The bottom-right corner resizes the group; its panes follow.
        right, bottom = resized['x'] + resized['w'], resized['y'] + resized['h']
        self.drag(ui, right, bottom, right + 8, bottom + 2)
        grown = self.group()
        self.assertEqual((grown['w'], grown['h']), (resized['w'] + 8, resized['h'] + 2))
        self.assertEqual(sum(c['w'] for c in grown['c']) + 1, grown['w'])

    def test_outer_border_drag_follows_the_pointer_past_the_minimum(self):
        ui = self.attach(width=100, height=30)
        self.rmux('new-pane', '-x', '42', '-y', '12', '-X', '10', '-Y', '5')
        self.rmux('split-window', '-G', '-h')
        self.settle(ui)
        group = self.group()
        right, y = group['x'] + group['w'], group['y'] + 4
        points = [(x, y) for x in range(right - 1, group['x'] + 1, -1)]
        points += [(x, y) for x in range(group['x'] + 2, right + 7)]
        ui.mouse(right, y, code=0)
        ui.drain(.05)
        for x, py in points:
            ui.mouse(x, py, code=32)
            ui.drain(.02)
        ui.mouse(right + 6, y, code=0, release=True)
        self.settle(ui)
        self.assertEqual(self.group()['w'], group['w'] + 6)

    def test_rmux_menu_picks_a_layout(self):
        ui = self.attach(width=100, height=30)
        self.rmux('split-window', '-h')
        self.rmux('split-window', '-v')
        self.settle(ui)
        self.click(ui, 2, ui.screen.lines - 1)
        self.pick(ui, 'Layout')
        self.pick(ui, 'Stacked')
        self.assertTrue(wait_until(
            lambda: len(set(self.rmux('list-panes', '-F', '#{pane_width}').split())) == 1))


class Sessions(Desk):
    """Sessions are saved and restored from keys, menus and the CLI; a pane
    title drag moves a floating pane."""

    def agent(self, *args):
        return subprocess.run([str(AGENT), 'session', '--socket', str(self.socket), *args],
                              env=self.env, capture_output=True, text=True, timeout=30)

    def snapshots(self):
        return sorted((self.home / '.local/share/rmux/sessions').glob('*.json'))

    def layouts(self):
        import json

        def shape(cell):
            cell = {k: v for k, v in cell.items() if k not in ('I', 'a', 'l')}
            if 'c' in cell:
                cell['c'] = [shape(child) for child in cell['c']]
            return cell
        return {w: shape(json.loads(self.rmux('display', '-p', '-t', w, '#{window_layout}'))['L'])
                for w in self.rmux('list-windows', '-a', '-F', '#{session_name}:#{window_index}').split()}

    def test_save_restart_and_restore_layouts_directories_and_commands(self):
        ui = self.attach(width=120, height=36)
        project = self.path / 'project'
        project.mkdir()
        (project / 'notes.txt').write_text('hello\n')
        self.rmux('send-keys', f'cd {project} && vim notes.txt', 'Enter')
        self.rmux('split-window', '-h', '-c', str(project))
        self.rmux('send-keys', 'sleep 1000', 'Enter')
        self.rmux('new-pane', '-x', '50', '-y', '12', '-X', '20', '-Y', '6', '-c', '/tmp')
        self.rmux('split-window', '-G', '-h', '-c', '/usr')
        self.rmux('new-window', '-n', 'logs', '-c', '/var')
        self.rmux('new-session', '-d', '-s', 'second', '-c', '/tmp')
        self.assertTrue(wait_until(lambda: 'vim' in self.rmux('list-panes', '-a', '-F', '#{pane_current_command}')))
        before = self.layouts()
        result = self.agent('save')
        self.assertEqual(result.returncode, 0, result.stderr)
        snapshot = self.snapshots()[0]
        self.assertEqual(snapshot.stat().st_mode & 0o777, 0o600)
        self.assertEqual(snapshot.parent.stat().st_mode & 0o777, 0o700)
        # Nothing changed, so an automatic save keeps the latest snapshot.
        self.assertIn('"stage":"unchanged"', self.agent('save', '--auto').stdout)
        self.assertEqual(len(self.snapshots()), 1)

        pid = self.rmux('display', '-p', '#{pid}')
        self.rmux('kill-server')
        # Like tmux, a server with several panes can take ten seconds to exit.
        self.assertTrue(wait_until(
            lambda: subprocess.run(['kill', '-0', pid], capture_output=True).returncode != 0, 15))
        ui = self.attach(width=120, height=36)
        client = self.rmux('list-clients', '-F', '#{client_name}').split()[0]
        result = self.agent('--client', client, 'restore')
        self.assertEqual(result.returncode, 0, result.stderr)
        # The just-started session with the saved name is replaced.
        self.assertEqual(sorted(self.rmux('list-sessions', '-F', '#{session_name}').split()),
                         ['second', 'work'])
        self.assertEqual(self.rmux('list-clients', '-F', '#{client_session}'), 'work')
        self.assertEqual(self.layouts(), before)
        panes = self.rmux('list-panes', '-a', '-F', '#{pane_current_path} #{pane_floating_flag}')
        self.assertIn(f'{project.resolve()} 0', panes)
        self.assertIn('/usr 1', panes)
        # vim is on the restore list; sleep is not.
        self.assertTrue(wait_until(lambda: 'vim' in self.rmux('list-panes', '-a', '-F', '#{pane_current_command}')))
        self.assertNotIn('sleep', self.rmux('list-panes', '-a', '-F', '#{pane_current_command}'))

    def test_restore_never_replaces_a_session_in_use(self):
        self.attach(width=100, height=30)
        self.rmux('send-keys', 'seq 1 20', 'Enter')
        self.assertTrue(wait_until(lambda: int(self.rmux('display', '-p', '#{cursor_y}')) > 3))
        self.rmux('new-session', '-d', '-s', 'notes', 'sleep 1000')
        self.assertEqual(self.agent('save').returncode, 0)
        result = self.agent('restore')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('"skipped":["notes","work"]', result.stdout.replace(' ', ''))
        self.assertEqual(sorted(self.rmux('list-sessions', '-F', '#{session_name}').split()),
                         ['notes', 'work'])
        self.assertIn('sleep', self.rmux('list-panes', '-t', '=notes:', '-F', '#{pane_current_command}'))

    def test_restore_keeps_a_new_shell_that_has_jobs(self):
        self.attach(width=100, height=30)
        pane = self.rmux('display', '-p', '#{pane_pid}')
        self.rmux('send-keys', 'sleep 1000 &', 'Enter')
        job = lambda: subprocess.run(['pgrep', '-P', pane, 'sleep'], capture_output=True, text=True).stdout
        self.assertTrue(wait_until(job))
        self.assertEqual(self.agent('save').returncode, 0)
        client = self.rmux('list-clients', '-F', '#{client_name}').split()[0]
        self.assertEqual(self.agent('--client', client, 'restore').returncode, 0)
        time.sleep(.5)
        self.assertEqual(self.rmux('list-sessions', '-F', '#{session_name}'), 'work')
        self.assertTrue(job())

    def test_hashes_in_names_come_back(self):
        self.attach(width=100, height=30)
        self.rmux('new-session', '-d', '-s', 'lab##1', '-n', 'x####y')
        self.rmux('select-pane', '-t', '=lab#1:', '-T', 'a##b')
        names = lambda: (self.rmux('list-windows', '-t', '=lab#1', '-F', '#{window_name}'),
                         self.rmux('display', '-p', '-t', '=lab#1:', '#{pane_title}'))
        self.assertEqual(names(), ('x##y', 'a#b'))
        self.assertEqual(self.agent('save').returncode, 0)
        self.rmux('kill-session', '-t', '=lab#1')
        result = self.agent('restore')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('lab#1', result.stdout)
        self.assertEqual(names(), ('x##y', 'a#b'))

    def test_restore_types_no_control_characters(self):
        self.attach(width=100, height=30)
        mark = self.path / 'PWNED'
        self.rmux('new-session', '-d', '-s', 'inject')
        self.rmux('send-keys', '-t', '=inject:', f"tail -F \"$(printf 'log\\025touch {mark}\\r')\"", 'Enter')
        self.assertTrue(wait_until(lambda: 'tail' in self.rmux(
            'list-panes', '-t', '=inject:', '-F', '#{pane_current_command}')))
        self.rmux('rename-window', '-t', '=inject:', 'build\\;')
        self.assertEqual(self.rmux('list-windows', '-t', '=inject', '-F', '#{window_name}'), 'build;')
        self.assertEqual(self.agent('save').returncode, 0)
        self.rmux('kill-session', '-t', '=inject')
        result = self.agent('restore')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('"commands":0', result.stdout)
        time.sleep(1)
        self.assertFalse(mark.exists())
        self.assertEqual(self.rmux('list-windows', '-t', '=inject', '-F', '#{window_name}'), 'build;')

    def test_grouped_sessions_run_a_command_once(self):
        self.attach(width=100, height=30)
        self.rmux('new-session', '-d', '-s', 'code')
        self.rmux('send-keys', '-t', '=code:', 'tail -f /dev/null', 'Enter')
        self.assertTrue(wait_until(lambda: 'tail' in self.rmux(
            'list-panes', '-t', '=code:', '-F', '#{pane_current_command}')))
        self.rmux('new-session', '-d', '-t', 'code', '-s', 'view')
        self.assertEqual(self.agent('save').returncode, 0)
        self.rmux('kill-session', '-t', '=view')
        self.rmux('kill-session', '-t', '=code')
        result = self.agent('restore')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('"commands":1', result.stdout)
        self.assertTrue(wait_until(lambda: 'tail' in self.rmux(
            'list-panes', '-t', '=code:', '-F', '#{pane_current_command}')))
        self.assertNotIn('tail', self.rmux('list-panes', '-t', '=view:', '-F', '#{pane_current_command}'))

    def test_a_bad_layout_falls_back_to_tiles(self):
        import json
        self.attach(width=100, height=30)
        self.rmux('new-session', '-d', '-s', 'grid', '-x', '100', '-y', '30')
        self.rmux('split-window', '-t', '=grid:')
        self.rmux('split-window', '-h', '-t', '=grid:')
        self.assertEqual(self.agent('save').returncode, 0)
        path = self.snapshots()[-1]
        data = json.loads(path.read_text())
        for session in data['sessions']:
            for window in session['windows']:
                window['layout'] = 'bogus'
        path.write_text(json.dumps(data))
        self.rmux('kill-session', '-t', '=grid')
        result = self.agent('restore')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('"layout_fallbacks":2', result.stdout)
        flags = self.rmux('list-panes', '-t', '=grid:', '-F', '#{pane_floating_flag}').split()
        self.assertEqual(flags, ['0', '0', '0'])

    def test_layer_keys_leave_user_bindings_and_go_with_the_layer(self):
        (self.home / '.tmux.conf').write_text('bind C-s display-message mine\n')
        self.attach()
        self.assertIn('mine', self.rmux('list-keys', '-T', 'prefix', 'C-s'))
        self.assertIn('session menu', self.rmux('list-keys', '-T', 'prefix', 'C-r'))
        self.settings('--set', '@rmux-lang', 'ko')
        self.assertIn('mine', self.rmux('list-keys', '-T', 'prefix', 'C-s'))
        self.settings('--layer', 'off')
        self.assertNotIn('session', self.rmux('list-keys', '-T', 'prefix', 'C-r', check=False))

    def test_menu_errors_stay_out_of_the_pane(self):
        ui = self.attach(width=100, height=30)
        self.rmux('run-shell', '-b', f"'{AGENT}' session --socket '{self.socket}' "
                  "--client /dev/ttys999 restore 20000101-000000-manual")
        time.sleep(1)
        self.assertEqual(self.rmux('display', '-p', '#{pane_in_mode}'), '0')

    def test_keys_and_menus_save_and_restore(self):
        ui = self.attach(width=100, height=30)
        ui.send('\x02\x13')
        self.assertTrue(wait_until(lambda: len(self.snapshots()) == 1, 6))
        ui.send('\x02\x12')
        self.assertTrue(wait_until(lambda: 'Restore sessions' in ui.text, 6), ui.text)
        self.assertIn('1 session, 1 pane', ui.text)
        ui.send('\x1b')
        self.settle(ui)
        # The rmux menu offers the same.
        self.click(ui, 2, ui.screen.lines - 1)
        self.assertIn('Save sessions', ui.text)
        self.assertIn('Restore sessions', ui.text)
        ui.send('\x1b')

    def test_one_autosaver_per_server(self):
        self.attach()
        self.rmux('source-file', '-q', '/dev/null')
        count = lambda: subprocess.run(['pgrep', '-f', f'session autosave --socket {self.socket}'],
                                       capture_output=True, text=True).stdout.split()
        self.assertTrue(wait_until(lambda: len(count()) == 1))
        # Running the layer's keys again does not start a second saver.
        self.rmux('run-shell', f"'{AGENT}' session autosave --socket '{self.socket}'")
        time.sleep(.5)
        self.assertEqual(len(count()), 1)

    def test_title_drag_moves_a_floating_pane_and_a_click_renames(self):
        import json
        ui = self.attach(width=100, height=30)
        self.rmux('new-pane', '-x', '40', '-y', '10', '-X', '10', '-Y', '5')
        self.settle(ui)
        cell = next(c for c in json.loads(self.rmux('display', '-p', '#{window_layout}'))['L']['c'] if 'z' in c)
        y = cell['y'] - 1
        x = ui.screen.display[y].index(' 1 ') + 3
        ui.mouse(x, y, code=0)
        ui.drain(.1)
        for i in range(1, 7):
            ui.mouse(x + i, y + i // 2, code=32)
            ui.drain(.05)
        ui.mouse(x + 6, y + 3, code=0, release=True)
        self.settle(ui)
        moved = next(c for c in json.loads(self.rmux('display', '-p', '#{window_layout}'))['L']['c'] if 'z' in c)
        self.assertEqual((moved['x'], moved['y']), (cell['x'] + 6, cell['y'] + 3))
        self.click(ui, moved['x'] + 3, moved['y'] - 1)
        self.assertIn('Pane name:', ui.screen.display[-1])
        ui.send('\x1b')


class Settings(Desk):
    def test_cli_applies_reads_back_and_turns_the_layer_off(self):
        self.attach()
        result = self.settings('--set', '@rmux-status-position', 'right',
                               '--set', '@rmux-theme', 'light')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn('Applied', result.stdout)
        self.assertEqual(self.rmux('show', '-gv', 'status-position'), 'right')
        saved = self.home / '.config/rmux/settings.conf'
        self.assertIn('set -g @rmux-status-position right', saved.read_text())
        self.assertEqual(saved.stat().st_mode & 0o777, 0o600)

        self.assertEqual(self.settings('--set', '@rmux-status-position', 'up').returncode, 1)

        (self.home / '.tmux.conf').write_text('set -g history-limit 4321\n')
        self.assertEqual(self.settings('--layer', 'off').returncode, 0)
        self.assertEqual(self.rmux('show', '-gv', 'status-position'), 'bottom')
        self.assertEqual(self.rmux('show', '-gv', 'pane-border-status'), 'off')
        self.assertEqual(self.rmux('show', '-gv', 'history-limit'), '4321')
        self.assertNotIn('rmux-ui:', self.rmux('list-keys', '-N', '-T', 'root'))

        self.assertEqual(self.settings('--layer', 'on').returncode, 0)
        self.assertEqual(self.rmux('show', '-gv', 'status-position'), 'right')
        self.assertEqual(self.settings('--reset').returncode, 0)
        self.assertEqual(self.rmux('show', '-gv', 'status-position'), 'bottom')
        self.assertNotIn('@rmux-', saved.read_text())

    def test_choices_keep_values_from_the_users_tmux_conf(self):
        (self.home / '.tmux.conf').write_text(
            'set -g base-index 1\nset -g status-style bg=red\n')
        self.attach()
        result = self.settings('--set', '@rmux-history', '50000',
                               '--set', '@rmux-theme', 'light')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.rmux('show', '-gv', 'history-limit'), '50000')
        self.assertEqual(self.rmux('show', '-gv', 'base-index'), '1')
        self.assertEqual(self.rmux('show', '-gv', 'status-style'), 'bg=red')
        self.assertEqual(self.rmux('show', '-gv', 'menu-style'), 'bg=#fafbfb,fg=#192229')
        self.assertEqual(self.settings('--reset').returncode, 0)
        self.assertEqual(self.rmux('show', '-gv', 'base-index'), '1')

    def test_leaving_the_tmux_theme_restores_rmux_styles(self):
        (self.home / '.tmux.conf').write_text('set -g message-style bg=red\n')
        self.attach()
        dark = self.rmux('show', '-gv', 'status-style')
        self.assertEqual(self.settings('--set', '@rmux-theme', 'tmux').returncode, 0)
        self.assertNotEqual(self.rmux('show', '-gv', 'status-style'), dark)
        self.assertEqual(self.settings('--set', '@rmux-theme', 'dark').returncode, 0)
        self.assertEqual(self.rmux('show', '-gv', 'status-style'), dark)
        self.assertEqual(self.rmux('show', '-gv', 'message-style'), 'bg=red')

    def test_unsaved_change_while_off_is_a_failure(self):
        self.attach()
        self.assertEqual(self.settings('--layer', 'off').returncode, 0)
        saved = self.home / '.config/rmux/settings.conf'
        saved.unlink()
        saved.symlink_to(self.path / 'elsewhere.conf')
        result = self.settings('--set', '@rmux-status-position', 'left')
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn('Failed', result.stdout)

    def test_title_click_renames_pane_one(self):
        ui = self.attach(width=100, height=24)
        self.rmux('split-window', '-h')
        self.settle(ui)
        panes = self.rmux('list-panes', '-F', '#{pane_id} #{pane_left}').splitlines()
        right = next(p.split()[0] for p in panes if p.split()[1] != '0')
        self.assertEqual(right, '%1')
        left = int(self.rmux('display', '-p', '-t', '{left}', '#{pane_width}'))
        self.rmux('select-pane', '-t', '{left}')
        self.click(ui, left + 4, 0)
        ui.send('\x15renamed-one\r')
        self.settle(ui)
        self.assertTrue(wait_until(
            lambda: self.rmux('display', '-p', '-t', '%1', '#{pane_title}') == 'renamed-one'),
            self.rmux('show-messages'))

    def test_menu_settings_item_with_a_space_in_the_path(self):
        spaced = self.path / 'with space'
        spaced.mkdir()
        for name in ('rmux', 'rmux-agent'):
            shutil.copy2(RMUX.parent / name, spaced / name)
        ui = Terminal([spaced / 'rmux', '-S', self.socket, 'new-session', '-s', 'work'],
                      self.env, 120, 32)
        self.addCleanup(ui.close)
        self.assertTrue(wait_until(lambda: self.rmux('list-sessions', check=False) != ''))
        self.settle(ui)
        self.click(ui, 2, ui.screen.lines - 1)
        x, y = ui.locate('Settings')
        # An open menu picks the item under the last pointer move.
        ui.mouse(x, y, code=35)
        ui.drain(.1)
        self.click(ui, x, y)
        self.assertTrue(wait_until(lambda: 'Left sidebar' in ui.text, 6), ui.text)

    def test_settings_screen_applies_a_click(self):
        ui = self.attach(width=120, height=32)
        self.settle(ui)
        ui.click('Settings')
        self.assertTrue(wait_until(lambda: 'Left sidebar' in ui.text, 6), ui.text)
        ui.click(' Left sidebar ')
        self.assertTrue(wait_until(
            lambda: self.rmux('show', '-gv', 'status-position') == 'left', 4))
        self.assertTrue(wait_until(lambda: 'Applied' in ui.text, 4), ui.text)
        ui.send('q')
        self.assertTrue(wait_until(lambda: 'Left sidebar' not in ui.text, 4))


if __name__ == '__main__':
    unittest.main(verbosity=2)
