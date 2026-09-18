"""Resolve engine tracks through an exact product release set."""

from __future__ import annotations

from dataclasses import dataclass
import json
from pathlib import Path
from typing import Any

from .manifest import EngineManifest, EngineManifestError, load_manifest


class EngineRegistryError(ValueError):
    """Raised when a release set cannot resolve an engine build."""


@dataclass(frozen=True)
class EngineRegistry:
    release_set: Path
    bundle_root: Path
    engines: dict[str, dict[str, Any]]

    @classmethod
    def load(cls, release_set: Path, bundle_root: Path) -> "EngineRegistry":
        try:
            value = json.loads(release_set.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as exc:
            raise EngineRegistryError(f"cannot read release set {release_set}: {exc}") from exc
        if not isinstance(value, dict) or value.get("schema_version") != 1:
            raise EngineRegistryError("release set schema_version must be 1")
        engines = value.get("engines")
        if not isinstance(engines, dict):
            raise EngineRegistryError("release set engines must be a mapping")
        for track_id, entry in engines.items():
            if not isinstance(track_id, str) or not track_id or not isinstance(entry, dict):
                raise EngineRegistryError("engine entries must be named mappings")
            manifest = entry.get("manifest")
            digest = entry.get("build_digest")
            if not isinstance(manifest, str) or not manifest or Path(manifest).is_absolute():
                raise EngineRegistryError(f"engines.{track_id}.manifest must be relative")
            if not isinstance(digest, str) or len(digest) != 64:
                raise EngineRegistryError(f"engines.{track_id}.build_digest must be SHA-256")
        return cls(release_set, bundle_root.resolve(), engines)

    def resolve(self, track_id: str, target: str) -> tuple[EngineManifest, Path]:
        try:
            entry = self.engines[track_id]
        except KeyError as exc:
            raise EngineRegistryError(f"engine track is not in the release set: {track_id}") from exc
        manifest_path = (self.bundle_root / entry["manifest"]).resolve()
        if manifest_path != self.bundle_root and self.bundle_root not in manifest_path.parents:
            raise EngineRegistryError("engine manifest escapes the bundle root")
        try:
            manifest = load_manifest(manifest_path, require_clean=True)
        except EngineManifestError as exc:
            raise EngineRegistryError(str(exc)) from exc
        if manifest.track_id != track_id:
            raise EngineRegistryError(
                f"manifest track {manifest.track_id!r} does not match release track {track_id!r}"
            )
        if manifest.build_digest != entry["build_digest"]:
            raise EngineRegistryError(f"engine build digest does not match release set for {track_id}")
        executable = manifest.executable(target)
        if not executable.is_file():
            raise EngineRegistryError(f"engine executable is missing: {executable}")
        return manifest, executable
