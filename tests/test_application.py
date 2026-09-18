import json

from machineemu.runtime import OperatorApplication, OperatorConfig


def test_application_creates_and_reads_recorded_plan(tmp_path):
    bundle = tmp_path / "bundles" / "track"
    (bundle / "bin").mkdir(parents=True)
    (bundle / "bin/qemu").write_bytes(b"qemu")
    digest = "1" * 64
    (bundle / "engine-build.json").write_text(json.dumps({
        "schema_version": 1, "track_id": "track", "build_digest": digest,
        "source_revision": "commit", "targets": ["aarch64-softmmu"],
        "executables": {"aarch64-softmmu": "bin/qemu"}, "dirty_source": False,
    }), encoding="utf-8")
    release = tmp_path / "release.json"
    release.write_text(json.dumps({"schema_version": 1, "engines": {
        "track": {"manifest": "track/engine-build.json", "build_digest": digest}
    }}), encoding="utf-8")
    profile = tmp_path / "profile.json"
    profile.write_text(json.dumps({
        "schema_version": 1, "id": "demo", "engine": {"track": "track"},
        "machine": "virt", "resources": {"memory": "1GiB", "vcpus": 1},
    }), encoding="utf-8")
    config = OperatorConfig(
        tmp_path / "engines", tmp_path / "assets", tmp_path / "state",
        tmp_path / "runtime", tmp_path / "artifacts",
    )
    app = OperatorApplication(config, release_set=release, bundle_root=tmp_path / "bundles")
    record = app.create_session(profile, target="aarch64-softmmu", instance_id="instance", session_id="session")
    command, qmp = app.recorded_plan(record)
    assert command[0].endswith("bin/qemu")
    assert qmp.name == "qmp.sock"
    assert (tmp_path / "state/instances/instance/instance.json").is_file()
