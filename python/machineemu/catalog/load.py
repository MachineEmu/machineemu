"""Load catalog profiles without resolving operator-specific paths."""

from __future__ import annotations

from pathlib import Path
import re
from typing import Any

from machineemu.documents import DocumentError, load_document


class CatalogError(ValueError):
    """Raised when a catalog profile is not redistributable metadata."""


_FIELD = re.compile(r"^[A-Za-z][A-Za-z0-9_.-]{0,63}$")


def _descriptor_map(value: Any, where: str) -> None:
    if not isinstance(value, dict):
        raise CatalogError(f"{where} must be a mapping")
    for name, descriptor in value.items():
        if not isinstance(name, str) or not _FIELD.fullmatch(name):
            raise CatalogError(f"{where} contains an invalid descriptor name")
        if isinstance(descriptor, bool):
            continue
        if not isinstance(descriptor, dict):
            raise CatalogError(f"{where}.{name} must be a boolean or descriptor mapping")
        kind = descriptor.get("type", descriptor.get("kind"))
        if kind is not None and (not isinstance(kind, str) or not kind or not _FIELD.fullmatch(kind)):
            raise CatalogError(f"{where}.{name}.type must be a safe non-empty string")
        if "available" in descriptor and not isinstance(descriptor["available"], bool):
            raise CatalogError(f"{where}.{name}.available must be boolean")
        if "reason" in descriptor and not isinstance(descriptor["reason"], str):
            raise CatalogError(f"{where}.{name}.reason must be a string")


def _validate_shape(value: dict[str, Any]) -> None:
    resources = value.get("resources")
    if resources is not None:
        if not isinstance(resources, dict):
            raise CatalogError("catalog profile resources must be a mapping")
        vcpus = resources.get("vcpus")
        if vcpus is not None and (not isinstance(vcpus, int) or isinstance(vcpus, bool) or not 1 <= vcpus <= 256):
            raise CatalogError("catalog profile resources.vcpus must be an integer from 1 to 256")
        for field in ("memory", "accelerator"):
            if field in resources and (not isinstance(resources[field], str) or not resources[field]):
                raise CatalogError(f"catalog profile resources.{field} must be a non-empty string")
    for field in ("devices", "capabilities"):
        if field in value:
            _descriptor_map(value[field], f"catalog profile {field}")
    external_assets = value.get("external_assets")
    if external_assets is not None:
        if not isinstance(external_assets, list):
            raise CatalogError("catalog profile external_assets must be a list")
        for index, asset in enumerate(external_assets):
            where = f"catalog profile external_assets[{index}]"
            if not isinstance(asset, dict):
                raise CatalogError(f"{where} must be a mapping")
            if not isinstance(asset.get("id"), str) or not _FIELD.fullmatch(asset["id"]):
                raise CatalogError(f"{where}.id must be a safe non-empty string")
            if "kind" in asset and (not isinstance(asset["kind"], str) or not _FIELD.fullmatch(asset["kind"])):
                raise CatalogError(f"{where}.kind must be a safe non-empty string")
            if "required" in asset and not isinstance(asset["required"], bool):
                raise CatalogError(f"{where}.required must be boolean")


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
        value = load_document(path)
    except DocumentError as exc:
        raise CatalogError(f"cannot read catalog profile {path}: {exc}") from exc
    if not isinstance(value, dict) or value.get("schema_version") != 1:
        raise CatalogError("catalog profile schema_version must be 1")
    for key in ("id", "domain", "machine"):
        if not isinstance(value.get(key), str) or not value[key]:
            raise CatalogError(f"catalog profile {key} must be a non-empty string")
    if not isinstance(value.get("engine"), dict) or not isinstance(value["engine"].get("track"), str):
        raise CatalogError("catalog profile engine.track is required")
    _validate_shape(value)
    _walk(value)
    return value
