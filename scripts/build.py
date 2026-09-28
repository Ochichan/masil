#!/usr/bin/env python3
"""Build the pinned tmux-derived core without installing system packages."""
import argparse
import hashlib
import io
import json
import os
import re
from pathlib import Path
import shutil
import subprocess
import tarfile

ROOT = Path(__file__).resolve().parents[1]
BUILD = ROOT / '.build'
SHA = '94796f6b1182507efac8a272fc309a79e22e58a5'
TOOLS = [('m4', '1.4.20'), ('autoconf', '2.72'), ('automake', '1.17')]
TOOL_HASHES = {
    'm4': 'e236ea3a1ccf5f6c270b1c4bb60726f371fa49459a8eaaebc90b216b328daf2b',
    'autoconf': 'ba885c1319578d6c94d46e9b0dceb4014caafe2490e437a0dbca3f270a223f5a',
    'automake': '8920c1fc411e13b90bf704ef9db6f29d540e76d232cb3b2c9f4dc4cc599bd990',
}
CONFIGURE_FLAGS = ['--disable-debug', '--enable-optimizations', '--enable-utf8proc', '--enable-jemalloc']


def run(args, cwd, env, name):
    logs = BUILD / 'logs'
    logs.mkdir(parents=True, exist_ok=True)
    log = logs / (name + '.log')
    print(f'{name}: {log.relative_to(ROOT)}', flush=True)
    with log.open('w') as output:
        result = subprocess.run(args, cwd=cwd, env=env, stdout=output,
                                stderr=subprocess.STDOUT)
    if result.returncode:
        print(log.read_text(errors='replace')[-16000:])
        raise SystemExit(f'{name} failed ({result.returncode})')


def environment():
    env = os.environ.copy()
    env['PATH'] = str(BUILD / 'tools' / 'bin') + os.pathsep + env['PATH']
    pcs = [Path('/opt/homebrew/opt/libevent/lib/pkgconfig'),
           Path('/opt/homebrew/opt/ncurses/lib/pkgconfig'),
           Path('/opt/homebrew/opt/utf8proc/lib/pkgconfig'),
           Path('/opt/homebrew/opt/jemalloc/lib/pkgconfig')]
    paths = [str(p) for p in pcs if p.is_dir()]
    if env.get('PKG_CONFIG_PATH'):
        paths.append(env['PKG_CONFIG_PATH'])
    env['PKG_CONFIG_PATH'] = os.pathsep.join(paths)
    macros = Path('/opt/homebrew/share/aclocal')
    if macros.is_dir():
        env['ACLOCAL_PATH'] = str(macros) + (os.pathsep + env['ACLOCAL_PATH']
                                           if env.get('ACLOCAL_PATH') else '')
    env.setdefault('CC', 'clang' if shutil.which('clang') else 'cc')
    # Identical optimization settings for rmux and the upstream comparison.
    # Upstream's macOS debug build already exempts these compatibility headers.
    # Apply the same exceptions to both optimized builds; all other diagnostics fail.
    env['CFLAGS'] = '-O2 -Werror -Wno-macro-redefined -Wno-pointer-sign -Wno-deprecated-declarations'
    return env


def bootstrap(env):
    env = env | {'CFLAGS': '-O2'}
    for name, version in TOOLS:
        existing = shutil.which(name, path=env['PATH'])
        if existing:
            if name != 'm4':
                continue
            detected = subprocess.check_output([existing, '--version'], text=True).splitlines()[0]
            match = re.search(r'(\d+)\.(\d+)\.(\d+)', detected)
            if match and tuple(map(int, match.groups())) >= (1, 4, 8):
                continue
        url = f'https://ftp.gnu.org/gnu/{name}/{name}-{version}.tar.xz'
        cache = BUILD / 'downloads'
        cache.mkdir(parents=True, exist_ok=True)
        archive = cache / f'{name}-{version}.tar.xz'
        if not archive.exists():
            print(f'Downloading {url}', flush=True)
            temporary = archive.with_suffix('.partial')
            subprocess.run(['curl', '--fail', '--location', '--proto', '=https',
                            '--connect-timeout', '20', '--max-time', '180',
                            '--output', str(temporary), url], check=True)
            temporary.replace(archive)
        data = archive.read_bytes()
        if hashlib.sha256(data).hexdigest() != TOOL_HASHES[name]:
            raise SystemExit(f'Archive checksum mismatch: {archive}')
        print(f'{archive.name} sha256={hashlib.sha256(data).hexdigest()}', flush=True)
        sources = BUILD / 'tool-src'
        sources.mkdir(exist_ok=True)
        with tarfile.open(fileobj=io.BytesIO(data), mode='r:xz') as tar:
            tar.extractall(sources, filter='data')
        source = sources / f'{name}-{version}'
        run(['sh', 'configure', f'--prefix={BUILD / "tools"}'], source, env, name+'-configure')
        run(['make', '-j4'], source, env, name+'-build')
        run(['make', 'install'], source, env, name+'-local-install')


def baseline_source():
    source = BUILD / 'upstream'
    if source.is_dir():
        return source
    archive = BUILD / 'tmux-baseline.tar'
    if not archive.exists():
        clone = Path(os.environ.get('RMUX_TMUX_SOURCE',
                                    str(Path.home() / 'Documents/git_clones/tmux')))
        actual = subprocess.check_output(['git', '-C', str(clone), 'rev-parse', SHA], text=True).strip()
        if actual != SHA:
            raise SystemExit('Wrong upstream commit')
        archive.write_bytes(subprocess.check_output(['git', '-C', str(clone), 'archive', SHA]))
    source.mkdir()
    with tarfile.open(archive) as tar:
        tar.extractall(source, filter='data')
    return source


def build(source, name, env, jobs):
    configure = source / 'configure'
    needs_autogen = not configure.exists() or any(
        p.stat().st_mtime > configure.stat().st_mtime
        for p in (source / 'configure.ac', source / 'Makefile.am'))
    if needs_autogen:
        run(['sh', 'autogen.sh'], source, env, name+'-autogen')
    output = BUILD / name
    output.mkdir(exist_ok=True)
    probe_env = env | {'CFLAGS': env['CFLAGS'].replace(' -Werror', '')}
    config_key = json.dumps({'flags': CONFIGURE_FLAGS, 'cflags': env['CFLAGS'],
                            'probe_cflags': probe_env['CFLAGS'], 'cc': env['CC']}, sort_keys=True)
    stamp = output / 'configure-input.json'
    if not (output / 'Makefile').exists() or not stamp.exists() or stamp.read_text() != config_key or configure.stat().st_mtime > (output / 'Makefile').stat().st_mtime:
        if (output / 'Makefile').exists():
            run(['make', 'clean'], output, env, name+'-clean')
        run([str(configure), *CONFIGURE_FLAGS,
             f'--prefix={ROOT / "bin-prefix"}'], output, probe_env, name+'-configure')
        stamp.write_text(config_key)
    run(['make', f'-j{jobs}', 'CFLAGS=' + env['CFLAGS']], output, env, name+'-build')
    destination = ROOT / 'bin' / ('tmux-baseline' if name == 'baseline' else 'rmux')
    destination.parent.mkdir(exist_ok=True)
    staged = destination.with_suffix('.new')
    shutil.copy2(output / 'tmux', staged)
    staged.replace(destination)
    metadata = {'source_commit': SHA, 'binary': str(destination),
                'sha256': hashlib.sha256(destination.read_bytes()).hexdigest(),
                'compiler': subprocess.check_output([env['CC'], '--version'], text=True).splitlines()[0],
                'cflags': env['CFLAGS'], 'configure': CONFIGURE_FLAGS,
                'version': subprocess.check_output([str(destination), '-V'], text=True).strip()}
    (output / 'build.json').write_text(json.dumps(metadata, indent=2) + '\n')
    print(metadata['version'], destination, flush=True)


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--baseline', action='store_true')
    p.add_argument('--agent', action='store_true', help='build only the optional Rust observation client')
    p.add_argument('--clean', action='store_true', help='remove project build outputs only; preserve local tools')
    p.add_argument('--jobs', type=int, default=4)
    args = p.parse_args()
    if args.clean:
        for path in (BUILD / 'core', BUILD / 'baseline', ROOT / 'bin'):
            if path.exists():
                shutil.rmtree(path)
        return
    if not 1 <= args.jobs <= 32:
        p.error('--jobs must be 1..32')
    BUILD.mkdir(exist_ok=True)
    env = environment()
    if args.agent:
        # The Rust observer has no external native-library dependencies. Do not
        # accidentally link a Conda/sysdeps libiconv with an unresolved @rpath.
        env.pop('LIBRARY_PATH', None)
        run(['cargo', 'build', '--locked', '--release', '--manifest-path', str(ROOT/'agent/Cargo.toml')], ROOT, env, 'agent-build')
        run([str(ROOT/'agent/target/release/rmux-agent'), '--version'], ROOT, env, 'agent-smoke')
        (ROOT/'bin').mkdir(exist_ok=True)
        shutil.copy2(ROOT/'agent/target/release/rmux-agent', ROOT/'bin/rmux-agent')
        return
    bootstrap(env)
    source = baseline_source() if args.baseline else ROOT / 'core'
    build(source, 'baseline' if args.baseline else 'core', env, args.jobs)


if __name__ == '__main__':
    main()
