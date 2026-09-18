import json

import pytest

from machineemu.runtime import OperatorConfig, OperatorConfigError


def test_operator_config_resolves_roots_relative_to_file(tmp_path):
    path = tmp_path / "operator.json"
    path.write_text(json.dumps({
        "schema_version": 1,
        "roots": {
            "engine_root": "engines",
            "asset_root": "assets",
            "state_root": "state",
            "runtime_root": "runtime",
            "artifact_root": "artifacts",
        },
    }), encoding="utf-8")

    config = OperatorConfig.load(path)
    assert config.engine_root == (tmp_path / "engines").resolve()
    assert config.runtime_root == (tmp_path / "runtime").resolve()


def test_operator_config_requires_all_roots(tmp_path):
    path = tmp_path / "operator.json"
    path.write_text(json.dumps({"schema_version": 1, "roots": {}}), encoding="utf-8")
    with pytest.raises(OperatorConfigError, match="engine_root"):
        OperatorConfig.load(path)
