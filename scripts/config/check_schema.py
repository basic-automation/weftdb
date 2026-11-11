#!/usr/bin/env python3
"""Check that the configuration schema is complete and versioned.

Checks against the source of truth (`weft-server/src/settings.rs`):

1. Every variable registered in the Rust registry must be documented in
   ``docs/config-schema.md``; every documented variable must be registered.

2. The ``schema_version`` declared in the document must match the machine-readable
   copy in ``config/schema.toml``.

3. The per-section variable counts (lead-in words and the version-table totals)
   must match the actual number of rows in each tier table.

4. Every ``WEFT_*`` name that appears as prose (outside table cells) in
   ``docs/config-schema.md`` or ``CHANGELOG.md`` must be a known registered name.

Prints one ``config: <problem>`` line per problem and exits 1 when any is found.
Works offline, does not shell out to cargo.

Usage::

    python3 scripts/config/check_schema.py
        [--doc docs/config-schema.md]
        [--registry weft-server/src/settings.rs]
        [--changelog CHANGELOG.md]
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

# Map English number-words to their integer values.
# Supports the range 0-99 (compound words like "forty-three" are handled
# separately at the call site).
_NUMBER_WORD_MAP: dict[str, int] = {
    "zero": 0,
    "one": 1,
    "two": 2,
    "three": 3,
    "four": 4,
    "five": 5,
    "six": 6,
    "seven": 7,
    "eight": 8,
    "nine": 9,
    "ten": 10,
    "eleven": 11,
    "twelve": 12,
    "thirteen": 13,
    "fourteen": 14,
    "fifteen": 15,
    "sixteen": 16,
    "seventeen": 17,
    "eighteen": 18,
    "nineteen": 19,
    "twenty": 20,
    "thirty": 30,
    "forty": 40,
    "fifty": 50,
    "sixty": 60,
    "seventy": 70,
    "eighty": 80,
    "ninety": 90,
}

# Suffixes that match wildcard/prefix references (``WEFT_GPU_*``) rather than
# full variable names.  Any extracted name that exactly equals one of these is
# a prose convention, not a real variable reference.
_WILDCARD_PROSE_SUFFIXES: set[str] = {
    "WEFT_GPU_",
    "WEFT_SEGMENT_",
    "WEFT_STORE_",
}

# Names that appear intentionally in prose as examples of what a non-registered
# (wrong or discussed-but-dropped) name looks like.  They are not registry
# errors and should not trigger the prose-name check.
_KNOWN_PROSE_ONLY_NAMES: set[str] = {
    "WEFT_ALLOW_INSECURE_BIND",
    "WEFT_BACKUP_DIRS",
}

# Map tier section headings to the token used in version-table claims.
_TIER_TO_TOKEN: dict[str, str] = {
    "## Stable": "Stable",
    "## Tuning": "Tuning",
    "## Experimental": "Experimental",
    "## Test": "Test",
}


# ── helpers ────────────────────────────────────────────────────────────────


def word_to_int(text: str) -> int | None:
    """Parse an English number-word or compound (e.g. ``'Forty-two'``).

    Handles ``zero`` … ``ninety-nine``.  Returns ``None`` when unparseable.
    """
    cleaned = text.strip().lower().replace("-", " ").replace("_", " ")
    parts = [p for p in cleaned.split() if p]
    if len(parts) == 1:
        return _NUMBER_WORD_MAP.get(parts[0])
    if len(parts) == 2:
        tens = _NUMBER_WORD_MAP.get(parts[0])
        ones = _NUMBER_WORD_MAP.get(parts[1])
        if tens is not None and ones is not None and tens >= 20 and ones < 10:
            return tens + ones
    return None


def _heading_section(doc_text: str, heading: str) -> str:
    """Return the text between *heading* and the next ``##``-level heading."""
    pat = re.compile(
        rf"^{re.escape(heading)}\s*$\n(.*?)(?=^##\s|\Z)",
        re.MULTILINE | re.DOTALL,
    )
    m = pat.search(doc_text)
    return m.group(1).strip() if m else ""


def _count_table_rows(section_text: str) -> int:
    """Count distinct ``| `NAME` |`` table rows in a section body."""
    return len(set(re.findall(r"\| `([A-Z][A-Z_0-9]+)` \|", section_text)))


def _parse_lead_in_count(section_text: str) -> int | None:
    """Parse the int from the lead-in sentence (e.g. ``'Forty-three variables'``)."""
    m = re.search(
        r"([A-Za-z]+(?:[- ][A-Za-z]+)?)\s+variables?\b",
        section_text[:300],
    )
    if not m:
        return None
    return word_to_int(m.group(1))


def _section_variable_counts(doc_text: str) -> dict[str, int]:
    """Return ``{section_heading: actual_table_row_count}`` for each tier."""
    counts: dict[str, int] = {}
    for heading in _TIER_TO_TOKEN:
        sec_text = _heading_section(doc_text, heading)
        if sec_text:
            counts[heading] = _count_table_rows(sec_text)
    return counts


def _extract_doc_table_names(doc_text: str) -> set[str]:
    """Extract names from markdown table cells (the existing check)."""
    return set(re.findall(r'\|\s*`([A-Z][A-Z_0-9]+)`\s*\|', doc_text))


def _extract_prose_names(text: str) -> set[str]:
    """Extract ``WEFT_*`` mentions that appear **outside** table cells.

    Filters out wildcard patterns (e.g. ``WEFT_GPU_*``) which end with
    a bare underscore, and known prose-only names that exist intentionally
    as examples of non-registered names.
    """
    all_names = set(re.findall(r"WEFT_[A-Z0-9_]+", text))
    result: set[str] = set()
    for name in all_names:
        if name in _WILDCARD_PROSE_SUFFIXES:
            continue
        if name.endswith("_") and len(name) > 6:
            continue
        if name in _KNOWN_PROSE_ONLY_NAMES:
            continue
        result.add(name)
    return result


# ── extraction (exported for testing) ──────────────────────────────────────


def extract_registry_names(registry_path: Path) -> set[str]:
    """Extract every VarSpec name from settings.rs without compiling Rust."""
    text = registry_path.read_text(encoding="utf-8")
    names = set(re.findall(r'VarSpec\s*\{\s*name:\s*"([^"]+)"', text))
    return names


def extract_doc_names(doc_path: Path) -> set[str]:
    """Extract every env-var name from the markdown tables in config-schema.md."""
    text = doc_path.read_text(encoding="utf-8")
    names = _extract_doc_table_names(text)
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


# ── verification ────────────────────────────────────────────────────────────


def verify(
    doc_path: Path,
    registry_path: Path,
    schema_toml: Path | None = None,
    changelog_path: Path | None = None,
) -> list[str]:
    """Every problem; empty means everything is consistent."""
    problems: list[str] = []

    doc_text = doc_path.read_text(encoding="utf-8")
    registry_names = extract_registry_names(registry_path)
    doc_table_names = _extract_doc_table_names(doc_text)

    # ---- Check 1: variable coverage (pre-existing) ----
    for name in sorted(registry_names - doc_table_names):
        problems.append(
            f"{name} is registered in {registry_path} but not documented in {doc_path}"
        )

    for name in sorted(doc_table_names - registry_names):
        problems.append(
            f"{name} is documented in {doc_path} but not registered in {registry_path}"
        )

    # ---- Check 2: schema version alignment (pre-existing) ----
    doc_ver = extract_doc_schema_version(doc_path)
    toml_ver = extract_toml_schema_version(
        schema_toml if schema_toml is not None else SCHEMA_TOML
    )

    if doc_ver is None:
        problems.append(
            f"{doc_path} has no schema_version marker --- "
            "add a `schema_version: N` line to the document"
        )
    effectively = schema_toml if schema_toml is not None else SCHEMA_TOML
    if toml_ver is None:
        problems.append(
            f"{effectively} does not exist or has no schema.version --- "
            "create it with a matching version"
        )
    if doc_ver is not None and toml_ver is not None and doc_ver != toml_ver:
        problems.append(
            f"schema_version mismatch: doc says {doc_ver}, "
            f"{effectively} says {toml_ver}"
        )

    # ---- Check 3: tier counts (NEW) ----
    counted = _section_variable_counts(doc_text)
    total_doc = sum(counted.values())

    for heading, actual in sorted(counted.items()):
        token = _TIER_TO_TOKEN.get(heading, heading)
        sec_text = _heading_section(doc_text, heading)
        lead_int = _parse_lead_in_count(sec_text)

        # 3a. Lead-in word vs actual count
        if lead_int is not None and lead_int != actual:
            problems.append(
                f"{token} section lead-in (claims {lead_int}) "
                f"but table has {actual}"
            )

        # 3b. Version-table per-tier totals
        _check_totals_for_tier(doc_text, token, actual, problems)

    # 3c. Overall version-table total
    _check_total_count(doc_text, total_doc, problems)

    # ---- Check 4: prose names (NEW) ----
    doc_prose = _extract_prose_names(doc_text)
    for name in sorted(doc_prose - registry_names):
        # Don't double-report names already caught by the coverage check
        if name in doc_table_names:
            continue
        problems.append(
            f"{name} appears in prose in {doc_path} but is not registered"
        )

    if changelog_path:
        changelog_text = changelog_path.read_text(encoding="utf-8")
        cl_prose = _extract_prose_names(changelog_text)
        for name in sorted(cl_prose - registry_names):
            problems.append(
                f"{name} appears in prose in {changelog_path} "
                "but is not registered"
            )

    return problems


def _check_totals_for_tier(
    doc_text: str, token: str, actual: int, problems: list[str]
) -> None:
    """Verify the *token* tier count in every version-table row."""
    pat = re.compile(
        r"(\d+)\s+"
        + re.escape(token)
        + r"(?:\s+variable[s]?)?(?:\s*[,;)])"
    )
    for m in pat.finditer(doc_text):
        claimed = int(m.group(1))
        if claimed != actual:
            problems.append(
                f"version table claims {claimed} {token} variables, "
                f"but the table has {actual}"
            )


def _check_total_count(
    doc_text: str, actual_total: int, problems: list[str]
) -> None:
    """Verify overall variable totals claimed in version rows."""
    for m in re.finditer(r"(\d+)\s+variables?:", doc_text):
        claimed = int(m.group(1))
        if claimed != actual_total:
            problems.append(
                f"version table claims {claimed} total variables, "
                f"but the sum of tier tables is {actual_total}"
            )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--doc",
        default=REPO / "docs" / "config-schema.md",
        type=Path,
    )
    parser.add_argument(
        "--registry",
        default=REPO / "weft-server" / "src" / "settings.rs",
        type=Path,
    )
    parser.add_argument(
        "--changelog",
        default=REPO / "CHANGELOG.md",
        type=Path,
    )
    args = parser.parse_args()

    problems = verify(args.doc, args.registry, changelog_path=args.changelog)
    for problem in problems:
        print(f"config: {problem}")
    if not problems:
        print(
            f"config: schema is complete and versioned --- "
            f"{len(extract_registry_names(args.registry))} variables documented"
        )
    return 1 if problems else 0


if __name__ == "__main__":
    raise SystemExit(main())
