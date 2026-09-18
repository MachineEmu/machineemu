import json

import pytest

from machineemu.engines import EngineRegistry
from machineemu.profiles import ProfileError, resolve_profile


def _registry(tmp_path):
    bundle = tmp_path / "bundles" / "track"
    (bundle / "bin").mkdir(parents=True)
    (bundle / "bin/qemu").write_bytes(b"qemu")
    digest = "c" * 64
    (bundle / "engine-build.json").write_text(json.dumps({
        "schema_version": 1,
        "track_id": "track",
        "build_digest": digest,
        "source_revision": "commit",
        "targets": ["aarch64-softmmu"],
        "executables": {"aarch64-softmmu": "bin/qemu"},
        "dirty_source": False,
    }), encoding="utf-8")
    release = tmp_path / "release.json"
    release.write_text(json.dumps({
        "schema_version": 1,
        "engines": {"track": {"manifest": "track/engine-build.json", "build_digest": digest}},
    }), encoding="utf-8")
    return EngineRegistry.load(release, tmp_path / "bundles")


def test_profile_resolves_engine_without_starting_process(tmp_path):
    profile = tmp_path / "profile.json"
    profile.write_text(json.dumps({
        "schema_version": 1,
        "id": "debian-aarch64",
        "engine": {"track": "track"},
        "machine": "virt",
        "resources": {"memory": "1GiB", "vcpus": 2},
        "assets": {"disk": "sha256:" + "d" * 64},
    }), encoding="utf-8")
    resolved = resolve_profile(profile, _registry(tmp_path), target="aarch64-softmmu")
    assert resolved.profile_id == "debian-aarch64"
    assert resolved.executable.name == "qemu"


def test_profile_rejects_host_path_asset(tmp_path):
    profile = tmp_path / "profile.json"
    profile.write_text(json.dumps({
        "schema_version": 1,
        "id": "bad",
        "engine": {"track": "track"},
        "machine": "virt",
        "assets": {"disk": "/absolute/disk.qcow2"},
    }), encoding="utf-8")
    with pytest.raises(ProfileError, match="sha256"):
        resolve_profile(profile, _registry(tmp_path), target="aarch64-softmmu")
