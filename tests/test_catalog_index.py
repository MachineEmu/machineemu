import json

import pytest

from machineemu.catalog import CatalogError, ProfileCatalog


def test_profile_catalog_indexes_only_matching_profiles(tmp_path):
    profile = {
        "schema_version": 1, "id": "demo", "domain": "lab", "machine": "virt",
        "engine": {"track": "track"},
    }
    (tmp_path / "demo.json").write_text(json.dumps(profile), encoding="utf-8")
    (tmp_path / "other.json").write_text(json.dumps({**profile, "id": "wrong"}), encoding="utf-8")
    catalog = ProfileCatalog(tmp_path)
    assert [item["id"] for item in catalog.list_profiles()] == ["demo", "wrong"]
    assert catalog.get("demo")["machine"] == "virt"
    with pytest.raises(CatalogError, match="filename"):
        catalog.get("other")
