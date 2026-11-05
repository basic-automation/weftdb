"""Tests for check_exec_bits.py: which lines are commands, and what counts as a violation."""
import importlib.util
import unittest
from pathlib import Path

spec = importlib.util.spec_from_file_location(
    "check_exec_bits", Path(__file__).with_name("check_exec_bits.py")
)
assert spec is not None and spec.loader is not None
check_exec_bits = importlib.util.module_from_spec(spec)
spec.loader.exec_module(check_exec_bits)


def calls(text: str):
    return check_exec_bits.direct_script_calls({"ci.yml": text})


def judge(found, executable=(), tree=None, parents=None):
    """Report `found` as if only `tree` scripts exist, `parents` directories exist, and
    `executable` ones carry the bit."""
    tree = {c[2] for c in found} if tree is None else tree
    parents = {str(Path(script).parent) for script in tree} if parents is None else parents
    return check_exec_bits.problems(
        found,
        lambda script: script in set(executable),
        lambda script: script in tree,
        lambda script: str(Path(script).parent) in parents,
    )


class CommandHeadTest(unittest.TestCase):
    def test_an_inline_run_step_that_starts_with_a_script_is_a_direct_call(self):
        text = "      - name: R client\n        run: scripts/clients/test-r.sh target/debug/weft-server\n"
        self.assertEqual(calls(text), [("ci.yml", 2, "scripts/clients/test-r.sh")])

    def test_a_block_scalar_body_counts_at_its_own_line(self):
        text = (
            "      - run: |\n"
            "          set -euo pipefail\n"
            "          scripts/packaging/smoke-container.sh \\\n"
            "            --tag x\n"
        )
        self.assertEqual(calls(text), [("ci.yml", 3, "scripts/packaging/smoke-container.sh")])

    def test_a_folded_argument_is_not_a_command(self):
        text = (
            "      - run: |\n"
            "          shellcheck scripts/release/*.sh scripts/compat/*.sh \\\n"
            "            scripts/licenses/*.sh scripts/clients/*.sh\n"
        )
        self.assertEqual(calls(text), [])

    def test_a_paths_filter_entry_is_not_a_command(self):
        text = (
            "on:\n"
            "  pull_request:\n"
            "    paths:\n"
            "      - scripts/crash-matrix.sh\n"
            "      - .github/workflows/crash-matrix.yml\n"
        )
        self.assertEqual(calls(text), [])

    def test_a_job_or_step_id_named_run_is_not_a_command(self):
        text = "    runs-on: ubuntu-latest\n    steps:\n      - run: cargo test -p weftdb --locked\n"
        self.assertEqual(calls(text), [])

    def test_an_interpreted_script_is_not_a_direct_call(self):
        self.assertEqual(calls("      - run: python3 scripts/crates/check.py check\n"), [])
        self.assertEqual(calls("      - run: bash scripts/crash-matrix.sh --all\n"), [])

    def test_an_explicit_dot_slash_counts(self):
        self.assertEqual(
            calls("      - run: ./tools/check_claims.py --registry x\n"),
            [("ci.yml", 1, "tools/check_claims.py")],
        )

    def test_a_glob_or_an_expanded_variable_is_not_one_command_to_check(self):
        self.assertEqual(calls("      - run: scripts/licenses/*.sh check\n"), [])
        self.assertEqual(calls("      - run: scripts/release/${SCRIPT}.sh\n"), [])

    def test_the_block_ends_where_the_next_step_starts(self):
        text = (
            "      - run: |\n"
            "          scripts/release/test-stage.sh\n"
            "      - name: next\n"
            "        run: scripts/release/test-verify.sh\n"
        )
        self.assertEqual(
            calls(text),
            [("ci.yml", 2, "scripts/release/test-stage.sh"), ("ci.yml", 4, "scripts/release/test-verify.sh")],
        )


class ReportingTest(unittest.TestCase):
    def test_an_executable_script_is_clean(self):
        found = calls("      - run: scripts/licenses/generate.sh check\n")
        self.assertEqual(judge(found, executable=["scripts/licenses/generate.sh"]), [])

    def test_a_missing_bit_is_named_with_its_workflow_line(self):
        found = calls("      - run: scripts/clients/test-r.sh target/debug/weft-server\n")
        self.assertEqual(
            judge(found, executable=[]),
            ["ci.yml:1: scripts/clients/test-r.sh is run directly but is not executable"],
        )

    def test_a_renamed_away_script_in_a_real_directory_is_reported(self):
        found = calls("      - run: scripts/release/test-removed.sh\n")
        self.assertEqual(
            judge(found, executable=[], tree=set(), parents={"scripts/release"}),
            ["ci.yml:1: scripts/release/test-removed.sh is run directly but is not in the tree"],
        )

    def test_a_path_another_checkout_lays_down_is_not_ours_to_check(self):
        found = calls("      - run: tools/scripts/release/stage.sh sums dist\n")
        self.assertEqual(judge(found, executable=[], tree=set(), parents=set()), [])

    def test_the_same_script_called_twice_reports_both_lines_once_each(self):
        text = (
            "      - run: scripts/release/verify.sh tag v1\n"
            "      - run: scripts/release/verify.sh ref HEAD\n"
        )
        self.assertEqual(len(judge(calls(text), executable=[])), 2)


if __name__ == "__main__":
    unittest.main()
