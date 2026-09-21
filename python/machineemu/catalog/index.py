"""Index redistributable catalog profiles by opaque profile ID."""

from __future__ import annotations

from pathlib import Path
import re
from typing import Any

from machineemu.documents import DOCUMENT_SUFFIXES

from .load import CatalogError, load_profile

_ID = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$")


class ProfileCatalog:
    def __init__(self, root: Path):
        self.root = root.resolve()

    def list_profiles(self) -> list[dict[str, Any]]:
        profiles: list[dict[str, Any]] = []
        if not self.root.is_dir():
            raise CatalogError(f"catalog root is unavailable: {self.root}")
        paths = [path for suffix in DOCUMENT_SUFFIXES for path in self.root.glob(f"*{suffix}")]
        for path in sorted(paths):
            if path.is_symlink() or not path.is_file():
                continue
            profiles.append(load_profile(path))
        return profiles

    def _path(self, profile_id: str) -> Path:
        found = [path for suffix in DOCUMENT_SUFFIXES
                 if (path := self.root / f"{profile_id}{suffix}").is_file() and not path.is_symlink()]
        if not found:
            raise CatalogError(f"catalog profile is unavailable: {profile_id}")
        if len(found) > 1:
            names = ", ".join(sorted(path.name for path in found))
            raise CatalogError(f"catalog profile {profile_id} is defined more than once: {names}")
        return found[0]

    def get(self, profile_id: str) -> dict[str, Any]:
        if not isinstance(profile_id, str) or not _ID.fullmatch(profile_id):
            raise CatalogError("profile ID is not a valid catalog identifier")
        profile = load_profile(self._path(profile_id))
        if profile.get("id") != profile_id:
            raise CatalogError(f"catalog filename does not match profile id: {profile_id}")
        return profile
