import json

import pytest

from machineemu.assets import AssetError, AssetStore
from machineemu.catalog import CatalogError, load_profile


def test_asset_store_verifies_and_deduplicates(tmp_path):
    source = tmp_path / "firmware.bin"
    source.write_bytes(b"firmware")
    store = AssetStore(tmp_path / "assets")
    reference, first = store.import_file(source)
    second_reference, second = store.import_file(source, expected=reference)
    assert reference == second_reference
    assert first == second
    assert store.resolve(reference).read_bytes() == b"firmware"


def test_asset_store_rejects_wrong_digest(tmp_path):
    source = tmp_path / "disk.qcow2"
    source.write_bytes(b"disk")
    with pytest.raises(AssetError, match="mismatch"):
        AssetStore(tmp_path / "assets").import_file(source, expected="sha256:" + "0" * 64)


def test_catalog_rejects_host_path(tmp_path):
    profile = tmp_path / "profile.json"
    profile.write_text(json.dumps({"schema_version": 1, "id": "bad", "domain": "x",
        "machine": "pc", "engine": {"track": "track"}, "path": "/host/file"}), encoding="utf-8")
    with pytest.raises(CatalogError, match="host-specific"):
        load_profile(profile)
