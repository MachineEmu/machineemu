"""Validate immutable engine-build manifests and resolve executable paths."""

from __future__ import annotations

from dataclasses import dataclass
import hashlib
import json
from pathlib import Path
from typing import Any


class EngineManifestError(ValueError):
    """Raised when an installed engine manifest is invalid."""


@dataclass(frozen=True)
class EngineManifest:
    path: Path
    track_id: str
    build_digest: str
    source_revision: str
    targets: tuple[str, ...]
    executables: dict[str, Path]
    executable_sha256: dict[str, str]
    dirty_source: bool

    def executable(self, target: str) -> Path:
        try:
            return self.executables[target]
        except KeyError as exc:
            raise EngineManifestError(
                f"engine {self.track_id!r} has no executable for target {target!r}"
            ) from exc


def _required_string(value: Any, name: str) -> str:
    if not isinstance(value, str) or not value:
        raise EngineManifestError(f"{name} must be a non-empty string")
    return value


def load_manifest(path: Path, *, require_clean: bool = False) -> EngineManifest:
    """Load and validate an installed engine-build manifest.

    Executable paths are resolved below the manifest directory and may not
    escape it. A release caller can set ``require_clean`` to reject a build
    produced from a dirty source tree.
    """
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        raise EngineManifestError(f"cannot read engine manifest {path}: {exc}") from exc
    if not isinstance(value, dict) or value.get("schema_version") != 1:
        raise EngineManifestError("engine manifest schema_version must be 1")
    track_id = _required_string(value.get("track_id"), "track_id")
    source_revision = _required_string(value.get("source_revision"), "source_revision")
    build_digest = value.get("build_digest")
    if not isinstance(build_digest, str) or len(build_digest) != 64:
        raise EngineManifestError("build_digest must be a SHA-256 hex digest")
    try:
        int(build_digest, 16)
    except ValueError as exc:
        raise EngineManifestError("build_digest must be hexadecimal") from exc
    targets = value.get("targets")
    if not isinstance(targets, list) or not targets or any(not isinstance(x, str) or not x for x in targets):
        raise EngineManifestError("targets must be a non-empty list of names")
    executable_values = value.get("executables")
    if not isinstance(executable_values, dict):
        raise EngineManifestError("executables must be a mapping")
    root = path.parent.resolve()
    executables: dict[str, Path] = {}
    for target in targets:
        raw = executable_values.get(target)
        if not isinstance(raw, str) or not raw:
            raise EngineManifestError(f"executables.{target} must be a path")
        executable = (root / raw).resolve()
        if executable != root and root not in executable.parents:
            raise EngineManifestError(f"executable for {target} escapes the engine bundle")
        executables[target] = executable
    dirty_source = value.get("dirty_source")
    if not isinstance(dirty_source, bool):
        raise EngineManifestError("dirty_source must be boolean")
    if require_clean and dirty_source:
        raise EngineManifestError("dirty engine builds cannot be used for release")
    hashes_value = value.get("executable_sha256", {})
    if not isinstance(hashes_value, dict):
        raise EngineManifestError("executable_sha256 must be a mapping")
    executable_sha256: dict[str, str] = {}
    for target in targets:
        digest = hashes_value.get(target)
        if digest is None:
            continue
        if not isinstance(digest, str) or len(digest) != 64:
            raise EngineManifestError(f"executable_sha256.{target} must be a SHA-256 hex digest")
        try:
            int(digest, 16)
        except ValueError as exc:
            raise EngineManifestError(f"executable_sha256.{target} must be hexadecimal") from exc
        executable_sha256[target] = digest
    return EngineManifest(path, track_id, build_digest, source_revision,
                          tuple(targets), executables, executable_sha256, dirty_source)


def manifest_digest(value: dict[str, Any]) -> str:
    """Return the canonical digest used for manifest input comparisons."""
    encoded = json.dumps(value, sort_keys=True, separators=(",", ":")).encode()
    return hashlib.sha256(encoded).hexdigest()
