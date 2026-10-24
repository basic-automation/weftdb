#!/usr/bin/env python3
"""Tests for scripts/crates/check.py.

Run from the repository root:

    python3 -m unittest discover -s scripts/crates -p 'test_*.py' -v

Every test builds a throwaway four-crate workspace in a temporary directory — `leaf` with
no dependencies, `middle` depending on it, `top` depending on that, and `weft-bench`, a
`publish = false` binary — mutates one thing about it, and asserts the sentence the checker
answers with. No cargo, no network, nothing outside the temporary directory.
"""

from __future__ import annotations

import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import check  # noqa: E402

REPO_ROOT = HERE.parent.parent

ROOT_MANIFEST = """[workspace]
resolver = "2"
members = ["leaf", "middle", "top", "weft-bench"]

[workspace.package]
license = "MIT OR Apache-2.0"
authors = ["Justin Icenhour"]
repository = "https://github.com/basic-automation/weftdb"
readme = "README.md"
edition = "2021"
rust-version = "1.95"

[workspace.dependencies]
leaf = { path = "./leaf", version = "0.1.0" }
middle = { path = "./middle", version = "0.1.0" }
top = { path = "./top", version = "0.1.0" }
"""

BINARY_MANIFEST = """[package]
name = "weft-bench"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true
authors.workspace = true
license.workspace = true
repository.workspace = true
readme.workspace = true
description = "Fixture binary crate."
publish = false

[dependencies]
top = { workspace = true }
"""


def member_manifest(name: str, dependencies: str = "") -> str:
    body = f"""[package]
name = "{name}"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true
authors.workspace = true
license.workspace = true
repository.workspace = true
documentation = "https://docs.rs/{name}"
readme = "README.md"
description = "Fixture crate {name}."
keywords = ["timeseries", "fixture"]
categories = ["database-implementations"]
"""
    return body + "\n" + dependencies


class WorkspaceFixture(unittest.TestCase):
    """A workspace that passes every rule, which each test breaks in exactly one way."""

    def setUp(self) -> None:
        self._directories = []
        self.repo = self.build()

    def tearDown(self) -> None:
        for directory in self._directories:
            directory.cleanup()

    def build(self) -> Path:
        """Write the clean fixture into a fresh temporary repository and return its root."""
        holder = tempfile.TemporaryDirectory()
        self._directories.append(holder)
        repo = Path(holder.name)
        (repo / "Cargo.toml").write_text(ROOT_MANIFEST)
        (repo / "README.md").write_text("# Fixture workspace\n")
        for name, dependencies in (
            ("leaf", ""),
            ("middle", '[dependencies]\nleaf = { workspace = true }\n'),
            ("top", '[dependencies]\nmiddle = { workspace = true }\n'),
        ):
            directory = repo / name
            directory.mkdir()
            (directory / "Cargo.toml").write_text(member_manifest(name, dependencies))
            (directory / "README.md").write_text(f"# {name}\n")
            (directory / "LICENSE-MIT").write_text("MIT\n")
            (directory / "LICENSE-APACHE").write_text("Apache-2.0\n")
            (directory / "src").mkdir()
            (directory / "src" / "lib.rs").write_text("")
        binary = repo / "weft-bench"
        binary.mkdir()
        (binary / "Cargo.toml").write_text(BINARY_MANIFEST)
        (binary / "src").mkdir()
        (binary / "src" / "main.rs").write_text("fn main() {}\n")
        return repo

    def mutate(self, relative: str, old: str, new: str) -> None:
        text = (self.repo / relative).read_text()
        self.assertIn(old, text, f"{relative} has no {old!r} to replace")
        (self.repo / relative).write_text(text.replace(old, new, 1))

    def append(self, relative: str, text: str) -> None:
        """Add a table at the end of a manifest, where a table cannot be mistaken for another."""
        (self.repo / relative).write_text((self.repo / relative).read_text() + text)

    def problems(self) -> list[str]:
        return check.check(self.repo)

    def assertProblem(self, fragment: str) -> None:
        problems = self.problems()
        self.assertTrue(problems, f"expected a problem naming {fragment!r}, found none")
        self.assertTrue(any(fragment in problem for problem in problems), f"{fragment!r} not in {problems}")

    def assertClean(self) -> None:
        self.assertEqual(self.problems(), [])


class TestCleanFixture(WorkspaceFixture):
    def test_the_default_fixture_passes(self):
        self.assertClean()

    def test_publish_order_is_dependencies_first(self):
        self.assertEqual(check.publish_order(check.read_workspace(self.repo)), ["leaf", "middle", "top"])

    def test_a_path_inherited_from_the_workspace_is_found(self):
        """`{ workspace = true }` carries a path relative to the root, not to the member.

        Read it against the member directory and a workspace dependency looks like an
        external crate, which silently disables every dependency rule below.
        """
        state = check.read_workspace(self.repo)
        self.assertEqual([dep["owner"] for dep in state["crates"]["middle"]["internal_deps"]], ["leaf"])
        self.assertEqual([dep["owner"] for dep in state["crates"]["weft-bench"]["internal_deps"]], ["top"])

    def test_a_target_specific_dependency_is_seen(self):
        self.mutate("top/Cargo.toml", "[dependencies]", "[target.'cfg(unix)'.dev-dependencies]")
        state = check.read_workspace(self.repo)
        dependency = state["crates"]["top"]["internal_deps"][0]
        self.assertEqual(dependency["owner"], "middle")
        self.assertEqual(dependency["via"], "target.dev-dependencies")


class TestVersions(WorkspaceFixture):
    def test_lockstep_version_is_a_problem(self):
        self.mutate("middle/Cargo.toml", 'version = "0.1.0"', "version.workspace = true")
        self.assertProblem("version inherits the workspace")

    def test_a_workspace_version_nothing_reads_is_a_problem(self):
        self.mutate("Cargo.toml", "[workspace.package]\nlicense", '[workspace.package]\nversion = "0.1.0"\nlicense')
        self.assertProblem("sets version")

    def test_each_crate_may_be_at_its_own_version(self):
        self.mutate("leaf/Cargo.toml", 'version = "0.1.0"', 'version = "0.1.4"')
        self.mutate("leaf/src/lib.rs", "", "// patched on its own\n")
        self.assertClean()

    def test_a_version_that_is_not_a_version_is_reported_not_raised(self):
        self.mutate("leaf/Cargo.toml", 'version = "0.1.0"', 'version = "0.1"')
        self.mutate("leaf/Cargo.toml", 'version = "0.1"', 'version = "nightly"')
        self.assertProblem("nightly")


class TestEdition(WorkspaceFixture):
    def test_a_crate_on_its_own_edition_is_a_problem(self):
        self.mutate("middle/Cargo.toml", "edition.workspace = true", 'edition = "2024"')
        self.assertProblem('edition = "2024"')

    def test_a_missing_workspace_edition_is_a_problem(self):
        self.mutate("Cargo.toml", 'edition = "2021"\n', "")
        self.assertProblem("must set edition")

    def test_a_member_that_declares_nothing_is_a_problem(self):
        self.mutate("top/Cargo.toml", "edition.workspace = true\n", "")
        self.assertProblem("declares no edition")


class TestPublishSplit(WorkspaceFixture):
    def test_a_binary_that_forgets_publish_false_is_a_problem(self):
        self.mutate("weft-bench/Cargo.toml", "publish = false\n", "")
        self.assertProblem("does not set publish = false")

    def test_a_library_marked_unpublishable_is_a_problem(self):
        self.mutate("middle/Cargo.toml", 'description = "Fixture crate middle."', 'description = "Fixture crate middle."\npublish = false')
        self.assertProblem("never reach crates.io")

    def test_a_published_crate_cannot_depend_on_a_binary(self):
        self.mutate("top/Cargo.toml", "[dependencies]\nmiddle = { workspace = true }", "[dependencies]\nmiddle = { workspace = true }\nweft-bench = { path = \"../weft-bench\", version = \"0.1.0\" }")
        self.assertProblem("cannot depend on a crate crates.io does not carry")

    def test_a_dev_dependency_on_a_binary_counts_too(self):
        """`cargo publish` builds the package, so a dev-dependency has to resolve as well."""
        self.mutate("top/Cargo.toml", "[dependencies]", "[dev-dependencies]\nweft-bench = { path = \"../weft-bench\" }\n\n[dependencies]")
        self.assertProblem("cannot depend on a crate crates.io does not carry")


class TestInternalDependencies(WorkspaceFixture):
    def test_a_versioned_path_dependency_is_required(self):
        self.mutate("top/Cargo.toml", "middle = { workspace = true }", 'middle = { path = "../middle" }')
        self.assertProblem("needs a version requirement")

    def test_the_requirement_must_be_one_the_member_satisfies(self):
        self.mutate("leaf/Cargo.toml", 'version = "0.1.0"', 'version = "0.2.0"')
        self.assertProblem("does not satisfy")

    def test_a_patch_release_still_satisfies_the_requirement(self):
        self.mutate("leaf/Cargo.toml", 'version = "0.1.0"', 'version = "0.1.9"')
        self.assertClean()

    def test_an_inherited_row_supplies_the_requirement(self):
        self.mutate("middle/Cargo.toml", 'leaf = { workspace = true }', 'leaf = { workspace = true, features = ["serde"] }')
        self.assertClean()

    def test_a_dependency_cycle_is_reported(self):
        # `top` already depends on `middle`; this makes the leaf depend on `top`.
        self.append("leaf/Cargo.toml", '\n[dependencies]\ntop = { workspace = true }\n')
        self.assertProblem("dependency cycle")


class TestPackageHygiene(WorkspaceFixture):
    def test_a_readme_inherited_from_the_root_is_a_problem(self):
        self.mutate("middle/Cargo.toml", 'readme = "README.md"', "readme.workspace = true")
        self.assertProblem("monorepo README")

    def test_a_readme_outside_the_package_is_a_problem(self):
        self.mutate("middle/Cargo.toml", 'readme = "README.md"', 'readme = "../README.md"')
        self.assertProblem("outside the package directory")

    def test_a_readme_that_is_not_there_is_a_problem(self):
        (self.repo / "top" / "README.md").unlink()
        self.assertProblem("names no file")

    def test_the_documentation_link_is_the_docs_rs_page(self):
        self.mutate("top/Cargo.toml", 'documentation = "https://docs.rs/top"', 'documentation = "https://example.com"')
        self.assertProblem("https://docs.rs/top")

    def test_a_license_with_no_text_behind_it_is_a_problem(self):
        (self.repo / "leaf" / "LICENSE-APACHE").unlink()
        self.assertProblem("no LICENSE-APACHE")

    def test_a_missing_description_is_a_problem(self):
        self.mutate("middle/Cargo.toml", 'description = "Fixture crate middle."\n', "")
        self.assertProblem("no description")

    def test_too_many_keywords_is_a_problem(self):
        self.mutate("leaf/Cargo.toml", 'keywords = ["timeseries", "fixture"]', 'keywords = ["a", "b", "c", "d", "e", "f"]')
        self.assertProblem("crates.io keeps 5")

    def test_the_binary_is_exempt_from_registry_hygiene(self):
        self.mutate("weft-bench/Cargo.toml", 'description = "Fixture binary crate."\n', "")
        self.assertClean()


class TestPublishScript(WorkspaceFixture):
    def test_a_second_list_of_crates_is_a_problem(self):
        scripts = self.repo / "scripts"
        scripts.mkdir()
        (scripts / "publish.sh").write_text("#!/usr/bin/env bash\nCRATES=(leaf middle top)\n")
        self.assertProblem("keeps its own CRATES array")

    def test_the_real_publish_script_has_no_list(self):
        problems = check.check_publish_script(check.read_workspace(REPO_ROOT))
        self.assertEqual(problems, [])


class TestSemver(unittest.TestCase):
    def test_requirements_a_crate_can_carry(self):
        cases = [
            ("^0.1.0", "0.1.0", True),
            ("^0.1.0", "0.1.4", True),
            ("^0.1.0", "0.2.0", False),
            ("^0.1.0", "1.0.0", False),
            ("^1.2.3", "1.9.0", True),
            ("^1.2.3", "2.0.0", False),
            ("^0.0.3", "0.0.3", True),
            ("^0.0.3", "0.0.4", False),
            ("~1.2.3", "1.2.9", True),
            ("~1.2.3", "1.3.0", False),
            ("~1", "1.9.9", True),
            ("=1.2.3", "1.2.3", True),
            ("=1.2.3", "1.2.4", False),
            (">=1.2, <2.0", "1.4.0", True),
            (">=1.2, <2.0", "2.0.0", False),
            ("1.2.3", "1.5.0", True),
            ("*", "9.9.9", True),
        ]
        for requirement, version, expected in cases:
            satisfied, why = check.req_satisfied(requirement, version)
            self.assertEqual(satisfied, expected, f"{version} vs {requirement!r}: {why}")

    def test_a_prerelease_needs_a_prerelease_requirement(self):
        self.assertEqual(check.req_satisfied("^1.0.0", "1.0.0-rc.1"), (False, "1.0.0-rc.1 is a pre-release and '^1.0.0' names none"))
        self.assertTrue(check.req_satisfied("^1.0.0-rc.1", "1.0.0-rc.1")[0])

    def test_an_unreadable_requirement_is_explained(self):
        satisfied, why = check.req_satisfied("blob", "1.0.0")
        self.assertFalse(satisfied)
        self.assertIn("blob", why)


class TestThisRepository(unittest.TestCase):
    def test_the_workspace_passes_its_own_rules(self):
        self.assertEqual(check.check(REPO_ROOT), [])

    def test_every_publishable_member_is_in_the_publish_order_once(self):
        state = check.read_workspace(REPO_ROOT)
        ordered = check.publish_order(state)
        publishable = {name for name in state["crates"] if check.is_published(state["crates"][name])}
        self.assertEqual(sorted(ordered), sorted(publishable))
        self.assertEqual(len(ordered), len(publishable), "the publish order repeated a crate")

    def test_the_binaries_are_the_ones_the_release_archive_carries(self):
        state = check.read_workspace(REPO_ROOT)
        self.assertEqual({name for name, crate in state["crates"].items() if not check.is_published(crate)}, check.BINARY_CRATES)


if __name__ == "__main__":
    unittest.main()
