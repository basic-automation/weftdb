#!/usr/bin/env python3
"""Tests for scripts/bench/ndbc_to_csv.py.

Run from the repository root:

    python3 -m unittest discover -s scripts/bench -p 'test_*.py' -v

The sample rows are cut from NDBC station 44013's 2023 history, columns trimmed.
"""

from __future__ import annotations

import gzip
import io
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import ndbc_to_csv  # noqa: E402

SAMPLE = """\
#YY  MM DD hh mm   PRES  ATMP  WTMP
#yr  mo dy hr mn    hPa  degC  degC
2023 01 01 00 00 1012.4   8.1 999.0
2023 01 01 00 10  999.0   8.2   7.4
2023 01 01 00 20 9999.0   8.2   7.5
"""


class ReadRows(unittest.TestCase):
	def test_keeps_the_decimal_text_and_drops_the_columns_missing_marker(self) -> None:
		rows = list(ndbc_to_csv.read_rows(SAMPLE.splitlines(), "WTMP"))
		self.assertEqual(rows, [(1672531800, "7.4"), (1672532400, "7.5")])

	def test_a_reading_of_nines_is_real_where_that_column_marks_missing_otherwise(self) -> None:
		rows = list(ndbc_to_csv.read_rows(SAMPLE.splitlines(), "PRES"))
		self.assertEqual([v for _, v in rows], ["1012.4", "999.0"], "999.0 hPa is a pressure; 9999.0 is missing")

	def test_refuses_an_unknown_column_an_old_layout_and_a_non_number(self) -> None:
		with self.assertRaisesRegex(ValueError, "no column 'XYZ'"):
			list(ndbc_to_csv.read_rows(SAMPLE.splitlines(), "XYZ"))
		with self.assertRaisesRegex(ValueError, "pre-2005"):
			list(ndbc_to_csv.read_rows(["YYYY MM DD hh WTMP", "1999 01 01 00 7.4"], "WTMP"))
		with self.assertRaises(ValueError):
			list(ndbc_to_csv.read_rows(SAMPLE.replace("7.5", "x").splitlines(), "WTMP"))


class Convert(unittest.TestCase):
	def test_merges_files_in_time_order_keeping_the_first_of_a_repeated_instant(self) -> None:
		later = SAMPLE.replace("2023 01 01 00 10  999.0   8.2   7.4", "2023 01 01 00 10  999.0   8.2   9.9").replace("2023 01 01 00 20", "2023 01 01 00 30")
		with tempfile.TemporaryDirectory() as tmp:
			first, second = Path(tmp, "a.txt.gz"), Path(tmp, "b.txt")
			with gzip.open(first, "wt") as handle:
				handle.write(SAMPLE)
			second.write_text(later)
			out = io.StringIO()
			rows = ndbc_to_csv.convert([second, first], "WTMP", out)
		self.assertEqual(rows, 3)
		self.assertEqual(out.getvalue(), "epoch,wtmp\n1672531800,9.9\n1672532400,7.5\n1672533000,7.5\n")


if __name__ == "__main__":
	unittest.main()
