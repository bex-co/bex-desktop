import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("bex_release", Path(__file__).with_name("bex-release.py"))
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)


class ReleaseTests(unittest.TestCase):
    def test_only_bex_revision_tags(self):
        self.assertEqual(release.version_tuple("bex-v1.20.3-bex.1"), (1, 20, 3, 1))
        for tag in ("main", "v1.2", "bex-v01.2.3-bex.1", "bex-v1.2.3-bex.1-preview", "bex-v1.2.3-bex.1\n", "bex-v1.2.3-bex.1;echo injected"):
            with self.subTest(tag=tag), self.assertRaises(ValueError):
                release.version_tuple(tag)

    def test_cannot_republish_or_downgrade(self):
        for tag in ("bex-v1.2.3-bex.1", "bex-v1.2.2-bex.1", "bex-v0.99.0-bex.1"):
            with self.subTest(tag=tag), self.assertRaises(ValueError):
                release.validate_release(tag, [{"tag_name": "bex-v1.2.3-bex.1", "draft": False, "prerelease": False}])
        release.validate_release("bex-v1.2.4-bex.1", [{"tag_name": "bex-v1.2.4-bex.1", "draft": True, "prerelease": False}])
        release.validate_release("bex-v1.2.4-bex.1", [{"tag_name": "nightly", "draft": False, "prerelease": True}])

    def test_revision_ordering_and_native_limits(self):
        previous = [{"tag_name": "bex-v1.2.3-bex.2", "draft": False, "prerelease": False}]
        release.validate_release("bex-v1.2.3-bex.10", previous)
        release.validate_release("bex-v1.2.4-bex.1", previous)
        for tag in ("v1.2.3", "bex-v1.2.3-bex.0", "bex-v1.2.3-bex.01", "bex-v1.2.3-bex.10000"):
            with self.subTest(tag=tag), self.assertRaises(ValueError):
                release.version_tuple(tag)
        with self.assertRaises(ValueError):
            release.validate_release("bex-v1.2.3-bex.1", previous)

    def test_validates_declared_upstream_baseline(self):
        package = {
            "version": "1.20.0-bex.1",
            "metadata": {"bex-upstream": {"version": "1.20.0", "commit": "upstream-commit"}},
        }

        def git(*arguments):
            if arguments[1] == "show":
                return '[package]\nversion = "1.20.0"\n'
            return "release-commit"

        with patch.object(release.tomllib, "load", return_value={"package": package}), patch.object(release, "run", side_effect=git) as run:
            self.assertEqual(release.validate_tag("bex-v1.20.0-bex.1"), "1.20.0-bex.1")
            run.assert_any_call("git", "merge-base", "--is-ancestor", "upstream-commit", "release-commit")
            package["metadata"]["bex-upstream"]["version"] = "1.19.0"
            with self.assertRaisesRegex(ValueError, "retain the declared upstream version"):
                release.validate_tag("bex-v1.20.0-bex.1")
            package["version"] = "1.19.0-bex.1"
            with self.assertRaisesRegex(ValueError, "Upstream commit does not match"):
                release.validate_tag("bex-v1.19.0-bex.1")

    def test_prepares_native_versions_without_losing_revision(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            channel = directory / "crates/zed"
            channel.mkdir(parents=True)
            environment = directory / "environment"
            previous = Path.cwd()
            try:
                os.chdir(directory)
                with patch.object(release, "validate_tag", return_value="1.20.3-bex.10"), patch.dict(os.environ, {"GITHUB_ENV": str(environment)}):
                    release.prepare("bex-v1.20.3-bex.10")
                self.assertEqual((channel / "RELEASE_CHANNEL").read_text(), "stable\n")
                self.assertEqual(environment.read_text().splitlines(), [
                    "RELEASE_VERSION=1.20.3-bex.10", "ZED_RELEASE_CHANNEL=stable",
                    "BEX_WINDOWS_PACKAGE_VERSION=1.20.3.10",
                    "BEX_MACOS_BUNDLE_VERSION=10", "BEX_UPSTREAM_VERSION=1.20.3",
                ])
            finally:
                os.chdir(previous)

    def test_matches_website_artifact_contract(self):
        self.assertEqual(release.asset_names("windows", "aarch64"), ("Zed-aarch64.exe", "zed-remote-server-windows-aarch64.zip"))
        self.assertEqual(release.asset_names("linux", "x86_64"), ("zed-linux-x86_64.tar.gz", "zed-remote-server-linux-x86_64.gz"))
        self.assertEqual(release.asset_names("macos", "aarch64"), ("Zed-aarch64.dmg", "zed-remote-server-macos-aarch64.gz"))
        self.assertEqual(len(release.expected_assets()), 12)

    def test_collects_linux_installer_from_bundle_output(self):
        with tempfile.TemporaryDirectory() as temporary:
            previous = Path.cwd()
            try:
                os.chdir(temporary)
                Path("target/release").mkdir(parents=True)
                Path("target/release/zed-linux-x86_64.tar.gz").write_bytes(b"installer")
                Path("target/zed-remote-server-linux-x86_64.gz").write_bytes(b"sidecar")
                release.collect("linux", "x86_64")
                self.assertEqual(Path("release-artifacts/zed-linux-x86_64.tar.gz").read_bytes(), b"installer")
                self.assertEqual(Path("release-artifacts/zed-remote-server-linux-x86_64.gz").read_bytes(), b"sidecar")
            finally:
                os.chdir(previous)

    def test_requires_all_nonempty_artifacts(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            for name in release.expected_assets():
                (directory / name).write_bytes(b"signed artifact")
            self.assertEqual(len(release.verify_artifacts(directory)), 12)
            artifact = directory / "Zed-aarch64.dmg"
            artifact.write_bytes(b"")
            with self.assertRaisesRegex(ValueError, "Empty"):
                release.verify_artifacts(directory)
            artifact.unlink()
            with self.assertRaisesRegex(ValueError, "missing"):
                release.verify_artifacts(directory)

    def test_signing_preflight_names_missing_fields_without_leaking_values(self):
        with patch.dict(os.environ, {"MACOS_CERTIFICATE": "secret-value"}, clear=True):
            with self.assertRaises(ValueError) as error:
                release.check_signing()
            self.assertIn("MACOS_SIGNING_IDENTITY", str(error.exception))
            self.assertNotIn("secret-value", str(error.exception))

    def test_publishes_only_after_complete_upload(self):
        with tempfile.TemporaryDirectory() as temporary:
            previous = Path.cwd()
            try:
                os.chdir(temporary)
                directory = Path("release-artifacts")
                directory.mkdir()
                for name in release.expected_assets():
                    (directory / name).write_bytes(b"signed artifact")
                calls = []

                def github(*arguments):
                    calls.append(arguments)
                    if arguments[:2] == ("gh", "api"):
                        return json.dumps({"draft": True, "assets": [
                            {"name": path.name, "size": path.stat().st_size, "state": "uploaded"}
                            for path in directory.iterdir()
                        ]})
                    return ""

                with patch.object(release, "upstream_notes", return_value="Upstream base: Zed 1.2.3"), patch.object(release, "validate_tag"), patch.object(release, "release_list", return_value=[]), patch.object(release, "run", side_effect=github):
                    release.publish("bex-v1.2.3-bex.1")
                self.assertEqual(calls[0][:3], ("gh", "release", "create"))
                self.assertIn("--draft", calls[0])
                self.assertEqual(calls[-1][:3], ("gh", "release", "edit"))
                self.assertIn("--draft=false", calls[-1])
                self.assertIn("--latest", calls[-1])
            finally:
                os.chdir(previous)

    def test_incomplete_build_never_creates_a_release(self):
        with tempfile.TemporaryDirectory() as temporary:
            previous = Path.cwd()
            try:
                os.chdir(temporary)
                Path("release-artifacts").mkdir()
                with patch.object(release, "upstream_notes", return_value="Upstream base: Zed 1.2.3"), patch.object(release, "validate_tag"), patch.object(release, "release_list", return_value=[]), patch.object(release, "run") as github:
                    with self.assertRaises(ValueError):
                        release.publish("bex-v1.2.3-bex.1")
                    github.assert_not_called()
            finally:
                os.chdir(previous)


if __name__ == "__main__":
    unittest.main()
