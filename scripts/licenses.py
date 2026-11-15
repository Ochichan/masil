#!/usr/bin/env python3
"""Collect the licence texts of the Rust crates linked into masil-agent."""
import argparse
import json
import re
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
NAMES = re.compile(r'(licen[cs]e|copying|notice|copyright|unlicense)', re.IGNORECASE)


def host():
    for line in subprocess.check_output(['rustc', '-vV'], text=True, encoding='utf-8').splitlines():
        if line.startswith('host: '):
            return line[6:].strip()
    raise SystemExit('rustc -vV did not report a host')


def linked():
    """Packages reached from masil-agent through normal and build dependencies."""
    metadata = json.loads(subprocess.check_output(
        ['cargo', 'metadata', '--format-version', '1', '--locked',
         '--filter-platform', host(),
         '--manifest-path', str(ROOT / 'agent/Cargo.toml')], cwd=ROOT, text=True,
        encoding='utf-8'))
    nodes = {node['id']: node for node in metadata['resolve']['nodes']}
    root = metadata['resolve']['root']
    seen, pending = {root}, [root]
    while pending:
        for dependency in nodes[pending.pop()]['deps']:
            used = any(kind['kind'] in (None, 'build') for kind in dependency['dep_kinds'])
            if used and dependency['pkg'] not in seen:
                seen.add(dependency['pkg'])
                pending.append(dependency['pkg'])
    seen.discard(root)
    return sorted((package for package in metadata['packages'] if package['id'] in seen),
                  key=lambda package: (package['name'], package['version']))


def text():
    parts = ['Licences of the Rust crates linked into masil-agent.\n'
             'agent/Cargo.lock in the source pins the same versions.\n']
    for package in linked():
        directory = Path(package['manifest_path']).parent
        files = sorted(path for path in directory.iterdir()
                       if path.is_file() and NAMES.match(path.name))
        parts.append('=' * 72)
        parts.append(f"{package['name']} {package['version']}")
        parts.append(f"License: {package.get('license') or 'see the files below'}")
        if package.get('repository'):
            parts.append(f"Source: {package['repository']}")
        if not files:
            parts.append('The crate ships no licence file; the expression above applies.')
        for path in files:
            parts.append('-' * 72)
            parts.append(path.name)
            parts.append('')
            parts.append(path.read_text(encoding='utf-8', errors='replace').rstrip())
        parts.append('')
    return '\n'.join(parts) + '\n'


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    arguments = parser.parse_args()
    arguments.output.parent.mkdir(parents=True, exist_ok=True)
    arguments.output.write_text(text(), encoding='utf-8')
