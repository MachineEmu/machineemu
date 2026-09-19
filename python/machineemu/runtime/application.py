"""Application operations shared by the CLI and future HTTP adapters."""

from __future__ import annotations

import json
from pathlib import Path
from typing import Sequence

from machineemu.assets import AssetStore
from machineemu.catalog import ProfileCatalog
from machineemu.engines import EngineRegistry
from machineemu.profiles import build_launch_plan, resolve_profile, resolve_profile_value

from .config import OperatorConfig
from .instance import InstanceStore
from .migration import inventory_json
from .state import SessionRecord, SessionStore
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
        plan = build_launch_plan(profile, self.config.runtime_root / "sessions" / session_id)
        self.instances.ensure(instance_id, profile)
        record = self.store.create(instance_id, session_id, profile)
        self.store.update(record, "created", metadata={"launch_plan": plan.manifest})
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
        plan = build_launch_plan(profile, self.config.runtime_root / "sessions" / session_id)
        self.instances.ensure(instance_id, profile)
        record = self.store.create(instance_id, session_id, profile)
        self.store.update(record, "created", metadata={"launch_plan": plan.manifest})
        return record

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
            capabilities: dict[str, dict[str, bool]] = {}
            if isinstance(devices, dict):
                if devices.get("lcd") is True:
                    capabilities["lcd_view"] = {"available": True}
                if devices.get("bluetooth") is True:
                    capabilities["bluetooth"] = {"available": True}
            summaries.append({
                "session_id": record.session_id,
                "instance_id": record.instance_id,
                "profile_id": manifest.get("profile_id", "unknown"),
                "machine": manifest.get("machine", "unknown"),
                "state": manifest.get("state", "unknown"),
                "capabilities": capabilities,
            })
        return summaries

    def inventory_instance(self, instance_id: str) -> dict[str, object]:
        """Return a read-only inventory for an existing durable instance."""
        record = self.instances.open(instance_id)
        return inventory_json(record.state_dir)

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
