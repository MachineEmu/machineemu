import json

import pytest

from machineemu.domains.analysis import create_clone, validate_clone


def test_analysis_clone_copies_only_declared_baseline_assets(tmp_path):
    disk = tmp_path / "disk.qcow2"
    vars_file = tmp_path / "vars.fd"
    disk.write_bytes(b"disk")
    vars_file.write_bytes(b"vars")
    qemu_img = tmp_path / "qemu-img"
    qemu_img.write_text(
        f"#!/bin/sh\nif [ \"$1\" = info ]; then echo '{{\"backing-filename\": \"{disk}\"}}'; else : > \"$8\"; fi\n",
        encoding="utf-8",
    )
    qemu_img.chmod(0o755)
    result = create_clone(
        destination_root=tmp_path / "clones", clone_id="sample-1", identity_seed="seed",
        assets={"disk": disk, "firmware_vars": vars_file}, profile_revision="analysis-1", qemu_img=qemu_img,
    )
    clone = tmp_path / "clones" / "sample-1"
    assert result["clone_id"] == "sample-1"
    assert json.loads((clone / "clone.json").read_text())["identity"]["clone"] == "sample-1"
    assert (clone / "overlay.qcow2").is_file()
    assert (clone / "OVMF_VARS.fd").read_bytes() == b"vars"
    validation = validate_clone(clone, qemu_img=qemu_img)
    assert validation["passed"] is True
    with pytest.raises(ValueError):
        create_clone(destination_root=tmp_path / "clones", clone_id="sample-1", identity_seed="seed",
                     assets={"disk": disk, "firmware_vars": vars_file}, profile_revision="analysis-1", qemu_img=qemu_img)
