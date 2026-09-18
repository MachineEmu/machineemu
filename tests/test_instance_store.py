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
