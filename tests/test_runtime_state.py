import json

import pytest

from machineemu.engines import EngineRegistry
from machineemu.profiles import resolve_profile
from machineemu.runtime import RuntimeStateError, SessionStore


def _profile(tmp_path):
    bundle = tmp_path / "bundles" / "track"
    (bundle / "bin").mkdir(parents=True)
    (bundle / "bin/qemu").write_bytes(b"qemu")
    digest = "e" * 64
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
    profile.write_text(json.dumps({"schema_version": 1, "id": "debian",
        "engine": {"track": "track"}, "machine": "virt"}), encoding="utf-8")
    return resolve_profile(profile, EngineRegistry.load(release, tmp_path / "bundles"), target="aarch64-softmmu")


def test_session_store_separates_runtime_state_and_artifacts(tmp_path):
    record = SessionStore(tmp_path / "run", tmp_path / "state", tmp_path / "artifacts").create(
        "instance-1", "session-1", _profile(tmp_path)
    )
    assert record.runtime_dir == (tmp_path / "run/sessions/session-1").resolve()
    assert record.state_dir == (tmp_path / "state/instances/instance-1").resolve()
    assert record.artifact_dir == (tmp_path / "artifacts/sessions/session-1").resolve()
    assert json.loads(record.manifest.read_text())["engine"]["build_digest"] == "e" * 64
    manifest = json.loads(record.manifest.read_text())
    assert manifest["configuration"]["machine"] == "virt"
    assert manifest["assets"] == {}
    assert (record.runtime_dir / "sockets").is_dir()


def test_session_store_rejects_path_like_ids(tmp_path):
    with pytest.raises(RuntimeStateError, match="opaque"):
        SessionStore(tmp_path / "run", tmp_path / "state", tmp_path / "artifacts").create(
            "../instance", "session", _profile(tmp_path)
        )


def test_session_store_rejects_duplicate_session(tmp_path):
    store = SessionStore(tmp_path / "run", tmp_path / "state", tmp_path / "artifacts")
    profile = _profile(tmp_path)
    store.create("instance-1", "session-1", profile)
    with pytest.raises(RuntimeStateError, match="already exists"):
        store.create("instance-2", "session-1", profile)
