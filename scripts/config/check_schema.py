#!/usr/bin/env python3
"""Check that the configuration schema is complete and versioned.

Two checks against the source of truth (`weft-server/src/settings.rs`):

1. Every variable registered in the Rust registry must be documented in
   `docs/config-schema.md`; every documented variable must be registered.

2. The `schema_version` declared in the document must match the machine-readable
   copy in `config/schema.toml`.

Prints one `config: <problem>` line per problem and exits 1 when any is found.
Works offline, does not shell out to cargo.

Usage:

    python3 scripts/config/check_schema.py [--doc docs/config-schema.md] [--registry weft-server/src/settings.rs]
"""

from __future__ import annotations

import argparse
import re
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent

SCHEMA_TOML = REPO / "config" / "schema.toml"


# Sentinel for the default — lets tests override without patching the module.
_DEFAULT_TOML: Path | None = None


def extract_registry_names(registry_path: Path) -> set[str]:
    """Extract every VarSpec name from settings.rs without compiling Rust."""
    text = registry_path.read_text(encoding="utf-8")
    names = set(re.findall(r'VarSpec\s*\{\s*name:\s*"([^"]+)"', text))
    return names


def extract_doc_names(doc_path: Path) -> set[str]:
    """Extract every env-var name from the markdown tables in config-schema.md."""
    text = doc_path.read_text(encoding="utf-8")
    names = set(re.findall(r'\|\s*`([A-Z][A-Z_0-9]+)`\s*\|', text))
    return names


def extract_doc_schema_version(doc_path: Path) -> str | None:
    """Extract the schema_version from the doc."""
    text = doc_path.read_text(encoding="utf-8")
    m = re.search(r"schema_version:\s*(\d+)", text)
    return m.group(1) if m else None


def extract_toml_schema_version(toml_path: Path) -> str | None:
    """Extract the schema_version from schema.toml."""
    if not toml_path.is_file():
        return None
    import tomllib  # noqa: PLC0415

    data = tomllib.loads(toml_path.read_text(encoding="utf-8"))
    version = data.get("schema", {}).get("version")
    return str(version) if version is not None else None


def verify(doc_path: Path, registry_path: Path, schema_toml: Path | None = None) -> list[str]:
    """Every problem; empty means everything is consistent."""
    problems: list[str] = []

    # ---- Check variable coverage ----
    registry_names = extract_registry_names(registry_path)
    doc_names = extract_doc_names(doc_path)

    for name in sorted(registry_names - doc_names):
        problems.append(f"{name} is registered in {registry_path} but not documented in {doc_path}")

    for name in sorted(doc_names - registry_names):
        problems.append(f"{name} is documented in {doc_path} but not registered in {registry_path}")

    # ---- Check schema version alignment ----
    doc_ver = extract_doc_schema_version(doc_path)
    toml_ver = extract_toml_schema_version(schema_toml if schema_toml is not None else SCHEMA_TOML)

    if doc_ver is None:
        problems.append(f"{doc_path} has no schema_version marker --- add a `schema_version: N` line to the document")
    effectively = schema_toml if schema_toml is not None else SCHEMA_TOML
    if toml_ver is None:
        problems.append(f"{effectively} does not exist or has no schema.version --- create it with a matching version")
    if doc_ver is not None and toml_ver is not None and doc_ver != toml_ver:
        problems.append(f"schema_version mismatch: doc says {doc_ver}, {effectively} says {toml_ver}")

    return problems


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--doc", default=REPO / "docs" / "config-schema.md", type=Path)
    parser.add_argument("--registry", default=REPO / "weft-server" / "src" / "settings.rs", type=Path)
    args = parser.parse_args()

    problems = verify(args.doc, args.registry)
    for problem in problems:
        print(f"config: {problem}")
    if not problems:
        print(f"config: schema is complete and versioned --- {len(extract_registry_names(args.registry))} variables documented")
    return 1 if problems else 0


if __name__ == "__main__":
    raise SystemExit(main())
