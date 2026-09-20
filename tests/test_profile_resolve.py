import json

import pytest

from machineemu.assets import AssetStore
from machineemu.engines import EngineRegistry
from machineemu.profiles import ProfileError, build_launch_plan, resolve_profile, resolve_profile_value


def _registry(tmp_path, target="aarch64-softmmu"):
    bundle = tmp_path / "bundles" / "track"
    (bundle / "bin").mkdir(parents=True)
    (bundle / "bin/qemu").write_bytes(b"qemu")
    digest = "c" * 64
    (bundle / "engine-build.json").write_text(json.dumps({
        "schema_version": 1,
        "track_id": "track",
        "build_digest": digest,
        "source_revision": "commit",
        "targets": [target],
        "executables": {target: "bin/qemu"},
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


def test_profile_resolves_content_addressed_assets(tmp_path):
    source = tmp_path / "disk.qcow2"
    source.write_bytes(b"disk")
    store = AssetStore(tmp_path / "assets")
    reference, stored = store.import_file(source)
    profile = tmp_path / "profile.json"
    profile.write_text(json.dumps({
        "schema_version": 1,
        "id": "asset-backed",
        "engine": {"track": "track"},
        "machine": "virt",
        "assets": {"disk": reference},
    }), encoding="utf-8")

    resolved = resolve_profile(profile, _registry(tmp_path), target="aarch64-softmmu", asset_store=store)
    assert resolved.assets == {"disk": stored}


def test_analysis_profile_is_validated_and_launch_metadata_is_non_secret(tmp_path):
    profile = tmp_path / "profile.json"
    profile.write_text(json.dumps({
        "schema_version": 1, "id": "analysis", "engine": {"track": "track"}, "machine": "q35",
        "resources": {"memory": "8GiB", "vcpus": 4},
        "analysis": {"enabled": True, "profile": "malware-analysis", "identity_seed": "private-seed",
                      "smbios": {"system_product": "Analysis PC"}},
    }), encoding="utf-8")
    resolved = resolve_profile(profile, _registry(tmp_path, "x86_64-softmmu"), target="x86_64-softmmu")
    assert resolved.analysis["profile"] == "malware-analysis"
    assert "private-seed" not in json.dumps(resolved.analysis)
    plan = build_launch_plan(resolved, tmp_path / "runtime")
    assert "-uuid" in plan.command and "-cpu" in plan.command
    assert "kvm=off" in plan.command[plan.command.index("-cpu") + 1]
    assert plan.manifest["analysis_argv"]
    assert any("type=1" in argument and "uuid=" in argument for argument in plan.command)
    assert any("type=1" in argument and "product=Analysis PC" in argument for argument in plan.command)
    with pytest.raises(ProfileError, match="malware-analysis"):
        resolve_profile_value({"schema_version": 1, "id": "bad", "engine": {"track": "track"},
                               "machine": "q35", "analysis": {"enabled": True, "profile": "bad",
                               "identity_seed": "seed"}}, _registry(tmp_path / "bad", "x86_64-softmmu"), target="x86_64-softmmu")
