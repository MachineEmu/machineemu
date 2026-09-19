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


def test_api_lists_only_public_complete_session_summaries(tmp_path):
    config = OperatorConfig(
        tmp_path / "engines", tmp_path / "assets", tmp_path / "state",
        tmp_path / "runtime", tmp_path / "artifacts",
    )
    runtime = config.runtime_root / "sessions/session-1"
    state = config.state_root / "instances/instance-1"
    artifact = config.artifact_root / "sessions/session-1"
    for path in (runtime, state, artifact):
        path.mkdir(parents=True)
    (runtime / "manifest.json").write_text(json.dumps({
        "schema_version": 1, "session_id": "session-1", "instance_id": "instance-1",
        "profile_id": "udm-pro-lab", "machine": "udm-pro", "state": "stopped",
        "configuration": {"devices": {"lcd": True, "bluetooth": True}},
        "launch_plan": {"argv": ["/private/qemu"]},
    }), encoding="utf-8")
    # Partial directories are normal after interrupted work and are not listed.
    (config.runtime_root / "sessions/partial").mkdir(parents=True)

    client = TestClient(create_app(OperatorApplication(config), token="test-token"),
                        base_url="http://127.0.0.1")
    response = client.get("/api/v1/sessions", headers={"X-MachineEmu-Token": "test-token"})
    assert response.status_code == 200
    assert response.json() == {"sessions": [{
        "session_id": "session-1", "instance_id": "instance-1", "profile_id": "udm-pro-lab",
        "machine": "udm-pro", "state": "stopped",
        "capabilities": {"lcd_view": {"available": True}, "bluetooth": {"available": True}},
    }]}


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


def test_api_instance_inventory_is_read_only(tmp_path):
    config = OperatorConfig(
        tmp_path / "engines", tmp_path / "assets", tmp_path / "state",
        tmp_path / "runtime", tmp_path / "artifacts",
    )
    state = config.state_root / "instances/instance-1"
    state.mkdir(parents=True)
    (state / "instance.json").write_text(json.dumps({
        "schema_version": 1, "instance_id": "instance-1", "profile_id": "demo",
    }), encoding="utf-8")
    (state / "disk.img").write_bytes(b"disk")
    client = TestClient(create_app(OperatorApplication(config), token="test-token"),
                        base_url="http://127.0.0.1")
    response = client.get("/api/v1/instances/instance-1/state/inventory",
                          headers={"X-MachineEmu-Token": "test-token"})
    assert response.status_code == 200
    payload = response.json()
    assert payload["file_count"] == 2
    assert [item["path"] for item in payload["files"]] == ["disk.img", "instance.json"]


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


def test_api_catalog_is_read_only_and_id_indexed(tmp_path, monkeypatch):
    config = OperatorConfig(
        tmp_path / "engines", tmp_path / "assets", tmp_path / "state",
        tmp_path / "runtime", tmp_path / "artifacts",
    )
    catalog = tmp_path / "catalog"
    catalog.mkdir()
    (catalog / "demo.json").write_text(json.dumps({
        "schema_version": 1, "id": "demo", "domain": "lab", "machine": "virt",
        "engine": {"track": "track"},
    }), encoding="utf-8")
    application = OperatorApplication(config)
    client = TestClient(
        create_app(application, token="test-token", catalog_root=catalog),
        base_url="http://127.0.0.1",
    )
    headers = {"X-MachineEmu-Token": "test-token"}
    assert client.get("/api/v1/catalog/profiles", headers=headers).json()[0]["id"] == "demo"
    assert client.get("/api/v1/catalog/profiles/demo", headers=headers).json()["machine"] == "virt"

    class Record:
        session_id = "session-1"
        manifest = tmp_path / "manifest.json"

    Record.manifest.write_text("{}", encoding="utf-8")
    monkeypatch.setattr(application, "create_catalog_session", lambda *args, **kwargs: Record())
    created = client.post("/api/v1/catalog/sessions", headers={**headers, "Origin": "http://127.0.0.1"}, json={
        "profile_id": "demo", "instance_id": "instance-1", "session_id": "session-1",
    })
    assert created.status_code == 201
    assert created.json()["session_id"] == "session-1"
