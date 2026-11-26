# Install and update masil

Use one installation method for a prefix. Package managers (Homebrew, Nix, AUR) manage
their own files; the source installer (`scripts/install.py`) refuses to replace unrelated
binaries. None of the install channels replace system `tmux`.

## Homebrew (macOS / Linux)

Stable releases publish automatically to `Ochichan/homebrew-tap`:

```sh
brew install Ochichan/tap/masil
```

To follow the development `main` branch directly from this repository:

```sh
brew tap Ochichan/masil https://github.com/Ochichan/masil
brew install --HEAD Ochichan/masil/masil
```

Update with:

```sh
brew update
brew upgrade Ochichan/tap/masil              # or: brew upgrade --fetch-HEAD Ochichan/masil/masil
masil-agent doctor
```

Homebrew compiles `masil` against the exact `libevent`, `ncurses`, `utf8proc`, and
`jemalloc` libraries installed on your machine.

## Nix flake (Linux / macOS)

Run without installing permanently:

```sh
nix run github:Ochichan/masil
```

Install into your profile:

```sh
nix profile install github:Ochichan/masil
```

Additional flake outputs: `masil-agent`, `masil-relay`, and `devShells.<system>.default`
(`nix develop`). Supported systems: `x86_64-linux`, `aarch64-linux`, `aarch64-darwin`
(Intel macOS users can use Homebrew or `install.sh`).

## Arch Linux (AUR)

Stable releases publish `masil` to the AUR (`x86_64` and `aarch64`):

```sh
yay -S masil
```

The `PKGBUILD` builds from the release source archive against Arch's native `libevent`,
`ncurses`, `libutf8proc`, and `jemalloc`.

## Direct installer (`install.sh`)

For macOS 15+ (`arm64`, `x86_64`) and Ubuntu 24.04 (`x86_64`, `arm64`), run the
release-pinned installer:

```sh
curl -fsSL https://github.com/Ochichan/masil/releases/latest/download/install.sh | bash
```

Options:
- `MASIL_VERSION=vX.Y.Z ./install.sh` — pin a specific release tag.
- `./install.sh --prefix /your/prefix` — install under a custom prefix (default `$HOME/.local`).
- `./install.sh --no-modify-path` — do not edit your shell rc file.
- `./install.sh --build` — build from source inside a local checkout (`make all && make install`).

`install.sh` always fetches `SHA256SUMS` first and verifies the downloaded archive's
SHA-256 digest before extracting anything.

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

## Release archives and verification

Prebuilt archives cover four Unix targets:
- `macos-arm64` (macOS 15+, Apple Silicon, `/opt/homebrew`)
- `macos-x86_64` (macOS 15+, Intel, `/usr/local`)
- `linux-x86_64` (Ubuntu 24.04, glibc 2.39)
- `linux-arm64` (Ubuntu 24.04, glibc 2.39)

Every release publishes `SHA256SUMS` and GitHub build-provenance attestations:

```sh
# Verify SHA-256 checksums:
sha256sum -c --ignore-missing SHA256SUMS        # macOS: shasum -a 256 -c --ignore-missing SHA256SUMS

# Verify GitHub build provenance (requires GitHub CLI):
gh attestation verify masil-vX.Y.Z-linux-x86_64.tar.gz --repo Ochichan/masil
```

Keep both `bin/masil` and `bin/masil-agent` together. The archives are dynamically linked
and unsigned (not Apple-notarized). The archive's `dependencies.txt` records linked
libraries. Other distributions/ABIs should use Homebrew, Nix, AUR, or a source build.

## Troubleshooting

- If `command -v masil` points at the wrong installation, fix PATH before
  updating again. `masil -V` intentionally prints the compatible tmux version;
  `masil-agent --version` reports the agent program version.
- If `scripts/install.py` refuses an existing file, choose a new prefix or remove the
  old installation through the tool that owns it.
- If libraries are missing, install the runtime dependencies or rebuild for
  the current machine. Copying the binary alone does not make it portable.
- `masil-agent doctor --versions` additionally runs installed providers'
  `--version` commands. Plain `doctor` does not run them or contact the network.
