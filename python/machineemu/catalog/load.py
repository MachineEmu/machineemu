"""Load catalog profiles without resolving operator-specific paths."""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any


class CatalogError(ValueError):
    """Raised when a catalog profile is not redistributable metadata."""


def _walk(value: Any, where: str = "profile") -> None:
    if isinstance(value, str) and (value.startswith("/") or "\\" in value):
        raise CatalogError(f"{where} contains a host-specific path")
    if isinstance(value, dict):
        for key, child in value.items():
            _walk(child, f"{where}.{key}")
    elif isinstance(value, list):
        for index, child in enumerate(value):
            _walk(child, f"{where}[{index}]")


def load_profile(path: Path) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        raise CatalogError(f"cannot read catalog profile {path}: {exc}") from exc
    if not isinstance(value, dict) or value.get("schema_version") != 1:
        raise CatalogError("catalog profile schema_version must be 1")
    for key in ("id", "domain", "machine"):
        if not isinstance(value.get(key), str) or not value[key]:
            raise CatalogError(f"catalog profile {key} must be a non-empty string")
    if not isinstance(value.get("engine"), dict) or not isinstance(value["engine"].get("track"), str):
        raise CatalogError("catalog profile engine.track is required")
    _walk(value)
    return value
