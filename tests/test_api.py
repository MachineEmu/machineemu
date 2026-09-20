import json
from pathlib import Path
import shutil
import socket
import tempfile
import threading
from types import SimpleNamespace

from fastapi.testclient import TestClient

from machineemu.api import create_app
from machineemu.runtime import OperatorApplication, OperatorConfig
from machineemu.runtime.terminal import TerminalTicketStore


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


def test_api_exposes_bounded_public_diagnostics_without_runtime_paths(tmp_path):
    config = OperatorConfig(
        tmp_path / "engines", tmp_path / "assets", tmp_path / "state",
        tmp_path / "runtime", tmp_path / "artifacts",
    )
    runtime = config.runtime_root / "sessions/session-1"
    for path in (runtime / "control", runtime / "sockets", runtime / "logs",
                 config.state_root / "instances/instance-1", config.artifact_root / "sessions/session-1"):
        path.mkdir(parents=True)
    (runtime / "manifest.json").write_text(json.dumps({
        "schema_version": 1, "session_id": "session-1", "instance_id": "instance-1",
        "state": "stopped", "configuration": {"adapter": "pc", "devices": {"vnc": True}},
        "analysis": {"schema_version": 1, "profile": "malware-analysis"},
    }), encoding="utf-8")
    (runtime / "logs/stdout.log").write_text("one\ntwo\nthree\n", encoding="utf-8")
    client = TestClient(create_app(OperatorApplication(config), token="test-token"),
                        base_url="http://127.0.0.1")
    headers = {"X-MachineEmu-Token": "test-token"}
    hardware = client.get("/api/v1/sessions/instance-1/session-1/hardware-config", headers=headers)
    environment = client.get("/api/v1/sessions/instance-1/session-1/environment", headers=headers)
    logs = client.get("/api/v1/sessions/instance-1/session-1/logs?tail=2", headers=headers)
    assert hardware.json()["configuration"]["adapter"] == "pc"
    assert environment.json()["analysis"]["profile"] == "malware-analysis"
    assert logs.json()["lines"] == ["two", "three"]
    assert str(runtime) not in json.dumps(hardware.json())


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


def test_api_delete_records_operation_and_removes_only_session_runtime(tmp_path):
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
        "schema_version": 1, "session_id": "session-1", "instance_id": "instance-1", "state": "stopped",
    }), encoding="utf-8")
    client = TestClient(create_app(OperatorApplication(config), token="test-token"),
                        base_url="http://127.0.0.1")
    headers = {"X-MachineEmu-Token": "test-token", "Origin": "http://127.0.0.1"}
    response = client.delete("/api/v1/sessions/instance-1/session-1", headers=headers)
    assert response.status_code == 202
    operation = response.json()
    assert operation["state"] == "succeeded"
    assert client.get(f"/api/v1/operations/{operation['operation_id']}", headers=headers).json() == operation
    assert not runtime.exists()
    assert not artifact.exists()
    assert state.exists()


def test_api_preserves_source_capability_vocabulary_and_reasons(tmp_path):
    config = OperatorConfig(
        tmp_path / "engines", tmp_path / "assets", tmp_path / "state",
        tmp_path / "runtime", tmp_path / "artifacts",
    )
    runtime = config.runtime_root / "sessions/session-1"
    for path in (runtime, config.state_root / "instances/instance-1", config.artifact_root / "sessions/session-1"):
        path.mkdir(parents=True)
    (runtime / "manifest.json").write_text(json.dumps({
        "schema_version": 1, "session_id": "session-1", "instance_id": "instance-1",
        "profile_id": "analysis", "machine": "q35", "state": "created",
        "configuration": {"devices": {
            "front_panel": True,
            "remote_devices": {"available": False, "reason": "helper not configured"},
            "unknown_future_device": True,
        }},
    }), encoding="utf-8")

    client = TestClient(create_app(OperatorApplication(config), token="test-token"),
                        base_url="http://127.0.0.1")
    response = client.get("/api/v1/sessions", headers={"X-MachineEmu-Token": "test-token"})
    assert response.status_code == 200
    assert response.json()["sessions"][0]["capabilities"] == {
        "front_panel": {"available": True},
        "remote_devices": {"available": False, "reason": "helper not configured"},
    }


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


def test_terminal_ticket_is_one_time_and_requires_a_declared_socket(tmp_path):
    # Unix-domain socket paths are capped near 108 bytes, unlike ordinary runtime files.
    short_root = Path(tempfile.mkdtemp(prefix="me-", dir="/tmp"))
    config = OperatorConfig(
        short_root / "engines", short_root / "assets", short_root / "state",
        short_root / "runtime", short_root / "artifacts",
    )
    runtime = config.runtime_root / "sessions/session-1"
    state = config.state_root / "instances/instance-1"
    artifact = config.artifact_root / "sessions/session-1"
    for path in (runtime / "sockets", state, artifact):
        path.mkdir(parents=True)
    uart = runtime / "sockets/uart.sock"
    listener: socket.socket | None = None
    try:
        listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        listener.bind(str(uart))
        (runtime / "manifest.json").write_text(json.dumps({
            "schema_version": 1, "session_id": "session-1", "instance_id": "instance-1", "state": "running",
            "launch_plan": {"uart_socket": str(uart)},
        }), encoding="utf-8")
        app = create_app(OperatorApplication(config), token="test-token")
        client = TestClient(app, base_url="http://127.0.0.1")
        headers = {"X-MachineEmu-Token": "test-token", "Origin": "http://127.0.0.1"}
        response = client.post("/api/v1/sessions/instance-1/session-1/terminal/ticket", headers=headers)
        assert response.status_code == 200
        ticket = response.json()["ticket"]
        assert app.state.terminal_tickets.consume(ticket, "instance-1", "session-1") is True
        assert app.state.terminal_tickets.consume(ticket, "instance-1", "session-1") is False
    finally:
        if listener is not None:
            listener.close()
        shutil.rmtree(short_root)


def test_terminal_ticket_store_binds_tickets_to_a_session():
    store = TerminalTicketStore()
    ticket = store.issue("instance-1", "session-1")
    assert store.consume(ticket, "instance-2", "session-1") is False


def test_terminal_websocket_is_ticketed_view_only_until_control_is_claimed(tmp_path):
    short_root = Path(tempfile.mkdtemp(prefix="me-", dir="/tmp"))
    config = OperatorConfig(
        short_root / "engines", short_root / "assets", short_root / "state",
        short_root / "runtime", short_root / "artifacts",
    )
    runtime = config.runtime_root / "sessions/session-1"
    state = config.state_root / "instances/instance-1"
    artifact = config.artifact_root / "sessions/session-1"
    for path in (runtime / "sockets", state, artifact):
        path.mkdir(parents=True)
    uart = runtime / "sockets/uart.sock"
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    received: list[bytes] = []
    try:
        listener.bind(str(uart))
        listener.listen(1)
        (runtime / "manifest.json").write_text(json.dumps({
            "schema_version": 1, "session_id": "session-1", "instance_id": "instance-1", "state": "running",
            "launch_plan": {"uart_socket": str(uart)},
        }), encoding="utf-8")

        def serial_peer() -> None:
            connection, _ = listener.accept()
            with connection:
                connection.sendall(b"ready\n")
                data = connection.recv(1024)
                received.append(data)
                connection.sendall(b"echo:" + data)

        peer = threading.Thread(target=serial_peer)
        peer.start()
        app = create_app(OperatorApplication(config), token="test-token")
        client = TestClient(app, base_url="http://127.0.0.1")
        headers = {"X-MachineEmu-Token": "test-token", "Origin": "http://127.0.0.1"}
        ticket = client.post("/api/v1/sessions/instance-1/session-1/terminal/ticket", headers=headers).json()["ticket"]
        with client.websocket_connect(
            f"/ws/v1/sessions/instance-1/session-1/terminal?ticket={ticket}",
            headers={"host": "127.0.0.1", "origin": "http://127.0.0.1"},
        ) as terminal:
            assert terminal.receive_json() == {"v": 1, "type": "terminal.ready", "view_only": True}
            assert terminal.receive_bytes() == b"ready\n"
            terminal.send_json({"v": 1, "type": "terminal.claim"})
            assert terminal.receive_json() == {"v": 1, "type": "terminal.claimed"}
            terminal.send_bytes(b"help\n")
            assert terminal.receive_bytes() == b"echo:help\n"
        peer.join(timeout=1)
        assert received == [b"help\n"]
    finally:
        listener.close()
        shutil.rmtree(short_root)


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


def test_api_exposes_only_qmp_status(monkeypatch, tmp_path):
    config = OperatorConfig(tmp_path / "engines", tmp_path / "assets", tmp_path / "state", tmp_path / "runtime", tmp_path / "artifacts")
    runtime = config.runtime_root / "sessions/session-1"
    state = config.state_root / "instances/instance-1"
    artifact = config.artifact_root / "sessions/session-1"
    for path in (runtime, state, artifact):
        path.mkdir(parents=True)
    (runtime / "manifest.json").write_text(json.dumps({"schema_version": 1, "session_id": "session-1", "instance_id": "instance-1"}), encoding="utf-8")
    application = OperatorApplication(config)

    async def fake_status(record):
        assert record.session_id == "session-1"
        return {"status": "running", "running": True, "singlestep": False}

    monkeypatch.setattr(application, "qmp_status", fake_status)
    client = TestClient(create_app(application, token="test-token"), base_url="http://127.0.0.1")
    response = client.get("/api/v1/sessions/instance-1/session-1/qmp/status", headers={"X-MachineEmu-Token": "test-token"})
    assert response.json() == {"status": "running", "running": True, "singlestep": False}


def test_api_exposes_allowlisted_qmp_inspection(monkeypatch, tmp_path):
    config = OperatorConfig(tmp_path / "engines", tmp_path / "assets", tmp_path / "state", tmp_path / "runtime", tmp_path / "artifacts")
    runtime = config.runtime_root / "sessions/session-1"
    for path in (runtime, config.state_root / "instances/instance-1", config.artifact_root / "sessions/session-1"):
        path.mkdir(parents=True)
    (runtime / "manifest.json").write_text(json.dumps({"schema_version": 1, "session_id": "session-1", "instance_id": "instance-1"}), encoding="utf-8")
    application = OperatorApplication(config)

    async def fake_inspect(record, command, path, property):
        assert (record.session_id, command, path, property) == ("session-1", "qom-get", "/machine", "type")
        return {"command": command, "result": "pc-q35-10.2"}

    monkeypatch.setattr(application, "qmp_inspect", fake_inspect)
    client = TestClient(create_app(application, token="test-token"), base_url="http://127.0.0.1")
    response = client.post(
        "/api/v1/sessions/instance-1/session-1/qmp/inspect",
        headers={"X-MachineEmu-Token": "test-token", "Origin": "http://127.0.0.1"},
        json={"command": "qom-get", "path": "/machine", "property": "type"},
    )
    assert response.status_code == 200
    assert response.json() == {"command": "qom-get", "result": "pc-q35-10.2"}


def test_api_delegates_allowlisted_session_actions(monkeypatch, tmp_path):
    config = OperatorConfig(tmp_path / "engines", tmp_path / "assets", tmp_path / "state", tmp_path / "runtime", tmp_path / "artifacts")
    runtime = config.runtime_root / "sessions/session-1"
    for path in (runtime, config.state_root / "instances/instance-1", config.artifact_root / "sessions/session-1"):
        path.mkdir(parents=True)
    (runtime / "manifest.json").write_text(json.dumps({"schema_version": 1, "session_id": "session-1", "instance_id": "instance-1", "state": "running"}), encoding="utf-8")
    application = OperatorApplication(config)

    async def fake_action(record, action):
        assert (record.session_id, action) == ("session-1", "pause")
        return "paused"

    monkeypatch.setattr(application, "qmp_action", fake_action)
    client = TestClient(create_app(application, token="test-token"), base_url="http://127.0.0.1")
    response = client.post(
        "/api/v1/sessions/instance-1/session-1/actions",
        headers={"X-MachineEmu-Token": "test-token", "Origin": "http://127.0.0.1"},
        json={"action": "pause"},
    )
    assert response.status_code == 200
    assert response.json() == {"session_id": "session-1", "action": "pause", "state": "paused"}


def test_api_serves_session_owned_screenshot(monkeypatch, tmp_path):
    config = OperatorConfig(tmp_path / "engines", tmp_path / "assets", tmp_path / "state", tmp_path / "runtime", tmp_path / "artifacts")
    runtime = config.runtime_root / "sessions/session-1"
    artifact = config.artifact_root / "sessions/session-1"
    for path in (runtime, config.state_root / "instances/instance-1", artifact):
        path.mkdir(parents=True)
    (runtime / "manifest.json").write_text(json.dumps({"schema_version": 1, "session_id": "session-1", "instance_id": "instance-1"}), encoding="utf-8")
    application = OperatorApplication(config)
    image = artifact / "screenshot.png"
    image.write_bytes(b"png-data")

    async def fake_screenshot(record):
        assert record.artifact_dir == artifact
        return image, "image/png"

    monkeypatch.setattr(application, "screenshot", fake_screenshot)
    client = TestClient(create_app(application, token="test-token"), base_url="http://127.0.0.1")
    response = client.get(
        "/api/v1/sessions/instance-1/session-1/screenshot",
        headers={"X-MachineEmu-Token": "test-token"},
    )
    assert response.status_code == 200
    assert response.headers["content-type"] == "image/png"
    assert response.content == b"png-data"


def test_api_reports_audio_transport_without_exposing_host_paths(tmp_path):
    config = OperatorConfig(tmp_path / "engines", tmp_path / "assets", tmp_path / "state", tmp_path / "runtime", tmp_path / "artifacts")
    runtime = config.runtime_root / "sessions/session-1"
    for path in (runtime, config.state_root / "instances/instance-1", config.artifact_root / "sessions/session-1"):
        path.mkdir(parents=True)
    (runtime / "manifest.json").write_text(json.dumps({
        "schema_version": 1, "session_id": "session-1", "instance_id": "instance-1",
        "launch_plan": {"audio_socket": str(runtime / "sockets" / "audio.sock")},
    }), encoding="utf-8")
    client = TestClient(create_app(OperatorApplication(config), token="test-token"), base_url="http://127.0.0.1")
    response = client.get(
        "/api/v1/sessions/instance-1/session-1/audio",
        headers={"X-MachineEmu-Token": "test-token"},
    )
    assert response.status_code == 200
    assert response.json() == {
        "schema_version": 1, "available": False,
        "reason": "Audio is not configured for this session",
        "capture_held": False, "capture_ttl": 30,
    }


def test_api_reports_remote_device_capabilities_without_host_paths(tmp_path):
    config = OperatorConfig(tmp_path / "engines", tmp_path / "assets", tmp_path / "state", tmp_path / "runtime", tmp_path / "artifacts")
    runtime = config.runtime_root / "sessions/session-1"
    for path in (runtime, config.state_root / "instances/instance-1", config.artifact_root / "sessions/session-1"):
        path.mkdir(parents=True)
    (runtime / "manifest.json").write_text(json.dumps({
        "schema_version": 1, "session_id": "session-1", "instance_id": "instance-1",
        "configuration": {"adapter": "pc", "remote_devices": {
            "enabled": True, "profiles": {"sensor": {"vendor_id": 1234, "product_id": 5678, "serial": "lab"}},
        }},
    }), encoding="utf-8")
    client = TestClient(create_app(OperatorApplication(config), token="test-token"), base_url="http://127.0.0.1")
    response = client.get(
        "/api/v1/sessions/instance-1/session-1/remote-devices/capabilities",
        headers={"X-MachineEmu-Token": "test-token"},
    )
    assert response.status_code == 200
    value = response.json()
    assert value["modes"]["generic_usb"]["available"] is True
    assert value["profiles"] == {"sensor": {"vendor_id": 1234, "product_id": 5678, "serial": "lab"}}
    assert "/" not in json.dumps(value)


def test_api_remote_device_attachment_lifecycle_is_session_scoped(tmp_path):
    config = OperatorConfig(tmp_path / "engines", tmp_path / "assets", tmp_path / "state", tmp_path / "runtime", tmp_path / "artifacts")
    runtime = config.runtime_root / "sessions/session-1"
    for path in (runtime, config.state_root / "instances/instance-1", config.artifact_root / "sessions/session-1"):
        path.mkdir(parents=True)
    (runtime / "manifest.json").write_text(json.dumps({
        "schema_version": 1, "session_id": "session-1", "instance_id": "instance-1",
        "configuration": {"adapter": "pc", "remote_devices": {
            "enabled": True, "profiles": {"sensor": {"vendor_id": 1234, "product_id": 5678}},
        }},
    }), encoding="utf-8")
    client = TestClient(create_app(OperatorApplication(config), token="test-token"), base_url="http://127.0.0.1")
    headers = {"X-MachineEmu-Token": "test-token", "Origin": "http://127.0.0.1"}
    created = client.post(
        "/api/v1/sessions/instance-1/session-1/remote-devices/attachments",
        headers=headers, json={"mode": "generic_usb", "profile": "sensor"},
    )
    assert created.status_code == 201
    attachment = created.json()
    assert attachment["state"] == "reserved"
    listed = client.get("/api/v1/sessions/instance-1/session-1/remote-devices/attachments", headers=headers)
    assert listed.json() == [attachment]
    ticket = client.post(
        f"/api/v1/sessions/instance-1/session-1/remote-devices/attachments/{attachment['attachment_id']}/connect-ticket",
        headers=headers, json={"role": "local"},
    )
    assert ticket.status_code == 200
    assert ticket.json()["ticket"]
    revoked = client.delete(
        f"/api/v1/sessions/instance-1/session-1/remote-devices/attachments/{attachment['attachment_id']}",
        headers=headers,)
    assert revoked.status_code == 200
    assert revoked.json()["state"] == "revoked"


def test_remote_device_websocket_proxies_owned_socket_and_requires_cleanup(tmp_path):
    short_root = Path(tempfile.mkdtemp(prefix="me-", dir="/tmp"))
    config = OperatorConfig(short_root / "engines", short_root / "assets", short_root / "state", short_root / "runtime", short_root / "artifacts")
    runtime = config.runtime_root / "sessions/session-1"
    sockets = runtime / "sockets"
    for path in (sockets, config.state_root / "instances/instance-1", config.artifact_root / "sessions/session-1"):
        path.mkdir(parents=True)
    endpoint = sockets / "remote-usb.sock"
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    received: list[bytes] = []
    listener.bind(str(endpoint))
    listener.listen(1)
    (runtime / "manifest.json").write_text(json.dumps({
        "schema_version": 1, "session_id": "session-1", "instance_id": "instance-1",
        "configuration": {"adapter": "pc", "remote_devices": {
            "enabled": True, "profiles": {"sensor": {"vendor_id": 1234, "product_id": 5678}},
        }},
    }), encoding="utf-8")

    def peer() -> None:
        connection, _ = listener.accept()
        with connection:
            received.append(connection.recv(1024))
            connection.sendall(b"qemu-data")

    worker = threading.Thread(target=peer)
    worker.start()
    try:
        application = OperatorApplication(config)
        client = TestClient(create_app(application, token="test-token"), base_url="http://127.0.0.1")
        headers = {"X-MachineEmu-Token": "test-token", "Origin": "http://127.0.0.1"}
        created = client.post(
            "/api/v1/sessions/instance-1/session-1/remote-devices/attachments",
            headers=headers, json={"mode": "generic_usb", "profile": "sensor"},
        ).json()
        ticket = client.post(
            f"/api/v1/sessions/instance-1/session-1/remote-devices/attachments/{created['attachment_id']}/connect-ticket",
            headers=headers, json={"role": "local"},
        ).json()["ticket"]
        with client.websocket_connect(
            f"/ws/v1/sessions/instance-1/session-1/remote-devices/{created['attachment_id']}",
            headers={"host": "127.0.0.1", "origin": "http://127.0.0.1"},
        ) as stream:
            stream.send_json({"ticket": ticket})
            stream.send_json({"vendor_id": 1234, "product_id": 5678})
            assert stream.receive_json()["type"] == "remote_device.ready"
            stream.send_bytes(b"browser-data")
            assert stream.receive_bytes() == b"qemu-data"
            stream.send_json({"type": "cleanup", "confirmed": True})
        worker.join(timeout=1)
        assert received == [b"browser-data"]
    finally:
        listener.close()
        shutil.rmtree(short_root)


def test_api_audio_control_requires_a_session_owned_audio_socket(tmp_path):
    short_root = Path(tempfile.mkdtemp(prefix="me-", dir="/tmp"))
    config = OperatorConfig(short_root / "engines", short_root / "assets", short_root / "state", short_root / "runtime", short_root / "artifacts")
    runtime = config.runtime_root / "sessions/session-1"
    sockets = runtime / "sockets"
    for path in (sockets, config.state_root / "instances/instance-1", config.artifact_root / "sessions/session-1"):
        path.mkdir(parents=True)
    audio = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    audio.bind(str(sockets / "audio.sock"))
    (runtime / "manifest.json").write_text(json.dumps({
        "schema_version": 1, "session_id": "session-1", "instance_id": "instance-1",
        "launch_plan": {"audio_socket": str(sockets / "audio.sock")},
    }), encoding="utf-8")
    try:
        client = TestClient(create_app(OperatorApplication(config), token="test-token"), base_url="http://127.0.0.1")
        headers = {"X-MachineEmu-Token": "test-token", "Origin": "http://127.0.0.1"}
        attached = client.post("/api/v1/sessions/instance-1/session-1/audio/control", headers=headers, json={"action": "attach"})
        assert attached.status_code == 200
        token = attached.json()["client_token"]
        claimed = client.post("/api/v1/sessions/instance-1/session-1/audio/control", headers=headers,
                              json={"action": "claim", "client_token": token})
        assert claimed.json()["capture"] is True
        released = client.post("/api/v1/sessions/instance-1/session-1/audio/control", headers=headers,
                               json={"action": "release", "client_token": token})
        assert released.json() == {"ok": True, "capture": False}
        detached = client.post("/api/v1/sessions/instance-1/session-1/audio/control", headers=headers,
                               json={"action": "detach", "client_token": token})
        assert detached.json() == {"ok": True, "attached": False}
    finally:
        audio.close()
        shutil.rmtree(short_root)


def test_api_exposes_verified_instance_snapshot_lifecycle(tmp_path):
    config = OperatorConfig(tmp_path / "engines", tmp_path / "assets", tmp_path / "state", tmp_path / "runtime", tmp_path / "artifacts")
    state = config.state_root / "instances/instance-1"
    state.mkdir(parents=True)
    disk = state / "disk.img"
    disk.write_bytes(b"before")
    import hashlib
    (state / "instance.json").write_text(json.dumps({
        "schema_version": 1, "instance_id": "instance-1", "state": "created",
        "state_files": {"disk.img": {"sha256": "sha256:" + hashlib.sha256(b"before").hexdigest(), "size": 6}},
    }), encoding="utf-8")
    client = TestClient(create_app(OperatorApplication(config), token="test-token"), base_url="http://127.0.0.1")
    headers = {"X-MachineEmu-Token": "test-token", "Origin": "http://127.0.0.1"}
    created = client.post("/api/v1/instances/instance-1/snapshots", headers=headers,
                          json={"snapshot_id": "cold-boot"})
    assert created.status_code == 201
    assert created.json() == {"snapshot_id": "cold-boot", "state": "created", "files": ["disk.img"]}
    disk.write_bytes(b"after")
    restored = client.post("/api/v1/instances/instance-1/snapshots/cold-boot/restore", headers=headers)
    assert restored.status_code == 200
    assert restored.json() == {"snapshot_id": "cold-boot", "state": "restored"}
    assert disk.read_bytes() == b"before"
    assert client.get("/api/v1/instances/instance-1/snapshots", headers=headers).json() == {
        "schema_version": 1, "snapshots": [{"snapshot_id": "cold-boot", "files": ["disk.img"]}],
    }


def test_frontpanel_websocket_validates_and_forwards_bounded_frames(tmp_path):
    short_root = Path(tempfile.mkdtemp(prefix="me-", dir="/tmp"))
    config = OperatorConfig(short_root / "engines", short_root / "assets", short_root / "state", short_root / "runtime", short_root / "artifacts")
    runtime = config.runtime_root / "sessions/session-1"
    sockets = runtime / "sockets"
    for path in (sockets, config.state_root / "instances/instance-1", config.artifact_root / "sessions/session-1"):
        path.mkdir(parents=True)
    endpoint = sockets / "frontpanel.sock"
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    listener.bind(str(endpoint))
    listener.listen(1)
    (runtime / "manifest.json").write_text(json.dumps({
        "schema_version": 1, "session_id": "session-1", "instance_id": "instance-1",
        "configuration": {"devices": {"front_panel": True}},
    }), encoding="utf-8")

    def peer() -> None:
        connection, _ = listener.accept()
        with connection:
            connection.sendall(b'{"schema":"unifi.frontpanel.v1","kind":"status","ports":[]}\n')

    worker = threading.Thread(target=peer)
    worker.start()
    try:
        client = TestClient(create_app(OperatorApplication(config), token="test-token"), base_url="http://127.0.0.1")
        with client.websocket_connect(
            "/ws/v1/sessions/instance-1/session-1/frontpanel",
            headers={"host": "127.0.0.1", "origin": "http://127.0.0.1"},
        ) as stream:
            assert stream.receive_json() == {"schema": "unifi.frontpanel.v1", "kind": "status", "ports": []}
        worker.join(timeout=1)
    finally:
        listener.close()
        shutil.rmtree(short_root)


def test_lcd_websocket_forwards_validated_read_only_frames(tmp_path):
    short_root = Path(tempfile.mkdtemp(prefix="me-", dir="/tmp"))
    config = OperatorConfig(short_root / "engines", short_root / "assets", short_root / "state", short_root / "runtime", short_root / "artifacts")
    runtime = config.runtime_root / "sessions/session-1"
    sockets = runtime / "sockets"
    for path in (sockets, config.state_root / "instances/instance-1", config.artifact_root / "sessions/session-1"):
        path.mkdir(parents=True)
    endpoint = sockets / "lcd.sock"
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    listener.bind(str(endpoint))
    listener.listen(1)
    (runtime / "manifest.json").write_text(json.dumps({
        "schema_version": 1, "session_id": "session-1", "instance_id": "instance-1",
        "configuration": {"devices": {"lcd": True}},
    }), encoding="utf-8")

    def peer() -> None:
        connection, _ = listener.accept()
        with connection:
            connection.sendall(b'{"schema":"unifi.lcm.v1","kind":"status","ui":{}}\n')

    worker = threading.Thread(target=peer)
    worker.start()
    try:
        client = TestClient(create_app(OperatorApplication(config), token="test-token"), base_url="http://127.0.0.1")
        with client.websocket_connect(
            "/ws/v1/sessions/instance-1/session-1/lcd",
            headers={"host": "127.0.0.1", "origin": "http://127.0.0.1"},
        ) as stream:
            assert stream.receive_json() == {"schema": "unifi.lcm.v1", "kind": "status", "ui": {}}
        worker.join(timeout=1)
    finally:
        listener.close()
        shutil.rmtree(short_root)


def test_lcd_touch_accepts_semantic_actions_and_returns_owned_reply(tmp_path):
    short_root = Path(tempfile.mkdtemp(prefix="me-", dir="/tmp"))
    config = OperatorConfig(short_root / "engines", short_root / "assets", short_root / "state", short_root / "runtime", short_root / "artifacts")
    runtime = config.runtime_root / "sessions/session-1"
    sockets = runtime / "sockets"
    for path in (sockets, config.state_root / "instances/instance-1", config.artifact_root / "sessions/session-1"):
        path.mkdir(parents=True)
    endpoint = sockets / "display-input.sock"
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    listener.bind(str(endpoint))
    listener.listen(1)
    (runtime / "manifest.json").write_text(json.dumps({
        "schema_version": 1, "session_id": "session-1", "instance_id": "instance-1",
    }), encoding="utf-8")

    def peer() -> None:
        connection, _ = listener.accept()
        with connection:
            assert json.loads(connection.recv(2048)) == {"screen": "menu.main"}
            connection.sendall(b'{"ok":true,"screen":"menu.main"}\n')

    worker = threading.Thread(target=peer)
    worker.start()
    try:
        client = TestClient(create_app(OperatorApplication(config), token="test-token"), base_url="http://127.0.0.1")
        response = client.post(
            "/api/v1/sessions/instance-1/session-1/lcd/touch",
            headers={"X-MachineEmu-Token": "test-token", "Origin": "http://127.0.0.1"},
            json={"screen": "menu.main"},
        )
        assert response.status_code == 200
        assert response.json() == {"ok": True, "screen": "menu.main"}
        worker.join(timeout=1)
    finally:
        listener.close()
        shutil.rmtree(short_root)


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
