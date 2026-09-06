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
    def test_only_plain_semver_tags(self):
        self.assertEqual(release.version_tuple("v1.20.3"), (1, 20, 3))
        for tag in ("main", "v1.2", "v01.2.3", "v1.2.3-preview", "v1.2.3\n", "v1.2.3;echo injected"):
            with self.subTest(tag=tag), self.assertRaises(ValueError):
                release.version_tuple(tag)

    def test_cannot_republish_or_downgrade(self):
        for tag in ("v1.2.3", "v1.2.2", "v0.99.0"):
            with self.subTest(tag=tag), self.assertRaises(ValueError):
                release.validate_release(tag, [{"tag_name": "v1.2.3", "draft": False, "prerelease": False}])
        release.validate_release("v1.2.4", [{"tag_name": "v1.2.4", "draft": True, "prerelease": False}])
        release.validate_release("v1.2.4", [{"tag_name": "nightly", "draft": False, "prerelease": True}])

    def test_matches_website_artifact_contract(self):
        self.assertEqual(release.asset_names("windows", "aarch64"), ("Zed-aarch64.exe", "zed-remote-server-windows-aarch64.zip"))
        self.assertEqual(release.asset_names("linux", "x86_64"), ("zed-linux-x86_64.tar.gz", "zed-remote-server-linux-x86_64.gz"))
        self.assertEqual(release.asset_names("macos", "aarch64"), ("Zed-aarch64.dmg", "zed-remote-server-macos-aarch64.gz"))
        self.assertEqual(len(release.expected_assets()), 12)

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

                with patch.object(release, "validate_tag"), patch.object(release, "release_list", return_value=[]), patch.object(release, "run", side_effect=github):
                    release.publish("v1.2.3")
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
                with patch.object(release, "validate_tag"), patch.object(release, "release_list", return_value=[]), patch.object(release, "run") as github:
                    with self.assertRaises(ValueError):
                        release.publish("v1.2.3")
                    github.assert_not_called()
            finally:
                os.chdir(previous)


if __name__ == "__main__":
    unittest.main()
