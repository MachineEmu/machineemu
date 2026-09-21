import importlib.util
import json

import pytest

from machineemu.catalog import CatalogError, ProfileCatalog
from machineemu.documents import DocumentError, load_document
from machineemu.runtime import OperatorConfig

needs_yaml = pytest.mark.skipif(importlib.util.find_spec("yaml") is None,
                                reason="PyYAML is not installed; install the yaml extra")

PROFILE = {
    "schema_version": 1,
    "id": "demo",
    "domain": "sandbox",
    "machine": "q35",
    "engine": {"track": "track"},
    "resources": {"memory": "1GiB", "vcpus": 2},
}


@needs_yaml
def test_json_and_yaml_documents_parse_to_the_same_value(tmp_path):
    (tmp_path / "a.json").write_text(json.dumps(PROFILE), encoding="utf-8")
    (tmp_path / "a.yaml").write_text(
        "schema_version: 1\n"
        "id: demo\n"
        "domain: sandbox\n"
        "machine: q35\n"
        "engine:\n  track: track\n"
        "resources:\n  memory: 1GiB\n  vcpus: 2\n", encoding="utf-8")
    assert load_document(tmp_path / "a.json") == load_document(tmp_path / "a.yaml") == PROFILE


@needs_yaml
def test_yaml_documents_cannot_construct_python_objects(tmp_path):
    path = tmp_path / "unsafe.yaml"
    path.write_text("!!python/object/apply:os.system ['true']\n", encoding="utf-8")
    with pytest.raises(DocumentError, match="cannot parse"):
        load_document(path)


def test_an_unknown_suffix_is_refused_rather_than_guessed(tmp_path):
    path = tmp_path / "profile.toml"
    path.write_text("id = 'demo'\n", encoding="utf-8")
    with pytest.raises(DocumentError, match="unsupported configuration format"):
        load_document(path)


@needs_yaml
def test_catalog_lists_and_gets_profiles_written_in_either_format(tmp_path):
    (tmp_path / "demo.json").write_text(json.dumps(PROFILE), encoding="utf-8")
    (tmp_path / "other.yml").write_text(
        "schema_version: 1\nid: other\ndomain: sandbox\nmachine: q35\nengine:\n  track: track\n",
        encoding="utf-8")
    catalog = ProfileCatalog(tmp_path)

    assert sorted(profile["id"] for profile in catalog.list_profiles()) == ["demo", "other"]
    assert catalog.get("other")["id"] == "other"


def test_catalog_refuses_a_profile_id_defined_in_two_formats(tmp_path):
    (tmp_path / "demo.json").write_text(json.dumps(PROFILE), encoding="utf-8")
    (tmp_path / "demo.yaml").write_text("schema_version: 1\nid: demo\n", encoding="utf-8")
    with pytest.raises(CatalogError, match="defined more than once"):
        ProfileCatalog(tmp_path).get("demo")


@needs_yaml
def test_operator_config_loads_from_yaml(tmp_path):
    path = tmp_path / "operator.yaml"
    path.write_text(
        "schema_version: 1\n"
        "roots:\n"
        f"  engine_root: {tmp_path}/engines\n"
        f"  asset_root: {tmp_path}/assets\n"
        f"  state_root: {tmp_path}/state\n"
        f"  runtime_root: {tmp_path}/run\n"
        f"  artifact_root: {tmp_path}/artifacts\n", encoding="utf-8")

    config = OperatorConfig.load(path)
    assert config.asset_root == tmp_path / "assets"
