"""Minimal HTTP adapter over the application operation layer."""

from __future__ import annotations

import asyncio
import hmac
import json
from pathlib import Path
import secrets
from typing import Literal

from fastapi import FastAPI, HTTPException, Request, WebSocket, WebSocketDisconnect
from fastapi.responses import FileResponse, JSONResponse
from pydantic import BaseModel, ConfigDict, Field

from .catalog import CatalogError, ProfileCatalog
from .domains.unifi.compat.bluetooth import BluetoothError, advertise as bluetooth_advertise, stats as bluetooth_stats
from .domains.unifi.compat.hwsim import HwsimError, configure as hwsim_configure, stats as hwsim_stats
from .runtime import (AudioClientRegistry, ExternalVncListener, OperatorApplication, QMPError,
                      OperationJournal, RemoteDeviceRegistry, RfbInputGate, RfbProtocolError,
                      TerminalTicketStore)
from .runtime.gdb import GdbConsole, GdbUnavailable, gdb_target
from .runtime.spice_audio import SpiceClientGate, SpiceProtocolError, SpiceServerGate


class SessionRequest(BaseModel):
    model_config = ConfigDict(extra="forbid")
    instance_id: str = Field(min_length=1, max_length=64)
    session_id: str = Field(min_length=1, max_length=64)


class CreateSessionRequest(SessionRequest):
    profile_path: str = Field(min_length=1, max_length=4096)
    target: str = Field(min_length=1, max_length=128)


class CreateCatalogSessionRequest(SessionRequest):
    profile_id: str = Field(min_length=1, max_length=64)
    target: str | None = Field(default=None, max_length=128)


class DeviceLaunchRequest(BaseModel):
    model_config = ConfigDict(extra="forbid")
    target: str | None = Field(default=None, max_length=128)


class DeviceSessionRequest(SessionRequest):
    target: str | None = Field(default=None, max_length=128)


class AnalysisCloneRequest(BaseModel):
    model_config = ConfigDict(extra="forbid")
    clone_id: str = Field(min_length=1, max_length=64, pattern=r"^[A-Za-z0-9][A-Za-z0-9_.-]*$")
    target: str | None = Field(default=None, max_length=128)


class Capability(BaseModel):
    available: bool
    reason: str | None = None


class SessionSummary(BaseModel):
    session_id: str
    instance_id: str
    profile_id: str
    machine: str
    state: str
    capabilities: dict[str, Capability] = Field(default_factory=dict)


class SessionInventory(BaseModel):
    sessions: list[SessionSummary]


class TerminalTicket(BaseModel):
    ticket: str
    expires_in_seconds: int


class QmpStatus(BaseModel):
    status: str
    running: bool | None = None
    singlestep: bool | None = None


class QmpInspectRequest(BaseModel):
    model_config = ConfigDict(extra="forbid")
    command: str = Field(pattern=r"^(query-status|query-pci|query-chardev|query-block|info-usb|qom-list|qom-get)$")
    path: str | None = None
    property: str | None = None


class QmpInspectResponse(BaseModel):
    command: str
    result: object


class AudioStatus(BaseModel):
    schema_version: int = 1
    available: bool
    reason: str | None = None
    capture_held: bool = False
    capture_ttl: int = 30


class AudioControlRequest(BaseModel):
    model_config = ConfigDict(extra="forbid")
    action: Literal["attach", "renew", "claim", "release", "detach"]
    client_token: str | None = Field(default=None, min_length=1, max_length=128)
    takeover: bool = False


class SessionActionRequest(BaseModel):
    model_config = ConfigDict(extra="forbid")
    action: Literal["pause", "resume", "reset"]


class SnapshotRequest(BaseModel):
    model_config = ConfigDict(extra="forbid")
    snapshot_id: str = Field(pattern=r"^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$")
    files: list[str] | None = None


class RemoteDeviceCapabilities(BaseModel):
    schema_version: int = 1
    modes: dict[str, dict[str, object]]
    profiles: dict[str, dict[str, object]] = Field(default_factory=dict)
    limits: dict[str, int]


class RemoteDeviceCreateRequest(BaseModel):
    model_config = ConfigDict(extra="forbid")
    mode: Literal["generic_usb", "webauthn", "ctap"]
    profile: str = Field(min_length=1, max_length=64)


class RemoteDeviceTicketRequest(BaseModel):
    model_config = ConfigDict(extra="forbid")
    role: Literal["local", "guest"]


class RemoteDeviceAttachment(BaseModel):
    attachment_id: str
    session_id: str
    mode: str
    profile: str
    selected_device: dict[str, object] | None = None
    state: str
    generation: int
    lease_deadline: float


class HwsimMediumRequest(BaseModel):
    model_config = ConfigDict(extra="forbid")
    signal: int | None = Field(default=None, ge=-110, le=0)
    jitter: int | None = Field(default=None, ge=0, le=60)
    loss: float | None = Field(default=None, ge=0, le=1)
    latency_ms: int | None = Field(default=None, ge=0, le=60000)
    rate_index: int | None = Field(default=None, ge=0, lt=32)
    aggregate: bool | None = None
    seed: int | None = None


class BluetoothPeerRequest(BaseModel):
    model_config = ConfigDict(extra="forbid")
    address: str
    data: str = ""
    rssi: int = Field(default=-60, ge=-127, le=20)
    event_type: int | None = Field(default=None, ge=0, le=4)
    address_type: int | None = Field(default=None, ge=0, le=4)


class UsbHostRequest(BaseModel):
    model_config = ConfigDict(extra="forbid")
    hostbus: int = Field(ge=1, le=255)
    hostaddr: int = Field(ge=1, le=127)


class UsbImageRequest(BaseModel):
    model_config = ConfigDict(extra="forbid")
    image: str = Field(min_length=1, max_length=128)
    read_only: bool = True


class DeviceIdRequest(BaseModel):
    model_config = ConfigDict(extra="forbid")
    id: str = Field(min_length=1, max_length=64, pattern=r"^[A-Za-z0-9_.-]+$")


class CdromInsertRequest(DeviceIdRequest):
    image: str = Field(min_length=1, max_length=128)


class NetworkLinkRequest(BaseModel):
    model_config = ConfigDict(extra="forbid")
    name: str = Field(min_length=1, max_length=64, pattern=r"^[A-Za-z0-9_.-]+$")
    up: bool


class NetworkAttachRequest(BaseModel):
    model_config = ConfigDict(extra="forbid")
    id: str | None = Field(default=None, min_length=1, max_length=64, pattern=r"^[A-Za-z0-9_.-]+$")
    type: Literal["user", "bridge", "tap"] = "user"
    model: dict[str, object] = Field(default_factory=dict)
    source: dict[str, object] = Field(default_factory=dict)
    target: dict[str, object] = Field(default_factory=dict)
    mac: str | None = None


def _loopback_host(host: str) -> bool:
    hostname = host.rsplit(":", 1)[0].strip("[]").lower()
    return hostname in {"localhost", "127.0.0.1", "::1"}


def create_app(application: OperatorApplication, *, token: str | None = None,
               catalog_root: Path | None = None) -> FastAPI:
    """Create an API that delegates all stateful work to ``application``."""
    app = FastAPI(title="MachineEmu", version="0.1.0")
    app.state.token = token or secrets.token_urlsafe(32)
    app.state.catalog = ProfileCatalog(catalog_root) if catalog_root is not None else None
    app.state.terminal_tickets = TerminalTicketStore()
    app.state.remote_devices = RemoteDeviceRegistry()
    app.state.audio_clients = AudioClientRegistry()
    app.state.vnc_owners = {}
    app.state.vnc_connections = {}
    app.state.external_vnc = {}
    app.state.external_vnc_locks = {}
    app.state.operations = OperationJournal(path=application.config.runtime_root / "operations.json")
    app.state.gdb_consoles = {}
    app.state.gdb_locks = {}
    app.state.lcd_locks = {}
    if app.state.catalog is not None and application.catalog is None:
        application.catalog = app.state.catalog

    @app.middleware("http")
    async def security(request: Request, call_next):
        if not _loopback_host(request.headers.get("host", "")):
            return JSONResponse({"detail": "loopback Host required"}, status_code=403)
        if request.url.path.startswith("/api/"):
            supplied = request.headers.get("x-machineemu-token", "")
            if not hmac.compare_digest(supplied, app.state.token):
                return JSONResponse({"detail": "application token required"}, status_code=401)
            if request.method not in {"GET", "HEAD", "OPTIONS"}:
                origin = request.headers.get("origin")
                expected = f"{request.url.scheme}://{request.headers.get('host', '')}"
                if origin != expected:
                    return JSONResponse({"detail": "same-origin request required"}, status_code=403)
        response = await call_next(request)
        response.headers["X-Content-Type-Options"] = "nosniff"
        response.headers["X-Frame-Options"] = "DENY"
        response.headers["Referrer-Policy"] = "no-referrer"
        response.headers["Cache-Control"] = "no-store"
        return response

    @app.get("/api/v1/health")
    async def health() -> dict[str, str]:
        return {"status": "ok"}

    @app.websocket("/ws/v1/status")
    async def status_stream(websocket: WebSocket) -> None:
        host = websocket.headers.get("host", "")
        scheme = "https" if websocket.url.scheme == "wss" else "http"
        if not _loopback_host(host) or websocket.headers.get("origin") != f"{scheme}://{host}":
            await websocket.close(code=4403)
            return
        await websocket.accept()
        try:
            while True:
                await websocket.send_json({"v": 1, "type": "status",
                                           "sessions": application.list_session_summaries()})
                await asyncio.sleep(2)
        except (WebSocketDisconnect, RuntimeError):
            return

    def display_socket(record, kind: str) -> Path:
        """Return only a manifest-declared display socket under this session."""
        try:
            manifest = json.loads(record.manifest.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as exc:
            raise ValueError(f"cannot read display configuration: {exc}") from exc
        configuration = manifest.get("configuration", {})
        devices = configuration.get("devices", {}) if isinstance(configuration, dict) else {}
        capabilities = manifest.get("capabilities", {})
        capability = capabilities.get(kind, {}) if isinstance(capabilities, dict) else {}
        declared = isinstance(devices, dict) and devices.get(kind) is True
        available = isinstance(capability, dict) and capability.get("available") is True
        if not (declared or available):
            raise ValueError(f"session has no available {kind} display")
        expected = record.runtime_dir / "sockets" / f"{kind}.sock"
        plan = manifest.get("launch_plan", {})
        endpoint = plan.get(f"{kind}_socket") if isinstance(plan, dict) else None
        if endpoint != str(expected) or not expected.is_socket():
            raise ValueError(f"session {kind} endpoint is unavailable")
        return expected

    def helper_socket(record, kind: str) -> Path:
        """Return a manifest-declared helper control socket under this session."""
        try:
            manifest = json.loads(record.manifest.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as exc:
            raise ValueError(f"cannot read helper configuration: {exc}") from exc
        configuration = manifest.get("configuration", {})
        devices = configuration.get("devices", {}) if isinstance(configuration, dict) else {}
        capabilities = manifest.get("capabilities", {})
        capability = capabilities.get(kind, {}) if isinstance(capabilities, dict) else {}
        declared = isinstance(devices, dict) and devices.get(kind) is True
        available = isinstance(capability, dict) and capability.get("available") is True
        if not (declared or available):
            raise ValueError(f"session has no available {kind} helper")
        key = {"wifi_hwsim": "hwsim_control_socket", "bluetooth_control": "bluetooth_control_socket"}[kind]
        expected = record.runtime_dir / "sockets" / f"{kind}.sock"
        plan = manifest.get("launch_plan", {})
        endpoint = plan.get(key) if isinstance(plan, dict) else None
        if endpoint != str(expected) or not expected.is_socket():
            raise ValueError(f"session {kind} helper endpoint is unavailable")
        return expected

    def gdb_endpoint(record) -> tuple[dict[str, object], str]:
        try:
            manifest = json.loads(record.manifest.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as exc:
            raise ValueError(f"cannot read GDB configuration: {exc}") from exc
        plan = manifest.get("launch_plan", {})
        endpoint = plan.get("gdb") if isinstance(plan, dict) else None
        if not isinstance(endpoint, dict):
            raise ValueError("session was not launched with the QEMU GDB stub enabled")
        if endpoint.get("transport") == "unix":
            path = endpoint.get("path")
            expected = record.runtime_dir / "sockets" / "gdb.sock"
            if path != str(expected) or not expected.is_socket():
                raise ValueError("session GDB endpoint is unavailable")
        target = gdb_target(endpoint)
        return endpoint, target

    @app.get("/api/v1/sessions", response_model=SessionInventory, response_model_exclude_none=True)
    async def sessions() -> dict[str, list[dict[str, object]]]:
        return {"sessions": application.list_session_summaries()}

    @app.post("/api/v1/sessions/{instance_id}/{session_id}/terminal/ticket", response_model=TerminalTicket)
    async def terminal_ticket(instance_id: str, session_id: str) -> TerminalTicket:
        try:
            record = application.open_session(instance_id, session_id)
            application.terminal_socket(record)
        except (ValueError, OSError) as exc:
            raise HTTPException(status_code=409, detail=str(exc)) from exc
        ticket = app.state.terminal_tickets.issue(instance_id, session_id)
        return TerminalTicket(ticket=ticket, expires_in_seconds=30)

    @app.get("/api/v1/sessions/{instance_id}/{session_id}/qmp/status", response_model=QmpStatus)
    async def qmp_status(instance_id: str, session_id: str) -> dict[str, object]:
        try:
            return await application.qmp_status(application.open_session(instance_id, session_id))
        except (ValueError, OSError, QMPError) as exc:
            raise HTTPException(status_code=409, detail=str(exc)) from exc

    @app.post("/api/v1/sessions/{instance_id}/{session_id}/actions")
    async def session_action(instance_id: str, session_id: str,
                             request: SessionActionRequest) -> dict[str, str]:
        try:
            record = application.open_session(instance_id, session_id)
            state = await application.qmp_action(record, request.action)
            return {"session_id": session_id, "action": request.action, "state": state}
        except (ValueError, OSError, QMPError) as exc:
            raise HTTPException(status_code=409, detail=str(exc)) from exc

    @app.post("/api/v1/sessions/{instance_id}/{session_id}/qmp/inspect", response_model=QmpInspectResponse)
    async def qmp_inspect(instance_id: str, session_id: str, request: QmpInspectRequest) -> dict[str, object]:
        try:
            record = application.open_session(instance_id, session_id)
            return await application.qmp_inspect(record, request.command, request.path, request.property)
        except (ValueError, OSError, QMPError) as exc:
            raise HTTPException(status_code=409, detail=str(exc)) from exc

    def session_asset(record, name: str) -> Path:
        try:
            manifest = json.loads(record.manifest.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as exc:
            raise ValueError(f"cannot read session assets: {exc}") from exc
        assets = manifest.get("assets", {})
        path = assets.get(name) if isinstance(assets, dict) else None
        if not isinstance(path, str) or not path or not Path(path).is_file() or Path(path).is_symlink():
            raise ValueError("image is not a declared session asset")
        return Path(path)

    async def hotplug(instance_id: str, session_id: str, command: str,
                      arguments: dict[str, object]) -> dict[str, object]:
        operation = app.state.operations.create(command, session_id)
        app.state.operations.update(operation, "executing")
        try:
            record = application.open_session(instance_id, session_id)
            result = await application.qmp_execute(record, command, arguments)
            payload = {"operation_id": operation.operation_id, "result": result}
            app.state.operations.update(operation, "succeeded", result=payload)
            return payload
        except (ValueError, OSError, QMPError) as exc:
            app.state.operations.update(operation, "failed",
                                        error={"code": "hotplug_failed", "message": str(exc)})
            raise HTTPException(status_code=409, detail=str(exc)) from exc

    @app.get("/api/v1/host/usb")
    async def host_usb() -> dict[str, list[dict[str, object]]]:
        devices: list[dict[str, object]] = []
        root = Path("/dev/bus/usb")
        if root.is_dir():
            for bus in sorted(root.iterdir()):
                if not bus.name.isdigit() or not bus.is_dir():
                    continue
                for device in sorted(bus.iterdir()):
                    if not device.name.isdigit() or not device.is_char_device():
                        continue
                    devices.append({"id": f"usb-{int(bus.name)}-{int(device.name)}", "kind": "host",
                                    "hostbus": int(bus.name), "hostaddr": int(device.name)})
        return {"devices": devices}

    @app.get("/api/v1/sessions/{instance_id}/{session_id}/usb")
    async def usb_status(instance_id: str, session_id: str) -> dict[str, object]:
        result = await hotplug(instance_id, session_id, "query-usb", {})
        return {"devices": result.get("result", []) if isinstance(result.get("result"), list) else []}

    @app.get("/api/v1/sessions/{instance_id}/{session_id}/cdrom")
    async def cdrom_status(instance_id: str, session_id: str) -> dict[str, object]:
        result = await hotplug(instance_id, session_id, "query-block", {})
        return {"devices": result.get("result", []) if isinstance(result.get("result"), list) else []}

    @app.get("/api/v1/sessions/{instance_id}/{session_id}/network")
    async def network_status(instance_id: str, session_id: str) -> dict[str, object]:
        result = await hotplug(instance_id, session_id, "query-netdev", {})
        return {"devices": result.get("result", []) if isinstance(result.get("result"), list) else []}

    @app.post("/api/v1/sessions/{instance_id}/{session_id}/usb/host")
    async def usb_attach_host(instance_id: str, session_id: str, request: UsbHostRequest) -> dict[str, object]:
        device_id = f"usb-host-{request.hostbus}-{request.hostaddr}"
        return await hotplug(instance_id, session_id, "device_add", {
            "driver": "usb-host", "hostbus": request.hostbus,
            "hostaddr": request.hostaddr, "id": device_id,
        })

    @app.post("/api/v1/sessions/{instance_id}/{session_id}/usb/image")
    async def usb_attach_image(instance_id: str, session_id: str, request: UsbImageRequest) -> dict[str, object]:
        try:
            record = application.open_session(instance_id, session_id)
            image = session_asset(record, request.image)
        except (ValueError, OSError) as exc:
            raise HTTPException(status_code=404, detail=str(exc)) from exc
        node = f"machineemu-usb-{secrets.token_hex(6)}"
        await hotplug(instance_id, session_id, "blockdev-add", {
            "node-name": node, "driver": "raw", "read-only": request.read_only,
            "file": {"driver": "file", "filename": str(image)},
        })
        return await hotplug(instance_id, session_id, "device_add", {
            "driver": "usb-storage", "drive": node, "id": node,
        })

    @app.post("/api/v1/sessions/{instance_id}/{session_id}/usb/detach")
    async def usb_detach(instance_id: str, session_id: str, request: DeviceIdRequest) -> dict[str, object]:
        return await hotplug(instance_id, session_id, "device_del", {"id": request.id})

    @app.post("/api/v1/sessions/{instance_id}/{session_id}/cdrom/eject")
    async def cdrom_eject(instance_id: str, session_id: str, request: DeviceIdRequest) -> dict[str, object]:
        return await hotplug(instance_id, session_id, "eject", {"device": request.id, "force": True})

    @app.post("/api/v1/sessions/{instance_id}/{session_id}/cdrom/insert")
    async def cdrom_insert(instance_id: str, session_id: str, request: CdromInsertRequest) -> dict[str, object]:
        try:
            record = application.open_session(instance_id, session_id)
            image = session_asset(record, request.image)
        except (ValueError, OSError) as exc:
            raise HTTPException(status_code=404, detail=str(exc)) from exc
        return await hotplug(instance_id, session_id, "blockdev-change-medium",
                             {"device": request.id, "filename": str(image), "format": "raw"})

    @app.post("/api/v1/sessions/{instance_id}/{session_id}/cdrom/image")
    async def cdrom_image(instance_id: str, session_id: str, request: UsbImageRequest) -> dict[str, object]:
        try:
            record = application.open_session(instance_id, session_id)
            image = session_asset(record, request.image)
        except (ValueError, OSError) as exc:
            raise HTTPException(status_code=404, detail=str(exc)) from exc
        node = f"machineemu-cdrom-{secrets.token_hex(6)}"
        await hotplug(instance_id, session_id, "blockdev-add", {
            "node-name": node, "driver": "raw", "read-only": True,
            "file": {"driver": "file", "filename": str(image)},
        })
        return await hotplug(instance_id, session_id, "device_add", {
            "driver": "ide-cd", "drive": node, "id": node,
        })

    @app.post("/api/v1/sessions/{instance_id}/{session_id}/cdrom/detach")
    async def cdrom_detach(instance_id: str, session_id: str, request: DeviceIdRequest) -> dict[str, object]:
        return await hotplug(instance_id, session_id, "device_del", {"id": request.id})

    @app.post("/api/v1/sessions/{instance_id}/{session_id}/network/link")
    async def network_link(instance_id: str, session_id: str, request: NetworkLinkRequest) -> dict[str, object]:
        return await hotplug(instance_id, session_id, "set_link", {"name": request.name, "up": request.up})

    @app.post("/api/v1/sessions/{instance_id}/{session_id}/network/attach")
    async def network_attach(instance_id: str, session_id: str, request: NetworkAttachRequest) -> dict[str, object]:
        device_id = request.id or f"machineemu-net-{secrets.token_hex(6)}"
        netdev_id = f"{device_id}-netdev"
        options: dict[str, object] = {"type": request.type, "id": netdev_id}
        options.update(request.source)
        await hotplug(instance_id, session_id, "netdev_add", options)
        device: dict[str, object] = {"driver": request.model.get("driver", "virtio-net-pci"),
                                     "netdev": netdev_id, "id": device_id}
        device.update(request.target)
        if request.mac is not None:
            device["mac"] = request.mac
        return await hotplug(instance_id, session_id, "device_add", device)

    @app.post("/api/v1/sessions/{instance_id}/{session_id}/network/detach")
    async def network_detach(instance_id: str, session_id: str, request: DeviceIdRequest) -> dict[str, object]:
        return await hotplug(instance_id, session_id, "device_del", {"id": request.id})

    @app.get("/api/v1/sessions/{instance_id}/{session_id}/screenshot")
    async def screenshot(instance_id: str, session_id: str):
        try:
            path, content_type = await application.screenshot(
                application.open_session(instance_id, session_id)
            )
        except (ValueError, OSError, QMPError) as exc:
            raise HTTPException(status_code=409, detail=str(exc)) from exc
        if not path.is_file():
            raise HTTPException(status_code=503, detail="QEMU did not produce a screenshot")
        return FileResponse(path, media_type=content_type, filename=path.name)

    @app.get("/api/v1/sessions/{instance_id}/{session_id}/audio", response_model=AudioStatus,
             response_model_exclude_none=True)
    async def audio_status(instance_id: str, session_id: str) -> dict[str, object]:
        try:
            record = application.open_session(instance_id, session_id)
            status = application.audio_status(record)
            status["capture_held"] = app.state.audio_clients.capture_owner(instance_id, session_id) is not None
            return status
        except (ValueError, OSError) as exc:
            raise HTTPException(status_code=404, detail=str(exc)) from exc

    @app.get("/api/v1/sessions/{instance_id}/{session_id}/hwsim")
    async def hwsim_status(instance_id: str, session_id: str) -> dict[str, object]:
        try:
            record = application.open_session(instance_id, session_id)
            endpoint = helper_socket(record, "wifi_hwsim")
            return await hwsim_stats(endpoint, instance_id)
        except (ValueError, OSError) as exc:
            raise HTTPException(status_code=404, detail=str(exc)) from exc
        except HwsimError as exc:
            raise HTTPException(status_code=409, detail=str(exc)) from exc

    @app.post("/api/v1/sessions/{instance_id}/{session_id}/hwsim/medium")
    async def hwsim_medium(instance_id: str, session_id: str, request: HwsimMediumRequest) -> dict[str, object]:
        settings = request.model_dump(exclude_unset=True)
        if not settings:
            raise HTTPException(status_code=422, detail="at least one medium setting is required")
        try:
            record = application.open_session(instance_id, session_id)
            endpoint = helper_socket(record, "wifi_hwsim")
            await hwsim_configure(endpoint, settings, instance_id)
            return await hwsim_stats(endpoint, instance_id)
        except (ValueError, OSError) as exc:
            raise HTTPException(status_code=404, detail=str(exc)) from exc
        except HwsimError as exc:
            raise HTTPException(status_code=409, detail=str(exc)) from exc

    @app.get("/api/v1/sessions/{instance_id}/{session_id}/bluetooth")
    async def bluetooth_status(instance_id: str, session_id: str) -> dict[str, object]:
        try:
            record = application.open_session(instance_id, session_id)
            endpoint = helper_socket(record, "bluetooth_control")
            return await bluetooth_stats(endpoint, instance_id)
        except (ValueError, OSError) as exc:
            raise HTTPException(status_code=404, detail=str(exc)) from exc
        except BluetoothError as exc:
            raise HTTPException(status_code=409, detail=str(exc)) from exc

    @app.post("/api/v1/sessions/{instance_id}/{session_id}/bluetooth/advertise")
    async def bluetooth_advertise_peer(instance_id: str, session_id: str,
                                       request: BluetoothPeerRequest) -> dict[str, object]:
        try:
            record = application.open_session(instance_id, session_id)
            endpoint = helper_socket(record, "bluetooth_control")
            await bluetooth_advertise(endpoint, request.model_dump(exclude_none=True), instance_id)
            return await bluetooth_stats(endpoint, instance_id)
        except (ValueError, OSError) as exc:
            raise HTTPException(status_code=404, detail=str(exc)) from exc
        except BluetoothError as exc:
            raise HTTPException(status_code=409, detail=str(exc)) from exc

    async def helper_stream(websocket: WebSocket, instance_id: str, session_id: str,
                            kind: str) -> None:
        host = websocket.headers.get("host", "")
        scheme = "https" if websocket.url.scheme == "wss" else "http"
        if not _loopback_host(host) or websocket.headers.get("origin") != f"{scheme}://{host}":
            await websocket.close(code=4403)
            return
        try:
            record = application.open_session(instance_id, session_id)
            endpoint = helper_socket(record, kind)
            await websocket.accept()
            while True:
                if kind == "wifi_hwsim":
                    value = await hwsim_stats(endpoint, instance_id)
                else:
                    value = await bluetooth_stats(endpoint, instance_id)
                await websocket.send_json(value)
                await asyncio.sleep(2)
        except (ValueError, OSError, HwsimError, BluetoothError, WebSocketDisconnect, RuntimeError):
            try:
                await websocket.close(code=4404)
            except RuntimeError:
                pass

    async def shared_gdb(instance_id: str, session_id: str, record, target: str) -> GdbConsole:
        key = (instance_id, session_id)
        lock = app.state.gdb_locks.setdefault(key, asyncio.Lock())
        async with lock:
            console = app.state.gdb_consoles.get(key)
            if console is not None and console.running:
                return console
            configuration = json.loads(record.manifest.read_text(encoding="utf-8")).get("configuration", {})
            debug = configuration.get("debug", {}) if isinstance(configuration, dict) else {}
            executable = debug.get("gdb", "gdb-multiarch") if isinstance(debug, dict) else "gdb-multiarch"
            if not isinstance(executable, str) or not executable:
                executable = "gdb-multiarch"
            console = GdbConsole(session_id, target, executable=executable,
                                 on_idle=lambda _: app.state.gdb_consoles.pop(key, None))
            await console.start()
            app.state.gdb_consoles[key] = console
            return console

    @app.websocket("/ws/v1/sessions/{instance_id}/{session_id}/gdb")
    async def gdb_stream(websocket: WebSocket, instance_id: str, session_id: str) -> None:
        host = websocket.headers.get("host", "")
        scheme = "https" if websocket.url.scheme == "wss" else "http"
        if not _loopback_host(host) or websocket.headers.get("origin") != f"{scheme}://{host}":
            await websocket.close(code=4403)
            return
        key = (instance_id, session_id)
        viewer_id = websocket.query_params.get("client_id", "").lower()
        if len(viewer_id) != 32 or any(char not in "0123456789abcdef" for char in viewer_id):
            viewer_id = secrets.token_hex(16)
        try:
            record = application.open_session(instance_id, session_id)
            _, target = gdb_endpoint(record)
            console = await shared_gdb(instance_id, session_id, record, target)
            history, queue = console.subscribe()
            await websocket.accept()
            await websocket.send_json({"v": 1, "type": "gdb.ready", "session_id": session_id,
                                       "target": console.target, "executable": console.executable,
                                       "viewer_id": viewer_id, "count": console.viewers})
            for frame in history:
                await websocket.send_json(frame)

            async def to_viewer() -> None:
                while True:
                    await websocket.send_json(await queue.get())

            async def from_viewer() -> None:
                while True:
                    control = await websocket.receive_json()
                    if not isinstance(control, dict) or control.get("v") != 1:
                        await websocket.send_json({"v": 1, "type": "gdb.error", "error": "unsupported GDB control message"})
                        continue
                    if control.get("type") == "gdb.command" and isinstance(control.get("text"), str):
                        text = control["text"]
                    elif control.get("type") == "gdb.interrupt":
                        text = "-exec-interrupt"
                    else:
                        await websocket.send_json({"v": 1, "type": "gdb.error", "error": "unsupported GDB control message"})
                        continue
                    try:
                        await console.submit(text, origin=viewer_id)
                    except (GdbUnavailable, ValueError, OSError) as exc:
                        await websocket.send_json({"v": 1, "type": "gdb.error", "error": str(exc)})

            tasks = {asyncio.create_task(to_viewer()), asyncio.create_task(from_viewer())}
            _done, pending = await asyncio.wait(tasks, return_when=asyncio.FIRST_COMPLETED)
            for task in pending:
                task.cancel()
            await asyncio.gather(*tasks, return_exceptions=True)
        except (ValueError, OSError, GdbUnavailable, FileNotFoundError, WebSocketDisconnect, RuntimeError):
            try:
                await websocket.close(code=4410)
            except RuntimeError:
                pass
        finally:
            console = app.state.gdb_consoles.get(key)
            if 'queue' in locals() and console is not None:
                console.unsubscribe(queue)

    @app.websocket("/ws/v1/sessions/{instance_id}/{session_id}/hwsim")
    async def hwsim_stream(websocket: WebSocket, instance_id: str, session_id: str) -> None:
        await helper_stream(websocket, instance_id, session_id, "wifi_hwsim")

    @app.websocket("/ws/v1/sessions/{instance_id}/{session_id}/bluetooth")
    async def bluetooth_stream(websocket: WebSocket, instance_id: str, session_id: str) -> None:
        await helper_stream(websocket, instance_id, session_id, "bluetooth_control")

    @app.post("/api/v1/sessions/{instance_id}/{session_id}/audio/control")
    async def audio_control(instance_id: str, session_id: str, request: AudioControlRequest) -> dict[str, object]:
        try:
            record = application.open_session(instance_id, session_id)
            if not application.audio_status(record)["available"]:
                raise HTTPException(status_code=404, detail="registered audio session not found")
            registry = app.state.audio_clients
            if request.action == "attach":
                if request.client_token is not None or request.takeover:
                    raise HTTPException(status_code=422, detail="invalid audio control request")
                client = registry.attach(instance_id, session_id, "local")
                return {"ok": True, "client_token": client.token,
                        "expires_in": 60, "capture_ttl": 30}
            if request.client_token is None or request.takeover and request.action != "claim":
                raise HTTPException(status_code=422, detail="invalid audio control request")
            client = registry.lookup(instance_id, session_id, request.client_token, "local")
            if client is None:
                raise HTTPException(status_code=409, detail="this audio client is not registered; attach again")
            if request.action == "renew":
                return {"ok": True, "client_token": client.token, "expires_in": registry.renew(client),
                        "capture": registry.capture_owner(instance_id, session_id) == client.token}
            if request.action == "claim":
                if not registry.claim(client, request.takeover):
                    raise HTTPException(status_code=409, detail="the microphone is held by another viewer")
                return {"ok": True, "capture": True, "expires_in": 30}
            if request.action == "release":
                if not registry.release(client):
                    raise HTTPException(status_code=409, detail="this viewer does not hold the microphone")
                return {"ok": True, "capture": False}
            registry.revoke(client)
            return {"ok": True, "attached": False}
        except HTTPException:
            raise
        except (ValueError, OSError) as exc:
            raise HTTPException(status_code=404, detail=str(exc)) from exc

    @app.websocket("/ws/v1/sessions/{instance_id}/{session_id}/audio/{channel}")
    async def audio_stream(websocket: WebSocket, instance_id: str, session_id: str, channel: str) -> None:
        if channel not in {"main", "playback", "record"}:
            await websocket.close(code=4404)
            return
        host = websocket.headers.get("host", "")
        scheme = "https" if websocket.url.scheme == "wss" else "http"
        if not _loopback_host(host) or websocket.headers.get("origin") != f"{scheme}://{host}":
            await websocket.close(code=4403)
            return
        client = None
        writer = None
        try:
            record = application.open_session(instance_id, session_id)
            if not application.audio_status(record)["available"]:
                await websocket.close(code=4404)
                return
            token = websocket.query_params.get("client_token", "")
            client = app.state.audio_clients.lookup(instance_id, session_id, token, "local")
            if client is None:
                await websocket.close(code=4403)
                return
            if channel == "main":
                if client.channels or client.connection_id is not None:
                    await websocket.close(code=4409)
                    return
                connection_id = 0
            elif client.connection_id is None or channel in client.channels:
                await websocket.close(code=4409)
                return
            else:
                connection_id = client.connection_id
            if channel == "record" and app.state.audio_clients.capture_owner(instance_id, session_id) != client.token:
                await websocket.close(code=4409)
                return
            gate = SpiceClientGate(channel, connection_id)
            observer = SpiceServerGate(channel)
            client.channels.add(channel)
            await websocket.accept()
            prelude = bytearray()
            while gate.phase != "messages":
                chunk = await websocket.receive_bytes()
                prelude.extend(gate.feed(chunk))
            endpoint = record.runtime_dir / "sockets" / "audio.sock"
            reader, writer = await asyncio.wait_for(asyncio.open_unix_connection(endpoint), 3)
            writer.write(prelude)
            await writer.drain()

            async def browser_to_server() -> None:
                while True:
                    chunk = await websocket.receive_bytes()
                    forwarded = gate.feed(chunk)
                    if forwarded:
                        writer.write(forwarded)
                        await writer.drain()

            async def server_to_browser() -> None:
                while chunk := await reader.read(64 * 1024):
                    observer.feed(chunk)
                    if channel == "main" and observer.connection_id is not None:
                        client.connection_id = observer.connection_id
                    await websocket.send_bytes(chunk)

            tasks = {asyncio.create_task(browser_to_server()), asyncio.create_task(server_to_browser())}
            _done, pending = await asyncio.wait(tasks, return_when=asyncio.FIRST_COMPLETED)
            for task in pending:
                task.cancel()
            await asyncio.gather(*tasks, return_exceptions=True)
        except (OSError, ValueError, SpiceProtocolError, asyncio.TimeoutError, WebSocketDisconnect, RuntimeError):
            try:
                await websocket.close(code=4400)
            except RuntimeError:
                pass
        finally:
            if writer is not None:
                writer.close()
                try:
                    await writer.wait_closed()
                except OSError:
                    pass
            if client is not None:
                client.channels.discard(channel)
                if channel == "main":
                    app.state.audio_clients.revoke(client)

    @app.websocket("/ws/v1/sessions/{instance_id}/{session_id}/frontpanel")
    async def frontpanel_stream(websocket: WebSocket, instance_id: str, session_id: str) -> None:
        host = websocket.headers.get("host", "")
        scheme = "https" if websocket.url.scheme == "wss" else "http"
        if not _loopback_host(host) or websocket.headers.get("origin") != f"{scheme}://{host}":
            await websocket.close(code=4403)
            return
        writer = None
        try:
            record = application.open_session(instance_id, session_id)
            manifest = json.loads(record.manifest.read_text(encoding="utf-8"))
            configuration = manifest.get("configuration", {})
            capability = manifest.get("capabilities", {}).get("front_panel", {}) if isinstance(manifest.get("capabilities"), dict) else {}
            declared = isinstance(configuration, dict) and configuration.get("devices", {}).get("front_panel") is True
            if capability.get("available") is not True and not declared:
                await websocket.close(code=4404)
                return
            endpoint = record.runtime_dir / "sockets" / "frontpanel.sock"
            reader, writer = await asyncio.wait_for(asyncio.open_unix_connection(endpoint, limit=64 * 1024), 3)
            await websocket.accept()
            while raw := await reader.readline():
                if len(raw) > 64 * 1024 or not raw.endswith(b"\n"):
                    raise ValueError("invalid front-panel frame")
                value = json.loads(raw)
                if (not isinstance(value, dict) or value.get("schema") != "unifi.frontpanel.v1"
                        or not isinstance(value.get("kind"), str) or not isinstance(value.get("ports", []), list)):
                    raise ValueError("invalid front-panel schema")
                await websocket.send_json(value)
        except (OSError, ValueError, json.JSONDecodeError, asyncio.TimeoutError, WebSocketDisconnect, RuntimeError):
            try:
                await websocket.close(code=1011)
            except RuntimeError:
                pass
        finally:
            if writer is not None:
                writer.close()
                try:
                    await writer.wait_closed()
                except OSError:
                    pass

    @app.websocket("/ws/v1/sessions/{instance_id}/{session_id}/lcd")
    async def lcd_stream(websocket: WebSocket, instance_id: str, session_id: str) -> None:
        host = websocket.headers.get("host", "")
        scheme = "https" if websocket.url.scheme == "wss" else "http"
        if not _loopback_host(host) or websocket.headers.get("origin") != f"{scheme}://{host}":
            await websocket.close(code=4403)
            return
        writer = None
        try:
            record = application.open_session(instance_id, session_id)
            manifest = json.loads(record.manifest.read_text(encoding="utf-8"))
            configuration = manifest.get("configuration", {})
            capability = manifest.get("capabilities", {}).get("lcd_view", {}) if isinstance(manifest.get("capabilities"), dict) else {}
            devices = configuration.get("devices", {}) if isinstance(configuration, dict) else {}
            if capability.get("available") is not True and not (isinstance(devices, dict) and devices.get("lcd") is True):
                await websocket.close(code=4404)
                return
            endpoint = record.runtime_dir / "sockets" / "lcd.sock"
            reader, writer = await asyncio.wait_for(asyncio.open_unix_connection(endpoint, limit=64 * 1024), 3)
            await websocket.accept()
            while raw := await reader.readline():
                if len(raw) > 64 * 1024 or not raw.endswith(b"\n"):
                    raise ValueError("invalid LCD frame")
                value = json.loads(raw)
                if (not isinstance(value, dict) or value.get("schema") != "unifi.lcm.v1"
                        or not isinstance(value.get("kind"), str)):
                    raise ValueError("invalid LCD schema")
                await websocket.send_json(value)
        except (OSError, ValueError, json.JSONDecodeError, asyncio.TimeoutError, WebSocketDisconnect, RuntimeError):
            try:
                await websocket.close(code=1011)
            except RuntimeError:
                pass
        finally:
            if writer is not None:
                writer.close()
                try:
                    await writer.wait_closed()
                except OSError:
                    pass

    @app.post("/api/v1/sessions/{instance_id}/{session_id}/lcd/touch")
    async def lcd_touch(instance_id: str, session_id: str, request: dict[str, object]) -> dict[str, object]:
        """Send one validated semantic LCD action through the owned input socket."""
        if not isinstance(request, dict) or len(request) != 1:
            raise HTTPException(status_code=422, detail="unsupported touch action")
        valid = False
        if isinstance(request.get("screen"), str):
            valid = request["screen"] in {
                "menu.main", "menu.ports.type", "menu.ports.up", "menu.ports.down",
                "info.rj45", "info.sfp", "ip.address", "power", "menu.network",
                "menu.protect", "menu.access", "menu.talk", "menu.connect", "menu.settings",
                "menu.info", "network.throughput",
            }
        elif type(request.get("port")) is int:
            valid = 1 <= request["port"] <= 26
        elif type(request.get("screensaver")) is bool:
            valid = True
        elif isinstance(request.get("dismiss"), str):
            value = request["dismiss"]
            valid = 0 < len(value) <= 64 and value.replace(".", "").replace("_", "").isalnum()
        if not valid:
            raise HTTPException(status_code=422, detail="unsupported touch action")
        try:
            record = application.open_session(instance_id, session_id)
            endpoint = record.runtime_dir / "sockets" / "display-input.sock"
            if not endpoint.is_socket():
                raise HTTPException(status_code=409, detail="touch input is not ready for this display")
            lock = app.state.lcd_locks.setdefault((instance_id, session_id), asyncio.Lock())
            async with lock:
                reader, writer = await asyncio.wait_for(asyncio.open_unix_connection(endpoint, limit=2048), 3)
                try:
                    writer.write((json.dumps(request, separators=(",", ":")) + "\n").encode())
                    await writer.drain()
                    raw = await asyncio.wait_for(reader.readline(), 8)
                finally:
                    writer.close()
                    await writer.wait_closed()
            if not raw or len(raw) > 2048 or not raw.endswith(b"\n"):
                raise ValueError("invalid touch reply")
            result = json.loads(raw)
            if not isinstance(result, dict) or type(result.get("ok")) is not bool:
                raise ValueError("invalid touch reply")
            return result
        except HTTPException:
            raise
        except (OSError, ValueError, json.JSONDecodeError, asyncio.TimeoutError) as exc:
            raise HTTPException(status_code=502, detail=str(exc)) from exc

    @app.get("/api/v1/sessions/{instance_id}/{session_id}/remote-devices/capabilities",
             response_model=RemoteDeviceCapabilities)
    async def remote_device_capabilities(instance_id: str, session_id: str) -> dict[str, object]:
        try:
            return application.remote_device_capabilities(application.open_session(instance_id, session_id))
        except (ValueError, OSError) as exc:
            raise HTTPException(status_code=404, detail=str(exc)) from exc

    @app.get("/api/v1/sessions/{instance_id}/{session_id}/remote-devices/attachments",
             response_model=list[RemoteDeviceAttachment])
    async def remote_device_attachments(instance_id: str, session_id: str) -> list[dict[str, object]]:
        try:
            application.open_session(instance_id, session_id)
            return app.state.remote_devices.list(instance_id, session_id, "local")
        except (ValueError, OSError) as exc:
            raise HTTPException(status_code=404, detail=str(exc)) from exc

    @app.post("/api/v1/sessions/{instance_id}/{session_id}/remote-devices/attachments",
              response_model=RemoteDeviceAttachment, status_code=201)
    async def create_remote_device_attachment(instance_id: str, session_id: str,
                                              request: RemoteDeviceCreateRequest) -> dict[str, object]:
        try:
            record = application.open_session(instance_id, session_id)
            capabilities = application.remote_device_capabilities(record)
            configuration = json.loads(record.manifest.read_text(encoding="utf-8")).get("configuration", {})
            configuration = configuration if isinstance(configuration, dict) else {}
            remote = configuration.get("remote_devices", {})
            item = app.state.remote_devices.create(
                instance_id, session_id, "local", request.mode, request.profile,
                enabled=capabilities["modes"]["generic_usb"]["available"] is True,
                adapter=configuration.get("adapter", ""),
                profiles=remote.get("profiles", {}) if isinstance(remote, dict) else {},
            )
            return item.public()
        except (ValueError, OSError, json.JSONDecodeError) as exc:
            raise HTTPException(status_code=409, detail=str(exc)) from exc

    @app.post("/api/v1/sessions/{instance_id}/{session_id}/remote-devices/attachments/{attachment_id}/connect-ticket")
    async def remote_device_ticket(instance_id: str, session_id: str, attachment_id: str,
                                   request: RemoteDeviceTicketRequest) -> dict[str, str]:
        try:
            return {"ticket": app.state.remote_devices.ticket(
                attachment_id, instance_id, session_id, "local", request.role,
            )}
        except KeyError as exc:
            raise HTTPException(status_code=404, detail=str(exc)) from exc
        except ValueError as exc:
            raise HTTPException(status_code=409, detail=str(exc)) from exc

    @app.delete("/api/v1/sessions/{instance_id}/{session_id}/remote-devices/attachments/{attachment_id}",
                response_model=RemoteDeviceAttachment)
    async def revoke_remote_device_attachment(instance_id: str, session_id: str,
                                              attachment_id: str) -> dict[str, object]:
        try:
            return app.state.remote_devices.revoke(attachment_id, instance_id, session_id, "local")
        except KeyError as exc:
            raise HTTPException(status_code=404, detail=str(exc)) from exc

    @app.websocket("/ws/v1/sessions/{instance_id}/{session_id}/remote-devices/{attachment_id}")
    async def remote_device_stream(websocket: WebSocket, instance_id: str, session_id: str,
                                   attachment_id: str) -> None:
        host = websocket.headers.get("host", "")
        scheme = "https" if websocket.url.scheme == "wss" else "http"
        if not _loopback_host(host) or websocket.headers.get("origin") != f"{scheme}://{host}":
            await websocket.close(code=4403)
            return
        await websocket.accept()
        attachment = None
        writer = None
        clean = False
        try:
            hello = await asyncio.wait_for(websocket.receive_json(), 5)
            ticket = hello.get("ticket") if isinstance(hello, dict) else None
            if not isinstance(ticket, str):
                await websocket.close(code=4400)
                return
            attachment = app.state.remote_devices.redeem(attachment_id, instance_id, session_id, "local", ticket)
            metadata = await asyncio.wait_for(websocket.receive_json(), 5)
            if not isinstance(metadata, dict):
                await websocket.close(code=4400)
                return
            record = application.open_session(instance_id, session_id)
            configuration = json.loads(record.manifest.read_text(encoding="utf-8")).get("configuration", {})
            remote = configuration.get("remote_devices", {}) if isinstance(configuration, dict) else {}
            profiles = remote.get("profiles", {}) if isinstance(remote, dict) else {}
            app.state.remote_devices.validate_metadata(attachment, metadata, profiles if isinstance(profiles, dict) else {})
            endpoint = record.runtime_dir / "sockets" / "remote-usb.sock"
            reader, writer = await asyncio.wait_for(asyncio.open_unix_connection(endpoint), 3)
            await websocket.send_json({"type": "remote_device.ready", "generation": attachment.generation})

            async def from_qemu() -> None:
                while data := await reader.read(64 * 1024):
                    await websocket.send_bytes(data)

            async def from_browser() -> None:
                nonlocal clean
                while True:
                    message = await websocket.receive()
                    if message["type"] == "websocket.disconnect":
                        return
                    if isinstance(message.get("bytes"), bytes):
                        if len(message["bytes"]) > 1024 * 1024:
                            raise ValueError("remote-device frame is too large")
                        writer.write(message["bytes"])
                        await writer.drain()
                    elif isinstance(message.get("text"), str):
                        try:
                            control = json.loads(message["text"])
                        except json.JSONDecodeError:
                            continue
                        if control == {"type": "cleanup", "confirmed": True}:
                            clean = True
                            return

            pumps = [asyncio.create_task(from_qemu()), asyncio.create_task(from_browser())]
            _done, pending = await asyncio.wait(pumps, return_when=asyncio.FIRST_COMPLETED)
            for task in pending:
                task.cancel()
            await asyncio.gather(*pumps, return_exceptions=True)
        except (KeyError, ValueError, OSError, asyncio.TimeoutError, WebSocketDisconnect, RuntimeError):
            if attachment is not None:
                try:
                    await websocket.close(code=4403)
                except RuntimeError:
                    pass
        finally:
            if writer is not None:
                writer.close()
                try:
                    await writer.wait_closed()
                except OSError:
                    pass
            if attachment is not None:
                app.state.remote_devices.cleanup_ack(attachment, quarantined=not clean)

    @app.post("/api/v1/sessions/{instance_id}/{session_id}/vnc/control")
    async def vnc_control(instance_id: str, session_id: str, body: dict) -> dict[str, object]:
        if (set(body) - {"action", "client_id", "takeover"}
                or body.get("action") not in {"claim", "release"}
                or not isinstance(body.get("client_id"), str)):
            raise HTTPException(status_code=422, detail="invalid VNC control request")
        client_id = body["client_id"].lower()
        if (len(client_id) != 32 or any(char not in "0123456789abcdef" for char in client_id)
                or type(body.get("takeover", False)) is not bool
                or (body["action"] == "release" and body.get("takeover", False))):
            raise HTTPException(status_code=422, detail="invalid VNC control request")
        try:
            record = application.open_session(instance_id, session_id)
            try:
                display_socket(record, "vnc")
            except ValueError:
                display_socket(record, "video")
        except (ValueError, OSError) as exc:
            raise HTTPException(status_code=404, detail=str(exc)) from exc
        owner = app.state.vnc_owners.get((instance_id, session_id))
        if owner and owner[1] <= asyncio.get_running_loop().time():
            app.state.vnc_owners.pop((instance_id, session_id), None)
            owner = None
        key = (instance_id, session_id)
        if body["action"] == "claim":
            if owner and owner[0] != client_id and not body.get("takeover", False):
                raise HTTPException(status_code=409, detail="VNC input is owned by another viewer")
            if owner and owner[0] != client_id:
                for socket in tuple(app.state.vnc_connections.get(key, {}).get(owner[0], ())):
                    try:
                        await socket.close(code=4409, reason="VNC input ownership was taken over")
                    except RuntimeError:
                        pass
            app.state.vnc_owners[key] = (client_id, asyncio.get_running_loop().time() + 60)
            return {"ok": True, "owner": client_id, "view_only": False, "expires_in": 60}
        if not owner or owner[0] != client_id:
            raise HTTPException(status_code=409, detail="this viewer does not own VNC input")
        app.state.vnc_owners.pop(key, None)
        return {"ok": True, "owner": None, "view_only": True}

    @app.get("/api/v1/sessions/{instance_id}/{session_id}/vnc/expose")
    async def vnc_expose_status(instance_id: str, session_id: str) -> dict[str, object]:
        try:
            record = application.open_session(instance_id, session_id)
            display_socket(record, "vnc")
        except (ValueError, OSError) as exc:
            raise HTTPException(status_code=404, detail=str(exc)) from exc
        listener = app.state.external_vnc.get((instance_id, session_id))
        return listener.details() if listener else {"ok": True, "enabled": False}

    @app.post("/api/v1/sessions/{instance_id}/{session_id}/vnc/expose")
    async def vnc_expose(instance_id: str, session_id: str) -> dict[str, object]:
        key = (instance_id, session_id)
        try:
            record = application.open_session(instance_id, session_id)
            endpoint = display_socket(record, "vnc")
        except (ValueError, OSError) as exc:
            raise HTTPException(status_code=404, detail=str(exc)) from exc
        lock = app.state.external_vnc_locks.setdefault(key, asyncio.Lock())
        async with lock:
            listener = app.state.external_vnc.get(key)
            if listener:
                return listener.details()
            try:
                listener = ExternalVncListener(endpoint)
                result = await listener.start()
            except (ImportError, OSError, ValueError) as exc:
                raise HTTPException(status_code=503, detail=str(exc)) from exc
            app.state.external_vnc[key] = listener
            return result

    @app.delete("/api/v1/sessions/{instance_id}/{session_id}/vnc/expose")
    async def vnc_expose_stop(instance_id: str, session_id: str) -> dict[str, object]:
        key = (instance_id, session_id)
        app.state.external_vnc_locks.setdefault(key, asyncio.Lock())
        async with app.state.external_vnc_locks[key]:
            listener = app.state.external_vnc.pop(key, None)
            if listener:
                await listener.stop()
            return {"ok": True, "enabled": False}

    @app.websocket("/ws/v1/sessions/{instance_id}/{session_id}/vnc")
    async def vnc_stream(websocket: WebSocket, instance_id: str, session_id: str) -> None:
        host = websocket.headers.get("host", "")
        scheme = "https" if websocket.url.scheme == "wss" else "http"
        if not _loopback_host(host) or websocket.headers.get("origin") != f"{scheme}://{host}":
            await websocket.close(code=4403)
            return
        client_id = websocket.query_params.get("client_id", "").lower()
        if len(client_id) != 32 or any(char not in "0123456789abcdef" for char in client_id):
            await websocket.close(code=4404)
            return
        reader = writer = None
        key = (instance_id, session_id)
        try:
            record = application.open_session(instance_id, session_id)
            endpoint = display_socket(record, "vnc")
            reader, writer = await asyncio.wait_for(asyncio.open_unix_connection(endpoint), 3)
            await websocket.accept()
            app.state.vnc_connections.setdefault(key, {}).setdefault(client_id, set()).add(websocket)
            gate = RfbInputGate()

            async def upstream_to_browser() -> None:
                while chunk := await reader.read(64 * 1024):
                    await websocket.send_bytes(chunk)

            async def browser_to_upstream() -> None:
                while True:
                    chunk = await websocket.receive_bytes()
                    if len(chunk) > 1_048_576:
                        raise RfbProtocolError("VNC frame exceeds the size limit")
                    owner = app.state.vnc_owners.get(key)
                    if owner and owner[1] <= asyncio.get_running_loop().time():
                        app.state.vnc_owners.pop(key, None)
                        owner = None
                    forwarded = gate.feed(chunk, allow_input=bool(owner and owner[0] == client_id))
                    if forwarded:
                        writer.write(forwarded)
                        await writer.drain()

            tasks = {asyncio.create_task(upstream_to_browser()), asyncio.create_task(browser_to_upstream())}
            done, pending = await asyncio.wait(tasks, return_when=asyncio.FIRST_COMPLETED)
            for task in pending:
                task.cancel()
            results = await asyncio.gather(*done, *pending, return_exceptions=True)
            if any(isinstance(result, RfbProtocolError) for result in results):
                await websocket.close(code=4400)
        except (OSError, ValueError, asyncio.TimeoutError, WebSocketDisconnect, RuntimeError):
            try:
                await websocket.close(code=4400)
            except RuntimeError:
                pass
        finally:
            viewers = app.state.vnc_connections.get(key, {})
            viewers.get(client_id, set()).discard(websocket)
            if not viewers.get(client_id):
                viewers.pop(client_id, None)
                owner = app.state.vnc_owners.get(key)
                if owner and owner[0] == client_id:
                    app.state.vnc_owners.pop(key, None)
            if not viewers:
                app.state.vnc_connections.pop(key, None)
            if writer is not None:
                writer.close()
                try:
                    await writer.wait_closed()
                except OSError:
                    pass

    @app.websocket("/ws/v1/sessions/{instance_id}/{session_id}/video")
    async def video_stream(websocket: WebSocket, instance_id: str, session_id: str) -> None:
        """Relay bounded framed display records and lease-checked controls."""
        host = websocket.headers.get("host", "")
        scheme = "https" if websocket.url.scheme == "wss" else "http"
        if not _loopback_host(host) or websocket.headers.get("origin") != f"{scheme}://{host}":
            await websocket.close(code=4403)
            return
        client_id = websocket.query_params.get("client_id", "").lower()
        if len(client_id) != 32 or any(char not in "0123456789abcdef" for char in client_id):
            await websocket.close(code=4404)
            return
        reader = writer = None
        key = (instance_id, session_id)
        try:
            record = application.open_session(instance_id, session_id)
            endpoint = display_socket(record, "video")
            reader, writer = await asyncio.wait_for(asyncio.open_unix_connection(endpoint), 3)
            await websocket.accept()

            async def upstream_to_browser() -> None:
                pending = bytearray()
                while raw := await reader.read(64 * 1024):
                    pending.extend(raw)
                    if len(pending) > 8 * 1024 * 1024:
                        raise ValueError("video record exceeds the size limit")
                    while len(pending) >= 16:
                        length = int.from_bytes(pending[4:8], "big")
                        if int.from_bytes(pending[2:4], "big") != 0 or length > 8 * 1024 * 1024:
                            raise ValueError("invalid video record")
                        total = 16 + length
                        if len(pending) < total:
                            break
                        await websocket.send_bytes(bytes(pending[:total]))
                        del pending[:total]
                if pending:
                    raise ValueError("truncated video record")

            async def browser_to_upstream() -> None:
                allowed = {"key_down", "key_up", "mouse_move", "mouse_abs", "mouse_down",
                            "mouse_up", "mouse_wheel", "resize", "clipboard_set", "clipboard_request"}
                while True:
                    message = await websocket.receive()
                    if message["type"] == "websocket.disconnect":
                        return
                    text = message.get("text")
                    if not isinstance(text, str) or len(text.encode()) > 64 * 1024:
                        raise ValueError("invalid video control frame")
                    control = json.loads(text)
                    if not isinstance(control, dict) or control.get("type") != "request_idr":
                        if not isinstance(control, dict) or control.get("type") not in allowed:
                            raise ValueError("unknown video control message")
                        owner = app.state.vnc_owners.get(key)
                        if owner and owner[1] <= asyncio.get_running_loop().time():
                            app.state.vnc_owners.pop(key, None)
                            owner = None
                        if not owner or owner[0] != client_id:
                            continue
                        if (control.get("type") == "clipboard_set"
                                and (not isinstance(control.get("text"), str)
                                     or len(control["text"].encode()) > 64 * 1024)):
                            raise ValueError("clipboard control frame exceeds the size limit")
                    writer.write((json.dumps(control, separators=(",", ":")) + "\n").encode())
                    await writer.drain()

            tasks = {asyncio.create_task(upstream_to_browser()), asyncio.create_task(browser_to_upstream())}
            done, pending = await asyncio.wait(tasks, return_when=asyncio.FIRST_COMPLETED)
            for task in pending:
                task.cancel()
            await asyncio.gather(*done, *pending, return_exceptions=True)
        except (OSError, ValueError, json.JSONDecodeError, asyncio.TimeoutError, WebSocketDisconnect, RuntimeError):
            try:
                await websocket.close(code=4400)
            except RuntimeError:
                pass
        finally:
            if writer is not None:
                writer.close()
                try:
                    await writer.wait_closed()
                except OSError:
                    pass

    @app.websocket("/ws/v1/sessions/{instance_id}/{session_id}/terminal")
    async def terminal(websocket: WebSocket, instance_id: str, session_id: str) -> None:
        host = websocket.headers.get("host", "")
        scheme = "https" if websocket.url.scheme == "wss" else "http"
        if not _loopback_host(host) or websocket.headers.get("origin") != f"{scheme}://{host}":
            await websocket.close(code=4403)
            return
        ticket = websocket.query_params.get("ticket", "")
        try:
            record = application.open_session(instance_id, session_id)
            endpoint = application.terminal_socket(record)
        except (ValueError, OSError):
            await websocket.close(code=4404)
            return
        if not app.state.terminal_tickets.consume(ticket, instance_id, session_id):
            await websocket.close(code=4403)
            return
        try:
            reader, writer = await asyncio.open_unix_connection(endpoint)
        except OSError:
            await websocket.close(code=1011)
            return
        await websocket.accept()
        claimed = False
        try:
            await websocket.send_json({"v": 1, "type": "terminal.ready", "view_only": True})
            while True:
                uart_read = asyncio.create_task(reader.read(16 * 1024))
                browser_read = asyncio.create_task(websocket.receive())
                done, pending = await asyncio.wait({uart_read, browser_read}, return_when=asyncio.FIRST_COMPLETED)
                for task in pending:
                    task.cancel()
                await asyncio.gather(*pending, return_exceptions=True)
                if uart_read in done:
                    data = uart_read.result()
                    if not data:
                        await websocket.close(code=1011)
                        return
                    await websocket.send_bytes(data)
                if browser_read in done:
                    message = browser_read.result()
                    if message.get("type") == "websocket.disconnect":
                        return
                    if isinstance(message.get("text"), str):
                        try:
                            control = json.loads(message["text"])
                        except json.JSONDecodeError:
                            await websocket.close(code=4400)
                            return
                        if control == {"v": 1, "type": "terminal.claim"}:
                            claimed = True
                            await websocket.send_json({"v": 1, "type": "terminal.claimed"})
                        elif control == {"v": 1, "type": "terminal.release"}:
                            claimed = False
                            await websocket.send_json({"v": 1, "type": "terminal.released"})
                        else:
                            await websocket.close(code=4400)
                            return
                    elif isinstance(message.get("bytes"), bytes):
                        if not claimed or len(message["bytes"]) > 16 * 1024:
                            await websocket.close(code=4403 if not claimed else 4400)
                            return
                        writer.write(message["bytes"])
                        await writer.drain()
        except (WebSocketDisconnect, OSError, RuntimeError, asyncio.CancelledError):
            return
        finally:
            writer.close()
            await writer.wait_closed()

    @app.get("/api/v1/catalog/profiles")
    async def catalog_profiles() -> list[dict]:
        if app.state.catalog is None:
            raise HTTPException(status_code=404, detail="catalog is not configured")
        try:
            return app.state.catalog.list_profiles()
        except CatalogError as exc:
            raise HTTPException(status_code=500, detail=str(exc)) from exc

    @app.get("/api/v1/catalog/profiles/{profile_id}")
    async def catalog_profile(profile_id: str) -> dict:
        if app.state.catalog is None:
            raise HTTPException(status_code=404, detail="catalog is not configured")
        try:
            return app.state.catalog.get(profile_id)
        except CatalogError as exc:
            raise HTTPException(status_code=404, detail=str(exc)) from exc

    @app.get("/api/v1/devices")
    async def devices() -> dict[str, list[dict[str, object]]]:
        if app.state.catalog is None:
            raise HTTPException(status_code=404, detail="catalog is not configured")
        try:
            profiles = app.state.catalog.list_profiles()
        except CatalogError as exc:
            raise HTTPException(status_code=500, detail=str(exc)) from exc
        sessions = application.list_session_summaries()
        result = []
        for profile in profiles:
            profile_id = str(profile["id"])
            result.append({
                "id": profile_id,
                "name": profile.get("name", profile_id),
                "model": profile.get("machine"),
                "adapter": profile.get("adapter"),
                "capabilities": profile.get("capabilities", profile.get("devices", {})),
                "sessions": [item for item in sessions if item.get("profile_id") == profile_id],
            })
        return {"devices": result}

    @app.get("/api/v1/devices/{device_id}")
    async def device(device_id: str) -> dict[str, object]:
        if app.state.catalog is None:
            raise HTTPException(status_code=404, detail="catalog is not configured")
        try:
            profile = app.state.catalog.get(device_id)
        except CatalogError as exc:
            raise HTTPException(status_code=404, detail=str(exc)) from exc
        return {"id": device_id, "name": profile.get("name", device_id),
                "profile": profile, "sessions": [item for item in application.list_session_summaries()
                                                    if item.get("profile_id") == device_id]}

    @app.post("/api/v1/devices/{device_id}/launch-validation")
    async def device_launch_validation(device_id: str, request: DeviceLaunchRequest) -> dict[str, object]:
        try:
            profile, resolved, plan = application.preview_catalog_profile(device_id, target=request.target)
        except (CatalogError, ValueError, OSError) as exc:
            return {"valid": False, "device_id": device_id, "errors": [str(exc)],
                    "state_policy": "configured"}
        return {"valid": True, "device_id": device_id, "errors": [],
                "state_policy": "configured", "target": resolved.target,
                "capabilities": profile.get("capabilities", profile.get("devices", {})),
                "launch_argv_count": len(plan.command)}

    @app.post("/api/v1/devices/{device_id}/sessions", status_code=201)
    async def device_session(device_id: str, request: DeviceSessionRequest) -> dict[str, str]:
        try:
            record = application.create_catalog_session(
                device_id, target=request.target, instance_id=request.instance_id,
                session_id=request.session_id,
            )
        except (CatalogError, ValueError, OSError) as exc:
            raise HTTPException(status_code=400, detail=str(exc)) from exc
        return {"session_id": record.session_id, "manifest": str(record.manifest), "state": "created"}

    @app.post("/api/v1/devices/{device_id}/clones", status_code=201)
    async def device_clone(device_id: str, body: AnalysisCloneRequest) -> dict[str, object]:
        try:
            return application.create_analysis_clone(device_id, body.clone_id, target=body.target)
        except (CatalogError, ValueError, OSError) as exc:
            raise HTTPException(status_code=409, detail=str(exc)) from exc

    @app.post("/api/v1/catalog/sessions", status_code=201)
    async def create_catalog_session(request: CreateCatalogSessionRequest) -> dict[str, str]:
        try:
            record = application.create_catalog_session(
                request.profile_id, target=request.target,
                instance_id=request.instance_id, session_id=request.session_id,
            )
        except (ValueError, OSError) as exc:
            raise HTTPException(status_code=400, detail=str(exc)) from exc
        return {"session_id": record.session_id, "manifest": str(record.manifest), "state": "created"}

    @app.post("/api/v1/sessions/reconcile")
    async def reconcile(request: SessionRequest) -> dict[str, str]:
        try:
            record = application.open_session(request.instance_id, request.session_id)
            state = application.reconcile_session(record)
        except (ValueError, OSError) as exc:
            raise HTTPException(status_code=404, detail=str(exc)) from exc
        return {"session_id": record.session_id, "state": state}

    @app.get("/api/v1/operations/{operation_id}")
    async def operation_status(operation_id: str) -> dict[str, object]:
        operation = app.state.operations.get(operation_id)
        if operation is None:
            raise HTTPException(status_code=404, detail="operation not found")
        return operation.public()

    @app.post("/api/v1/sessions", status_code=201)
    async def create_session(request: CreateSessionRequest) -> dict[str, str]:
        try:
            record = application.create_session(
                Path(request.profile_path), target=request.target,
                instance_id=request.instance_id, session_id=request.session_id,
            )
        except (ValueError, OSError) as exc:
            raise HTTPException(status_code=400, detail=str(exc)) from exc
        return {"session_id": record.session_id, "manifest": str(record.manifest), "state": "created"}

    @app.get("/api/v1/sessions/{instance_id}/{session_id}")
    async def inspect(instance_id: str, session_id: str) -> dict:
        try:
            record = application.open_session(instance_id, session_id)
            return json.loads(record.manifest.read_text(encoding="utf-8"))
        except (ValueError, OSError, json.JSONDecodeError) as exc:
            raise HTTPException(status_code=404, detail=str(exc)) from exc

    @app.get("/api/v1/sessions/{instance_id}/{session_id}/hardware-config")
    async def hardware_config(instance_id: str, session_id: str) -> dict[str, object]:
        try:
            record = application.open_session(instance_id, session_id)
            value = json.loads(record.manifest.read_text(encoding="utf-8"))
            configuration = value.get("configuration", {})
            return {"schema_version": 1, "configuration": configuration if isinstance(configuration, dict) else {}}
        except (ValueError, OSError, json.JSONDecodeError) as exc:
            raise HTTPException(status_code=404, detail=str(exc)) from exc

    @app.get("/api/v1/sessions/{instance_id}/{session_id}/environment")
    async def environment(instance_id: str, session_id: str) -> dict[str, object]:
        try:
            record = application.open_session(instance_id, session_id)
            value = json.loads(record.manifest.read_text(encoding="utf-8"))
            analysis = value.get("analysis")
            return {"schema_version": 1, "analysis": analysis if isinstance(analysis, dict) else None}
        except (ValueError, OSError, json.JSONDecodeError) as exc:
            raise HTTPException(status_code=404, detail=str(exc)) from exc

    @app.get("/api/v1/sessions/{instance_id}/{session_id}/logs")
    async def logs(instance_id: str, session_id: str, stream: str = "stdout", tail: int = 200) -> dict[str, object]:
        if stream not in {"stdout", "stderr"} or not 1 <= tail <= 2000:
            raise HTTPException(status_code=422, detail="stream must be stdout or stderr and tail must be 1..2000")
        try:
            record = application.open_session(instance_id, session_id)
            path = record.runtime_dir / "logs" / f"{stream}.log"
            if not path.is_file():
                return {"schema_version": 1, "stream": stream, "lines": [], "truncated": False}
            lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
            return {"schema_version": 1, "stream": stream, "lines": lines[-tail:], "truncated": len(lines) > tail}
        except (ValueError, OSError) as exc:
            raise HTTPException(status_code=404, detail=str(exc)) from exc

    @app.get("/api/v1/instances/{instance_id}/state/inventory")
    async def instance_state_inventory(instance_id: str) -> dict:
        """Hash managed instance state without modifying it."""
        try:
            return application.inventory_instance(instance_id)
        except (ValueError, OSError) as exc:
            raise HTTPException(status_code=404, detail=str(exc)) from exc

    @app.get("/api/v1/instances/{instance_id}/snapshots")
    async def snapshots(instance_id: str) -> dict[str, object]:
        try:
            return application.list_snapshots(instance_id)
        except (ValueError, OSError) as exc:
            raise HTTPException(status_code=404, detail=str(exc)) from exc

    @app.post("/api/v1/instances/{instance_id}/snapshots", status_code=201)
    async def create_snapshot(instance_id: str, request: SnapshotRequest) -> dict[str, object]:
        try:
            return application.create_snapshot(instance_id, request.snapshot_id, request.files)
        except (ValueError, OSError) as exc:
            raise HTTPException(status_code=409, detail=str(exc)) from exc

    @app.post("/api/v1/instances/{instance_id}/snapshots/{snapshot_id}/restore")
    async def restore_snapshot(instance_id: str, snapshot_id: str) -> dict[str, object]:
        try:
            return application.restore_snapshot(instance_id, snapshot_id)
        except (ValueError, OSError) as exc:
            raise HTTPException(status_code=409, detail=str(exc)) from exc

    @app.get("/api/v1/sessions/{instance_id}/{session_id}/snapshots")
    async def session_snapshots(instance_id: str, session_id: str) -> dict[str, object]:
        try:
            application.open_session(instance_id, session_id)
            return application.list_snapshots(instance_id)
        except (ValueError, OSError) as exc:
            raise HTTPException(status_code=404, detail=str(exc)) from exc

    @app.post("/api/v1/sessions/{instance_id}/{session_id}/snapshots", status_code=201)
    async def session_snapshot_create(instance_id: str, session_id: str,
                                      request: SnapshotRequest) -> dict[str, object]:
        try:
            application.open_session(instance_id, session_id)
            return application.create_snapshot(instance_id, request.snapshot_id, request.files)
        except (ValueError, OSError) as exc:
            raise HTTPException(status_code=409, detail=str(exc)) from exc

    @app.post("/api/v1/sessions/{instance_id}/{session_id}/snapshots/{snapshot_id}/restore")
    async def session_snapshot_restore(instance_id: str, session_id: str,
                                       snapshot_id: str) -> dict[str, object]:
        try:
            application.open_session(instance_id, session_id)
            return application.restore_snapshot(instance_id, snapshot_id)
        except (ValueError, OSError) as exc:
            raise HTTPException(status_code=409, detail=str(exc)) from exc

    @app.delete("/api/v1/sessions/{instance_id}/{session_id}/snapshots/{snapshot_id}")
    async def session_snapshot_delete(instance_id: str, session_id: str,
                                      snapshot_id: str) -> dict[str, object]:
        try:
            application.open_session(instance_id, session_id)
            return application.delete_snapshot(instance_id, snapshot_id)
        except (ValueError, OSError) as exc:
            raise HTTPException(status_code=409, detail=str(exc)) from exc

    @app.post("/api/v1/sessions/{instance_id}/{session_id}/start")
    async def start(instance_id: str, session_id: str) -> dict[str, int | str]:
        try:
            record = application.open_session(instance_id, session_id)
            running = await application.start_recorded_session(record)
        except (ValueError, OSError) as exc:
            raise HTTPException(status_code=400, detail=str(exc)) from exc
        return {"session_id": record.session_id, "pid": running.process.pid, "state": "running"}

    @app.post("/api/v1/sessions/{instance_id}/{session_id}/stop")
    async def stop(instance_id: str, session_id: str) -> dict[str, int | str]:
        try:
            record = application.open_session(instance_id, session_id)
            exit_code = application.stop_session(record)
        except (ValueError, OSError) as exc:
            raise HTTPException(status_code=400, detail=str(exc)) from exc
        return {"session_id": record.session_id, "exit_code": exit_code, "state": "stopped"}

    @app.post("/api/v1/sessions/{instance_id}/{session_id}/restart", status_code=202)
    async def restart(instance_id: str, session_id: str) -> dict[str, object]:
        operation = app.state.operations.create("restart", session_id)
        app.state.operations.update(operation, "executing")
        try:
            record = application.open_session(instance_id, session_id)
            value = json.loads(record.manifest.read_text(encoding="utf-8"))
            state = value.get("state")
            if state == "running":
                application.stop_session(record)
            elif state in {"stopping"}:
                raise ValueError("session is stopping")
            running = await application.start_recorded_session(record)
            result = {"session_id": record.session_id, "pid": running.process.pid, "state": "running"}
            app.state.operations.update(operation, "succeeded", result=result)
        except (ValueError, OSError) as exc:
            app.state.operations.update(operation, "failed", error={"code": "restart_failed", "message": str(exc)})
        return operation.public()

    @app.delete("/api/v1/sessions/{instance_id}/{session_id}", status_code=202)
    async def delete_session(instance_id: str, session_id: str) -> dict[str, object]:
        operation = app.state.operations.create("delete", session_id)
        app.state.operations.update(operation, "executing")
        key = (instance_id, session_id)
        try:
            record = application.open_session(instance_id, session_id)
            if key in app.state.external_vnc:
                await app.state.external_vnc.pop(key).stop()
            console = app.state.gdb_consoles.pop(key, None)
            if console is not None:
                await console.stop()
            application.store.remove(record)
            result = {"session_id": session_id, "state": "deleted"}
            app.state.operations.update(operation, "succeeded", result=result)
        except (ValueError, OSError) as exc:
            app.state.operations.update(operation, "failed", error={"code": "delete_failed", "message": str(exc)})
        return operation.public()

    return app
