import json

from fastapi.testclient import TestClient

from machineemu.api import create_app
from machineemu.runtime import OperatorApplication, OperatorConfig


def test_api_health_and_session_inspection(tmp_path):
    config = OperatorConfig(
        tmp_path / "engines", tmp_path / "assets", tmp_path / "state",
        tmp_path / "runtime", tmp_path / "artifacts",
    )
    runtime = config.runtime_root / "sessions/session-1"
    state = config.state_root / "instances/instance-1"
    artifact = config.artifact_root / "sessions/session-1"
    for path in (runtime / "control", runtime / "sockets", runtime / "logs", state, artifact):
        path.mkdir(parents=True)
    (runtime / "manifest.json").write_text(json.dumps({
        "schema_version": 1, "session_id": "session-1", "instance_id": "instance-1", "state": "created",
    }), encoding="utf-8")
    client = TestClient(create_app(OperatorApplication(config)))
    assert client.get("/api/v1/health").json() == {"status": "ok"}
    response = client.get("/api/v1/sessions/instance-1/session-1")
    assert response.status_code == 200
    assert response.json()["state"] == "created"
