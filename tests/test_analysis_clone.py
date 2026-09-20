import json

import pytest

from machineemu.domains.analysis import create_clone


def test_analysis_clone_copies_only_declared_baseline_assets(tmp_path):
    disk = tmp_path / "disk.qcow2"
    vars_file = tmp_path / "vars.fd"
    disk.write_bytes(b"disk")
    vars_file.write_bytes(b"vars")
    result = create_clone(
        destination_root=tmp_path / "clones", clone_id="sample-1", identity_seed="seed",
        assets={"disk": disk, "firmware_vars": vars_file}, profile_revision="analysis-1",
    )
    clone = tmp_path / "clones" / "sample-1"
    assert result["clone_id"] == "sample-1"
    assert json.loads((clone / "clone.json").read_text())["identity"]["clone"] == "sample-1"
    assert (clone / disk.name).read_bytes() == b"disk"
    with pytest.raises(ValueError):
        create_clone(destination_root=tmp_path / "clones", clone_id="sample-1", identity_seed="seed",
                     assets={"disk": disk, "firmware_vars": vars_file}, profile_revision="analysis-1")
