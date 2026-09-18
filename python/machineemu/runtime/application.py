"""Application operations shared by the CLI and future HTTP adapters."""

from __future__ import annotations

import json
from pathlib import Path
from typing import Sequence

from machineemu.assets import AssetStore
from machineemu.engines import EngineRegistry
from machineemu.profiles import build_launch_plan, resolve_profile

from .config import OperatorConfig
from .state import SessionRecord, SessionStore
from .supervisor import RunningSession, SessionSupervisor


class OperatorApplication:
    """Coordinate validated operations without owning a server or event loop."""

    def __init__(self, config: OperatorConfig, *, release_set: Path | None = None,
                 bundle_root: Path | None = None):
        self.config = config
        self.store = SessionStore(config.runtime_root, config.state_root, config.artifact_root)
        self.assets = AssetStore(config.asset_root)
        self.release_set = release_set
        self.bundle_root = bundle_root

    def create_session(self, profile_path: Path, *, target: str, instance_id: str,
                       session_id: str) -> SessionRecord:
        if self.release_set is None or self.bundle_root is None:
            raise ValueError("release set and bundle root are required to create a session")
        registry = EngineRegistry.load(self.release_set, self.bundle_root)
        profile = resolve_profile(profile_path, registry, target=target, asset_store=self.assets)
        plan = build_launch_plan(profile, self.config.runtime_root / "sessions" / session_id)
        record = self.store.create(instance_id, session_id, profile)
        self.store.update(record, "created", metadata={"launch_plan": plan.manifest})
        return record

    def open_session(self, instance_id: str, session_id: str) -> SessionRecord:
        return self.store.open(instance_id, session_id)

    async def start_session(self, record: SessionRecord, command: Sequence[str], qmp_socket: Path) -> RunningSession:
        return await SessionSupervisor(self.store).start(record, command, qmp_socket)

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
