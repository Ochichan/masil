#!/usr/bin/env python3
"""Prepare release files locally. This script never publishes or changes a tag."""
import argparse
import hashlib
import io
import json
import platform
import re
import shutil
import subprocess
import tarfile
import tomllib
from pathlib import Path

import licenses

ROOT = Path(__file__).resolve().parents[1]
OUTPUT = ROOT / '.build/release'
REPOSITORY = 'https://github.com/Ochichan/masil'
TARGETS = ('macos-arm64', 'macos-x86_64', 'linux-x86_64', 'linux-arm64')
MARKER = '  # Stable release metadata is inserted here.'
VERSION_MANIFESTS = (
    'agent/Cargo.toml',
    'relay/Cargo.toml',
    'crates/masil-wire/Cargo.toml',
)


def command(*args):
    return subprocess.check_output(args, cwd=ROOT, text=True).strip()


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def cargo_version(manifest_rel):
    with (ROOT / manifest_rel).open('rb') as source:
        return tomllib.load(source)['package']['version']


def version():
    return cargo_version('agent/Cargo.toml')


def validate_versions():
    expected = version()
    for manifest in VERSION_MANIFESTS[1:]:
        found = cargo_version(manifest)
        if found != expected:
            raise SystemExit(
                f'{manifest} version {found} does not match agent/Cargo.toml version {expected}'
            )
    flake_path = ROOT / 'flake.nix'
    if flake_path.is_file():
        flake_text = flake_path.read_text(encoding='utf-8')
        needle = f'version = "{expected}";'
        count = flake_text.count(needle)
        if count != 1:
            raise SystemExit(
                f'flake.nix must declare {needle!r} exactly once (found {count})'
            )
    return expected


def validate(tag):
    if not re.fullmatch(r'v[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z]+(?:[.-][0-9A-Za-z]+)*)?', tag):
        raise SystemExit('release tag must be vMAJOR.MINOR.PATCH, with an optional prerelease suffix')
    pkg_version = validate_versions()
    stripped = tag[1:]
    base_tag = stripped.split('-', 1)[0]
    if stripped != pkg_version and base_tag != pkg_version:
        raise SystemExit(f'tag {tag} does not match package version {pkg_version}')
    return tag


def formula(url, release_version, sha256):
    source = (ROOT / 'Formula/masil.rb').read_text(encoding='utf-8')
    if source.count(MARKER) != 1:
        raise SystemExit('Formula/masil.rb must contain one stable metadata marker')
    # JSON string quoting is also valid for these Ruby string literals.
    # Homebrew reads the version from a release URL; its audit flags one that
    # only repeats it, so an empty field is left out.
    fields = '\n'.join(f'  {key} {json.dumps(value)}' for key, value in (
        ('url', url), ('version', release_version), ('sha256', sha256)) if value)
    # Keep the marker when promoting a release so the next release can update it.
    pattern = re.escape(MARKER) + r'(?:\n  (?:url|version|sha256) "[^"\n]*"){0,3}'
    return re.sub(pattern, lambda _: MARKER + '\n' + fields, source, count=1)


def render_pkgbuild(tag, sha256):
    template_path = ROOT / 'packaging/aur/PKGBUILD.tmpl'
    template = template_path.read_text(encoding='utf-8')
    for placeholder in ('__VERSION__', '__SHA_SOURCE__'):
        if placeholder not in template:
            raise SystemExit(f'{template_path} is missing placeholder {placeholder}')
    # Arch pkgver cannot contain hyphens; prerelease tags are never pushed to AUR,
    # and stable tags satisfy tag == f"v{pkgver}".
    pkgver = tag[1:].replace('-', '.')
    return (
        template
        .replace('__VERSION__', pkgver)
        .replace('__SHA_SOURCE__', sha256)
    )


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
    target = {
        ('Darwin', 'arm64'): 'macos-arm64',
        ('Darwin', 'x86_64'): 'macos-x86_64',
        ('Linux', 'x86_64'): 'linux-x86_64',
        ('Linux', 'aarch64'): 'linux-arm64',
        ('Linux', 'arm64'): 'linux-arm64',
    }.get((platform.system(), platform.machine()))
    if target not in TARGETS:
        raise SystemExit(
            'binary releases support macOS (arm64, x86_64) and Linux (x86_64, arm64)'
        )
    name = f'masil-{tag}-{target}'
    core_build = json.loads((ROOT / '.build/core/build.json').read_text(encoding='utf-8'))
    if core_build['sha256'] != digest(ROOT / 'bin/masil'):
        raise SystemExit('core binary differs from build metadata; run make all')
    if core_build.get('masil_revision') != {'commit': commit, 'dirty': False}:
        raise SystemExit('core was not built from this clean source revision; run make all')
    agent_record = ROOT / '.build/agent/build.json'
    agent_build = json.loads(agent_record.read_text(encoding='utf-8')) if agent_record.exists() else {}
    if agent_build.get('sha256') != digest(ROOT / 'bin/masil-agent'):
        raise SystemExit('agent binary differs from build metadata; run make all')
    if agent_build.get('masil_revision') != {'commit': commit, 'dirty': False}:
        raise SystemExit('agent was not built from this clean source revision; run make all')
    agent_version = command(str(ROOT / 'bin/masil-agent'), '--version')
    if agent_version != f'masil-agent {version()}':
        raise SystemExit('agent binary version differs from Cargo.toml; update the version and rebuild')
    metadata = {
        'version': tag, 'commit': commit, 'dirty': dirty, 'target': target,
        'platform': platform.platform(), 'core_build': core_build,
        'agent_version': agent_version,
        'sha256': {bin_name: digest(ROOT / 'bin' / bin_name) for bin_name in ('masil', 'masil-agent')},
    }
    dependencies = command('otool', '-L', str(ROOT / 'bin/masil'), str(ROOT / 'bin/masil-agent')) \
        if platform.system() == 'Darwin' else \
        command('ldd', str(ROOT / 'bin/masil'), str(ROOT / 'bin/masil-agent'))
    readme = f'''masil {tag}, {target}

Keep bin/masil and bin/masil-agent together and add their directory to PATH.
These binaries dynamically link system libraries. See dependencies.txt.
THIRD-PARTY-LICENSES.txt holds the licences of the Rust crates in masil-agent.

macOS (arm64 / x86_64): macOS 15 or newer with Homebrew (/opt/homebrew or /usr/local):
  brew install libevent ncurses utf8proc jemalloc
Linux (x86_64 / arm64): Ubuntu 24.04 (glibc 2.39), with these runtime packages:
  sudo apt-get install libevent-core-2.1-7t64 libncursesw6 libtinfo6 libutf8proc3 libjemalloc2
Other distributions/library versions can install via Homebrew, Nix (`nix run github:Ochichan/masil`),
AUR (`yay -S masil`), or `./install.sh --build`.

Run bin/masil to start. It does not replace tmux. The manual is masil.1,
although the compatible version command still reports tmux next-3.9.
'''
    files = {
        'bin/masil': 'bin/masil', 'bin/masil-agent': 'bin/masil-agent',
        'LICENSE': 'LICENSE', 'NOTICE': 'NOTICE',
        'core/COPYING': 'core/COPYING', 'core/UPSTREAM.json': 'core/UPSTREAM.json',
        'core/compat/yyjson/LICENSE': 'core/compat/yyjson/LICENSE',
        'agent/src/manifests/LICENSE': 'agent/src/manifests/LICENSE',
    }
    # configure.ac sets sysconfdir to /etc for the default prefix.
    manual = (ROOT / 'core/tmux.1').read_text(encoding='utf-8').replace('@SYSCONFDIR@', '/etc')
    texts = {'README.txt': readme, 'build.json': json.dumps(metadata, indent=2) + '\n',
             'dependencies.txt': dependencies + '\n',
             'share/man/man1/masil.1': manual,
             'THIRD-PARTY-LICENSES.txt': licenses.text()}
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
    sidecar = OUTPUT / f'{destination.name}.sha256'
    sidecar.write_text(f'{digest(destination)}  {destination.name}\n', encoding='utf-8')
    print(destination)


def prepare(tag):
    validate(tag)
    commit = command('git', 'rev-parse', 'HEAD')
    expected = {f'masil-{tag}-{target}.tar.gz' for target in TARGETS}
    actual = {path.name for path in OUTPUT.glob('*.tar.gz')}
    if actual != expected:
        raise SystemExit(f'expected only {sorted(expected)}, found {sorted(actual)}')
    for name in sorted(expected):
        archive_path = OUTPUT / name
        sidecar_path = OUTPUT / f'{name}.sha256'
        if not sidecar_path.is_file():
            raise SystemExit(f'missing checksum sidecar {sidecar_path.name}')
        sidecar_parts = sidecar_path.read_text(encoding='utf-8').strip().split()
        if len(sidecar_parts) != 2 or sidecar_parts[1].lstrip('*') != name:
            raise SystemExit(f'malformed checksum sidecar {sidecar_path.name}')
        if sidecar_parts[0] != digest(archive_path):
            raise SystemExit(f'checksum mismatch for {name}')
        prefix = name[:-7]
        with tarfile.open(archive_path) as archive:
            members = set(archive.getnames())
            for required in (
                f'{prefix}/bin/masil',
                f'{prefix}/bin/masil-agent',
                f'{prefix}/build.json',
                f'{prefix}/THIRD-PARTY-LICENSES.txt',
            ):
                if required not in members:
                    raise SystemExit(f'{name} is missing {required}')
            metadata = json.load(archive.extractfile(f'{prefix}/build.json'))
            if metadata['commit'] != commit or metadata['dirty'] or metadata['version'] != tag:
                raise SystemExit(f'{name} was not built from the release revision')
    source = source_archive(f'masil-{tag}')
    source_sha = digest(source)
    # Homebrew reads a plain version from the URL. It misreads some
    # prerelease suffixes (v0.1.0-dev.2 as 0.1.0), so those are stated.
    stated = tag[1:] if '-' in tag else None
    (OUTPUT / 'masil.rb').write_text(
        formula(f'{REPOSITORY}/releases/download/{tag}/{source.name}', stated, source_sha),
        encoding='utf-8',
    )
    (OUTPUT / 'PKGBUILD').write_text(render_pkgbuild(tag, source_sha), encoding='utf-8')
    installer_dst = OUTPUT / 'install.sh'
    shutil.copy2(ROOT / 'install.sh', installer_dst)
    installer_dst.chmod(0o755)
    paths = sorted([
        *OUTPUT.glob('*.tar.gz'),
        installer_dst,
        OUTPUT / 'masil.rb',
        OUTPUT / 'PKGBUILD',
    ])
    (OUTPUT / 'SHA256SUMS').write_text(
        ''.join(f'{digest(path)}  {path.name}\n' for path in paths),
        encoding='utf-8',
    )
    print(f'Release files ready in {OUTPUT}')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest='command', required=True)
    for name in ('validate', 'package', 'prepare'):
        sub = commands.add_parser(name)
        sub.add_argument('--version', default='', required=name != 'package')
    commands.add_parser('validate-versions')
    local = commands.add_parser('local-formula')
    local.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    OUTPUT.mkdir(parents=True, exist_ok=True)
    if args.command == 'validate-versions':
        print(validate_versions())
    elif args.command == 'validate':
        validate(args.version)
    elif args.command == 'package':
        package(args.version)
    elif args.command == 'prepare':
        prepare(args.version)
    else:
        source = source_archive('masil-ci')
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(formula(source.as_uri(), version(), digest(source)), encoding='utf-8')


if __name__ == '__main__':
    main()
