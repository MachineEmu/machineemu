"""Read-only inventory of state trees before migration or restore."""

from __future__ import annotations

from dataclasses import asdict, dataclass
import hashlib
import json
from pathlib import Path

from .state import RuntimeStateError


@dataclass(frozen=True)
class InventoryEntry:
    path: str
    size: int
    sha256: str


def inventory_tree(root: Path) -> list[InventoryEntry]:
    """Hash a state tree without modifying it or following symlinks."""
    root = root.resolve()
    if not root.is_dir():
        raise RuntimeStateError(f"inventory root is unavailable: {root}")
    entries: list[InventoryEntry] = []
    for path in sorted(root.rglob("*")):
        if path.is_symlink():
            raise RuntimeStateError(f"inventory refuses symlink: {path.relative_to(root)}")
        if not path.is_file():
            continue
        digest = hashlib.sha256()
        with path.open("rb") as stream:
            for block in iter(lambda: stream.read(1024 * 1024), b""):
                digest.update(block)
        entries.append(InventoryEntry(
            path=str(path.relative_to(root)), size=path.stat().st_size,
            sha256=f"sha256:{digest.hexdigest()}",
        ))
    return entries


def inventory_json(root: Path) -> dict[str, object]:
    entries = inventory_tree(root)
    return {
        "schema_version": 1,
        "root": str(root.resolve()),
        "files": [asdict(entry) for entry in entries],
        "file_count": len(entries),
    }
