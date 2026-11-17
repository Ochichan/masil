#!/usr/bin/env python3
"""Local installer ownership, paired updates and failure behavior."""
import importlib.util
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
spec = importlib.util.spec_from_file_location('masil_install', Path(sys.path[0]) / 'install.py')
installer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(installer)


def files(version):
    return {name: (version.encode(), mode) for name, mode in (
        ('bin/masil', 0o755), ('bin/masil-agent', 0o755),
        ('share/man/man1/masil.1', 0o644), ('share/masil/LICENSE', 0o644))}


class InstallTests(unittest.TestCase):
    def test_update_switches_pair_and_preserves_old_bundle(self):
        with tempfile.TemporaryDirectory() as root:
            prefix = Path(root) / 'space in prefix'
            first = installer.install(prefix, files('first'))
            second = installer.install(prefix, files('second'))
            self.assertNotEqual(first, second)
            self.assertEqual((first / 'bin/masil').read_text(), 'first')
            for name in ('masil', 'masil-agent'):
                self.assertEqual((prefix / 'bin' / name).read_text(), 'second')
                self.assertTrue(os.access(prefix / 'bin' / name, os.X_OK))
            self.assertEqual(installer.install(prefix, files('second')), second)

    def test_refuses_unrelated_binary_before_changes(self):
        with tempfile.TemporaryDirectory() as root:
            prefix = Path(root)
            (prefix / 'bin').mkdir()
            (prefix / 'bin/masil').write_text('other install')
            with self.assertRaisesRegex(ValueError, 'unrelated path'):
                installer.install(prefix, files('new'))
            self.assertEqual((prefix / 'bin/masil').read_text(), 'other install')
            self.assertFalse((prefix / 'lib').exists())

    def test_switch_failure_keeps_previous_pair(self):
        with tempfile.TemporaryDirectory() as root:
            prefix = Path(root)
            installer.install(prefix, files('old'))
            with patch.object(installer.os, 'replace', side_effect=OSError('disk error')):
                with self.assertRaises(OSError):
                    installer.install(prefix, files('new'))
            for name in ('masil', 'masil-agent'):
                self.assertEqual((prefix / 'bin' / name).read_text(), 'old')

    def test_refuses_modified_bundle_and_directory_symlink(self):
        with tempfile.TemporaryDirectory() as root:
            prefix = Path(root) / 'prefix'
            bundle = installer.install(prefix, files('old'))
            (bundle / 'bin/masil').write_text('modified')
            with self.assertRaisesRegex(ValueError, 'modified'):
                installer.install(prefix, files('old'))
            other = Path(root) / 'other'
            other.mkdir()
            (other / 'bin').symlink_to(prefix / 'bin')
            with self.assertRaisesRegex(ValueError, 'symlink'):
                installer.install(other, files('new'))


if __name__ == '__main__':
    unittest.main()
