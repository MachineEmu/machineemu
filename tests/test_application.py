import json

import pytest

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


def test_application_writes_analysis_environment_report_without_secret_or_host_paths(tmp_path):
    bundle = tmp_path / "bundles" / "track"
    (bundle / "bin").mkdir(parents=True)
    executable = bundle / "bin/qemu-system-x86_64"
    executable.write_bytes(b"analysis-qemu")
    digest = "2" * 64
    (bundle / "engine-build.json").write_text(json.dumps({
        "schema_version": 1, "track_id": "track", "build_digest": digest,
        "source_revision": "analysis-commit", "targets": ["x86_64-softmmu"],
        "executables": {"x86_64-softmmu": "bin/qemu-system-x86_64"}, "dirty_source": False,
    }), encoding="utf-8")
    release = tmp_path / "release.json"
    release.write_text(json.dumps({"schema_version": 1, "engines": {
        "track": {"manifest": "track/engine-build.json", "build_digest": digest}
    }}), encoding="utf-8")
    profile = tmp_path / "analysis.json"
    profile.write_text(json.dumps({
        "schema_version": 1, "id": "analysis", "engine": {"track": "track"},
        "machine": "q35", "resources": {"memory": "1GiB", "vcpus": 2},
        "network": {"type": "disabled"},
        "analysis": {"enabled": True, "profile": "malware-analysis", "identity_seed": "private-seed"},
    }), encoding="utf-8")
    config = OperatorConfig(
        tmp_path / "engines", tmp_path / "assets", tmp_path / "state",
        tmp_path / "runtime", tmp_path / "artifacts",
    )
    app = OperatorApplication(config, release_set=release, bundle_root=tmp_path / "bundles")
    record = app.create_session(profile, target="x86_64-softmmu", instance_id="instance", session_id="session")
    report_path = record.artifact_dir / "environment.json"
    report = json.loads(report_path.read_text(encoding="utf-8"))
    encoded = json.dumps(report)
    assert report["profile"] == "malware-analysis"
    assert report["identity_seed_sha256"]
    assert report["qemu"]["sha256"]
    assert report["network"] == {"type": "disabled"}
    assert report["endpoints"]["qmp"]["configured"] is True
    assert "private-seed" not in encoded
    assert str(tmp_path) not in encoded
    assert "private-seed" not in record.manifest.read_text(encoding="utf-8")


def test_application_rejects_catalog_profile_with_unresolved_external_assets(tmp_path):
    from machineemu.catalog import ProfileCatalog

    catalog_root = tmp_path / "catalog"
    catalog_root.mkdir()
    (catalog_root / "requires-assets.json").write_text(json.dumps({
        "schema_version": 1, "id": "requires-assets", "domain": "lab", "machine": "virt",
        "target": "aarch64-softmmu", "engine": {"track": "track"},
        "external_assets": [{"id": "disk", "required": True}],
    }), encoding="utf-8")
    config = OperatorConfig(
        tmp_path / "engines", tmp_path / "assets", tmp_path / "state",
        tmp_path / "runtime", tmp_path / "artifacts",
    )
    app = OperatorApplication(config, catalog=ProfileCatalog(catalog_root))
    app.release_set = tmp_path / "release.json"
    app.bundle_root = tmp_path / "bundles"
    with pytest.raises(ValueError, match="requires imported assets"):
        app.create_catalog_session("requires-assets", target=None, instance_id="instance", session_id="session")


def _analysis_catalog(tmp_path):
    from machineemu.catalog import ProfileCatalog

    catalog_root = tmp_path / "catalog"
    catalog_root.mkdir()
    (catalog_root / "analysis.json").write_text(json.dumps({
        "schema_version": 1, "id": "analysis", "domain": "sandbox", "machine": "q35",
        "target": "x86_64-softmmu", "engine": {"track": "track"},
        "resources": {"memory": "8GiB", "vcpus": 2},
        "cpu": "host,kvm=off",
        "analysis": {"enabled": True, "profile": "malware-analysis",
                     "identity_seed": "seed", "patch_revision": "analysis-1"},
    }), encoding="utf-8")
    return ProfileCatalog(catalog_root)


def test_analysis_clone_reads_the_instance_state_not_the_pristine_baseline(tmp_path, monkeypatch):
    config = OperatorConfig(
        tmp_path / "engines", tmp_path / "assets", tmp_path / "state",
        tmp_path / "runtime", tmp_path / "artifacts",
    )
    app = OperatorApplication(config, catalog=_analysis_catalog(tmp_path))

    class Profile:
        analysis = {"profile": "malware-analysis", "patch_revision": "analysis-1"}
        assets = {"disk": tmp_path / "assets" / "pristine-baseline.qcow2"}

    monkeypatch.setattr(app, "preview_catalog_profile",
                        lambda profile_id, target=None: (app.catalog.get(profile_id), Profile(), None))

    state_dir = config.state_root / "instances" / "instance-1"
    state_dir.mkdir(parents=True)
    (state_dir / "instance.json").write_text(json.dumps({
        "schema_version": 1, "instance_id": "instance-1", "profile_id": "analysis"}), encoding="utf-8")
    (state_dir / "overlay.qcow2").write_bytes(b"accumulated disk")
    (state_dir / "OVMF_VARS.fd").write_bytes(b"enrolled keys")
    (state_dir / "tpm").mkdir()
    (state_dir / "tpm" / "tpm2-00.permall").write_bytes(b"\0" * 4096)

    captured = {}
    monkeypatch.setattr("machineemu.runtime.application.create_clone",
                        lambda **kwargs: captured.update(kwargs) or {"clone_id": kwargs["clone_id"]})
    app.create_analysis_clone("analysis", "clone-1", instance_id="instance-1")

    assert captured["assets"] == {
        "disk": state_dir / "overlay.qcow2",
        "firmware_vars": state_dir / "OVMF_VARS.fd",
        "tpm": state_dir / "tpm",
    }
    assert Profile.assets["disk"] not in captured["assets"].values()


def test_analysis_clone_refuses_an_instance_that_has_never_run(tmp_path, monkeypatch):
    config = OperatorConfig(
        tmp_path / "engines", tmp_path / "assets", tmp_path / "state",
        tmp_path / "runtime", tmp_path / "artifacts",
    )
    app = OperatorApplication(config, catalog=_analysis_catalog(tmp_path))

    class Profile:
        analysis = {"profile": "malware-analysis", "patch_revision": "analysis-1"}
        assets = {}

    monkeypatch.setattr(app, "preview_catalog_profile",
                        lambda profile_id, target=None: (app.catalog.get(profile_id), Profile(), None))
    state_dir = config.state_root / "instances" / "instance-1"
    state_dir.mkdir(parents=True)
    (state_dir / "instance.json").write_text(json.dumps({
        "schema_version": 1, "instance_id": "instance-1", "profile_id": "analysis"}), encoding="utf-8")

    with pytest.raises(ValueError, match="no machine state to clone"):
        app.create_analysis_clone("analysis", "clone-1", instance_id="instance-1")


def test_analysis_clone_refuses_an_instance_from_a_different_profile(tmp_path, monkeypatch):
    config = OperatorConfig(
        tmp_path / "engines", tmp_path / "assets", tmp_path / "state",
        tmp_path / "runtime", tmp_path / "artifacts",
    )
    app = OperatorApplication(config, catalog=_analysis_catalog(tmp_path))

    class Profile:
        analysis = {"profile": "malware-analysis", "patch_revision": "analysis-1"}
        assets = {}

    monkeypatch.setattr(app, "preview_catalog_profile",
                        lambda profile_id, target=None: (app.catalog.get(profile_id), Profile(), None))
    state_dir = config.state_root / "instances" / "instance-1"
    state_dir.mkdir(parents=True)
    (state_dir / "instance.json").write_text(json.dumps({
        "schema_version": 1, "instance_id": "instance-1", "profile_id": "other"}), encoding="utf-8")

    with pytest.raises(ValueError, match="not created from profile"):
        app.create_analysis_clone("analysis", "clone-1", instance_id="instance-1")
