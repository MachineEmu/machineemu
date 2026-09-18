import json
import hashlib

import pytest

from machineemu.engines import EngineRegistry, EngineRegistryError


def test_registry_resolves_only_the_pinned_build(tmp_path):
    bundle = tmp_path / "bundles" / "unifi-10.2"
    (bundle / "bin").mkdir(parents=True)
    executable = bundle / "bin/qemu-system-aarch64"
    executable.write_bytes(b"qemu")
    executable_sha256 = hashlib.sha256(executable.read_bytes()).hexdigest()
    digest = "b" * 64
    (bundle / "engine-build.json").write_text(json.dumps({
        "schema_version": 1,
        "track_id": "unifi-10.2",
        "build_digest": digest,
        "source_revision": "commit",
        "targets": ["aarch64-softmmu"],
        "executables": {"aarch64-softmmu": "bin/qemu-system-aarch64"},
        "executable_sha256": {"aarch64-softmmu": executable_sha256},
        "dirty_source": False,
    }), encoding="utf-8")
    release_set = tmp_path / "release-set.json"
    release_set.write_text(json.dumps({
        "schema_version": 1,
        "engines": {"unifi-10.2": {"manifest": "unifi-10.2/engine-build.json", "build_digest": digest}},
    }), encoding="utf-8")

    manifest, resolved = EngineRegistry.load(release_set, tmp_path / "bundles").resolve(
        "unifi-10.2", "aarch64-softmmu"
    )
    assert manifest.build_digest == digest
    assert resolved == executable.resolve()


def test_registry_rejects_executable_digest_drift(tmp_path):
    bundle = tmp_path / "bundles"
    bundle.mkdir()
    executable = bundle / "qemu"
    executable.write_bytes(b"changed")
    release_set = tmp_path / "release-set.json"
    release_set.write_text(json.dumps({
        "schema_version": 1,
        "engines": {"track": {"manifest": "manifest.json", "build_digest": "a" * 64}},
    }), encoding="utf-8")
    (bundle / "manifest.json").write_text(json.dumps({
        "schema_version": 1, "track_id": "track", "build_digest": "a" * 64,
        "source_revision": "commit", "targets": ["target"], "dirty_source": False,
        "executables": {"target": "qemu"}, "executable_sha256": {"target": "b" * 64},
    }), encoding="utf-8")
    with pytest.raises(EngineRegistryError, match="executable digest"):
        EngineRegistry.load(release_set, bundle).resolve("track", "target")


def test_registry_rejects_digest_drift(tmp_path):
    release_set = tmp_path / "release-set.json"
    release_set.write_text(json.dumps({
        "schema_version": 1,
        "engines": {"unifi-10.2": {"manifest": "manifest.json", "build_digest": "a" * 64}},
    }), encoding="utf-8")
    (tmp_path / "manifest.json").write_text(json.dumps({
        "schema_version": 1,
        "track_id": "unifi-10.2",
        "build_digest": "b" * 64,
        "source_revision": "commit",
        "targets": ["aarch64-softmmu"],
        "executables": {"aarch64-softmmu": "qemu"},
        "dirty_source": False,
    }), encoding="utf-8")
    with pytest.raises(EngineRegistryError, match="digest"):
        EngineRegistry.load(release_set, tmp_path).resolve("unifi-10.2", "aarch64-softmmu")
