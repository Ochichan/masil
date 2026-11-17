# Install and update masil

Use one installation method for a prefix. The Homebrew formula manages its
own files; the source installer refuses to replace those files or other
unrelated binaries. Neither method replaces `tmux` or edits your shell config.

## Homebrew

The formula currently follows the development branch:

```sh
brew tap Ochichan/masil https://github.com/Ochichan/masil
brew install --HEAD Ochichan/masil/masil
```

Update with:

```sh
brew update
brew upgrade --fetch-HEAD Ochichan/masil/masil
masil-agent doctor
```

Homebrew builds for your installed libraries. A stable source formula is
promoted only after a release; this guide does not imply one has been published.

## Source build

Use Python 3.12 or newer, Rust with edition 2024 support, a C compiler, make,
pkg-config, autoconf, automake and m4. CI uses Rust 1.96.0 and Python 3.13.
Install libevent, ncurses, utf8proc and jemalloc development libraries. On
Debian/Ubuntu the package names are `libevent-dev`, `libncurses-dev`,
`libutf8proc-dev` and `libjemalloc-dev`; the build also needs bison.

```sh
make all
make test-ci
make install
```

Add `$HOME/.local/bin` to your shell's `PATH`, then run `masil-agent doctor`.
For a separate installation use `make install PREFIX=/your/prefix` and add
that prefix's `bin` directory instead. The command never invokes sudo.

The installer places a complete bundle under `PREFIX/lib/masil/<content-hash>`.
Both public binary links resolve through `PREFIX/lib/masil/current`. It prepares
the bundle before atomically switching `current`, so an interrupted switch
does not select half of the new pair. Old bundles remain available; installing
again does not delete them. Each bundle includes the manual and license texts.
This is a same-user filesystem installation, not protection against another
process that can modify the prefix.

To update, fetch and select the source revision you want, then run `make install`
again. The installer does not fetch code or change branches for you. Rebuilding
the same inputs selects the same bundle. A modified existing bundle is refused.

## Running servers after an update

An update does not restart a server or stop its panes. Save your work and use
`masil-agent session save` before a planned restart. A new client can require
a newer core protocol or feature than an old running server provides. Restart
each server when its work can safely stop; restoration creates new processes
and cannot preserve an arbitrary running process across a restart.

To try a new build without disturbing existing work, start a separate socket:

```sh
masil -L upgrade-check -f /dev/null new-session -s check
```

Detach with `C-b d`. When finished, close only that test server with
`masil -L upgrade-check kill-server`.

## Release archives

Use only the archive matching the published OS and architecture, verify its
SHA-256 against the release's `SHA256SUMS`, and extract into a directory you own.
Keep both binaries together. They are dynamically linked and unsigned. The
archive's `dependencies.txt` records linked libraries; supported release
targets and runtime package names are listed in the root README. Other library
ABIs need a source build. Do not infer signed or universal standalone binaries
from the existence of a release workflow.

## Troubleshooting

- If `command -v masil` points at the wrong installation, fix PATH before
  updating again. `masil -V` intentionally prints the compatible tmux version;
  `masil-agent --version` reports the agent program version.
- If installation refuses an existing file, choose a new prefix or remove the
  old installation through the tool that owns it. The installer does not use
  a force-overwrite option.
- If libraries are missing, install the runtime dependencies or rebuild for
  the current machine. Copying the binary alone does not make it portable.
- `masil-agent doctor --versions` additionally runs installed providers'
  `--version` commands. Plain `doctor` does not run them or contact the network.
