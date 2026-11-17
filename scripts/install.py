#!/usr/bin/env python3
"""Install a local build as a versioned bundle, without replacing tmux."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import tempfile

import licenses

ROOT = Path(__file__).resolve().parents[1]


def payload():
    files = {}
    for name in ('masil', 'masil-agent'):
        source = ROOT / 'bin' / name
        if not source.is_file() or not os.access(source, os.X_OK):
            raise ValueError(f'{source} is missing or not executable; run make all')
        files[f'bin/{name}'] = (source.read_bytes(), 0o755)
    for name in ('LICENSE', 'NOTICE', 'core/COPYING', 'core/UPSTREAM.json',
                 'core/compat/yyjson/LICENSE', 'agent/src/manifests/LICENSE'):
        files[f'share/masil/{name}'] = ((ROOT / name).read_bytes(), 0o644)
    files['share/masil/THIRD-PARTY-LICENSES.txt'] = (licenses.text().encode(), 0o644)
    manual = (ROOT / 'core/tmux.1').read_text().replace('@SYSCONFDIR@', '/etc')
    files['share/man/man1/masil.1'] = (manual.encode(), 0o644)
    return files


def install(prefix, files):
    """Prepare both binaries before atomically selecting their bundle.

    Existing unrelated paths are refused. Previous bundles stay available to
    running processes. No configurations, sockets or sessions are changed.
    """
    prefix = prefix.expanduser().absolute()
    bundle_root = prefix / 'lib/masil'
    current = bundle_root / 'current'
    links = {
        prefix / 'bin/masil': current / 'bin/masil',
        prefix / 'bin/masil-agent': current / 'bin/masil-agent',
        prefix / 'share/man/man1/masil.1': current / 'share/man/man1/masil.1',
        prefix / 'share/masil': current / 'share/masil',
    }
    metadata = {}
    for name, (data, mode) in sorted(files.items()):
        path = Path(name)
        if path.is_absolute() or '..' in path.parts:
            raise ValueError(f'invalid bundle member: {name}')
        metadata[name] = {'sha256': hashlib.sha256(data).hexdigest(), 'mode': mode}
    manifest = (json.dumps(metadata, sort_keys=True, indent=2) + '\n').encode()
    version = hashlib.sha256(manifest).hexdigest()
    destination = bundle_root / version
    # Do not follow a pre-existing directory symlink into another installation.
    for directory in (bundle_root, *(path.parent for path in links)):
        for parent in (directory, *directory.parents):
            if parent == prefix:
                break
            if parent.is_symlink():
                raise ValueError(f'installation directory is a symlink: {parent}')
    for link, target in links.items():
        expected = os.path.relpath(target, link.parent)
        if os.path.lexists(link) and (not link.is_symlink() or os.readlink(link) != expected):
            raise ValueError(f'refusing to replace unrelated path: {link}')
    if os.path.lexists(current):
        if not current.is_symlink():
            raise ValueError(f'refusing to replace unrelated path: {current}')
        previous = os.readlink(current)
        if len(previous) != 64 or any(c not in '0123456789abcdef' for c in previous):
            raise ValueError(f'unrecognized installation link: {current}')
    bundle_root.mkdir(parents=True, exist_ok=True)
    if destination.exists() or destination.is_symlink():
        if destination.is_symlink() or not destination.is_dir():
            raise ValueError(f'invalid existing bundle: {destination}')
        for name, (data, mode) in files.items():
            member = destination / name
            if (member.is_symlink() or not member.is_file()
                    or member.read_bytes() != data or member.stat().st_mode & 0o777 != mode):
                raise ValueError(f'existing bundle was modified: {member}')
    else:
        stage = Path(tempfile.mkdtemp(prefix='.install-', dir=bundle_root))
        try:
            for name, (data, mode) in files.items():
                member = stage / name
                member.parent.mkdir(parents=True, exist_ok=True)
                member.write_bytes(data)
                member.chmod(mode)
            (stage / 'manifest.json').write_bytes(manifest)
            stage.chmod(0o755)
            stage.rename(destination)
        finally:
            if stage.exists():
                shutil.rmtree(stage)
    created = []
    temporary = None
    try:
        for link, target in links.items():
            link.parent.mkdir(parents=True, exist_ok=True)
            if not os.path.lexists(link):
                link.symlink_to(os.path.relpath(target, link.parent))
                created.append(link)
        descriptor, path = tempfile.mkstemp(prefix='.current-', dir=bundle_root)
        os.close(descriptor)
        temporary = Path(path)
        temporary.unlink()
        temporary.symlink_to(version)
        os.replace(temporary, current)
    except BaseException:
        for link in reversed(created):
            link.unlink(missing_ok=True)
        raise
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)
    return destination


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--prefix', type=Path, default=Path.home() / '.local')
    args = parser.parse_args()
    try:
        destination = install(args.prefix, payload())
    except (OSError, ValueError) as error:
        parser.exit(1, f'install: {error}\n')
    print(f'Installed {destination}')
    print(f'Add {args.prefix.expanduser().absolute() / "bin"} to PATH.')
    print('Existing servers keep running their old core. Save work and restart them when ready.')


if __name__ == '__main__':
    main()
