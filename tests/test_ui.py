#!/usr/bin/env python3
"""Real terminal input, backend effects and teardown, in isolated PTYs."""
import codecs
import fcntl
import os
from pathlib import Path
import pty
import select
import signal
import struct
import subprocess
import sys
import termios
import time
import unittest

import pyte

from test_agentd import AGENT, AgentdHarness
from test_compatibility import RMUX, Server, wait_for


class Screen(pyte.Screen):
    def report_device_status(self, mode, **kwargs):
        if not kwargs.get("private"):
            super().report_device_status(mode)


class Terminal:
    def __init__(self, args, env, width=120, height=32, tty_mode=None):
        self.master, self.slave = pty.openpty()
        self.screen = Screen(width, height)
        self.stream = pyte.Stream(self.screen)
        self.decoder = codecs.getincrementaldecoder("utf-8")("replace")
        self.output = bytearray()
        self.resize(width, height, signal_child=False)
        self.original = termios.tcgetattr(self.slave)
        self.tty_mode = tty_mode
        self.original_permissions = os.fstat(self.slave).st_mode & 0o777

        def controlling_terminal():
            os.setsid()
            fcntl.ioctl(0, termios.TIOCSCTTY, 0)
            if tty_mode is not None:
                os.fchmod(0, tty_mode)

        self.child = subprocess.Popen([str(arg) for arg in args], stdin=self.slave,
                                      stdout=self.slave, stderr=self.slave, env=env,
                                      preexec_fn=controlling_terminal)

    def resize(self, width, height, signal_child=True):
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", height, width, 0, 0))
        self.screen.resize(lines=height, columns=width)
        if signal_child and self.child.poll() is None:
            self.child.send_signal(signal.SIGWINCH)

    def drain(self, duration=.08):
        end = time.monotonic() + duration
        total = 0
        while time.monotonic() < end:
            if not select.select([self.master], [], [], max(0, end - time.monotonic()))[0]:
                break
            try:
                data = os.read(self.master, 65536)
            except OSError:
                break
            if not data:
                break
            total += len(data)
            self.output.extend(data)
            self.stream.feed(self.decoder.decode(data))
            # Crossterm/ratatui can query cursor position on initialization.
            if b"\x1b[6n" in data:
                os.write(self.master, b"\x1b[1;1R")
        return total

    @property
    def text(self):
        self.drain()
        return "\n".join(self.screen.display)

    def until(self, text, timeout=5):
        try:
            wait_for(lambda: text in self.text, timeout)
        except AssertionError as error:
            raise AssertionError(f"Expected {text!r}, exit={self.child.poll()}:\n{self.text}") from error

    def send(self, data):
        os.write(self.master, data.encode() if isinstance(data, str) else data)
        self.drain()

    def mouse(self, x, y, code=0, release=False):
        self.send(f"\x1b[<{code};{x + 1};{y + 1}{'m' if release else 'M'}")

    def locate(self, label):
        self.drain()
        for y, line in enumerate(self.screen.display):
            if label in line:
                return line.index(label), y
        raise AssertionError(f"No visible {label!r}:\n{self.text}")

    def click(self, label):
        x, y = self.locate(label)
        self.mouse(x, y)
        self.mouse(x, y, release=True)

    def close(self):
        if self.master is None:
            return
        if self.tty_mode is not None:
            os.fchmod(self.slave, self.original_permissions)
        if self.child.poll() is None:
            self.child.terminate()
        try:
            self.wait()
        except subprocess.TimeoutExpired:
            self.child.kill()
            self.drain(.2)
        os.close(self.master)
        os.close(self.slave)
        self.master = None
        self.child.wait(timeout=4)

    def wait(self, timeout=4):
        end = time.monotonic() + timeout
        while self.child.poll() is None and time.monotonic() < end:
            self.drain(.02)
        if self.child.poll() is None:
            raise subprocess.TimeoutExpired(self.child.args, timeout)


class DeskIntegration(AgentdHarness):
    def desk(self, *args, width=120, height=32):
        terminal = Terminal([AGENT, "--socket", self.manager, "ui", *args],
                            self.core.env | {"XDG_CONFIG_HOME": str(self.core.path / "config")},
                            width, height)
        self.addCleanup(terminal.close)
        terminal.until("agent-0")
        return terminal

    def ready(self):
        self.provider.permissions = [{"id": "per_0", "sessionID": "ses_0"}]
        self.start()
        self.fresh()

    def test_mouse_ack_cancel_and_shared_result_no_provider_write(self):
        self.ready()
        ui = self.desk()
        x, y = ui.locate("Mark seen")
        ui.mouse(x, y)
        ui.mouse(0, 0, code=32)
        ui.mouse(0, 0, release=True)
        self.assertFalse(self.row()["attention"]["acknowledged"])
        ui.click("Mark seen")
        wait_for(lambda: self.row()["attention"]["acknowledged"], 4)
        self.assertTrue(all(method == "GET" for method, _, _ in self.provider.requests))
        ui.send("q")
        ui.wait()
        ui.drain()
        self.assertEqual(ui.child.returncode, 0, ui.output.decode(errors="replace"))
        self.assertEqual(termios.tcgetattr(ui.master), ui.original)
        self.assertIn(b"\x1b[?1006l", ui.output)
        self.assertIn(b"\x1b[?1049l", ui.output)

    def test_keyboard_unicode_resize_idle_and_signal_restore(self):
        self.ready()
        ui = self.desk()
        ui.drain(.3)
        self.assertEqual(ui.drain(.6), 0, "idle UI emits terminal output")
        ui.send("/")
        ui.send("\x1b[200~한글 中文\x1b[201~")
        ui.until("한글 中文")
        ui.send("\x15")
        ui.send("\x1b")
        ui.send("l")
        ui.until("에이전트")
        ui.resize(24, 8)
        ui.drain(.3)
        self.assertIn("28", ui.text)
        ui.resize(80, 24)
        ui.drain(.3)
        ui.child.terminate()
        ui.wait()
        ui.drain()
        self.assertEqual(termios.tcgetattr(ui.master), ui.original)
        self.assertIn(b"\x1b[?2004l", ui.output)

    def test_menu_owns_outside_click_and_disconnect_disables_ack(self):
        self.ready()
        ui = self.desk()
        x, y = ui.locate("agent-0")
        ui.mouse(x, y, code=2)
        ui.drain()
        # Dismissing a context menu on the action row must not acknowledge.
        ax, ay = ui.locate("Mark seen")
        ui.mouse(ax, ui.screen.lines - 4)
        ui.mouse(ax, ui.screen.lines - 4, release=True)
        self.assertFalse(self.row()["attention"]["acknowledged"])
        self.query("stop")
        self.child.communicate(timeout=5)
        ui.until("disconnected")
        self.assertIn("agent-0", ui.text, "last evidence should remain visible")
        ui.send("a")
        ui.send("q")
        ui.wait()
        self.assertEqual(ui.child.returncode, 0)

    @unittest.skipUnless(sys.platform.startswith("linux") and os.geteuid() != 0,
                         "Linux non-root PTY pathname-permission regression")
    def test_inherited_ssh_tty_without_pathname_access(self):
        self.ready()
        ui = Terminal([AGENT, "--socket", self.manager, "ui"], self.core.env, tty_mode=0)
        self.addCleanup(ui.close)
        ui.until("agent-0")
        ui.click("Mark seen")
        wait_for(lambda: self.row()["attention"]["acknowledged"])
        ui.send("q")
        ui.wait()
        self.assertEqual(ui.child.returncode, 0)
        self.assertEqual(termios.tcgetattr(ui.master), ui.original)

    def test_distinct_input_and_output_terminals_are_rejected_before_raw_mode(self):
        first_master, first_slave = pty.openpty()
        second_master, second_slave = pty.openpty()
        try:
            original = termios.tcgetattr(first_master)

            def controlling_terminal():
                os.setsid()
                fcntl.ioctl(0, termios.TIOCSCTTY, 0)

            child = subprocess.Popen([AGENT, "--socket", self.manager, "ui"],
                                     stdin=first_slave, stdout=second_slave, stderr=subprocess.PIPE,
                                     env=self.core.env, preexec_fn=controlling_terminal)
            try:
                _, error = child.communicate(timeout=5)
                self.assertNotEqual(child.returncode, 0)
                self.assertIn(b"terminal", error.lower())
                self.assertEqual(termios.tcgetattr(first_master), original)
            finally:
                if child.poll() is None:
                    child.kill()
                    child.communicate(timeout=5)
        finally:
            for fd in (first_master, first_slave, second_master, second_slave):
                os.close(fd)

    def native_client(self):
        client = Terminal([RMUX, "-S", self.core.socket, "attach-session", "-t", "main"], self.core.env)
        self.addCleanup(client.close)
        wait_for(lambda: bool(self.core.text("list-clients", "-F", "#{client_name}").strip()))
        client.drain(.2)
        return client

    def sidebar(self, *args):
        return subprocess.run([AGENT, "--socket", self.manager, "sidebar", "--core-native", self.core.socket, *args],
                              env=self.core.env, capture_output=True, text=True, timeout=15)

    def test_native_sidebar_reuse_zoom_navigation_and_owned_exit(self):
        self.ready()
        client = self.native_client()
        self.core.run("set-option", "-g", "remain-on-exit", "on")
        first = self.sidebar("--lang", "en")
        self.assertEqual(first.returncode, 0, first.stderr)
        pane = self.core.text("list-panes", "-t", "main:0", "-f", "#{@rmux-sidebar-owned}", "-F", "#{pane_id}").strip()
        self.assertTrue(pane.startswith("%"), first.stdout)
        self.assertEqual(self.core.text("display-message", "-p", "-t", "main:0", "#{pane_id}").strip(), "%0")
        again = self.sidebar()
        self.assertEqual(again.returncode, 0, again.stderr)
        self.assertIn("already open", again.stdout)
        self.assertEqual(self.core.text("show-options", "-pv", "-t", pane, "remain-on-exit").strip(), "off")
        self.core.run("select-pane", "-t", pane)
        wait_for(lambda: "agent-0" in self.core.text("capture-pane", "-p", "-t", pane), 5)
        client.until("Mark seen")
        client.click("Mark seen")
        wait_for(lambda: self.row()["attention"]["acknowledged"], 4)
        self.core.run("send-keys", "-t", pane, "z")
        wait_for(lambda: self.core.text("display-message", "-p", "-t", pane, "#{window_zoomed_flag}").strip() == "1", 5)
        # Native layout changes precede the helper's completion receipt. Wait
        # until the UI has released its single-flight action before navigating.
        wait_for(lambda: "Expanded agent sidebar" in self.core.text("capture-pane", "-p", "-t", pane), 5)
        self.core.run("send-keys", "-t", pane, "g")
        wait_for(lambda: self.core.text("display-message", "-p", "-t", "%0", "#{pane_active}:#{window_zoomed_flag}").strip() == "1:0", 5)
        self.core.run("select-pane", "-t", pane)
        self.core.run("send-keys", "-t", pane, "q")
        wait_for(lambda: pane not in self.core.text("list-panes", "-a", "-F", "#{pane_id}").splitlines(), 5)
        self.assertIn("%0", self.core.text("list-panes", "-a", "-F", "#{pane_id}"))
        client.drain()

    def test_native_navigation_and_wrong_core_rejection(self):
        self.ready()
        self.native_client()
        ui = self.desk("--core-native", str(self.core.socket))
        ui.send("jg")
        wait_for(lambda: self.core.text("list-clients", "-F", "#{pane_id}").strip() == self.panes[1], 5)
        ui.until("Selected the configured pane")
        ui.send("l")
        ui.send("c")
        wait_for(lambda: "rmux-agent-id" in self.core.text("list-buffers", "-F", "#{buffer_name}"), 5)
        ui.until("복사했습니다")
        self.assertEqual(self.core.text("show-buffer", "-b", "rmux-agent-id").strip(), "agent-1")
        other = Server(RMUX)
        other.env["RMUX_BRIDGE_SOCKET"] = str(other.path / "observe.sock")
        with other:
            peer = Terminal([RMUX,"-S",other.socket,"attach-session","-t","main"], other.env)
            try:
                wait_for(lambda: bool(other.text("list-clients", "-F", "#{client_name}").strip()))
                wrong = subprocess.run([AGENT,"--socket",self.manager,"sidebar","--core-native",other.socket],
                                       env=self.core.env, capture_output=True, text=True, timeout=8)
                self.assertNotEqual(wrong.returncode, 0)
                self.assertIn("boot", wrong.stderr)
            finally:
                peer.close()
            self.assertEqual(other.text("list-panes", "-a", "-F", "#{pane_id}").splitlines(), ["%0"])

    def test_native_shared_window_and_readonly_client_refusal(self):
        self.ready()
        self.native_client()
        second = self.native_client()
        refused = self.sidebar()
        self.assertNotEqual(refused.returncode, 0)
        self.assertEqual(len(self.core.text("list-panes", "-t", "main:0").splitlines()), 1)
        # A hidden target window has no viewers, but switch-client would move
        # both clients of the target session without a session-level fence.
        names = self.core.text("list-clients", "-F", "#{client_name}").splitlines()
        ui = self.desk("--core-native", str(self.core.socket), "--client", names[0])
        ui.send("jg")
        ui.until("rejected")
        self.assertEqual(self.core.text("list-clients", "-F", "#{pane_id}").splitlines(), ["%0", "%0"])
        second.close()
        # close is idempotent for registered cleanup.
        name = self.core.text("list-clients", "-F", "#{client_name}").strip()
        self.core.run("refresh-client", "-f", "read-only", "-t", name)
        refused = self.sidebar("--client", name)
        self.assertNotEqual(refused.returncode, 0)

    def test_respawned_sidebar_is_not_reused_or_killed(self):
        self.ready()
        self.native_client()
        first = self.sidebar()
        self.assertEqual(first.returncode, 0, first.stderr)
        old = self.core.text("list-panes", "-t", "main:0", "-f", "#{@rmux-sidebar-owned}", "-F", "#{pane_id}").strip()
        before = self.core.text("display-message", "-p", "-t", old, "#{rmux_pty_generation}")
        # Two argv elements select tmux's direct-exec path; a single command
        # string can retain an intermediary shell as the foreground process.
        self.core.run("respawn-pane", "-k", "-t", old, "/bin/cat", "-")
        after = self.core.text("display-message", "-p", "-t", old, "#{rmux_pty_generation}")
        self.assertNotEqual(before, after)
        again = self.sidebar()
        self.assertEqual(again.returncode, 0, again.stderr)
        self.assertIn("Created", again.stdout)
        self.assertIn(old, self.core.text("list-panes", "-a", "-F", "#{pane_id}").splitlines())
        # Wait for the newly forked process to reach exec before querying it.
        wait_for(lambda: self.core.text("display-message", "-p", "-t", old, "#{pane_current_command}").strip() == "cat")
        self.assertEqual(self.core.text("display-message", "-p", "-t", old, "#{pane_current_command}").strip(), "cat")

    def test_sidebar_uses_full_window_and_refuses_narrow_window(self):
        self.ready()
        client = self.native_client()
        narrow = self.core.text("split-window", "-h", "-l", "40", "-P", "-F", "#{pane_id}", "-t", "%0", "/bin/cat").strip()
        result = self.sidebar("--target", narrow)
        self.assertEqual(result.returncode, 0, result.stderr)
        width = int(self.core.text("display-message", "-p", "-t", narrow, "#{pane_width}"))
        self.assertGreaterEqual(width, 20, "sidebar split shrank only the 40-column target")
        panel = self.core.text("list-panes", "-t", "main:0", "-f", "#{@rmux-sidebar-owned}", "-F", "#{pane_id}").strip()
        self.core.run("kill-pane", "-t", panel)
        client.resize(80, 24)
        wait_for(lambda: int(self.core.text("display-message", "-p", "-t", "%0", "#{window_width}")) == 80)
        self.assertEqual(self.core.text("display-message", "-p", "-t", "%0", "#{e|>=:#{window_width},100}").strip(), "0")
        refused = self.sidebar()
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn("100", refused.stderr)


if __name__ == "__main__":
    unittest.main(verbosity=2)
