"""Index redistributable catalog profiles by opaque profile ID."""

from __future__ import annotations

from pathlib import Path
import re
from typing import Any

from .load import CatalogError, load_profile

_ID = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$")


class ProfileCatalog:
    def __init__(self, root: Path):
        self.root = root.resolve()

    def list_profiles(self) -> list[dict[str, Any]]:
        profiles: list[dict[str, Any]] = []
        if not self.root.is_dir():
            raise CatalogError(f"catalog root is unavailable: {self.root}")
        for path in sorted(self.root.glob("*.json")):
            if path.is_symlink() or not path.is_file():
                continue
            profiles.append(load_profile(path))
        return profiles

    def get(self, profile_id: str) -> dict[str, Any]:
        if not isinstance(profile_id, str) or not _ID.fullmatch(profile_id):
            raise CatalogError("profile ID is not a valid catalog identifier")
        path = self.root / f"{profile_id}.json"
        if path.is_symlink() or not path.is_file():
            raise CatalogError(f"catalog profile is unavailable: {profile_id}")
        profile = load_profile(path)
        if profile.get("id") != profile_id:
            raise CatalogError(f"catalog filename does not match profile id: {profile_id}")
        return profile
