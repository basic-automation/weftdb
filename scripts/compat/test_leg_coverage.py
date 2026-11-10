#!/usr/bin/env python3
"""Every declared compat leg must have a runner.

A pin in compat/releases.toml declares which compatibility legs it participates in.
Each declared leg must have an actual test implementation — a placeholder with no
runner is an error (FRE-16). This prevents the cycle that left "rollback" and
"binary" as declared-but-unexecuted legs.

Run from the repository root:

    python3 -m unittest discover -s scripts/compat -p 'test_leg_coverage.py' -v
"""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import releases  # noqa: E402

REPO_ROOT = HERE.parent.parent
PIN_FILE = REPO_ROOT / releases.PIN_FILE

# The set of compat legs that have an actual test implementation.  When a new leg
# gets a real test runner (a CI job that downloads the previous release's binary,
# writes data with the current build, and reads it back with the older binary,
# or equivalent), add it here.
IMPLEMENTED_LEGS: set[str] = {"upgrade"}


class TestLegCoverage(unittest.TestCase):
    """Every declared leg in compat/releases.toml must be implemented by a runner."""

    def test_every_declared_leg_has_a_runner(self):
        """No pin may declare a leg that lacks a test runner."""
        pins = releases.load_pin(PIN_FILE)
        for pin in pins:
            version = pin.get("version", "?")
            declared = set(pin.get("legs", []))
            unimpl = [leg for leg in sorted(declared) if leg not in IMPLEMENTED_LEGS]
            self.assertFalse(
                unimpl,
                f"release {version}: declared leg(s) {unimpl} have no test runner; "
                f"implement a test for them or remove them from the pin. "
                f"Implemented legs: {sorted(IMPLEMENTED_LEGS)}",
            )

    def test_releases_toml_is_not_empty(self):
        """An empty or absent releases.toml must also fail CI."""
        pins = releases.load_pin(PIN_FILE)
        self.assertTrue(
            len(pins) > 0,
            "compat/releases.toml must pin at least one release",
        )
