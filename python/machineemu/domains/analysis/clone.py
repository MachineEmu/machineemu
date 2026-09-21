"""Safe cloning of explicitly declared analysis baseline assets."""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
from typing import Any

from .identity import build_identity


_CLONE_ID = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$")


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def create_clone(*, destination_root: Path, clone_id: str, identity_seed: str,
                 assets: dict[str, Path], profile_revision: str,
                 qemu_img: str | Path | None = None) -> dict[str, Any]:
    if not isinstance(clone_id, str) or not _CLONE_ID.fullmatch(clone_id):
        raise ValueError("analysis clone ID is not safe")
    disk = assets.get("disk")
    vars_file = assets.get("firmware_vars", assets.get("nvram"))
    if disk is None or vars_file is None:
        raise ValueError("analysis clone requires declared disk and firmware_vars assets")
    selected = {"disk": disk, "firmware_vars": vars_file}
    if "tpm" in assets:
        selected["tpm"] = assets["tpm"]
    for name, path in selected.items():
        if path.is_symlink() or (not path.is_file() and not (name == "tpm" and path.is_dir())):
            raise ValueError(f"analysis asset is not a regular file: {name}")
    destination = destination_root / clone_id
    destination_root.mkdir(parents=True, exist_ok=True)
    if destination.exists() or destination.is_symlink():
        raise ValueError(f"analysis clone already exists: {clone_id}")
    baseline_disk_sha256 = _sha256(disk)
    baseline_vars_sha256 = _sha256(vars_file)
    temporary = Path(tempfile.mkdtemp(prefix=f".{clone_id}.", dir=destination_root))
    try:
        overlay = temporary / "overlay.qcow2"
        image_tool = str(qemu_img or os.environ.get("QEMU_IMG", "qemu-img"))
        result = subprocess.run(
            [image_tool, "create", "-f", "qcow2", "-F", "qcow2", "-b", str(disk), str(overlay)],
            capture_output=True, text=True, check=False,
        )
        if result.returncode:
            raise ValueError(result.stderr.strip() or "unable to create analysis clone overlay")
        vars_target = temporary / "OVMF_VARS.fd"
        shutil.copyfile(vars_file, vars_target)
        if _sha256(disk) != baseline_disk_sha256 or _sha256(vars_file) != baseline_vars_sha256:
            raise ValueError("analysis clone baseline changed while preparing clone")
        tpm_target: Path | None = None
        if "tpm" in assets:
            tpm_target = temporary / "tpm"
            shutil.copytree(assets["tpm"], tpm_target)
        final_overlay = destination / "overlay.qcow2"
        final_vars = destination / "OVMF_VARS.fd"
        final_tpm = destination / "tpm" if tpm_target is not None else None
        manifest = {
            "schema_version": 1, "clone_id": clone_id, "profile_revision": profile_revision,
            "baseline": str(disk), "baseline_sha256": baseline_disk_sha256,
            "ovmf_vars_sha256": baseline_vars_sha256,
            "directory": str(destination), "overlay": str(final_overlay), "ovmf_vars": str(final_vars),
            "tpm_directory": str(final_tpm) if final_tpm is not None else None,
            "tpm_copied": tpm_target is not None,
            "identity": build_identity(identity_seed, clone_id),
        }
        (temporary / "clone.json").write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        temporary.replace(destination)
    except BaseException:
        shutil.rmtree(temporary, ignore_errors=True)
        raise
    return {**manifest, "directory": str(destination), "state": "created"}


def _qemu_img_info(path: Path, qemu_img: str | Path | None = None) -> dict[str, Any]:
    image_tool = str(qemu_img or os.environ.get("QEMU_IMG", "qemu-img"))
    result = subprocess.run([image_tool, "info", "--output=json", str(path)],
                            capture_output=True, text=True, check=False)
    if result.returncode:
        raise ValueError(result.stderr.strip() or "unable to inspect analysis clone overlay")
    try:
        value = json.loads(result.stdout)
    except json.JSONDecodeError as exc:
        raise ValueError("qemu-img returned invalid overlay metadata") from exc
    if not isinstance(value, dict):
        raise ValueError("qemu-img returned invalid overlay metadata")
    return value


def validate_clone_metadata(metadata: dict[str, Any], *, qemu_img: str | Path | None = None) -> dict[str, Any]:
    """Validate clone files, identity metadata, and immutable backing state."""
    directory = Path(str(metadata.get("directory", "")))
    baseline = Path(str(metadata.get("baseline", "")))
    overlay = Path(str(metadata.get("overlay") or directory / "overlay.qcow2"))
    vars_file = Path(str(metadata.get("ovmf_vars") or directory / "OVMF_VARS.fd"))
    identity = metadata.get("identity")
    checks: dict[str, bool] = {
        "metadata": bool(metadata.get("clone_id")) and isinstance(identity, dict),
        "directory": directory.is_dir(),
        "overlay": overlay.is_file(),
        "baseline": baseline.is_file(),
        "ovmf_vars": vars_file.is_file(),
        "identity": isinstance(identity, dict) and isinstance(identity.get("uuid"), str)
        and isinstance(identity.get("mac"), str),
    }
    expected_disk = metadata.get("baseline_sha256")
    expected_vars = metadata.get("ovmf_vars_sha256")
    checks["baseline_unchanged"] = (
        baseline.is_file() and isinstance(expected_disk, str) and _sha256(baseline) == expected_disk
    )
    checks["ovmf_vars_copied"] = (
        vars_file.is_file() and isinstance(expected_vars, str) and _sha256(vars_file) == expected_vars
    )
    if metadata.get("tpm_copied") is True:
        checks["tpm_state"] = isinstance(metadata.get("tpm_directory"), str) and Path(str(metadata["tpm_directory"])).is_dir()
    overlay_info: dict[str, Any] | None = None
    if overlay.is_file():
        try:
            overlay_info = _qemu_img_info(overlay, qemu_img)
            backing = overlay_info.get("backing-filename") or overlay_info.get("full-backing-filename")
            if isinstance(backing, str) and not Path(backing).is_absolute():
                backing = str(overlay.parent / backing)
            checks["overlay_backing"] = isinstance(backing, str) and Path(backing).resolve() == baseline.resolve()
        except ValueError:
            checks["overlay_backing"] = False
    else:
        checks["overlay_backing"] = False
    return {"schema_version": 1, "passed": all(checks.values()), "checks": checks, "overlay_info": overlay_info}


def validate_clone(directory: Path, *, qemu_img: str | Path | None = None) -> dict[str, Any]:
    metadata_path = directory / "clone.json"
    try:
        metadata = json.loads(metadata_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        raise ValueError(f"unable to read analysis clone metadata: {metadata_path}") from exc
    if not isinstance(metadata, dict):
        raise ValueError("analysis clone metadata must be a JSON object")
    validation = validate_clone_metadata(metadata, qemu_img=qemu_img)
    metadata["validation"] = validation
    metadata_path.write_text(json.dumps(metadata, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return validation
