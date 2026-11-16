#!/usr/bin/env python3
"""Every compatibility leg must have a status, and the status must be checkable.

A `[[release]]` block in `compat/releases.toml` declares which compatibility legs it takes
part in. Two versions of this gate have failed in two different directions, and each
failure was silent because the gate checked only one half of the mapping:

* **FRE-10c** declared legs with no test behind them. The pin named three legs and CI ran
  one; nothing noticed, and FRE-16 removed the two fictional legs from the pin and from
  the format's vocabulary to make the claim honest (`DECISIONS.md`, 2026-11-10, "Rollback
  and binary compatibility: unsupported for 1.0").
* **Issue #762** found the mapping empty in the other direction. The gate's
  `IMPLEMENTED_LEGS` was a set holding one string, and the job meant to execute the
  missing leg existed only as a sentence in a comment. Neither half of the mapping named
  the other, so "the gate checks leg coverage" described a gate that could not see a leg
  from either side.

So the mapping runs both ways here, and it names things rather than leg words. Every leg a
pinned release could be tested on must fall into exactly one of four statuses:

* **declared** — the pin's `legs` array names it. `releases.check_block` admits only names
  from `releases.LEGS`, which is `{"upgrade"}` today because FRE-16's rule is that a pin
  declares only what CI truly executes;
* **runned** (`LEG_RUNNERS`) — a named test or CI job executes it, so its leg must also be
  declared: a runner cannot be quietly dropped from the pin's claim, which is the
  direction the old gate had no words for;
* **substituted** (`SUBSTITUTE_RUNNERS`) — CI runs a stand-in for the leg rather than the
  leg. `rollback` is the case: the runner tampers the store-format marker instead of
  running an older binary. Naming the stand-in keeps the test visible without letting the
  pin claim a rollback CI does not perform, which is exactly the confusion FRE-16 cleared
  up by deleting the name;
* **unexecuted** (`UNEXECUTABLE_LEGS`) — nothing runs it, and the gate carries the why with
  citations to files a reader can open (`binary`).

The cross-checks then hold the categories honest: a leg has at most one status; the claim
tables speak only about pinned releases and every pinned release has claims; every
`path::identifier` resolves to a real file containing that identifier; every stand-in says
what it leaves out and where the prose says so, and the prose must name the stand-in; and
every leg the pin format's own field documentation describes has a status, so a leg cannot
survive in the prose while vanishing from the gate (FRE-16's failure) or hide behind a
runner that pretends to be the real thing.

Run from the repository root:

    python3 -m unittest discover -s scripts/compat -p 'test_*.py' -v

CI runs it in the `compat` job of `.github/workflows/ci.yml`.
"""

from __future__ import annotations

import re
import sys
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import releases  # noqa: E402

REPO_ROOT = HERE.parent.parent
PIN_FILE = REPO_ROOT / releases.PIN_FILE

# A runner is "<repository path>::<identifier that appears in that file>" — a test
# function for a test runner, or a job id for a CI job. A leg may have several runners.
# The identifiers are not decoration: the gate greps for them, so renaming a test without
# updating this table fails CI.
LEG_RUNNERS: dict[str, dict[str, list[str]]] = {
    "0.1.0": {
        # Leg A: open a store that v0.1.0 wrote, and read back what its own server
        # recorded while it owned the files.
        "upgrade": [
            "weftdb/tests/compat_v010.rs::a_v010_store_migrates_forward_and_keeps_reading_its_own_values",
            "weftdb/tests/compat_v010.rs::v010_compat_fre13_upgrade_contract",
        ],
    },
}

# A leg CI covers with a stand-in. The stand-in is named like a runner; `replaces` states
# in one sentence what it leaves out, and `documented_in` names the file that must say so
# out loud — including the stand-in's own identifier, so the prose cannot drift.
SUBSTITUTE_RUNNERS: dict[str, dict[str, dict[str, str]]] = {
    "0.1.0": {
        "rollback": {
            "stand_in": "weftdb/tests/compat_v010.rs::v010_compat_fre13_rollback_after_writes_loses_nothing",
            "replaces": (
                "no older binary is ever pointed at the candidate's store: the test raises "
                "the `STORE_FORMAT` marker's `min_write_layout` to 99, asserts the open is "
                "refused with `StoreError::IncompatibleLayout`, restores the marker and "
                "reads every measurement back"
            ),
            "documented_in": "UPGRADING.md",
        },
    },
}

# A leg with nothing behind it is allowed only as a stated, checkable absence. Each reason
# must cite repository paths, because a reason that cites nothing is a comment, not a claim.
UNEXECUTABLE_LEGS: dict[str, dict[str, str]] = {
    "0.1.0": {
        "binary": (
            "nothing runs a released binary against a store the candidate wrote. The "
            "fetching half landed: `scripts/compat/releases.py fetch` downloads one pinned "
            "archive and refuses it unless its sha256 matches the digest in "
            "`compat/releases.toml`, and the `pinned release archives` job in "
            "`.github/workflows/ci.yml` runs it with `--extract` on every PR, which is what "
            "puts `weft-server` from v0.1.0 onto a runner's disk and re-tests the pin's "
            "claim about bytes each time. Two things still keep the leg from existing. The "
            "first is the tree to run the old process against: a v0.1.0 process classifies a "
            "layout-2 store as layout 1 and writes into it, so a leg needs a scratch copy of "
            "a store the candidate wrote, and the released v0.1.0 binary answers no "
            "`--version` and no `--help` — measured on the archive that job downloads, it "
            "prints its configuration banner and starts listening — so a leg has to drive it "
            "through its environment and its HTTP endpoints rather than through a flag. The "
            "second is a pinned release whose binaries can read the current layout: "
            "`compat/releases.toml` pins only v0.1.0 and its fixture set lives at "
            "`compat/fixtures/v0.1.0`, and a newer block would need a committed fixture set "
            "of its own before `scripts/compat/releases.py verify` would accept it. "
            "`scripts/compat/test_releases.py` pins what the fetch command guarantees (a "
            "digest that does not match, an asset the pin does not carry, a tar member that "
            "tries to escape the destination); `UPGRADING.md` states the same absence for the "
            "rollback leg's binary half."
        ),
    },
}


def _pin_versions(pins: list[dict]) -> list[str]:
    return [str(pin.get("version", "?")) for pin in pins]


def _declared_legs(pin: dict) -> list[str]:
    return sorted(str(leg) for leg in pin.get("legs", []))


def _documented_legs() -> set[str]:
    """The leg names the pin format documents in its own field reference.

    Read out of `compat/releases.toml` rather than restated here, so prose and gate cannot
    drift apart without a failure. The legs are the backticked names the `legs` field's own
    documentation spells out, which run across several comment lines, so the block is
    collected first and then scanned; anchoring the scan to a line start finds only the leg
    that happens to begin a line and silently makes the others invisible to this gate — the
    exact failure the check exists to catch, and the reason `test_the_leg_vocabulary_read_is_complete`
    pins what the scraper is expected to see.
    """
    block: list[str] = []
    inside = False
    for line in PIN_FILE.read_text(encoding="utf-8").splitlines():
        if not inside:
            inside = bool(re.match(r"^#\s+legs\b", line))
            continue
        # The field reference is a comment block; the next field (or a blank line) ends it.
        if not line.startswith("#") or re.match(r"^#\s+[a-z_]+\s{2,}", line):
            break
        block.append(line)
    return set(re.findall(r"`([a-z]+)` \(", " ".join(block)))


def _statuses(version: str) -> dict[str, str]:
    """Which status each leg of a pinned release falls into."""
    status: dict[str, str] = {}
    for table, name in (
        (LEG_RUNNERS.get(version, {}), "runner"),
        (SUBSTITUTE_RUNNERS.get(version, {}), "stand-in"),
        (UNEXECUTABLE_LEGS.get(version, {}), "unexecuted"),
    ):
        for leg in table:
            status[leg] = name
    return status


class TestLegCoverage(unittest.TestCase):
    """A declared leg names a runner, a runner's leg is declared, and every claim resolves."""

    def test_releases_toml_is_not_empty(self) -> None:
        """An empty or absent releases.toml must also fail CI."""
        pins = releases.load_pin(PIN_FILE)
        self.assertTrue(
            len(pins) > 0,
            "compat/releases.toml must pin at least one release",
        )

    def test_every_declared_leg_names_a_runner_or_a_stand_in(self) -> None:
        """No pin may declare a leg that nothing executes and nothing stands in for."""
        for pin in releases.load_pin(PIN_FILE):
            version = str(pin.get("version", "?"))
            status = _statuses(version)
            for leg in _declared_legs(pin):
                self.assertIn(
                    leg,
                    releases.LEGS,
                    f"release {version}: {leg!r} is not one of the legs the pin format "
                    f"knows ({', '.join(sorted(releases.LEGS))})",
                )
                self.assertIn(
                    status.get(leg, ""),
                    ("runner", "stand-in"),
                    f"release {version}: declared leg {leg!r} has neither a runner in "
                    f"LEG_RUNNERS nor a stand-in in SUBSTITUTE_RUNNERS; a pin may not "
                    f"claim a leg nothing executes",
                )

    def test_every_runner_leg_is_declared_by_the_pin(self) -> None:
        """A leg with a runner must be declared, so the pin cannot understate what runs."""
        declared = {
            str(pin.get("version", "?")): set(_declared_legs(pin))
            for pin in releases.load_pin(PIN_FILE)
        }
        for version, runners in sorted(LEG_RUNNERS.items()):
            self.assertIn(
                version,
                declared,
                f"LEG_RUNNERS speaks for release {version}, which compat/releases.toml "
                f"does not pin; the pin is the authority every compat gate reads",
            )
            for leg, runner in sorted(runners.items()):
                self.assertTrue(runner, f"release {version}: leg {leg!r} lists no runner")
                self.assertIn(
                    leg,
                    declared[version],
                    f"release {version}: leg {leg!r} has a runner "
                    f"({', '.join(runner)}) but the pin does not declare it — the pin "
                    f"understates what CI proves, and declaring it is the fix",
                )

    def test_a_stand_in_leg_is_never_declared_as_executed(self) -> None:
        """A leg only a stand-in covers may not be declared, or the claim is a lie."""
        for pin in releases.load_pin(PIN_FILE):
            version = str(pin.get("version", "?"))
            for leg in _declared_legs(pin):
                self.assertNotIn(
                    leg,
                    SUBSTITUTE_RUNNERS.get(version, {}),
                    f"release {version}: leg {leg!r} is declared by the pin but only a "
                    f"stand-in covers it; declare it when a real runner exists, and cite "
                    f"it in LEG_RUNNERS",
                )

    def test_every_leg_has_exactly_one_status(self) -> None:
        """Runner, stand-in and absence are mutually exclusive."""
        for version in sorted(set(LEG_RUNNERS) | set(SUBSTITUTE_RUNNERS) | set(UNEXECUTABLE_LEGS)):
            seen: dict[str, list[str]] = {}
            for table, name in (
                (LEG_RUNNERS.get(version, {}), "LEG_RUNNERS"),
                (SUBSTITUTE_RUNNERS.get(version, {}), "SUBSTITUTE_RUNNERS"),
                (UNEXECUTABLE_LEGS.get(version, {}), "UNEXECUTABLE_LEGS"),
            ):
                for leg in table:
                    seen.setdefault(leg, []).append(name)
            clash = {leg: tables for leg, tables in seen.items() if len(tables) > 1}
            self.assertFalse(
                clash,
                f"release {version}: leg(s) {sorted(clash)} appear in more than one claim "
                f"table {clash}; a leg is runned, substituted or unexecuted, not several",
            )

    def test_the_claim_tables_speak_only_about_pinned_releases(self) -> None:
        """A new pin block cannot arrive with no claims, and claims cannot outrun pins."""
        versions = set(_pin_versions(releases.load_pin(PIN_FILE)))
        tables = (
            (LEG_RUNNERS, "LEG_RUNNERS"),
            (SUBSTITUTE_RUNNERS, "SUBSTITUTE_RUNNERS"),
            (UNEXECUTABLE_LEGS, "UNEXECUTABLE_LEGS"),
        )
        for table, name in tables:
            unknown = sorted(set(table) - versions)
            self.assertFalse(
                unknown,
                f"{name} speaks for release(s) {unknown}, which are not pinned",
            )
        silent = sorted(
            v
            for v in versions
            if not any(v in table for table, _ in tables)
        )
        self.assertFalse(
            silent,
            f"pinned release(s) {silent} appear in no claim table; a pin must say what "
            f"runs its legs, what stands in, or why nothing does",
        )

    def test_every_documented_leg_has_a_status_for_every_pinned_release(self) -> None:
        """A leg the pin format describes cannot be invisible to the gate."""
        documented = _documented_legs()
        self.assertTrue(
            documented,
            "could not read the leg vocabulary out of compat/releases.toml's field "
            "documentation; the format's own comment must list its legs as `name` (...)",
        )
        for version in _pin_versions(releases.load_pin(PIN_FILE)):
            status = _statuses(version)
            for leg in sorted(documented):
                self.assertIn(
                    leg,
                    status,
                    f"release {version}: the leg {leg!r} is documented by "
                    f"compat/releases.toml but has no entry in LEG_RUNNERS, "
                    f"SUBSTITUTE_RUNNERS or UNEXECUTABLE_LEGS; that is how a leg becomes "
                    f"a promise the prose keeps and the gate forgets",
                )

    def test_the_leg_vocabulary_read_is_complete(self) -> None:
        """The scraper must see every leg the prose describes, not just one of them.

        The three names below are what `compat/releases.toml`'s `legs` documentation says
        today. This is not the gate's vocabulary — `_documented_legs()` is — it is a check
        that the scraper still reads it, because a regex that narrows to a single leg would
        keep every other check green while making two documented legs invisible.
        """
        self.assertEqual(
            _documented_legs(),
            {"upgrade", "rollback", "binary"},
            "the leg vocabulary read out of compat/releases.toml changed shape; either the "
            "prose gained, lost or reflowed a leg, and the gate's view of it must be "
            "re-read rather than trusted",
        )

    def test_the_pin_vocabulary_names_only_documented_legs(self) -> None:
        """`releases.LEGS` cannot invent a leg the pin format does not describe."""
        documented = _documented_legs()
        unknown = sorted(releases.LEGS - documented)
        self.assertFalse(
            unknown,
            f"releases.LEGS accepts {unknown}, which compat/releases.toml's own field "
            f"documentation does not describe; document the leg or stop accepting it",
        )

    def test_runners_and_stand_ins_resolve_to_things_that_exist(self) -> None:
        """Each claim names a real file and a real identifier inside it."""
        claims: list[tuple[str, str, str, str]] = []
        for version, runners in LEG_RUNNERS.items():
            for leg, names in runners.items():
                claims.extend((version, leg, runner, "runner") for runner in names)
        for version, subs in SUBSTITUTE_RUNNERS.items():
            for leg, sub in subs.items():
                claims.append((version, leg, sub["stand_in"], "stand-in"))
        for version, leg, claim, kind in sorted(claims):
            path_text, _, identifier = claim.partition("::")
            self.assertTrue(
                path_text and identifier,
                f"release {version}: {kind} {claim!r} for leg {leg!r} is not `path::identifier`",
            )
            path = REPO_ROOT / path_text
            self.assertTrue(
                path.is_file(),
                f"release {version}: {kind} {claim!r} for leg {leg!r} points at "
                f"{path_text}, which is not a file",
            )
            if path.is_file():
                self.assertIn(
                    identifier,
                    path.read_text(encoding="utf-8"),
                    f"release {version}: {identifier!r} does not appear in {path_text}; "
                    f"either it was renamed or the {leg!r} leg lost its {kind}",
                )

    def test_a_stand_in_says_what_it_leaves_out_and_the_prose_says_so(self) -> None:
        """A stand-in must name its own gap and be described where the gap is documented."""
        for version, subs in sorted(SUBSTITUTE_RUNNERS.items()):
            for leg, sub in sorted(subs.items()):
                self.assertTrue(
                    len(sub["replaces"]) > 60,
                    f"release {version}: the {leg!r} stand-in does not say what it leaves "
                    f"out in enough words to be checked",
                )
                doc = REPO_ROOT / sub["documented_in"]
                self.assertTrue(
                    doc.is_file(),
                    f"release {version}: the {leg!r} stand-in points at "
                    f"{sub['documented_in']} for the honest statement, which is not a file",
                )
                if doc.is_file():
                    text = doc.read_text(encoding="utf-8")
                    identifier = sub["stand_in"].partition("::")[2]
                    self.assertIn(
                        identifier,
                        text,
                        f"release {version}: {sub['documented_in']} never names "
                        f"{identifier}, so a reader cannot find out that the {leg!r} leg "
                        f"is covered by a stand-in rather than by the real thing",
                    )
                    self.assertIn(
                        leg,
                        text,
                        f"release {version}: {sub['documented_in']} never mentions the "
                        f"{leg!r} leg at all",
                    )

    def test_absence_reasons_cite_repository_paths_that_exist(self) -> None:
        """An absence must be anchored in files a reader can check, not in prose."""
        pattern = re.compile(r"[A-Za-z0-9._/-]*\.(?:md|rs|toml|py|yml|sh|json)")
        for version, reasons in sorted(UNEXECUTABLE_LEGS.items()):
            for leg, reason in sorted(reasons.items()):
                self.assertTrue(
                    len(reason) > 120,
                    f"release {version}: the reason given for the {leg!r} leg is too "
                    f"short to be a reason",
                )
                cited = [c.strip("/") for c in pattern.findall(reason) if c.strip("/")]
                self.assertTrue(
                    cited,
                    f"release {version}: the reason for the {leg!r} leg cites no "
                    f"repository file, so nothing checks it",
                )
                for cite in cited:
                    self.assertTrue(
                        (REPO_ROOT / cite).exists(),
                        f"release {version}: the reason for the {leg!r} leg cites "
                        f"{cite}, which does not exist",
                    )


if __name__ == "__main__":
    unittest.main()
