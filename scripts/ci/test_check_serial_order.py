"""Tests for check-serial-order.py: #[serial] must sit immediately above #[test] or #[tokio::test]."""
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
    """#[serial] must sit immediately above a test attribute."""

    # --- Correct order (accept) ---

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
        """#[serial] with a conditional attr in between is accepted:
        the conditional attribute does not change the test nature."""
        src = "#[serial]\n#[cfg(unix)]\n#[tokio::test]\nasync fn it_works() {}"
        self.assertEqual(check(src), [])

    def test_no_serial_is_always_fine(self):
        src = "#[tokio::test]\nasync fn no_serial() {}"
        self.assertEqual(check(src), [])

    # --- Wrong order (reject) ---

    def test_test_before_serial_rejects(self):
        src = "#[tokio::test]\n#[serial]\nasync fn it_works() {}"
        errs = check(src)
        self.assertEqual(len(errs), 1)
        # The violation should point at the #[serial] line (line 2)
        filepath, lineno, detail = errs[0]
        # error for the second line (the #[serial], line 2)
        # Actually the error is for the serial line at line 2
        # Our checker currently produces an error for #[serial] that is not
        # immediately above a test attribute. But with the parent-child ordering,
        # what does the error say?
        # The checker reports #[serial] on line 2 as "not immediately above a test attribute"
        # because the next non-blank non-comment line is #[serial] on line 1, then the testattr check
        # Wait - let me trace through check_file for this input:
        # line 1: #[tokio::test]
        # line 2: #[serial]
        # line 3: async fn it_works() {}
        # state starts None. line 1 matches serial? no. line 2 matches serial? yes (line 2).
        # scan forward from line 2: look_ahead=2 (line 3) -> "async fn it_works() {}"
        #   not blank, not comment - _SERIAL_RE.match? no. _TEST_ATTR_RE.match? no.
        #   starts_with("#[")? no. So we hit: errors.append for "not above a test attribute"
        self.assertIn("serial", detail.lower())

    def test_serial_on_plain_fn_no_test_rejects(self):
        """#[serial] on a function without a test attribute is flagged."""
        src = "#[serial]\nasync fn helper() {}"
        errs = check(src)
        self.assertEqual(len(errs), 1)

    # --- False positives: #[serial] in non-code positions ---

    def test_serial_in_doc_comment_is_ignored(self):
        """A doc comment containing #[serial] must not trigger."""
        src = "/// Some docs with #[serial] in them\n#[tokio::test]\nasync fn it_works() {}"
        self.assertEqual(check(src), [])

    def test_serial_in_line_comment_is_ignored(self):
        src = "// #[serial] commented out\n#[tokio::test]\nasync fn it_works() {}"
        self.assertEqual(check(src), [])

    def test_serial_in_string_literal_is_ignored(self):
        src = 'let s = "#[serial]";\n#[tokio::test]\nasync fn it_works() {}'
        self.assertEqual(check(src), [])

    # --- #[serial] on an impl-block helper is permitted if correctly ordered ---
    # The checker flags #[serial] that is not immediately above a test attribute,
    # which is the right thing for a #[serial] on a non-test fn.

    def test_serial_on_impl_helper_rejects(self):
        """#[serial] on a non-test function inside an impl is flagged because
        the serial attribute is wasted there — but the static guard flags it
        for awareness."""
        src = "\t#[serial]\n\tasync fn helper() {}"
        errs = check(src)
        self.assertEqual(len(errs), 1)

    # --- Edge cases ---

    def test_several_serial_attrs_collapsed(self):
        """Multiple #[serial] on the same target should be collapsed by
        the compiler — only the last one above a test attr passes."""
        src = "#[serial]\n#[serial]\n#[tokio::test]\nasync fn it_works() {}"
        self.assertEqual(check(src), [])

    def test_blank_line_between_serial_and_test_attr_rejects(self):
        """A blank line between #[serial] and the test attribute is rejected."""
        src = "#[serial]\n\n#[tokio::test]\nasync fn it_works() {}"
        errs = check(src)
        self.assertEqual(len(errs), 1)
        self.assertIn("blank line", errs[0][2].lower())

    def test_empty_file(self):
        self.assertEqual(check(""), [])


if __name__ == "__main__":
    unittest.main()
