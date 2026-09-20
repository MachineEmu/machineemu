"""Safe cloning of explicitly declared analysis baseline assets."""

from __future__ import annotations

import hashlib
import json
from pathlib import Path
import shutil
import tempfile
from typing import Any

from .identity import build_identity


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def create_clone(*, destination_root: Path, clone_id: str, identity_seed: str,
                 assets: dict[str, Path], profile_revision: str) -> dict[str, Any]:
    if not clone_id or "/" in clone_id or "\\" in clone_id or clone_id in {".", ".."}:
        raise ValueError("analysis clone ID is not safe")
    disk = assets.get("disk")
    vars_file = assets.get("firmware_vars", assets.get("nvram"))
    if disk is None or vars_file is None:
        raise ValueError("analysis clone requires declared disk and firmware_vars assets")
    selected = {"disk": disk, "firmware_vars": vars_file}
    if "tpm" in assets:
        selected["tpm"] = assets["tpm"]
    for name, path in selected.items():
        if path.is_symlink() or not path.is_file():
            raise ValueError(f"analysis asset is not a regular file: {name}")
    destination = destination_root / clone_id
    destination_root.mkdir(parents=True, exist_ok=True)
    if destination.exists() or destination.is_symlink():
        raise ValueError(f"analysis clone already exists: {clone_id}")
    temporary = Path(tempfile.mkdtemp(prefix=f".{clone_id}.", dir=destination_root))
    try:
        files: dict[str, dict[str, Any]] = {}
        for name, source in selected.items():
            target = temporary / source.name
            shutil.copy2(source, target, follow_symlinks=False)
            files[name] = {"name": target.name, "sha256": _sha256(target), "size": target.stat().st_size}
        manifest = {
            "schema_version": 1, "clone_id": clone_id, "profile_revision": profile_revision,
            "identity": build_identity(identity_seed, clone_id), "files": files,
        }
        (temporary / "clone.json").write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        temporary.replace(destination)
    except BaseException:
        shutil.rmtree(temporary, ignore_errors=True)
        raise
    return {"clone_id": clone_id, "profile_revision": profile_revision,
            "identity": manifest["identity"], "files": files, "state": "created"}
