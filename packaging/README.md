# Packaging — install channels & supply-chain pipeline

These files are the **source of truth** for external package repositories. The
`Release` workflow (`.github/workflows/release.yml`) renders them on every `v*` tag
from the exact release source archive SHA-256 and pushes stable releases out, keeping
the Homebrew tap and AUR package in sync automatically.
**Edit the templates here — never the copies in external repos** (CI overwrites them).

| File | Renders to | One command it powers |
| --- | --- | --- |
| `../Formula/masil.rb` | `Ochichan/homebrew-tap` → `Formula/masil.rb` | `brew install Ochichan/tap/masil` |
| `aur/PKGBUILD.tmpl` | AUR → `masil` | `yay -S masil` |
| `../flake.nix` | Nix flake in this repo | `nix run github:Ochichan/masil` |
| `../install.sh` | Attached to every release + `main` | `curl -fsSL https://github.com/Ochichan/masil/releases/latest/download/install.sh \| bash` |

> **Why no Windows / Scoop channel?**
> `masil`'s terminal multiplexer core (`core/`) is a POSIX C fork of `tmux` requiring
> Unix PTYs, Unix domain sockets, and POSIX C libraries (`libevent`, `ncurses`,
> `utf8proc`, `jemalloc`). Windows is not supported as a masil host.

> **Why Homebrew and AUR build from the release source archive?**
> `bin/masil` dynamically links `libevent`, `ncurses` (with separate `libtinfo` on
> Debian/Ubuntu vs combined `libncursesw` on Arch), `utf8proc`, and `jemalloc`. Building
> from the attested `masil-vX.Y.Z-source.tar.gz` guarantees exact ABI linkage on every
> macOS version (arm64/x86_64), Linuxbrew, and Arch Linux (`x86_64`/`aarch64`).
> Prebuilt binary tarballs are shipped on GitHub Releases for all 4 Unix targets
> (`macos-arm64`, `macos-x86_64`, `linux-x86_64`, `linux-arm64`) and consumed by
> `install.sh`.

## What CI does on a `v*` tag

1. **`validate`** — checks the tag format and verifies that `agent/Cargo.toml`,
   `relay/Cargo.toml`, `crates/masil-wire/Cargo.toml`, and `flake.nix` share the same
   version.
2. **`build`** (`.github/workflows/build.yml`) — runs Rust formatting, Clippy, tests,
   `cargo-deny` (advisories across `--all-features`, bans, licenses, sources),
   `scripts/download_release_artifacts.py` tests, core C regressions, performance harness
   tests, smoke tests, Homebrew formula verification, Wire & Relay checks, Mobile checks,
   and packages all 4 Unix targets (`macos-arm64`, `macos-x86_64`, `linux-x86_64`,
   `linux-arm64`) with per-archive `.sha256` sidecars.
3. **`release`** — downloads platform artifacts with SHA-256 digest and ZIP-inventory
   verification (`scripts/download_release_artifacts.py`), verifies every archive's
   `build.json` and binary layout, creates `masil-vX.Y.Z-source.tar.gz`, renders
   `masil.rb` and `PKGBUILD`, copies `install.sh`, generates `SHA256SUMS`, generates
   GitHub build-provenance attestations (`actions/attest-build-provenance`), and creates
   the GitHub Release (marked as prerelease when the tag contains `-`).
4. **`publish-homebrew`** (stable tags only) — pushes the rendered `Formula/masil.rb`
   to `Ochichan/homebrew-tap`. Cleanly no-ops if `TAP_TOKEN` is unset.
5. **`publish-aur`** (stable tags only) — pushes the rendered `PKGBUILD` (with
   `updpkgsums` + `.SRCINFO`) to AUR package `masil`. Cleanly no-ops if
   `AUR_SSH_PRIVATE_KEY` is unset.

## One-time maintainer setup

### 1. Repository secrets (Settings → Secrets and variables → Actions)

- **`TAP_TOKEN`** — a fine-grained Personal Access Token with **Contents: Read and write**
  for `Ochichan/homebrew-tap` (you can reuse the same token used by `Yututui`).
- **`AUR_SSH_PRIVATE_KEY`** — private half of the SSH key registered on your AUR account
  (https://aur.archlinux.org → My Account → SSH Public Key).

### 2. Testing with a prerelease tag first

Exercise the full 4-target build, verification, attestation, and release pipeline on a
prerelease tag before cutting a stable tag:

```sh
git tag v0.1.0-rc1 && git push origin v0.1.0-rc1
```

Prerelease tags (containing `-`) publish a GitHub Pre-release with all 4 archives,
`install.sh`, `SHA256SUMS`, and attestations, while skipping `publish-homebrew` and
`publish-aur`. Delete the prerelease and tag afterward.
