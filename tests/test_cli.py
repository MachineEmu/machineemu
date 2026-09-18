import json

from machineemu.cli import main


def test_asset_import_cli(tmp_path, capsys):
    source = tmp_path / "disk.img"
    source.write_bytes(b"disk")
    assert main(["asset-import", "--asset-root", str(tmp_path / "assets"), "--source", str(source)]) == 0
    output = json.loads(capsys.readouterr().out)
    assert output["reference"].startswith("sha256:")


def test_profile_check_cli(tmp_path, capsys):
    bundle = tmp_path / "bundles" / "track"
    (bundle / "bin").mkdir(parents=True)
    (bundle / "bin/qemu").write_bytes(b"qemu")
    digest = "a" * 64
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
        "schema_version": 1, "id": "demo", "engine": {"track": "track"}, "machine": "virt"
    }), encoding="utf-8")

    assert main([
        "profile-check", "--release-set", str(release), "--bundle-root", str(tmp_path / "bundles"),
        "--profile", str(profile), "--target", "aarch64-softmmu",
    ]) == 0
    output = json.loads(capsys.readouterr().out)
    assert output["profile_id"] == "demo"
    assert output["engine"]["build_digest"] == digest


def test_session_create_cli_writes_manifest(tmp_path, capsys):
    bundle = tmp_path / "bundles" / "track"
    (bundle / "bin").mkdir(parents=True)
    (bundle / "bin/qemu").write_bytes(b"qemu")
    digest = "b" * 64
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
        "schema_version": 1, "id": "demo", "engine": {"track": "track"}, "machine": "virt"
    }), encoding="utf-8")
    config = tmp_path / "operator.json"
    config.write_text(json.dumps({"schema_version": 1, "roots": {
        "engine_root": "engines", "asset_root": "assets", "state_root": "state",
        "runtime_root": "runtime", "artifact_root": "artifacts",
    }}), encoding="utf-8")

    assert main([
        "session-create", "--operator-config", str(config), "--release-set", str(release),
        "--bundle-root", str(tmp_path / "bundles"), "--profile", str(profile),
        "--target", "aarch64-softmmu", "--instance-id", "instance-1", "--session-id", "session-1",
    ]) == 0
    output = json.loads(capsys.readouterr().out)
    manifest = json.loads((tmp_path / "runtime/sessions/session-1/manifest.json").read_text())
    assert output["session_id"] == "session-1"
    assert manifest["profile_id"] == "demo"

    assert main([
        "session-inspect", "--operator-config", str(config),
        "--instance-id", "instance-1", "--session-id", "session-1",
    ]) == 0
    inspected = json.loads(capsys.readouterr().out)
    assert inspected["session_id"] == "session-1"
