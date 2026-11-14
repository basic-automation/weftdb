"""Tests for check-serial-order.py.

The rule under test: every ``#[serial]`` — plain or keyed — must share an
attribute block with ``#[test]`` or ``#[tokio::test]``. Position inside the
block (above or below the test attribute) is free; a ``#[serial]`` in a block
with no test attribute does nothing and is what the guard exists to catch.
"""
import importlib.util
import tempfile
import unittest
from pathlib import Path

spec = importlib.util.spec_from_file_location(
    "check_serial_order", Path(__file__).with_name("check-serial-order.py")
)
assert spec is not None and spec.loader is not None
check_serial_order = importlib.util.module_from_spec(spec)
spec.loader.exec_module(check_serial_order)


def check(source: str) -> list[tuple[str, int, str]]:
    """Run check_file on a temporary .rs file containing ``source``."""
    with tempfile.NamedTemporaryFile(suffix=".rs", mode="w", delete=False, encoding="utf-8") as f:
        f.write(source)
        tmppath = f.name
    try:
        return check_serial_order.check_file(Path(tmppath))
    finally:
        Path(tmppath).unlink(missing_ok=True)


class SerialOrderTest(unittest.TestCase):
    """#[serial] must share an attribute block with a test attribute."""

    # --- Attached, serial first (the order this repo mostly uses) ---

    def test_serial_above_tokio_test_accepts(self):
        src = "#[serial]\n#[tokio::test]\nasync fn it_works() {}"
        self.assertEqual(check(src), [])

    def test_serial_above_test_accepts(self):
        src = "#[serial]\n#[test]\nfn it_works() {}"
        self.assertEqual(check(src), [])

    def test_serial_above_tokio_test_with_flavor_accepts(self):
        src = '#[serial]\n#[tokio::test(flavor = "multi_thread")]\nasync fn it_works() {}'
        self.assertEqual(check(src), [])

    def test_serial_above_tokio_test_full_args_accepts(self):
        src = '#[serial]\n#[tokio::test(flavor = "multi_thread", worker_threads = 4)]\nasync fn it_works() {}'
        self.assertEqual(check(src), [])

    def test_serial_above_cfg_above_tokio_test_accepts(self):
        """An unrelated attribute between the two is still one block."""
        src = "#[serial]\n#[cfg(unix)]\n#[tokio::test]\nasync fn it_works() {}"
        self.assertEqual(check(src), [])

    def test_no_serial_is_always_fine(self):
        src = "#[tokio::test]\nasync fn no_serial() {}"
        self.assertEqual(check(src), [])

    # --- Attached, serial last: the order serial_test's own README documents ---

    def test_test_before_serial_accepts(self):
        src = "#[tokio::test]\n#[serial]\nasync fn it_works() {}"
        self.assertEqual(check(src), [])

    def test_sync_test_before_serial_accepts(self):
        src = "#[test]\n#[serial]\nfn it_works() {}"
        self.assertEqual(check(src), [])

    def test_attr_between_test_and_serial_accepts(self):
        src = "#[tokio::test]\n#[cfg(unix)]\n#[serial]\nasync fn it_works() {}"
        self.assertEqual(check(src), [])

    def test_multiline_test_attr_with_serial_below_accepts(self):
        """Bracket balance keeps a serial attached through a wrapped attribute."""
        src = '#[tokio::test(\n    flavor = "multi_thread",\n)]\n#[serial]\nasync fn it_works() {}'
        self.assertEqual(check(src), [])

    # --- Keyed form: the same attribute, and the form the old regex missed ---

    def test_keyed_serial_above_test_accepts(self):
        src = "#[serial(db)]\n#[test]\nfn it_works() {}"
        self.assertEqual(check(src), [])

    def test_keyed_serial_above_tokio_test_accepts(self):
        src = "#[serial(db)]\n#[tokio::test]\nasync fn it_works() {}"
        self.assertEqual(check(src), [])

    def test_keyed_serial_below_tokio_test_accepts(self):
        """The shape the fault-injector tests use: #[tokio::test] then #[serial(key)]."""
        src = "#[tokio::test]\n#[serial(backup_fault_points)]\nasync fn it_works() {}"
        self.assertEqual(check(src), [])

    def test_keyed_serial_multi_key_accepts(self):
        src = "#[serial(store, index)]\n#[tokio::test]\nasync fn it_works() {}"
        self.assertEqual(check(src), [])

    def test_keyed_serial_above_cfg_above_test_accepts(self):
        src = "#[serial(db)]\n#[cfg(unix)]\n#[test]\nfn it_works() {}"
        self.assertEqual(check(src), [])

    def test_keyed_and_plain_serial_collapsed_accepts(self):
        """A plain and a keyed #[serial] in one block pass."""
        src = "#[serial]\n#[serial(db)]\n#[tokio::test]\nasync fn it_works() {}"
        self.assertEqual(check(src), [])

    def test_lookalike_attribute_is_not_a_serial(self):
        """#[serializer] is not #[serial] and must never be flagged."""
        src = "#[serializer]\n#[test]\nfn it_works() {}"
        self.assertEqual(check(src), [])

    # --- Not attached to a test: the dead attribute this guard exists for ---

    def test_serial_on_plain_fn_no_test_rejects(self):
        src = "#[serial]\nasync fn helper() {}"
        errs = check(src)
        self.assertEqual(len(errs), 1)
        self.assertIn("no #[test]", errs[0][2])

    def test_keyed_serial_on_plain_fn_no_test_rejects(self):
        src = "#[serial(db)]\nasync fn helper() {}"
        errs = check(src)
        self.assertEqual(len(errs), 1)
        self.assertIn("no #[test]", errs[0][2])

    def test_serial_on_impl_helper_rejects(self):
        """#[serial] inside an impl is wasted; the guard flags it for awareness."""
        src = "\t#[serial]\n\tasync fn helper() {}"
        self.assertEqual(len(check(src)), 1)

    def test_serial_above_only_non_test_attrs_rejects(self):
        src = "#[serial(db)]\n#[allow(dead_code)]\nfn helper() {}"
        errs = check(src)
        self.assertEqual(len(errs), 1)

    def test_serial_on_a_struct_rejects(self):
        src = "#[serial(db)]\npub struct Gate;"
        self.assertEqual(len(check(src)), 1)

    def test_unattached_serial_is_reported_on_its_own_line(self):
        src = "#[derive(Debug)]\npub struct S;\n\n#[serial(locked)]\nfn helper() {}\n"
        errs = check(src)
        self.assertEqual(len(errs), 1)
        self.assertEqual(errs[0][1], 4)  # the #[serial] line, not the fn line

    def test_only_the_unattached_serial_of_two_is_reported(self):
        src = "#[tokio::test]\n#[serial(db)]\nasync fn good() {}\n\n#[serial(db)]\nfn helper() {}\n"
        errs = check(src)
        self.assertEqual(len(errs), 1)
        self.assertEqual(errs[0][1], 5)

    # --- Module-level use is supported by the crate ---

    def test_serial_on_a_module_accepts(self):
        src = "#[serial(db)]\nmod gated_tests {\n    #[tokio::test]\n    async fn inner() {}\n}\n"
        self.assertEqual(check(src), [])

    def test_pub_crate_serial_on_a_module_accepts(self):
        src = "#[serial(db)]\npub(crate) mod gated_tests {}"
        self.assertEqual(check(src), [])

    # --- Style rules, kept: one attribute per line, no blank line in a block ---

    def test_keyed_serial_same_line_content_rejects(self):
        src = "#[serial(db)] #[test]\nfn it_works() {}"
        errs = check(src)
        self.assertEqual(len(errs), 1)
        self.assertIn("same line", errs[0][2].lower())

    def test_blank_line_between_serial_and_test_attr_rejects(self):
        """A blank line splits the block: the serial is attached to nothing."""
        src = "#[serial]\n\n#[tokio::test]\nasync fn it_works() {}"
        errs = check(src)
        self.assertEqual(len(errs), 1)
        self.assertIn("blank line", errs[0][2].lower())

    def test_keyed_serial_blank_line_between_rejects(self):
        """The blank-line rule applies to the keyed form too."""
        src = "#[serial(db)]\n\n#[tokio::test]\nasync fn it_works() {}"
        errs = check(src)
        self.assertEqual(len(errs), 1)
        self.assertIn("blank line", errs[0][2].lower())

    # --- False positives: #[serial] in non-code positions ---

    def test_serial_in_doc_comment_is_ignored(self):
        src = "/// Some docs with #[serial] in them\n#[tokio::test]\nasync fn it_works() {}"
        self.assertEqual(check(src), [])

    def test_serial_in_line_comment_is_ignored(self):
        src = "// #[serial] commented out\n#[tokio::test]\nasync fn it_works() {}"
        self.assertEqual(check(src), [])

    def test_serial_in_string_literal_is_ignored(self):
        src = 'let s = "#[serial]";\n#[tokio::test]\nasync fn it_works() {}'
        self.assertEqual(check(src), [])

    # --- Edge cases ---

    def test_several_serial_attrs_in_one_block(self):
        src = "#[serial]\n#[serial]\n#[tokio::test]\nasync fn it_works() {}"
        self.assertEqual(check(src), [])

    def test_empty_file(self):
        self.assertEqual(check(""), [])


if __name__ == "__main__":
    unittest.main()
