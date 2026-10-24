#!/usr/bin/env python3
"""Per-crate release metadata: versions, one edition, the publish split, package hygiene.

FRE-1 (ROADMAP.md "Execution waves" W1) makes each published crate carry the metadata
crates.io renders and the release pipeline reads, and then keeps it true. Nothing here
needs cargo or the network: the manifests are the record, so this reads them with
`tomllib` and asks the questions a registry would ask. A rule that only `cargo publish`
could catch is caught a merge cycle earlier, on a PR, instead of at a tag.

Subcommands:

    check             assert every rule and exit 1 on any problem (offline; CI runs this)
    publish-order     print the publishable crates, dependencies first (scripts/publish.sh
                      derives its list and its order from this, so it cannot drift)
    show              one line per member: name, version, edition, published or not

The rules, and the failure each one prevents:

  * **Every crate names its own `version`.** `version.workspace = true` releases the
    workspace in lockstep, which means a patch to one library bumps the other nine and
    ships nine releases whose only change is the number. The release tag is
    `weft-server`'s version (scripts/release/verify.sh), so per-crate versions cost the
    release pipeline nothing; `[workspace.package].version` must stay absent, or the root
    file becomes a second truth that nothing reads.
  * **One edition, inherited.** A crate on a different edition is a crate whose `unsafe`
    rules, `impl Trait` capture and prelude differ from its neighbours' while all ten
    compile together. `[workspace.package].edition` is the single place, so FRE-22's move
    to 2024 is one edit and one review.
  * **The publish split is the whole manifest.** Exactly `BINARY_CRATES` sets
    `publish = false`; every other member is publishable. A tenth crate that forgets the
    field is silently publishable, and one that adds it is silently missing from
    `scripts/publish.sh` — which is why publish.sh is not allowed to carry a list.
  * **A published crate's `readme` is its own file.** crates.io renders that file as the
    crate's page. Inheriting the workspace README puts the server's and the TUI's story on
    a library's page, and reaches outside the package directory.
  * **`documentation` is the docs.rs page of that name**, so the crate links to the docs
    built from the version it is served with, not to a search box.
  * **An internal path dependency names a version, and that version exists.** crates.io
    resolves a path dependency by its `version` requirement; without one the upload fails
    at the registry with a message about the workspace layout, and a requirement the
    member's own version does not satisfy fails the same way. Both are caught here.
  * **A published crate does not depend on an unpublished one** — in any kind, including
    dev: `cargo publish` builds the package, so a dev-dependency on `weft-bench` cannot
    resolve from the registry either.

`check` prints one `crates: <problem>` line per problem and exits 1 if there is any.
"""

from __future__ import annotations

import argparse
import re
import sys
import tomllib
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent

# The crates that ship as GitHub release binaries and never as registry crates
# (RELEASING.md "Which crates are published"). Adding a crate here is a decision that the
# release artifacts change; adding a member anywhere else makes it publishable on purpose.
BINARY_CRATES = {"weft-bench", "weft-server", "weft-tui"}

# crates.io caps both lists at five, and an empty one is a crate nobody can find.
MAX_KEYWORDS = 5
MAX_CATEGORIES = 5

# The dependency tables a package can carry, top level and per target.
DEP_TABLES = ("dependencies", "dev-dependencies", "build-dependencies")


class ManifestError(Exception):
    """A manifest cannot be read at all. The message names the file and what is wrong."""


# --------------------------------------------------------------------------------------
# reading the manifests
# --------------------------------------------------------------------------------------


def load_toml(path: Path) -> dict:
    try:
        with path.open("rb") as handle:
            return tomllib.load(handle)
    except tomllib.TOMLDecodeError as error:
        raise ManifestError(f"{display(path)}: {error}") from error


def display(path: Path, repo: Path = REPO) -> str:
    """A path as a reader refers to it: under the repository when it is, absolute when not."""
    try:
        return str(path.relative_to(repo))
    except ValueError:
        return str(path)


def workspace_default(package_table: dict, field: str) -> object:
    """The `[workspace.package]` value a member inherits with `<field>.workspace = true`."""
    return package_table.get(field)


def inherited(table: dict, field: str, default: object) -> tuple[bool, object]:
    """`(declared, value)` for a package field that may come from `[workspace.package]`.

    `declared` is False when the member says nothing at all — which is different from
    saying `false`, and matters for `publish`, whose absence means "publishable".
    """
    if field not in table:
        return False, default
    value = table[field]
    if isinstance(value, dict) and value.get("workspace") is True:
        return True, default
    return True, value


def dependency_entries(manifest: dict) -> list[tuple[str, str, dict]]:
    """Every dependency of a crate as `(table, name, entry)`, target-specific included.

    Takes the whole manifest, not its `[package]` table: cargo spells dependencies as
    top-level `[dependencies]`, `[dev-dependencies]`, `[build-dependencies]` and
    `[target.<cfg>.…]` tables, and a dev-dependency is as much a packaging problem as a
    normal one.
    """
    entries: list[tuple[str, str, dict]] = []
    for table_name in DEP_TABLES:
        table = manifest.get(table_name) or {}
        for name, entry in table.items():
            entries.append((table_name, name, entry))
    for target in (manifest.get("target") or {}).values():
        for table_name in DEP_TABLES:
            for name, entry in (target.get(table_name) or {}).items():
                entries.append((f"target.{table_name}", name, entry))
    return entries


def resolve_dependency(name: str, entry: object, workspace_dependencies: dict) -> tuple[str, dict, bool]:
    """Flatten a dependency entry to `(crate name, fields, path from the workspace root)`.

    The fields of the `[workspace.dependencies]` row it inherits are included, so a version
    requirement written once at the root still counts as written. A `path` inherited from
    that row is relative to the workspace root, while one written in the member's own
    manifest is relative to the member's directory; read them the same way and a workspace
    dependency looks like an external crate.
    """
    if isinstance(entry, str):
        return name, {"version": entry}, False
    if not isinstance(entry, dict):
        raise ManifestError(f"dependency {name}: expected a string or a table")
    fields = dict(entry)
    if fields.get("workspace") is True:
        inherited_row = workspace_dependencies.get(fields.get("package", name))
        if inherited_row is None:
            raise ManifestError(f"dependency {name}: `workspace = true` but `[workspace.dependencies]` has no such row")
        base = {"version": inherited_row} if isinstance(inherited_row, str) else dict(inherited_row)
        inherited_path = "path" in base and "path" not in fields
        base.update({key: value for key, value in fields.items() if key != "workspace"})
        return fields.get("package", name), base, inherited_path
    return fields.get("package", name), fields, False


def read_workspace(repo: Path = REPO) -> dict:
    """The workspace as one dict: members by name, with each member's manifest fields."""
    root = load_toml(repo / "Cargo.toml")
    workspace = root.get("workspace") or {}
    members = list(workspace.get("members") or [])
    workspace_dependencies = workspace.get("dependencies") or {}
    workspace_package = workspace.get("package") or {}

    crates: dict[str, dict] = {}
    for member in members:
        directory = repo / member
        manifest = load_toml(directory / "Cargo.toml")
        package = manifest.get("package")
        if not isinstance(package, dict) or "name" not in package:
            raise ManifestError(f"{display(directory / 'Cargo.toml', repo)}: [package] with a name is required")
        name = package["name"]
        if name in crates:
            raise ManifestError(f"{display(directory / 'Cargo.toml', repo)}: crate {name} is already a member")
        crates[name] = {
            "name": name,
            "member": member,
            "directory": directory,
            "manifest_path": directory / "Cargo.toml",
            "package": package,
            "manifest": manifest,
        }
    # Map an internal path dependency to its member by the directory it names.
    by_directory = {crate["directory"].resolve(): crate for crate in crates.values()}
    for crate in crates.values():
        internal: list[dict] = []
        for table_name, entry_name, entry in dependency_entries(crate["manifest"]):
            crate_name, fields, path_from_root = resolve_dependency(entry_name, entry, workspace_dependencies)
            path = fields.get("path")
            if not isinstance(path, str):
                continue
            # Inherited from `[workspace.dependencies]`: relative to the root. Written here:
            # relative to this crate.
            base = repo if path_from_root else crate["directory"]
            target = (base / path).resolve()
            owner = by_directory.get(target)
            if owner is None:
                continue  # a patched vendored crate under patches/, not a workspace member
            internal.append({"via": table_name, "name": crate_name, "fields": fields, "owner": owner["name"]})
        crate["internal_deps"] = internal
    return {
        "repo": repo,
        "root": root,
        "workspace": workspace,
        "members": members,
        # The root manifest's member order, which is the order every rule reports in.
        "members_order": list(crates),
        "crates": crates,
        "workspace_package": workspace_package,
        "workspace_dependencies": workspace_dependencies,
    }


# --------------------------------------------------------------------------------------
# semver, enough of it for a version requirement
# --------------------------------------------------------------------------------------


def parse_version(text: str) -> tuple[int, int, int, str]:
    """`"1.2.3-rc.1"` to `(1, 2, 3, "rc.1")`; a missing part is 0, as cargo reads it."""
    parts = text.strip().split("-", 1)
    pre = parts[1] if len(parts) > 1 else ""
    numbers = parts[0].split(".")
    if not numbers or any(not part.isdigit() for part in numbers[:3]) or len(numbers) > 3:
        raise ValueError(f"not a version this checker reads: {text!r}")
    while len(numbers) < 3:
        numbers.append("0")
    return (int(numbers[0]), int(numbers[1]), int(numbers[2]), pre)


def _cmp(left: tuple[int, int, int, str], right: tuple[int, int, int, str]) -> int:
    if left[:3] != right[:3]:
        return -1 if left[:3] < right[:3] else 1
    # A pre-release sorts before the release it precedes, and a bare version has none.
    if left[3] == right[3]:
        return 0
    if not left[3]:
        return 1
    if not right[3]:
        return -1
    return -1 if left[3] < right[3] else 1


def _satisfies(comparator: str, version: tuple[int, int, int, str]) -> bool:
    """One comma-separated comparator against a parsed version."""
    comparator = comparator.strip()
    if comparator in ("*", ""):
        return True
    for operator in (">=", "<=", "=", "~", "^", ">", "<"):
        if not comparator.startswith(operator):
            continue
        bound = parse_version(comparator[len(operator) :])
        order = _cmp(version, bound)
        if operator == "=":
            return order == 0
        if operator == ">":
            return order > 0
        if operator == "<":
            return order < 0
        if operator == ">=":
            return order >= 0
        if operator == "<=":
            return order <= 0
        if operator == "^":
            return _satisfies_caret(comparator, bound, version)
        return _satisfies_tilde(comparator, bound, version)
    # No operator at all: cargo reads a bare requirement as a caret.
    return _satisfies_caret(comparator, parse_version(comparator), version)


def _written_parts(comparator: str) -> int:
    """How many of major.minor.patch the comparator spells, which is what fixes the floor."""
    body = comparator.lstrip("=<>~^").split("-", 1)[0]
    return len([part for part in body.split(".") if part != ""])


def _satisfies_caret(comparator: str, bound: tuple[int, int, int, str], version: tuple[int, int, int, str]) -> bool:
    """A caret allows the leftmost non-zero part to stand still.

    `^1.2.3` is `<2.0.0`, `^0.1.2` is `<0.2.0`, `^0.0.3` is `<0.0.4`, and a requirement
    that spells nothing non-zero — `^0`, `^0.0`, `^0.0.0` — stops where the part it left
    out says it should: `<1.0.0` for `^0`, `<0.1.0` for the others.
    """
    if _cmp(version, bound) < 0:
        return False
    written = _written_parts(comparator)
    major, minor, patch, _ = bound
    if major != 0 or written == 1:
        ceiling = (major + 1, 0, 0, "")
    elif minor != 0:
        ceiling = (0, minor + 1, 0, "")
    elif patch != 0:
        ceiling = (0, 0, patch + 1, "")
    else:
        ceiling = (0, 1, 0, "")
    return _cmp(version, ceiling) < 0


def _satisfies_tilde(comparator: str, bound: tuple[int, int, int, str], version: tuple[int, int, int, str]) -> bool:
    """`~1.2.3` and `~1.2` are `>=…, <1.3.0`; `~1` is `<2.0.0`."""
    if _cmp(version, bound) < 0:
        return False
    major, minor, _, _ = bound
    ceiling = (major, minor + 1, 0, "") if _written_parts(comparator) >= 2 else (major + 1, 0, 0, "")
    return _cmp(version, ceiling) < 0


def req_satisfied(requirement: str, version: str) -> tuple[bool, str]:
    """Does a crate's version satisfy a dependency requirement? `""` explains a no."""
    parsed = parse_version(version)
    comparators = [part for part in requirement.replace(" ", ",").split(",") if part.strip()]
    if any(part.strip() in ("*", "") for part in comparators):
        return True, ""
    if parsed[3] and not any("-" in part.lstrip("=<>~^") for part in comparators):
        # Cargo: a pre-release satisfies only a comparator that carries a pre-release too.
        return False, f"{version} is a pre-release and {requirement!r} names none"
    for comparator in comparators:
        try:
            ok = _satisfies(comparator, parsed)
        except ValueError as error:
            return False, str(error)
        if not ok:
            return False, f"{version} does not satisfy {requirement!r}"
    return True, ""


# --------------------------------------------------------------------------------------
# the rules
# --------------------------------------------------------------------------------------


def is_published(crate: dict) -> bool:
    """`publish = false` is the only way a member opts out of the registry."""
    return crate["package"].get("publish", True) is not False


def check_publish_split(state: dict) -> list[str]:
    problems: list[str] = []
    declared_binary = {name for name, crate in state["crates"].items() if not is_published(crate)}
    for name in sorted(declared_binary - BINARY_CRATES):
        problems.append(f"{state['crates'][name]['member']} sets publish = false but is not a binary crate: it would never reach crates.io and never reach a release archive either — add it to BINARY_CRATES in {display(HERE / 'check.py')} with the reason, or publish it")
    for name in sorted((BINARY_CRATES & set(state["crates"])) - declared_binary):
        problems.append(f"{state['crates'][name]['member']} is listed as a binary crate but does not set publish = false: `cargo publish` would take it — set publish = false, or drop it from BINARY_CRATES")
    return problems


def check_versions(state: dict) -> list[str]:
    problems: list[str] = []
    if "version" in state["workspace_package"]:
        problems.append("Cargo.toml [workspace.package] sets version: with per-crate versions nothing reads it, and the next bump will edit the wrong file — delete it and bump each crate's own [package].version")
    for name in state["members_order"]:
        package = state["crates"][name]["package"]
        version = package.get("version")
        if isinstance(version, dict):
            problems.append(f"{state['crates'][name]['member']}: version inherits the workspace — each crate names its own version, or a patch to this crate bumps every crate in the workspace")
        elif not isinstance(version, str):
            problems.append(f"{state['crates'][name]['member']}: [package] has no version string")
        else:
            try:
                parse_version(version)
            except ValueError as error:
                problems.append(f"{state['crates'][name]['member']}: version {error}")
    return problems


def check_edition(state: dict) -> list[str]:
    problems: list[str] = []
    edition = state["workspace_package"].get("edition")
    if not isinstance(edition, str):
        problems.append("Cargo.toml [workspace.package] must set edition — it is the one place the workspace's edition is written (FRE-22 moves it in one edit)")
        return problems
    for name in state["members_order"]:
        crate = state["crates"][name]
        value = crate["package"].get("edition")
        if isinstance(value, dict) and value.get("workspace") is True:
            continue
        if isinstance(value, str):
            worse = "" if value == edition else f", and it is not the workspace's {edition}"
            problems.append(f"{crate['member']} sets edition = \"{value}\" itself{worse}: a crate on another edition compiles under different rules than its neighbours — say edition.workspace = true")
        else:
            problems.append(f"{crate['member']} declares no edition: say edition.workspace = true and inherit {edition}")
    return problems


def check_published_metadata(state: dict) -> list[str]:
    problems: list[str] = []
    for name in state["members_order"]:
        crate = state["crates"][name]
        if not is_published(crate):
            continue
        package = crate["package"]
        for field in ("license", "repository", "rust-version"):
            if field not in package:
                problems.append(f"{crate['member']}: [package] has no {field} — crates.io shows it and `cargo publish` warns about it")
        description = package.get("description")
        if not isinstance(description, str) or not description.strip():
            problems.append(f"{crate['member']}: [package] has no description — it is the one line a registry search shows")
        for field, limit in (("keywords", MAX_KEYWORDS), ("categories", MAX_CATEGORIES)):
            values = package.get(field)
            if not isinstance(values, list) or not values:
                problems.append(f"{crate['member']}: [package] has no {field} — nothing on crates.io can be searched to this crate")
                continue
            if len(values) > limit:
                problems.append(f"{crate['member']}: {len(values)} {field} but crates.io keeps {limit}")
            elif any(not isinstance(value, str) or not value.strip() for value in values):
                problems.append(f"{crate['member']}: an empty {field[:-1]} entry")
        documentation = package.get("documentation")
        if documentation != f"https://docs.rs/{name}":
            problems.append(f"{crate['member']}: documentation is {documentation!r}, not https://docs.rs/{name} — the registry's doc link should be the page built from this crate")
        problems.extend(check_readme(crate, state))
        problems.extend(check_license_files(crate, state))
    return problems


def check_readme(crate: dict, state: dict) -> list[str]:
    """A published crate's README is its crates.io page, so it must be the crate's own."""
    package = crate["package"]
    value = package.get("readme")
    if value is None or value is False:
        reason = "no readme"
    elif isinstance(value, dict) and value.get("workspace") is True:
        reason = "readme inherited from the workspace root, so the registry page shows the monorepo README — with the server's and the TUI's story on a library's page"
    elif isinstance(value, str):
        path = (crate["directory"] / value).resolve()
        if not path.is_file():
            return [f"{crate['member']}: readme = \"{value}\" names no file (looked at {display(path, state['repo'])}) — cargo drops a missing readme from the package in silence"]
        if crate["directory"].resolve() not in path.parents:
            return [f"{crate['member']}: readme = \"{value}\" is outside the package directory, so it is not part of what the registry serves"]
        return []
    else:
        reason = f"readme = {value!r}"
    if value is not None and value is not False and not isinstance(value, dict):
        return []
    return [f"{crate['member']}: {reason} — set readme = \"README.md\" and write the crate's own page"]


def check_license_files(crate: dict, state: dict) -> list[str]:
    """The `license` expression promises texts the package must actually carry.

    The expression is usually inherited from `[workspace.package]`, so it is resolved
    before it is matched against the files: inherited metadata still has to be true.
    """
    expression = crate["package"].get("license")
    if isinstance(expression, dict):
        expression = state["workspace_package"].get("license")
    if not isinstance(expression, str):
        return []
    want = {"MIT": "LICENSE-MIT", "Apache-2.0": "LICENSE-APACHE"}
    problems = []
    for part in (piece.strip() for piece in expression.replace("/", " OR ").split("OR")):
        filename = want.get(part)
        if filename is None:
            problems.append(f"{crate['member']}: license expression {part!r} is not one this checker maps to a file — extend it deliberately")
        elif not (crate["directory"] / filename).is_file():
            problems.append(f"{crate['member']}: license {part} but no {filename} in the crate directory — the package would carry an expression with no text behind it")
    return problems


def check_internal_dependencies(state: dict) -> list[str]:
    problems: list[str] = []
    for name in state["members_order"]:
        crate = state["crates"][name]
        for dependency in crate["internal_deps"]:
            owner = state["crates"][dependency["owner"]]
            requirement = dependency["fields"].get("version")
            if not is_published(owner) and is_published(crate):
                problems.append(f"{crate['member']} depends on {owner['member']} ({dependency['via']}): a published crate cannot depend on a crate crates.io does not carry — `cargo publish` builds the package and cannot resolve it")
            if not isinstance(requirement, str) or not requirement.strip():
                if is_published(crate):
                    problems.append(f"{crate['member']} -> {owner['name']} ({dependency['via']}): a path dependency of a published crate needs a version requirement — the registry resolves it by version, and the upload fails with a message about the workspace")
                continue
            try:
                owner_version = owner["package"].get("version")
                if not isinstance(owner_version, str):
                    # `check_versions` has already said this crate names no version of its
                    # own; comparing a requirement against a `[workspace.package]` table
                    # would only report the same problem twice, in a stranger voice.
                    continue
                ok, why = req_satisfied(requirement, owner_version)
            except ValueError as error:
                problems.append(f"{crate['member']} -> {owner['name']}: {error}")
                continue
            if not ok:
                problems.append(f"{crate['member']} -> {owner['name']} requires {requirement!r} but that member is {owner_version}: {why} — bump the requirement when you bump the crate")
    return problems


def dependency_cycle(state: dict) -> list[str] | None:
    """A dependency cycle among the publishable crates, as the path of names that closes it."""
    publishable = {name for name in state["crates"] if is_published(state["crates"][name])}
    walking: list[str] = []
    settled: set[str] = set()

    def visit(name: str) -> list[str] | None:
        if name in walking:
            return walking[walking.index(name):] + [name]
        if name in settled:
            return None
        walking.append(name)
        for dependency in state["crates"][name]["internal_deps"]:
            if dependency["owner"] in publishable:
                found = visit(dependency["owner"])
                if found:
                    return found
        walking.pop()
        settled.add(name)
        return None

    for name in state["members_order"]:
        if name in publishable:
            found = visit(name)
            if found:
                return found
    return None


def check_publish_script(state: dict) -> list[str]:
    """publish.sh must ask this script for the list; a second list drifts on the first new crate."""
    script = state["repo"] / "scripts" / "publish.sh"
    if not script.is_file():
        return []
    text = script.read_text(encoding="utf-8")
    # An empty `CRATES=()` that gets filled from `publish-order` is the shape this rule
    # asks for; an array with names in it is the second list.
    if re.search(r"CRATES=\(\s*[^\s)]", text):
        return ["scripts/publish.sh keeps its own CRATES array: a member added without editing it is never published — take the list from `scripts/crates/check.py publish-order`"]
    return []


def publish_order(state: dict) -> list[str]:
    """The publishable crates, every dependency before the crate that needs it.

    Stable: candidates are taken in workspace-member order, so the output is a diff-able
    list rather than whatever a set iteration produced.
    """
    publishable = [name for name in state["members_order"] if is_published(state["crates"][name])]
    remaining = list(publishable)
    ordered: list[str] = []
    while remaining:
        for name in list(remaining):
            needs = {dependency["owner"] for dependency in state["crates"][name]["internal_deps"] if dependency["owner"] in remaining}
            if needs:
                continue
            remaining.remove(name)
            ordered.append(name)
            break
        else:
            # Only a cycle can get here, and a cycle between published crates cannot
            # publish at all, which check_internal_dependencies has already said about.
            return ordered + remaining
    return ordered


def check(repo: Path = REPO) -> list[str]:
    """Every problem with the workspace's release metadata, as `crates: …` sentences."""
    state = read_workspace(repo)
    problems: list[str] = []
    for rule in (check_publish_split, check_versions, check_edition, check_published_metadata, check_internal_dependencies, check_publish_script):
        try:
            problems.extend(rule(state))
        except ManifestError as error:
            problems.append(str(error))
    cycle = dependency_cycle(state)
    if cycle:
        problems.append(f"the publishable crates contain a dependency cycle ({' -> '.join(cycle)}): crates.io cannot take either half first, so none of them can be published")
    return problems


# --------------------------------------------------------------------------------------
# command line
# --------------------------------------------------------------------------------------


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="check.py", description=(__doc__ or "").splitlines()[0])
    parser.add_argument("--repo", type=Path, default=REPO, help="repository root to read (default: this checkout)")
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("check", help="assert every rule and report problems")
    sub.add_parser("publish-order", help="print the publishable crates, dependencies first")
    sub.add_parser("show", help="print one line per member")
    arguments = parser.parse_args(argv)

    if arguments.command == "check":
        problems = check(arguments.repo)
        for problem in problems:
            print(f"crates: {problem}")
        if problems:
            return 1
        state = read_workspace(arguments.repo)
        published = [name for name in state["crates"] if is_published(state["crates"][name])]
        print(f"crates: {len(state['crates'])} members, {len(published)} publishable, edition {state['workspace_package'].get('edition')}, every path dependency versioned")
        return 0

    state = read_workspace(arguments.repo)
    if arguments.command == "publish-order":
        for name in publish_order(state):
            print(name)
        return 0

    for name in state["members_order"]:
        crate = state["crates"][name]
        edition = crate["package"].get("edition")
        edition = state["workspace_package"].get("edition") if isinstance(edition, dict) else edition
        print(f"{name}\t{crate['package'].get('version')}\t{edition}\t{'published' if is_published(crate) else 'binary only'}")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except ManifestError as error:
        print(f"crates: {error}")
        sys.exit(1)
