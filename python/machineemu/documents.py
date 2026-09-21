"""Read operator-authored configuration as either JSON or YAML.

Only files an operator writes by hand go through here. Runtime manifests and
session state this project generates itself stay JSON so they round-trip
exactly.
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any

JSON_SUFFIXES = (".json",)
YAML_SUFFIXES = (".yaml", ".yml")
DOCUMENT_SUFFIXES = JSON_SUFFIXES + YAML_SUFFIXES


class DocumentError(ValueError):
    """Raised when a configuration document cannot be read or parsed."""


def load_document(path: Path) -> Any:
    """Parse one JSON or YAML document, chosen by the file's suffix."""
    suffix = path.suffix.lower()
    if suffix not in DOCUMENT_SUFFIXES:
        raise DocumentError(f"unsupported configuration format {suffix or path.name!r}: "
                            f"expected one of {', '.join(DOCUMENT_SUFFIXES)}")
    try:
        text = path.read_text(encoding="utf-8")
    except OSError as exc:
        raise DocumentError(f"cannot read {path}: {exc}") from exc
    if suffix in JSON_SUFFIXES:
        try:
            return json.loads(text)
        except json.JSONDecodeError as exc:
            raise DocumentError(f"cannot parse {path}: {exc}") from exc
    try:
        import yaml
    except ModuleNotFoundError as exc:
        raise DocumentError(
            f"reading {path} needs PyYAML; install the yaml extra with: "
            "python -m pip install -e '.[yaml]'") from exc
    try:
        # safe_load only: these documents are operator input, and the full
        # loader would let one construct arbitrary Python objects.
        return yaml.safe_load(text)
    except yaml.YAMLError as exc:
        raise DocumentError(f"cannot parse {path}: {exc}") from exc
