#!/usr/bin/env python3
"""Tests for scripts/config/check-schema.py.

Run from the repository root:

    python3 -m unittest discover -s scripts/config -p 'test_*.py' -v
"""

from __future__ import annotations

import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import check_schema  # noqa: E402


class RegistryExtraction(unittest.TestCase):
    def test_extract_varspec_names(self):
        with tempfile.NamedTemporaryFile(mode="w", suffix=".rs", delete=False) as f:
            f.write('VarSpec { name: "WEFT_SERVER_ADDR", tier: Tier::Stable },\n')
            f.write('VarSpec { name: "WEFT_AUDIT_FILE", tier: Tier::Stable },\n')
            p = Path(f.name)
        try:
            names = check_schema.extract_registry_names(p)
            self.assertEqual(names, {"WEFT_SERVER_ADDR", "WEFT_AUDIT_FILE"})
        finally:
            p.unlink(missing_ok=True)

    def test_extract_empty(self):
        with tempfile.NamedTemporaryFile(mode="w", suffix=".rs", delete=False) as f:
            f.write("fn main() {}\n")
            p = Path(f.name)
        try:
            names = check_schema.extract_registry_names(p)
            self.assertEqual(names, set())
        finally:
            p.unlink(missing_ok=True)

    def test_extract_doc_names(self):
        with tempfile.NamedTemporaryFile(mode="w", suffix=".md", delete=False) as f:
            f.write("| `WEFT_SERVER_ADDR` | The address | host:port | 127.0.0.1:8080 | srv |\n")
            f.write("| `WEFT_DATA_DIR` | Data dir | path | ~/.weftdb/data | db |\n")
            p = Path(f.name)
        try:
            names = check_schema.extract_doc_names(p)
            self.assertEqual(names, {"WEFT_SERVER_ADDR", "WEFT_DATA_DIR"})
        finally:
            p.unlink(missing_ok=True)

    def test_extract_doc_skips_lowercase(self):
        with tempfile.NamedTemporaryFile(mode="w", suffix=".md", delete=False) as f:
            f.write("| `some_other` | desc | | | |\n")
            p = Path(f.name)
        try:
            names = check_schema.extract_doc_names(p)
            self.assertEqual(names, set())
        finally:
            p.unlink(missing_ok=True)


class SchemaVersion(unittest.TestCase):
    def test_doc_version_present(self):
        with tempfile.NamedTemporaryFile(mode="w", suffix=".md", delete=False) as f:
            f.write("prefix\nschema_version: 3\nsuffix\n")
            p = Path(f.name)
        try:
            self.assertEqual(check_schema.extract_doc_schema_version(p), "3")
        finally:
            p.unlink(missing_ok=True)

    def test_doc_version_missing(self):
        with tempfile.NamedTemporaryFile(mode="w", suffix=".md", delete=False) as f:
            f.write("no version\n")
            p = Path(f.name)
        try:
            self.assertIsNone(check_schema.extract_doc_schema_version(p))
        finally:
            p.unlink(missing_ok=True)

    def test_toml_version_present(self):
        with tempfile.NamedTemporaryFile(mode="w", suffix=".toml", delete=False) as f:
            f.write("[schema]\nversion = 7\n")
            p = Path(f.name)
        try:
            self.assertEqual(check_schema.extract_toml_schema_version(p), "7")
        finally:
            p.unlink(missing_ok=True)

    def test_toml_version_missing_file(self):
        self.assertIsNone(check_schema.extract_toml_schema_version(Path("/nonexistent/schema.toml")))

    def test_toml_version_missing_key(self):
        with tempfile.NamedTemporaryFile(mode="w", suffix=".toml", delete=False) as f:
            f.write("[other]\nfoo = 1\n")
            p = Path(f.name)
        try:
            self.assertIsNone(check_schema.extract_toml_schema_version(p))
        finally:
            p.unlink(missing_ok=True)


class Verify(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.doc = Path(self.tmp.name) / "doc.md"
        self.reg = Path(self.tmp.name) / "settings.rs"
        self.schema_toml = Path(self.tmp.name) / "schema.toml"

    def tearDown(self):
        self.tmp.cleanup()

    def given_registry(self, *names: str) -> None:
        self.reg.write_text("\n".join(f'VarSpec {{ name: "{n}", tier: Tier::Tuning }},' for n in names))

    def given_doc(self, *names: str, version: str | None = "1") -> None:
        lines = [f"| `{n}` | sum | type | default | reader |" for n in names]
        if version is not None:
            lines.insert(0, f"schema_version: {version}")
        self.doc.write_text("\n".join(lines))

    def given_schema_toml(self, version: int | None = 1) -> None:
        if version is not None:
            self.schema_toml.write_text(f"[schema]\nversion = {version}\n")

    def test_clean(self):
        self.given_registry("WEFT_A", "WEFT_B")
        self.given_doc("WEFT_A", "WEFT_B")
        self.given_schema_toml()
        self.assertEqual(check_schema.verify(self.doc, self.reg, self.schema_toml), [])

    def test_undocumented_variable(self):
        self.given_registry("WEFT_A", "WEFT_B")
        self.given_doc("WEFT_A")
        self.given_schema_toml()
        probs = check_schema.verify(self.doc, self.reg, self.schema_toml)
        self.assertTrue(any("WEFT_B" in p and "not documented" in p for p in probs))

    def test_orphan_doc_name(self):
        self.given_registry("WEFT_A")
        self.given_doc("WEFT_A", "WEFT_EXTRA")
        self.given_schema_toml()
        probs = check_schema.verify(self.doc, self.reg, self.schema_toml)
        self.assertTrue(any("WEFT_EXTRA" in p and "not registered" in p for p in probs))

    def test_version_mismatch(self):
        self.given_registry("WEFT_A")
        self.given_doc("WEFT_A", version="1")
        self.given_schema_toml(version=2)
        probs = check_schema.verify(self.doc, self.reg, self.schema_toml)
        self.assertTrue(any("schema_version mismatch" in p for p in probs))

    def test_missing_doc_version(self):
        self.given_registry("WEFT_A")
        self.given_doc("WEFT_A", version=None)
        self.given_schema_toml()
        probs = check_schema.verify(self.doc, self.reg, self.schema_toml)
        self.assertTrue(any("schema_version" in p for p in probs))

    def test_missing_toml(self):
        self.given_registry("WEFT_A")
        self.given_doc("WEFT_A", version="1")
        probs = check_schema.verify(self.doc, self.reg, self.schema_toml)
        self.assertTrue(any("schema.toml" in p for p in probs))

    def test_clean_with_no_version_gap(self):
        self.given_registry("WEFT_A")
        self.given_doc("WEFT_A", version="1")
        self.given_schema_toml(version=1)
        self.assertEqual(check_schema.verify(self.doc, self.reg, self.schema_toml), [])


if __name__ == "__main__":
    unittest.main()
