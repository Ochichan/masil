# Security Policy

## Reporting a vulnerability

Please report security vulnerabilities privately via
[GitHub private vulnerability reporting](https://github.com/Ochichan/masil/security/advisories/new)
— **not** in a public issue or pull request.

This is a solo-maintained project. You should normally hear back within **7 days**;
confirmed issues are fixed in the next release (or an out-of-band release for anything
severe). Please leave reasonable time for a fix before public disclosure.

## Supported versions

masil is an actively developed project. Only the **latest release** (and `main`) receives
security fixes — older versions are not patched retroactively.

## Scope

Especially relevant areas given what masil handles:

- **End-to-end encrypted remote gateway & relay (`crates/masil-wire`, `relay/`, `mobile/`)**:
  Noise NK handshake, pairing token expiry/single-use enforcement, device revocation,
  and relay envelope isolation.
- **Local plugin runtime (`agent/src/plugin/`)**: FD 3 scope enforcement (`read` / `act` /
  `admin`), human-only approval boundaries, and plugin path/manifest validation.
- **Agent sandbox (`agent/src/sandbox/`)**: macOS Seatbelt and Linux bubblewrap
  read/write isolation and secret-path masking.
- **Local IPC and operation store (`core/`, `agent/src/managed/`)**: Unix socket
  permissions, `O_NOFOLLOW` / `SQLITE_OPEN_NOFOLLOW` symlink checks, and durable prompt
  deduplication.
- **Release pipeline and packaging**: Homebrew tap, AUR package, Nix flake, `install.sh`,
  checksums, and GitHub build-provenance attestations.

Non-security crashes and UI bugs belong in regular GitHub Issues.

## Remote access — threat model

- **Opt-in only**: Ordinary `masil` usage starts no remote daemon and opens no listening
  network port until you explicitly configure `masil-agent remote setup`.
- **The relay is untrusted**: `masil-relay` only routes opaque Noise NK encrypted
  envelopes between paired peers and cannot read prompts, diffs, files, or terminal
  output. It can observe connection metadata (peer IDs, timestamps, packet sizes) and
  deny availability; self-host `masil-relay` or connect directly over Tailscale/LAN if
  metadata privacy matters.
- **Paired devices are trusted until revoked**: Pairing uses a short-lived, single-use
  code. Revoke a lost device immediately from `masil-agent remote setup` (Paired devices).

## Verifying release artifacts

Every release publishes `SHA256SUMS` and GitHub build-provenance attestations for its
archives, source tarball, `install.sh`, and Homebrew formula:

```sh
# 1. Verify SHA-256 checksums (artifact and SHA256SUMS in the same directory):
sha256sum -c --ignore-missing SHA256SUMS        # macOS: shasum -a 256 -c --ignore-missing SHA256SUMS

# 2. Verify GitHub build provenance (proves the bytes came from this repo's release workflow):
gh attestation verify masil-vX.Y.Z-linux-x86_64.tar.gz --repo Ochichan/masil
```

To avoid running an installer fetched from a moving branch, pin `install.sh` to a release:

```sh
curl -fsSL https://github.com/Ochichan/masil/releases/latest/download/install.sh | bash
```
