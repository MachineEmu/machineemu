"""Atomic, manifest-backed firmware preparation service.

Recipes write only into a private staging directory.  This coordinator verifies
the complete resulting bundle before an atomic publication, so callers never
observe a partially extracted firmware image.
"""
from __future__ import annotations

import fcntl
import os
import platform
import shutil
import tempfile
import zlib
from pathlib import Path
from types import ModuleType

from . import udm_pro, us24pro
from .models import Bundle, FirmwareError, FirmwareInfo, PrepareOptions, digest, load_bundle, manifest_for, write_json

RECIPES: dict[str, ModuleType] = {"udm-pro": udm_pro, "us24pro": us24pro}


def recipe(device: str) -> ModuleType:
    try:
        return RECIPES[device]
    except KeyError as exc:
        raise FirmwareError(f"unsupported firmware recipe: {device}") from exc


def inspect(source: Path, device: str) -> FirmwareInfo:
    return recipe(device).inspect(source.resolve())


def _tool_versions(device: str, options: PrepareOptions) -> dict[str, str]:
    versions = {"python": platform.python_version(), "zlib": zlib.ZLIB_RUNTIME_VERSION}
    if device == "udm-pro":
        from .squashfs import library_path

        library = library_path()
        versions["libsquashfs"] = digest(Path(library)) if Path(library).is_file() else library
        binary = shutil.which("mke2fs")
        if binary:
            versions["mke2fs"] = digest(Path(binary))
    if options.passwords:
        binary = shutil.which("openssl")
        if binary:
            versions["openssl"] = digest(Path(binary))
    if options.factory_lab_key is not None:
        try:
            import cryptography
        except ImportError as exc:
            raise FirmwareError("lab signing requires cryptography; install machineemu[lab]") from exc
        versions["cryptography"] = cryptography.__version__
    return versions


def prepare(source: Path, device: str, output: Path, options: PrepareOptions | None = None) -> Bundle:
    """Prepare ``source`` into a new immutable bundle directory.

    A pre-existing directory is accepted only when every source, recipe,
    option, tool and artifact digest agrees with this request.
    """
    source = source.resolve()
    output = output.absolute()
    options = options or PrepareOptions()
    selected = recipe(device)
    public = options.public()
    tools = _tool_versions(device, options)
    if output.exists() or output.is_symlink():
        if options.passwords:
            raise FirmwareError("password-bearing preparation requires a new output directory")
        bundle = load_bundle(output)
        manifest = bundle.manifest
        if (manifest["info"]["source_sha256"] != digest(source)
                or manifest["info"]["source_size"] != source.stat().st_size
                or manifest["info"]["device"] != device
                or manifest["recipe_revision"] != selected.REVISION
                or manifest["options"] != public
                or manifest["tools"] != tools):
            raise FirmwareError("prepared output differs; choose a new output directory")
        return bundle
    output.parent.mkdir(parents=True, exist_ok=True)
    staging = Path(tempfile.mkdtemp(prefix="." + output.name + "-", dir=output.parent))
    try:
        result = selected.prepare(source, staging, options)
        if digest(source) != result.info.source_sha256 or options.public() != public:
            raise FirmwareError("preparation inputs changed during extraction")
        write_json(staging / "manifest.json", manifest_for(result, staging, selected.REVISION, public, tools))
        load_bundle(staging)
        lock = output.parent / ("." + output.name + ".publish-lock")
        fd = os.open(lock, os.O_CREAT | os.O_WRONLY | os.O_NOFOLLOW, 0o600)
        try:
            try:
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError as exc:
                raise FirmwareError("another preparation is publishing this output") from exc
            if output.exists() or output.is_symlink():
                raise FirmwareError("output was created concurrently")
            staging.rename(output)
        finally:
            os.close(fd)
    finally:
        if staging.exists():
            shutil.rmtree(staging)
    return load_bundle(output)
