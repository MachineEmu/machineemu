"""Check that a release set names complete, integrity-verifiable bundles."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import re
import sys


def check(path: Path) -> list[str]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        return [str(exc)]
    errors: list[str] = []
    if not isinstance(value, dict) or value.get("schema_version") != 1:
        return ["release set schema_version must be 1"]
    engines = value.get("engines")
    if not isinstance(engines, dict) or not engines:
        return ["release set engines must be a non-empty mapping"]
    for track, entry in engines.items():
        prefix = f"engines.{track}"
        if not isinstance(entry, dict):
            errors.append(f"{prefix} must be a mapping")
            continue
        manifest = entry.get("manifest")
        if not isinstance(manifest, str) or not manifest or Path(manifest).is_absolute():
            errors.append(f"{prefix}.manifest must be relative")
        digest = entry.get("build_digest")
        if not isinstance(digest, str) or not re.fullmatch(r"[0-9a-f]{64}", digest):
            errors.append(f"{prefix}.build_digest must be a SHA-256 hex digest")
        if entry.get("require_executable_hashes") is not True:
            errors.append(f"{prefix}.require_executable_hashes must be true")
    return errors


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("release_set", type=Path)
    args = parser.parse_args(argv)
    errors = check(args.release_set)
    if errors:
        for error in errors:
            print(f"release-set: {error}", file=sys.stderr)
        return 1
    print(f"release-set valid: {args.release_set}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
