#!/usr/bin/env python3
"""Annotate metric drift warnings from a SoakComparison verdict JSON.

Reads a SoakComparison JSON file (--verdict PATH) and prints
GitHub Actions ::warning:: annotations for every metric whose
drift exceeds its documented tolerance.
"""
import json
import sys


def main() -> None:
    if len(sys.argv) != 3 or sys.argv[1] != "--verdict":
        print("usage: annotate-soak-drift.py --verdict <verdict.json>", file=sys.stderr)
        sys.exit(2)

    path = sys.argv[2]
    with open(path) as f:
        comp = json.load(f)

    if not comp.get("has_drift_warning"):
        return

    for drift in comp.get("metric_drifts", []):
        if drift.get("past_tolerance"):
            pct = drift["drift_pct"] * 100
            tol = drift["tolerance"] * 100
            print(
                f"::warning:: Metric drift: {drift['metric_name']} "
                f"changed from {drift['previous_value']} to {drift['current_value']} "
                f"({pct:.1f}%, tolerance {tol:.0f}%)"
            )


if __name__ == "__main__":
    main()
