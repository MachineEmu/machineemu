"""Validate profiles and resolve their exact engine executable."""

from __future__ import annotations

from dataclasses import dataclass
import json
from pathlib import Path
from typing import Any

from machineemu.assets import AssetError, AssetStore
from machineemu.engines import EngineManifest, EngineRegistry, EngineRegistryError


class ProfileError(ValueError):
    """Raised when a profile cannot become a launch input."""


@dataclass(frozen=True)
class ResolvedProfile:
    profile_id: str
    machine: str
    target: str
    configuration: dict[str, Any]
    engine: EngineManifest
    executable: Path
    assets: dict[str, Path]


def _required_string(value: Any, name: str) -> str:
    if not isinstance(value, str) or not value:
        raise ProfileError(f"{name} must be a non-empty string")
    return value


def resolve_profile(path: Path, registry: EngineRegistry, *, target: str,
                    asset_store: AssetStore | None = None) -> ResolvedProfile:
    """Resolve a profile without creating state or starting a process."""
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        raise ProfileError(f"cannot read profile {path}: {exc}") from exc
    return resolve_profile_value(value, registry, target=target, asset_store=asset_store)


def resolve_profile_value(value: Any, registry: EngineRegistry, *, target: str,
                          asset_store: AssetStore | None = None) -> ResolvedProfile:
    """Resolve already-loaded profile data (for catalog-backed callers)."""
    if not isinstance(value, dict) or value.get("schema_version") != 1:
        raise ProfileError("profile schema_version must be 1")
    profile_id = _required_string(value.get("id"), "profile.id")
    machine = _required_string(value.get("machine"), "profile.machine")
    engine = value.get("engine")
    if not isinstance(engine, dict):
        raise ProfileError("profile.engine must be a mapping")
    track = _required_string(engine.get("track"), "profile.engine.track")
    resources = value.get("resources", {})
    if not isinstance(resources, dict):
        raise ProfileError("profile.resources must be a mapping")
    assets = value.get("assets", {})
    if not isinstance(assets, dict):
        raise ProfileError("profile.assets must be a mapping")
    for name, digest in assets.items():
        if not isinstance(name, str) or not name or not isinstance(digest, str) or not digest.startswith("sha256:"):
            raise ProfileError(f"profile.assets.{name} must be a sha256 reference")
    resolved_assets: dict[str, Path] = {}
    if assets and asset_store is None:
        raise ProfileError("profile assets require an asset store")
    if asset_store is not None:
        for name, reference in assets.items():
            try:
                resolved_assets[name] = asset_store.resolve(reference)
            except AssetError as exc:
                raise ProfileError(f"asset {name!r} is unavailable: {exc}") from exc
    try:
        manifest, executable = registry.resolve(track, target)
    except EngineRegistryError as exc:
        raise ProfileError(str(exc)) from exc
    return ResolvedProfile(profile_id, machine, target, value, manifest, executable, resolved_assets)
