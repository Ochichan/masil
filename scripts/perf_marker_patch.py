#!/usr/bin/env python3
"""Build the stock tmux comparison binary that carries masil's perf markers.

The marker hooks live in core/ (core/masil-perf.c and one-line hooks in
upstream files). This script copies the pinned stock source to
~/.cache/masil/perf-markers/src, adds the same module and the same hooks,
builds it with scripts/build.py's compiler and flags, and installs
~/.cache/masil/perf-markers/tmux-markers plus build.json. `--check` only
verifies the hook table against both trees.
"""
import argparse
import hashlib
import json
import os
import shutil
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import build as masil_build  # noqa: E402

ROOT = masil_build.ROOT
CORE = ROOT / 'core'
CACHE = Path.home() / '.cache/masil/perf-markers'
SENTINEL = '.masil-perf-cache'
MODULE_FILES = ['masil-perf.c', 'masil-perf.h']
INCLUDE = '#include "masil-perf.h" /* masil-perf */'

# (file, anchor line, hook line). A hook is inserted right after its anchor.
# Each anchor must match exactly once in the stock file and in core/.
HOOKS = [
    ['Makefile.am', '\tregsub.c \\', '\tmasil-perf.c \\'],
    ['Makefile.am', '\tmasil-perf.c \\', '\tmasil-perf.h \\'],
    ['tty.c', '#include "tmux.h"', INCLUDE],
    ['tty.c', '\tint\t\t nread;',
     '\tmasil_perf_input_begin(); /* masil-perf */'],
    ['tty.c', '\tlog_debug("%s: read %d bytes (already %zu)", name, nread, size);',
     '\tmasil_perf_input_read(); /* masil-perf */'],
    ['tty.c', '\tevbuffer_drain(tty->out, size);',
     '\tmasil_perf_tty_drained(tty); /* masil-perf */'],
    ['tty.c', '\tevbuffer_add(tty->out, buf, len);',
     '\tmasil_perf_tty_queue(tty); /* masil-perf */'],
    ['tty.c', '\tlog_debug("%s: wrote %d bytes (of %zu)", c->name, nwrite, size);',
     '\tmasil_perf_tty_written(tty); /* masil-perf */'],
    ['input-keys.c', '#include "tmux.h"', INCLUDE],
    ['input-keys.c', '\tbufferevent_write(bev, data, size);',
     '\tmasil_perf_key_write(bev); /* masil-perf */'],
    ['window.c', '#include "tmux.h"', INCLUDE],
    ['window.c', '\tstruct client\t\t\t*c;',
     '\tmasil_perf_pane_read_begin(); /* masil-perf */'],
    ['window.c', '\tinput_parse_pane(wp);',
     '\tmasil_perf_pane_read_end(); /* masil-perf */'],
    ['window.c', '\t\tbufferevent_free(wp->event);',
     '\t\tmasil_perf_pty_free(wp->event); /* masil-perf */'],
    ['window.c', '\twp->ictx = input_init(wp, wp->event, &wp->palette);',
     '\tmasil_perf_pty_event(wp->event); /* masil-perf */'],
    ['server-client.c', '#include "tmux.h"', INCLUDE],
    ['server-client.c', '\t\tserver_client_set_progress_bar(c);',
     '\t\tmasil_perf_redraw_begin(c); /* masil-perf */'],
    ['server-client.c', '\t\tredraw_screen(c);',
     '\t\tmasil_perf_redraw_end(); /* masil-perf */'],
    ['server.c', '#include "tmux.h"', INCLUDE],
    ['server.c', '\tserver_acl_init();',
     '\tmasil_perf_init(); /* masil-perf */'],
    ['server.c', '\t} while (items != 0);',
     '\tmasil_perf_loop_drained(); /* masil-perf */'],
    ['server.c', '\tprompt_save_history();',
     '\tmasil_perf_write(); /* masil-perf */'],
]


def hook_table_digest():
    table = json.dumps(HOOKS, sort_keys=True, separators=(',', ':'),
                       ensure_ascii=True)
    return hashlib.sha256(table.encode()).hexdigest()


def sha256_file(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def read_lines(path):
    return Path(path).read_text().split('\n')


def hook_failures(tree, label, apply_to=None):
    """Check every hook against the files of tree. With apply_to (a copy of
    the stock tree) the hooks are also inserted there. Returns failures."""
    failures = []
    cache = {}
    for index, (name, anchor, hook) in enumerate(HOOKS):
        if name not in cache:
            cache[name] = read_lines(Path(tree) / name)
        lines = cache[name]
        found = [i for i, line in enumerate(lines) if line == anchor]
        where = f'hook {index} ({name}, anchor {anchor!r})'
        if len(found) != 1:
            failures.append(f'{label}: {where}: anchor matches {len(found)} times')
            continue
        if apply_to is not None:
            lines.insert(found[0] + 1, hook)
        elif found[0] + 1 >= len(lines) or lines[found[0] + 1] != hook:
            failures.append(f'{label}: {where}: hook line {hook!r} does not follow')
    if apply_to is not None and not failures:
        for name, lines in cache.items():
            (Path(apply_to) / name).write_text('\n'.join(lines))
    return failures


def check_core():
    return hook_failures(CORE, 'core')


def check_stock(source):
    # Verify against a scratch copy of the table application, not the tree.
    failures = []
    cache = {}
    for index, (name, anchor, hook) in enumerate(HOOKS):
        lines = cache.setdefault(name, read_lines(Path(source) / name))
        found = [i for i, line in enumerate(lines) if line == anchor]
        if len(found) != 1:
            failures.append(f'stock: hook {index} ({name}, anchor {anchor!r}): '
                            f'anchor matches {len(found)} times')
        else:
            lines.insert(found[0] + 1, hook)
    return failures


def run(args, cwd, env, name, logs):
    logs.mkdir(parents=True, exist_ok=True)
    log = logs / (name + '.log')
    print(f'{name}: {log}', flush=True)
    with log.open('w') as output:
        result = subprocess.run(args, cwd=cwd, env=env, stdout=output,
                                stderr=subprocess.STDOUT)
    if result.returncode:
        print(log.read_text(errors='replace')[-16000:])
        raise SystemExit(f'{name} failed ({result.returncode})')


def prepare_cache():
    """Create the cache dir with a sentinel. Refuse to delete anything under
    an existing dir that lacks the sentinel."""
    sentinel = CACHE / SENTINEL
    if CACHE.exists() and not sentinel.is_file():
        if any(CACHE.iterdir()):
            raise SystemExit(f'{CACHE} exists without {SENTINEL}; not touching it')
    CACHE.mkdir(parents=True, exist_ok=True)
    sentinel.write_text('masil perf marker cache\n')


def prepare_source(stock):
    source = CACHE / 'src'
    prepare_cache()
    if source.exists():
        shutil.rmtree(source)
    shutil.copytree(stock, source, symlinks=True)
    for name in MODULE_FILES:
        shutil.copy2(CORE / name, source / name)
    failures = hook_failures(source, 'stock', apply_to=source)
    if failures:
        raise SystemExit('\n'.join(failures))
    return source


def build_stock(source):
    env = masil_build.environment()
    jobs = os.cpu_count() or 4
    logs = CACHE / 'logs'
    output = CACHE / 'build'
    if output.exists():
        if not (CACHE / SENTINEL).is_file():
            raise SystemExit(f'{CACHE} has no {SENTINEL}; not removing build/')
        shutil.rmtree(output)
    output.mkdir(parents=True)
    run(['sh', 'autogen.sh'], source, env, 'autogen', logs)
    probe_env = env | {'CFLAGS': env['CFLAGS'].replace(' -Werror', '')}
    run([str(source / 'configure'), *masil_build.CONFIGURE_FLAGS,
         f'--prefix={CACHE / "prefix"}'], output, probe_env, 'configure', logs)
    define = '-DMASIL_PERF_PRODUCT=\\"stock\\"'
    cflags = env['CFLAGS'] + ' ' + define
    run(['make', f'-j{jobs}', 'CFLAGS=' + cflags], output, env, 'make', logs)
    destination = CACHE / 'tmux-markers'
    staged = destination.with_suffix('.new')
    shutil.copy2(output / 'tmux', staged)
    staged.replace(destination)
    metadata = {
        'product': 'stock',
        'source_commit': masil_build.SHA,
        'hook_table_sha256': hook_table_digest(),
        'module_sha256': {n: sha256_file(CORE / n) for n in MODULE_FILES},
        'masil_revision': masil_build.git_revision(),
        'binary': str(destination),
        'sha256': sha256_file(destination),
        'compiler': subprocess.check_output([env['CC'], '--version'],
                                            text=True).splitlines()[0],
        'cflags': cflags,
        'configure': masil_build.CONFIGURE_FLAGS,
        'dependencies': masil_build.dependency_flags(env),
    }
    (CACHE / 'build.json').write_text(json.dumps(metadata, indent=2) + '\n')
    print(destination, flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--check', action='store_true',
                        help='verify the hook table against core/ and the stock source, no build')
    args = parser.parse_args()

    for name in MODULE_FILES:
        if not (CORE / name).is_file():
            raise SystemExit(f'missing core/{name}')
    if args.check:
        stock = ROOT / '.build/upstream'
        if not (stock / 'tmux.h').is_file():
            raise SystemExit(f'{stock} is missing; run a build first, --check '
                             'does not extract it')
    else:
        stock = masil_build.baseline_source()
    failures = check_core() + check_stock(stock)
    if failures:
        print('\n'.join(failures))
        raise SystemExit(1)
    print(f'hook table ok: {len(HOOKS)} hooks, sha256 {hook_table_digest()}')
    if args.check:
        return
    build_stock(prepare_source(stock))


if __name__ == '__main__':
    main()
