"""Fail if the checked-in OpenAPI contract is stale."""

from __future__ import annotations

import json
from pathlib import Path
import sys

from openapi import schema


def main() -> int:
    expected_path = Path(__file__).parents[1] / "contracts/openapi.json"
    expected = json.loads(expected_path.read_text(encoding="utf-8"))
    actual = schema()
    if actual != expected:
        print("OpenAPI contract is stale; run scripts/openapi.py contracts/openapi.json", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
