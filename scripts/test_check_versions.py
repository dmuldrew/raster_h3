"""Negative-path tests for the release metadata guard."""

from copy import deepcopy
import json
from pathlib import Path
import subprocess
import unittest

from check_versions import check, field


class VersionChecks(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.root = Path(__file__).resolve().parents[1]
        cls.metadata = json.loads(subprocess.check_output(
            ["cargo", "metadata", "--no-deps", "--format-version=1", "--locked"],
            cwd=cls.root, text=True,
        ))
        cls.version = next(p["version"] for p in cls.metadata["packages"]
                           if p["name"] == "raster-h3-core")

    def test_matching_release(self):
        self.assertEqual(check(self.root, self.metadata, f"refs/tags/v{self.version}"), [])

    def test_mismatched_tag(self):
        self.assertTrue(any("release tag" in e for e in check(self.root, self.metadata, "refs/tags/v99.0.0")))

    def test_mismatched_package_and_path_requirement(self):
        metadata = deepcopy(self.metadata)
        cli = next(p for p in metadata["packages"] if p["name"] == "raster-h3-cli")
        cli["version"] = "99.0.0"
        next(d for d in cli["dependencies"] if d.get("path"))["req"] = "*"
        errors = check(self.root, metadata)
        self.assertTrue(any("version 99.0.0" in e for e in errors))
        self.assertTrue(any("must require" in e for e in errors))

    def test_private_adapter_policy(self):
        metadata = deepcopy(self.metadata)
        next(p for p in metadata["packages"] if p["name"] == "raster-h3-python")["publish"] = None
        self.assertTrue(any("publishable" in e for e in check(self.root, metadata)))

    def test_missing_and_duplicate_fields_fail_closed(self):
        for text in ["", 'version = "1"\nversion = "2"']:
            with self.assertRaises(ValueError):
                field(text, r'^version = "([^"]+)"')


if __name__ == "__main__":
    unittest.main()
