#!/usr/bin/env python3
"""Unit tests for scripts/release.py supply-chain & packaging helpers."""
import hashlib
import io
import json
from pathlib import Path
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import release  # noqa: E402


def make_archive(dest_dir, tag, target, commit, *, dirty=False, corrupt_sidecar=False):
    prefix = f"masil-{tag}-{target}"
    archive_path = dest_dir / f"{prefix}.tar.gz"
    meta = {"version": tag, "commit": commit, "dirty": dirty, "target": target}
    with tarfile.open(archive_path, "w:gz") as tar:
        for rel, body in (
            ("bin/masil", b"core"),
            ("bin/masil-agent", b"agent"),
            ("build.json", json.dumps(meta).encode()),
            ("THIRD-PARTY-LICENSES.txt", b"licenses\n"),
        ):
            info = tarfile.TarInfo(f"{prefix}/{rel}")
            info.size = len(body)
            info.mode = 0o755 if rel.startswith("bin/") else 0o644
            tar.addfile(info, io.BytesIO(body))
    digest = hashlib.sha256(archive_path.read_bytes()).hexdigest()
    if corrupt_sidecar:
        digest = "0" * 64
    (dest_dir / f"{archive_path.name}.sha256").write_text(
        f"{digest}  {archive_path.name}\n", encoding="utf-8"
    )
    return archive_path


class ReleaseScriptTests(unittest.TestCase):
    def test_validate_versions_matches_all_manifests_and_flake(self):
        found = release.validate_versions()
        self.assertEqual(found, release.version())
        self.assertEqual(release.validate(f"v{found}"), f"v{found}")
        self.assertEqual(release.validate(f"v{found}-rc1"), f"v{found}-rc1")
        with self.assertRaises(SystemExit):
            release.validate("v99.99.99")
        with self.assertRaises(SystemExit):
            release.validate("not-a-tag")

    def test_render_pkgbuild_substitutes_version_and_sha(self):
        rendered = release.render_pkgbuild("v0.1.0", "a" * 64)
        self.assertIn("pkgver=0.1.0", rendered)
        self.assertIn(f"sha256sums=('{'a' * 64}')", rendered)
        self.assertNotIn("__VERSION__", rendered)
        self.assertNotIn("__SHA_SOURCE__", rendered)

    def test_prepare_assembles_all_four_targets_and_verifies_sidecars(self):
        tag = f"v{release.version()}"
        commit = release.command("git", "rev-parse", "HEAD")
        with tempfile.TemporaryDirectory(prefix="masil-release-test-") as tmp:
            out = Path(tmp)
            for target in release.TARGETS:
                make_archive(out, tag, target, commit)
            with patch.object(release, "OUTPUT", out):
                release.prepare(tag)

            sums = (out / "SHA256SUMS").read_text(encoding="utf-8").splitlines()
            recorded = {line.split()[1]: line.split()[0] for line in sums}
            expected_files = {
                *(f"masil-{tag}-{t}.tar.gz" for t in release.TARGETS),
                f"masil-{tag}-source.tar.gz",
                "install.sh",
                "masil.rb",
                "PKGBUILD",
            }
            self.assertEqual(set(recorded), expected_files)
            for name, expected_sha in recorded.items():
                actual_sha = hashlib.sha256((out / name).read_bytes()).hexdigest()
                self.assertEqual(actual_sha, expected_sha, name)

            formula_text = (out / "masil.rb").read_text(encoding="utf-8")
            self.assertIn(f"masil-{tag}-source.tar.gz", formula_text)
            self.assertIn(recorded[f"masil-{tag}-source.tar.gz"], formula_text)
            pkgbuild_text = (out / "PKGBUILD").read_text(encoding="utf-8")
            self.assertIn(recorded[f"masil-{tag}-source.tar.gz"], pkgbuild_text)

    def test_prepare_rejects_corrupt_sidecar(self):
        tag = f"v{release.version()}"
        commit = release.command("git", "rev-parse", "HEAD")
        with tempfile.TemporaryDirectory(prefix="masil-release-bad-") as tmp:
            out = Path(tmp)
            for idx, target in enumerate(release.TARGETS):
                make_archive(out, tag, target, commit, corrupt_sidecar=(idx == 0))
            with patch.object(release, "OUTPUT", out):
                with self.assertRaisesRegex(SystemExit, "checksum mismatch"):
                    release.prepare(tag)


if __name__ == "__main__":
    unittest.main()
