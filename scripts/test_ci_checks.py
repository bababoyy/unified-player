#!/usr/bin/env python3
"""Regression checks for the inexpensive CI gates; no application build."""

import contextlib
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
from unittest import mock

import check_ejs_assets as ejs
import check_publication_surface as publication


ROOT = Path(__file__).resolve().parents[1]


class CiChecks(unittest.TestCase):
    def test_asset_corruption_and_crlf_are_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory)
            for name in ("manifest.toml", "yt.solver.lib.js", "yt.solver.core.js"):
                shutil.copy(ejs.ASSETS / name, target / name)
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertTrue(ejs.check_assets(target))
                for name in ("yt.solver.lib.js", "yt.solver.core.js"):
                    path = target / name
                    original = path.read_bytes()
                    for corrupted in (original + b"x", original.replace(b"\n", b"\r\n")):
                        path.write_bytes(corrupted)
                        self.assertFalse(ejs.check_assets(target))
                    path.write_bytes(original)

    def test_git_checkout_preserves_hashed_assets_with_autocrlf(self):
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory)
            def git(*args):
                subprocess.run(["git", "-C", directory, *args], check=True, capture_output=True)
            git("init", "-q")
            git("config", "core.autocrlf", "true")
            shutil.copy(ROOT / ".gitattributes", target / ".gitattributes")
            relative = ejs.ASSETS.relative_to(ROOT)
            (target / relative).mkdir(parents=True)
            for name in ("yt.solver.lib.js", "yt.solver.core.js"):
                shutil.copy(ejs.ASSETS / name, target / relative / name)
            git("add", ".gitattributes", str(relative))
            for path in (target / relative).glob("*.js"):
                path.unlink()
            git("checkout-index", "-a", "-f")
            for path in (target / relative).glob("*.js"):
                self.assertEqual(path.read_bytes(), (ejs.ASSETS / path.name).read_bytes())

    def test_tracked_only_never_invokes_cargo_and_still_rejects_paths(self):
        for paths, expected in (([], 0), (["credentials.json"], 1)):
            with (
                mock.patch("sys.argv", ["check", "--tracked-only"]),
                mock.patch.object(publication, "tracked_paths", return_value=paths),
                mock.patch.object(publication, "tracked_size_violations", return_value=0),
                mock.patch.object(publication, "docker_context_policy", return_value=True),
                mock.patch.object(publication, "license_copy_policy", return_value=True),
                mock.patch.object(publication, "package_paths") as packages,
                mock.patch.object(publication, "vendored_notice_policy") as vendored,
                contextlib.redirect_stdout(io.StringIO()),
            ):
                self.assertEqual(publication.main(), expected)
                packages.assert_not_called()
                vendored.assert_not_called()

    def test_rust_profile_arguments_preserve_backend_and_target_boundaries(self):
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory)
            cargo = target / "cargo"
            cargo.write_text('#!/usr/bin/env python3\nimport json,sys\nprint(json.dumps(sys.argv[1:]))\n')
            cargo.chmod(0o755)
            env = dict(os.environ, PATH=directory + os.pathsep + os.environ["PATH"],
                       CARGO_TARGET_DIR=directory + "/target with spaces")
            for profile in ("standard", "ci", "minimal"):
                result = subprocess.run(
                    ["bash", str(ROOT / "scripts/check_rust_profile.sh"), "test", profile],
                    env=env, capture_output=True, text=True, check=True,
                )
                args = json.loads(result.stdout)
                self.assertEqual(args[:4], ["test", "--locked", "--target-dir", env["CARGO_TARGET_DIR"]])
                self.assertEqual(args[4:], ["--no-default-features"] +
                                 ([] if profile == "minimal" else ["--features", profile]))
            result = subprocess.run(
                ["bash", str(ROOT / "scripts/check_rust_profile.sh"), "test", "invalid"],
                env=env, capture_output=True,
            )
            self.assertEqual(result.returncode, 2)
            self.assertEqual(result.stdout, b"")


if __name__ == "__main__":
    unittest.main()
