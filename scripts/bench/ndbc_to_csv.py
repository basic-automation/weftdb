#!/usr/bin/env python3
"""Turn NOAA NDBC standard-meteorological history into a `weft-bench --gf-csv` corpus.

The spline-fill claim needs a real smooth physical signal (ROADMAP.md, #144). NDBC buoys log
water temperature, air temperature and pressure every ten minutes (hourly in older years), and
their yearly history files are free to use and redistribute. A file is whitespace-separated
text: two `#` header lines naming and unit-labelling the columns, then one row per reading
that starts `YY MM DD hh mm` in UTC (the layout since 2005). A missing reading is written as
nines, with a marker that depends on the column: `999.0` for a temperature but `9999.0` for
pressure, where 999.0 hPa is a real reading.

This reads any number of those files (gzipped or not), keeps one column, drops the missing
readings, and writes `epoch_seconds,value` rows in time order. The value is copied as the
file's decimal text, so it reaches WeftDB without passing through a float. A timestamp seen
twice (the yearly files can overlap at the year boundary) keeps the first reading.

    python3 scripts/bench/ndbc_to_csv.py --column WTMP 44013h2022.txt.gz 44013h2023.txt.gz > 44013-wtmp.csv
    weft-bench --gap-fill --gf-csv 44013-wtmp.csv --gf-bucket h --gf-fill cubic

The corpus is not committed: download the files from
https://www.ndbc.noaa.gov/data/historical/stdmet/ (`<station>h<year>.txt.gz`).
"""

from __future__ import annotations

import argparse
import calendar
import gzip
import sys
from pathlib import Path
from typing import IO, Iterable, Iterator

# NDBC's missing marker per column ("Missing data" on NDBC's measurement-description page).
MISSING = {
	"WDIR": "999", "WSPD": "99.0", "GST": "99.0", "WVHT": "99.00", "DPD": "99.00", "APD": "99.00", "MWD": "999",
	"PRES": "9999.0", "ATMP": "999.0", "WTMP": "999.0", "DEWP": "999.0", "VIS": "99.0", "PTDY": "99.0", "TIDE": "99.00",
}


def read_rows(lines: Iterable[str], column: str) -> Iterator[tuple[int, str]]:
	"""Yield `(epoch_seconds, value_text)` for every row of one file that has a `column` reading."""
	index = None
	for line_no, line in enumerate(lines, 1):
		fields = line.split()
		if not fields:
			continue
		if fields[0].startswith("#") or not fields[0].isdigit():  # files before 2007 omit the `#`
			if index is None:
				names = [fields[0].lstrip("#")] + fields[1:]
				if names[:5] != ["YY", "MM", "DD", "hh", "mm"]:
					raise ValueError(f"line {line_no}: header opens {' '.join(names[:5])!r}, not 'YY MM DD hh mm' (pre-2005 layouts are not read)")
				if column not in names[5:]:
					raise ValueError(f"line {line_no}: no column {column!r} (have {' '.join(names[5:])})")
				index = names.index(column)
			continue
		if index is None:
			raise ValueError(f"line {line_no}: data before the header")
		if len(fields) <= index:
			raise ValueError(f"line {line_no}: {len(fields)} fields, {column} is field {index + 1}")
		year, month, day, hour, minute = (int(f) for f in fields[:5])
		value = fields[index]
		if value == MISSING.get(column):
			continue
		float(value)  # refuse what is not a number; the text itself is what gets written
		yield calendar.timegm((year, month, day, hour, minute, 0)), value


def convert(files: Iterable[Path], column: str, out: IO[str]) -> int:
	"""Write `files`' `column` to `out` as `epoch,value`; return the rows written."""
	readings: dict[int, str] = {}
	for path in files:
		opener = gzip.open if path.suffix == ".gz" else open
		with opener(path, "rt") as handle:
			try:
				for ts, value in read_rows(handle, column):
					readings.setdefault(ts, value)
			except ValueError as e:
				raise ValueError(f"{path}: {e}") from None
	if len(readings) < 2:
		raise ValueError(f"{column}: {len(readings)} readings; at least two are needed")
	out.write(f"epoch,{column.lower()}\n")
	for ts in sorted(readings):
		out.write(f"{ts},{readings[ts]}\n")
	return len(readings)


def main(argv: list[str] | None = None) -> int:
	parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
	parser.add_argument("--column", default="WTMP", help="column to keep (WTMP, ATMP, PRES, …; default WTMP)")
	parser.add_argument("files", nargs="+", type=Path, help="NDBC stdmet history files, .txt or .txt.gz")
	args = parser.parse_args(argv)
	try:
		rows = convert(args.files, args.column, sys.stdout)
	except (OSError, ValueError) as e:
		print(f"ndbc_to_csv: {e}", file=sys.stderr)
		return 1
	print(f"ndbc_to_csv: {rows} {args.column} readings", file=sys.stderr)
	return 0


if __name__ == "__main__":
	sys.exit(main())
