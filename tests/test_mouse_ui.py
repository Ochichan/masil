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
        self.assertEqual(notes.count('rmux-ui:'), 6)
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
        self.click(ui, bar.index(' + ') + 1, ui.screen.lines - 1)
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

        # A right-click on the current window's row opens the window menu
        # beside it. Wait out the double-click interval so it is a fresh
        # click. (next-3.9 attaches a menu for another window to that
        # window, so it is not shown; stock tmux behaves the same.)
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
