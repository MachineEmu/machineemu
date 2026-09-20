import json

from machineemu.engines import EngineRegistry
from machineemu.profiles import build_launch_plan, resolve_profile


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
    assert plan.manifest["video_socket"].endswith("runtime/sockets/video.sock")
    assert plan.manifest["hwsim_control_socket"].endswith("runtime/sockets/wifi_hwsim.sock")
    assert plan.manifest["bluetooth_control_socket"].endswith("runtime/sockets/bluetooth_control.sock")
    assert plan.manifest["gdb"]["transport"] == "unix"
    assert plan.manifest["gdb"]["path"].endswith("runtime/sockets/gdb.sock")
    assert "-gdb" in plan.command
    assert "-serial" in plan.command and "chardev:machineemu-uart" in plan.command
