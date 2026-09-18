"""Operator-owned roots and engine bundle configuration."""

from __future__ import annotations

from dataclasses import dataclass
import json
from pathlib import Path
from typing import Any


class OperatorConfigError(ValueError):
    """Raised when operator configuration is incomplete or unsafe."""


@dataclass(frozen=True)
class OperatorConfig:
    engine_root: Path
    asset_root: Path
    state_root: Path
    runtime_root: Path
    artifact_root: Path

    @classmethod
    def load(cls, path: Path) -> "OperatorConfig":
        try:
            value: Any = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as exc:
            raise OperatorConfigError(f"cannot read operator config {path}: {exc}") from exc
        if not isinstance(value, dict) or value.get("schema_version") != 1:
            raise OperatorConfigError("operator config schema_version must be 1")
        roots = value.get("roots")
        if not isinstance(roots, dict):
            raise OperatorConfigError("operator config roots must be a mapping")
        names = ("engine_root", "asset_root", "state_root", "runtime_root", "artifact_root")
        paths: dict[str, Path] = {}
        for name in names:
            raw = roots.get(name)
            if not isinstance(raw, str) or not raw or "\x00" in raw:
                raise OperatorConfigError(f"roots.{name} must be a non-empty path")
            candidate = Path(raw).expanduser()
            if not candidate.is_absolute():
                candidate = path.parent / candidate
            paths[name] = candidate.resolve()
        return cls(*(paths[name] for name in names))
