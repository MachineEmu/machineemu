"""Application operations shared by the CLI and future HTTP adapters."""

from __future__ import annotations

import json
from pathlib import Path
from typing import Sequence

from machineemu.assets import AssetStore
from machineemu.domains.analysis import create_clone, write_environment_report
from machineemu.catalog import ProfileCatalog
from machineemu.engines import EngineRegistry
from machineemu.profiles import build_launch_plan, resolve_profile, resolve_profile_value

from .config import OperatorConfig
from .instance import InstanceStore
from .migration import inventory_json
from .state import SessionRecord, SessionStore
from .qmp import QMPClient, QMPError
from .supervisor import RunningSession, SessionSupervisor


class OperatorApplication:
    """Coordinate validated operations without owning a server or event loop."""

    def __init__(self, config: OperatorConfig, *, release_set: Path | None = None,
                 bundle_root: Path | None = None, catalog: ProfileCatalog | None = None):
        self.config = config
        self.store = SessionStore(config.runtime_root, config.state_root, config.artifact_root)
        self.instances = InstanceStore(config.state_root)
        self.assets = AssetStore(config.asset_root)
        self.release_set = release_set
        self.bundle_root = bundle_root
        self.catalog = catalog

    def create_session(self, profile_path: Path, *, target: str, instance_id: str,
                       session_id: str) -> SessionRecord:
        if self.release_set is None or self.bundle_root is None:
            raise ValueError("release set and bundle root are required to create a session")
        registry = EngineRegistry.load(self.release_set, self.bundle_root)
        profile = resolve_profile(profile_path, registry, target=target, asset_store=self.assets)
        plan = build_launch_plan(profile, self.config.runtime_root / "sessions" / session_id,
                                 self.config.state_root / "instances" / instance_id)
        self.instances.ensure(instance_id, profile)
        record = self.store.create(instance_id, session_id, profile)
        self.store.update(record, "created", metadata={"launch_plan": plan.manifest})
        if profile.analysis is not None:
            write_environment_report(record, profile, plan)
        return record

    def create_catalog_session(self, profile_id: str, *, target: str | None,
                               instance_id: str, session_id: str) -> SessionRecord:
        if self.catalog is None:
            raise ValueError("catalog is not configured")
        if self.release_set is None or self.bundle_root is None:
            raise ValueError("release set and bundle root are required to create a session")
        value = self.catalog.get(profile_id)
        external_assets = value.get("external_assets", [])
        if isinstance(external_assets, list) and any(
            isinstance(item, dict) and item.get("required") is True
            for item in external_assets
        ) and not value.get("assets"):
            raise ValueError("catalog profile requires imported assets before launch")
        selected_target = target or value.get("target")
        if not isinstance(selected_target, str) or not selected_target:
            raise ValueError("catalog profile has no target; target is required")
        registry = EngineRegistry.load(self.release_set, self.bundle_root)
        profile = resolve_profile_value(value, registry, target=selected_target, asset_store=self.assets)
        plan = build_launch_plan(profile, self.config.runtime_root / "sessions" / session_id,
                                 self.config.state_root / "instances" / instance_id)
        self.instances.ensure(instance_id, profile)
        record = self.store.create(instance_id, session_id, profile)
        self.store.update(record, "created", metadata={"launch_plan": plan.manifest})
        if profile.analysis is not None:
            write_environment_report(record, profile, plan)
        return record

    def preview_catalog_profile(self, profile_id: str, *, target: str | None = None):
        """Resolve a catalog profile and launch plan without creating runtime state."""
        if self.catalog is None or self.release_set is None or self.bundle_root is None:
            raise ValueError("catalog and engine bundle configuration are required")
        value = self.catalog.get(profile_id)
        selected_target = target or value.get("target")
        if not isinstance(selected_target, str) or not selected_target:
            raise ValueError("catalog profile has no target; target is required")
        registry = EngineRegistry.load(self.release_set, self.bundle_root)
        profile = resolve_profile_value(value, registry, target=selected_target, asset_store=self.assets)
        plan = build_launch_plan(profile, self.config.runtime_root / "sessions" / "device-validation")
        return value, profile, plan

    def create_analysis_clone(self, profile_id: str, clone_id: str, *, instance_id: str,
                              target: str | None = None) -> dict[str, object]:
        """Clone an instance's accumulated state, not the pristine baseline.

        The disk comes from the instance's overlay, and the UEFI variables and
        TPM from its per-instance copies, so a clone carries whatever the
        machine has enrolled and installed since it was created.
        """
        value, profile, _ = self.preview_catalog_profile(profile_id, target=target)
        analysis = profile.analysis
        if not isinstance(analysis, dict) or analysis.get("profile") != "malware-analysis":
            raise ValueError("analysis cloning is only available for malware-analysis profiles")
        record = self.instances.open(instance_id)
        try:
            instance = json.loads(record.manifest.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as exc:
            raise ValueError(f"cannot read instance manifest: {exc}") from exc
        if instance.get("profile_id") != profile_id:
            raise ValueError(f"instance {instance_id} was not created from profile {profile_id}")
        state_dir = record.state_dir
        assets = {"disk": state_dir / "overlay.qcow2", "firmware_vars": state_dir / "OVMF_VARS.fd"}
        missing = sorted(name for name, path in assets.items() if not path.is_file())
        if missing:
            raise ValueError(f"instance {instance_id} has no machine state to clone: "
                             f"{', '.join(missing)}; start the session at least once")
        tpm_state = state_dir / "tpm"
        if tpm_state.is_dir():
            assets["tpm"] = tpm_state
        destination = self.config.artifact_root / "analysis-clones"
        return create_clone(destination_root=destination, clone_id=clone_id,
                            identity_seed=str(value.get("analysis", {}).get("identity_seed", "")),
                            assets=assets, profile_revision=str(analysis.get("patch_revision", "unknown")))

    def open_session(self, instance_id: str, session_id: str) -> SessionRecord:
        return self.store.open(instance_id, session_id)

    def list_session_summaries(self) -> list[dict[str, object]]:
        """List public session metadata without returning host paths or launch argv."""
        summaries: list[dict[str, object]] = []
        for record in self.store.list_records():
            try:
                manifest = json.loads(record.manifest.read_text(encoding="utf-8"))
            except (OSError, json.JSONDecodeError):
                continue
            if not isinstance(manifest, dict):
                continue
            configuration = manifest.get("configuration")
            devices = configuration.get("devices") if isinstance(configuration, dict) else None
            capabilities: dict[str, dict[str, object]] = {}
            if isinstance(devices, dict):
                # Keep this vocabulary aligned with the source web contract.
                # A capability may be either a boolean shorthand or a mapping
                # carrying an availability flag and a human-readable reason.
                aliases = {
                    "lcd": "lcd_view",
                    "lcd_touch": "lcd_touch",
                    "front_panel": "front_panel",
                    "vnc": "vnc",
                    "video": "video",
                    "snapshots": "snapshots",
                    "wifi_hwsim": "wifi_hwsim",
                    "bluetooth": "bluetooth",
                    "bluetooth_control": "bluetooth_control",
                    "remote_devices": "remote_devices",
                    "pause": "pause",
                    "reset": "reset",
                    "fresh_state": "fresh_state",
                    "state_reuse": "state_reuse",
                }
                for source_name, public_name in aliases.items():
                    value = devices.get(source_name)
                    if value is True:
                        capabilities[public_name] = {"available": True}
                    elif isinstance(value, dict) and isinstance(value.get("available"), bool):
                        capability = {"available": value["available"]}
                        if isinstance(value.get("reason"), str):
                            capability["reason"] = value["reason"]
                        capabilities[public_name] = capability
            console = configuration.get("console") if isinstance(configuration, dict) else None
            if isinstance(console, dict) and console.get("uart") is True:
                capabilities["uart_view"] = {"available": True}
                capabilities["uart_control"] = {"available": True}
            summaries.append({
                "session_id": record.session_id,
                "instance_id": record.instance_id,
                "profile_id": manifest.get("profile_id", "unknown"),
                "machine": manifest.get("machine", "unknown"),
                "state": manifest.get("state", "unknown"),
                "capabilities": capabilities,
            })
        return summaries

    def terminal_socket(self, record: SessionRecord) -> Path:
        """Return the profile-declared UART endpoint only when it is owned by the session."""
        try:
            manifest = json.loads(record.manifest.read_text(encoding="utf-8"))
            plan = manifest.get("launch_plan")
            endpoint = plan.get("uart_socket") if isinstance(plan, dict) else None
        except (OSError, json.JSONDecodeError) as exc:
            raise ValueError(f"cannot read session terminal configuration: {exc}") from exc
        expected = record.runtime_dir / "sockets" / "uart.sock"
        if endpoint != str(expected) or not expected.is_socket():
            raise ValueError("session has no available UART terminal")
        return expected

    async def qmp_status(self, record: SessionRecord) -> dict[str, object]:
        """Expose only QMP's non-mutating status query to browser callers."""
        _, endpoint = self.recorded_plan(record)
        expected = record.runtime_dir / "sockets" / "qmp.sock"
        if endpoint != expected:
            raise ValueError("session QMP endpoint is not owned by its runtime directory")
        client = await QMPClient.connect(endpoint, timeout=1.0)
        try:
            result = await client.execute("query-status", timeout=1.0)
        finally:
            await client.close()
        if not isinstance(result, dict) or not isinstance(result.get("status"), str):
            raise QMPError("QMP status response is invalid")
        return {key: result[key] for key in ("status", "running", "singlestep") if key in result}

    async def qmp_action(self, record: SessionRecord, action: str) -> str:
        """Apply one allowlisted lifecycle action through the owned QMP socket."""
        commands = {"pause": "stop", "resume": "cont", "reset": "system_reset"}
        command = commands.get(action)
        if command is None:
            raise ValueError("QMP action not allowed")
        _, endpoint = self.recorded_plan(record)
        expected = record.runtime_dir / "sockets" / "qmp.sock"
        if endpoint != expected:
            raise ValueError("session QMP endpoint is not owned by its runtime directory")
        client = await QMPClient.connect(endpoint, timeout=1.0)
        try:
            await client.execute(command, timeout=1.0)
        finally:
            await client.close()
        state = {"pause": "paused", "resume": "running", "reset": "running"}[action]
        self.store.update(record, state)
        return state

    async def qmp_inspect(self, record: SessionRecord, command: str,
                          path: str | None = None, property: str | None = None) -> dict[str, object]:
        """Run one allowlisted, read-only QMP query for diagnostics."""
        allowed = {"query-status", "query-pci", "query-chardev", "query-block", "qom-list", "qom-get"}
        if command == "info-usb":
            qmp_command, arguments = "human-monitor-command", {"command-line": "info usb"}
        elif command in allowed:
            qmp_command, arguments = command, {}
            if command in {"qom-list", "qom-get"}:
                if not isinstance(path, str) or not path.startswith("/"):
                    raise ValueError("absolute QOM path required")
                arguments["path"] = path
                if command == "qom-get":
                    if not isinstance(property, str) or not property:
                        raise ValueError("QMP property required")
                    arguments["property"] = property
        else:
            raise ValueError("QMP command not allowed")
        _, endpoint = self.recorded_plan(record)
        expected = record.runtime_dir / "sockets" / "qmp.sock"
        if endpoint != expected:
            raise ValueError("session QMP endpoint is not owned by its runtime directory")
        client = await QMPClient.connect(endpoint, timeout=1.0)
        try:
            result = await client.execute(qmp_command, timeout=1.0, **arguments)
        finally:
            await client.close()
        return {"command": command, "result": result}

    async def qmp_execute(self, record: SessionRecord, command: str,
                          arguments: dict[str, object] | None = None) -> object:
        """Execute one narrowly allowlisted live hotplug QMP operation."""
        allowed = {
            "query-usb", "query-block", "query-netdev", "device_add", "device_del",
            "blockdev-add", "blockdev-del", "blockdev-change-medium", "eject",
            "netdev_add", "netdev_del", "set_link",
        }
        if command not in allowed:
            raise ValueError("QMP hotplug command is not allowed")
        _, endpoint = self.recorded_plan(record)
        expected = record.runtime_dir / "sockets" / "qmp.sock"
        if endpoint != expected:
            raise ValueError("session QMP endpoint is not owned by its runtime directory")
        value = json.loads(record.manifest.read_text(encoding="utf-8"))
        if value.get("state") not in {"running", "paused"}:
            raise ValueError("session is not running")
        client = await QMPClient.connect(endpoint, timeout=1.0)
        try:
            return await client.execute(command, timeout=5.0, **(arguments or {}))
        finally:
            await client.close()

    async def screenshot(self, record: SessionRecord) -> tuple[Path, str]:
        """Capture the primary display into the session-owned artifact directory."""
        _, endpoint = self.recorded_plan(record)
        expected = record.runtime_dir / "sockets" / "qmp.sock"
        if endpoint != expected:
            raise ValueError("session QMP endpoint is not owned by its runtime directory")
        record.artifact_dir.mkdir(parents=True, exist_ok=True)
        client = await QMPClient.connect(endpoint, timeout=1.0)
        try:
            png = record.artifact_dir / "screenshot.png"
            try:
                await client.execute("screendump", timeout=1.0, filename=str(png), format="png")
                return png, "image/png"
            except QMPError:
                ppm = record.artifact_dir / "screenshot.ppm"
                await client.execute("screendump", timeout=1.0, filename=str(ppm))
                return ppm, "image/x-portable-pixmap"
        finally:
            await client.close()

    def audio_status(self, record: SessionRecord) -> dict[str, object]:
        """Report the audio transport without exposing host paths."""
        try:
            manifest = json.loads(record.manifest.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as exc:
            raise ValueError(f"cannot read session audio configuration: {exc}") from exc
        plan = manifest.get("launch_plan")
        endpoint = plan.get("audio_socket") if isinstance(plan, dict) else None
        available = isinstance(endpoint, str) and endpoint == str(record.runtime_dir / "sockets" / "audio.sock") and Path(endpoint).is_socket()
        return {
            "schema_version": 1,
            "available": available,
            "reason": None if available else "Audio is not configured for this session",
            "capture_held": False,
            "capture_ttl": 30,
        }

    def remote_device_capabilities(self, record: SessionRecord) -> dict[str, object]:
        """Describe the approved remote-device profiles without host details."""
        try:
            manifest = json.loads(record.manifest.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as exc:
            raise ValueError(f"cannot read session remote-device configuration: {exc}") from exc
        configuration = manifest.get("configuration")
        configuration = configuration if isinstance(configuration, dict) else {}
        remote = configuration.get("remote_devices")
        remote = remote if isinstance(remote, dict) else {}
        profiles = remote.get("profiles")
        profiles = profiles if isinstance(profiles, dict) else {}
        public_profiles: dict[str, dict[str, object]] = {}
        allowed_fields = ("accept_all", "vendor_id", "product_id", "serial", "interfaces", "endpoints",
                          "allow_control", "allow_bulk", "allow_interrupt")
        for name, value in profiles.items():
            if isinstance(name, str) and isinstance(value, dict):
                public_profiles[name] = {key: value[key] for key in allowed_fields if key in value}
        enabled = remote.get("enabled") is True
        adapter = configuration.get("adapter")
        available = enabled and adapter == "pc" and bool(public_profiles)
        reason = None if available else (
            "remote_devices is not enabled" if not enabled
            else "generic USB MVP requires the pc adapter"
        )
        return {
            "schema_version": 1,
            "modes": {
                "generic_usb": {"available": available, "reason": reason},
                "webauthn": {"available": False, "reason": "remote-origin feasibility gate is not proven"},
                "ctap": {"available": False, "reason": "native CTAP helper feasibility gate is not proven"},
            },
            "profiles": public_profiles,
            "limits": {"max_payload": 65536, "max_outstanding": 16, "max_queued_bytes": 1048576},
        }

    def inventory_instance(self, instance_id: str) -> dict[str, object]:
        """Return a read-only inventory for an existing durable instance."""
        record = self.instances.open(instance_id)
        return inventory_json(record.state_dir)

    def _instance_is_running(self, instance_id: str) -> bool:
        return any(
            record.instance_id == instance_id and self._session_state(record) in {"running", "paused", "stopping"}
            for record in self.store.list_records()
        )

    @staticmethod
    def _session_state(record: SessionRecord) -> str:
        try:
            value = json.loads(record.manifest.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            return "unknown"
        return value.get("state", "unknown") if isinstance(value.get("state"), str) else "unknown"

    def list_snapshots(self, instance_id: str) -> dict[str, object]:
        record = self.instances.open(instance_id)
        snapshots = record.state_dir / "snapshots"
        result = []
        if snapshots.is_dir():
            for item in sorted(snapshots.iterdir(), key=lambda path: path.name):
                manifest = item / "snapshot.json"
                if item.is_symlink() or not item.is_dir() or manifest.is_symlink() or not manifest.is_file():
                    continue
                try:
                    value = json.loads(manifest.read_text(encoding="utf-8"))
                except (OSError, json.JSONDecodeError):
                    continue
                if value.get("instance_id") == instance_id and value.get("snapshot_id") == item.name:
                    files = value.get("files", {})
                    result.append({"snapshot_id": item.name, "files": sorted(files) if isinstance(files, dict) else []})
        return {"schema_version": 1, "snapshots": result}

    def create_snapshot(self, instance_id: str, snapshot_id: str,
                        files: list[str] | None = None) -> dict[str, object]:
        record = self.instances.open(instance_id)
        if self._instance_is_running(instance_id):
            raise ValueError("snapshot creation requires all sessions for the instance to be stopped")
        destination = self.instances.snapshot(record, snapshot_id, files)
        return {"snapshot_id": snapshot_id, "state": "created", "files": sorted(
            path.name for path in destination.iterdir() if path.name != "snapshot.json"
        )}

    def restore_snapshot(self, instance_id: str, snapshot_id: str) -> dict[str, object]:
        record = self.instances.open(instance_id)
        if self._instance_is_running(instance_id):
            raise ValueError("snapshot restore requires all sessions for the instance to be stopped")
        self.instances.stage_snapshot_restore(record, snapshot_id)
        self.instances.apply_staged_restore(record, snapshot_id, instance_state="stopped")
        return {"snapshot_id": snapshot_id, "state": "restored"}

    def delete_snapshot(self, instance_id: str, snapshot_id: str) -> dict[str, object]:
        record = self.instances.open(instance_id)
        if self._instance_is_running(instance_id):
            raise ValueError("snapshot deletion requires all sessions for the instance to be stopped")
        self.instances.delete_snapshot(record, snapshot_id)
        return {"snapshot_id": snapshot_id, "state": "deleted"}

    async def start_session(self, record: SessionRecord, command: Sequence[str], qmp_socket: Path) -> RunningSession:
        return await SessionSupervisor(self.store).start(record, command, qmp_socket)

    async def start_recorded_session(self, record: SessionRecord) -> RunningSession:
        command, qmp_socket = self.recorded_plan(record)
        return await self.start_session(record, command, qmp_socket)

    def reconcile_session(self, record: SessionRecord) -> str:
        return SessionSupervisor(self.store).recover(record)

    def stop_session(self, record: SessionRecord, timeout: float = 5.0) -> int:
        return SessionSupervisor(self.store).stop_recovered(record, timeout)

    @staticmethod
    def recorded_plan(record: SessionRecord) -> tuple[list[str], Path]:
        try:
            value = json.loads(record.manifest.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as exc:
            raise ValueError(f"cannot read session launch plan: {exc}") from exc
        plan = value.get("launch_plan")
        if not isinstance(plan, dict) or not isinstance(plan.get("argv"), list):
            raise ValueError("session manifest has no launch plan")
        endpoint = plan.get("qmp_socket")
        if not isinstance(endpoint, str) or not endpoint:
            raise ValueError("session launch plan has no QMP endpoint")
        return plan["argv"], Path(endpoint)
