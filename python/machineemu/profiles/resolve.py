"""Validate profiles and resolve their exact engine executable."""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import Any

from machineemu.assets import AssetError, AssetStore
from machineemu.documents import DocumentError, load_document
from machineemu.domains.analysis import validate as validate_analysis
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
    asset_kinds: dict[str, str]
    analysis: dict[str, Any] | None = None
    analysis_payload: dict[str, Any] | None = None


def _required_string(value: Any, name: str) -> str:
    if not isinstance(value, str) or not value:
        raise ProfileError(f"{name} must be a non-empty string")
    return value


def _network(value: Any) -> dict[str, Any]:
    if value is None:
        value = {}
    if not isinstance(value, dict):
        raise ProfileError("profile.network must be a mapping")
    result = dict(value)
    network_type = result.get("type", "user")
    if network_type not in {"disabled", "user", "bridge"}:
        raise ProfileError("profile.network.type must be disabled, user, or bridge")
    if network_type == "bridge":
        bridge = result.get("bridge")
        if not isinstance(bridge, str) or not bridge or len(bridge) > 64 or any(
            character not in "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_.-"
            for character in bridge
        ):
            raise ProfileError("profile.network.bridge must be a safe bridge name")
        result["bridge"] = bridge
    elif "bridge" in result:
        raise ProfileError("profile.network.bridge is only valid for bridge networking")
    return {"type": network_type, **({"bridge": result["bridge"]} if network_type == "bridge" else {})}


def _cpu(value: Any, *, analysis: bool) -> str:
    if value is None:
        return "host,kvm=off" if analysis else "max"
    if not isinstance(value, str) or not value or "\x00" in value:
        raise ProfileError("profile.cpu must be a non-empty string")
    parts = value.split(",")
    if not parts[0] or any(not part for part in parts[1:]):
        raise ProfileError("profile.cpu contains an empty option")
    if analysis and ("hypervisor" in parts[1:] or "kvm=off" not in parts[1:]):
        raise ProfileError("malware-analysis CPU policy must include kvm=off and omit hypervisor")
    return value


def _asset_kinds(value: Any) -> dict[str, str]:
    """Index declared external asset kinds by asset ID so launch can wire them."""
    if value is None:
        return {}
    if not isinstance(value, list):
        raise ProfileError("profile.external_assets must be a list")
    kinds: dict[str, str] = {}
    for index, asset in enumerate(value):
        if not isinstance(asset, dict):
            raise ProfileError(f"profile.external_assets[{index}] must be a mapping")
        identifier = asset.get("id")
        if not isinstance(identifier, str) or not identifier:
            raise ProfileError(f"profile.external_assets[{index}].id must be a non-empty string")
        kind = asset.get("kind")
        if kind is None:
            continue
        if not isinstance(kind, str) or not kind:
            raise ProfileError(f"profile.external_assets[{index}].kind must be a non-empty string")
        kinds[identifier] = kind
    return kinds


def resolve_profile(path: Path, registry: EngineRegistry, *, target: str,
                    asset_store: AssetStore | None = None) -> ResolvedProfile:
    """Resolve a profile without creating state or starting a process."""
    try:
        value = load_document(path)
    except DocumentError as exc:
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
    network = _network(value.get("network"))
    assets = value.get("assets", {})
    if not isinstance(assets, dict):
        raise ProfileError("profile.assets must be a mapping")
    for name, digest in assets.items():
        if not isinstance(name, str) or not name or not isinstance(digest, str) or not digest.startswith("sha256:"):
            raise ProfileError(f"profile.assets.{name} must be a sha256 reference")
    asset_kinds = _asset_kinds(value.get("external_assets"))
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
    try:
        analysis = validate_analysis(value.get("analysis"))
    except ValueError as exc:
        raise ProfileError(str(exc)) from exc
    if analysis is not None:
        if machine not in {"q35", "pc"} and not machine.startswith(("pc-q35-", "pc-i440fx-")):
            raise ProfileError("malware-analysis requires the q35 or pc machine")
        if not target.startswith("x86_64-"):
            raise ProfileError("malware-analysis requires an x86_64 engine target")
    cpu = _cpu(value.get("cpu"), analysis=analysis is not None)
    configuration = dict(value)
    configuration["network"] = network
    configuration["cpu"] = cpu
    if analysis is not None:
        configuration["analysis"] = analysis
    analysis_payload = None
    if analysis is not None:
        resources = value.get("resources", {})
        analysis_payload = {
            "analysis": analysis,
            "network": network,
            "cpu": cpu,
            "vcpu": resources.get("vcpus", 1) if isinstance(resources, dict) else 1,
            "memory": resources.get("memory") if isinstance(resources, dict) else None,
        }
    return ResolvedProfile(profile_id, machine, target, configuration, manifest, executable, resolved_assets,
                           asset_kinds, analysis, analysis_payload)
