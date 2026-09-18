import json

import pytest

from machineemu.runtime import InstanceStore, RuntimeStateError
from tests.test_runtime_state import _profile


def test_instance_store_persists_identity_once(tmp_path):
    profile = _profile(tmp_path)
    store = InstanceStore(tmp_path / "state")
    record = store.ensure("instance-1", profile)
    value = json.loads(record.manifest.read_text())
    assert value["profile_id"] == "debian"
    same = store.ensure("instance-1", profile)
    assert same == record


def test_instance_store_rejects_path_like_identity(tmp_path):
    with pytest.raises(RuntimeStateError, match="opaque"):
        InstanceStore(tmp_path / "state").ensure("../bad", _profile(tmp_path))


def test_instance_store_imports_state_with_digest_provenance(tmp_path):
    profile = _profile(tmp_path)
    store = InstanceStore(tmp_path / "state")
    record = store.ensure("instance-1", profile)
    source = tmp_path / "disk.qcow2"
    source.write_bytes(b"mutable disk")
    digest, destination = store.import_state_file(record, source, "disk.qcow2")
    value = json.loads(record.manifest.read_text())
    assert destination.read_bytes() == b"mutable disk"
    assert value["state_files"]["disk.qcow2"]["sha256"] == digest
    with pytest.raises(RuntimeStateError, match="already exists"):
        store.import_state_file(record, source, "disk.qcow2")


def test_instance_store_records_only_imported_backing_chain(tmp_path):
    profile = _profile(tmp_path)
    store = InstanceStore(tmp_path / "state")
    record = store.ensure("instance-1", profile)
    base = tmp_path / "base.img"
    overlay = tmp_path / "overlay.img"
    base.write_bytes(b"base")
    overlay.write_bytes(b"overlay")
    store.import_state_file(record, base, "base.img")
    store.import_state_file(record, overlay, "overlay.img")
    store.record_backing_chain(record, "overlay.img", ["base.img"])
    value = json.loads(record.manifest.read_text())
    assert value["state_files"]["overlay.img"]["backing_chain"] == ["base.img"]
    with pytest.raises(RuntimeStateError, match="not imported"):
        store.record_backing_chain(record, "overlay.img", ["missing.img"])
    with pytest.raises(RuntimeStateError, match="cycle"):
        store.record_backing_chain(record, "overlay.img", ["base.img", "base.img"])


def test_instance_store_publishes_immutable_snapshot(tmp_path):
    profile = _profile(tmp_path)
    store = InstanceStore(tmp_path / "state")
    record = store.ensure("instance-1", profile)
    source = tmp_path / "disk.img"
    source.write_bytes(b"disk")
    store.import_state_file(record, source, "disk.img")
    snapshot = store.snapshot(record, "cold-boot")
    assert (snapshot / "disk.img").read_bytes() == b"disk"
    manifest = json.loads((snapshot / "snapshot.json").read_text())
    assert manifest["instance_id"] == "instance-1"
    with pytest.raises(RuntimeStateError, match="already exists"):
        store.snapshot(record, "cold-boot")


def test_instance_store_stages_verified_restore_without_touching_live_state(tmp_path):
    profile = _profile(tmp_path)
    store = InstanceStore(tmp_path / "state")
    record = store.ensure("instance-1", profile)
    source = tmp_path / "disk.img"
    source.write_bytes(b"disk")
    store.import_state_file(record, source, "disk.img")
    store.snapshot(record, "cold-boot")
    staged = store.stage_snapshot_restore(record, "cold-boot")
    assert (staged / "disk.img").read_bytes() == b"disk"
    assert (record.state_dir / "disk.img").read_bytes() == b"disk"
    with pytest.raises(RuntimeStateError, match="already exists"):
        store.stage_snapshot_restore(record, "cold-boot")


def test_instance_store_applies_restore_only_when_stopped(tmp_path):
    profile = _profile(tmp_path)
    store = InstanceStore(tmp_path / "state")
    record = store.ensure("instance-1", profile)
    source = tmp_path / "disk.img"
    source.write_bytes(b"before")
    store.import_state_file(record, source, "disk.img")
    store.snapshot(record, "cold-boot")
    (record.state_dir / "disk.img").write_bytes(b"after")
    store.stage_snapshot_restore(record, "cold-boot")
    with pytest.raises(RuntimeStateError, match="stopped"):
        store.apply_staged_restore(record, "cold-boot", instance_state="running")
    store.apply_staged_restore(record, "cold-boot", instance_state="stopped")
    assert (record.state_dir / "disk.img").read_bytes() == b"before"
    assert json.loads(record.manifest.read_text())["last_restore"]["state"] == "applied"
