import json

import pytest

from machineemu.engines import EngineManifestError, load_manifest


def _write_manifest(tmp_path, **updates):
    value = {
        "schema_version": 1,
        "track_id": "upstream-10.2",
        "build_digest": "a" * 64,
        "source_revision": "commit",
        "targets": ["aarch64-softmmu"],
        "executables": {"aarch64-softmmu": "bin/qemu-system-aarch64"},
        "dirty_source": False,
        **updates,
    }
    path = tmp_path / "engine-build.json"
    path.write_text(json.dumps(value), encoding="utf-8")
    return path


def test_manifest_resolves_executable_below_bundle(tmp_path):
    path = _write_manifest(tmp_path)
    manifest = load_manifest(path, require_clean=True)
    assert manifest.executable("aarch64-softmmu") == (tmp_path / "bin/qemu-system-aarch64").resolve()


def test_manifest_rejects_path_escape(tmp_path):
    path = _write_manifest(tmp_path, executables={"aarch64-softmmu": "../qemu"})
    with pytest.raises(EngineManifestError, match="escapes"):
        load_manifest(path)


def test_manifest_rejects_dirty_release(tmp_path):
    path = _write_manifest(tmp_path, dirty_source=True)
    with pytest.raises(EngineManifestError, match="dirty"):
        load_manifest(path, require_clean=True)
