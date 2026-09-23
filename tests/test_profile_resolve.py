import base64
import json
from pathlib import Path

import pytest

from machineemu.assets import AssetStore
from machineemu.catalog import load_profile
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
    assert plan.command[plan.command.index("-machine") + 1].startswith("q35,analysis-profile=on,")
    machine_argument = plan.command[plan.command.index("-machine") + 1]
    encoded = machine_argument.split("x-analysis-profile-json-base64=", 1)[1].split(",", 1)[0]
    assert json.loads(base64.b64decode(encoded))["analysis"]["profile"] == "malware-analysis"
    assert "private-seed" not in json.dumps(plan.manifest)
    assert plan.manifest["cpu"] == "host,kvm=off"
    assert plan.command[plan.command.index("-cpu") + 1] == "host,kvm=off"
    assert any("type=1" in argument and "uuid=" in argument for argument in plan.command)
    assert any("type=1" in argument and "product=Analysis PC" in argument for argument in plan.command)
    with pytest.raises(ProfileError, match="malware-analysis"):
        resolve_profile_value({"schema_version": 1, "id": "bad", "engine": {"track": "track"},
                               "machine": "q35", "analysis": {"enabled": True, "profile": "bad",
                               "identity_seed": "seed"}}, _registry(tmp_path / "bad", "x86_64-softmmu"), target="x86_64-softmmu")


def test_analysis_profile_rejects_cpu_hypervisor_policy(tmp_path):
    with pytest.raises(ProfileError, match="CPU policy"):
        resolve_profile_value({
            "schema_version": 1, "id": "bad-cpu", "engine": {"track": "track"}, "machine": "q35",
            "cpu": "host,hypervisor=on", "analysis": {
                "enabled": True, "profile": "malware-analysis", "identity_seed": "seed",
            },
        }, _registry(tmp_path, "x86_64-softmmu"), target="x86_64-softmmu")


@pytest.mark.parametrize(("network", "expected"), [
    ({"type": "disabled"}, "none"),
    ({"type": "user"}, "user"),
    ({"type": "bridge", "bridge": "br-analysis"}, "bridge,br=br-analysis"),
])
def test_profile_network_mode_is_resolved_into_launch_plan(tmp_path, network, expected):
    profile = {
        "schema_version": 1, "id": "networked", "engine": {"track": "track"},
        "machine": "virt", "resources": {"memory": "1GiB"}, "network": network,
    }
    resolved = resolve_profile_value(profile, _registry(tmp_path), target="aarch64-softmmu")
    plan = build_launch_plan(resolved, tmp_path / "runtime")
    assert plan.command[plan.command.index("-nic") + 1] == expected
    assert plan.manifest["network"] == network


def test_profile_rejects_unsafe_or_incomplete_network_mode(tmp_path):
    registry = _registry(tmp_path)
    with pytest.raises(ProfileError, match="network.type"):
        resolve_profile_value({"schema_version": 1, "id": "bad", "engine": {"track": "track"},
                               "machine": "virt", "network": {"type": "host"}}, registry,
                              target="aarch64-softmmu")
    with pytest.raises(ProfileError, match="bridge"):
        resolve_profile_value({"schema_version": 1, "id": "bad", "engine": {"track": "track"},
                               "machine": "virt", "network": {"type": "bridge", "bridge": "bad/name"}}, registry,
                              target="aarch64-softmmu")


def test_catalog_analysis_profile_preserves_non_secret_descriptor_set(tmp_path):
    catalog_path = Path(__file__).parents[1] / "catalog" / "profiles" / "malware-analysis-x64.json"
    value = load_profile(catalog_path)
    value["engine"] = {"track": "track"}
    # The pinned analysis firmware needs an asset store; this test is about the
    # analysis descriptors only.
    value["assets"] = {}
    resolved = resolve_profile_value(value, _registry(tmp_path, "x86_64-softmmu"), target="x86_64-softmmu")

    assert resolved.analysis["smbios"]["system_product"] == "NUC11TNKi5"
    assert resolved.analysis["acpi"]["oem_id"] == "INTEL"
    assert resolved.analysis["device_descriptors"]["display"]["pci_identity"] is True
    assert resolved.analysis["sensors"]["fan_rpm"] == 1200
    assert "catalog-analysis-default" not in json.dumps(resolved.analysis)
    assert resolved.configuration["cpu"].startswith("host,kvm=off,-hypervisor")
