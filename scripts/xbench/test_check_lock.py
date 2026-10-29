"""Tests for check-lock.py's drift rule."""
import importlib.util
import unittest
from pathlib import Path

spec = importlib.util.spec_from_file_location("check_lock", Path(__file__).with_name("check-lock.py"))
check_lock = importlib.util.module_from_spec(spec)
spec.loader.exec_module(check_lock)


class DriftTest(unittest.TestCase):
    def test_shared_packages_at_equal_versions_pass(self):
        self.assertEqual(check_lock.drift({"serde": {"1.0.1"}}, {"serde": {"1.0.1"}, "duckdb": {"1.2"}}), [])

    def test_a_shared_package_at_another_version_is_drift(self):
        self.assertEqual(check_lock.drift({"serde": {"1.0.1"}}, {"serde": {"1.0.2"}}), ["serde: workspace 1.0.1, weft-xbench 1.0.2"])

    def test_another_major_pulled_in_by_duckdb_is_not_drift(self):
        self.assertEqual(check_lock.drift({"sha2": {"0.10.9"}}, {"sha2": {"0.10.9", "0.11.1"}}), [])

    def test_a_pre_1_0_minor_is_its_own_class_but_a_patch_is_not(self):
        self.assertEqual(check_lock.drift({"rand": {"0.10.0"}}, {"rand": {"0.10.3"}}), ["rand: workspace 0.10.0, weft-xbench 0.10.3"])

    def test_a_subset_of_the_workspace_versions_passes(self):
        self.assertEqual(check_lock.drift({"rand": {"0.8.5", "0.10.0"}}, {"rand": {"0.10.0"}}), [])


if __name__ == "__main__":
    unittest.main()
