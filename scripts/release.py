#!/usr/bin/env python3
"""Prepare release files locally. This script never publishes or changes a tag."""
import argparse
import hashlib
import io
import json
import platform
import re
import subprocess
import tarfile
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
OUTPUT = ROOT / '.build/release'
REPOSITORY = 'https://github.com/Ochichan/masil'
TARGETS = ('macos-arm64', 'linux-x86_64')
MARKER = '  # Stable release metadata is inserted here.'


def command(*args):
    return subprocess.check_output(args, cwd=ROOT, text=True).strip()


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def version():
    with (ROOT / 'agent/Cargo.toml').open('rb') as source:
        return tomllib.load(source)['package']['version']


def validate(tag):
    if not re.fullmatch(r'v[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z]+(?:[.-][0-9A-Za-z]+)*)?', tag):
        raise SystemExit('release tag must be vMAJOR.MINOR.PATCH, with an optional prerelease suffix')
    if tag != 'v' + version():
        raise SystemExit(f'tag {tag} does not match agent/Cargo.toml version {version()}')
    return tag


def formula(url, release_version, sha256):
    source = (ROOT / 'Formula/masil.rb').read_text()
    if source.count(MARKER) != 1:
        raise SystemExit('Formula/masil.rb must contain one stable metadata marker')
    # JSON string quoting is also valid for these Ruby string literals.
    fields = '\n'.join(f'  {key} {json.dumps(value)}' for key, value in (
        ('url', url), ('version', release_version), ('sha256', sha256)))
    # Keep the marker when promoting a release so the next release can update it.
    pattern = re.escape(MARKER) + r'(?:\n  (?:url|version|sha256) "[^"\n]*"){0,3}'
    return re.sub(pattern, lambda _: MARKER + '\n' + fields, source, count=1)


def source_archive(name):
    destination = OUTPUT / f'{name}-source.tar.gz'
    subprocess.run(['git', 'archive', '--format=tar.gz', f'--prefix={name}/',
                    '-o', str(destination), 'HEAD'], cwd=ROOT, check=True)
    return destination


def package(tag):
    commit = command('git', 'rev-parse', 'HEAD')
    dirty = bool(command('git', 'status', '--porcelain', '--untracked-files=normal'))
    if tag:
        validate(tag)
        if dirty:
            raise SystemExit('release packaging requires a clean checkout')
    else:
        tag = f'dev-{commit[:12]}'
    target = {('Darwin', 'arm64'): 'macos-arm64',
              ('Linux', 'x86_64'): 'linux-x86_64'}.get((platform.system(), platform.machine()))
    if target not in TARGETS:
        raise SystemExit('binary releases currently support macOS arm64 and Linux x86_64')
    name = f'masil-{tag}-{target}'
    core_build = json.loads((ROOT / '.build/core/build.json').read_text())
    if core_build['sha256'] != digest(ROOT / 'bin/masil'):
        raise SystemExit('core binary differs from build metadata; run make all')
    if core_build.get('masil_revision') != {'commit': commit, 'dirty': False}:
        raise SystemExit('core was not built from this clean source revision; run make all')
    agent_version = command(str(ROOT / 'bin/masil-agent'), '--version')
    if agent_version != f'masil-agent {version()}':
        raise SystemExit('agent binary version differs from Cargo.toml; update the version and rebuild')
    metadata = {
        'version': tag, 'commit': commit, 'dirty': dirty, 'target': target,
        'platform': platform.platform(), 'core_build': core_build,
        'agent_version': agent_version,
        'sha256': {name: digest(ROOT / 'bin' / name) for name in ('masil', 'masil-agent')},
    }
    dependencies = command('otool', '-L', str(ROOT / 'bin/masil'), str(ROOT / 'bin/masil-agent')) \
        if platform.system() == 'Darwin' else \
        command('ldd', str(ROOT / 'bin/masil'), str(ROOT / 'bin/masil-agent'))
    readme = f'''masil {tag}, {target}

Keep bin/masil and bin/masil-agent together and add their directory to PATH.
These binaries dynamically link system libraries. See dependencies.txt.

macOS: macOS 15 or newer on Apple Silicon, with Homebrew in /opt/homebrew:
  brew install libevent ncurses utf8proc jemalloc
Linux: Ubuntu 24.04 x86_64 (glibc 2.39), with these runtime packages:
  sudo apt-get install libevent-core-2.1-7t64 libncursesw6 libtinfo6 libutf8proc3 libjemalloc2
Other distributions/library versions are not covered by this binary release.
Homebrew builds from source for the libraries installed on your machine.

Run bin/masil to start. It does not replace tmux. The manual is masil.1,
although the compatible version command still reports tmux next-3.9.
'''
    files = {
        'bin/masil': 'bin/masil', 'bin/masil-agent': 'bin/masil-agent',
        'share/man/man1/masil.1': 'core/tmux.1',
        'LICENSE': 'LICENSE', 'NOTICE': 'NOTICE',
        'core/COPYING': 'core/COPYING', 'core/UPSTREAM.json': 'core/UPSTREAM.json',
        'core/compat/yyjson/LICENSE': 'core/compat/yyjson/LICENSE',
        'agent/src/manifests/LICENSE': 'agent/src/manifests/LICENSE',
    }
    texts = {'README.txt': readme, 'build.json': json.dumps(metadata, indent=2) + '\n',
             'dependencies.txt': dependencies + '\n'}
    destination = OUTPUT / f'{name}.tar.gz'
    with tarfile.open(destination, 'w:gz') as archive:
        for member, source in files.items():
            archive.add(ROOT / source, arcname=f'{name}/{member}', recursive=False)
        for member, text in texts.items():
            data = text.encode()
            info = tarfile.TarInfo(f'{name}/{member}')
            info.size = len(data)
            info.mode = 0o644
            archive.addfile(info, io.BytesIO(data))
    print(destination)


def prepare(tag):
    validate(tag)
    commit = command('git', 'rev-parse', 'HEAD')
    expected = {f'masil-{tag}-{target}.tar.gz' for target in TARGETS}
    actual = {path.name for path in OUTPUT.glob('*.tar.gz')}
    if actual != expected:
        raise SystemExit(f'expected only {sorted(expected)}, found {sorted(actual)}')
    for name in sorted(expected):
        with tarfile.open(OUTPUT / name) as archive:
            metadata = json.load(archive.extractfile(f'{name[:-7]}/build.json'))
            if metadata['commit'] != commit or metadata['dirty'] or metadata['version'] != tag:
                raise SystemExit(f'{name} was not built from the release revision')
    source = source_archive(f'masil-{tag}')
    (OUTPUT / 'masil.rb').write_text(formula(
        f'{REPOSITORY}/releases/download/{tag}/{source.name}', tag[1:], digest(source)))
    paths = sorted([*OUTPUT.glob('*.tar.gz'), OUTPUT / 'masil.rb'])
    (OUTPUT / 'SHA256SUMS').write_text(''.join(f'{digest(path)}  {path.name}\n' for path in paths))
    print(f'Release files ready in {OUTPUT}')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest='command', required=True)
    for name in ('validate', 'package', 'prepare'):
        sub = commands.add_parser(name)
        sub.add_argument('--version', default='', required=name != 'package')
    local = commands.add_parser('local-formula')
    local.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    OUTPUT.mkdir(parents=True, exist_ok=True)
    if args.command == 'validate':
        validate(args.version)
    elif args.command == 'package':
        package(args.version)
    elif args.command == 'prepare':
        prepare(args.version)
    else:
        source = source_archive('masil-ci')
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(formula(source.as_uri(), version(), digest(source)))


if __name__ == '__main__':
    main()
