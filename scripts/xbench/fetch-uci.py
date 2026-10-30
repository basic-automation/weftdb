#!/usr/bin/env python3
"""Fetch the UCI household-power corpus the cross-engine report runs on (EVI-15).

Downloads "Individual household electric power consumption" (Hébrail and Bérard, UCI Machine
Learning Repository, dataset 235, CC BY 4.0), checks the zip against the SHA-256 the published
report was run on, and extracts household_power_consumption.txt into DIR. A file already present
with the right checksum is not fetched again.

Usage: scripts/xbench/fetch-uci.py [DIR]   (default: data/uci-household-power)
Then:  weft-xbench --uci DIR/household_power_consumption.txt ...
"""
from __future__ import annotations

import hashlib
import sys
import urllib.request
import zipfile
from pathlib import Path

URL = "https://archive.ics.uci.edu/static/public/235/individual+household+electric+power+consumption.zip"
SHA256 = "9f84b46ade8a2d8e1286ec4b2b6c2987a45a755c59f263be3b3b3d10dfbda3ff"
MEMBER = "household_power_consumption.txt"


def sha256(path: Path) -> str:
	digest = hashlib.sha256()
	with path.open("rb") as f:
		for chunk in iter(lambda: f.read(1 << 20), b""):
			digest.update(chunk)
	return digest.hexdigest()


def main(argv: list[str]) -> int:
	out = Path(argv[1] if len(argv) > 1 else "data/uci-household-power")
	out.mkdir(parents=True, exist_ok=True)
	archive = out / "household_power_consumption.zip"
	if not archive.exists() or sha256(archive) != SHA256:
		print(f"fetching {URL}", file=sys.stderr)
		urllib.request.urlretrieve(URL, archive)
	got = sha256(archive)
	if got != SHA256:
		print(f"checksum mismatch: expected {SHA256}, got {got}", file=sys.stderr)
		return 1
	with zipfile.ZipFile(archive) as z:
		z.extract(MEMBER, out)
	print(out / MEMBER)
	return 0


if __name__ == "__main__":
	sys.exit(main(sys.argv))
