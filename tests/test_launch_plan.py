import json

import pytest

from machineemu.assets import AssetStore
from machineemu.engines import EngineRegistry
from machineemu.profiles import ProfileError, build_launch_plan, resolve_profile


def test_launch_plan_is_deterministic_and_does_not_start_process(tmp_path):
    bundle = tmp_path / "bundles" / "track"
    (bundle / "bin").mkdir(parents=True)
    (bundle / "bin/qemu").write_bytes(b"qemu")
    digest = "f" * 64
    (bundle / "engine-build.json").write_text(json.dumps({
        "schema_version": 1, "track_id": "track", "build_digest": digest,
        "source_revision": "commit", "targets": ["aarch64-softmmu"],
        "executables": {"aarch64-softmmu": "bin/qemu"}, "dirty_source": False,
    }), encoding="utf-8")
    release = tmp_path / "release.json"
    release.write_text(json.dumps({"schema_version": 1, "engines": {
        "track": {"manifest": "track/engine-build.json", "build_digest": digest}
    }}), encoding="utf-8")
    profile_path = tmp_path / "profile.json"
    profile_path.write_text(json.dumps({
        "schema_version": 1, "id": "demo", "engine": {"track": "track"},
        "machine": "virt", "resources": {"memory": "1GiB", "vcpus": 2},
        "console": {"uart": True}, "debug": {"enabled": True, "transport": "unix"}, "devices": {
            "vnc": True, "video": True, "wifi_hwsim": True, "bluetooth_control": True,
        },
    }), encoding="utf-8")
    profile = resolve_profile(profile_path, EngineRegistry.load(release, tmp_path / "bundles"), target="aarch64-softmmu")

    plan = build_launch_plan(profile, tmp_path / "runtime")
    assert plan.command[0].endswith("bin/qemu")
    assert "-machine" in plan.command and "virt" in plan.command
    assert plan.manifest["engine"]["build_digest"] == digest
    assert plan.manifest["qmp_socket"].endswith("runtime/sockets/qmp.sock")
    assert plan.manifest["uart_socket"].endswith("runtime/sockets/uart.sock")
    assert plan.manifest["vnc_socket"].endswith("runtime/sockets/vnc.sock")
    assert "-vnc" in plan.command
    assert plan.command[plan.command.index("-vnc") + 1].endswith("runtime/sockets/vnc.sock")
    assert plan.manifest["video_socket"].endswith("runtime/sockets/video.sock")
    assert plan.manifest["hwsim_control_socket"].endswith("runtime/sockets/wifi_hwsim.sock")
    assert plan.manifest["bluetooth_control_socket"].endswith("runtime/sockets/bluetooth_control.sock")
    assert plan.manifest["gdb"]["transport"] == "unix"
    assert plan.manifest["gdb"]["path"].endswith("runtime/sockets/gdb.sock")
    assert "-gdb" in plan.command
    assert "-serial" in plan.command and "chardev:machineemu-uart" in plan.command


def _asset_backed_profile(tmp_path, kinds):
    bundle = tmp_path / "bundles" / "track"
    (bundle / "bin").mkdir(parents=True)
    (bundle / "bin/qemu").write_bytes(b"qemu")
    digest = "f" * 64
    (bundle / "engine-build.json").write_text(json.dumps({
        "schema_version": 1, "track_id": "track", "build_digest": digest,
        "source_revision": "commit", "targets": ["aarch64-softmmu"],
        "executables": {"aarch64-softmmu": "bin/qemu"}, "dirty_source": False,
    }), encoding="utf-8")
    release = tmp_path / "release.json"
    release.write_text(json.dumps({"schema_version": 1, "engines": {
        "track": {"manifest": "track/engine-build.json", "build_digest": digest}
    }}), encoding="utf-8")
    store = AssetStore(tmp_path / "assets")
    assets = {}
    stored = {}
    for name in kinds:
        source = tmp_path / f"{name}.bin"
        source.write_bytes(name.encode())
        assets[name], stored[name] = store.import_file(source)
    profile_path = tmp_path / "profile.json"
    profile_path.write_text(json.dumps({
        "schema_version": 1, "id": "assets", "engine": {"track": "track"},
        "machine": "virt", "resources": {"memory": "1GiB", "vcpus": 1},
        "external_assets": [{"id": name, "kind": kind} for name, kind in kinds.items() if kind is not None],
        "assets": assets,
    }), encoding="utf-8")
    profile = resolve_profile(profile_path, EngineRegistry.load(release, tmp_path / "bundles"),
                              target="aarch64-softmmu", asset_store=store)
    return profile, stored


def test_launch_plan_wires_assets_by_declared_kind(tmp_path):
    profile, stored = _asset_backed_profile(tmp_path, {
        "disk": "qcow2-analysis-baseline",
        "firmware_vars": "ovmf-vars",
        "firmware_code": "ovmf-code",
        "installer": "iso",
    })

    plan = build_launch_plan(profile, tmp_path / "runtime")
    drives = [plan.command[index + 1] for index, value in enumerate(plan.command) if value == "-drive"]
    assert f"file={stored['disk']},if=virtio,format=qcow2" in drives
    assert f"file={stored['firmware_vars']},if=pflash,format=raw,unit=1" in drives
    assert f"file={stored['firmware_code']},if=pflash,format=raw,unit=0,readonly=on" in drives
    assert f"file={stored['installer']},media=cdrom,readonly=on" in drives


def test_launch_plan_defaults_to_raw_disk_without_a_declared_kind(tmp_path):
    profile, stored = _asset_backed_profile(tmp_path, {"disk": None})

    plan = build_launch_plan(profile, tmp_path / "runtime")
    assert f"file={stored['disk']},if=virtio,format=raw" in plan.command


def test_launch_plan_rejects_an_asset_kind_it_cannot_wire(tmp_path):
    profile, _ = _asset_backed_profile(tmp_path, {"bundle": "prepared-firmware-bundle"})
    with pytest.raises(ProfileError, match="prepared-firmware-bundle"):
        build_launch_plan(profile, tmp_path / "runtime")
