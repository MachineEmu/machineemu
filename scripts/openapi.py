"""Build the canonical OpenAPI document without starting a server."""

from __future__ import annotations

import json
from pathlib import Path

from machineemu.api import create_app
from machineemu.runtime import OperatorApplication, OperatorConfig


def schema() -> dict:
    root = Path("/var/lib/machineemu")
    config = OperatorConfig(root / "engines", root / "assets", root / "state", root / "runtime", root / "artifacts")
    return create_app(OperatorApplication(config)).openapi()


def main() -> int:
    import argparse

    parser = argparse.ArgumentParser()
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    args.output.write_text(json.dumps(schema(), indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
