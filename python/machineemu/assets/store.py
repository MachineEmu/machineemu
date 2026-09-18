"""Import external files into an immutable SHA-256 asset store."""

from __future__ import annotations

import hashlib
from pathlib import Path
import shutil
import tempfile


class AssetError(ValueError):
    """Raised when an asset cannot be verified or published."""


class AssetStore:
    def __init__(self, root: Path):
        self.root = root.resolve()

    def import_file(self, source: Path, expected: str | None = None) -> tuple[str, Path]:
        if not source.is_file() or source.is_symlink():
            raise AssetError("asset source must be a regular file")
        hasher = hashlib.sha256()
        with source.open("rb") as stream:
            for block in iter(lambda: stream.read(1024 * 1024), b""):
                hasher.update(block)
        digest = hasher.hexdigest()
        reference = f"sha256:{digest}"
        if expected is not None and expected != reference:
            raise AssetError(f"asset digest mismatch: expected {expected}, got {reference}")
        destination = self.root / "sha256" / digest
        destination.parent.mkdir(parents=True, exist_ok=True)
        if destination.exists():
            if not destination.is_file() or destination.is_symlink():
                raise AssetError(f"asset store entry is not a regular file: {destination}")
            return reference, destination
        with tempfile.NamedTemporaryFile(dir=destination.parent, prefix=f".{digest}.", delete=False) as temporary:
            temporary_path = Path(temporary.name)
            with source.open("rb") as stream:
                shutil.copyfileobj(stream, temporary)
            temporary.flush()
        try:
            temporary_path.replace(destination)
        except FileExistsError:
            temporary_path.unlink(missing_ok=True)
        return reference, destination

    def resolve(self, reference: str) -> Path:
        if not isinstance(reference, str) or not reference.startswith("sha256:"):
            raise AssetError("asset reference must start with sha256:")
        digest = reference.removeprefix("sha256:")
        if len(digest) != 64 or any(char not in "0123456789abcdef" for char in digest):
            raise AssetError("asset reference must contain a SHA-256 digest")
        path = (self.root / "sha256" / digest).resolve()
        if self.root not in path.parents or not path.is_file():
            raise AssetError(f"asset is not available: {reference}")
        return path
