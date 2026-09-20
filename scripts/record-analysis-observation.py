#!/usr/bin/env python3
"""Record bounded guest observations in a MachineEmu analysis report."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import sys

from machineemu.domains.analysis import missing_observation_fields, record_observation


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("report", type=Path)
    parser.add_argument("observation", type=Path, nargs="?", help="JSON file; defaults to stdin")
    parser.add_argument("--allow-incomplete", action="store_true")
    args = parser.parse_args()
    try:
        report = json.loads(args.report.read_text(encoding="utf-8"))
        raw = args.observation.read_text(encoding="utf-8") if args.observation else sys.stdin.read()
        observed = json.loads(raw)
        missing = missing_observation_fields(report, observed)
        if missing and not args.allow_incomplete:
            raise ValueError("observation is missing required field(s): " + ", ".join(missing))
        updated = record_observation(report, observed)
        args.report.write_text(json.dumps(updated, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    except (OSError, json.JSONDecodeError, TypeError, ValueError) as exc:
        print(f"unable to record analysis observation: {exc}", file=sys.stderr)
        return 2
    print(json.dumps(updated["guest_observed"], indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
