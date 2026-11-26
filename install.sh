#!/usr/bin/env bash
#
# masil installer (macOS / Linux) — installs `masil` and `masil-agent` together.
#
#   curl -fsSL https://github.com/Ochichan/masil/releases/latest/download/install.sh | bash
#   curl -fsSL https://raw.githubusercontent.com/Ochichan/masil/main/install.sh | bash
#                                 # download & verify a prebuilt archive — no clone needed
#   ./install.sh                  # download prebuilt (or build locally if unsupported target)
#   ./install.sh --build          # always build from source in a checkout (make all && make install)
#   ./install.sh --prefix DIR     # install prefix (default: $HOME/.local)
#   ./install.sh --no-modify-path # do not edit any shell rc file
#
# Pin a release tag with:  MASIL_VERSION=v0.1.0 ./install.sh  (default: latest release)
#
set -euo pipefail

REPO_SLUG="Ochichan/masil"
PREFIX="${PREFIX:-$HOME/.local}"
FORCE_BUILD=0
NO_MODIFY_PATH=0
DOWNLOAD_TMP=""

cleanup_download_tmp() {
  if [ -n "${DOWNLOAD_TMP:-}" ]; then
    rm -rf "$DOWNLOAD_TMP"
  fi
}
trap cleanup_download_tmp EXIT

c_info=$'\033[1;36m'
c_ok=$'\033[1;32m'
c_warn=$'\033[1;33m'
c_err=$'\033[1;31m'
c_off=$'\033[0m'
info() { printf '%s==>%s %s\n' "$c_info" "$c_off" "$*"; }
ok()   { printf '%s\xe2\x9c\x93%s %s\n' "$c_ok" "$c_off" "$*"; }
warn() { printf '%swarn:%s %s\n' "$c_warn" "$c_off" "$*" >&2; }
die()  { printf '%serror:%s %s\n' "$c_err" "$c_off" "$*" >&2; exit 1; }

usage() {
  cat <<'EOF'
Usage: install.sh [OPTIONS]

Options:
  --build           Build from source in a local masil checkout (make all && make install)
  --prefix DIR      Installation prefix (default: $HOME/.local)
  --no-modify-path  Do not add <prefix>/bin to your shell rc file
  -h, --help        Show this help message

Environment:
  MASIL_VERSION     Release tag to install (e.g. v0.1.0); defaults to latest
  PREFIX            Alternative way to set --prefix
EOF
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --build)
      FORCE_BUILD=1
      shift
      ;;
    --no-modify-path)
      NO_MODIFY_PATH=1
      shift
      ;;
    --prefix)
      [ "$#" -ge 2 ] || die "--prefix requires a directory argument"
      PREFIX="$2"
      shift 2
      ;;
    --prefix=*)
      PREFIX="${1#--prefix=}"
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      warn "ignoring unknown argument: $1"
      shift
      ;;
  esac
done

OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
  Darwin|Linux) ;;
  *)
    die "Unsupported OS '$OS'. masil supports macOS and Linux (Windows hosts are not supported)."
    ;;
esac

release_target_for_platform() {
  case "$OS/$ARCH" in
    Darwin/arm64)              echo "macos-arm64" ;;
    Darwin/x86_64)             echo "macos-x86_64" ;;
    Linux/x86_64)              echo "linux-x86_64" ;;
    Linux/aarch64|Linux/arm64) echo "linux-arm64" ;;
    *)                         echo "" ;;
  esac
}

fetch() {
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --proto '=https' --tlsv1.2 "$1" -o "$2"
  elif command -v wget >/dev/null 2>&1; then
    wget -q --https-only -O "$2" "$1"
  else
    die "need curl or wget to download a release archive"
  fi
}

sha256_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    die "need sha256sum or shasum to verify the release archive"
  fi
}

is_masil_checkout() {
  [ -f "$1/Makefile" ] && [ -f "$1/scripts/build.py" ] && [ -f "$1/agent/Cargo.toml" ]
}

install_from_source() {
  local checkout=""
  if [ -n "${BASH_SOURCE[0]:-}" ] && [ -f "${BASH_SOURCE[0]}" ]; then
    local script_dir
    script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd)"
    if is_masil_checkout "$script_dir"; then
      checkout="$script_dir"
    fi
  fi
  if [ -z "$checkout" ] && is_masil_checkout "$PWD"; then
    checkout="$PWD"
  fi
  if [ -z "$checkout" ]; then
    die "Source build requires a masil git checkout.
  Clone it and re-run:
    git clone https://github.com/$REPO_SLUG && cd masil && ./install.sh --build"
  fi
  for tool in make python3 cargo; do
    command -v "$tool" >/dev/null 2>&1 || die "Source build requires '$tool' on PATH."
  done
  info "Building masil and masil-agent from source in $checkout…"
  (cd "$checkout" && make all && make install PREFIX="$PREFIX")
  ok "Built and installed into $PREFIX"
}

install_extracted_bundle() {
  local bundle_dir="$1"
  local bin_dir="$PREFIX/bin"
  local man_dir="$PREFIX/share/man/man1"
  local share_dir="$PREFIX/share/masil"

  mkdir -p "$bin_dir" "$man_dir" "$share_dir"

  # Stage both binaries next to their targets and atomically replace.
  for bin in masil masil-agent; do
    [ -f "$bundle_dir/bin/$bin" ] || die "Archive is missing bin/$bin"
    local staged="$bin_dir/.$bin.new.$$"
    install -m 0755 "$bundle_dir/bin/$bin" "$staged"
    if [ "$OS" = Darwin ]; then
      xattr -d com.apple.quarantine "$staged" 2>/dev/null || true
    fi
    mv -f "$staged" "$bin_dir/$bin"
  done

  if [ -f "$bundle_dir/share/man/man1/masil.1" ]; then
    install -m 0644 "$bundle_dir/share/man/man1/masil.1" "$man_dir/masil.1"
  fi

  for doc in LICENSE NOTICE THIRD-PARTY-LICENSES.txt README.txt dependencies.txt build.json; do
    if [ -f "$bundle_dir/$doc" ]; then
      install -m 0644 "$bundle_dir/$doc" "$share_dir/$doc"
    fi
  done
  for rel in core/COPYING core/UPSTREAM.json core/compat/yyjson/LICENSE agent/src/manifests/LICENSE; do
    if [ -f "$bundle_dir/$rel" ]; then
      mkdir -p "$share_dir/$(dirname "$rel")"
      install -m 0644 "$bundle_dir/$rel" "$share_dir/$rel"
    fi
  done

  ok "Installed masil + masil-agent -> $bin_dir"
}

download_release() {
  local target="$1"
  local ver="${MASIL_VERSION:-latest}"
  local base="https://github.com/$REPO_SLUG/releases"
  local sums_url archive_url archive expected_sha actual_sha tmp

  if [ "$ver" = latest ]; then
    sums_url="$base/latest/download/SHA256SUMS"
  else
    case "$ver" in
      v*) ;;
      *) ver="v$ver" ;;
    esac
    sums_url="$base/download/$ver/SHA256SUMS"
  fi

  tmp="$(mktemp -d)"
  DOWNLOAD_TMP="$tmp"

  info "Fetching release checksums ($ver)…"
  if ! fetch "$sums_url" "$tmp/SHA256SUMS"; then
    die "Could not fetch SHA256SUMS from $sums_url.
  If no binary release has been published yet, install via Homebrew, Nix, or build from source:
    brew install Ochichan/tap/masil
    nix run github:$REPO_SLUG
    git clone https://github.com/$REPO_SLUG && cd masil && ./install.sh --build"
  fi

  local match
  if [ "$ver" = latest ]; then
    match="$(awk -v suffix="-${target}.tar.gz" '
      {
        f = $2; sub(/^\*/, "", f)
        if (f ~ /^masil-v/ && substr(f, length(f) - length(suffix) + 1) == suffix) {
          print $1 " " f
          exit
        }
      }
    ' "$tmp/SHA256SUMS")"
  else
    local want_file="masil-${ver}-${target}.tar.gz"
    match="$(awk -v want="$want_file" '
      {
        f = $2; sub(/^\*/, "", f)
        if (f == want) {
          print $1 " " f
          exit
        }
      }
    ' "$tmp/SHA256SUMS")"
  fi

  [ -n "$match" ] || die "Release $ver has no archive for target '$target' in SHA256SUMS"
  expected_sha="${match%% *}"
  archive="${match#* }"

  if [ "$ver" = latest ]; then
    archive_url="$base/latest/download/$archive"
  else
    archive_url="$base/download/$ver/$archive"
  fi

  info "Downloading $archive…"
  fetch "$archive_url" "$tmp/$archive" || die "Download failed: $archive_url"

  actual_sha="$(sha256_file "$tmp/$archive")"
  if [ "$actual_sha" != "$expected_sha" ]; then
    die "SHA-256 mismatch for $archive (expected $expected_sha, got $actual_sha) — aborting"
  fi
  ok "SHA-256 verified ($archive)"

  info "Extracting…"
  mkdir -p "$tmp/extract"
  tar -xzf "$tmp/$archive" -C "$tmp/extract"
  local bundle_dir
  bundle_dir="$(find "$tmp/extract" -mindepth 1 -maxdepth 1 -type d | head -n 1)"
  [ -n "$bundle_dir" ] || die "Unexpected archive layout in $archive"

  install_extracted_bundle "$bundle_dir"

  rm -rf "$tmp"
  DOWNLOAD_TMP=""
}

if [ "$FORCE_BUILD" -eq 1 ]; then
  install_from_source
else
  target="$(release_target_for_platform)"
  if [ -n "$target" ]; then
    download_release "$target"
  else
    warn "No prebuilt archive for $OS/$ARCH — attempting source build."
    install_from_source
  fi
fi

INSTALL_DIR="$PREFIX/bin"

# --- ensure INSTALL_DIR is on PATH -----------------------------------------------------
on_path() { case ":${PATH:-}:" in *":$1:"*) return 0 ;; *) return 1 ;; esac; }

path_line="export PATH=\"$INSTALL_DIR:\$PATH\""
if on_path "$INSTALL_DIR"; then
  ok "$INSTALL_DIR is already on your PATH"
elif [ "$NO_MODIFY_PATH" -eq 1 ]; then
  warn "$INSTALL_DIR is not on your PATH (--no-modify-path set). Add it yourself:"
  printf '    %s\n' "$path_line"
else
  warn "$INSTALL_DIR is not on your PATH yet."
  _shell="${SHELL:-/bin/sh}"
  case "${_shell##*/}" in
    zsh)  rc="$HOME/.zshrc" ;;
    bash) [ "$OS" = Darwin ] && rc="$HOME/.bash_profile" || rc="$HOME/.bashrc" ;;
    *)    rc="$HOME/.profile" ;;
  esac
  if [ -f "$rc" ] && grep -qsF '# >>> masil >>>' "$rc"; then
    rc_tmp="$(mktemp)"
    awk '/# >>> masil >>>/{skip=1} skip && /# <<< masil <<</{skip=0; next} !skip' "$rc" > "$rc_tmp" \
      && cat "$rc_tmp" > "$rc" && rm -f "$rc_tmp"
  fi
  {
    printf '\n# >>> masil >>>\n'
    printf '# added by masil install.sh — remove this block to undo\n'
    printf '%s\n' "$path_line"
    printf '# <<< masil <<<\n'
  } >> "$rc"
  ok "Added $INSTALL_DIR to PATH in $rc"
  info "Activate it now (or open a new terminal):"
  printf '    %s\n' "$path_line"
fi

# --- preflight shared libraries --------------------------------------------------------
if "$INSTALL_DIR/masil" -V >/dev/null 2>&1 && "$INSTALL_DIR/masil-agent" --version >/dev/null 2>&1; then
  ok "Runtime check passed ($("$INSTALL_DIR/masil-agent" --version))"
else
  warn "masil could not load its runtime shared libraries (libevent, ncurses, utf8proc, jemalloc)."
  if [ "$OS" = Darwin ]; then
    warn "Install them with Homebrew:  brew install libevent ncurses utf8proc jemalloc"
  else
    warn "On Ubuntu 24.04 / Debian:    sudo apt-get install libevent-core-2.1-7t64 libncursesw6 libtinfo6 libutf8proc3 libjemalloc2"
    warn "On other Linux distros:      build from source (./install.sh --build), use Nix (nix run github:$REPO_SLUG), or AUR (yay -S masil)"
  fi
fi

printf '\n'
ok "Done. Start a workspace with:  ${c_info}masil${c_off}  (or run ${c_info}masil-agent doctor${c_off})"
