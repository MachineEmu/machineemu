import json
from types import SimpleNamespace

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
    app = create_app(OperatorApplication(config), token="test-token")
    client = TestClient(app, base_url="http://127.0.0.1")
    assert client.get("/api/v1/health").status_code == 401
    headers = {"X-MachineEmu-Token": "test-token"}
    assert client.get("/api/v1/health", headers=headers).json() == {"status": "ok"}
    response = client.get("/api/v1/sessions/instance-1/session-1", headers=headers)
    assert response.status_code == 200
    assert response.json()["state"] == "created"
    assert response.headers["x-content-type-options"] == "nosniff"


def test_api_requires_same_origin_for_mutations(tmp_path):
    config = OperatorConfig(
        tmp_path / "engines", tmp_path / "assets", tmp_path / "state",
        tmp_path / "runtime", tmp_path / "artifacts",
    )
    app = create_app(OperatorApplication(config), token="test-token")
    client = TestClient(app, base_url="http://127.0.0.1")
    headers = {"X-MachineEmu-Token": "test-token"}
    response = client.post("/api/v1/sessions/reconcile", json={
        "instance_id": "instance", "session_id": "session",
    }, headers=headers)
    assert response.status_code == 403


def test_api_start_and_stop_delegate_to_application(monkeypatch, tmp_path):
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
        "launch_plan": {"argv": ["/opt/qemu"], "qmp_socket": str(runtime / "sockets/qmp.sock")},
    }), encoding="utf-8")
    application = OperatorApplication(config)

    async def fake_start(record):
        return SimpleNamespace(process=SimpleNamespace(pid=1234))

    monkeypatch.setattr(application, "start_recorded_session", fake_start)
    monkeypatch.setattr(application, "stop_session", lambda record: 0)
    client = TestClient(create_app(application, token="test-token"), base_url="http://127.0.0.1")
    headers = {"X-MachineEmu-Token": "test-token", "Origin": "http://127.0.0.1"}
    started = client.post("/api/v1/sessions/instance-1/session-1/start", headers=headers)
    assert started.json() == {"session_id": "session-1", "pid": 1234, "state": "running"}
    stopped = client.post("/api/v1/sessions/instance-1/session-1/stop", headers=headers)
    assert stopped.json() == {"session_id": "session-1", "exit_code": 0, "state": "stopped"}
